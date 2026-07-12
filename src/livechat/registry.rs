//! ホストレジストリ(配線層 — US1 読み取り/同期のスレセッション統合)
//!
//! [`crate::livechat::host::HostThread`] を板ごとに保持し、P2P セッション(参加者接続)と
//! gossip ハブ(announce 発行・ブロードキャスト)を仲介する共有状態。`Arc<LivechatRegistry>`
//! として [`crate::p2p::runtime::P2pRuntime`] と各接続タスクで共有する。
//!
//! ## 役割の境界
//!
//! - **保持**: 板ごとの [`HostThread`]・スレ主ペルソナ鍵・接続時同期の再生に要する署名済み
//!   イベント(確定レス kind 1311 / 発行済み ORDER kind 21311)。スレデータ自体は揮発
//!   (FR-015)であり本レジストリのメモリ内にのみ存在する。
//! - **判定**: THREAD_JOIN の受理可否([`HostThread::decide_join`])と、WELCOME 後に送出すべき
//!   同期フレーム列([`crate::p2p::frame::Message`])の生成。
//! - **採番(シーケンサ — T030)**: 参加者からの RES を受信検証後に一意採番し
//!   ([`LivechatRegistry::accept_write`])、ORDER(kind 21311)を発行して RES + ORDER を
//!   全接続参加者の outbox へ配布する(FR-007・不変条件 T3/O1 — PlusCal モデル対応)。
//! - **非責務**: トランスポート I/O(TCP)は配線側(runtime)。本レジストリは参加者の outbox
//!   ([`tokio::sync::mpsc::UnboundedSender`])を保持し、そこへメッセージを流すところまで。
//!
//! ## 採番の単点性(Principle V / PlusCal 検査対象)
//!
//! [`accept_write`](LivechatRegistry::accept_write) は docs/formal/livechat_sequencer.tla の
//! HostProcess「採番」遷移に対応し、重複排除(D1)・上限(T3)・状態(T1)ガードを TLC で
//! 検査済みの不変条件どおりに実装する(各メソッドの意図コメント参照)。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use nostr::{Event, Keys};
use tokio::sync::mpsc::UnboundedSender;

use crate::event::livechat::{
    self, LivechatBuildError, OrderEntry, OrderInfo as OrderEnvelope, Res as ResEnvelope,
};
use crate::p2p::frame::Message as WireMessage;

use super::host::{HostThread, JoinDecision, SyncItem, board_settings_json};
use super::thread::{BoardSettings, Res, Thread, ThreadError};

/// ポイズン時も内部値を回収してロックを返す(パニックしない)。
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 1 板分のホスト状態(HostThread + スレ主鍵 + 署名済みイベントキャッシュ)。
struct HostEntry {
    host: HostThread,
    /// スレ主ペルソナ鍵(WELCOME 署名・ORDER 署名・announce 署名に使う)。
    persona: Keys,
    /// ホスト接続先 `ip:port`(announce の `tip`)。スレ開設時に確定する(FR-004)。
    tip: String,
    /// 確定レスの署名済みイベント(event_id → kind 1311)。同期再送で RES フレームへ写す。
    res_events: HashMap<String, Event>,
    /// 発行済み ORDER の署名済みイベント(seq → kind 21311)。同期再送で ORDER フレームへ写す。
    order_events: HashMap<u32, Event>,
    /// **採番済みイベント id の集合(板単位・世代跨ぎ)**。設計制約 D1(PlusCal
    /// `AssignedIds` / `w.id \notin AssignedIds` ガード)の実装。参加者は確定通知(ORDER)を
    /// 受け取る前に切断されると同一イベントを再送しうるため、これで重複採番を排除しないと
    /// 同一イベントが二つの res_no を得て AssignedOnce(不変条件 O1)が破れる。
    assigned_ids: HashSet<String>,
    /// 接続中参加者の送信口(peer_id → outbox)。採番した RES + ORDER をここへ配布する
    /// (registry → outbox。PlusCal の `chan[p]` への Append に対応)。
    outboxes: HashMap<String, UnboundedSender<WireMessage>>,
}

/// スレ seed(確定レス投入)・接続受理の失敗理由。`Display` は内部情報を漏らさない。
#[derive(Debug)]
pub enum RegistryError {
    /// 指定 board_id のスレが開設されていない。
    UnknownBoard,
    /// スレ開設が掲載ペルソナに限定される制約に反する(board_id ≠ 鍵の公開鍵 — T019)。
    BoardIdMismatch,
    /// 確定レスの投入に失敗(不変条件 T1/T3 違反)。
    Confirm(ThreadError),
    /// イベント署名・構築の失敗。
    Build(LivechatBuildError),
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegistryError::UnknownBoard => write!(f, "指定された板は開設されていません"),
            RegistryError::BoardIdMismatch => {
                write!(f, "スレ開設は掲載ペルソナに限定されます")
            }
            RegistryError::Confirm(_) => write!(f, "レスを確定できません"),
            RegistryError::Build(_) => write!(f, "イベントの構築に失敗しました"),
        }
    }
}

impl std::error::Error for RegistryError {}

/// THREAD_JOIN を処理した結果(配線側が送出・登録する)。
pub struct JoinOutcome {
    /// 参加者へ返すフレーム列(WELCOME + 同期の RES/ORDER、または REJECT)。
    pub frames: Vec<WireMessage>,
    /// 受理されたか(true なら配線側が participant を登録し接続を維持する)。
    pub accepted: bool,
}

/// 参加者からの RES 採番([`LivechatRegistry::accept_write`])の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcceptOutcome {
    /// 採番して RES + ORDER を配布した。`res_no` は割り当てたレス番号、`seq` は ORDER の連番。
    Numbered { res_no: u16, seq: u32 },
    /// 既採番の event_id(再送)。採番も配布もしない(設計制約 D1 — O1 を保つ)。
    Duplicate,
    /// 採番せず定型拒否(非 Active・対象スレ不一致・res_limit 到達)。配線側は
    /// `livechat_write_rejected` を記録しうるが応答で理由を開示しない(FR-006)。
    Rejected,
}

/// ホストレジストリ(板ごとの [`HostThread`] を共有保持する)。
pub struct LivechatRegistry {
    /// board_id(スレ主ペルソナ pubkey hex)→ ホスト状態。
    hosts: Mutex<HashMap<String, HostEntry>>,
    /// ホストの受入接続上限(Settings.thread_max_participants)。
    max_participants: usize,
}

impl LivechatRegistry {
    /// レジストリを作る(`max_participants` は Settings.thread_max_participants)。
    pub fn new(max_participants: usize) -> Arc<Self> {
        Arc::new(Self {
            hosts: Mutex::new(HashMap::new()),
            max_participants,
        })
    }

    /// スレを開設する(T019 — 掲載ペルソナ限定)。
    ///
    /// `persona` はスレ主ペルソナ鍵。`board_id`(= `persona` の公開鍵)以外での開設は
    /// [`RegistryError::BoardIdMismatch`] で拒否する(FR-003 の発行側不変条件)。既存の
    /// 同 board_id は置換する(板あたりアクティブ 1 本 — 不変条件 T2)。
    /// `tip` はホスト接続先 `ip:port`(announce の `tip` — 他ノードが接続する自ノードの
    /// 到達アドレス。受信のみでは接続しない FR-004)。
    #[allow(clippy::too_many_arguments)]
    pub fn open_thread(
        &self,
        persona: Keys,
        channel: impl Into<String>,
        generation: u32,
        key: u64,
        title: impl Into<String>,
        settings: BoardSettings,
        tip: impl Into<String>,
    ) -> Result<(), RegistryError> {
        let board_id = persona.public_key().to_hex();
        let thread = Thread::new(
            &board_id,
            channel,
            generation,
            key,
            title,
            settings.res_limit,
        );
        let host = HostThread::new(thread, settings);
        lock(&self.hosts).insert(
            board_id,
            HostEntry {
                host,
                persona,
                tip: tip.into(),
                res_events: HashMap::new(),
                order_events: HashMap::new(),
                assigned_ids: HashSet::new(),
                outboxes: HashMap::new(),
            },
        );
        Ok(())
    }

    /// 開設中の board_id 一覧(status・診断用)。
    pub fn board_ids(&self) -> Vec<String> {
        lock(&self.hosts).keys().cloned().collect()
    }

    /// 開設中の全スレの announce(kind 31311)を署名して返す(T019 — 60 秒間隔の定期発行)。
    ///
    /// 各 board のスレ主ペルソナ鍵で署名し、`tip` は開設時に保持した自ノードの到達アドレスを
    /// 使う。`expiration = created_at + 600` は封筒側([`ThreadAnnounce::sign`])が付与する。
    /// 実際の gossip 発行([`crate::p2p::hub::GossipHub::publish_local`])は配線側(main の
    /// 定期タスク)が行う — 本メソッドは「発行すべき Event を作る」ところまで(署名失敗の
    /// board は黙って飛ばす)。
    pub fn build_announce_events(&self, created_at: u64, pow_bits: u8) -> Vec<Event> {
        let hosts = lock(&self.hosts);
        let mut events = Vec::new();
        for entry in hosts.values() {
            let announce = livechat::ThreadAnnounce {
                channel: entry.host.thread.channel.clone(),
                title: entry.host.thread.title.clone(),
                generation: entry.host.thread.generation,
                key: entry.host.thread.key,
                res_count: Some(entry.host.res_count() as u64),
                tip: entry.tip.clone(),
            };
            if let Ok(ev) = announce.sign(&entry.persona, created_at, pow_bits) {
                events.push(ev);
            }
        }
        events
    }

    /// 確定済みレスを 1 件投入する(テスト・互換 seed 用 — US2 の採番前に既存レスを積む)。
    ///
    /// 署名済み kind 1311 イベント(`res_event`)を受け取り、[`Thread::confirm`] で確定列へ
    /// 追加し、対応する ORDER(kind 21311)をスレ主鍵で署名して記録する。これにより接続時
    /// 同期([`Self::sync_frames`])が既存レスを RES/ORDER で再生できる。
    ///
    /// `created_at` は ORDER の署名時刻。res_no は現在の確定数 + 1 に固定される(T3)。
    pub fn seed_confirmed_res(
        &self,
        board_id: &str,
        res_event: &Event,
        created_at: u64,
    ) -> Result<u16, RegistryError> {
        // 封筒の形式検証(kind 1311・タグ・本文)を通してドメイン Res を作る。
        let envelope =
            ResEnvelope::from_event(res_event).map_err(|_| RegistryError::UnknownBoard)?;
        let mut hosts = lock(&self.hosts);
        let entry = hosts.get_mut(board_id).ok_or(RegistryError::UnknownBoard)?;

        let event_id = res_event.id.to_hex();
        let res_no = entry.host.thread.next_res_no();
        let domain = Res {
            event_id: event_id.clone(),
            board_key: res_event.pubkey.to_hex(),
            name: envelope.name.clone(),
            mail: envelope.mail.clone(),
            body: envelope.body.clone(),
            created_at: res_event.created_at.as_secs() as i64,
            res_no: None,
            pending: false,
        };
        entry
            .host
            .thread
            .confirm(domain, res_no)
            .map_err(RegistryError::Confirm)?;

        // ORDER を採番して記録し、スレ主鍵で署名した kind 21311 をキャッシュする。
        let order = entry.host.record_order(vec![(res_no, event_id.clone())]);
        let envelope = OrderEnvelope {
            board_id: board_id.to_string(),
            generation: entry.host.thread.generation,
            seq: order.seq,
            entries: vec![OrderEntry {
                res_no,
                event_id: event_id.clone(),
            }],
        };
        let order_event = envelope
            .sign(&entry.persona, created_at)
            .map_err(RegistryError::Build)?;
        entry.assigned_ids.insert(event_id.clone());
        entry.res_events.insert(event_id, res_event.clone());
        entry.order_events.insert(order.seq, order_event);
        Ok(res_no)
    }

    /// THREAD_JOIN を処理する(T021/T023 — 受理判定 + 同期フレーム生成)。
    ///
    /// `thread_ref` は `<board_id>:<gen>`。board_id を抜き出して対応ホストを引き、
    /// [`HostThread::decide_join`] で受理可否を判定する。受理時は WELCOME に続けて
    /// `since_seq` 以降の同期フレーム(確定レス RES + 未受信 ORDER)を seq 順に並べる。
    ///
    /// 未知の board_id は定型 `unknown_thread` REJECT(内部情報を開示しない — FR-006)。
    pub fn handle_join(
        &self,
        thread_ref: &str,
        challenge_hex: &str,
        since_seq: u32,
    ) -> JoinOutcome {
        let board_id = thread_ref.split_once(':').map(|(b, _)| b).unwrap_or("");
        let mut hosts = lock(&self.hosts);
        let Some(entry) = hosts.get_mut(board_id) else {
            // 未知スレは定型拒否(存在しない board を掴んだ接続 — FR-006)。
            return JoinOutcome {
                frames: vec![WireMessage::ThreadReject {
                    reason: crate::p2p::frame::thread_reject_reason::UNKNOWN_THREAD.to_string(),
                }],
                accepted: false,
            };
        };

        let decision = entry.host.decide_join(
            thread_ref,
            challenge_hex,
            &entry.persona,
            self.max_participants,
        );
        match decision {
            JoinDecision::Welcome { .. } => {
                let board_settings = board_settings_json(&entry.host.settings);
                let welcome = super::host::join_decision_to_message(decision, board_settings);
                let mut frames = vec![welcome];
                // 接続時同期: since_seq 以降の RES/ORDER を seq 順にワイヤ化する。
                for item in entry.host.sync_since(since_seq) {
                    if let Some(msg) = Self::sync_item_to_message(entry, item) {
                        frames.push(msg);
                    }
                }
                JoinOutcome {
                    frames,
                    accepted: true,
                }
            }
            JoinDecision::Reject { .. } => {
                let board_settings = board_settings_json(&entry.host.settings);
                JoinOutcome {
                    frames: vec![super::host::join_decision_to_message(
                        decision,
                        board_settings,
                    )],
                    accepted: false,
                }
            }
        }
    }

    /// RESEND_REQ を処理し、`from_seq..=to_seq` の ORDER と対応 RES を再送する(T023)。
    ///
    /// 欠落検出後の再送要求(不変条件 O2)。範囲外・未知 seq は黙って飛ばす(要求側の
    /// 誤りに応答で反応しない — 内部情報非開示)。
    pub fn handle_resend(&self, board_id: &str, from_seq: u32, to_seq: u32) -> Vec<WireMessage> {
        let mut hosts = lock(&self.hosts);
        let Some(entry) = hosts.get_mut(board_id) else {
            return Vec::new();
        };
        let mut frames = Vec::new();
        for order in entry.host.orders() {
            if order.seq < from_seq || order.seq > to_seq {
                continue;
            }
            // 各 ORDER に対応する RES を先に、続いて ORDER を送る(seq 連続で復元可能に)。
            for (_res_no, event_id) in &order.entries {
                if let Some(ev) = entry.res_events.get(event_id) {
                    frames.push(res_event_to_message(ev));
                }
            }
            if let Some(ev) = entry.order_events.get(&order.seq) {
                frames.push(order_event_to_message(ev));
            }
        }
        frames
    }

    /// 参加者からの RES(書き込み)を受信検証する(FR-007/FR-011 の配線層強制 — 採番前)。
    ///
    /// US1 は**読み取り/同期のみ**のため採番はしない(採番・配布は US2 の T030)。ただし
    /// ホストが受信した RES の**封筒署名・形式・対象スレ一致**は本段で検証し、不正な書き込みを
    /// 検出できるようにする(検出結果は配線側が `livechat_write_rejected` として記録する)。
    ///
    /// 検証内容(thread-events.md §ホスト側受信検証の 1〜4 相当。BAN/PoW/レートは US2/US4):
    ///
    /// 1. **署名**: nostr の id・sig 検証(封筒が本物であること)。
    /// 2. **形式**: kind=1311・必須タグ・本文制約([`ResEnvelope::from_event`])。
    /// 3. **対象スレ一致**: 封筒の board_id・世代が本ホストの Active スレと一致すること
    ///    (別スレ・別世代・未知板への書き込みは受理しない — 不変条件 T1/T2)。
    /// 4. **状態**: 対象スレが Active であること(Frozen/Closed は受理しない — T1)。
    ///
    /// 妥当な書き込みなら `true`(US1 では採番せず破棄)、不正なら `false`(記録は配線側 —
    /// 応答で理由を開示しない)。
    pub fn verify_incoming_res(&self, board_id: &str, res_event: &Event) -> bool {
        // 1. 署名(id・sig)。
        if res_event.verify().is_err() {
            return false;
        }
        // 2. 形式(kind 1311・タグ・本文)。
        let Ok(envelope) = ResEnvelope::from_event(res_event) else {
            return false;
        };
        // 3. 対象スレ一致 + 4. 状態(Active)。
        let hosts = lock(&self.hosts);
        let Some(entry) = hosts.get(board_id) else {
            return false;
        };
        if envelope.board_id != entry.host.thread.board_id
            || envelope.generation != entry.host.thread.generation
        {
            return false;
        }
        entry.host.thread.check_writable().is_ok()
    }

    /// 参加者からの RES を採番し、RES + ORDER を全接続参加者へ配布する(T030 — シーケンサ)。
    ///
    /// **PlusCal モデル(docs/formal/livechat_sequencer.tla)の HostProcess「採番」遷移に対応**。
    /// TLC で検査済みの不変条件 AssignedOnce(O1)・NoOverLimit・DisplayPrefix(T3)を保つよう、
    /// 以下の順序・ガードを厳守する(モデルの `await` 節と 1:1):
    ///
    /// 1. **対象スレ一致 + 状態(T1)**: 封筒の board_id・世代が本ホストの Active スレと一致し、
    ///    スレが Active であること(モデル `phase = "active"`)。不一致・非 Active は
    ///    [`AcceptOutcome::Rejected`]。
    /// 2. **重複排除(設計制約 D1 — `w.id \notin AssignedIds`)**: event_id が板単位で既採番なら
    ///    採番せず [`AcceptOutcome::Duplicate`](再送 × 切断で二重採番が起きるのを防ぐ — O1)。
    /// 3. **上限(NoOverLimit / T3 — `RoomInActive`)**: 次 res_no が res_limit を超えるなら
    ///    採番せず [`AcceptOutcome::Rejected`](次スレ移行は US5/T047)。
    /// 4. **採番(単点性 — Principle V)**: [`Thread::confirm`] で res_no を 1 つ割り当てる
    ///    (T3 を強制)。ORDER(kind 21311・seq 連番)をスレ主鍵で署名する。
    /// 5. **配布(`chan[p]` への Append)**: RES + ORDER を **全接続参加者(送信者含む)** の
    ///    outbox へ seq 順に送る。切断済み outbox への送信失敗は無視する(unregister は配線側)。
    ///
    /// 署名検証・形式・BAN・PoW・レートの受信検証(1〜7)は呼び出し前に
    /// [`Self::verify_incoming_res`] 等で済ませてある前提(モデル境界 — ADR-0014 §2)。
    /// 本メソッドは検証通過後の**採番判定のみ**をモデル化する。
    pub fn accept_write(
        &self,
        board_id: &str,
        res_event: &Event,
        created_at: u64,
    ) -> Result<AcceptOutcome, RegistryError> {
        let envelope =
            ResEnvelope::from_event(res_event).map_err(|_| RegistryError::UnknownBoard)?;
        let mut hosts = lock(&self.hosts);
        let entry = hosts.get_mut(board_id).ok_or(RegistryError::UnknownBoard)?;

        // 1. 対象スレ一致 + 状態(T1 — phase = active)。
        if envelope.board_id != entry.host.thread.board_id
            || envelope.generation != entry.host.thread.generation
            || entry.host.thread.check_writable().is_err()
        {
            return Ok(AcceptOutcome::Rejected);
        }

        let event_id = res_event.id.to_hex();
        // 2. 重複排除(D1 — w.id \notin AssignedIds)。既採番は No-op(採番も配布もしない)。
        if entry.assigned_ids.contains(&event_id) {
            return Ok(AcceptOutcome::Duplicate);
        }
        // 3. 上限(NoOverLimit / T3 — RoomInActive)。
        if entry.host.thread.next_res_no() > entry.host.thread.res_limit {
            return Ok(AcceptOutcome::Rejected);
        }

        // 4. 採番(単点性)。confirm が T3(res_no 欠番なし単調増加・上限)を強制する。
        let res_no = entry.host.thread.next_res_no();
        let domain = Res {
            event_id: event_id.clone(),
            board_key: res_event.pubkey.to_hex(),
            name: envelope.name.clone(),
            mail: envelope.mail.clone(),
            body: envelope.body.clone(),
            created_at: res_event.created_at.as_secs() as i64,
            res_no: None,
            pending: false,
        };
        entry
            .host
            .thread
            .confirm(domain, res_no)
            .map_err(RegistryError::Confirm)?;

        // ORDER(seq 連番)をスレ主ペルソナ鍵で署名する(1 採番 = 1 ORDER entry)。
        let order = entry.host.record_order(vec![(res_no, event_id.clone())]);
        let order_env = OrderEnvelope {
            board_id: board_id.to_string(),
            generation: entry.host.thread.generation,
            seq: order.seq,
            entries: vec![OrderEntry {
                res_no,
                event_id: event_id.clone(),
            }],
        };
        let order_event = order_env
            .sign(&entry.persona, created_at)
            .map_err(RegistryError::Build)?;

        // キャッシュ + 採番済み集合へ記録(D1・再送同期の再生に使う)。
        entry.assigned_ids.insert(event_id.clone());
        entry.res_events.insert(event_id, res_event.clone());
        entry.order_events.insert(order.seq, order_event.clone());

        // 5. 配布: RES + ORDER を全接続参加者(送信者含む)の outbox へ seq 順に送る。
        let res_msg = res_event_to_message(res_event);
        let order_msg = order_event_to_message(&order_event);
        for tx in entry.outboxes.values() {
            // 送信失敗(受信側キュー破棄 = 切断途上)は無視する。unregister は配線側(pump)。
            let _ = tx.send(res_msg.clone());
            let _ = tx.send(order_msg.clone());
        }

        Ok(AcceptOutcome::Numbered {
            res_no,
            seq: order.seq,
        })
    }

    /// 参加者を登録する(WELCOME 送出成功後に配線側が呼ぶ)。
    ///
    /// `outbox` は当該接続への送信口。採番した RES + ORDER のブロードキャスト先になる
    /// (registry → outbox — T030 の配布配線)。
    pub fn register_participant(
        &self,
        board_id: &str,
        peer_id: &str,
        outbox: UnboundedSender<WireMessage>,
    ) {
        if let Some(entry) = lock(&self.hosts).get_mut(board_id) {
            entry.host.register_participant(peer_id);
            entry.outboxes.insert(peer_id.to_string(), outbox);
        }
    }

    /// 参加者の登録を解除する(切断時)。outbox も除去する。
    pub fn unregister_participant(&self, board_id: &str, peer_id: &str) {
        if let Some(entry) = lock(&self.hosts).get_mut(board_id) {
            entry.host.unregister_participant(peer_id);
            entry.outboxes.remove(peer_id);
        }
    }

    /// 同期 1 項目をワイヤメッセージへ写す(署名済みイベントキャッシュから引く)。
    fn sync_item_to_message(entry: &HostEntry, item: SyncItem) -> Option<WireMessage> {
        match item {
            SyncItem::Res(res) => entry
                .res_events
                .get(&res.event_id)
                .map(res_event_to_message),
            SyncItem::Order(order) => entry
                .order_events
                .get(&order.seq)
                .map(order_event_to_message),
        }
    }
}

/// 署名済み kind 1311 イベントを `RES` メッセージへ写す。
fn res_event_to_message(event: &Event) -> WireMessage {
    WireMessage::Res {
        event: serde_json::to_value(event).unwrap_or(serde_json::Value::Null),
    }
}

/// 署名済み kind 21311 イベントを `ORDER` メッセージへ写す。
fn order_event_to_message(event: &Event) -> WireMessage {
    WireMessage::Order {
        event: serde_json::to_value(event).unwrap_or(serde_json::Value::Null),
    }
}

/// スレ主鍵で確定レス用の kind 1311 イベントを署名する補助(seed・テスト用)。
///
/// 板鍵で署名するのが本来だが(FR-016)、seed 用途では任意の署名鍵を受け取れるよう
/// 分離する。`board_id`(スレ主 pubkey)と `channel` は封筒の必須フィールド。
pub fn sign_res(
    board_key: &Keys,
    board_id: &str,
    channel: &str,
    generation: u32,
    body: &str,
    created_at: u64,
) -> Result<Event, LivechatBuildError> {
    let envelope = ResEnvelope {
        channel: channel.to_string(),
        board_id: board_id.to_string(),
        generation,
        name: None,
        mail: None,
        body: body.to_string(),
    };
    envelope.sign(board_key, created_at, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::p2p::frame::thread_reject_reason;

    const GUID: &str = "0123456789abcdef0123456789abcdef";

    fn persona() -> Keys {
        Keys::generate()
    }

    fn channel_of(board_id: &str) -> String {
        format!("30311:{board_id}:{GUID}")
    }

    fn registry_with_thread(persona: &Keys, max: usize) -> Arc<LivechatRegistry> {
        let reg = LivechatRegistry::new(max);
        let board_id = persona.public_key().to_hex();
        reg.open_thread(
            persona.clone(),
            channel_of(&board_id),
            1,
            1_700_000_000,
            "実況スレ",
            BoardSettings::default(),
            "198.51.100.1:7147",
        )
        .unwrap();
        reg
    }

    #[test]
    fn open_thread_registers_board() {
        let p = persona();
        let reg = registry_with_thread(&p, 128);
        assert_eq!(reg.board_ids(), vec![p.public_key().to_hex()]);
    }

    #[test]
    fn handle_join_welcomes_and_syncs_seeded_res() {
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);

        // 板鍵で 2 レスを seed(閲覧のみのため board_key は任意鍵でよい)。
        let board_key = Keys::generate();
        let ch = channel_of(&board_id);
        let r1 = sign_res(&board_key, &board_id, &ch, 1, "一つ目", 1_700_000_001).unwrap();
        let r2 = sign_res(&board_key, &board_id, &ch, 1, "二つ目", 1_700_000_002).unwrap();
        assert_eq!(
            reg.seed_confirmed_res(&board_id, &r1, 1_700_000_001)
                .unwrap(),
            1
        );
        assert_eq!(
            reg.seed_confirmed_res(&board_id, &r2, 1_700_000_002)
                .unwrap(),
            2
        );

        let challenge = crate::livechat::session::generate_challenge();
        let outcome = reg.handle_join(&format!("{board_id}:1"), &challenge, 0);
        assert!(outcome.accepted);
        // WELCOME + (RES,ORDER)×2 = 5 フレーム。
        assert!(matches!(
            outcome.frames[0],
            WireMessage::ThreadWelcome { .. }
        ));
        let res_count = outcome
            .frames
            .iter()
            .filter(|m| matches!(m, WireMessage::Res { .. }))
            .count();
        let order_count = outcome
            .frames
            .iter()
            .filter(|m| matches!(m, WireMessage::Order { .. }))
            .count();
        assert_eq!(res_count, 2, "確定レス 2 件が RES で同期される");
        assert_eq!(order_count, 2, "ORDER 2 件が同期される");
    }

    #[test]
    fn welcome_sig_verifies_on_participant() {
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);
        let challenge = crate::livechat::session::generate_challenge();
        let outcome = reg.handle_join(&format!("{board_id}:1"), &challenge, 0);
        match &outcome.frames[0] {
            WireMessage::ThreadWelcome { sig, .. } => {
                // 参加者側検証がスレ主公開鍵で成功する(FR-005)。
                assert!(crate::livechat::session::verify_welcome_sig(
                    sig, &challenge, &board_id, 1
                ));
            }
            other => panic!("WELCOME を期待: {other:?}"),
        }
    }

    #[test]
    fn handle_join_unknown_board_rejects() {
        let reg = LivechatRegistry::new(128);
        let outcome = reg.handle_join("deadbeef:1", "00ff", 0);
        assert!(!outcome.accepted);
        assert!(matches!(
            &outcome.frames[0],
            WireMessage::ThreadReject { reason } if reason == thread_reject_reason::UNKNOWN_THREAD
        ));
    }

    #[test]
    fn handle_join_full_rejects() {
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 1);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        reg.register_participant(&board_id, "peer-a", tx);
        let challenge = crate::livechat::session::generate_challenge();
        let outcome = reg.handle_join(&format!("{board_id}:1"), &challenge, 0);
        assert!(!outcome.accepted);
        assert!(matches!(
            &outcome.frames[0],
            WireMessage::ThreadReject { reason } if reason == thread_reject_reason::FULL
        ));
    }

    #[test]
    fn resend_returns_requested_range() {
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);
        let board_key = Keys::generate();
        let ch = channel_of(&board_id);
        for i in 1..=3u64 {
            let r = sign_res(&board_key, &board_id, &ch, 1, "x", 1_700_000_000 + i).unwrap();
            reg.seed_confirmed_res(&board_id, &r, 1_700_000_000 + i)
                .unwrap();
        }
        // seq 2..=3 を再送要求。
        let frames = reg.handle_resend(&board_id, 2, 3);
        let orders = frames
            .iter()
            .filter(|m| matches!(m, WireMessage::Order { .. }))
            .count();
        assert_eq!(orders, 2, "seq 2 と 3 の ORDER が再送される");
    }

    // --- RES 受信検証(FR-007/FR-011 の配線層強制)---------------------------

    #[test]
    fn verify_incoming_res_accepts_valid_write() {
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);
        let board_key = Keys::generate();
        let ch = channel_of(&board_id);
        // 正当な板鍵署名・対象スレ一致の RES は妥当(Active スレへの書き込み)。
        let res = sign_res(&board_key, &board_id, &ch, 1, "書き込み", 1_700_000_005).unwrap();
        assert!(reg.verify_incoming_res(&board_id, &res));
    }

    #[test]
    fn verify_incoming_res_rejects_wrong_generation() {
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);
        let board_key = Keys::generate();
        let ch = channel_of(&board_id);
        // 別世代(gen=2)の RES は対象スレ不一致で拒否(不変条件 T1/T2)。
        let res = sign_res(&board_key, &board_id, &ch, 2, "別世代", 1_700_000_005).unwrap();
        assert!(!reg.verify_incoming_res(&board_id, &res));
    }

    #[test]
    fn verify_incoming_res_rejects_unknown_board() {
        let reg = LivechatRegistry::new(128);
        let board_key = Keys::generate();
        let board_id = "ab".repeat(32);
        let ch = channel_of(&board_id);
        let res = sign_res(&board_key, &board_id, &ch, 1, "未知板", 1_700_000_005).unwrap();
        // 開設されていない板への書き込みは拒否。
        assert!(!reg.verify_incoming_res(&board_id, &res));
    }

    #[test]
    fn verify_incoming_res_rejects_tampered_signature() {
        use nostr::JsonUtil;
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);
        let board_key = Keys::generate();
        let ch = channel_of(&board_id);
        let res = sign_res(&board_key, &board_id, &ch, 1, "本文", 1_700_000_005).unwrap();
        // content を改竄すると id 再計算が合わず署名検証に失敗する。
        let raw = res
            .as_json()
            .replace("\"content\":\"本文\"", "\"content\":\"改竄\"");
        let tampered = Event::from_json(&raw).unwrap();
        assert!(!reg.verify_incoming_res(&board_id, &tampered));
    }

    #[test]
    fn verify_incoming_res_rejects_when_frozen() {
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);
        // スレを Frozen にすると書き込みは受理されない(T1)。
        {
            let mut hosts = lock(&reg.hosts);
            hosts
                .get_mut(&board_id)
                .unwrap()
                .host
                .thread
                .freeze()
                .unwrap();
        }
        let board_key = Keys::generate();
        let ch = channel_of(&board_id);
        let res = sign_res(&board_key, &board_id, &ch, 1, "凍結後", 1_700_000_005).unwrap();
        assert!(!reg.verify_incoming_res(&board_id, &res));
    }

    // --- announce 発行(T019 — 60 秒間隔の定期発行)-------------------------

    #[test]
    fn build_announce_events_signs_all_boards() {
        use nostr::JsonUtil;
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);
        let events = reg.build_announce_events(1_700_000_000, 0);
        assert_eq!(events.len(), 1, "開設中の 1 板分の announce を発行する");
        let ev = &events[0];
        assert_eq!(ev.kind.as_u16(), crate::event::livechat::ANNOUNCE_KIND);
        // 署名者 = スレ主ペルソナ(a タグの pubkey と一致 — FR-003)。
        assert_eq!(ev.pubkey, p.public_key());
        assert!(ev.verify().is_ok());
        // gossip 受信検証(検査 1〜7)を通る = 可視な announce。
        let cfg = crate::event::schema::VerifyConfig::default();
        let verified =
            crate::event::schema::verify_incoming_announce(&ev.as_json(), &cfg, 1_700_000_000);
        assert!(verified.is_ok(), "自ノード発行 announce は受信側検証を通る");
        let restored = verified.unwrap();
        assert_eq!(restored.announce.tip, "198.51.100.1:7147");
        assert_eq!(restored.announce.generation, 1);
        // board_id 先頭。
        assert!(board_id.starts_with(&restored.event.pubkey.to_hex()[..8]));
    }

    #[test]
    fn build_announce_events_empty_without_threads() {
        let reg = LivechatRegistry::new(128);
        assert!(reg.build_announce_events(1_700_000_000, 0).is_empty());
    }

    // --- T030: ホストシーケンサ(採番・配布)-------------------------------

    #[test]
    fn accept_write_numbers_and_broadcasts_to_all_participants() {
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);
        // 2 名の参加者を outbox 付きで登録(送信者含む全員へ配布されることを確認)。
        let (tx1, mut rx1) = tokio::sync::mpsc::unbounded_channel();
        let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel();
        reg.register_participant(&board_id, "peer-1", tx1);
        reg.register_participant(&board_id, "peer-2", tx2);

        let board_key = Keys::generate();
        let res = sign_res(
            &board_key,
            &board_id,
            &channel_of(&board_id),
            1,
            "書き込み",
            1_700_000_010,
        )
        .unwrap();
        let outcome = reg.accept_write(&board_id, &res, 1_700_000_010).unwrap();
        assert_eq!(outcome, AcceptOutcome::Numbered { res_no: 1, seq: 1 });

        // 両参加者へ RES + ORDER が seq 順に配布される。
        for rx in [&mut rx1, &mut rx2] {
            assert!(
                matches!(rx.try_recv(), Ok(WireMessage::Res { .. })),
                "RES 配布"
            );
            assert!(
                matches!(rx.try_recv(), Ok(WireMessage::Order { .. })),
                "ORDER 配布"
            );
        }
    }

    #[test]
    fn accept_write_dedups_resent_event() {
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);
        let board_key = Keys::generate();
        let res = sign_res(
            &board_key,
            &board_id,
            &channel_of(&board_id),
            1,
            "本文",
            1_700_000_010,
        )
        .unwrap();
        // 初回は採番、同一 event_id の再送は Duplicate(D1 — O1 を保つ)。
        assert_eq!(
            reg.accept_write(&board_id, &res, 1_700_000_010).unwrap(),
            AcceptOutcome::Numbered { res_no: 1, seq: 1 }
        );
        assert_eq!(
            reg.accept_write(&board_id, &res, 1_700_000_011).unwrap(),
            AcceptOutcome::Duplicate
        );
    }

    #[test]
    fn accept_write_assigns_consecutive_res_no() {
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);
        let board_key = Keys::generate();
        for (i, expected) in [(1u64, 1u16), (2, 2), (3, 3)] {
            let res = sign_res(
                &board_key,
                &board_id,
                &channel_of(&board_id),
                1,
                &format!("本文{i}"),
                1_700_000_010 + i,
            )
            .unwrap();
            let outcome = reg
                .accept_write(&board_id, &res, 1_700_000_010 + i)
                .unwrap();
            assert_eq!(
                outcome,
                AcceptOutcome::Numbered {
                    res_no: expected,
                    seq: expected as u32
                },
                "res_no・seq は欠番なく連番(T3/O2)"
            );
        }
    }

    #[test]
    fn accept_write_rejects_when_over_res_limit() {
        let p = persona();
        let board_id = p.public_key().to_hex();
        // res_limit=100 の下限で開設(BoardSettings 既定は 1000 だが、確実に上限へ到達させる
        // ため小さいスレを直接組む)。
        let reg = LivechatRegistry::new(128);
        let settings = BoardSettings {
            res_limit: 100,
            ..Default::default()
        };
        reg.open_thread(
            p.clone(),
            channel_of(&board_id),
            1,
            1_700_000_000,
            "実況スレ",
            settings,
            "198.51.100.1:7147",
        )
        .unwrap();
        // 上限まで採番を埋める。
        let board_key = Keys::generate();
        for i in 0..100u64 {
            let res = sign_res(
                &board_key,
                &board_id,
                &channel_of(&board_id),
                1,
                &format!("r{i}"),
                1_700_000_010 + i,
            )
            .unwrap();
            assert!(matches!(
                reg.accept_write(&board_id, &res, 1_700_000_010 + i)
                    .unwrap(),
                AcceptOutcome::Numbered { .. }
            ));
        }
        // 101 件目は上限超過で Rejected(NoOverLimit / T3)。
        let over = sign_res(
            &board_key,
            &board_id,
            &channel_of(&board_id),
            1,
            "溢れ",
            1_700_000_200,
        )
        .unwrap();
        assert_eq!(
            reg.accept_write(&board_id, &over, 1_700_000_200).unwrap(),
            AcceptOutcome::Rejected
        );
    }

    #[test]
    fn accept_write_rejects_when_frozen() {
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);
        {
            let mut hosts = lock(&reg.hosts);
            hosts
                .get_mut(&board_id)
                .unwrap()
                .host
                .thread
                .freeze()
                .unwrap();
        }
        let board_key = Keys::generate();
        let res = sign_res(
            &board_key,
            &board_id,
            &channel_of(&board_id),
            1,
            "凍結後",
            1_700_000_010,
        )
        .unwrap();
        // 非 Active(Frozen)への書き込みは採番されない(T1）。
        assert_eq!(
            reg.accept_write(&board_id, &res, 1_700_000_010).unwrap(),
            AcceptOutcome::Rejected
        );
    }

    #[test]
    fn accept_write_unregistered_participant_not_broadcast() {
        // 切断で unregister した参加者へは配布されない(outbox 除去)。
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        reg.register_participant(&board_id, "peer-1", tx);
        reg.unregister_participant(&board_id, "peer-1");

        let board_key = Keys::generate();
        let res = sign_res(
            &board_key,
            &board_id,
            &channel_of(&board_id),
            1,
            "本文",
            1_700_000_010,
        )
        .unwrap();
        reg.accept_write(&board_id, &res, 1_700_000_010).unwrap();
        // 登録解除済みなので何も届かない。
        assert!(rx.try_recv().is_err());
    }
}

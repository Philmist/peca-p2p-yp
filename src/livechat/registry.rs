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

use super::host::{HostThread, JoinDecision, Participant, SyncItem, board_settings_json};
use super::thread::{BoardSettings, BoardSettingsError, Res, Thread, ThreadError};

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
    /// 採番実績のある板鍵集合(pubkey hex)。**初見板鍵の PoW 判定**に使う(FR-021 / research R6 —
    /// 初見は `first_post_pow_bits`、既知は通常しきい値)。採番成功で当該板鍵を既知にする。
    known_board_keys: HashSet<String>,
    /// 板鍵ごとの書き込みレート窓(FR-021 — `thread_write_rate`)。30 秒窓内のレス数を数える。
    write_windows: HashMap<String, WriteRateWindow>,
    /// BAN 済み板鍵集合(T042 — thread-events.md 検証 5)。完全一致のみで判定する(FR-018)。
    banned_keys: HashSet<String>,
    /// ConnBan 済み接続元アドレス集合(T042 — FR-019)。HELLO 後 CLOSE で切断する
    /// (理由非開示 — thread-delivery.md §防御)。
    conn_banned: HashSet<String>,
    /// **直前(1 世代分)の凍結スレのスナップショット**(T055/compat-api.md §subject.txt/dat)。
    ///
    /// 次スレ移行時に旧 `Thread`(state・res を含む)を丸ごと複製して保持する。
    /// 互換 API の dat 追記不変性(MUST)は「取得済み key の応答が不変であること」までを
    /// 要求し、**保持していない過去世代の dat は定型 404 でよい**と契約が明記する
    /// (compat-api.md §dat「保持していない dat(…過去世代の未保持 key…)は定型 404」)。
    /// このため直近 1 世代のみを保持するベストエフォート実装とし、2 世代以上前へ遡る
    /// dat 要求は 404 になる(スレデータは揮発 — FR-015 の精神とも整合する)。
    frozen_snapshot: Option<Thread>,
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
    /// 板設定の値域違反(FR-025 — title/res_limit/noname_name/local_rules/pow の範囲外)。
    InvalidSettings(BoardSettingsError),
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
            RegistryError::InvalidSettings(_) => write!(f, "板設定の値が不正です"),
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

/// 互換 API 向けの板スナップショット([`LivechatRegistry::board_snapshot`])。
#[derive(Debug, Clone)]
pub struct BoardSnapshot {
    /// アクティブスレ(state は Active/Frozen/Closed のいずれもありうる — 通知なき
    /// 切断は本レジストリの外(参加者側)でのみ Frozen 化するため、ホスト自身から見た
    /// アクティブスレは基本 Active か Closed)。
    pub active: Thread,
    /// 直近 1 世代分の凍結スレ(次スレ移行前の旧スレ)。未保持・未移行なら `None`。
    pub frozen: Option<Thread>,
    /// 板設定(現行値。SETTING.TXT・head.txt の出力元)。
    pub settings: BoardSettings,
}

/// 板鍵単位の書き込みレート窓(FR-021 — `thread_write_rate`)。
///
/// 固定 30 秒窓(data-model §Settings で窓長は 30 秒固定)。窓を跨ぐと計数をリセットする。
/// `now`(unix 秒)を採番判定と同じ時刻源から注入する(テスト可能)。
#[derive(Debug, Clone)]
struct WriteRateWindow {
    window_start: u64,
    count: u32,
}

/// `thread_write_rate` の窓長(秒)。data-model §Settings で 30 秒固定。
const WRITE_RATE_WINDOW_SECS: u64 = 30;

/// `thread_write_rate` の既定値(板鍵あたり 30 秒で 4 レス — data-model §Settings)。
pub const DEFAULT_THREAD_WRITE_RATE: u32 = 4;

/// ホストレジストリ(板ごとの [`HostThread`] を共有保持する)。
pub struct LivechatRegistry {
    /// board_id(スレ主ペルソナ pubkey hex)→ ホスト状態。
    hosts: Mutex<HashMap<String, HostEntry>>,
    /// ホストの受入接続上限(Settings.thread_max_participants)。
    max_participants: usize,
    /// 板鍵あたりの書き込みレート上限(Settings.thread_write_rate — 30 秒窓内のレス数上限)。
    thread_write_rate: u32,
}

impl LivechatRegistry {
    /// レジストリを作る(`max_participants` は Settings.thread_max_participants)。
    ///
    /// `thread_write_rate` は既定値([`DEFAULT_THREAD_WRITE_RATE`])。設定値を反映する場合は
    /// [`new_with_rate`](Self::new_with_rate)を使う。
    pub fn new(max_participants: usize) -> Arc<Self> {
        Self::new_with_rate(max_participants, DEFAULT_THREAD_WRITE_RATE)
    }

    /// 書き込みレート上限を指定してレジストリを作る(Settings.thread_write_rate — FR-021)。
    pub fn new_with_rate(max_participants: usize, thread_write_rate: u32) -> Arc<Self> {
        Arc::new(Self {
            hosts: Mutex::new(HashMap::new()),
            max_participants,
            thread_write_rate,
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
                known_board_keys: HashSet::new(),
                write_windows: HashMap::new(),
                banned_keys: HashSet::new(),
                conn_banned: HashSet::new(),
                frozen_snapshot: None,
            },
        );
        Ok(())
    }

    /// 開設中の board_id 一覧(status・診断用)。
    pub fn board_ids(&self) -> Vec<String> {
        lock(&self.hosts).keys().cloned().collect()
    }

    /// 指定 board の現行スレ世代(自動移行の成立確認・診断用)。未知 board は `None`。
    pub fn board_generation(&self, board_id: &str) -> Option<u32> {
        lock(&self.hosts)
            .get(board_id)
            .map(|e| e.host.thread.generation)
    }

    /// 指定 board のホスト接続先 `tip`(announce の `tip` — スレ開設時に確定した自ノードの
    /// 到達アドレス。T065 のスレ一覧が視聴者へ提示する接続先)。未知 board は `None`。
    pub fn board_tip(&self, board_id: &str) -> Option<String> {
        lock(&self.hosts).get(board_id).map(|e| e.tip.clone())
    }

    // -------------------------------------------------- T054/T055: 互換 API 読み取り

    /// 互換 API(subject.txt/dat/SETTING.TXT/head.txt)向けの板スナップショット。
    ///
    /// アクティブスレ(`active`)+ 直近 1 世代分の凍結スレ(`frozen`。`None` は未保持)を
    /// 一括で読み取る(compat-api.md §subject.txt「アクティブスレ 1 行 + 凍結スレを
    /// 保持していればその行」)。板設定(`settings`)は板単位で共通(現行の設定 = 次スレへ
    /// 引き継がれる値。SETTING.TXT の出力元)。
    pub fn board_snapshot(&self, board_id: &str) -> Option<BoardSnapshot> {
        let hosts = lock(&self.hosts);
        let entry = hosts.get(board_id)?;
        Some(BoardSnapshot {
            active: entry.host.thread.clone(),
            frozen: entry.frozen_snapshot.clone(),
            settings: entry.host.settings.clone(),
        })
    }

    /// 指定 board・key(スレ作成 unix 秒)の `Thread` を読み取る(dat 取得用)。
    ///
    /// アクティブスレの key と一致すればアクティブスレを、直近 1 世代分の凍結スナップ
    /// ショットの key と一致すれば凍結スレを返す。いずれとも一致しない(未知の key・
    /// 2 世代以上前の key・クローズで削除済み)場合は `None`(呼び出し側は定型 404)。
    pub fn thread_by_key(&self, board_id: &str, key: u64) -> Option<Thread> {
        let hosts = lock(&self.hosts);
        let entry = hosts.get(board_id)?;
        if entry.host.thread.key == key {
            return Some(entry.host.thread.clone());
        }
        entry
            .frozen_snapshot
            .as_ref()
            .filter(|t| t.key == key)
            .cloned()
    }

    // ----------------------------------------------------------- T042: BAN

    /// 板鍵を BAN する(スレ主 — 採番拒否。thread-events.md 検証 5)。
    /// 未知 board は `false`(何もしない)。
    pub fn ban_board_key(&self, board_id: &str, board_key: &str) -> bool {
        let mut hosts = lock(&self.hosts);
        let Some(entry) = hosts.get_mut(board_id) else {
            return false;
        };
        entry.banned_keys.insert(board_key.to_string());
        true
    }

    /// 板鍵の BAN を解除する。未知 board は `false`。
    pub fn unban_board_key(&self, board_id: &str, board_key: &str) -> bool {
        let mut hosts = lock(&self.hosts);
        let Some(entry) = hosts.get_mut(board_id) else {
            return false;
        };
        entry.banned_keys.remove(board_key);
        true
    }

    /// 接続元アドレスを ConnBan する(スレ主 — 接続拒否。FR-019)。未知 board は `false`。
    pub fn ban_connection(&self, board_id: &str, addr: &str) -> bool {
        let mut hosts = lock(&self.hosts);
        let Some(entry) = hosts.get_mut(board_id) else {
            return false;
        };
        entry.conn_banned.insert(addr.to_string());
        true
    }

    /// 接続元アドレスの ConnBan を解除する。未知 board は `false`。
    pub fn unban_connection(&self, board_id: &str, addr: &str) -> bool {
        let mut hosts = lock(&self.hosts);
        let Some(entry) = hosts.get_mut(board_id) else {
            return false;
        };
        entry.conn_banned.remove(addr);
        true
    }

    /// 指定接続元アドレスが ConnBan 済みか(完全一致)。未知 board は `false`。
    pub fn is_conn_banned(&self, board_id: &str, addr: &str) -> bool {
        lock(&self.hosts)
            .get(board_id)
            .is_some_and(|e| e.conn_banned.contains(addr))
    }

    /// 指定板鍵が本ホストにとって既知(採番実績あり)か(T056 レビュー対応)。
    ///
    /// `accept_write` の PoW 判定(検証 6 — `is_first_post = !known_board_keys.contains`)
    /// と同じ情報源を外部公開する。bbs.cgi(自ノードホスト前提)は本 API で正確に
    /// 「PoW 計算が必要か」を判定できる — [`BoardKeyManager::existing_pubkey`] の
    /// 「ローカルに板鍵があるか」による近似では板鍵ローテーション後に破綻するため
    /// (ローテーション後の新鍵はローカルには存在するが、ホストにとっては未見)。
    /// 未知 board は `false`(初見として扱い PoW を要求する安全側)。
    pub fn is_known_board_key(&self, board_id: &str, board_key: &str) -> bool {
        lock(&self.hosts)
            .get(board_id)
            .is_some_and(|e| e.known_board_keys.contains(board_key))
    }

    /// BAN 済み板鍵一覧(UI/一覧用)。未知 board は空。
    pub fn banned_board_keys(&self, board_id: &str) -> Vec<String> {
        lock(&self.hosts)
            .get(board_id)
            .map(|e| e.banned_keys.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// 開設中の全スレの announce(kind 31311)を署名して返す(T019 — 60 秒間隔の定期発行)。
    ///
    /// 各 board のスレ主ペルソナ鍵で署名し、`tip` は開設時に保持した自ノードの到達アドレスを
    /// 使う。`expiration = created_at + 600` は封筒側([`ThreadAnnounce::sign`])が付与する。
    /// 実際の gossip 発行([`crate::p2p::hub::GossipHub::publish_local`])は配線側(main の
    /// 定期タスク)が行う — 本メソッドは「発行すべき Event を作る」ところまで(署名失敗の
    /// board は黙って飛ばす)。
    ///
    /// **クローズ済みスレは対象外**(T047 — announce の発行停止)。Frozen(凍結中)は
    /// 対象に含める(閲覧は継続するため一覧への掲載自体は続ける — announce 鮮度切れで
    /// 一覧から除去されるのは呼び出し側 gossip の鮮度規則に委ねる)。
    pub fn build_announce_events(&self, created_at: u64, pow_bits: u8) -> Vec<Event> {
        let hosts = lock(&self.hosts);
        let mut events = Vec::new();
        for entry in hosts.values() {
            if matches!(entry.host.thread.state, super::thread::ThreadState::Closed) {
                continue;
            }
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
        // 名無し名は「当該レス確定時点」の板設定値で焼き込む(FR-023/FR-024 —
        // dat 追記不変性の基盤。以後 noname_name が変わっても本レスの表示名は遡及しない)。
        let name = resolve_display_name(envelope.name.as_deref(), &entry.host.settings.noname_name);
        let domain = Res {
            event_id: event_id.clone(),
            board_key: res_event.pubkey.to_hex(),
            name,
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
        // Last-Modified の単調性(T055 レビュー対応)。ホスト受信時刻(`created_at` 引数)を
        // 基準にし、投稿者申告の created_at には依存しない。
        entry.host.thread.bump_last_confirmed_at(created_at as i64);

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
        // seed したレスの板鍵も既知扱い(以後の PoW は通常しきい値)。
        entry.known_board_keys.insert(res_event.pubkey.to_hex());
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
    ///    採番せず [`AcceptOutcome::Rejected`](res_limit ちょうどに達する採番自体は許可し、
    ///    確定後に手順 9 で次スレへ自動移行する — T046)。
    /// 4. **BAN(thread-events.md 検証 5 — spec Edge Case / T042)**: 板鍵が BAN 済みなら
    ///    採番せず [`AcceptOutcome::Rejected`](理由は応答で開示しない — FR-006)。
    /// 5. **PoW(thread-events.md 検証 6 — FR-021)**: 初見板鍵は `first_post_pow_bits` を満たす
    ///    こと(既知は通常しきい値)。不足は [`AcceptOutcome::Rejected`]。
    /// 6. **レート(thread-events.md 検証 7 — FR-021)**: 板鍵単位 `thread_write_rate` / 30 秒窓。
    ///    超過は [`AcceptOutcome::Rejected`](接続単位 `thread_msg_rate` は配線側 runtime)。
    /// 7. **採番(単点性 — Principle V)**: [`Thread::confirm`] で res_no を 1 つ割り当てる
    ///    (T3 を強制)。ORDER(kind 21311・seq 連番)をスレ主鍵で署名する。
    /// 8. **配布(`chan[p]` への Append)**: RES + ORDER を **全接続参加者(送信者含む)** の
    ///    outbox へ seq 順に送る。切断済み outbox への送信失敗は無視する(unregister は配線側)。
    /// 9. **上限到達時の自動次スレ移行(T046 — FR-013)**: 直前の確定で `res_no == res_limit`
    ///    に達したら、**配布と同じロック内**で
    ///    [`Self::migrate_to_next_generation_locked`] を呼び、旧スレを Frozen 化 + 新世代を
    ///    開始して `NEXT_THREAD` を配布する。同一ロック内で行うことで、移行境界(res_limit
    ///    到達の採番と次スレ開始の間)に他の書き込みが割り込む余地をなくす(移行境界の
    ///    二重採番なし — PlusCal モデルの検査前提)。
    ///
    /// 署名検証・形式は呼び出し前に [`Self::verify_incoming_res`] 等で済ませてある前提
    /// (モデル境界 — ADR-0014 §2)。本メソッドは検証通過後の**採番判定 + BAN/PoW/レート**を
    /// モデル化する。`created_at` はレート窓・ORDER 署名の時刻源。
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
        //
        // **移行境界の設計判断(ADR-0014 §4 D2 / docs/formal/livechat_sequencer.tla の
        // 「破棄」遷移に対応 — `w.gen < activeGen` は常に定型拒否を選ぶ)**: PlusCal モデルは
        // 「新スレへ採番」「定型拒否」の両方が安全(AssignedOnce 等の不変条件を保つ)ことを
        // 検査済みだが、実装では**常に定型拒否**を選ぶ。理由: 書き込みイベントは署名済みで
        // thread タグ(board_id・gen)を書き換えられないため、「新スレへ採番」を選ぶと
        // 採番先スレの世代とイベント内 thread タグの gen が食い違う。参加者側の ORDER 検証
        // (session.rs `apply_order` — thread-events.md §参加者側検証)はスレ主一致に加え
        // 対象スレの世代一致も見るため、この食い違いは参加者側で「別スレの ORDER」として
        // 拒否されかねず、かえって表示不一致のリスクを生む。常に定型拒否なら参加者は
        // NEXT_THREAD 受信後に再送すればよいだけで済み、より単純かつ安全側に倒せる。
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

        let board_key = res_event.pubkey.to_hex();
        // 3.5 BAN(thread-events.md 検証 5 — spec Edge Case / T042)。BAN 済み板鍵は採番せず
        //     Rejected。記録は配線側が livechat_write_rejected として行うが、応答で理由は
        //     開示しない(FR-006)。完全鍵照合のみ(短縮 ID 非適用 — FR-018)。
        if entry.banned_keys.contains(&board_key) {
            return Ok(AcceptOutcome::Rejected);
        }
        // 3.6 PoW(thread-events.md 検証 6 — FR-021 / research R6)。**初見板鍵**(採番実績なし)は
        //     `first_post_pow_bits` を満たすこと。既知板鍵は通常しきい値(0)。初回書き込みへ
        //     計算コストを課し、使い捨て板鍵の大量生成による荒らしを抑止する。
        let is_first_post = !entry.known_board_keys.contains(&board_key);
        if is_first_post {
            let required = entry.host.settings.first_post_pow_bits;
            if required > 0 && !res_event.check_pow(required) {
                return Ok(AcceptOutcome::Rejected);
            }
        }
        // 3.7 レート(thread-events.md 検証 7 — FR-021)。**板鍵単位**の書き込みレート
        //     (`thread_write_rate` / 30 秒窓)。窓を跨いだらリセットし、窓内で上限に達していれば
        //     採番せず破棄する(接続単位の `thread_msg_rate` は配線側 runtime が担う)。
        {
            let window = entry
                .write_windows
                .entry(board_key.clone())
                .or_insert(WriteRateWindow {
                    window_start: created_at,
                    count: 0,
                });
            if created_at.saturating_sub(window.window_start) >= WRITE_RATE_WINDOW_SECS {
                window.window_start = created_at;
                window.count = 0;
            }
            if window.count >= self.thread_write_rate {
                return Ok(AcceptOutcome::Rejected);
            }
        }

        // 4. 採番(単点性)。confirm が T3(res_no 欠番なし単調増加・上限)を強制する。
        let res_no = entry.host.thread.next_res_no();
        // 名無し名は「当該レス確定時点」の板設定値で焼き込む(FR-023/FR-024 —
        // dat 追記不変性の基盤。以後 noname_name が変わっても本レスの表示名は遡及しない。
        // レビュー対応: 従来は dat 出力時に *現在の* noname_name を後付けで解決していたため、
        // 板主が noname_name を変更すると配信済み dat の名無し行が書き換わり MUST 違反に
        // なっていた — 確定処理そのものへ焼き込むことで構造的に防ぐ)。
        let name = resolve_display_name(envelope.name.as_deref(), &entry.host.settings.noname_name);
        let domain = Res {
            event_id: event_id.clone(),
            board_key: res_event.pubkey.to_hex(),
            name,
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
        // Last-Modified の単調性(T055 レビュー対応 — キャッシュ汚染攻撃の防止)。
        // ホスト受信時刻(`accept_write` の `created_at` 引数)を基準にする。投稿者(参加者の
        // 板鍵)が申告する `res_event` 内の created_at は未検証(ホスト検証 1〜7 に時刻検査
        // なし)であり、過去日時を申告されると Last-Modified が後退し得るため使わない。
        entry.host.thread.bump_last_confirmed_at(created_at as i64);

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
        // 板鍵を既知にし(以後は通常 PoW しきい値)、レート窓の計数を進める(FR-021)。
        entry.known_board_keys.insert(board_key.clone());
        if let Some(window) = entry.write_windows.get_mut(&board_key) {
            window.count += 1;
        }

        // 5. 配布: RES + ORDER を全接続参加者(送信者含む)の outbox へ seq 順に送る。
        let res_msg = res_event_to_message(res_event);
        let order_msg = order_event_to_message(&order_event);
        for tx in entry.outboxes.values() {
            // 送信失敗(受信側キュー破棄 = 切断途上)は無視する。unregister は配線側(pump)。
            let _ = tx.send(res_msg.clone());
            let _ = tx.send(order_msg.clone());
        }

        // 5.5 上限到達時の自動次スレ移行(T046 — FR-013 の「res_no = res_limit 確定後」
        //     トリガー)。RES+ORDER の配布と同一ロック内で行う(移行境界の原子性)。
        //     新スレの key は本書き込みの created_at、title は現行スレの title を引き継ぐ
        //     (配信者の明示操作トリガー時に呼ぶ start_next_generation/web trait の
        //     next_thread(board_id) が title 引数を持たないのと一貫させる)。
        if res_no == entry.host.thread.res_limit {
            let title = entry.host.thread.title.clone();
            let _ = Self::migrate_to_next_generation_locked(board_id, entry, created_at, title);
        }

        Ok(AcceptOutcome::Numbered {
            res_no,
            seq: order.seq,
        })
    }

    /// 板主が板設定を変更し、全接続参加者へ即時配布する(T032 — FR-022/FR-023/FR-025)。
    ///
    /// - **値域検証(FR-025)**: [`BoardSettings::validate`] 違反は [`RegistryError::InvalidSettings`]
    ///   で拒否する(変更を適用せず配布もしない)。制御文字は [`BoardSettings::sanitized`] で除去。
    /// - **即時反映(FR-022)**: title・noname_name・local_rules・first_post_pow_bits は即時に
    ///   ホスト設定へ反映する(以後の WELCOME・PoW 判定・確定レス表示に効く)。
    /// - **res_limit は次スレから(FR-023)**: 設定値は保持するが**進行中スレの
    ///   [`Thread::res_limit`] は作成時スナップショットのまま変えない**(dat 追記不変性の基盤)。
    ///   次スレ作成(US5/T047)が新しい res_limit を採用する。
    /// - **配布(FR-023 — SETTINGS 即時配布)**: 全 outbox へ `Message::Settings{board_settings}` を
    ///   送る(registry → outbox。切断済み outbox への送信失敗は無視する)。
    ///
    /// 配布した板設定 JSON を返す(呼び出し側の確認・ログ用)。未知 board は
    /// [`RegistryError::UnknownBoard`]。
    pub fn update_settings(
        &self,
        board_id: &str,
        new: BoardSettings,
    ) -> Result<serde_json::Value, RegistryError> {
        // 制御文字除去 → 値域検証(FR-025)。違反は適用せず拒否する。
        let sanitized = new.sanitized();
        sanitized
            .validate()
            .map_err(RegistryError::InvalidSettings)?;

        let mut hosts = lock(&self.hosts);
        let entry = hosts.get_mut(board_id).ok_or(RegistryError::UnknownBoard)?;

        // 即時反映(res_limit も settings には入るが、進行中 Thread.res_limit は不変 — 次スレから)。
        entry.host.settings = sanitized;
        let board_settings = board_settings_json(&entry.host.settings);

        // 全接続参加者へ SETTINGS を即時配布(FR-023)。
        let msg = WireMessage::Settings {
            board_settings: board_settings.clone(),
        };
        for tx in entry.outboxes.values() {
            let _ = tx.send(msg.clone());
        }
        Ok(board_settings)
    }

    // ------------------------------------------------------- T046: 次スレ移行

    /// 次スレへ移行する(T046 — FR-012/FR-013・配信者の明示操作トリガー)。
    ///
    /// 公開 API(次スレ操作 API 相当)からの呼び出し口。上限到達トリガーは
    /// [`Self::accept_write`] が採番確定直後に**同一ロック内**で
    /// [`Self::migrate_to_next_generation_locked`] を呼ぶため、本メソッドとは別経路になる
    /// (両者とも同じ内部ヘルパーへ収束させ、移行手順の重複を避ける)。
    ///
    /// 未知 board は [`RegistryError::UnknownBoard`]。旧スレが既に非 Active(Frozen/Closed)
    /// なら移行しない([`RegistryError::Confirm`] で `T1` 違反を明示)。
    pub fn start_next_generation(
        &self,
        board_id: &str,
        new_key: u64,
        title: impl Into<String>,
    ) -> Result<u32, RegistryError> {
        let mut hosts = lock(&self.hosts);
        let entry = hosts.get_mut(board_id).ok_or(RegistryError::UnknownBoard)?;
        Self::migrate_to_next_generation_locked(board_id, entry, new_key, title.into())
    }

    /// 次スレ移行の実処理(T046 — [`Self::start_next_generation`] と
    /// [`Self::accept_write`] の上限到達検出の**両方から呼ばれる内部ヘルパー**)。
    ///
    /// **PlusCal モデル(docs/formal/livechat_sequencer.tla)の「次スレ移行」遷移に対応**。
    /// トリガーは res_no = res_limit 確定後 **または** 配信者の明示操作(次スレ操作 API)の
    /// いずれか — モデルは両方を「Active 中の任意時点で移行しうる」に抽象化しており(モデル
    /// ヘッダコメント参照)、実装も呼び出し元がどちらでも本ヘルパーへ収束させることで
    /// 移行手順(freeze → 新世代作成 → NEXT_THREAD 配布)の実装を一箇所に保つ。
    ///
    /// **原子性**: `accept_write` から呼ぶ場合、採番確定([`Thread::confirm`])から
    /// 移行完了までを**同一の `hosts` ロック内**で行う(呼び出し元が `MutexGuard` 越しの
    /// `entry` を渡すため、ロックを跨がない)。これにより「res_no = res_limit の確定」と
    /// 「移行(Frozen 化 + NEXT_THREAD 配布)」の間に他の書き込みが割り込む余地がなく、
    /// 移行境界の二重採番(PlusCal モデルの検査前提)を構造的に防ぐ。
    ///
    /// 手順(モデルの `frozenLen[activeGen] := Len(log[activeGen])` → `chan` への `next` 追加 →
    /// `activeGen := ng` に対応):
    ///
    /// 1. **旧スレを Frozen に**(`Thread::freeze` — 不変条件 T1。以後 [`Self::accept_write`] は
    ///    旧世代宛を定型拒否する。書き込み不可・閲覧は継続)。
    /// 2. **新 `HostThread` を作る**(`gen + 1`・新 `key`(スレ作成秒)・`res_limit` は
    ///    **現在の板設定**の値を採用する — FR-023「res_limit は次スレから」の適用点)。
    /// 3. **板スコープの状態は引き継ぐ**(T049 — 板鍵 BAN・ConnBan・板設定・**既知板鍵
    ///    (`known_board_keys`)・書き込みレート窓(`write_windows`)は板 = ペルソナ単位の
    ///    スコープでありスレ(世代)に依存しないため、次スレ移行後も保持する**。research R6 は
    ///    「板鍵が当該**板**で未知(初見)なら first_post_pow_bits を要求」と規定しており、
    ///    世代単位の判定ではない — 移行のたびに既知板鍵を初見扱いへ戻すと、移行済みの
    ///    書き込み者全員へ不要な PoW 再計算を強いてしまう。`assigned_ids`・`res_events`・
    ///    `order_events` は世代固有(採番実績そのもの)のためリセットする)。
    /// 4. **`NEXT_THREAD` を全接続参加者へ配布**(`chan[p]` への Append)。参加者は受信して
    ///    `knownGen` を更新し、以後の書き込みは新世代宛になる。
    fn migrate_to_next_generation_locked(
        board_id: &str,
        entry: &mut HostEntry,
        new_key: u64,
        title: String,
    ) -> Result<u32, RegistryError> {
        // 1. 旧スレを Frozen に(T1 — Active でなければ移行しない)。
        entry
            .host
            .thread
            .freeze()
            .map_err(|_| RegistryError::Confirm(ThreadError::NotActive))?;

        // 2. 新 HostThread を作る(res_limit は現在の板設定 — FR-023「次スレから」)。
        let new_generation = entry.host.thread.generation + 1;
        let channel = entry.host.thread.channel.clone();
        let settings = entry.host.settings.clone();
        let new_thread = Thread::new(
            board_id,
            channel,
            new_generation,
            new_key,
            title,
            settings.res_limit,
        );
        let new_host = HostThread::new(new_thread, settings);
        let old_participants: Vec<Participant> = entry.host.participants().to_vec();
        // T055: 旧スレ(Frozen 済み)のスナップショットを直近 1 世代分だけ保持する
        // (compat-api.md §dat「凍結スレを保持していれば」— 互換 API の subject.txt/dat が
        // 移行後も旧世代を参照できるようにする。2 世代以上前は破棄されベストエフォート)。
        entry.frozen_snapshot = Some(entry.host.thread.clone());
        entry.host = new_host;
        // 旧スレの参加者登録(接続維持中)は新スレへそのまま引き継ぐ(接続自体は継続し、
        // 対象スレだけが切り替わる — outbox は不変)。
        for p in &old_participants {
            entry.host.register_participant(p.peer_id.clone());
        }

        // 3. 世代固有の状態のみリセットする(板スコープの BAN/ConnBan/settings/既知板鍵/
        //    レート窓は引き継ぐ — T049 / research R6)。
        entry.res_events.clear();
        entry.order_events.clear();
        entry.assigned_ids.clear();

        // 4. NEXT_THREAD を全接続参加者へ配布。
        let msg = WireMessage::NextThread {
            generation: new_generation,
            key: new_key,
        };
        for tx in entry.outboxes.values() {
            let _ = tx.send(msg.clone());
        }
        Ok(new_generation)
    }

    // --------------------------------------------------------- T047: 明示クローズ

    /// スレを明示クローズする(T047 — FR-014/FR-015)。
    ///
    /// **PlusCal モデルの「明示クローズ」遷移に対応**(`phase := "closed"` →
    /// `pending := {}` → `chan` への `close` 追加)。以後 [`Self::accept_write`] は
    /// `check_writable` が非 Active を検出して常に定型拒否する(T1)。
    ///
    /// 手順:
    /// 1. **スレ主署名付き THREAD_CLOSE を生成**([`ThreadClose::sign`] — kind 21311 の
    ///    `["peca","close"]` 特殊形。署名鍵はスレ主ペルソナ = `entry.persona`)。
    /// 2. **`Thread::close`** で終端状態へ遷移(不変条件 T1。Closed は再遷移不可)。
    /// 3. **`THREAD_CLOSE` を全接続参加者へ配布**。受信側([`crate::livechat::session`])は
    ///    スレデータを削除する(揮発 — FR-015)。announce の発行停止は呼び出し側
    ///    ([`Self::build_announce_events`] は Closed スレを対象外にする)が担う。
    /// 4. **ホスト側のスレデータ・凍結スナップショットも削除する**(FR-015 — 揮発は参加者
    ///    側だけでなくホスト自身にも適用される)。互換 API の dat は以後 404 になる
    ///    (compat-api.md §dat「クローズで削除済み」の dat は定型 404)。
    ///
    /// クローズ済みイベントを返す(呼び出し側が announce 停止・ログ等に使う)。未知 board は
    /// [`RegistryError::UnknownBoard`]、署名構築失敗は [`RegistryError::Build`]。
    pub fn close_thread(&self, board_id: &str, created_at: u64) -> Result<Event, RegistryError> {
        let mut hosts = lock(&self.hosts);
        let entry = hosts.get_mut(board_id).ok_or(RegistryError::UnknownBoard)?;

        let close = livechat::ThreadClose {
            board_id: board_id.to_string(),
            generation: entry.host.thread.generation,
        };
        let close_event = close
            .sign(&entry.persona, created_at)
            .map_err(RegistryError::Build)?;

        entry
            .host
            .thread
            .close()
            .map_err(|_| RegistryError::Confirm(ThreadError::NotActive))?;

        let msg = WireMessage::ThreadClose {
            event: serde_json::to_value(&close_event).unwrap_or(serde_json::Value::Null),
        };
        for tx in entry.outboxes.values() {
            let _ = tx.send(msg.clone());
        }

        // 4. 揮発(FR-015): 確定レス・凍結スナップショットを削除する。Thread の state
        //    (Closed)は互換 API の subject.txt/dat 404 判定に使うため保持する。
        entry.host.thread.res.clear();
        entry.frozen_snapshot = None;

        Ok(close_event)
    }

    /// スレがクローズ済みか(announce 発行停止の判定 — T047)。未知 board は `true`
    /// (announce を出さない安全側)。
    pub fn is_closed(&self, board_id: &str) -> bool {
        lock(&self.hosts)
            .get(board_id)
            .map(|e| matches!(e.host.thread.state, super::thread::ThreadState::Closed))
            .unwrap_or(true)
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

/// レス確定時点の表示名を解決する(FR-023/FR-024 — dat 追記不変性の基盤)。
///
/// 名前欄が空・未指定なら**当該レス確定時点**の `noname_name` を返し、`Res::name` へ
/// 焼き込む(`Some` として確定させる)。以後 `noname_name` が変更されても、既に確定した
/// レスの表示名は遡及して変わらない(T055 レビュー対応 — 従来は dat 出力時に都度
/// 現在の板設定を参照しており、板主が noname_name を変更すると配信済み dat の名無し行が
/// 書き換わって追記不変性(MUST)に違反していた)。
fn resolve_display_name(name: Option<&str>, noname_name: &str) -> Option<String> {
    match name {
        Some(n) if !n.is_empty() => Some(n.to_string()),
        _ => Some(noname_name.to_string()),
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
        // 採番・配布のテストは PoW を対象にしないため first_post_pow_bits=0 で開設する
        // (PoW/レートは専用テストで検証する)。
        let settings = BoardSettings {
            first_post_pow_bits: 0,
            ..Default::default()
        };
        reg.open_thread(
            persona.clone(),
            channel_of(&board_id),
            1,
            1_700_000_000,
            "実況スレ",
            settings,
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
        // ため小さいスレを直接組む)。PoW/レートは本テストの対象外なので回避する
        // (first_post_pow_bits=0・レート上限を大きく取る)。
        let reg = LivechatRegistry::new_with_rate(128, 10_000);
        let settings = BoardSettings {
            res_limit: 100,
            first_post_pow_bits: 0,
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
        // 100 件目の確定と同時に自動的に次スレ(gen=2)へ移行しているため(T046)、
        // 旧世代(gen=1)宛の 101 件目は移行境界の定型拒否(ADR-0014 D2)で Rejected になる
        // (NoOverLimit 到達そのものではなく世代不一致が理由 — 自動移行の検証は
        // res_limit_reached_migrates_automatically_after_final_confirm を参照)。
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
    fn res_limit_reached_migrates_automatically_after_final_confirm() {
        // T046: 「res_no = res_limit 確定後」トリガーの自動移行。tasks.md の要件どおり、
        // 明示操作(start_next_generation の手動呼び出し)なしで、上限到達の採番確定と
        // 同一の accept_write 呼び出し内で次スレが開始されることを確認する。
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = LivechatRegistry::new_with_rate(128, 10_000);
        let settings = BoardSettings {
            res_limit: crate::livechat::thread::RES_LIMIT_MIN,
            first_post_pow_bits: 0,
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
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        reg.register_participant(&board_id, "peer-1", tx);

        let board_key = Keys::generate();
        let limit = crate::livechat::thread::RES_LIMIT_MIN as u64;
        for i in 0..limit {
            let res = sign_res(
                &board_key,
                &board_id,
                &channel_of(&board_id),
                1,
                &format!("r{i}"),
                1_700_000_010 + i,
            )
            .unwrap();
            let outcome = reg
                .accept_write(&board_id, &res, 1_700_000_010 + i)
                .unwrap();
            assert!(
                matches!(outcome, AcceptOutcome::Numbered { .. }),
                "書き込み {i} は採番されるべき: {outcome:?}"
            );
            // 各書き込みで RES + ORDER を読み捨てる。最後の 1 件(上限到達)だけ
            // 続けて NEXT_THREAD も配布されるので、その分は後段でまとめて確認する。
            let _ = rx.try_recv();
            let _ = rx.try_recv();
        }

        // 上限到達の確定と同一呼び出し内で次スレが自動的に開始されている。
        // NEXT_THREAD が最後の RES/ORDER に続けて配布されていることを確認する。
        match rx.try_recv() {
            Ok(WireMessage::NextThread { generation, .. }) => {
                assert_eq!(
                    generation, 2,
                    "res_limit 到達確定と同時に世代 2 へ自動移行する"
                )
            }
            other => panic!("NEXT_THREAD が自動配布されるべき: {other:?}"),
        }

        // 旧世代(gen=1)宛の追加書き込みは拒否される(手動 start_next_generation なしで
        // 既に Frozen 化されている)。
        let old_gen_write = sign_res(
            &board_key,
            &board_id,
            &channel_of(&board_id),
            1,
            "移行後の旧世代宛",
            1_700_000_500,
        )
        .unwrap();
        assert_eq!(
            reg.accept_write(&board_id, &old_gen_write, 1_700_000_500)
                .unwrap(),
            AcceptOutcome::Rejected,
            "自動移行後は旧世代宛が拒否される"
        );

        // 新世代(gen=2)宛の書き込みは res_no=1 から採番される(手動呼び出しなしで有効)。
        let new_gen_write = sign_res(
            &board_key,
            &board_id,
            &channel_of(&board_id),
            2,
            "自動移行後の新スレへの投稿",
            1_700_000_500,
        )
        .unwrap();
        assert_eq!(
            reg.accept_write(&board_id, &new_gen_write, 1_700_000_500)
                .unwrap(),
            AcceptOutcome::Numbered { res_no: 1, seq: 1 },
            "手動呼び出しなしで開始された新スレへ正しく採番される"
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

    // --- T030: PoW(初見板鍵)・レート(板鍵単位)— FR-021 -------------------

    /// PoW 付きで kind 1311 を署名する(初見板鍵テスト用)。
    fn sign_res_pow(
        board_key: &Keys,
        board_id: &str,
        channel: &str,
        body: &str,
        created_at: u64,
        pow_bits: u8,
    ) -> Event {
        crate::event::livechat::Res {
            channel: channel.to_string(),
            board_id: board_id.to_string(),
            generation: 1,
            name: None,
            mail: None,
            body: body.to_string(),
        }
        .sign(board_key, created_at, pow_bits)
        .unwrap()
    }

    #[test]
    fn accept_write_requires_pow_for_first_post() {
        let p = persona();
        let board_id = p.public_key().to_hex();
        // first_post_pow_bits=8(初見板鍵に 8 ビット PoW を要求)。
        let reg = LivechatRegistry::new_with_rate(128, 10_000);
        let settings = BoardSettings {
            first_post_pow_bits: 8,
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
        let ch = channel_of(&board_id);
        let board_key = Keys::generate();

        // PoW なしの初見板鍵は Rejected(FR-021)。
        let no_pow = sign_res(&board_key, &board_id, &ch, 1, "初回", 1_700_000_010).unwrap();
        assert_eq!(
            reg.accept_write(&board_id, &no_pow, 1_700_000_010).unwrap(),
            AcceptOutcome::Rejected
        );

        // PoW 8 付きの初回書き込みは Numbered。以後は既知板鍵になり PoW 不要。
        let pow = sign_res_pow(&board_key, &board_id, &ch, "初回", 1_700_000_011, 8);
        assert!(matches!(
            reg.accept_write(&board_id, &pow, 1_700_000_011).unwrap(),
            AcceptOutcome::Numbered { .. }
        ));
        // 2 回目(既知板鍵)は PoW なしでも Numbered(通常しきい値 0)。
        let second = sign_res(&board_key, &board_id, &ch, 1, "二回目", 1_700_000_012).unwrap();
        assert!(matches!(
            reg.accept_write(&board_id, &second, 1_700_000_012).unwrap(),
            AcceptOutcome::Numbered { .. }
        ));
    }

    #[test]
    fn rotated_board_key_is_treated_as_first_post_again() {
        // T044: 板鍵ローテーション(= ホストから見て未知の新しい Keys)は、既存鍵が
        // 既知でも「新しい鍵にとっては初見」なので再び PoW が要求される機序を確認する
        // (accept_write_requires_pow_for_first_post と同じ機序を「ローテーション後」の
        // 文脈で確認する回帰テスト)。
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = LivechatRegistry::new_with_rate(128, 10_000);
        let settings = BoardSettings {
            first_post_pow_bits: 8,
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
        let ch = channel_of(&board_id);

        // 旧鍵で PoW 付き初回書き込みを済ませ、既知板鍵にする。
        let old_key = Keys::generate();
        let pow_old = sign_res_pow(&old_key, &board_id, &ch, "旧鍵初回", 1_700_000_011, 8);
        assert!(matches!(
            reg.accept_write(&board_id, &pow_old, 1_700_000_011)
                .unwrap(),
            AcceptOutcome::Numbered { .. }
        ));

        // ローテーション相当 = 新しい Keys(ホストから見て未知の板鍵)。
        let new_key = Keys::generate();
        let no_pow_new = sign_res(&new_key, &board_id, &ch, 1, "新鍵初回", 1_700_000_012).unwrap();
        assert_eq!(
            reg.accept_write(&board_id, &no_pow_new, 1_700_000_012)
                .unwrap(),
            AcceptOutcome::Rejected,
            "ローテーション直後の新鍵は PoW なしだと拒否される"
        );

        // PoW 付きなら新鍵でも Numbered。
        let pow_new = sign_res_pow(&new_key, &board_id, &ch, "新鍵初回", 1_700_000_013, 8);
        assert!(matches!(
            reg.accept_write(&board_id, &pow_new, 1_700_000_013)
                .unwrap(),
            AcceptOutcome::Numbered { .. }
        ));
        // 新鍵の 2 回目は PoW 不要(既知になった)。
        let second_new =
            sign_res(&new_key, &board_id, &ch, 1, "新鍵二回目", 1_700_000_014).unwrap();
        assert!(matches!(
            reg.accept_write(&board_id, &second_new, 1_700_000_014)
                .unwrap(),
            AcceptOutcome::Numbered { .. }
        ));
    }

    #[test]
    fn accept_write_enforces_board_key_rate() {
        let p = persona();
        let board_id = p.public_key().to_hex();
        // thread_write_rate=4(30 秒窓内 4 レス)・PoW なし。
        let reg = LivechatRegistry::new_with_rate(128, 4);
        let settings = BoardSettings {
            first_post_pow_bits: 0,
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
        let ch = channel_of(&board_id);
        let board_key = Keys::generate();

        // 同一秒内に 4 レスは採番(窓内上限まで)。
        for i in 0..4u64 {
            let res = sign_res(
                &board_key,
                &board_id,
                &ch,
                1,
                &format!("r{i}"),
                1_700_000_010 + i,
            )
            .unwrap();
            assert!(
                matches!(
                    reg.accept_write(&board_id, &res, 1_700_000_010).unwrap(),
                    AcceptOutcome::Numbered { .. }
                ),
                "窓内 {i} 件目は採番される"
            );
        }
        // 5 件目(同一窓)はレート超過で Rejected。
        let over = sign_res(&board_key, &board_id, &ch, 1, "5件目", 1_700_000_014).unwrap();
        assert_eq!(
            reg.accept_write(&board_id, &over, 1_700_000_010).unwrap(),
            AcceptOutcome::Rejected
        );

        // 30 秒窓を跨ぐとリセットされ、再び採番できる。
        let after = sign_res(&board_key, &board_id, &ch, 1, "窓明け", 1_700_000_014).unwrap();
        assert!(matches!(
            reg.accept_write(&board_id, &after, 1_700_000_050).unwrap(),
            AcceptOutcome::Numbered { .. }
        ));
    }

    // --- T032: 板設定の変更と配布(FR-022/FR-023/FR-025)---------------------

    #[test]
    fn update_settings_distributes_to_all_participants() {
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);
        let (tx1, mut rx1) = tokio::sync::mpsc::unbounded_channel();
        let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel();
        reg.register_participant(&board_id, "peer-1", tx1);
        reg.register_participant(&board_id, "peer-2", tx2);

        let new = BoardSettings {
            title: "新タイトル".into(),
            noname_name: "新名無し".into(),
            local_rules: "新ルール".into(),
            first_post_pow_bits: 12,
            ..Default::default()
        };
        let json = reg.update_settings(&board_id, new).unwrap();
        assert_eq!(json["title"], "新タイトル");
        assert_eq!(json["noname_name"], "新名無し");
        assert_eq!(json["first_post_pow_bits"], 12);

        // 全参加者へ SETTINGS が配布される(FR-023)。
        for rx in [&mut rx1, &mut rx2] {
            match rx.try_recv() {
                Ok(WireMessage::Settings { board_settings }) => {
                    assert_eq!(board_settings["title"], "新タイトル");
                }
                other => panic!("SETTINGS を期待: {other:?}"),
            }
        }
    }

    #[test]
    fn update_settings_rejects_invalid_values() {
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);
        // res_limit=50(範囲外・100 未満)は値域違反で拒否(FR-025)。
        let bad = BoardSettings {
            res_limit: 50,
            ..Default::default()
        };
        assert!(matches!(
            reg.update_settings(&board_id, bad),
            Err(RegistryError::InvalidSettings(_))
        ));
    }

    #[test]
    fn update_settings_res_limit_does_not_shrink_active_thread() {
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);
        // 進行中スレの res_limit は既定 1000。設定を res_limit=200 に変えても、
        // Active スレの res_limit は作成時スナップショット(1000)のまま(FR-023 — 次スレから)。
        let new = BoardSettings {
            res_limit: 200,
            ..Default::default()
        };
        reg.update_settings(&board_id, new).unwrap();
        let hosts = lock(&reg.hosts);
        let entry = hosts.get(&board_id).unwrap();
        assert_eq!(
            entry.host.thread.res_limit, 1000,
            "進行中スレの res_limit は変わらない(次スレから適用)"
        );
        // ただし settings 側は新値を保持(次スレ作成が採用する)。
        assert_eq!(entry.host.settings.res_limit, 200);
    }

    // --- T042: ホスト側 BAN(thread-events.md 検証 5 / FR-006 / FR-019)-------

    #[test]
    fn accept_write_rejects_banned_board_key() {
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);
        let board_key = Keys::generate();
        let banned_key_hex = board_key.public_key().to_hex();
        assert!(reg.ban_board_key(&board_id, &banned_key_hex));

        let res = sign_res(
            &board_key,
            &board_id,
            &channel_of(&board_id),
            1,
            "BAN 済み鍵からの投稿",
            1_700_000_010,
        )
        .unwrap();
        assert_eq!(
            reg.accept_write(&board_id, &res, 1_700_000_010).unwrap(),
            AcceptOutcome::Rejected,
            "BAN 済み板鍵は採番されない"
        );
    }

    #[test]
    fn accept_write_does_not_broadcast_banned_key_res() {
        // 配布(outbox)も発生しない = 他参加者へ一切届かない(FR-019)。
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        reg.register_participant(&board_id, "peer-1", tx);

        let board_key = Keys::generate();
        reg.ban_board_key(&board_id, &board_key.public_key().to_hex());
        let res = sign_res(
            &board_key,
            &board_id,
            &channel_of(&board_id),
            1,
            "BAN 済み",
            1_700_000_010,
        )
        .unwrap();
        reg.accept_write(&board_id, &res, 1_700_000_010).unwrap();
        assert!(rx.try_recv().is_err(), "BAN 済み鍵の書き込みは配布されない");
    }

    #[test]
    fn accept_write_allows_non_banned_key() {
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);
        let banned = Keys::generate();
        reg.ban_board_key(&board_id, &banned.public_key().to_hex());

        // BAN されていない別鍵は通常どおり採番される。
        let ok_key = Keys::generate();
        let res = sign_res(
            &ok_key,
            &board_id,
            &channel_of(&board_id),
            1,
            "通常の投稿",
            1_700_000_010,
        )
        .unwrap();
        assert!(matches!(
            reg.accept_write(&board_id, &res, 1_700_000_010).unwrap(),
            AcceptOutcome::Numbered { .. }
        ));
    }

    #[test]
    fn unban_board_key_restores_write_access() {
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);
        let board_key = Keys::generate();
        let key_hex = board_key.public_key().to_hex();
        reg.ban_board_key(&board_id, &key_hex);
        assert!(reg.unban_board_key(&board_id, &key_hex));

        let res = sign_res(
            &board_key,
            &board_id,
            &channel_of(&board_id),
            1,
            "解除後の投稿",
            1_700_000_010,
        )
        .unwrap();
        assert!(matches!(
            reg.accept_write(&board_id, &res, 1_700_000_010).unwrap(),
            AcceptOutcome::Numbered { .. }
        ));
    }

    #[test]
    fn ban_operations_on_unknown_board_return_false() {
        let reg = LivechatRegistry::new(128);
        assert!(!reg.ban_board_key("unknown", "key"));
        assert!(!reg.unban_board_key("unknown", "key"));
        assert!(!reg.ban_connection("unknown", "addr"));
        assert!(!reg.unban_connection("unknown", "addr"));
        assert!(!reg.is_conn_banned("unknown", "addr"));
        assert!(reg.banned_board_keys("unknown").is_empty());
    }

    #[test]
    fn conn_ban_applies_and_lifts() {
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);
        assert!(!reg.is_conn_banned(&board_id, "203.0.113.5:7147"));
        assert!(reg.ban_connection(&board_id, "203.0.113.5:7147"));
        assert!(reg.is_conn_banned(&board_id, "203.0.113.5:7147"));
        // 別アドレスには影響しない。
        assert!(!reg.is_conn_banned(&board_id, "203.0.113.6:7147"));

        assert!(reg.unban_connection(&board_id, "203.0.113.5:7147"));
        assert!(!reg.is_conn_banned(&board_id, "203.0.113.5:7147"));
    }

    #[test]
    fn banned_board_keys_lists_current_bans() {
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);
        let k1 = Keys::generate().public_key().to_hex();
        let k2 = Keys::generate().public_key().to_hex();
        reg.ban_board_key(&board_id, &k1);
        reg.ban_board_key(&board_id, &k2);
        let mut listed = reg.banned_board_keys(&board_id);
        listed.sort();
        let mut expected = vec![k1, k2];
        expected.sort();
        assert_eq!(listed, expected);
    }

    // --- T046: 次スレ移行(FR-012/FR-013)-------------------------------------

    #[test]
    fn start_next_generation_freezes_old_and_activates_new() {
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);

        let new_gen = reg
            .start_next_generation(&board_id, 1_700_001_000, "実況スレ その2")
            .unwrap();
        assert_eq!(new_gen, 2, "世代は 1 → 2 へ単調増加");

        let hosts = lock(&reg.hosts);
        let entry = hosts.get(&board_id).unwrap();
        assert_eq!(entry.host.thread.generation, 2, "新スレが Active");
        assert_eq!(
            entry.host.thread.state,
            crate::livechat::thread::ThreadState::Active
        );
        assert_eq!(entry.host.thread.key, 1_700_001_000);
        assert_eq!(entry.host.thread.title, "実況スレ その2");
    }

    #[test]
    fn start_next_generation_broadcasts_next_thread_to_participants() {
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        reg.register_participant(&board_id, "peer-1", tx);

        reg.start_next_generation(&board_id, 1_700_001_000, "次スレ")
            .unwrap();

        match rx.try_recv() {
            Ok(WireMessage::NextThread { generation, key }) => {
                assert_eq!(generation, 2);
                assert_eq!(key, 1_700_001_000);
            }
            other => panic!("NEXT_THREAD を期待: {other:?}"),
        }
    }

    #[test]
    fn old_generation_write_is_rejected_after_migration() {
        // 移行境界の設計判断(ADR-0014 D2): 旧世代宛の書き込みは常に定型拒否。
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);
        let board_key = Keys::generate();

        // 旧世代(gen=1)向けの署名済みレスを作っておく(署名後は書き換え不可)。
        let old_gen_res = sign_res(
            &board_key,
            &board_id,
            &channel_of(&board_id),
            1,
            "移行前に署名した書き込み",
            1_700_000_900,
        )
        .unwrap();

        reg.start_next_generation(&board_id, 1_700_001_000, "次スレ")
            .unwrap();

        // 移行後に届いた旧世代(gen=1)宛の書き込みは定型拒否(新スレへの誤採番はしない)。
        assert_eq!(
            reg.accept_write(&board_id, &old_gen_res, 1_700_001_010)
                .unwrap(),
            AcceptOutcome::Rejected,
            "旧世代宛の書き込みは新世代へ誤採番せず拒否する"
        );

        // 新世代(gen=2)宛の書き込みは通常どおり採番される。
        let new_gen_res = sign_res(
            &board_key,
            &board_id,
            &channel_of(&board_id),
            2,
            "次スレへの書き込み",
            1_700_001_010,
        )
        .unwrap();
        assert!(matches!(
            reg.accept_write(&board_id, &new_gen_res, 1_700_001_010)
                .unwrap(),
            AcceptOutcome::Numbered { res_no: 1, .. }
        ));
    }

    #[test]
    fn start_next_generation_preserves_participants_across_migration() {
        // 移行後も接続(outbox)は維持され、新世代の配布を受け取れる。
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        reg.register_participant(&board_id, "peer-1", tx);

        reg.start_next_generation(&board_id, 1_700_001_000, "次スレ")
            .unwrap();
        let _ = rx.try_recv(); // NEXT_THREAD を読み捨てる

        let board_key = Keys::generate();
        let res = sign_res(
            &board_key,
            &board_id,
            &channel_of(&board_id),
            2,
            "新スレへの書き込み",
            1_700_001_010,
        )
        .unwrap();
        reg.accept_write(&board_id, &res, 1_700_001_010).unwrap();

        // 引き継がれた接続が新スレの RES + ORDER を受け取れる。
        assert!(matches!(rx.try_recv(), Ok(WireMessage::Res { .. })));
        assert!(matches!(rx.try_recv(), Ok(WireMessage::Order { .. })));
    }

    #[test]
    fn start_next_generation_resets_numbering_but_keeps_board_scope() {
        // T049: 板単位スコープ(BAN・板設定・既知板鍵・レート窓)は次スレへ引き継がれるが、
        // 世代固有の採番実績(assigned_ids・res_events・order_events)はリセットされ、
        // 新世代の res_no は 1 から再開する。
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);
        let board_key = Keys::generate();
        let banned = Keys::generate();

        // 旧スレで板鍵を BAN し、別の板鍵で 1 レス採番しておく(既知板鍵化)。
        reg.ban_board_key(&board_id, &banned.public_key().to_hex());
        let res = sign_res(
            &board_key,
            &board_id,
            &channel_of(&board_id),
            1,
            "旧スレへの投稿",
            1_700_000_900,
        )
        .unwrap();
        reg.accept_write(&board_id, &res, 1_700_000_900).unwrap();

        reg.start_next_generation(&board_id, 1_700_001_000, "次スレ")
            .unwrap();

        // 板単位スコープ(BAN)は引き継がれる。
        let banned_res = sign_res(
            &banned,
            &board_id,
            &channel_of(&board_id),
            2,
            "BAN 済み鍵からの投稿(次スレ)",
            1_700_001_010,
        )
        .unwrap();
        assert_eq!(
            reg.accept_write(&board_id, &banned_res, 1_700_001_010)
                .unwrap(),
            AcceptOutcome::Rejected,
            "板鍵 BAN は次スレへ引き継がれる(FR-012 板単位スコープ)"
        );

        // 世代固有の状態(採番実績)はリセットされ、新世代の res_no は 1 から始まる。
        let new_res = sign_res(
            &board_key,
            &board_id,
            &channel_of(&board_id),
            2,
            "新スレへの投稿",
            1_700_001_010,
        )
        .unwrap();
        assert_eq!(
            reg.accept_write(&board_id, &new_res, 1_700_001_010)
                .unwrap(),
            AcceptOutcome::Numbered { res_no: 1, seq: 1 },
            "新世代の採番は res_no=1 から再開する"
        );
    }

    #[test]
    fn start_next_generation_keeps_known_board_keys_without_repeat_pow() {
        // research R6: 「板鍵が当該板で未知(初見)なら first_post_pow_bits を要求」は板
        // スコープの判定であり世代単位ではない。既知板鍵(旧スレで採番実績あり)は次スレ
        // 移行後も PoW なしで書き込めることを確認する(T049 — known_board_keys の引き継ぎ)。
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = LivechatRegistry::new_with_rate(128, 10_000);
        let settings = BoardSettings {
            first_post_pow_bits: 8,
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
        let board_key = Keys::generate();
        let ch = channel_of(&board_id);

        // 旧スレで PoW 付き初回書き込みを済ませ、既知板鍵にする。
        let pow_first = crate::event::livechat::Res {
            channel: ch.clone(),
            board_id: board_id.clone(),
            generation: 1,
            name: None,
            mail: None,
            body: "旧スレ初回(PoW 付き)".to_string(),
        }
        .sign(&board_key, 1_700_000_900, 8)
        .unwrap();
        assert!(matches!(
            reg.accept_write(&board_id, &pow_first, 1_700_000_900)
                .unwrap(),
            AcceptOutcome::Numbered { .. }
        ));

        reg.start_next_generation(&board_id, 1_700_001_000, "次スレ")
            .unwrap();

        // 次スレでの書き込みは PoW なしでも採番される(既知板鍵は板スコープで引き継がれる)。
        let no_pow_next = sign_res(
            &board_key,
            &board_id,
            &ch,
            2,
            "次スレへの投稿(PoW なし)",
            1_700_001_010,
        )
        .unwrap();
        assert_eq!(
            reg.accept_write(&board_id, &no_pow_next, 1_700_001_010)
                .unwrap(),
            AcceptOutcome::Numbered { res_no: 1, seq: 1 },
            "既知板鍵は次スレ移行後も PoW を再要求されない(research R6 は板スコープ)"
        );

        // 対照: ホストにとって本当に未見の新規鍵は、次スレでも初回 PoW が必要。
        let unseen_key = Keys::generate();
        let no_pow_unseen = sign_res(
            &unseen_key,
            &board_id,
            &ch,
            2,
            "次スレでの初見鍵(PoW なし)",
            1_700_001_020,
        )
        .unwrap();
        assert_eq!(
            reg.accept_write(&board_id, &no_pow_unseen, 1_700_001_020)
                .unwrap(),
            AcceptOutcome::Rejected,
            "本当に未見の板鍵は次スレでも初回 PoW が必要"
        );
    }

    #[test]
    fn start_next_generation_unknown_board_errors() {
        let reg = LivechatRegistry::new(128);
        assert!(matches!(
            reg.start_next_generation("unknown", 1_700_000_000, "x"),
            Err(RegistryError::UnknownBoard)
        ));
    }

    #[test]
    fn start_next_generation_rejects_when_not_active() {
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);
        reg.close_thread(&board_id, 1_700_000_500).unwrap();
        assert!(matches!(
            reg.start_next_generation(&board_id, 1_700_001_000, "x"),
            Err(RegistryError::Confirm(ThreadError::NotActive))
        ));
    }

    // --- T047: 明示クローズ(FR-014/FR-015)-----------------------------------

    #[test]
    fn close_thread_signs_close_event_and_transitions_to_closed() {
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);

        let event = reg.close_thread(&board_id, 1_700_000_500).unwrap();
        assert_eq!(event.kind.as_u16(), crate::event::livechat::ORDER_KIND);
        assert!(
            crate::event::livechat::is_close_notice(&event),
            "close タグ付きイベントを発行する"
        );
        assert_eq!(event.pubkey, p.public_key(), "署名者はスレ主ペルソナ");
        assert!(event.verify().is_ok());

        assert!(reg.is_closed(&board_id));
    }

    #[test]
    fn close_thread_broadcasts_to_participants() {
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        reg.register_participant(&board_id, "peer-1", tx);

        reg.close_thread(&board_id, 1_700_000_500).unwrap();

        match rx.try_recv() {
            Ok(WireMessage::ThreadClose { event }) => {
                use nostr::JsonUtil;
                let ev = nostr::Event::from_json(event.to_string()).unwrap();
                assert!(crate::event::livechat::is_close_notice(&ev));
            }
            other => panic!("THREAD_CLOSE を期待: {other:?}"),
        }
    }

    #[test]
    fn close_thread_rejects_further_writes() {
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);
        reg.close_thread(&board_id, 1_700_000_500).unwrap();

        let board_key = Keys::generate();
        let res = sign_res(
            &board_key,
            &board_id,
            &channel_of(&board_id),
            1,
            "クローズ後の投稿",
            1_700_000_600,
        )
        .unwrap();
        assert_eq!(
            reg.accept_write(&board_id, &res, 1_700_000_600).unwrap(),
            AcceptOutcome::Rejected,
            "クローズ後は採番されない(T1)"
        );
    }

    #[test]
    fn closed_thread_excluded_from_announce_events() {
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);
        assert_eq!(reg.build_announce_events(1_700_000_000, 0).len(), 1);

        reg.close_thread(&board_id, 1_700_000_500).unwrap();
        assert!(
            reg.build_announce_events(1_700_000_600, 0).is_empty(),
            "クローズ済みスレの announce は発行しない(T047)"
        );
    }

    #[test]
    fn close_thread_unknown_board_errors() {
        let reg = LivechatRegistry::new(128);
        assert!(matches!(
            reg.close_thread("unknown", 1_700_000_000),
            Err(RegistryError::UnknownBoard)
        ));
    }

    #[test]
    fn close_thread_twice_errors_on_second_call() {
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);
        reg.close_thread(&board_id, 1_700_000_500).unwrap();
        assert!(matches!(
            reg.close_thread(&board_id, 1_700_000_600),
            Err(RegistryError::Confirm(ThreadError::NotActive))
        ));
    }

    #[test]
    fn is_closed_true_for_unknown_board() {
        let reg = LivechatRegistry::new(128);
        assert!(
            reg.is_closed("unknown"),
            "未知 board は安全側(true)として扱う"
        );
    }

    // --- T054/T055: 互換 API 向けスナップショット --------------------------

    #[test]
    fn board_snapshot_returns_active_thread_and_settings() {
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);
        let snapshot = reg.board_snapshot(&board_id).unwrap();
        assert_eq!(snapshot.active.generation, 1);
        assert!(
            snapshot.frozen.is_none(),
            "移行前は凍結スナップショットなし"
        );
        assert_eq!(snapshot.settings.title, ""); // registry_with_thread の既定
    }

    #[test]
    fn board_snapshot_unknown_board_is_none() {
        let reg = LivechatRegistry::new(128);
        assert!(reg.board_snapshot("unknown").is_none());
    }

    #[test]
    fn board_snapshot_includes_frozen_after_migration() {
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);
        let board_key = Keys::generate();
        let res = sign_res(
            &board_key,
            &board_id,
            &channel_of(&board_id),
            1,
            "旧スレの投稿",
            1_700_000_010,
        )
        .unwrap();
        reg.accept_write(&board_id, &res, 1_700_000_010).unwrap();

        reg.start_next_generation(&board_id, 1_700_001_000, "次スレ")
            .unwrap();

        let snapshot = reg.board_snapshot(&board_id).unwrap();
        assert_eq!(snapshot.active.generation, 2, "アクティブスレは新世代");
        let frozen = snapshot.frozen.expect("移行後は凍結スナップショットを保持");
        assert_eq!(frozen.generation, 1, "凍結スレは旧世代");
        assert_eq!(frozen.res.len(), 1, "旧スレの確定レスを保持");
        assert_eq!(frozen.state, crate::livechat::thread::ThreadState::Frozen);
    }

    #[test]
    fn thread_by_key_resolves_active_and_frozen_but_not_older() {
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);
        // 世代 1(key=1_700_000_000)→ 世代 2(key=1_700_001_000)→ 世代 3(key=1_700_002_000)。
        reg.start_next_generation(&board_id, 1_700_001_000, "次スレ")
            .unwrap();
        reg.start_next_generation(&board_id, 1_700_002_000, "次々スレ")
            .unwrap();

        // アクティブ(世代 3)は解決できる。
        let active = reg.thread_by_key(&board_id, 1_700_002_000).unwrap();
        assert_eq!(active.generation, 3);
        // 直近 1 世代分の凍結(世代 2)も解決できる。
        let frozen = reg.thread_by_key(&board_id, 1_700_001_000).unwrap();
        assert_eq!(frozen.generation, 2);
        // 2 世代以上前(世代 1)は保持していないため None(呼び出し側は定型 404)。
        assert!(
            reg.thread_by_key(&board_id, 1_700_000_000).is_none(),
            "2 世代以上前の key はベストエフォート保持の対象外(compat-api.md §dat)"
        );
        // 存在しない key。
        assert!(reg.thread_by_key(&board_id, 9_999_999_999).is_none());
    }

    #[test]
    fn close_thread_clears_frozen_snapshot_and_confirmed_res() {
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);
        let board_key = Keys::generate();
        let res = sign_res(
            &board_key,
            &board_id,
            &channel_of(&board_id),
            1,
            "クローズ前の投稿",
            1_700_000_010,
        )
        .unwrap();
        reg.accept_write(&board_id, &res, 1_700_000_010).unwrap();
        reg.start_next_generation(&board_id, 1_700_001_000, "次スレ")
            .unwrap();
        assert!(reg.board_snapshot(&board_id).unwrap().frozen.is_some());

        reg.close_thread(&board_id, 1_700_002_000).unwrap();

        let snapshot = reg.board_snapshot(&board_id).unwrap();
        assert!(
            snapshot.frozen.is_none(),
            "クローズで凍結スナップショットも揮発する(FR-015)"
        );
        assert!(
            snapshot.active.res.is_empty(),
            "クローズ後は確定レスも揮発する"
        );
        assert_eq!(
            snapshot.active.state,
            crate::livechat::thread::ThreadState::Closed
        );
    }

    // --- T055 レビュー対応 1: 名無し名の確定時点固定(dat 追記不変性 MUST)-----------

    #[test]
    fn noname_name_is_baked_in_at_confirm_time_and_unaffected_by_later_settings_change() {
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);
        let board_key = Keys::generate();

        // 1 件目(名前欄なし)を「名無しさん」設定下で確定させる。
        let res1 = sign_res(
            &board_key,
            &board_id,
            &channel_of(&board_id),
            1,
            "1 件目",
            1_700_000_010,
        )
        .unwrap();
        reg.accept_write(&board_id, &res1, 1_700_000_010).unwrap();
        let name_before_change = reg.board_snapshot(&board_id).unwrap().active.res[0]
            .name
            .clone();
        assert_eq!(name_before_change.as_deref(), Some("名無しさん"));

        // 板主が noname_name を変更する。
        reg.update_settings(
            &board_id,
            BoardSettings {
                noname_name: "変更後の名無し".into(),
                ..Default::default()
            },
        )
        .unwrap();

        // 2 件目(名前欄なし)を新設定下で確定させる。
        let res2 = sign_res(
            &board_key,
            &board_id,
            &channel_of(&board_id),
            1,
            "2 件目",
            1_700_000_020,
        )
        .unwrap();
        reg.accept_write(&board_id, &res2, 1_700_000_020).unwrap();

        let snapshot = reg.board_snapshot(&board_id).unwrap();
        assert_eq!(
            snapshot.active.res[0].name.as_deref(),
            Some("名無しさん"),
            "既存レス(1 件目)の表示名は設定変更後も遡及して変わらない(FR-023/FR-024 MUST)"
        );
        assert_eq!(
            snapshot.active.res[1].name.as_deref(),
            Some("変更後の名無し"),
            "新規レス(2 件目)は確定時点の新設定を使う"
        );
    }

    #[test]
    fn seed_confirmed_res_also_bakes_in_noname_name_at_confirm_time() {
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);
        let board_key = Keys::generate();
        let res = sign_res(
            &board_key,
            &board_id,
            &channel_of(&board_id),
            1,
            "seed 経由",
            1_700_000_010,
        )
        .unwrap();
        reg.seed_confirmed_res(&board_id, &res, 1_700_000_010)
            .unwrap();
        let snapshot = reg.board_snapshot(&board_id).unwrap();
        assert_eq!(
            snapshot.active.res[0].name.as_deref(),
            Some("名無しさん"),
            "seed_confirmed_res 経由でも確定時点の noname_name が焼き込まれる"
        );
    }

    // --- T055 レビュー対応 2: Last-Modified の単調性(キャッシュ汚染攻撃の防止)-------

    #[test]
    fn last_confirmed_at_is_monotonic_even_with_backdated_created_at() {
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);
        let board_key = Keys::generate();

        // 1 件目をホスト受信時刻 1_700_001_000 で確定させる。
        let res1 = sign_res(
            &board_key,
            &board_id,
            &channel_of(&board_id),
            1,
            "1 件目",
            1_700_000_500, // イベント内 created_at(投稿者申告・未検証)
        )
        .unwrap();
        reg.accept_write(&board_id, &res1, 1_700_001_000).unwrap();
        let after_first = reg
            .board_snapshot(&board_id)
            .unwrap()
            .active
            .last_confirmed_at;
        assert_eq!(
            after_first, 1_700_001_000,
            "Last-Modified はホスト受信時刻を基準にする(投稿者申告値ではない)"
        );

        // 2 件目は「過去の」created_at を申告する攻撃を模す。ホスト受信時刻自体も
        // 意図的に 1 件目より小さい値(通常はあり得ないが単調性を厳密に確認するため)。
        let res2 = sign_res(
            &board_key,
            &board_id,
            &channel_of(&board_id),
            1,
            "2 件目(過去日時を申告)",
            1_000_000_000, // 大幅に過去の created_at
        )
        .unwrap();
        reg.accept_write(&board_id, &res2, 1_700_000_999).unwrap(); // 1 件目より僅かに前

        let after_second = reg
            .board_snapshot(&board_id)
            .unwrap()
            .active
            .last_confirmed_at;
        assert_eq!(
            after_second, 1_700_001_000,
            "Last-Modified は後退しない(単調性 — キャッシュ汚染攻撃の防止)"
        );

        // 3 件目は正常に進んだホスト受信時刻で確定させると Last-Modified も前進する。
        let res3 = sign_res(
            &board_key,
            &board_id,
            &channel_of(&board_id),
            1,
            "3 件目",
            1_700_000_010,
        )
        .unwrap();
        reg.accept_write(&board_id, &res3, 1_700_002_000).unwrap();
        let after_third = reg
            .board_snapshot(&board_id)
            .unwrap()
            .active
            .last_confirmed_at;
        assert_eq!(
            after_third, 1_700_002_000,
            "正常なホスト時刻進行では前進する"
        );
    }

    #[test]
    fn last_confirmed_at_survives_thread_migration_snapshot() {
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);
        let board_key = Keys::generate();
        let res = sign_res(
            &board_key,
            &board_id,
            &channel_of(&board_id),
            1,
            "移行前",
            1_700_000_010,
        )
        .unwrap();
        reg.accept_write(&board_id, &res, 1_700_000_500).unwrap();

        reg.start_next_generation(&board_id, 1_700_001_000, "次スレ")
            .unwrap();

        let snapshot = reg.board_snapshot(&board_id).unwrap();
        let frozen = snapshot.frozen.expect("凍結スナップショット");
        assert_eq!(
            frozen.last_confirmed_at, 1_700_000_500,
            "凍結スナップショットの Last-Modified 基準も維持される"
        );
    }

    // --- T056 レビュー対応: is_known_board_key の板鍵ローテーション対応 ------------

    #[test]
    fn is_known_board_key_reflects_actual_acceptance_history() {
        let p = persona();
        let board_id = p.public_key().to_hex();
        let reg = registry_with_thread(&p, 128);
        let board_key = Keys::generate();
        let key_hex = board_key.public_key().to_hex();

        assert!(
            !reg.is_known_board_key(&board_id, &key_hex),
            "書き込み前は未知"
        );

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

        assert!(
            reg.is_known_board_key(&board_id, &key_hex),
            "採番実績があれば既知"
        );

        // ローテーション相当の新鍵は未知のまま。
        let rotated_key = Keys::generate();
        assert!(!reg.is_known_board_key(&board_id, &rotated_key.public_key().to_hex()));
    }

    #[test]
    fn is_known_board_key_unknown_board_is_false() {
        let reg = LivechatRegistry::new(128);
        assert!(!reg.is_known_board_key("unknown", "anykey"));
    }
}

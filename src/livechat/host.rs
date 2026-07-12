//! ホスト側シーケンサ(T019/T021/T023 — contracts/thread-delivery.md)
//!
//! ホストが 1 本のスレを保持し、参加者接続の受理判定(T021)・接続時同期メッセージ列の
//! 生成(T023)・スレ開設に伴う announce イベントの組み立て(T019)を担う純粋寄りの
//! ドメイン層。TLC 検査済み PlusCal モデル(docs/formal/livechat_sequencer.tla)に対応する
//! 採番・ORDER 発行・次スレ移行・BAN 強制・PoW/レート判定は後続タスク
//! (T030/T032/T036/T037/T042/T044/T046/T047)で本層の上に構築する。
//!
//! **runtime/p2p session への非同期配線は本モジュールの責務外**。判定結果は [`JoinDecision`]
//! 等の enum で返し、ワイヤ送信([`crate::p2p::frame::Message`] の生成)は薄いアダプタ
//! ([`join_decision_to_message`])へ分離する(テスト容易性 — thread-delivery.md §検証方法)。
//!
//! ## チャレンジ署名(FR-005 — アドレス真正性の証明)
//!
//! WELCOME の `sig` は**スレ主ペルソナ鍵**による Schnorr 署名で、参加者が「接続先が
//! announce に記載された正当なスレ主である」ことを検証できるようにする(反射・偽スレ対策)。
//! 署名対象は [`challenge_message`] が定める `challenge(32) || board_id(32) || gen(BE4)` の
//! SHA-256 ダイジェスト。参加者側の検証は [`crate::livechat::session`] が対で実装する。

use nostr::hashes::{Hash, sha256};
use nostr::secp256k1::Message;
use nostr::{Event, Keys};
use serde_json::Value;

use crate::event::livechat::{LivechatBuildError, ThreadAnnounce};
use crate::p2p::frame::{Message as WireMessage, thread_reject_reason};
use crate::security::is_lower_hex;

use super::thread::{BoardSettings, Res, Thread, ThreadState};

// ---------------------------------------------------------------------------
// チャレンジ署名(FR-005)
// ---------------------------------------------------------------------------

/// WELCOME 署名対象のダイジェストを構成する(FR-005 — アドレス真正性)。
///
/// 署名対象バイト列は `challenge(32 バイト生値) || board_id(32 バイト生値) || gen(BE u32)` を
/// 連結し、その **SHA-256** を取る。Schnorr 署名は 32 バイトのメッセージ(ダイジェスト)を
/// 要求するため、可変長の連結値を直接署名せずハッシュを噛ませる。
///
/// - `challenge_hex` はワイヤ上 hex 文字列で来る 32 バイト乱数。生バイトに戻して連結する
///   (hex 表現のまま連結すると長さ・文字集合が攻撃者可変になるため、生バイトに正規化する)。
/// - `board_id_hex` はスレ主ペルソナ pubkey(hex 64)。生バイト 32 に戻して連結する。
/// - `gen` は世代を **ビッグエンディアン 4 バイト**で連結する(文字列表現の桁揺れを避け、
///   ホスト・参加者で同一バイト列になることを保証する)。
///
/// 形式不正(challenge/board_id が hex でない・長さ不一致)は `None`。呼び出し側は
/// これを検証失敗として扱う。
fn challenge_message(challenge_hex: &str, board_id_hex: &str, generation: u32) -> Option<Message> {
    // challenge は 32 バイト = hex 64 桁。board_id は pubkey で hex 64 桁。
    if !is_lower_hex(challenge_hex, 64) || !is_lower_hex(board_id_hex, 64) {
        return None;
    }
    let challenge = decode_hex32(challenge_hex)?;
    let board_id = decode_hex32(board_id_hex)?;
    let mut buf = Vec::with_capacity(32 + 32 + 4);
    buf.extend_from_slice(&challenge);
    buf.extend_from_slice(&board_id);
    buf.extend_from_slice(&generation.to_be_bytes());
    let digest = sha256::Hash::hash(&buf).to_byte_array();
    Some(Message::from_digest(digest))
}

/// 小文字 hex 64 桁を 32 バイトへデコードする(検証済み前提だが長さは再確認する)。
fn decode_hex32(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        let hi = (hex.as_bytes()[i * 2] as char).to_digit(16)?;
        let lo = (hex.as_bytes()[i * 2 + 1] as char).to_digit(16)?;
        *byte = (hi * 16 + lo) as u8;
    }
    Some(out)
}

/// スレ主ペルソナ鍵で WELCOME 署名を生成し hex 文字列で返す(ホスト側 — FR-005)。
///
/// 署名対象は [`challenge_message`] のダイジェスト。参加者は同じ構成で復元した
/// ダイジェストに対しスレ主公開鍵で検証する。形式不正な challenge/board_id は `None`。
pub fn sign_welcome(
    persona_keys: &Keys,
    challenge_hex: &str,
    board_id_hex: &str,
    generation: u32,
) -> Option<String> {
    let message = challenge_message(challenge_hex, board_id_hex, generation)?;
    let sig = persona_keys.sign_schnorr(&message);
    Some(sig.to_string())
}

// ---------------------------------------------------------------------------
// ホストが保持する 1 スレの状態
// ---------------------------------------------------------------------------

/// 接続中の参加者 1 名分の登録情報。
///
/// 接続の同一性(トランスポート層のハンドル)は配線側が握るため、本層は識別子
/// (`peer_id` — 正規アドレス等)のみを保持する。ブロードキャストの宛先列挙は配線側が
/// [`HostThread::participants`] を参照して行う。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Participant {
    /// 参加者接続の識別子(正規アドレス等 — 配線側が採番)。
    pub peer_id: String,
}

/// ホストが保持する 1 本のスレの状態(Thread + 参加者レジストリ + seq/ORDER 履歴)。
///
/// 採番済みレス(`thread.res`)と ORDER 履歴を保持し、接続時同期(T023)で `since_seq`
/// 以降の差分を再生する。次 seq カウンタは発行済み ORDER の連番(不変条件 O2)。
pub struct HostThread {
    /// スレ本体(状態遷移・採番の強制は [`Thread`] が担う)。
    pub thread: Thread,
    /// 板設定(WELCOME で配布する。JSON 化は [`board_settings_json`])。
    pub settings: BoardSettings,
    /// 接続中の参加者(参加上限判定・ブロードキャスト宛先)。
    participants: Vec<Participant>,
    /// 発行済み ORDER 履歴(seq 昇順)。接続時同期・再送(RESEND_REQ)で再生する。
    orders: Vec<HostOrder>,
    /// 次に発行する ORDER の seq(1 始まり。O2 の連番)。
    next_seq: u32,
}

/// ホストが発行した ORDER 1 件(seq と採番エントリ)。配布時に kind 21311 へ写す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostOrder {
    /// 確定情報の連番(不変条件 O2)。
    pub seq: u32,
    /// 今回確定した採番 `(res_no, event_id)`(欠番なく連続 — 不変条件 T3)。
    pub entries: Vec<(u16, String)>,
}

/// 参加受理判定の結果(ワイヤ送信は [`join_decision_to_message`] が担う)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinDecision {
    /// 受理。`sig` はスレ主鍵による WELCOME 署名(hex)。`res_count` は確定レス数。
    Welcome {
        thread: String,
        sig: String,
        res_count: u32,
    },
    /// 定型拒否(理由は [`thread_reject_reason`] のコード)。内部情報は含めない(FR-006)。
    Reject { reason: &'static str },
}

impl HostThread {
    /// スレを開設する(`Active` から開始 — data-model §状態遷移)。
    pub fn new(thread: Thread, settings: BoardSettings) -> Self {
        Self {
            thread,
            settings,
            participants: Vec::new(),
            orders: Vec::new(),
            next_seq: 1,
        }
    }

    /// `<board_id>:<gen>` 形式のスレ識別子(ワイヤ上の `thread` フィールド)。
    pub fn thread_ref(&self) -> String {
        format!("{}:{}", self.thread.board_id, self.thread.generation)
    }

    /// 現在の参加者一覧(ブロードキャスト宛先列挙・上限監視用)。
    pub fn participants(&self) -> &[Participant] {
        &self.participants
    }

    /// 確定レス数(WELCOME の `res_count`)。
    pub fn res_count(&self) -> u32 {
        self.thread.res.len() as u32
    }

    /// 板設定を WELCOME/SETTINGS 配布用の JSON へ写す。
    pub fn board_settings_json(&self) -> Value {
        board_settings_json(&self.settings)
    }

    // --- T021: 接続受理判定 -------------------------------------------------

    /// `THREAD_JOIN` の受理可否を判定する(T021 — thread-delivery.md §ホスト)。
    ///
    /// 判定順(いずれも定型拒否で内部情報を開示しない — FR-006):
    ///
    /// 1. **スレ一致**: `thread`(`<board_id>:<gen>`)が本スレと一致しなければ `unknown_thread`。
    /// 2. **状態**: `Frozen` は `frozen`、`Closed` は `closed`(不変条件 T1 と整合)。
    /// 3. **参加上限**: 受理すると `max_participants` を超えるなら `full`(FR-021)。
    /// 4. **受理**: スレ主鍵で WELCOME 署名を生成して `Welcome` を返す。challenge/board_id が
    ///    形式不正(hex でない等)なら真正性を証明できないため `unknown_thread` として扱う
    ///    (内部の署名失敗理由は開示しない)。
    ///
    /// **受理しても本メソッドは参加者を登録しない**(登録は WELCOME 送信成功後に配線側が
    /// [`register_participant`] を呼ぶ。送信前に枠を消費して枠内 DoS を許さないため)。
    pub fn decide_join(
        &self,
        thread_ref: &str,
        challenge_hex: &str,
        persona_keys: &Keys,
        max_participants: usize,
    ) -> JoinDecision {
        if thread_ref != self.thread_ref() {
            return JoinDecision::Reject {
                reason: thread_reject_reason::UNKNOWN_THREAD,
            };
        }
        match self.thread.state {
            ThreadState::Frozen => {
                return JoinDecision::Reject {
                    reason: thread_reject_reason::FROZEN,
                };
            }
            ThreadState::Closed => {
                return JoinDecision::Reject {
                    reason: thread_reject_reason::CLOSED,
                };
            }
            ThreadState::Active => {}
        }
        if self.participants.len() >= max_participants {
            return JoinDecision::Reject {
                reason: thread_reject_reason::FULL,
            };
        }
        match sign_welcome(
            persona_keys,
            challenge_hex,
            &self.thread.board_id,
            self.thread.generation,
        ) {
            Some(sig) => JoinDecision::Welcome {
                thread: self.thread_ref(),
                sig,
                res_count: self.res_count(),
            },
            // challenge/board_id の形式不正で真正性を証明できない。理由は開示しない。
            None => JoinDecision::Reject {
                reason: thread_reject_reason::UNKNOWN_THREAD,
            },
        }
    }

    /// 参加者を登録する(WELCOME 送信成功後に配線側が呼ぶ)。既登録の `peer_id` は無視する。
    pub fn register_participant(&mut self, peer_id: impl Into<String>) {
        let peer_id = peer_id.into();
        if self.participants.iter().any(|p| p.peer_id == peer_id) {
            return;
        }
        self.participants.push(Participant { peer_id });
    }

    /// 参加者の登録を解除する(切断時に配線側が呼ぶ)。解除できれば `true`。
    pub fn unregister_participant(&mut self, peer_id: &str) -> bool {
        let before = self.participants.len();
        self.participants.retain(|p| p.peer_id != peer_id);
        self.participants.len() != before
    }

    // --- ORDER 履歴(採番の記録)--------------------------------------------

    /// 採番済み ORDER を履歴へ記録する(発行側 — 後続 T036 の採番が呼ぶ)。
    ///
    /// `entries` は本スレの確定列と整合している前提(採番は [`Thread::confirm`] が強制済み)。
    /// seq は本メソッドが自動採番し(不変条件 O2)、記録した [`HostOrder`] を返す。
    pub fn record_order(&mut self, entries: Vec<(u16, String)>) -> HostOrder {
        let order = HostOrder {
            seq: self.next_seq,
            entries,
        };
        self.next_seq += 1;
        self.orders.push(order.clone());
        order
    }

    /// 発行済み ORDER 履歴(seq 昇順)。
    pub fn orders(&self) -> &[HostOrder] {
        &self.orders
    }

    // --- T023: 接続時同期 ---------------------------------------------------

    /// `since_seq` 以降の同期メッセージ列を生成する(T023 — thread-delivery.md §接続時同期)。
    ///
    /// joined 直後、ホストは確定レス(`RES`)と順序確定情報(`ORDER`)を **seq 順に**送る。
    /// 本メソッドは配線側が送出すべき [`SyncItem`] 列を返す(実際の署名済みイベント生成・
    /// フレーム化は配線側の責務 — 本層はドメインの再生順序のみを定める)。
    ///
    /// 生成順序: まず未送分の確定レスを res_no 昇順、続いて `since_seq` を**超える** seq の
    /// ORDER を seq 昇順(`since_seq` は「受信済みの最後の seq」であり、それ以下は再送しない)。
    /// 参加者は seq 連続性が保たれる限り受信順に表示してよい。
    pub fn sync_since(&self, since_seq: u32) -> Vec<SyncItem> {
        let mut items = Vec::new();
        // 確定レスは res_no 順(Thread::res は確定順で保持されている)。
        for res in &self.thread.res {
            items.push(SyncItem::Res(res.clone()));
        }
        // ORDER は since_seq を超える連番のみ(欠落なく昇順で再生 — 不変条件 O2)。
        for order in &self.orders {
            if order.seq > since_seq {
                items.push(SyncItem::Order(order.clone()));
            }
        }
        items
    }
}

/// 接続時同期で配線側が送出すべき 1 項目(署名済みイベント生成は配線側が担う)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncItem {
    /// 確定レス 1 件(`RES` メッセージへ写す)。
    Res(Res),
    /// 順序確定情報 1 件(`ORDER` メッセージへ写す)。
    Order(HostOrder),
}

// ---------------------------------------------------------------------------
// T019: announce 発行
// ---------------------------------------------------------------------------

/// スレ開設に伴う announce(kind 31311)イベントを組み立てて署名する(T019 — FR-002)。
///
/// `board_id`(掲載ペルソナ pubkey)への限定は署名鍵と `a` タグの一致で担保される
/// (`persona_keys` の公開鍵 = スレ主ペルソナ = `a` タグの `<pubkey>`。ペルソナ一致検査は
/// gossip 受信側 [`crate::event::schema`] が行う — FR-003)。`expiration = created_at + 600`
/// の付与は [`ThreadAnnounce::sign`] が担うため、本関数は `created_at` を渡すのみ。
///
/// 60 秒間隔の再発行はタイミング driver を注入可能にするため `created_at` を引数に取り、
/// 実際の gossip publish([`crate::p2p::ingest::IngestState::publish_local`])は配線側の
/// 責務とする(本関数は「発行すべき Event を作る」ところまで)。
#[allow(clippy::too_many_arguments)]
pub fn build_announce(
    persona_keys: &Keys,
    channel: impl Into<String>,
    title: impl Into<String>,
    generation: u32,
    key: u64,
    res_count: Option<u64>,
    tip: impl Into<String>,
    created_at: u64,
    pow_bits: u8,
) -> Result<Event, LivechatBuildError> {
    let announce = ThreadAnnounce {
        channel: channel.into(),
        title: title.into(),
        generation,
        key,
        res_count,
        tip: tip.into(),
    };
    announce.sign(persona_keys, created_at, pow_bits)
}

/// [`HostThread`] の現況から announce を組み立てて署名する(定期再発行の薄いラッパ)。
///
/// タイトル・世代・key・res_count・tip をスレ状態から埋める。`tip`(ホスト接続先)は
/// 配線側しか知らないため引数で受け取る。
pub fn build_announce_for(
    host: &HostThread,
    persona_keys: &Keys,
    tip: impl Into<String>,
    created_at: u64,
    pow_bits: u8,
) -> Result<Event, LivechatBuildError> {
    build_announce(
        persona_keys,
        host.thread.channel.clone(),
        host.thread.title.clone(),
        host.thread.generation,
        host.thread.key,
        Some(host.res_count() as u64),
        tip,
        created_at,
        pow_bits,
    )
}

// ---------------------------------------------------------------------------
// ワイヤアダプタ(判定結果 → Message)
// ---------------------------------------------------------------------------

/// 板設定を WELCOME/SETTINGS 配布用の JSON オブジェクトへ写す(FR-023)。
///
/// 受信側([`crate::livechat::session`])は本形式を [`BoardSettings`] へ復元して検証する。
pub fn board_settings_json(settings: &BoardSettings) -> Value {
    serde_json::json!({
        "title": settings.title,
        "res_limit": settings.res_limit,
        "noname_name": settings.noname_name,
        "local_rules": settings.local_rules,
        "first_post_pow_bits": settings.first_post_pow_bits,
    })
}

/// 参加受理判定をワイヤメッセージへ写す(送信は配線側)。
///
/// `Welcome` は `board_settings` に本ホストの板設定 JSON を載せる。`Reject` の `reason` は
/// 定型コードのみ(内部情報を含めない — FR-006)。
pub fn join_decision_to_message(decision: JoinDecision, board_settings: Value) -> WireMessage {
    match decision {
        JoinDecision::Welcome {
            thread,
            sig,
            res_count,
        } => WireMessage::ThreadWelcome {
            thread,
            sig,
            board_settings,
            res_count,
        },
        JoinDecision::Reject { reason } => WireMessage::ThreadReject {
            reason: reason.to_string(),
        },
    }
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::livechat::session;

    const GUID: &str = "0123456789abcdef0123456789abcdef";

    fn persona() -> Keys {
        Keys::generate()
    }

    /// 32 バイト challenge の hex(テスト固定値)。
    fn challenge() -> String {
        "ab".repeat(32)
    }

    fn host_thread(persona_keys: &Keys) -> HostThread {
        let board_id = persona_keys.public_key().to_hex();
        let channel = format!("30311:{board_id}:{GUID}");
        let thread = Thread::new(&board_id, channel, 1, 1_700_000_000, "実況スレ", 1000);
        HostThread::new(thread, BoardSettings::default())
    }

    fn sample_res(event_id: &str, board_key: &str) -> Res {
        Res {
            event_id: event_id.to_string(),
            board_key: board_key.to_string(),
            name: None,
            mail: None,
            body: "本文".to_string(),
            created_at: 1_700_000_000,
            res_no: None,
            pending: true,
        }
    }

    // --- T021: 受理/各 REJECT 分岐 ------------------------------------------

    #[test]
    fn decide_join_welcomes_active_within_limit() {
        let keys = persona();
        let host = host_thread(&keys);
        let decision = host.decide_join(&host.thread_ref(), &challenge(), &keys, 128);
        match decision {
            JoinDecision::Welcome {
                thread, res_count, ..
            } => {
                assert_eq!(thread, host.thread_ref());
                assert_eq!(res_count, 0);
            }
            other => panic!("受理されるべき: {other:?}"),
        }
    }

    #[test]
    fn decide_join_rejects_unknown_thread() {
        let keys = persona();
        let host = host_thread(&keys);
        let decision = host.decide_join("deadbeef:9", &challenge(), &keys, 128);
        assert_eq!(
            decision,
            JoinDecision::Reject {
                reason: thread_reject_reason::UNKNOWN_THREAD
            }
        );
    }

    #[test]
    fn decide_join_rejects_frozen_and_closed() {
        let keys = persona();
        let mut host = host_thread(&keys);
        host.thread.freeze().unwrap();
        assert_eq!(
            host.decide_join(&host.thread_ref(), &challenge(), &keys, 128),
            JoinDecision::Reject {
                reason: thread_reject_reason::FROZEN
            }
        );
        // Frozen -> Closed へ遷移させて closed 分岐も確認。
        host.thread.close().unwrap();
        assert_eq!(
            host.decide_join(&host.thread_ref(), &challenge(), &keys, 128),
            JoinDecision::Reject {
                reason: thread_reject_reason::CLOSED
            }
        );
    }

    #[test]
    fn decide_join_rejects_when_full() {
        let keys = persona();
        let mut host = host_thread(&keys);
        host.register_participant("peer-a");
        host.register_participant("peer-b");
        // 上限 2、既に 2 名 → full。
        assert_eq!(
            host.decide_join(&host.thread_ref(), &challenge(), &keys, 2),
            JoinDecision::Reject {
                reason: thread_reject_reason::FULL
            }
        );
        // 上限 3 なら受理。
        assert!(matches!(
            host.decide_join(&host.thread_ref(), &challenge(), &keys, 3),
            JoinDecision::Welcome { .. }
        ));
    }

    #[test]
    fn decide_join_rejects_malformed_challenge() {
        let keys = persona();
        let host = host_thread(&keys);
        // hex64 でない challenge は真正性を証明できず unknown_thread として拒否。
        let decision = host.decide_join(&host.thread_ref(), "not-hex", &keys, 128);
        assert_eq!(
            decision,
            JoinDecision::Reject {
                reason: thread_reject_reason::UNKNOWN_THREAD
            }
        );
    }

    // --- WELCOME sig が participant 側検証で通ること -------------------------

    #[test]
    fn welcome_sig_verifies_on_participant_side() {
        let keys = persona();
        let host = host_thread(&keys);
        let board_id = keys.public_key().to_hex();
        let sig = sign_welcome(&keys, &challenge(), &board_id, host.thread.generation).unwrap();
        // 参加者側(session)は announce 記載のスレ主公開鍵で検証する。
        assert!(session::verify_welcome_sig(
            &sig,
            &challenge(),
            &board_id,
            host.thread.generation
        ));
    }

    #[test]
    fn welcome_sig_fails_for_wrong_generation() {
        let keys = persona();
        let board_id = keys.public_key().to_hex();
        let sig = sign_welcome(&keys, &challenge(), &board_id, 1).unwrap();
        // 世代が違えばダイジェストが変わり検証は失敗する。
        assert!(!session::verify_welcome_sig(
            &sig,
            &challenge(),
            &board_id,
            2
        ));
    }

    // --- 参加者登録 ----------------------------------------------------------

    #[test]
    fn register_is_idempotent_and_unregister_works() {
        let keys = persona();
        let mut host = host_thread(&keys);
        host.register_participant("peer-a");
        host.register_participant("peer-a"); // 重複は無視
        assert_eq!(host.participants().len(), 1);
        assert!(host.unregister_participant("peer-a"));
        assert!(!host.unregister_participant("peer-a"));
        assert!(host.participants().is_empty());
    }

    // --- T023: 同期メッセージ列が seq 順・全件 ------------------------------

    #[test]
    fn sync_since_returns_all_res_and_orders_in_order() {
        let keys = persona();
        let mut host = host_thread(&keys);
        let board_key = "cd".repeat(32);
        // 2 レス確定 + 2 ORDER 記録。
        host.thread
            .confirm(sample_res(&"11".repeat(32), &board_key), 1)
            .unwrap();
        host.thread
            .confirm(sample_res(&"22".repeat(32), &board_key), 2)
            .unwrap();
        host.record_order(vec![(1, "11".repeat(32))]);
        host.record_order(vec![(2, "22".repeat(32))]);

        // since_seq=0 → 全レス + 全 ORDER。
        let items = host.sync_since(0);
        let res: Vec<_> = items
            .iter()
            .filter_map(|i| match i {
                SyncItem::Res(r) => r.res_no,
                _ => None,
            })
            .collect();
        assert_eq!(res, vec![1, 2], "確定レスは res_no 昇順で全件");
        let seqs: Vec<_> = items
            .iter()
            .filter_map(|i| match i {
                SyncItem::Order(o) => Some(o.seq),
                _ => None,
            })
            .collect();
        assert_eq!(seqs, vec![1, 2], "ORDER は seq 昇順で全件");
    }

    #[test]
    fn sync_since_skips_orders_at_or_below_since_seq() {
        let keys = persona();
        let mut host = host_thread(&keys);
        host.record_order(vec![(1, "11".repeat(32))]); // seq 1
        host.record_order(vec![(2, "22".repeat(32))]); // seq 2
        host.record_order(vec![(3, "33".repeat(32))]); // seq 3

        // since_seq=2 → seq 3 のみ(1・2 は受信済み)。
        let items = host.sync_since(2);
        let seqs: Vec<_> = items
            .iter()
            .filter_map(|i| match i {
                SyncItem::Order(o) => Some(o.seq),
                _ => None,
            })
            .collect();
        assert_eq!(seqs, vec![3]);
    }

    #[test]
    fn record_order_assigns_consecutive_seq() {
        let keys = persona();
        let mut host = host_thread(&keys);
        let o1 = host.record_order(vec![(1, "11".repeat(32))]);
        let o2 = host.record_order(vec![(2, "22".repeat(32))]);
        assert_eq!(o1.seq, 1);
        assert_eq!(o2.seq, 2);
        assert_eq!(host.orders().len(), 2);
    }

    // --- T019: announce 発行 ------------------------------------------------

    #[test]
    fn build_announce_signs_kind_31311_with_persona_key() {
        let keys = persona();
        let host = host_thread(&keys);
        let event =
            build_announce_for(&host, &keys, "198.51.100.1:7147", 1_700_000_000, 0).unwrap();
        assert_eq!(event.kind.as_u16(), crate::event::livechat::ANNOUNCE_KIND);
        // 署名者 = スレ主ペルソナ(a タグの pubkey と一致する — FR-003)。
        assert_eq!(event.pubkey, keys.public_key());
        assert!(event.verify().is_ok());
        // expiration = created_at + 600 は封筒側が付与する。
        let restored = ThreadAnnounce::from_event(&event).unwrap();
        assert_eq!(restored.generation, 1);
        assert_eq!(restored.res_count, Some(0));
    }

    // --- ワイヤアダプタ ------------------------------------------------------

    #[test]
    fn join_decision_to_message_maps_welcome_and_reject() {
        let welcome = JoinDecision::Welcome {
            thread: "abc:1".into(),
            sig: "sig".into(),
            res_count: 3,
        };
        let settings = board_settings_json(&BoardSettings::default());
        match join_decision_to_message(welcome, settings.clone()) {
            WireMessage::ThreadWelcome { res_count, .. } => assert_eq!(res_count, 3),
            other => panic!("WELCOME であるべき: {other:?}"),
        }
        let reject = JoinDecision::Reject {
            reason: thread_reject_reason::FULL,
        };
        match join_decision_to_message(reject, settings) {
            WireMessage::ThreadReject { reason } => assert_eq!(reason, "full"),
            other => panic!("REJECT であるべき: {other:?}"),
        }
    }
}

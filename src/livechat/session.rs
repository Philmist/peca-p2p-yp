//! 参加者セッション(T022/T023 — contracts/thread-delivery.md §参加者)
//!
//! 参加者側の状態機械: `THREAD_JOIN` 生成 → `THREAD_WELCOME` 受信 + チャレンジ検証
//! (T022)→ joined → 接続時同期の受信・確定列の再構成(T023)。sig 検証失敗・凍結時の
//! 指数バックオフ再接続も本層が定める。
//!
//! runtime/p2p session への非同期配線は本モジュールの責務外(判定は enum・純粋関数で返す)。
//!
//! ## チャレンジ検証(FR-005)
//!
//! `THREAD_WELCOME.sig` を **announce に記載されたスレ主ペルソナ公開鍵**で検証する。ホスト側
//! [`crate::livechat::host::sign_welcome`] と同一のダイジェスト構成
//! (`challenge(32) || board_id(32) || gen(BE4)` の SHA-256)を復元して照合する。検証失敗は
//! 「切断 + `livechat_challenge_failed` 記録 + バックオフ」を表す [`WelcomeOutcome`] を返す。

use nostr::hashes::{Hash, sha256};
use nostr::secp256k1::{Message, Secp256k1, schnorr};
use nostr::{Event, PublicKey};

use crate::event::livechat::{OrderInfo as OrderEnvelope, Res as ResEnvelope};
use crate::p2p::frame::Message as WireMessage;
use crate::security::{SecurityCategory, is_lower_hex};

use super::thread::{Res, Thread, ThreadError};

// ---------------------------------------------------------------------------
// 指数バックオフ(FR-005 — gossip 再接続と同一パラメータ)
// ---------------------------------------------------------------------------

/// 再接続バックオフの初期遅延(秒)。
pub const BACKOFF_INITIAL_SECS: u64 = 5;
/// バックオフの係数(2 倍)。
pub const BACKOFF_FACTOR: u64 = 2;
/// バックオフの上限(秒)。
pub const BACKOFF_MAX_SECS: u64 = 300;

/// 試行回数(0 始まり)から次の再接続遅延(秒)を返す純粋関数。
///
/// `5 * 2^attempt` を [`BACKOFF_MAX_SECS`] で頭打ちにする(5, 10, 20, 40, ..., 300 で飽和)。
/// `attempt = 0` は初回失敗直後の遅延 = 初期値。オーバーフローは上限へ丸める。
pub fn backoff_delay_secs(attempt: u32) -> u64 {
    // 2^attempt を checked で計算し、桁溢れは上限へ。
    let factor = BACKOFF_FACTOR.checked_pow(attempt).unwrap_or(u64::MAX);
    BACKOFF_INITIAL_SECS
        .checked_mul(factor)
        .unwrap_or(BACKOFF_MAX_SECS)
        .min(BACKOFF_MAX_SECS)
}

// ---------------------------------------------------------------------------
// チャレンジ生成・検証(FR-005)
// ---------------------------------------------------------------------------

/// 32 バイト乱数チャレンジを生成し hex 文字列で返す(`THREAD_JOIN` 用)。
///
/// nostr が再エクスポートする secp256k1 の乱数源([`nostr::secp256k1::rand`])を用い、
/// 追加の乱数クレートを増やさない。
pub fn generate_challenge() -> String {
    use nostr::secp256k1::rand::RngCore;
    let mut bytes = [0u8; 32];
    nostr::secp256k1::rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex_encode(&bytes)
}

/// バイト列を小文字 hex へ符号化する。
fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        out.push(char::from_digit((b & 0x0f) as u32, 16).unwrap());
    }
    out
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

/// WELCOME 署名を検証する(FR-005 — アドレス真正性)。
///
/// ホスト側 [`crate::livechat::host::sign_welcome`] と同一の署名対象ダイジェスト
/// (`challenge(32) || board_id(32) || gen(BE4)` の SHA-256)を復元し、`board_id` を
/// スレ主ペルソナ公開鍵として Schnorr 署名を検証する。以下はすべて検証失敗(`false`):
///
/// - `sig_hex` が Schnorr 署名として解釈できない
/// - `challenge_hex` / `board_id_hex` が hex 64 桁でない(ダイジェストを組めない)
/// - `board_id_hex` が有効な x-only 公開鍵でない
/// - 署名が公開鍵・ダイジェストに対して不正
pub fn verify_welcome_sig(
    sig_hex: &str,
    challenge_hex: &str,
    board_id_hex: &str,
    generation: u32,
) -> bool {
    if !is_lower_hex(challenge_hex, 64) || !is_lower_hex(board_id_hex, 64) {
        return false;
    }
    let Ok(sig) = sig_hex.parse::<schnorr::Signature>() else {
        return false;
    };
    // board_id(スレ主ペルソナ pubkey)を x-only 公開鍵へ。
    let Ok(pubkey) = PublicKey::from_hex(board_id_hex) else {
        return false;
    };
    let Ok(xonly) = pubkey.xonly() else {
        return false;
    };
    let (Some(challenge), Some(board_id)) =
        (decode_hex32(challenge_hex), decode_hex32(board_id_hex))
    else {
        return false;
    };
    let mut buf = Vec::with_capacity(32 + 32 + 4);
    buf.extend_from_slice(&challenge);
    buf.extend_from_slice(&board_id);
    buf.extend_from_slice(&generation.to_be_bytes());
    let digest = sha256::Hash::hash(&buf).to_byte_array();
    let message = Message::from_digest(digest);
    Secp256k1::verification_only()
        .verify_schnorr(&sig, &message, &xonly)
        .is_ok()
}

// ---------------------------------------------------------------------------
// T022: 参加者セッション状態機械
// ---------------------------------------------------------------------------

/// 参加者セッションの状態(接続要求 → 受理 → 同期 → 凍結)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// `THREAD_JOIN` 送信済み、WELCOME 待ち。
    Joining,
    /// WELCOME を検証して受理された(同期・受信フェーズ)。
    Joined,
    /// ホスト切断・検証失敗などで切断された(再接続はバックオフ付き)。
    Disconnected,
}

/// WELCOME 受信時の判定結果(配線側はこれに従い状態遷移・記録・バックオフする)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WelcomeOutcome {
    /// 受理。joined へ遷移し `since_seq` から同期を開始してよい。
    Accepted,
    /// sig 検証失敗。**切断すべき** + `livechat_challenge_failed` を記録 + バックオフ。
    ChallengeFailed {
        /// 記録すべきセキュリティカテゴリ(呼び出し側がログへ書く)。
        category: SecurityCategory,
    },
}

/// `THREAD_REJECT` 受信時の扱い(reason 別)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectHandling {
    /// バックオフして再試行する(`full` / `rate` — 一時的な混雑)。
    Backoff,
    /// 当該スレは接続対象外。再試行を諦める(`closed` / `unknown_thread`)。
    GiveUp,
    /// 凍結として扱い、announce 更新を待って再試行する(`frozen`)。
    WaitFrozen,
}

/// 参加者セッション(T022)。チャレンジ・スレ識別子・受信済み確定列を保持する。
///
/// トランスポート・タイマは配線側が握る。本層は「WELCOME をどう検証するか」「REJECT を
/// どう扱うか」「同期受信をどう確定列へ反映するか」の純粋ロジックを提供する。
pub struct ParticipantSession {
    /// 対象スレの板 id(スレ主ペルソナ pubkey)。WELCOME 検証・同期反映に使う。
    board_id: String,
    /// 対象スレの世代。
    generation: u32,
    /// 今回の接続で提示したチャレンジ(hex 32 バイト)。
    challenge: String,
    /// 受信済みの最後の ORDER seq(次回接続の `since_seq`。初回は 0 — 不変条件 O2)。
    last_seq: u32,
    /// 連続再接続失敗回数(バックオフ計算用)。
    attempt: u32,
    /// 現在の状態。
    state: SessionState,
    /// 受信・確定を反映するスレ(閲覧に板鍵は不要 — 検証は署名のみ)。
    thread: Thread,
}

impl ParticipantSession {
    /// 空のスレを対象にセッションを作る(初回接続前)。
    ///
    /// `thread` は表示・確定反映の器(閲覧のみのため板鍵は不要)。`challenge` は
    /// [`generate_challenge`] で生成した今回分。
    pub fn new(thread: Thread, challenge: impl Into<String>) -> Self {
        let board_id = thread.board_id.clone();
        let generation = thread.generation;
        Self {
            board_id,
            generation,
            challenge: challenge.into(),
            last_seq: 0,
            attempt: 0,
            state: SessionState::Joining,
            thread,
        }
    }

    /// 現在の状態。
    pub fn state(&self) -> SessionState {
        self.state
    }

    /// 次回接続で送る `since_seq`(受信済み最後の seq)。
    pub fn since_seq(&self) -> u32 {
        self.last_seq
    }

    /// 確定済みレス列(表示用の読み取りビュー)。
    pub fn confirmed(&self) -> &[Res] {
        &self.thread.res
    }

    /// 対象スレの読み取り(状態・世代の参照用)。
    pub fn thread(&self) -> &Thread {
        &self.thread
    }

    /// 送信すべき `THREAD_JOIN` メッセージを生成する(T022 — 参→ホ)。
    ///
    /// `since_seq` には受信済み最後の seq を載せ、瞬断復帰時に差分同期できるようにする。
    pub fn join_message(&self) -> WireMessage {
        WireMessage::ThreadJoin {
            thread: format!("{}:{}", self.board_id, self.generation),
            challenge: self.challenge.clone(),
            since_seq: self.last_seq,
        }
    }

    /// `THREAD_WELCOME` を受信して検証する(T022 — FR-005)。
    ///
    /// `sig` を **announce 記載のスレ主公開鍵(= `board_id`)** で検証する。成功時は joined へ
    /// 遷移し試行回数をリセット、失敗時は Disconnected へ遷移して
    /// [`WelcomeOutcome::ChallengeFailed`] を返す(配線側が切断 + 記録 + バックオフ)。
    pub fn on_welcome(&mut self, sig_hex: &str) -> WelcomeOutcome {
        if verify_welcome_sig(sig_hex, &self.challenge, &self.board_id, self.generation) {
            self.state = SessionState::Joined;
            self.attempt = 0;
            WelcomeOutcome::Accepted
        } else {
            self.state = SessionState::Disconnected;
            self.record_failure();
            WelcomeOutcome::ChallengeFailed {
                category: SecurityCategory::LivechatChallengeFailed,
            }
        }
    }

    /// `THREAD_REJECT` を受信したときの扱いを返す(reason 別)。
    ///
    /// 未知の reason は前方互換のためバックオフ再試行として扱う(切断・記録は配線側)。
    pub fn on_reject(&mut self, reason: &str) -> RejectHandling {
        use crate::p2p::frame::thread_reject_reason as r;
        self.state = SessionState::Disconnected;
        match reason {
            r::FULL | r::RATE => {
                self.record_failure();
                RejectHandling::Backoff
            }
            r::CLOSED | r::UNKNOWN_THREAD => RejectHandling::GiveUp,
            r::FROZEN => RejectHandling::WaitFrozen,
            _ => {
                self.record_failure();
                RejectHandling::Backoff
            }
        }
    }

    /// 再接続失敗を記録し、次回バックオフ遅延(秒)を返す。
    pub fn record_failure(&mut self) -> u64 {
        let delay = backoff_delay_secs(self.attempt);
        self.attempt = self.attempt.saturating_add(1);
        delay
    }

    /// 現在の試行回数に対応するバックオフ遅延(秒)。
    pub fn current_backoff_secs(&self) -> u64 {
        backoff_delay_secs(self.attempt)
    }

    // --- T023: 接続時同期の受信 --------------------------------------------

    /// 同期の `ORDER`(kind 21311)を適用して確定列を進める(T023 — 参加者側)。
    ///
    /// 検証順(thread-events.md §参加者側検証): 署名者一致(スレ主)→ seq 連続性 →
    /// res_no 連続性。`resolve` は event_id から本文(未確定に保持済みのレス封筒)を引く
    /// コールバックで、確定対象のレス実体を配線側の保留プールから取得する。
    ///
    /// - seq が `last_seq + 1` でなければ [`SyncError::SeqGap`](表示を進めず再送要求)。
    /// - スレ主以外の署名は [`SyncError::OrderInvalid`](記録は配線側 — FR-011)。
    /// - res_no 連続性違反・確定失敗は [`SyncError::Confirm`]。
    ///
    /// 成功時は `entries` を順に [`Thread::confirm`] し、`last_seq` を更新する。
    pub fn apply_order<F>(&mut self, order: &OrderEnvelope, mut resolve: F) -> Result<(), SyncError>
    where
        F: FnMut(&str) -> Option<Res>,
    {
        // スレ主一致(署名者 = board_id)。封筒側で署名検証済みの前提だが、対象スレの
        // board_id・世代の一致も確認する(別スレの ORDER を取り込まない)。
        if order.board_id != self.board_id || order.generation != self.generation {
            return Err(SyncError::OrderInvalid);
        }
        // seq 連続性(O2): last_seq の次でなければ表示を進めない。
        if order.seq != self.last_seq + 1 {
            return Err(SyncError::SeqGap {
                expected: self.last_seq + 1,
                got: order.seq,
            });
        }
        // res_no 連続性は Thread::confirm が強制する。各エントリを順に確定する。
        for entry in &order.entries {
            let res = resolve(&entry.event_id).ok_or(SyncError::MissingRes)?;
            self.thread
                .confirm(res, entry.res_no)
                .map_err(SyncError::Confirm)?;
        }
        self.last_seq = order.seq;
        Ok(())
    }
}

/// 同期受信の失敗理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncError {
    /// seq の欠落(表示を進めず [`WireMessage::ResendReq`] で再送要求 — 不変条件 O2)。
    SeqGap { expected: u32, got: u32 },
    /// 対象スレ不一致・スレ主不一致(FR-011 — `livechat_order_invalid` は配線側が記録)。
    OrderInvalid,
    /// 確定対象レスの実体が保留プールに見つからない(RES 未着 → 再送要求)。
    MissingRes,
    /// 確定の不変条件違反(res_no 連続性・res_limit — [`ThreadError`])。
    Confirm(ThreadError),
}

/// 参加者が確定列の器へ取り込むためのレス封筒 → ドメイン Res 変換(閲覧は署名のみで可)。
///
/// event_id は封筒側の [`nostr::Event`] が持つため、本関数は event id と署名者(板鍵)を
/// 別途受け取る。`pending = false`(同期で受け取るのは確定候補であり自分の未確定投稿ではない)。
pub fn res_from_envelope(
    env: &ResEnvelope,
    event_id: &str,
    board_key: &str,
    created_at: i64,
) -> Res {
    Res {
        event_id: event_id.to_string(),
        board_key: board_key.to_string(),
        name: env.name.clone(),
        mail: env.mail.clone(),
        body: env.body.clone(),
        created_at,
        res_no: None,
        pending: false,
    }
}

/// 検証済みイベントからドメイン Res を組み立てる(event id・署名者を [`Event`] から採る)。
pub fn res_from_event(env: &ResEnvelope, event: &Event) -> Res {
    res_from_envelope(
        env,
        &event.id.to_hex(),
        &event.pubkey.to_hex(),
        event.created_at.as_secs() as i64,
    )
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::livechat::OrderEntry;
    use crate::livechat::host;
    use crate::p2p::frame::thread_reject_reason;
    use nostr::Keys;

    const GUID: &str = "0123456789abcdef0123456789abcdef";

    fn persona() -> Keys {
        Keys::generate()
    }

    fn sample_thread(board_id: &str) -> Thread {
        let channel = format!("30311:{board_id}:{GUID}");
        Thread::new(board_id, channel, 1, 1_700_000_000, "実況スレ", 1000)
    }

    fn sample_res(event_id: &str) -> Res {
        Res {
            event_id: event_id.to_string(),
            board_key: "cd".repeat(32),
            name: None,
            mail: None,
            body: "本文".to_string(),
            created_at: 1_700_000_000,
            res_no: None,
            pending: false,
        }
    }

    // --- チャレンジ往復: host の WELCOME sig を検証成功 ----------------------

    #[test]
    fn challenge_roundtrip_verifies() {
        let keys = persona();
        let board_id = keys.public_key().to_hex();
        let challenge = generate_challenge();
        assert!(is_lower_hex(&challenge, 64), "challenge は hex64");
        let sig = host::sign_welcome(&keys, &challenge, &board_id, 1).unwrap();

        let mut session = ParticipantSession::new(sample_thread(&board_id), challenge);
        assert_eq!(session.on_welcome(&sig), WelcomeOutcome::Accepted);
        assert_eq!(session.state(), SessionState::Joined);
    }

    // --- 改ざん sig は失敗 + livechat_challenge_failed ----------------------

    #[test]
    fn tampered_sig_fails_with_challenge_failed() {
        let keys = persona();
        let board_id = keys.public_key().to_hex();
        let challenge = generate_challenge();
        let sig = host::sign_welcome(&keys, &challenge, &board_id, 1).unwrap();
        // 署名を 1 文字改ざん。
        let mut tampered: Vec<char> = sig.chars().collect();
        tampered[0] = if tampered[0] == 'a' { 'b' } else { 'a' };
        let tampered: String = tampered.into_iter().collect();

        let mut session = ParticipantSession::new(sample_thread(&board_id), challenge);
        assert_eq!(
            session.on_welcome(&tampered),
            WelcomeOutcome::ChallengeFailed {
                category: SecurityCategory::LivechatChallengeFailed
            }
        );
        assert_eq!(session.state(), SessionState::Disconnected);
    }

    #[test]
    fn welcome_sig_from_wrong_key_fails() {
        // 別のペルソナ鍵で署名した WELCOME は board_id と一致せず失敗する(偽スレ対策)。
        let host_keys = persona();
        let attacker = persona();
        let board_id = host_keys.public_key().to_hex();
        let challenge = generate_challenge();
        // attacker が board_id を詐称して署名しても、検証は board_id の公開鍵で行うため失敗。
        let sig = host::sign_welcome(&attacker, &challenge, &board_id, 1).unwrap();
        assert!(!verify_welcome_sig(&sig, &challenge, &board_id, 1));
    }

    // --- バックオフ数列(5,10,20,...,300 で頭打ち)--------------------------

    #[test]
    fn backoff_sequence_doubles_and_caps() {
        assert_eq!(backoff_delay_secs(0), 5);
        assert_eq!(backoff_delay_secs(1), 10);
        assert_eq!(backoff_delay_secs(2), 20);
        assert_eq!(backoff_delay_secs(3), 40);
        assert_eq!(backoff_delay_secs(4), 80);
        assert_eq!(backoff_delay_secs(5), 160);
        // 5 * 2^6 = 320 → 300 で頭打ち。
        assert_eq!(backoff_delay_secs(6), 300);
        assert_eq!(backoff_delay_secs(7), 300);
        // 大きな試行回数でもオーバーフローせず上限。
        assert_eq!(backoff_delay_secs(1000), 300);
    }

    #[test]
    fn record_failure_advances_backoff() {
        let keys = persona();
        let board_id = keys.public_key().to_hex();
        let mut session = ParticipantSession::new(sample_thread(&board_id), generate_challenge());
        assert_eq!(session.record_failure(), 5); // attempt 0 の遅延
        assert_eq!(session.record_failure(), 10); // attempt 1
        assert_eq!(session.record_failure(), 20); // attempt 2
    }

    #[test]
    fn accepted_welcome_resets_attempt() {
        let keys = persona();
        let board_id = keys.public_key().to_hex();
        let challenge = generate_challenge();
        let sig = host::sign_welcome(&keys, &challenge, &board_id, 1).unwrap();
        let mut session = ParticipantSession::new(sample_thread(&board_id), challenge);
        session.record_failure();
        session.record_failure();
        assert_eq!(session.on_welcome(&sig), WelcomeOutcome::Accepted);
        // 受理で試行回数リセット → 次の遅延は初期値。
        assert_eq!(session.current_backoff_secs(), 5);
    }

    // --- REJECT reason 別の扱い ---------------------------------------------

    #[test]
    fn reject_handling_by_reason() {
        let keys = persona();
        let board_id = keys.public_key().to_hex();
        let mk = || ParticipantSession::new(sample_thread(&board_id), generate_challenge());
        assert_eq!(
            mk().on_reject(thread_reject_reason::FULL),
            RejectHandling::Backoff
        );
        assert_eq!(
            mk().on_reject(thread_reject_reason::RATE),
            RejectHandling::Backoff
        );
        assert_eq!(
            mk().on_reject(thread_reject_reason::CLOSED),
            RejectHandling::GiveUp
        );
        assert_eq!(
            mk().on_reject(thread_reject_reason::UNKNOWN_THREAD),
            RejectHandling::GiveUp
        );
        assert_eq!(
            mk().on_reject(thread_reject_reason::FROZEN),
            RejectHandling::WaitFrozen
        );
        // 未知コードは前方互換でバックオフ再試行。
        assert_eq!(mk().on_reject("future_reason"), RejectHandling::Backoff);
    }

    // --- T023: 同期受信で確定順序復元 --------------------------------------

    #[test]
    fn apply_order_confirms_in_res_no_order() {
        let keys = persona();
        let board_id = keys.public_key().to_hex();
        let mut session = ParticipantSession::new(sample_thread(&board_id), generate_challenge());

        let id1 = "11".repeat(32);
        let id2 = "22".repeat(32);
        // 保留プール(event_id → Res)を模す。
        let pool = |eid: &str| -> Option<Res> {
            if eid == "11".repeat(32) {
                Some(sample_res(&"11".repeat(32)))
            } else if eid == "22".repeat(32) {
                Some(sample_res(&"22".repeat(32)))
            } else {
                None
            }
        };

        let order = OrderEnvelope {
            board_id: board_id.clone(),
            generation: 1,
            seq: 1,
            entries: vec![
                OrderEntry {
                    res_no: 1,
                    event_id: id1.clone(),
                },
                OrderEntry {
                    res_no: 2,
                    event_id: id2.clone(),
                },
            ],
        };
        session.apply_order(&order, pool).unwrap();

        let confirmed = session.confirmed();
        assert_eq!(confirmed.len(), 2);
        assert_eq!(confirmed[0].res_no, Some(1));
        assert_eq!(confirmed[0].event_id, id1);
        assert_eq!(confirmed[1].res_no, Some(2));
        assert_eq!(confirmed[1].event_id, id2);
        assert_eq!(session.since_seq(), 1);
    }

    #[test]
    fn apply_order_detects_seq_gap() {
        let keys = persona();
        let board_id = keys.public_key().to_hex();
        let mut session = ParticipantSession::new(sample_thread(&board_id), generate_challenge());
        // last_seq=0 のとき seq=2 が来ると欠落。
        let order = OrderEnvelope {
            board_id: board_id.clone(),
            generation: 1,
            seq: 2,
            entries: vec![OrderEntry {
                res_no: 1,
                event_id: "11".repeat(32),
            }],
        };
        let err = session
            .apply_order(&order, |_| Some(sample_res(&"11".repeat(32))))
            .unwrap_err();
        assert_eq!(
            err,
            SyncError::SeqGap {
                expected: 1,
                got: 2
            }
        );
        // 表示は進まない。
        assert!(session.confirmed().is_empty());
        assert_eq!(session.since_seq(), 0);
    }

    #[test]
    fn apply_order_rejects_wrong_thread() {
        let keys = persona();
        let board_id = keys.public_key().to_hex();
        let mut session = ParticipantSession::new(sample_thread(&board_id), generate_challenge());
        // 別の board_id の ORDER は取り込まない。
        let order = OrderEnvelope {
            board_id: "ff".repeat(32),
            generation: 1,
            seq: 1,
            entries: vec![OrderEntry {
                res_no: 1,
                event_id: "11".repeat(32),
            }],
        };
        assert_eq!(
            session
                .apply_order(&order, |_| Some(sample_res(&"11".repeat(32))))
                .unwrap_err(),
            SyncError::OrderInvalid
        );
    }

    #[test]
    fn apply_order_reports_missing_res() {
        let keys = persona();
        let board_id = keys.public_key().to_hex();
        let mut session = ParticipantSession::new(sample_thread(&board_id), generate_challenge());
        let order = OrderEnvelope {
            board_id: board_id.clone(),
            generation: 1,
            seq: 1,
            entries: vec![OrderEntry {
                res_no: 1,
                event_id: "11".repeat(32),
            }],
        };
        // 保留プールに無い → MissingRes(再送要求)。
        assert_eq!(
            session.apply_order(&order, |_| None).unwrap_err(),
            SyncError::MissingRes
        );
    }

    #[test]
    fn apply_consecutive_orders_advances_seq() {
        let keys = persona();
        let board_id = keys.public_key().to_hex();
        let mut session = ParticipantSession::new(sample_thread(&board_id), generate_challenge());
        for (seq, res_no, id) in [(1u32, 1u16, "11"), (2, 2, "22")] {
            let event_id = id.repeat(32);
            let order = OrderEnvelope {
                board_id: board_id.clone(),
                generation: 1,
                seq,
                entries: vec![OrderEntry {
                    res_no,
                    event_id: event_id.clone(),
                }],
            };
            session
                .apply_order(&order, |_| Some(sample_res(&event_id)))
                .unwrap();
        }
        assert_eq!(session.since_seq(), 2);
        assert_eq!(session.confirmed().len(), 2);
    }
}

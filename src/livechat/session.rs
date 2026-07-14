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
use nostr::{Event, Keys, PublicKey};

use crate::event::livechat::{LivechatBuildError, OrderInfo as OrderEnvelope, Res as ResEnvelope};
use crate::p2p::frame::Message as WireMessage;
use crate::security::{SecurityCategory, is_lower_hex};

use super::thread::{BoardSettings, Res, Thread, ThreadError};

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
    /// 自分の未確定投稿(「送信中」表示 — FR-008)。event_id → 送信中 Res(`pending = true`)。
    /// ホストの ORDER で当該 event_id が確定したら確定列へ移り、ここから除去される。
    pending: Vec<Res>,
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
            pending: Vec::new(),
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

    /// NG 判定を適用した可視レス列(T043 — FR-020。[`Thread::visible_res`] への薄い委譲)。
    pub fn visible_confirmed(&self, is_ng: impl Fn(&str) -> bool) -> Vec<&Res> {
        self.thread.visible_res(is_ng)
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

    /// 同期の `ORDER`(kind 21311)を適用して確定列を進める(T023/T031 — 参加者側)。
    ///
    /// **検証順(thread-events.md §参加者側検証 — サイズ → 署名 → スレ主一致 → seq 連続 →
    /// res_no 連続)**:
    ///
    /// 1. **サイズ・署名**: 封筒([`OrderEnvelope::from_event`] + `Event::verify`)が呼び出し前に
    ///    担う(≤ 16KB・id/sig 検証)。本メソッドは検証済み封筒を受け取る前提。
    /// 2. **スレ主一致(FR-011)**: 署名者 = board_id・対象スレの世代一致。不一致は
    ///    [`SyncError::OrderInvalid`](別スレ・偽 ORDER を取り込まない。記録は配線側)。
    /// 3. **seq 連続(O2)**: seq が `last_seq + 1` でなければ [`SyncError::SeqGap`]。表示を
    ///    進めず、配線側は [`Self::resend_request`] で `RESEND_REQ` を送って欠落を埋める。
    /// 4. **res_no 連続(T3)**: [`Thread::confirm`] が欠番なし単調増加・res_limit を強制する。
    ///    違反は [`SyncError::Confirm`]。
    ///
    /// `resolve` は event_id から確定対象のレス実体(配線側の保留プール)を引くコールバック。
    /// 見つからなければ [`SyncError::MissingRes`](RES 未着 → 再送要求)。
    ///
    /// 成功時は `entries` を順に確定し、`last_seq` を更新する。**確定したレスのみが表示列
    /// ([`Self::confirmed`])に入る**(未確定は表示しない — FR-008)。
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
            // 自分の未確定投稿がこの ORDER で確定したら「送信中」から除去する(FR-008)。
            // 確定実体は確定列(thread.res)へ入るため、pending 側の重複を消す。
            self.pending.retain(|p| p.event_id != entry.event_id);
        }
        self.last_seq = order.seq;
        Ok(())
    }

    /// 欠落した確定情報の再送を要求する `RESEND_REQ` を生成する(T031 — 不変条件 O2)。
    ///
    /// [`SyncError::SeqGap`] を検出したとき、受信済みの次(`last_seq + 1`)から欠落を検出した
    /// `up_to_seq` までの範囲をホストへ要求する(ホストは [`crate::livechat::registry`] の
    /// `handle_resend` で対応 RES + ORDER を seq 順に再送する)。表示は欠落が埋まるまで進めない。
    pub fn resend_request(&self, up_to_seq: u32) -> WireMessage {
        WireMessage::ResendReq {
            from_seq: self.last_seq + 1,
            to_seq: up_to_seq,
        }
    }

    /// 本文中のアンカー `>>n` を確定レスへ解決する(T031 — FR-009)。
    ///
    /// 確定列([`Thread::resolve_anchors_in`])に委譲する。全端末で確定列(res_no → event_id)が
    /// 一致するため、同一 `>>n` は全端末で同一イベントに解決される。未確定・範囲外のアンカーは
    /// 解決しない(確定済みのみ — FR-008/FR-009)。
    pub fn resolve_anchors(&self, body: &str) -> Vec<(u16, &Res)> {
        self.thread.resolve_anchors_in(body)
    }

    // --- T046/T047/T048: ライフサイクル(次スレ移行・明示クローズ・凍結/復帰)------

    /// 対象スレの現在の世代(`NEXT_THREAD` 受信前後の判定に使う — T046)。
    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// 対象スレの現在の状態(Active/Frozen/Closed)。
    pub fn thread_state(&self) -> super::thread::ThreadState {
        self.thread.state
    }

    /// `NEXT_THREAD` を受信して次世代へ移行する(T046 — FR-013)。
    ///
    /// **PlusCal モデルの参加者受信処理「next」分岐に対応**(`knownGen[p] := m.g`)。旧スレの
    /// 表示済みデータは保持したまま(揮発しない — 次スレ移行は「クローズ」ではなく閲覧継続の
    /// 対象)、以後の書き込み・同期は新世代宛になるよう内部状態を切り替える:
    ///
    /// 1. 旧 `Thread` を `freeze()`(不変条件 T1 — 以後この世代への `confirm` は拒否される。
    ///    モデルの「旧世代のログは移行時点から不変」に対応)。
    /// 2. 新しい空の `Thread`(`new_key`・`res_limit`)を対象にセッションを切り替える
    ///    (`generation`・`last_seq` をリセット。新世代は seq 1 から再開するため `since_seq`
    ///    は 0 に戻す — 旧世代の seq 系列と新世代の seq 系列は独立)。
    /// 3. `board_id` は板単位で不変(T049 — 板鍵・NG・BAN は板スコープで next 後も有効。
    ///    本メソッドは板鍵/NG/BAN 自体を保持しない上位層の責務のため何もしない)。
    ///
    /// 旧スレのスナップショットを返す(呼び出し側が「凍結スレとして一覧に残す」等に使う)。
    pub fn apply_next_thread(
        &mut self,
        new_generation: u32,
        new_key: u64,
        res_limit: u16,
    ) -> Thread {
        // 旧世代を Frozen に(既に Frozen/Closed なら freeze は失敗するが、次スレ移行の
        // 起点は Active のはずなので通常は成功する。失敗しても新世代への切り替えは進める —
        // 旧スレの状態表示に不整合が残るより、新スレへの追従を優先する)。
        let _ = self.thread.freeze();
        let old_thread = self.thread.clone();

        let new_thread = Thread::new(
            &self.board_id,
            &old_thread.channel,
            new_generation,
            new_key,
            &old_thread.title,
            res_limit,
        );
        self.thread = new_thread;
        self.generation = new_generation;
        self.last_seq = 0; // 新世代の ORDER seq 系列は独立(O2 は世代ごとに連番)。
        self.pending.clear(); // 旧世代宛の送信中投稿は新世代では確定し得ない。
        old_thread
    }

    /// 明示クローズ通知(`THREAD_CLOSE`)を受信してスレデータを削除する(T047 — FR-014/FR-015)。
    ///
    /// **PlusCal モデルの参加者受信処理「close」分岐に対応**(`pv[p] := [g \in Gens |-> <<>>]`・
    /// `conn[p] := "closed"`)。`Thread::close()` で終端状態へ遷移した上で、確定列・送信中投稿を
    /// すべて空にする(揮発 — ネットワークに何も残らないのと同様、ローカルメモリにも残さない)。
    pub fn apply_close(&mut self) {
        let _ = self.thread.close();
        self.thread.res.clear();
        self.pending.clear();
        self.state = SessionState::Disconnected;
    }

    /// ホストとの接続喪失(TCP 断・PING 無応答)による凍結(T048 — FR-014)。
    ///
    /// **PlusCal モデルの「参加者の切断(凍結)」遷移に対応**(`conn[p] := "frozen"`)。
    /// 取得済みレス(確定列)はそのまま保持し閲覧を継続する(表示は変えない)。`Thread` を
    /// `Frozen` にし、[`SessionState::Disconnected`] へ遷移する(再接続はバックオフ付きで
    /// 上位層 [`crate::livechat::participant::run_with_backoff`] が試みる)。
    ///
    /// 既に Frozen/Closed の場合は状態遷移を試みない(freeze 失敗は無視 — 二重凍結は無害)。
    pub fn on_disconnect(&mut self) {
        let _ = self.thread.freeze();
        self.state = SessionState::Disconnected;
    }

    /// 瞬断復帰(同一 gen が継続していた場合の `Frozen → Active`)を確認する(T048)。
    ///
    /// **PlusCal モデルの「再接続」遷移に対応**(`conn[p] := "joined"`)。再接続先ホストが
    /// 提示した `remote_generation` が本セッションの世代と一致すれば同一世代の継続とみなし
    /// `resume()` を試みる(Frozen → Active)。世代が進んでいた場合(移行を跨いだ切断)は
    /// resume せず `false` を返す — 呼び出し側は代わりに新しい [`ParticipantSession`] を
    /// 世代なりに作り直し、`since_seq=0` から全ログ同期すべき(次スレへの追従は再接続の
    /// 通常フローに委ねる。凍結中に移行が起きた場合の取り扱いは spec Edge Case 参照)。
    ///
    /// 成功時は `true`(セッションは Active へ復帰し、以後 `since_seq()` から差分同期できる)。
    pub fn try_resume(&mut self, remote_generation: u32) -> bool {
        if remote_generation != self.generation {
            return false;
        }
        match self.thread.resume() {
            Ok(()) => {
                self.state = SessionState::Joined;
                true
            }
            Err(_) => false,
        }
    }

    // --- T029: 書き込みクライアント経路(FR-008/FR-024/FR-029)---------------

    /// 板鍵でレスを自動署名して書き込み `RES` を生成する(T029 — 参→ホ)。
    ///
    /// - **板鍵署名(FR-016)**: `board_keys`(板単位の書き込み鍵。未生成なら呼び出し側が
    ///   [`crate::livechat::board::BoardKeyManager`] で生成して渡す)で kind 1311 を署名する。
    /// - **名前欄の `#` 以降除去(FR-024)**: 送信前に除去する(トリップ入力の秘匿)。
    /// - **mail 保持(FR-029)**: 表示互換のためそのまま載せる(機能的意味なし)。
    /// - 本文の制御文字除去・長さ/行数検査は封筒([`ResEnvelope::sign`])が行う。
    ///
    /// 署名済みイベントを**自分の未確定投稿**として `pending`(送信中)へ加え、送出すべき
    /// `RES` メッセージを返す(FR-008 — 送信中表示)。ホストの ORDER で確定すると
    /// [`Self::apply_order`] が pending から除去し確定列へ移す。`created_at` は署名時刻、
    /// `pow_bits` は初見板鍵の PoW(通常は 0。呼び出し側は [`first_post_pow_bits`] で
    /// 「この鍵で初めて書くか」に応じた値を求めて渡す — T044)。
    ///
    /// 形式違反(本文長・行数・名前長・チャンネル/board_id 不正)は
    /// [`LivechatBuildError`] を返し、pending へは加えない。
    #[allow(clippy::too_many_arguments)]
    pub fn compose_write(
        &mut self,
        board_keys: &Keys,
        channel: &str,
        name: Option<String>,
        mail: Option<String>,
        body: &str,
        created_at: u64,
        pow_bits: u8,
    ) -> Result<WireMessage, LivechatBuildError> {
        // 封筒を組んで板鍵で署名する(# 除去・mail 保持・本文検査は sign が担う — FR-024/FR-029)。
        let envelope = ResEnvelope {
            channel: channel.to_string(),
            board_id: self.board_id.clone(),
            generation: self.generation,
            name,
            mail,
            body: body.to_string(),
        };
        let event = envelope.sign(board_keys, created_at, pow_bits)?;

        // 署名済みイベントから復元した表示ビュー(# 除去後の name・制御文字除去後の body)を
        // 「送信中」Res として保持する(FR-008)。復元は封筒の形式検証を兼ねる。
        let view = ResEnvelope::from_event(&event)
            .map_err(|_| LivechatBuildError::Invalid("composed res failed self-verify"))?;
        let mut res = res_from_event(&view, &event);
        res.pending = true; // 自分の未確定投稿(送信中表示)。
        self.pending.push(res);

        Ok(WireMessage::Res {
            event: serde_json::to_value(&event)
                .map_err(|e| LivechatBuildError::Nostr(e.to_string()))?,
        })
    }

    /// 自分の未確定投稿(「送信中」表示 — FR-008)。ホスト採番前のレスのみを含む。
    pub fn pending(&self) -> &[Res] {
        &self.pending
    }

    /// 表示用の全レス列(確定列 + 自分の送信中投稿)。確定分を res_no 順に並べ、末尾へ
    /// 自分の送信中投稿(res_no なし)を付す(FR-008 — 送信中を区別して表示できる形)。
    pub fn display_res(&self) -> Vec<Res> {
        let mut rows: Vec<Res> = self.thread.res.clone();
        rows.extend(self.pending.iter().cloned());
        rows
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

/// 初回書き込みに要求される PoW ビット数を返す(T044 — research R6)。
///
/// 新規生成・ローテーション直後の板鍵の初回書き込みは板設定の `first_post_pow_bits` を、
/// 既知(投稿実績あり)の板鍵は 0(通常しきい値)を使う。判定はクライアント側の
/// 「この鍵で初めて書くか」に基づく(ホスト側の `known_board_keys` と対応 —
/// [`crate::livechat::registry::LivechatRegistry::accept_write`])。[`Self::compose_write`] の
/// `pow_bits` 引数に渡す値を決めるための純粋ヘルパ。
pub fn first_post_pow_bits(settings: &BoardSettings, is_first_post: bool) -> u8 {
    if is_first_post {
        settings.first_post_pow_bits
    } else {
        0
    }
}

/// 受信した `SETTINGS`(`board_settings` JSON)を検証して [`BoardSettings`] へ復元する
/// (T032 — FR-025 受信側検証)。
///
/// `board_settings` は [`crate::livechat::host::board_settings_json`] と対の JSON オブジェクト
/// (title/res_limit/noname_name/local_rules/first_post_pow_bits)。制御文字を除去
/// ([`BoardSettings::sanitized`])した上で値域を検証([`BoardSettings::validate`])し、違反は
/// **破棄**して記録すべきカテゴリ [`SecurityCategory::LivechatSettingsInvalid`] を返す(FR-025)。
/// 参加者はこの検証を通った設定のみ表示へ反映する(不正な設定で表示を汚さない)。
pub fn parse_and_validate_settings(
    board_settings: &serde_json::Value,
) -> Result<BoardSettings, SecurityCategory> {
    let parse_u16 = |key: &str, default: u16| -> u16 {
        board_settings
            .get(key)
            .and_then(|v| v.as_u64())
            .and_then(|n| u16::try_from(n).ok())
            .unwrap_or(default)
    };
    let parse_u8 = |key: &str, default: u8| -> u8 {
        board_settings
            .get(key)
            .and_then(|v| v.as_u64())
            .and_then(|n| u8::try_from(n).ok())
            .unwrap_or(default)
    };
    let parse_str = |key: &str| -> String {
        board_settings
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let defaults = BoardSettings::default();
    let candidate = BoardSettings {
        title: parse_str("title"),
        res_limit: parse_u16("res_limit", defaults.res_limit),
        noname_name: {
            // 名無し名は空文字を許さない(1〜64 文字)。欠落・空は既定値へ寄せず、
            // validate で NonameNameOutOfRange として弾く(受信側規範を厳格に保つ)。
            let n = parse_str("noname_name");
            if board_settings.get("noname_name").is_some() {
                n
            } else {
                defaults.noname_name.clone()
            }
        },
        local_rules: parse_str("local_rules"),
        first_post_pow_bits: parse_u8("first_post_pow_bits", defaults.first_post_pow_bits),
    };
    let sanitized = candidate.sanitized();
    sanitized
        .validate()
        .map(|_| sanitized.clone())
        .map_err(|_| SecurityCategory::LivechatSettingsInvalid)
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
    use nostr::{JsonUtil, Keys};

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

    // --- T029: 書き込みクライアント経路(FR-008/FR-024/FR-029)---------------

    fn channel_of(board_id: &str) -> String {
        format!("30311:{board_id}:{GUID}")
    }

    #[test]
    fn compose_write_signs_with_board_key_and_marks_pending() {
        let keys = persona();
        let board_id = keys.public_key().to_hex();
        let mut session = ParticipantSession::new(sample_thread(&board_id), generate_challenge());
        let board_key = Keys::generate();

        let msg = session
            .compose_write(
                &board_key,
                &channel_of(&board_id),
                Some("名無し".into()),
                Some("sage".into()),
                "本文テスト",
                1_700_000_010,
                0,
            )
            .unwrap();

        // 生成 RES は板鍵で署名され、封筒検証を通る。
        let WireMessage::Res { event } = msg else {
            panic!("RES を期待");
        };
        let raw = event.to_string();
        let signed = Event::from_json(&raw).unwrap();
        assert!(signed.verify().is_ok(), "板鍵署名が検証を通る");
        assert_eq!(signed.pubkey, board_key.public_key(), "署名鍵は板鍵");
        assert_eq!(signed.kind.as_u16(), crate::event::livechat::RES_KIND);

        // 自分の未確定投稿が「送信中」として保持される(FR-008)。
        assert_eq!(session.pending().len(), 1);
        assert!(session.pending()[0].pending, "pending フラグが立つ");
        assert_eq!(session.pending()[0].res_no, None, "未確定は res_no なし");
        assert_eq!(
            session.pending()[0].mail.as_deref(),
            Some("sage"),
            "mail 保持(FR-029)"
        );
    }

    #[test]
    fn compose_write_strips_trip_after_hash() {
        let keys = persona();
        let board_id = keys.public_key().to_hex();
        let mut session = ParticipantSession::new(sample_thread(&board_id), generate_challenge());
        let board_key = Keys::generate();

        // 名前欄に `#トリップ` を含めて送信 → 送信前に `#` 以降が除去される(FR-024)。
        let msg = session
            .compose_write(
                &board_key,
                &channel_of(&board_id),
                Some("コテハン#ひみつ".into()),
                None,
                "本文",
                1_700_000_010,
                0,
            )
            .unwrap();
        let WireMessage::Res { event } = msg else {
            panic!("RES を期待");
        };
        let signed = Event::from_json(event.to_string()).unwrap();
        let restored = ResEnvelope::from_event(&signed).unwrap();
        assert_eq!(
            restored.name.as_deref(),
            Some("コテハン"),
            "# 以降は除去済み"
        );
        // pending 側の表示ビューも除去後の名前。
        assert_eq!(session.pending()[0].name.as_deref(), Some("コテハン"));
    }

    #[test]
    fn pending_becomes_confirmed_on_matching_order() {
        let keys = persona();
        let board_id = keys.public_key().to_hex();
        let mut session = ParticipantSession::new(sample_thread(&board_id), generate_challenge());
        let board_key = Keys::generate();

        // 自分の書き込みを送信中にする。
        let msg = session
            .compose_write(
                &board_key,
                &channel_of(&board_id),
                None,
                None,
                "自分の投稿",
                1_700_000_010,
                0,
            )
            .unwrap();
        let WireMessage::Res { event } = msg else {
            panic!("RES を期待");
        };
        let signed = Event::from_json(event.to_string()).unwrap();
        let event_id = signed.id.to_hex();
        assert_eq!(session.pending().len(), 1);
        assert!(session.confirmed().is_empty());

        // ホストがこの event_id を res_no=1 で確定する ORDER を送る。
        let order = OrderEnvelope {
            board_id: board_id.clone(),
            generation: 1,
            seq: 1,
            entries: vec![OrderEntry {
                res_no: 1,
                event_id: event_id.clone(),
            }],
        };
        // 保留プールは自分の署名済みイベントから復元する(配線側の役割を模す)。
        let view = ResEnvelope::from_event(&signed).unwrap();
        let confirmed_res = res_from_event(&view, &signed);
        session
            .apply_order(&order, |eid| {
                if eid == event_id {
                    Some(confirmed_res.clone())
                } else {
                    None
                }
            })
            .unwrap();

        // 確定列へ移り、送信中から除去される(FR-008 の送信中 → 確定遷移)。
        assert_eq!(session.confirmed().len(), 1);
        assert_eq!(session.confirmed()[0].res_no, Some(1));
        assert_eq!(session.confirmed()[0].body, "自分の投稿");
        assert!(session.pending().is_empty(), "確定後は送信中から消える");
    }

    #[test]
    fn display_res_lists_confirmed_then_pending() {
        let keys = persona();
        let board_id = keys.public_key().to_hex();
        let mut session = ParticipantSession::new(sample_thread(&board_id), generate_challenge());
        let board_key = Keys::generate();

        // 他者の確定レス 1 件を先に反映。
        let other_id = "11".repeat(32);
        let order = OrderEnvelope {
            board_id: board_id.clone(),
            generation: 1,
            seq: 1,
            entries: vec![OrderEntry {
                res_no: 1,
                event_id: other_id.clone(),
            }],
        };
        session
            .apply_order(&order, |_| Some(sample_res(&other_id)))
            .unwrap();
        // 自分の未確定投稿 1 件を送信中にする。
        session
            .compose_write(
                &board_key,
                &channel_of(&board_id),
                None,
                None,
                "送信中の投稿",
                1_700_000_020,
                0,
            )
            .unwrap();

        let rows = session.display_res();
        assert_eq!(rows.len(), 2, "確定 1 + 送信中 1");
        assert_eq!(rows[0].res_no, Some(1), "確定分が先");
        assert!(!rows[0].pending);
        assert_eq!(rows[1].res_no, None, "送信中は末尾・res_no なし");
        assert!(rows[1].pending, "送信中フラグ");
    }

    // --- T031: 参加者 ORDER 検証・表示(FR-008/FR-009/FR-011・O2)-----------

    #[test]
    fn seq_gap_yields_resend_request() {
        let keys = persona();
        let board_id = keys.public_key().to_hex();
        let mut session = ParticipantSession::new(sample_thread(&board_id), generate_challenge());
        // last_seq=0 のとき seq=3 が来ると欠落(seq 1・2 が未着 — O2)。
        let order = OrderEnvelope {
            board_id: board_id.clone(),
            generation: 1,
            seq: 3,
            entries: vec![OrderEntry {
                res_no: 1,
                event_id: "11".repeat(32),
            }],
        };
        let err = session
            .apply_order(&order, |_| Some(sample_res(&"11".repeat(32))))
            .unwrap_err();
        let SyncError::SeqGap { got, .. } = err else {
            panic!("SeqGap を期待: {err:?}");
        };
        // 欠落検出 → RESEND_REQ(from_seq=1, to_seq=3)を生成する(表示は進めない)。
        let req = session.resend_request(got);
        assert_eq!(
            req,
            WireMessage::ResendReq {
                from_seq: 1,
                to_seq: 3
            }
        );
        assert!(session.confirmed().is_empty(), "欠落中は表示を進めない(O2)");
    }

    #[test]
    fn resolve_anchors_resolves_confirmed_only() {
        let keys = persona();
        let board_id = keys.public_key().to_hex();
        let mut session = ParticipantSession::new(sample_thread(&board_id), generate_challenge());

        // 2 レスを確定させる。
        let id1 = "11".repeat(32);
        let id2 = "22".repeat(32);
        for (seq, res_no, id) in [(1u32, 1u16, &id1), (2, 2, &id2)] {
            let order = OrderEnvelope {
                board_id: board_id.clone(),
                generation: 1,
                seq,
                entries: vec![OrderEntry {
                    res_no,
                    event_id: id.clone(),
                }],
            };
            let idc = id.clone();
            session
                .apply_order(&order, move |_| Some(sample_res(&idc)))
                .unwrap();
        }

        // 本文 ">>1 >>2 >>5" → 確定済み 1・2 のみ解決(FR-009 の全端末一致・確定のみ)。
        let resolved = session.resolve_anchors(">>1 と >>2 と >>5");
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].0, 1);
        assert_eq!(resolved[0].1.event_id, id1);
        assert_eq!(resolved[1].0, 2);
        assert_eq!(resolved[1].1.event_id, id2);
    }

    // --- T032: SETTINGS 受信側検証(FR-025)---------------------------------

    #[test]
    fn parse_and_validate_settings_accepts_valid() {
        let json = serde_json::json!({
            "title": "板タイトル",
            "res_limit": 500,
            "noname_name": "名無しさん",
            "local_rules": "ルール",
            "first_post_pow_bits": 16,
        });
        let settings = parse_and_validate_settings(&json).unwrap();
        assert_eq!(settings.title, "板タイトル");
        assert_eq!(settings.res_limit, 500);
        assert_eq!(settings.noname_name, "名無しさん");
        assert_eq!(settings.first_post_pow_bits, 16);
    }

    #[test]
    fn parse_and_validate_settings_rejects_out_of_range() {
        // res_limit=50(100 未満)は値域違反で破棄(FR-025)。
        let json = serde_json::json!({ "res_limit": 50, "noname_name": "名無し" });
        assert_eq!(
            parse_and_validate_settings(&json),
            Err(SecurityCategory::LivechatSettingsInvalid)
        );
    }

    #[test]
    fn parse_and_validate_settings_rejects_empty_noname() {
        // 明示的な空 noname_name(1〜64 文字違反)は破棄。
        let json = serde_json::json!({ "noname_name": "" });
        assert_eq!(
            parse_and_validate_settings(&json),
            Err(SecurityCategory::LivechatSettingsInvalid)
        );
    }

    #[test]
    fn parse_and_validate_settings_strips_control_chars() {
        // 制御文字は sanitized で除去され、除去後の値で検証を通る。
        let json = serde_json::json!({
            "title": "タイトル\u{7}制御",
            "noname_name": "名無し",
        });
        let settings = parse_and_validate_settings(&json).unwrap();
        assert_eq!(settings.title, "タイトル制御", "制御文字が除去される");
    }

    // --- T044: 初回書き込み PoW ビット数の選択(research R6)-------------------

    #[test]
    fn first_post_pow_bits_uses_setting_for_first_post() {
        let settings = BoardSettings {
            first_post_pow_bits: 12,
            ..Default::default()
        };
        assert_eq!(first_post_pow_bits(&settings, true), 12);
    }

    #[test]
    fn first_post_pow_bits_is_zero_for_known_key() {
        let settings = BoardSettings {
            first_post_pow_bits: 12,
            ..Default::default()
        };
        assert_eq!(first_post_pow_bits(&settings, false), 0);
    }

    // --- T046: 次スレ移行(参加者側 — NEXT_THREAD 受信)-----------------------

    #[test]
    fn apply_next_thread_freezes_old_and_switches_to_new_generation() {
        let keys = persona();
        let board_id = keys.public_key().to_hex();
        let mut session = ParticipantSession::new(sample_thread(&board_id), generate_challenge());

        // 旧世代(gen=1)で 1 レス確定させておく(表示済みデータの保持を確認するため)。
        let order = OrderEnvelope {
            board_id: board_id.clone(),
            generation: 1,
            seq: 1,
            entries: vec![OrderEntry {
                res_no: 1,
                event_id: "11".repeat(32),
            }],
        };
        session
            .apply_order(&order, |_| Some(sample_res(&"11".repeat(32))))
            .unwrap();
        assert_eq!(session.confirmed().len(), 1);

        let old = session.apply_next_thread(2, 1_700_001_000, 1000);
        assert_eq!(
            old.state,
            super::super::thread::ThreadState::Frozen,
            "旧スレは Frozen"
        );
        assert_eq!(
            old.res.len(),
            1,
            "旧スレのスナップショットは表示済みデータを保持"
        );

        assert_eq!(session.generation(), 2, "新世代へ切り替わる");
        assert_eq!(
            session.thread_state(),
            super::super::thread::ThreadState::Active
        );
        assert_eq!(
            session.since_seq(),
            0,
            "新世代の seq は 0 から再開(O2 は世代ごと)"
        );
        assert!(session.confirmed().is_empty(), "新スレは空から始まる");
    }

    #[test]
    fn apply_next_thread_clears_pending_writes() {
        let keys = persona();
        let board_id = keys.public_key().to_hex();
        let mut session = ParticipantSession::new(sample_thread(&board_id), generate_challenge());
        let board_key = Keys::generate();

        session
            .compose_write(
                &board_key,
                &channel_of(&board_id),
                None,
                None,
                "移行前の送信中投稿",
                1_700_000_010,
                0,
            )
            .unwrap();
        assert_eq!(session.pending().len(), 1);

        session.apply_next_thread(2, 1_700_001_000, 1000);
        assert!(
            session.pending().is_empty(),
            "旧世代宛の送信中投稿は新世代では確定し得ないため破棄する"
        );
    }

    // --- T047: 明示クローズ(参加者側 — THREAD_CLOSE 受信でデータ削除)---------

    #[test]
    fn apply_close_deletes_thread_data() {
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
        session
            .apply_order(&order, |_| Some(sample_res(&"11".repeat(32))))
            .unwrap();
        assert_eq!(session.confirmed().len(), 1);

        session.apply_close();

        assert!(session.confirmed().is_empty(), "確定列は揮発する(FR-015)");
        assert_eq!(
            session.thread_state(),
            super::super::thread::ThreadState::Closed
        );
        assert_eq!(session.state(), SessionState::Disconnected);
    }

    #[test]
    fn apply_close_clears_pending_writes() {
        let keys = persona();
        let board_id = keys.public_key().to_hex();
        let mut session = ParticipantSession::new(sample_thread(&board_id), generate_challenge());
        let board_key = Keys::generate();
        session
            .compose_write(
                &board_key,
                &channel_of(&board_id),
                None,
                None,
                "クローズ前の送信中投稿",
                1_700_000_010,
                0,
            )
            .unwrap();

        session.apply_close();
        assert!(session.pending().is_empty(), "送信中投稿も揮発する");
    }

    // --- T048: 凍結・復帰(TCP 断 → Frozen → 再接続で Active 復帰)------------

    #[test]
    fn on_disconnect_freezes_and_keeps_confirmed_data() {
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
        session
            .apply_order(&order, |_| Some(sample_res(&"11".repeat(32))))
            .unwrap();

        session.on_disconnect();

        assert_eq!(
            session.thread_state(),
            super::super::thread::ThreadState::Frozen
        );
        assert_eq!(session.state(), SessionState::Disconnected);
        assert_eq!(
            session.confirmed().len(),
            1,
            "凍結中も取得済みレスの閲覧は継続する(FR-014)"
        );
    }

    #[test]
    fn try_resume_succeeds_when_generation_unchanged() {
        let keys = persona();
        let board_id = keys.public_key().to_hex();
        let mut session = ParticipantSession::new(sample_thread(&board_id), generate_challenge());
        session.on_disconnect();
        assert_eq!(
            session.thread_state(),
            super::super::thread::ThreadState::Frozen
        );

        assert!(
            session.try_resume(1),
            "同一 gen が継続していれば Active へ復帰する"
        );
        assert_eq!(
            session.thread_state(),
            super::super::thread::ThreadState::Active
        );
        assert_eq!(session.state(), SessionState::Joined);
    }

    #[test]
    fn try_resume_fails_when_generation_advanced() {
        let keys = persona();
        let board_id = keys.public_key().to_hex();
        let mut session = ParticipantSession::new(sample_thread(&board_id), generate_challenge());
        session.on_disconnect();

        // 凍結中に次スレへ移行していた場合(gen が進んでいる)は resume しない。
        assert!(
            !session.try_resume(2),
            "世代が進んでいれば同一世代継続ではないため resume しない"
        );
        assert_eq!(
            session.thread_state(),
            super::super::thread::ThreadState::Frozen,
            "resume しない場合は Frozen のまま"
        );
    }

    #[test]
    fn try_resume_noop_when_not_frozen() {
        let keys = persona();
        let board_id = keys.public_key().to_hex();
        let mut session = ParticipantSession::new(sample_thread(&board_id), generate_challenge());
        // まだ凍結していない(Active)状態で resume を試みても失敗する。
        assert!(!session.try_resume(1));
    }
}

//! 参加者接続ドライバ(配線層 — US1 読み取り/同期)
//!
//! **明示操作(スレを開く)**を起点に、ホスト([`crate::livechat::registry`] を持つ相手
//! ノード、または契約テストのモックホスト)へ outbound 接続し、HELLO(feature `livechat1`)
//! → THREAD_JOIN → THREAD_WELCOME 検証([`crate::livechat::session`])→ 接続時同期の受信
//! までを駆動する。announce 受信**のみ**では接続しない(SC-005 — 接続は本ドライバの明示
//! 呼び出しでしか始まらない)。
//!
//! 状態機械の判断(WELCOME 検証・REJECT の扱い・同期の確定反映・バックオフ)は
//! [`ParticipantSession`] が担う純粋ロジック。本モジュールはそれをトランスポート(TCP)へ
//! 配線し、1 回の接続試行の結果を [`JoinResult`] として返す。凍結後の再接続ループ
//! ([`run_with_backoff`])はバックオフ付きで試行を繰り返す。

use std::sync::Arc;
use std::time::Duration;

use nostr::JsonUtil;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

use crate::event::livechat::{OrderInfo as OrderEnvelope, Res as ResEnvelope};
use crate::p2p::frame::{Hello, Message, read_frame, write_frame};
use crate::p2p::session::{FEATURE_LIVECHAT1, PROTOCOL_VERSION};
use crate::security::{SecurityCategory, SecurityLog};

use super::session::{
    ParticipantSession, RejectHandling, SyncError, WelcomeOutcome, generate_challenge,
    res_from_event,
};
use super::thread::{Res, Thread};

/// 初回同期のアイドル打ち切り時間。WELCOME 後にこの時間だけ RES/ORDER が来なければ、
/// 初回同期のバッチが尽きたとみなして確定列を返す(継続受信は US2)。
const SYNC_IDLE: Duration = Duration::from_millis(500);

/// 1 回の接続試行の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinResult {
    /// WELCOME 検証成功 → joined。同期で確定した確定レス列を持ち帰る。
    Joined { confirmed: Vec<Res> },
    /// WELCOME の sig 検証失敗(切断 + `livechat_challenge_failed` 記録済み + 要バックオフ)。
    ChallengeFailed,
    /// 定型 REJECT 受信(reason 別に扱いが分かれる — [`RejectHandling`])。
    Rejected {
        reason: String,
        handling: RejectHandling,
    },
    /// トランスポート/プロトコルのエラー(接続失敗・切断・不正フレーム)。
    Transport,
}

/// 参加者ドライバの依存(接続先・対象スレ・観測用ログ)。
pub struct ParticipantConfig {
    /// ホスト接続先(announce の `tip` — `ip:port`)。
    pub host_addr: String,
    /// 対象スレの板 id(スレ主ペルソナ pubkey hex)。
    pub board_id: String,
    /// 対象チャンネル(`30311:<pubkey>:<guid>` — 器の Thread に持たせる)。
    pub channel: String,
    /// スレ世代。
    pub generation: u32,
    /// スレ作成 unix 秒(器の Thread に持たせる)。
    pub key: u64,
    /// タイトル(器の Thread に持たせる — 表示用)。
    pub title: String,
    /// res_limit(器の Thread に持たせる)。
    pub res_limit: u16,
    /// セキュリティイベントログ(チャレンジ失敗の記録用)。
    pub security: Option<Arc<SecurityLog>>,
}

impl ParticipantConfig {
    /// 器となる空スレ(閲覧のみ・板鍵不要)を作る。
    fn make_thread(&self) -> Thread {
        Thread::new(
            &self.board_id,
            &self.channel,
            self.generation,
            self.key,
            &self.title,
            self.res_limit,
        )
    }
}

/// 1 回の接続試行を行う(明示操作起点)。`since_seq` は差分同期の起点(初回 0)。
///
/// 実 TCP でホストへ接続し、[`drive`] でハンドシェイク〜同期を駆動する。接続失敗は
/// [`JoinResult::Transport`]。
pub async fn connect_once(config: &ParticipantConfig, since_seq: u32) -> JoinResult {
    let Ok(stream) = TcpStream::connect(&config.host_addr).await else {
        return JoinResult::Transport;
    };
    let (reader, writer) = stream.into_split();
    drive(config, since_seq, reader, writer).await
}

/// ハンドシェイク〜JOIN〜WELCOME 検証〜同期受信を駆動する(トランスポート非依存 — テスト可能)。
///
/// 手順(thread-delivery.md §参加者):
/// 1. HELLO(feature `livechat1`)を送り HELLO_ACK を待つ。
/// 2. THREAD_JOIN(challenge=32B 乱数 hex, since_seq)を送る。
/// 3. THREAD_WELCOME を受けたら [`ParticipantSession::on_welcome`] で sig 検証。成功なら
///    joined、失敗なら `livechat_challenge_failed` を記録して [`JoinResult::ChallengeFailed`]。
/// 4. WELCOME 後は同期の RES/ORDER を受信し、封筒署名を検証してから
///    [`ParticipantSession::apply_order`] で確定列へ反映する(FR-011: ORDER 署名者 =
///    board_id を配線層で強制する)。
/// 5. THREAD_REJECT は reason 別に [`RejectHandling`] を返す。
pub async fn drive<R, W>(
    config: &ParticipantConfig,
    since_seq: u32,
    mut reader: R,
    mut writer: W,
) -> JoinResult
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    // 1. HELLO(livechat1 を掲げる)。
    let hello = Message::Hello(Hello {
        version: PROTOCOL_VERSION,
        listen_port: 0,
        features: vec![FEATURE_LIVECHAT1.into()],
        nonce: rand_nonce(),
        ts: unix_now(),
    });
    if write_frame(&mut writer, &hello).await.is_err() {
        return JoinResult::Transport;
    }
    match read_frame(&mut reader).await {
        Ok(Some(frame)) if matches!(frame.message, Message::HelloAck(_)) => {}
        _ => return JoinResult::Transport,
    }

    // 2. THREAD_JOIN(challenge を生成してセッションへ保持)。
    let challenge = generate_challenge();
    let mut session = ParticipantSession::new(config.make_thread(), challenge.clone());
    // since_seq を反映するため、器の状態にかかわらずワイヤの since_seq を採用する。
    let join = Message::ThreadJoin {
        thread: format!("{}:{}", config.board_id, config.generation),
        challenge,
        since_seq,
    };
    if write_frame(&mut writer, &join).await.is_err() {
        return JoinResult::Transport;
    }

    // 3. WELCOME / REJECT を待つ。
    let first = match read_frame(&mut reader).await {
        Ok(Some(f)) => f.message,
        _ => return JoinResult::Transport,
    };
    match first {
        Message::ThreadWelcome { sig, .. } => match session.on_welcome(&sig) {
            WelcomeOutcome::Accepted => {}
            WelcomeOutcome::ChallengeFailed { category } => {
                self_log(config, category);
                return JoinResult::ChallengeFailed;
            }
        },
        Message::ThreadReject { reason } => {
            let handling = session.on_reject(&reason);
            return JoinResult::Rejected { reason, handling };
        }
        // WELCOME/REJECT 以外が最初に来るのはプロトコル違反。
        _ => return JoinResult::Transport,
    }

    // 4. 同期受信(WELCOME 後に続く RES/ORDER を確定列へ反映する)。
    //    保留プール: event_id → 未確定レス(RES 先着 → ORDER 確定で res_no 付与)。
    //
    //    ホストは同期完了後も接続を維持し RES/ORDER を継続配信する(US2)。US1 の読み取りでは
    //    「初回同期のバッチ」を代表して確定列を返すため、フレームが [`SYNC_IDLE`] 継続して
    //    来なければ同期完了とみなす(アイドル打ち切り)。相手切断(EOF)でも同様に確定分で
    //    joined を成立させる。継続受信ループ(差分同期・凍結復帰)は US2 で別途構築する。
    let mut pending: std::collections::HashMap<String, Res> = std::collections::HashMap::new();
    loop {
        let read = tokio::time::timeout(SYNC_IDLE, read_frame(&mut reader)).await;
        let frame = match read {
            // アイドル打ち切り(これ以上の同期フレームが来ない) → 確定分で joined 成立。
            Err(_) => break,
            Ok(Ok(Some(f))) => f,
            // 相手切断(EOF)/エラーもここまでの確定分で joined 成立とみなす。
            Ok(Ok(None)) | Ok(Err(_)) => break,
        };
        match frame.message {
            Message::Res { event } => {
                // 封筒署名検証 + 形式検証(kind 1311)。失敗は破棄(前方互換で切断しない)。
                if let Some(res) = verify_res(&event) {
                    pending.insert(res.event_id.clone(), res);
                }
            }
            Message::Order { event } => {
                // FR-011: ORDER の署名者は board_id(スレ主)でなければならない。
                match verify_order(&event, &config.board_id) {
                    Some(order) => {
                        let resolve = |eid: &str| pending.get(eid).cloned();
                        match session.apply_order(&order, resolve) {
                            Ok(()) => {}
                            // seq 欠落は RESEND_REQ を送って続行(欠落検出 — O2)。
                            Err(SyncError::SeqGap { .. }) => {
                                let req = Message::ResendReq {
                                    from_seq: session.since_seq() + 1,
                                    to_seq: order.seq,
                                };
                                let _ = write_frame(&mut writer, &req).await;
                            }
                            // その他(確定不能・スレ不一致)は破棄して続行。
                            Err(_) => {}
                        }
                    }
                    None => {
                        // スレ主以外の署名 = 偽 ORDER。記録して破棄(表示に影響させない)。
                        self_log(config, SecurityCategory::LivechatOrderInvalid);
                    }
                }
            }
            // SETTINGS/その他ホスト→参 メッセージは US1 では観測対象外(前方互換で無視)。
            Message::Settings { .. } => {}
            // gossip 混在・不正フレームはホスト側が切断する。参加者側は EOF で抜ける。
            _ => {}
        }
    }

    JoinResult::Joined {
        confirmed: session.confirmed().to_vec(),
    }
}

/// 凍結後の再接続をバックオフ付きで繰り返す(FR-014 — 瞬断復帰)。
///
/// `max_attempts` 回まで接続を試み、Joined か GiveUp で終える。各失敗の後は
/// [`ParticipantSession`] のバックオフ数列(5,10,20,…,300)に従って待機する。テストで
/// 短時間に収束させたい場合は `sleep_scale` で待機を縮められる(本番は 1.0)。
pub async fn run_with_backoff(
    config: &ParticipantConfig,
    max_attempts: u32,
    sleep_scale: f64,
) -> JoinResult {
    let mut attempt = 0u32;
    let mut last = JoinResult::Transport;
    // US1 は初回同期(since_seq=0)のみ。差分同期(since_seq 更新)の継続受信は US2 で扱う。
    let since_seq = 0u32;
    while attempt < max_attempts {
        let result = connect_once(config, since_seq).await;
        match &result {
            JoinResult::Joined { .. } => {
                return result;
            }
            JoinResult::Rejected { handling, .. } => match handling {
                RejectHandling::GiveUp => return result,
                RejectHandling::Backoff | RejectHandling::WaitFrozen => {}
            },
            JoinResult::ChallengeFailed | JoinResult::Transport => {}
        }
        last = result;
        // バックオフ(試行回数に応じた遅延)。テストは sleep_scale で短縮できる。
        let delay = crate::livechat::session::backoff_delay_secs(attempt) as f64 * sleep_scale;
        if delay > 0.0 {
            tokio::time::sleep(Duration::from_secs_f64(delay)).await;
        }
        attempt += 1;
    }
    last
}

/// 受信 RES(kind 1311)の封筒署名 + 形式を検証してドメイン Res を作る。
///
/// nostr の id/sig 検証と [`ResEnvelope::from_event`] の形式検証を通す。失敗は `None`。
fn verify_res(event_value: &serde_json::Value) -> Option<Res> {
    let raw = event_value.to_string();
    let event = nostr::Event::from_json(&raw).ok()?;
    if event.verify().is_err() {
        return None;
    }
    let envelope = ResEnvelope::from_event(&event).ok()?;
    Some(res_from_event(&envelope, &event))
}

/// 受信 ORDER(kind 21311)の封筒署名 + スレ主一致(FR-011)を検証する。
///
/// nostr の id/sig 検証・[`OrderEnvelope::from_event`] の形式検証に加え、**署名者 pubkey が
/// `board_id`(スレ主)と一致**しなければ `None`(偽 ORDER — 破棄 + 記録は呼び出し側)。
fn verify_order(event_value: &serde_json::Value, board_id: &str) -> Option<OrderEnvelope> {
    let raw = event_value.to_string();
    let event = nostr::Event::from_json(&raw).ok()?;
    if event.verify().is_err() {
        return None;
    }
    // FR-011: 順序確定情報はスレ主ペルソナ鍵で署名されていなければならない。
    if event.pubkey.to_hex() != board_id {
        return None;
    }
    OrderEnvelope::from_event(&event).ok()
}

/// チャレンジ失敗・偽 ORDER をセキュリティログへ記録する(source = ホストアドレス)。
fn self_log(config: &ParticipantConfig, category: SecurityCategory) {
    if let Some(log) = &config.security {
        log.log(category, &config.host_addr, category.as_str());
    }
}

/// 32 バイト乱数の下位 8 バイトから HELLO nonce を作る(自己接続検出用・衝突回避)。
fn rand_nonce() -> u64 {
    use nostr::secp256k1::rand::RngCore;
    let mut buf = [0u8; 8];
    nostr::secp256k1::rand::rngs::OsRng.fill_bytes(&mut buf);
    u64::from_le_bytes(buf)
}

/// 現在の unix 秒(HELLO の `ts` 用)。
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

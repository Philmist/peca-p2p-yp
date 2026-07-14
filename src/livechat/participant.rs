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
    /// `THREAD_CLOSE` を受信してスレデータを削除した(T047 — FR-014/FR-015)。
    /// 再接続は行わない(スレそのものが終端したため — [`RejectHandling::GiveUp`] と同格)。
    Closed,
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
    // 1〜3. ハンドシェイク(HELLO → JOIN → WELCOME 検証)。joined 済みセッションを得る。
    let mut session = match handshake_join(config, since_seq, &mut reader, &mut writer).await {
        Ok(s) => s,
        Err(result) => return result,
    };

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
            Message::NextThread { generation, key } => {
                // T046: 次スレ移行。旧スレは Frozen(表示済みデータは保持)、以後の同期は
                // 新世代宛(seq は新世代で 1 から再開 — O2 は世代ごとに独立した連番)。
                session.apply_next_thread(generation, key, config.res_limit);
                pending.clear(); // 旧世代宛の保留プールは新世代の ORDER と対応しないため破棄。
            }
            Message::ThreadClose { .. } => {
                // T047: 明示クローズ。スレデータを削除して終了する(FR-014/FR-015)。
                session.apply_close();
                return JoinResult::Closed;
            }
            // gossip 混在・不正フレームはホスト側が切断する。参加者側は EOF で抜ける。
            _ => {}
        }
    }

    JoinResult::Joined {
        confirmed: session.confirmed().to_vec(),
    }
}

/// ハンドシェイク(HELLO → THREAD_JOIN → WELCOME 検証)を行い joined 済みセッションを返す
/// ([`drive`] と [`connect_write_collect`] の共通部)。
///
/// 成功時は [`SessionState::Joined`] な [`ParticipantSession`](提示した challenge を保持)を
/// `Ok` で返す。失敗はそのまま返すべき [`JoinResult`](Transport / ChallengeFailed / Rejected)を
/// `Err` で返す(呼び出し側は `return` するだけ)。
async fn handshake_join<R, W>(
    config: &ParticipantConfig,
    since_seq: u32,
    reader: &mut R,
    writer: &mut W,
) -> Result<ParticipantSession, JoinResult>
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
    if write_frame(writer, &hello).await.is_err() {
        return Err(JoinResult::Transport);
    }
    match read_frame(reader).await {
        Ok(Some(frame)) if matches!(frame.message, Message::HelloAck(_)) => {}
        _ => return Err(JoinResult::Transport),
    }

    // 2. THREAD_JOIN(challenge を生成してセッションへ保持)。
    let challenge = generate_challenge();
    let mut session = ParticipantSession::new(config.make_thread(), challenge.clone());
    let join = Message::ThreadJoin {
        thread: format!("{}:{}", config.board_id, config.generation),
        challenge,
        since_seq,
    };
    if write_frame(writer, &join).await.is_err() {
        return Err(JoinResult::Transport);
    }

    // 3. WELCOME / REJECT を待つ。
    let first = match read_frame(reader).await {
        Ok(Some(f)) => f.message,
        _ => return Err(JoinResult::Transport),
    };
    match first {
        Message::ThreadWelcome { sig, .. } => match session.on_welcome(&sig) {
            WelcomeOutcome::Accepted => Ok(session),
            WelcomeOutcome::ChallengeFailed { category } => {
                self_log(config, category);
                Err(JoinResult::ChallengeFailed)
            }
        },
        Message::ThreadReject { reason } => {
            let handling = session.on_reject(&reason);
            Err(JoinResult::Rejected { reason, handling })
        }
        // WELCOME/REJECT 以外が最初に来るのはプロトコル違反。
        _ => Err(JoinResult::Transport),
    }
}

/// **書き込みラウンドトリップ**: joined 後に `bodies` を書き込み、自分と他参加者の確定が
/// `expect_total` 件溜まるまで受信して確定列を返す(US2 — T033 統合テスト用)。
///
/// 手順:
/// 1. TCP 接続 → [`handshake_join`](HELLO → JOIN → WELCOME 検証)。失敗は
///    [`JoinResult::Transport`] / [`JoinResult::ChallengeFailed`] / [`JoinResult::Rejected`]。
/// 2. joined 後、`bodies` を [`ParticipantSession::compose_write`](板鍵 `board_keys`・
///    name/mail なし・PoW 0)で 1 件ずつ RES 送出(pending = 送信中 — FR-008)。
/// 3. 受信ループ: RES は保留プールへ、ORDER は FR-011 検証([`verify_order`])後に
///    [`ParticipantSession::apply_order`] で確定。**`confirmed().len() >= expect_total`** に
///    達したら終了。各読みは `tokio::time::timeout(idle, …)` で包み、**アイドル/EOF/エラーでも
///    打ち切って現在の確定列を返す(絶対にハングしない)**。SeqGap 時は [`resend_request`] 送出。
/// 4. [`JoinResult::Joined { confirmed }`] を返す(`confirmed` は res_no 順の確定列)。
pub async fn connect_write_collect(
    config: &ParticipantConfig,
    board_keys: &nostr::Keys,
    bodies: &[&str],
    expect_total: usize,
    idle: Duration,
) -> JoinResult {
    let Ok(stream) = TcpStream::connect(&config.host_addr).await else {
        return JoinResult::Transport;
    };
    let (mut reader, mut writer) = stream.into_split();

    // 1. ハンドシェイク。
    let mut session = match handshake_join(config, 0, &mut reader, &mut writer).await {
        Ok(s) => s,
        Err(result) => return result,
    };

    // 2. 自分の書き込みを送出(送信中 = pending)。
    for body in bodies {
        match session.compose_write(
            board_keys,
            &config.channel,
            None,
            None,
            body,
            unix_now() as u64,
            0,
        ) {
            Ok(msg) => {
                if write_frame(&mut writer, &msg).await.is_err() {
                    return JoinResult::Transport;
                }
            }
            // 形式違反(本文長・行数等)は送らずスキップ(前方互換で切断しない)。
            Err(_) => continue,
        }
    }

    // 3. 受信ループ: expect_total に達するまで RES/ORDER を処理。ハング防止のため各読みを
    //    idle タイムアウトで包み、アイドル/EOF/エラーで打ち切る。
    let mut pending: std::collections::HashMap<String, Res> = std::collections::HashMap::new();
    while session.confirmed().len() < expect_total {
        let read = tokio::time::timeout(idle, read_frame(&mut reader)).await;
        let frame = match read {
            Err(_) => break, // アイドル打ち切り(これ以上来ない)
            Ok(Ok(Some(f))) => f,
            Ok(Ok(None)) | Ok(Err(_)) => break, // EOF / I/O エラー
        };
        match frame.message {
            Message::Res { event } => {
                if let Some(res) = verify_res(&event) {
                    pending.insert(res.event_id.clone(), res);
                }
            }
            Message::Order { event } => match verify_order(&event, &config.board_id) {
                Some(order) => {
                    let resolve = |eid: &str| pending.get(eid).cloned();
                    match session.apply_order(&order, resolve) {
                        Ok(()) => {}
                        Err(SyncError::SeqGap { .. }) => {
                            let _ =
                                write_frame(&mut writer, &session.resend_request(order.seq)).await;
                        }
                        Err(_) => {}
                    }
                }
                None => self_log(config, SecurityCategory::LivechatOrderInvalid),
            },
            Message::Settings { .. } => {}
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
            // T047: スレがクローズ済み。再接続しても復帰しないため GiveUp と同格に扱う。
            JoinResult::Closed => return result,
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

// ---------------------------------------------------------------------------
// T048: 凍結・復帰(継続受信 + 切断検知 + since_seq 差分同期での再接続)
// ---------------------------------------------------------------------------

/// 継続受信中に生じうる終端理由(T048 — 凍結・クローズ・スレ機能上のエラー)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEnd {
    /// TCP 断・PING 無応答等で接続が失われた(Frozen へ遷移済み — FR-014)。
    /// 呼び出し側は取得済みレスの閲覧を継続しつつバックオフ再接続すべき。
    Disconnected,
    /// `THREAD_CLOSE` を受信してスレデータを削除した(T047)。以後は再接続しない。
    Closed,
}

/// 既に joined 済みの [`ParticipantSession`] を使って継続受信する(T048)。
///
/// `drive` の初回同期(アイドル打ち切りで確定分を返す — US1)とは異なり、本関数は
/// **アイドル打ち切りをしない**: フレームが来る限り受信を続け、EOF/I/O エラー時に
/// のみ終了する。これにより「ホストが同期完了後も継続配信する」(US2)接続を、
/// 予期しない切断(Frozen)が起きるまで維持できる。
///
/// - `RES`/`ORDER`/`SETTINGS` は `drive` と同じ処理(封筒検証 → 確定反映)。
/// - `NEXT_THREAD` は [`ParticipantSession::apply_next_thread`] で次世代へ切り替える(T046)。
/// - `THREAD_CLOSE` は [`ParticipantSession::apply_close`] でデータ削除し
///   [`StreamEnd::Closed`] を返す(T047)。
/// - EOF/I/O エラーは [`ParticipantSession::on_disconnect`] で Frozen へ遷移し
///   [`StreamEnd::Disconnected`] を返す(T048)。
pub async fn stream_until_disconnect<R, W>(
    config: &ParticipantConfig,
    session: &mut ParticipantSession,
    mut reader: R,
    mut writer: W,
) -> StreamEnd
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut pending: std::collections::HashMap<String, Res> = std::collections::HashMap::new();
    loop {
        let frame = match read_frame(&mut reader).await {
            Ok(Some(f)) => f,
            // EOF・I/O エラーは通知なき切断(瞬断・障害) — Frozen へ(FR-014)。
            Ok(None) | Err(_) => {
                session.on_disconnect();
                return StreamEnd::Disconnected;
            }
        };
        match frame.message {
            Message::Res { event } => {
                if let Some(res) = verify_res(&event) {
                    pending.insert(res.event_id.clone(), res);
                }
            }
            Message::Order { event } => match verify_order(&event, &config.board_id) {
                Some(order) => {
                    let resolve = |eid: &str| pending.get(eid).cloned();
                    match session.apply_order(&order, resolve) {
                        Ok(()) => {}
                        Err(SyncError::SeqGap { .. }) => {
                            let req = Message::ResendReq {
                                from_seq: session.since_seq() + 1,
                                to_seq: order.seq,
                            };
                            let _ = write_frame(&mut writer, &req).await;
                        }
                        Err(_) => {}
                    }
                }
                None => self_log(config, SecurityCategory::LivechatOrderInvalid),
            },
            Message::Settings { .. } => {}
            Message::NextThread { generation, key } => {
                session.apply_next_thread(generation, key, config.res_limit);
                pending.clear();
            }
            Message::ThreadClose { .. } => {
                session.apply_close();
                return StreamEnd::Closed;
            }
            _ => {}
        }
    }
}

/// ハンドシェイクのみ行い、joined 済みセッションと分割ソケットを返す(T048 — 再接続用)。
///
/// [`handshake_join`] の薄いラッパ。呼び出し側([`run_forever`])が
/// [`stream_until_disconnect`] と組み合わせて「接続 → 継続受信 → 切断 → 再接続」の
/// ループを回すために公開する。
async fn connect_and_handshake(
    config: &ParticipantConfig,
    since_seq: u32,
) -> Result<
    (
        ParticipantSession,
        tokio::net::tcp::OwnedReadHalf,
        tokio::net::tcp::OwnedWriteHalf,
    ),
    JoinResult,
> {
    let stream = TcpStream::connect(&config.host_addr)
        .await
        .map_err(|_| JoinResult::Transport)?;
    let (mut reader, mut writer) = stream.into_split();
    let session = handshake_join(config, since_seq, &mut reader, &mut writer).await?;
    Ok((session, reader, writer))
}

/// 明示操作(スレを開く)を起点に、接続 → 継続受信 → 凍結 → バックオフ再接続 を繰り返す
/// (T048 — FR-014)。
///
/// 初回は `since_seq=0` で接続し、以後は [`ParticipantSession::since_seq`] を引き継いで
/// 再接続する(**同一 gen が継続していれば** `since_seq` からの差分同期で Active へ復帰する
/// — spec「凍結中の再接続はバックオフ付きで試行され、ホスト再開後の実況は次スレとして
/// 扱われる」)。`THREAD_REJECT(unknown_thread)`(旧スレが消滅・別世代化した場合)を受けたら
/// 再接続を諦める([`RejectHandling::GiveUp`] と同格)。
///
/// `max_attempts` に達するか、[`StreamEnd::Closed`](T047)・
/// [`RejectHandling::GiveUp`](旧スレ消滅)に到達したら終了する。戻り値は最終セッション
/// (呼び出し側が確定列・状態を読む)。
pub async fn run_forever(
    config: &ParticipantConfig,
    max_attempts: u32,
    sleep_scale: f64,
) -> (Option<ParticipantSession>, JoinResult) {
    let mut attempt = 0u32;
    let mut since_seq = 0u32;
    let mut last_session: Option<ParticipantSession> = None;

    while attempt < max_attempts {
        let (mut session, reader, writer) = match connect_and_handshake(config, since_seq).await {
            Ok(parts) => parts,
            Err(
                result @ JoinResult::Rejected {
                    handling: RejectHandling::GiveUp,
                    ..
                },
            ) => {
                return (last_session, result);
            }
            Err(result @ JoinResult::Closed) => {
                return (last_session, result);
            }
            Err(result) => {
                // Transport/ChallengeFailed/Backoff/WaitFrozen 系はバックオフして再試行する。
                let delay =
                    crate::livechat::session::backoff_delay_secs(attempt) as f64 * sleep_scale;
                if delay > 0.0 {
                    tokio::time::sleep(Duration::from_secs_f64(delay)).await;
                }
                attempt += 1;
                last_session = None;
                if attempt >= max_attempts {
                    return (last_session, result);
                }
                continue;
            }
        };

        // 接続に成功したので試行回数をリセットし、継続受信へ入る。
        attempt = 0;
        let end = stream_until_disconnect(config, &mut session, reader, writer).await;
        since_seq = session.since_seq();
        match end {
            StreamEnd::Closed => {
                return (Some(session), JoinResult::Closed);
            }
            StreamEnd::Disconnected => {
                // Frozen 済み(session.on_disconnect 済み)。バックオフして再接続する。
                last_session = Some(session);
                let delay =
                    crate::livechat::session::backoff_delay_secs(attempt) as f64 * sleep_scale;
                if delay > 0.0 {
                    tokio::time::sleep(Duration::from_secs_f64(delay)).await;
                }
                attempt += 1;
            }
        }
    }

    (
        last_session,
        JoinResult::Joined {
            confirmed: Vec::new(),
        },
    )
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

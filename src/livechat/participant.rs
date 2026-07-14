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
use super::thread::{BoardSettings, Res, Thread};

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
        Ok((s, _board_settings)) => s,
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
) -> Result<(ParticipantSession, serde_json::Value), JoinResult>
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
        Message::ThreadWelcome {
            sig,
            board_settings,
            ..
        } => match session.on_welcome(&sig) {
            // 板設定(WELCOME 同梱)も持ち帰る(T064 — 他ノード板の設定表示に使う)。
            WelcomeOutcome::Accepted => Ok((session, board_settings)),
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
        Ok((s, _board_settings)) => s,
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
        serde_json::Value,
        tokio::net::tcp::OwnedReadHalf,
        tokio::net::tcp::OwnedWriteHalf,
    ),
    JoinResult,
> {
    let stream = TcpStream::connect(&config.host_addr)
        .await
        .map_err(|_| JoinResult::Transport)?;
    let (mut reader, mut writer) = stream.into_split();
    let (session, board_settings) =
        handshake_join(config, since_seq, &mut reader, &mut writer).await?;
    Ok((session, board_settings, reader, writer))
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
        let (mut session, _board_settings, reader, writer) =
            match connect_and_handshake(config, since_seq).await {
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

// ---------------------------------------------------------------------------
// T064: 常駐セッション(ライブ状態共有 + 書き込みコマンド)
// ---------------------------------------------------------------------------

/// 常駐セッションのライブ状態(視聴者から見た表示・接続状態)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLiveState {
    /// 接続確立前・再接続中(まだ WELCOME を受けていない)。
    Connecting,
    /// joined(継続受信中)。書き込み可。
    Active,
    /// 通知なき切断で凍結(取得済みレスの閲覧のみ・バックオフ再接続中 — FR-014)。
    Frozen,
    /// 明示クローズまたはスレ消滅で終端(以後再接続しない — FR-014/FR-015)。
    Closed,
}

/// 常駐セッションのライブ・スナップショット(マネージャが保持し UI/互換 API が読む)。
#[derive(Debug, Clone)]
pub struct SessionView {
    /// 確定レス(res_no 昇順)。
    pub confirmed: Vec<Res>,
    /// 自分の未確定投稿(送信中 — FR-008)。
    pub pending: Vec<Res>,
    /// 現在の世代。
    pub generation: u32,
    /// スレ作成 unix 秒(現行世代)。
    pub key: u64,
    /// 現在の接続・表示状態。
    pub state: SessionLiveState,
    /// 板設定(WELCOME/SETTINGS 由来。未受信・検証失敗は `None`)。
    pub settings: Option<BoardSettings>,
    /// スレが終端した(Closed / GiveUp)か。true なら再接続しない。
    pub terminated: bool,
}

impl SessionView {
    /// 接続前の初期状態(器の Thread から世代・key を引き継ぐ)。
    pub fn initial(generation: u32, key: u64) -> Self {
        SessionView {
            confirmed: Vec::new(),
            pending: Vec::new(),
            generation,
            key,
            state: SessionLiveState::Connecting,
            settings: None,
            terminated: false,
        }
    }
}

/// マネージャ → セッションタスクへの書き込み要求(T066)。
pub struct WriteCommand {
    /// 送信に使う板鍵(視聴者の当該板向け書き込み鍵。ローテーション後は新鍵)。
    pub board_keys: nostr::Keys,
    /// 名前欄(`#` 以降除去は封筒 sign が担う — FR-024)。
    pub name: Option<String>,
    /// メール欄(表示互換のみ — FR-029)。
    pub mail: Option<String>,
    /// 本文。
    pub body: String,
    /// 初回書き込み PoW ビット(初見板鍵は `first_post_pow_bits`、既知は 0 — research R6)。
    pub pow_bits: u8,
}

/// フレーム適用の結果(継続 / 終端)。
enum FrameOutcome {
    Continue,
    Closed,
}

/// 共有ビューへ現在のセッション状態を書き出す(確定列・送信中・世代・状態)。
fn publish_view(
    shared: &std::sync::Mutex<SessionView>,
    session: &ParticipantSession,
    state: SessionLiveState,
) {
    let mut view = shared.lock().unwrap_or_else(|e| e.into_inner());
    view.confirmed = session.confirmed().to_vec();
    view.pending = session.pending().to_vec();
    view.generation = session.generation();
    view.state = state;
}

/// 受信フレーム 1 件を joined 済みセッションへ適用する(継続受信 — [`stream_until_disconnect`] と
/// 同じ検証規則。書き込みと同一接続で多重化するため本ヘルパーに切り出す)。
async fn apply_session_frame<W>(
    config: &ParticipantConfig,
    session: &mut ParticipantSession,
    shared: &std::sync::Mutex<SessionView>,
    pending: &mut std::collections::HashMap<String, Res>,
    writer: &mut W,
    msg: Message,
) -> FrameOutcome
where
    W: AsyncWrite + Unpin,
{
    match msg {
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
                        let _ = write_frame(writer, &req).await;
                    }
                    Err(_) => {}
                }
            }
            None => self_log(config, SecurityCategory::LivechatOrderInvalid),
        },
        Message::Settings { board_settings } => {
            // 受信側検証を通った設定のみ表示へ反映(FR-025)。違反は破棄。
            if let Ok(bs) = crate::livechat::session::parse_and_validate_settings(&board_settings) {
                shared.lock().unwrap_or_else(|e| e.into_inner()).settings = Some(bs);
            }
        }
        Message::NextThread { generation, key } => {
            session.apply_next_thread(generation, key, config.res_limit);
            pending.clear();
            shared.lock().unwrap_or_else(|e| e.into_inner()).key = key;
        }
        Message::ThreadClose { .. } => {
            session.apply_close();
            return FrameOutcome::Closed;
        }
        _ => {}
    }
    FrameOutcome::Continue
}

/// 明示操作(スレを開く)を起点に常駐し、継続受信 + 書き込み + 凍結/復帰/クローズを駆動する
/// (T064 — FR-004/FR-008/FR-010/FR-014/FR-015)。
///
/// [`run_forever`] の接続 → 継続受信 → バックオフ再接続ループを土台に、次の 2 点を足す:
///
/// 1. **ライブ状態共有**: フレーム適用ごとに `shared`([`SessionView`])へ確定列・送信中・
///    世代・状態を書き出す(UI/互換 API がポーリングで読む)。
/// 2. **書き込み多重化**: `cmd_rx` から [`WriteCommand`] を受け、joined 中の同一接続で
///    [`ParticipantSession::compose_write`] → RES 送出する(送信中 = pending — FR-008)。
///
/// **キャンセル安全性**: 受信(`read_frame`)は別タスクへ分離し、[`Message`] を mpsc で
/// 主ループへ渡す。主ループは受信フレームと書き込みコマンドを [`tokio::select`] で待つため、
/// `read_frame` を select で途中キャンセルしてフレーム境界を壊すことがない。
///
/// `cmd_rx` が閉じた(マネージャがセッションを破棄した)ら終了する。`max_attempts` 到達・
/// `THREAD_CLOSE`・`GiveUp`(旧スレ消滅)でも終了し、`shared.terminated=true` を立てる。
pub async fn run_session(
    config: ParticipantConfig,
    shared: std::sync::Arc<std::sync::Mutex<SessionView>>,
    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<WriteCommand>,
    max_attempts: u32,
    sleep_scale: f64,
) {
    let channel = config.channel.clone();
    let mut since_seq = 0u32;
    let mut attempt = 0u32;

    while attempt < max_attempts {
        let (mut session, board_settings, reader, mut writer) =
            match connect_and_handshake(&config, since_seq).await {
                Ok(parts) => parts,
                Err(JoinResult::Rejected {
                    handling: RejectHandling::GiveUp,
                    ..
                })
                | Err(JoinResult::Closed) => break,
                Err(_) => {
                    // Transport/ChallengeFailed 系はバックオフ再試行(Connecting のまま)。
                    set_view_state(&shared, SessionLiveState::Connecting);
                    backoff(attempt, sleep_scale).await;
                    attempt += 1;
                    continue;
                }
            };
        attempt = 0;
        // WELCOME 同梱の板設定を反映(検証通過分のみ)。
        if let Ok(bs) = crate::livechat::session::parse_and_validate_settings(&board_settings) {
            shared.lock().unwrap_or_else(|e| e.into_inner()).settings = Some(bs);
        }
        publish_view(&shared, &session, SessionLiveState::Active);

        // 受信を別タスクへ分離(キャンセル安全)。フレームを mpsc で主ループへ渡す。
        let (frame_tx, mut frame_rx) = tokio::sync::mpsc::unbounded_channel::<Option<Message>>();
        let reader_task = tokio::spawn(async move {
            let mut reader = reader;
            loop {
                match read_frame(&mut reader).await {
                    Ok(Some(f)) => {
                        if frame_tx.send(Some(f.message)).is_err() {
                            break;
                        }
                    }
                    _ => {
                        let _ = frame_tx.send(None);
                        break;
                    }
                }
            }
        });

        let mut pending: std::collections::HashMap<String, Res> = std::collections::HashMap::new();
        // 継続受信 + 書き込み多重化。終端理由を持ち帰る。
        enum LoopEnd {
            Disconnected,
            Closed,
            ManagerGone,
        }
        let end = loop {
            tokio::select! {
                biased;
                maybe_cmd = cmd_rx.recv() => {
                    let Some(cmd) = maybe_cmd else { break LoopEnd::ManagerGone; };
                    // 形式違反(本文長・行数等)は送らずスキップ(前方互換で切断しない)。
                    // 書き込み失敗(切断途上)は凍結扱いで再接続へ。
                    if let Ok(msg) = session.compose_write(
                        &cmd.board_keys,
                        &channel,
                        cmd.name,
                        cmd.mail,
                        &cmd.body,
                        unix_now() as u64,
                        cmd.pow_bits,
                    ) && write_frame(&mut writer, &msg).await.is_err()
                    {
                        break LoopEnd::Disconnected;
                    }
                    publish_view(&shared, &session, SessionLiveState::Active);
                }
                maybe_frame = frame_rx.recv() => {
                    match maybe_frame {
                        Some(Some(msg)) => {
                            let outcome = apply_session_frame(
                                &config, &mut session, &shared, &mut pending, &mut writer, msg,
                            ).await;
                            match outcome {
                                FrameOutcome::Continue => {
                                    publish_view(&shared, &session, SessionLiveState::Active);
                                }
                                FrameOutcome::Closed => break LoopEnd::Closed,
                            }
                        }
                        // 受信タスク終了(EOF/エラー) = 通知なき切断。
                        Some(None) | None => break LoopEnd::Disconnected,
                    }
                }
            }
        };
        reader_task.abort();
        since_seq = session.since_seq();

        match end {
            LoopEnd::Closed => {
                // 明示クローズ: データ削除済み・終端(FR-014/FR-015)。
                let mut view = shared.lock().unwrap_or_else(|e| e.into_inner());
                view.confirmed.clear();
                view.pending.clear();
                view.state = SessionLiveState::Closed;
                view.terminated = true;
                return;
            }
            LoopEnd::ManagerGone => return,
            LoopEnd::Disconnected => {
                // 通知なき切断 → Frozen(閲覧継続)。バックオフして再接続する(FR-014)。
                session.on_disconnect();
                publish_view(&shared, &session, SessionLiveState::Frozen);
                backoff(attempt, sleep_scale).await;
                attempt += 1;
            }
        }
    }
    // max_attempts 到達・GiveUp・Closed(handshake 段)で終端。
    let mut view = shared.lock().unwrap_or_else(|e| e.into_inner());
    view.terminated = true;
    if view.state != SessionLiveState::Closed {
        view.state = SessionLiveState::Frozen;
    }
}

/// バックオフ待機(試行回数に応じた遅延。テストは `sleep_scale` で短縮できる)。
async fn backoff(attempt: u32, sleep_scale: f64) {
    let delay = crate::livechat::session::backoff_delay_secs(attempt) as f64 * sleep_scale;
    if delay > 0.0 {
        tokio::time::sleep(Duration::from_secs_f64(delay)).await;
    }
}

/// 共有ビューの状態だけを更新する(接続中・凍結の遷移表示用)。
fn set_view_state(shared: &std::sync::Mutex<SessionView>, state: SessionLiveState) {
    shared.lock().unwrap_or_else(|e| e.into_inner()).state = state;
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

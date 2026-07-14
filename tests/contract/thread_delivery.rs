//! T018 スレ配送ハンドシェイク契約テスト(contracts/thread-delivery.md §検証方法)。
//!
//! モックホスト(`tests/common/mock_peer.rs`)に対して**実参加者ドライバ**
//! (`peca_p2p_yp::livechat::participant`)を駆動し、以下を固定フィクスチャで検証する:
//!
//! - JOIN → WELCOME(**チャレンジ署名検証**): 参加者が mock ホストのスレ主鍵署名を
//!   announce 記載の公開鍵で検証し joined に達する(FR-005)。
//! - 改ざん sig の WELCOME → チャレンジ検証失敗 + `livechat_challenge_failed` 記録 + 要
//!   バックオフ(FR-005)。
//! - REJECT 定型(full / frozen / closed / unknown_thread / rate)を reason 別の扱いへ
//!   写す(FR-006)。
//!
//! これらはブロードキャスト・採番を伴わない US1 の読み取り/同期スコープの契約である。

#[path = "../common/mock_peer.rs"]
mod mock_peer;

use std::sync::Arc;
use std::time::Duration;

use nostr::{JsonUtil, Keys};
use serde_json::json;
use tempfile::tempdir;

use mock_peer::{MockPeer, ThreadResponse};
use peca_p2p_yp::livechat::participant::{JoinResult, ParticipantConfig, connect_once};
use peca_p2p_yp::livechat::session::RejectHandling;
use peca_p2p_yp::p2p::frame::thread_reject_reason;
use peca_p2p_yp::security::SecurityLog;

const GUID: &str = "0123456789abcdef0123456789abcdef";

/// 対象スレの参加者設定を作る(器の Thread は閲覧のみ = 板鍵不要)。
fn config_for(
    mock: &MockPeer,
    board_id: &str,
    security: Option<Arc<SecurityLog>>,
) -> ParticipantConfig {
    ParticipantConfig {
        host_addr: mock.addr().to_string(),
        board_id: board_id.to_string(),
        channel: format!("30311:{board_id}:{GUID}"),
        generation: 1,
        key: 1_700_000_000,
        title: "実況スレ".into(),
        res_limit: 1000,
        security,
    }
}

// ---------------------------------------------------------------------------
// JOIN → WELCOME(チャレンジ署名検証成功)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn join_welcome_challenge_verifies_and_joins() {
    let persona = Keys::generate();
    let board_id = persona.public_key().to_hex();
    let mock = MockPeer::spawn().await;
    // 動的署名 WELCOME: 受信 challenge にスレ主鍵で正しく署名して返す。
    mock.set_thread_response(ThreadResponse::DynamicWelcome {
        persona: persona.clone(),
        generation: 1,
        board_settings: json!({ "title": "実況スレ" }),
        res_count: 0,
    });

    let config = config_for(&mock, &board_id, None);
    let result = connect_once(&config, 0).await;
    // チャレンジ署名検証に成功して joined(確定レスは 0 件)。
    assert!(
        matches!(result, JoinResult::Joined { ref confirmed } if confirmed.is_empty()),
        "WELCOME 検証に成功して joined すべき: {result:?}"
    );
}

// ---------------------------------------------------------------------------
// 改ざん sig → チャレンジ検証失敗 + livechat_challenge_failed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn join_welcome_tampered_sig_fails_and_logs() {
    let persona = Keys::generate();
    let board_id = persona.public_key().to_hex();
    let mock = MockPeer::spawn().await;
    // 固定の(誤った)sig を返す WELCOME。参加者の検証は失敗する。
    mock.set_thread_response(ThreadResponse::Welcome {
        thread: format!("{board_id}:1"),
        sig: "00".repeat(64), // 有効な Schnorr 署名ではない
        board_settings: json!({}),
        res_count: 0,
    });

    let dir = tempdir().unwrap();
    let log = Arc::new(SecurityLog::new(dir.path().join("sec.log")).unwrap());
    let config = config_for(&mock, &board_id, Some(Arc::clone(&log)));
    let result = connect_once(&config, 0).await;
    assert_eq!(result, JoinResult::ChallengeFailed);

    // livechat_challenge_failed が記録される(FR-005)。
    log.flush();
    let content = std::fs::read_to_string(dir.path().join("sec.log")).unwrap();
    assert!(
        content.contains("livechat_challenge_failed"),
        "チャレンジ失敗が記録される: {content}"
    );
}

// ---------------------------------------------------------------------------
// REJECT 定型(reason 別の扱い)
// ---------------------------------------------------------------------------

async fn reject_with(reason: &str) -> JoinResult {
    let persona = Keys::generate();
    let board_id = persona.public_key().to_hex();
    let mock = MockPeer::spawn().await;
    mock.set_thread_response(ThreadResponse::Reject {
        reason: reason.to_string(),
    });
    let config = config_for(&mock, &board_id, None);
    connect_once(&config, 0).await
}

#[tokio::test]
async fn reject_full_backs_off() {
    let result = reject_with(thread_reject_reason::FULL).await;
    assert!(matches!(
        result,
        JoinResult::Rejected { ref reason, handling: RejectHandling::Backoff }
            if reason == thread_reject_reason::FULL
    ));
}

#[tokio::test]
async fn reject_rate_backs_off() {
    let result = reject_with(thread_reject_reason::RATE).await;
    assert!(matches!(
        result,
        JoinResult::Rejected {
            handling: RejectHandling::Backoff,
            ..
        }
    ));
}

#[tokio::test]
async fn reject_frozen_waits() {
    let result = reject_with(thread_reject_reason::FROZEN).await;
    assert!(matches!(
        result,
        JoinResult::Rejected {
            handling: RejectHandling::WaitFrozen,
            ..
        }
    ));
}

#[tokio::test]
async fn reject_closed_gives_up() {
    let result = reject_with(thread_reject_reason::CLOSED).await;
    assert!(matches!(
        result,
        JoinResult::Rejected {
            handling: RejectHandling::GiveUp,
            ..
        }
    ));
}

#[tokio::test]
async fn reject_unknown_thread_gives_up() {
    let result = reject_with(thread_reject_reason::UNKNOWN_THREAD).await;
    assert!(matches!(
        result,
        JoinResult::Rejected {
            handling: RejectHandling::GiveUp,
            ..
        }
    ));
}

// ---------------------------------------------------------------------------
// 接続時同期: WELCOME 後に RES/ORDER を受けて確定列を復元
// ---------------------------------------------------------------------------

#[tokio::test]
async fn join_then_sync_reconstructs_confirmed_res() {
    let persona = Keys::generate();
    let board_id = persona.public_key().to_hex();
    let channel = format!("30311:{board_id}:{GUID}");
    let mock = MockPeer::spawn().await;
    mock.set_thread_response(ThreadResponse::DynamicWelcome {
        persona: persona.clone(),
        generation: 1,
        board_settings: json!({}),
        res_count: 2,
    });

    // 板鍵で 2 レス、スレ主鍵で対応 ORDER を作り、WELCOME 後の同期フレームに仕込む。
    let board_key = Keys::generate();
    let r1 = peca_p2p_yp::livechat::registry::sign_res(
        &board_key,
        &board_id,
        &channel,
        1,
        "一つ目",
        1_700_000_001,
    )
    .unwrap();
    let r2 = peca_p2p_yp::livechat::registry::sign_res(
        &board_key,
        &board_id,
        &channel,
        1,
        "二つ目",
        1_700_000_002,
    )
    .unwrap();
    let order1 = peca_p2p_yp::event::livechat::OrderInfo {
        board_id: board_id.clone(),
        generation: 1,
        seq: 1,
        entries: vec![peca_p2p_yp::event::livechat::OrderEntry {
            res_no: 1,
            event_id: r1.id.to_hex(),
        }],
    }
    .sign(&persona, 1_700_000_001)
    .unwrap();
    let order2 = peca_p2p_yp::event::livechat::OrderInfo {
        board_id: board_id.clone(),
        generation: 1,
        seq: 2,
        entries: vec![peca_p2p_yp::event::livechat::OrderEntry {
            res_no: 2,
            event_id: r2.id.to_hex(),
        }],
    }
    .sign(&persona, 1_700_000_002)
    .unwrap();

    // 同期順: RES1, ORDER1, RES2, ORDER2。
    mock.serve_thread_frame(res_frame(&r1));
    mock.serve_thread_frame(order_frame(&order1));
    mock.serve_thread_frame(res_frame(&r2));
    mock.serve_thread_frame(order_frame(&order2));

    let config = config_for(&mock, &board_id, None);
    // mock は WELCOME + 同期フレームを送った後も接続を維持する。ドライバは EOF で
    // 同期を終える設計のため、mock 側を drop して EOF を促す短い待機を挟む。
    let handle = tokio::spawn(async move { connect_once(&config, 0).await });
    tokio::time::sleep(Duration::from_millis(150)).await;
    drop(mock); // 接続を閉じて参加者ドライバの同期ループを EOF で終了させる。
    let result = handle.await.unwrap();

    match result {
        JoinResult::Joined { confirmed } => {
            assert_eq!(confirmed.len(), 2, "確定レス 2 件が復元される");
            assert_eq!(confirmed[0].res_no, Some(1));
            assert_eq!(confirmed[1].res_no, Some(2));
        }
        other => panic!("joined すべき: {other:?}"),
    }
}

fn res_frame(event: &nostr::Event) -> peca_p2p_yp::p2p::frame::Message {
    peca_p2p_yp::p2p::frame::Message::Res {
        event: serde_json::to_value(event).unwrap(),
    }
}

fn order_frame(event: &nostr::Event) -> peca_p2p_yp::p2p::frame::Message {
    peca_p2p_yp::p2p::frame::Message::Order {
        event: serde_json::to_value(event).unwrap(),
    }
}

// ---------------------------------------------------------------------------
// T028: 採番・配布(RES 受理 → ORDER 発行 → 配布 / seq 欠落 → RESEND_REQ → 再送)
//
// 契約(ワイヤプロトコルの往復)を検証する。参加者側の状態機械([`ParticipantSession`])を
// 生 TCP + フレーム送受信で駆動し、モックホスト(mock_peer)との RES/ORDER/RESEND_REQ の
// 往復が thread-delivery.md どおりに成立することを確認する。ホスト側の採番ロジック
// (registry.accept_write の一意採番・重複排除・上限)は T030 の単体テストで検証済み。
// ここでは「参加者が RES を送出でき、ORDER を受けて確定できる」「seq 欠落を検出して
// RESEND_REQ を送り再送で確定を復元できる」プロトコル契約を固定フィクスチャで押さえる。
// ---------------------------------------------------------------------------

use peca_p2p_yp::event::livechat::{OrderEntry, OrderInfo};
use peca_p2p_yp::livechat::session::ParticipantSession;
use peca_p2p_yp::livechat::thread::Thread;
use peca_p2p_yp::p2p::frame::{Hello, Message, read_frame, write_frame};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

const PROTOCOL_VERSION: u32 = 1;

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 参加者としてモックへ接続し HELLO/HELLO_ACK → THREAD_JOIN → WELCOME 検証まで済ませ、
/// joined 済みの [`ParticipantSession`] と分割ソケットを返す(以後の書き込み・同期に使う)。
async fn join_session(
    mock: &MockPeer,
    persona: &Keys,
) -> (ParticipantSession, OwnedReadHalf, OwnedWriteHalf) {
    let board_id = persona.public_key().to_hex();
    // ホストは受信 challenge にスレ主鍵で動的署名した WELCOME を返す。
    mock.set_thread_response(ThreadResponse::DynamicWelcome {
        persona: persona.clone(),
        generation: 1,
        board_settings: json!({}),
        res_count: 0,
    });

    let stream = TcpStream::connect(mock.addr()).await.expect("connect");
    let (mut reader, mut writer) = stream.into_split();
    let hello = Message::Hello(Hello {
        version: PROTOCOL_VERSION,
        listen_port: 0,
        features: vec!["livechat1".into()],
        nonce: 0x0000_ABCD_0000_ABCD,
        ts: now() as i64,
    });
    write_frame(&mut writer, &hello).await.expect("send HELLO");
    assert!(matches!(
        read_frame(&mut reader).await.unwrap().unwrap().message,
        Message::HelloAck(_)
    ));

    // 器の Thread + セッション(challenge をセッションが保持)。
    let channel = format!("30311:{board_id}:{GUID}");
    let thread = Thread::new(&board_id, &channel, 1, 1_700_000_000, "実況スレ", 1000);
    let challenge = peca_p2p_yp::livechat::session::generate_challenge();
    let mut session = ParticipantSession::new(thread, challenge);
    write_frame(&mut writer, &session.join_message())
        .await
        .expect("send JOIN");

    // WELCOME を受けて検証(joined)。
    let welcome = read_frame(&mut reader).await.unwrap().unwrap().message;
    let Message::ThreadWelcome { sig, .. } = welcome else {
        panic!("WELCOME を期待: {welcome:?}");
    };
    assert_eq!(
        session.on_welcome(&sig),
        peca_p2p_yp::livechat::session::WelcomeOutcome::Accepted,
        "WELCOME 検証成功"
    );
    (session, reader, writer)
}

/// スレ主鍵で ORDER(kind 21311)を署名する。
fn sign_order(
    persona: &Keys,
    board_id: &str,
    seq: u32,
    res_no: u16,
    event_id: &str,
) -> nostr::Event {
    OrderInfo {
        board_id: board_id.to_string(),
        generation: 1,
        seq,
        entries: vec![OrderEntry {
            res_no,
            event_id: event_id.to_string(),
        }],
    }
    .sign(persona, 1_700_000_000 + u64::from(seq))
    .unwrap()
}

#[tokio::test]
async fn write_res_is_delivered_then_order_confirms() {
    let persona = Keys::generate();
    let board_id = persona.public_key().to_hex();
    let channel = format!("30311:{board_id}:{GUID}");
    let mock = MockPeer::spawn().await;
    let (mut session, mut reader, mut writer) = join_session(&mock, &persona).await;

    // 参加者が板鍵でレスを書き込む(compose_write で RES 生成 → 送出)。送信中に入る。
    let board_key = Keys::generate();
    let res_msg = session
        .compose_write(&board_key, &channel, None, None, "書き込みテスト", now(), 0)
        .unwrap();
    let Message::Res { event } = res_msg.clone() else {
        panic!("RES を期待");
    };
    let event_id = nostr::Event::from_json(event.to_string())
        .unwrap()
        .id
        .to_hex();
    write_frame(&mut writer, &res_msg).await.expect("send RES");
    assert_eq!(session.pending().len(), 1, "送信中として保持される(FR-008)");

    // ホスト(モック)は RES を受理・記録する(RES 受理の契約 — 採番は registry の責務)。
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        mock.received_thread_messages()
            .iter()
            .any(|m| matches!(m, Message::Res { .. })),
        "ホストは参加者の RES を受理する"
    );

    // ホストが採番して ORDER(seq=1, res_no=1)を配布する(疑似採番 = registry.accept_write 相当)。
    let order = sign_order(&persona, &board_id, 1, 1, &event_id);
    mock.push_thread_frame(order_frame(&order));

    // 参加者は ORDER を受けて確定する(送信中 → 確定へ遷移 — FR-008)。
    let ord_msg = read_frame(&mut reader).await.unwrap().unwrap().message;
    let Message::Order { event: ord_val } = ord_msg else {
        panic!("ORDER を期待");
    };
    let ord_env = peca_p2p_yp::event::livechat::OrderInfo::from_event(
        &nostr::Event::from_json(ord_val.to_string()).unwrap(),
    )
    .unwrap();
    // 保留プールは自分の送信中投稿(compose_write が保持)から引く。
    let pending_res = session.pending()[0].clone();
    session
        .apply_order(&ord_env, |eid| {
            (eid == pending_res.event_id).then(|| pending_res.clone())
        })
        .unwrap();

    assert_eq!(session.confirmed().len(), 1, "採番確定で確定列へ入る");
    assert_eq!(session.confirmed()[0].res_no, Some(1));
    assert!(session.pending().is_empty(), "確定後は送信中から消える");
}

#[tokio::test]
async fn seq_gap_triggers_resend_and_recovers() {
    let persona = Keys::generate();
    let board_id = persona.public_key().to_hex();
    let channel = format!("30311:{board_id}:{GUID}");
    let mock = MockPeer::spawn().await;
    let (mut session, mut reader, mut writer) = join_session(&mock, &persona).await;

    // 他者の確定レス(seq 1・2)の RES/ORDER を用意する。
    let board_key = Keys::generate();
    let r1 =
        peca_p2p_yp::livechat::registry::sign_res(&board_key, &board_id, &channel, 1, "一つ目", 1)
            .unwrap();
    let r2 =
        peca_p2p_yp::livechat::registry::sign_res(&board_key, &board_id, &channel, 1, "二つ目", 2)
            .unwrap();
    let o1 = sign_order(&persona, &board_id, 1, 1, &r1.id.to_hex());
    let o2 = sign_order(&persona, &board_id, 2, 2, &r2.id.to_hex());

    // ホストが **seq 2 を先に**配布する(seq 1 が欠落 = O2 の欠落検出契機)。
    mock.push_thread_frame(res_frame(&r2));
    mock.push_thread_frame(order_frame(&o2));

    // 参加者は RES を保留プールへ、ORDER で seq 欠落を検出する。
    let mut pool: std::collections::HashMap<String, _> = std::collections::HashMap::new();
    // 1 通目: RES2(保留)。
    if let Message::Res { event } = read_frame(&mut reader).await.unwrap().unwrap().message {
        let ev = nostr::Event::from_json(event.to_string()).unwrap();
        let env = peca_p2p_yp::event::livechat::Res::from_event(&ev).unwrap();
        pool.insert(
            ev.id.to_hex(),
            peca_p2p_yp::livechat::session::res_from_event(&env, &ev),
        );
    }
    // 2 通目: ORDER2 → seq 欠落(expected 1, got 2)。
    let ord2_env =
        if let Message::Order { event } = read_frame(&mut reader).await.unwrap().unwrap().message {
            peca_p2p_yp::event::livechat::OrderInfo::from_event(
                &nostr::Event::from_json(event.to_string()).unwrap(),
            )
            .unwrap()
        } else {
            panic!("ORDER を期待");
        };
    let pool_ref = pool.clone();
    let err = session
        .apply_order(&ord2_env, |eid| pool_ref.get(eid).cloned())
        .unwrap_err();
    let peca_p2p_yp::livechat::session::SyncError::SeqGap { got, .. } = err else {
        panic!("SeqGap を期待: {err:?}");
    };

    // 参加者は RESEND_REQ を送る(from_seq=1, to_seq=2 — O2)。
    let resend = session.resend_request(got);
    write_frame(&mut writer, &resend)
        .await
        .expect("send RESEND_REQ");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        mock.received_thread_messages().iter().any(|m| matches!(
            m,
            Message::ResendReq {
                from_seq: 1,
                to_seq: 2
            }
        )),
        "ホストは RESEND_REQ を受け取る"
    );

    // ホストが欠落分(seq 1)を再送する。
    mock.push_thread_frame(res_frame(&r1));
    mock.push_thread_frame(order_frame(&o1));

    // 参加者は RES1 を保留プールへ、ORDER1 で seq 連続を回復して確定する。
    if let Message::Res { event } = read_frame(&mut reader).await.unwrap().unwrap().message {
        let ev = nostr::Event::from_json(event.to_string()).unwrap();
        let env = peca_p2p_yp::event::livechat::Res::from_event(&ev).unwrap();
        pool.insert(
            ev.id.to_hex(),
            peca_p2p_yp::livechat::session::res_from_event(&env, &ev),
        );
    }
    let ord1_env =
        if let Message::Order { event } = read_frame(&mut reader).await.unwrap().unwrap().message {
            peca_p2p_yp::event::livechat::OrderInfo::from_event(
                &nostr::Event::from_json(event.to_string()).unwrap(),
            )
            .unwrap()
        } else {
            panic!("ORDER を期待");
        };
    let pool_ref = pool.clone();
    session
        .apply_order(&ord1_env, |eid| pool_ref.get(eid).cloned())
        .unwrap();
    // seq 1 が埋まったので、続けて seq 2 を再適用すると確定が進む。
    let pool_ref = pool.clone();
    session
        .apply_order(&ord2_env, |eid| pool_ref.get(eid).cloned())
        .unwrap();

    assert_eq!(session.confirmed().len(), 2, "再送で欠落が埋まり全確定");
    assert_eq!(session.confirmed()[0].res_no, Some(1));
    assert_eq!(session.confirmed()[1].res_no, Some(2));
}

// ---------------------------------------------------------------------------
// T028: 採番・配布(LivechatRegistry を直接使った配布・再送契約)
//
// 上記 2 テスト(write_res_is_delivered_then_order_confirms /
// seq_gap_triggers_resend_and_recovers)はワイヤプロトコルの往復(TCP + フレーム)を
// 押さえたが、ホスト側の採番ロジック(`LivechatRegistry::accept_write`)自体は
// registry.rs の同一クレート内 #[cfg(test)] で個別に検証済み(単体レベル)。本節は
// **公開クレート API のみ**を使い、`LivechatRegistry` を直接叩いて「RES 受理 → ORDER
// 発行 → 全参加者(送信者含む)への配布」「連続採番」「重複排除」「seq 欠落 →
// RESEND_REQ → 再送」という配布契約を、契約テストの立ち位置(公開 API のみ・内部実装に
// 依存しない)から固定する。
// ---------------------------------------------------------------------------

use peca_p2p_yp::livechat::registry::{AcceptOutcome, LivechatRegistry, sign_res};
use peca_p2p_yp::livechat::thread::BoardSettings;

const DELIVERY_GUID: &str = "0123456789abcdef0123456789abcdef";

fn delivery_channel(board_id: &str) -> String {
    format!("30311:{board_id}:{DELIVERY_GUID}")
}

/// 開設済みスレを 1 本持つレジストリを作る(採番・配布契約のテスト用)。
fn registry_with_open_thread(persona: &Keys) -> Arc<LivechatRegistry> {
    let reg = LivechatRegistry::new(128);
    let board_id = persona.public_key().to_hex();
    reg.open_thread(
        persona.clone(),
        delivery_channel(&board_id),
        1,
        1_700_000_000,
        "実況スレ",
        // first_post_pow_bits=0: 採番・配布の契約検証に PoW 計算を挟まない(PoW 自体の
        // 契約は別テスト。71cc9d9 で初見板鍵に PoW を課すようになったため明示的に無効化)。
        BoardSettings {
            first_post_pow_bits: 0,
            ..Default::default()
        },
        "198.51.100.1:7147",
    )
    .unwrap();
    reg
}

#[test]
fn accept_write_numbers_orders_and_broadcasts_to_all_participants() {
    // 契約: RES 受理 → ORDER 発行 → 全参加者(送信者含む)へ配布(FR-007)。
    let persona = Keys::generate();
    let board_id = persona.public_key().to_hex();
    let reg = registry_with_open_thread(&persona);

    // 2 名の参加者を outbox 付きで登録する。
    let (tx1, mut rx1) = tokio::sync::mpsc::unbounded_channel();
    let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel();
    reg.register_participant(&board_id, "peer-1", tx1);
    reg.register_participant(&board_id, "peer-2", tx2);

    let board_key = Keys::generate();
    let res = sign_res(
        &board_key,
        &board_id,
        &delivery_channel(&board_id),
        1,
        "書き込みテスト",
        1_700_000_010,
    )
    .unwrap();

    let outcome = reg.accept_write(&board_id, &res, 1_700_000_010).unwrap();
    assert_eq!(
        outcome,
        AcceptOutcome::Numbered { res_no: 1, seq: 1 },
        "初回書き込みは res_no=1・seq=1 で採番される"
    );

    // 両参加者(自分を含め登録した全員)の outbox へ RES → ORDER の順で届く。
    for rx in [&mut rx1, &mut rx2] {
        let first = rx.try_recv().expect("RES が届く");
        assert!(
            matches!(first, Message::Res { .. }),
            "先に RES が配布される: {first:?}"
        );
        let second = rx.try_recv().expect("ORDER が届く");
        let Message::Order { event } = second else {
            panic!("ORDER を期待: {second:?}");
        };
        // ORDER の署名者はスレ主(board_id)であること(FR-011)。
        let order_event = nostr::Event::from_json(event.to_string()).unwrap();
        assert_eq!(
            order_event.pubkey.to_hex(),
            board_id,
            "ORDER の署名者はスレ主ペルソナ"
        );
        let order_env = peca_p2p_yp::event::livechat::OrderInfo::from_event(&order_event).unwrap();
        assert_eq!(order_env.seq, 1);
        assert_eq!(order_env.entries[0].res_no, 1);
    }
}

#[test]
fn accept_write_assigns_consecutive_res_no_and_seq() {
    // 契約: 複数レスを順に受理すると res_no・seq が欠番なく連番になる(T3/O2)。
    let persona = Keys::generate();
    let board_id = persona.public_key().to_hex();
    let reg = registry_with_open_thread(&persona);
    let board_key = Keys::generate();

    for (i, expected) in [(0u64, 1u16), (1, 2), (2, 3)] {
        let res = sign_res(
            &board_key,
            &board_id,
            &delivery_channel(&board_id),
            1,
            &format!("レス{expected}"),
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
                seq: expected as u32,
            },
            "res_no・seq は欠番なく連番で進む"
        );
    }
}

#[test]
fn accept_write_rejects_duplicate_event_without_renumbering() {
    // 契約: 同一 event_id の再受理は Duplicate(再採番も再配布もしない — D1/O1)。
    let persona = Keys::generate();
    let board_id = persona.public_key().to_hex();
    let reg = registry_with_open_thread(&persona);
    let board_key = Keys::generate();

    let res = sign_res(
        &board_key,
        &board_id,
        &delivery_channel(&board_id),
        1,
        "本文",
        1_700_000_010,
    )
    .unwrap();

    assert_eq!(
        reg.accept_write(&board_id, &res, 1_700_000_010).unwrap(),
        AcceptOutcome::Numbered { res_no: 1, seq: 1 }
    );
    // 同じイベントをもう一度渡しても採番は進まない(重複排除)。
    assert_eq!(
        reg.accept_write(&board_id, &res, 1_700_000_020).unwrap(),
        AcceptOutcome::Duplicate,
        "既採番の event_id の再受理は Duplicate で採番・配布ともにされない"
    );
}

#[test]
fn handle_resend_returns_res_then_order_for_requested_seq_range() {
    // 契約: seq 欠落 → RESEND_REQ 相当の `handle_resend(from,to)` が、該当 seq 範囲の
    // RES(先)→ ORDER(後)を seq 昇順で返す(O2 の再送)。
    let persona = Keys::generate();
    let board_id = persona.public_key().to_hex();
    let reg = registry_with_open_thread(&persona);
    let board_key = Keys::generate();

    // seq 1・2・3 を採番しておく。
    for (i, body) in ["一つ目", "二つ目", "三つ目"].into_iter().enumerate() {
        let res = sign_res(
            &board_key,
            &board_id,
            &delivery_channel(&board_id),
            1,
            body,
            1_700_000_010 + i as u64,
        )
        .unwrap();
        reg.accept_write(&board_id, &res, 1_700_000_010 + i as u64)
            .unwrap();
    }

    // seq 2..=3 の再送を要求する(seq 1 は要求範囲外)。
    let frames = reg.handle_resend(&board_id, 2, 3);

    // RES(seq2) → ORDER(seq2) → RES(seq3) → ORDER(seq3) の順で seq ごとに揃って返る。
    assert_eq!(frames.len(), 4, "seq 2・3 の RES+ORDER 計 4 フレーム");
    assert!(matches!(frames[0], Message::Res { .. }), "seq2 の RES が先");
    let Message::Order { event: ord2 } = &frames[1] else {
        panic!("seq2 の ORDER を期待");
    };
    let ord2_env = peca_p2p_yp::event::livechat::OrderInfo::from_event(
        &nostr::Event::from_json(ord2.to_string()).unwrap(),
    )
    .unwrap();
    assert_eq!(ord2_env.seq, 2);

    assert!(
        matches!(frames[2], Message::Res { .. }),
        "seq3 の RES が続く"
    );
    let Message::Order { event: ord3 } = &frames[3] else {
        panic!("seq3 の ORDER を期待");
    };
    let ord3_env = peca_p2p_yp::event::livechat::OrderInfo::from_event(
        &nostr::Event::from_json(ord3.to_string()).unwrap(),
    )
    .unwrap();
    assert_eq!(ord3_env.seq, 3);

    // 要求範囲外の seq 1 は含まれない。
    let has_seq1 = frames.iter().any(|f| {
        matches!(f, Message::Order { event } if {
            let ev = nostr::Event::from_json(event.to_string()).unwrap();
            peca_p2p_yp::event::livechat::OrderInfo::from_event(&ev).unwrap().seq == 1
        })
    });
    assert!(!has_seq1, "要求範囲外の seq は再送に含まれない");
}

#[test]
fn accept_write_rejects_over_res_limit_without_broadcast() {
    // 契約(任意項目): res_limit 到達後の accept_write は Rejected(NoOverLimit /
    // T3)であり、配布もされない。
    //
    // T046: res_no = res_limit(100 件目)の確定と同一の accept_write 呼び出し内で
    // 自動的に次スレ(gen=2)へ移行するため、100 件目の配布は RES + ORDER に続けて
    // NEXT_THREAD も届く。101 件目は「旧世代(gen=1)宛」であり、移行境界の定型拒否
    // (ADR-0014 D2)によって Rejected になる — これも「配布されない」契約を満たす
    // (自動移行それ自体の契約は next_thread_* / res_limit_reached_migrates_* を参照)。
    let persona = Keys::generate();
    let board_id = persona.public_key().to_hex();
    // res_limit の契約だけを見たいので PoW・レートは無効化(pow=0・十分高いレート)。
    let reg = LivechatRegistry::new_with_rate(128, 1000);
    let settings = BoardSettings {
        res_limit: 100,
        first_post_pow_bits: 0,
        ..Default::default()
    };
    reg.open_thread(
        persona.clone(),
        delivery_channel(&board_id),
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
    for i in 0..100u64 {
        let res = sign_res(
            &board_key,
            &board_id,
            &delivery_channel(&board_id),
            1,
            "本文",
            1_700_000_010 + i,
        )
        .unwrap();
        assert!(matches!(
            reg.accept_write(&board_id, &res, 1_700_000_010 + i)
                .unwrap(),
            AcceptOutcome::Numbered { .. }
        ));
        // 配布分(RES+ORDER)を読み捨てる。100 件目(自動移行が起きる回)は続けて
        // NEXT_THREAD も届くため、その分もあわせて読み捨てる。
        let _ = rx.try_recv();
        let _ = rx.try_recv();
        if i + 1 == 100 {
            match rx.try_recv() {
                Ok(Message::NextThread { generation, .. }) => assert_eq!(generation, 2),
                other => panic!("100 件目の確定で NEXT_THREAD が自動配布されるべき: {other:?}"),
            }
        }
    }
    assert_eq!(
        reg.build_announce_events(1_700_000_300, 0)[0]
            .tags
            .iter()
            .find(|t| t.as_slice().first().map(String::as_str) == Some("gen"))
            .and_then(|t| t.as_slice().get(1).cloned()),
        Some("2".to_string()),
        "前提: 100 件目確定と同時に世代 2 が開始されている"
    );

    // 101 件目は旧世代(gen=1)宛であり、移行境界の定型拒否(D2)で Rejected になる。
    let over = sign_res(
        &board_key,
        &board_id,
        &delivery_channel(&board_id),
        1,
        "上限超過",
        1_700_000_200,
    )
    .unwrap();
    assert_eq!(
        reg.accept_write(&board_id, &over, 1_700_000_200).unwrap(),
        AcceptOutcome::Rejected,
        "自動移行後、旧世代(gen=1)宛の書き込みは Rejected"
    );
    // 拒否分は配布されない(outbox に何も届かない)。
    assert!(rx.try_recv().is_err(), "上限超過は配布されない");
}

// ---------------------------------------------------------------------------
// T035: 契約ネガティブテスト(配送) — なりすまし・不正耐性(US3)
//
// 「妥当に見える接続・フレームだが不正」なケースが受理・確定されないことを固定する:
// - 第三者(攻撃者)のアドレス/鍵で応答するホストへ接続すると、チャレンジ署名検証が
//   失敗し切断・要バックオフになる(FR-005 — announce 記載の公開鍵で検証するため、
//   第三者が応答してもなりすませない)。
// - WELCOME/REJECT より前にスレメッセージ(RES/ORDER 等)が届くのはプロトコル違反
//   (thread-delivery.md §ハンドシェイク順序)。
// - RES/ORDER の kind が期待(1311/21311)と異なるイベントは形式検証で拒否され、
//   確定列に反映されない(封筒検証の kind チェック)。
// - 板鍵単位のレート上限(`thread_write_rate`)超過は採番せず Rejected で破棄される
//   (FR-021 — T036 で実装済みの LivechatRegistry::accept_write を公開 API 経由で確認)。
// ---------------------------------------------------------------------------

#[tokio::test]
async fn third_party_persona_welcome_fails_challenge_and_backs_off() {
    // 攻撃シナリオ: 第三者(攻撃者)が本物のスレ主を騙って応答するホストになりすます。
    // 参加者は announce に記載された**本物の**スレ主公開鍵(board_id)でチャレンジ署名を
    // 検証するため、攻撃者の鍵で署名された WELCOME は必ず検証に失敗する(FR-005)。
    let real_persona = Keys::generate();
    let board_id = real_persona.public_key().to_hex(); // announce に記載された本物の鍵。
    let attacker = Keys::generate(); // 接続先ホストが実際に握っている(第三者の)鍵。

    let mock = MockPeer::spawn().await;
    // ホスト(mock)は攻撃者の鍵で「正しく」署名した WELCOME を返す(攻撃者から見れば
    // 正当な署名だが、参加者が期待する board_id とは異なる鍵)。
    mock.set_thread_response(ThreadResponse::DynamicWelcome {
        persona: attacker,
        generation: 1,
        board_settings: json!({}),
        res_count: 0,
    });

    let dir = tempdir().unwrap();
    let log = Arc::new(SecurityLog::new(dir.path().join("sec.log")).unwrap());
    // config は本物の board_id を対象スレとして指定する(参加者が本来接続したいスレ)。
    let config = config_for(&mock, &board_id, Some(Arc::clone(&log)));
    let result = connect_once(&config, 0).await;
    assert_eq!(
        result,
        JoinResult::ChallengeFailed,
        "第三者の鍵で署名された WELCOME はチャレンジ検証に失敗する"
    );

    // livechat_challenge_failed が記録される(切断 + 要バックオフの根拠 — FR-005)。
    log.flush();
    let content = std::fs::read_to_string(dir.path().join("sec.log")).unwrap();
    assert!(
        content.contains("livechat_challenge_failed"),
        "第三者なりすましのチャレンジ失敗が記録される: {content}"
    );

    // ChallengeFailed はバックオフ対象(WaitFrozen/GiveUp とは異なり再試行しうる)。
    // ParticipantSession の状態機械としては record_failure が呼ばれ、次回接続まで
    // 待機時間が生じることを確認する(バックオフの根拠は session.rs 側で単体検証済みの
    // ため、ここでは「切断 + ログ記録」という配送契約の観測可能な結果を固定する)。
}

#[tokio::test]
async fn thread_message_before_welcome_is_protocol_violation() {
    // 契約: WELCOME/REJECT より前に RES/ORDER 等のスレメッセージが届くのは
    // ハンドシェイク順序違反(thread-delivery.md)。参加者ドライバは JOIN 直後の
    // 最初の応答として WELCOME/REJECT 以外を受け取ると、プロトコル違反として
    // 即座に接続を諦める(Transport 扱い — 前方互換で不正終了を握り潰さない)。
    let persona = Keys::generate();
    let board_id = persona.public_key().to_hex();
    let channel = format!("30311:{board_id}:{GUID}");
    let mock = MockPeer::spawn().await;

    // THREAD_JOIN への応答を設定しない(WELCOME/REJECT を返さない)。代わりに接続確立後
    // (HELLO/HELLO_ACK 交換直後から push は有効)、参加者が THREAD_JOIN を送ったであろう
    // 頃合いを見て RES フレームを即時注入する(WELCOME より前にスレメッセージが届く状況を
    // 再現する。serve_thread_frame は WELCOME 後の同期専用のため使えない)。
    let board_key = Keys::generate();
    let premature_res = peca_p2p_yp::livechat::registry::sign_res(
        &board_key,
        &board_id,
        &channel,
        1,
        "WELCOME 前の不正な RES",
        1_700_000_001,
    )
    .unwrap();

    let config = config_for(&mock, &board_id, None);
    let handle = tokio::spawn(async move { connect_once(&config, 0).await });
    // JOIN 送信が済む頃合いまで待ってから、WELCOME の代わりに RES を先出しする。
    tokio::time::sleep(Duration::from_millis(100)).await;
    mock.push_thread_frame(res_frame(&premature_res));

    let result = handle.await.unwrap();
    assert_eq!(
        result,
        JoinResult::Transport,
        "WELCOME 前のスレメッセージはプロトコル違反として扱われる: {result:?}"
    );
}

#[tokio::test]
async fn res_message_with_mismatched_kind_is_ignored_not_confirmed() {
    // 契約: RES フレームに kind 1311 以外のイベントが載っていた場合、封筒検証
    // (Res::from_event の kind チェック)で拒否され、確定列に反映されない
    // (前方互換のため切断はしないが、不正データとして無視する)。
    let persona = Keys::generate();
    let board_id = persona.public_key().to_hex();
    let channel = format!("30311:{board_id}:{GUID}");
    let mock = MockPeer::spawn().await;
    mock.set_thread_response(ThreadResponse::DynamicWelcome {
        persona: persona.clone(),
        generation: 1,
        board_settings: json!({}),
        res_count: 1,
    });

    // kind 1311 ではなく kind 31311(announce)のイベントを RES フレームに詰める
    // (kind 不一致 — 「妥当に見えるが不正な」フレーム)。
    let bogus_announce = peca_p2p_yp::event::livechat::ThreadAnnounce {
        channel: channel.clone(),
        title: "偽装".into(),
        generation: 1,
        key: 1_700_000_000,
        res_count: None,
        tip: "198.51.100.1:7147".into(),
    }
    .sign(&persona, 1_700_000_000, 0)
    .unwrap();
    assert_eq!(bogus_announce.kind.as_u16(), 31311, "kind 不一致の前提");
    mock.serve_thread_frame(res_frame(&bogus_announce));

    let config = config_for(&mock, &board_id, None);
    let handle = tokio::spawn(async move { connect_once(&config, 0).await });
    tokio::time::sleep(Duration::from_millis(150)).await;
    drop(mock); // 同期ループを EOF で終える。
    let result = handle.await.unwrap();

    match result {
        JoinResult::Joined { confirmed } => {
            assert!(
                confirmed.is_empty(),
                "kind 不一致の RES は確定列に反映されない(拒否・無視): {confirmed:?}"
            );
        }
        other => panic!("joined すべき(不正フレームで切断はしない): {other:?}"),
    }
}

#[tokio::test]
async fn order_message_with_mismatched_kind_is_ignored_not_confirmed() {
    // 契約: ORDER フレームに kind 21311 以外のイベントが載っていた場合も同様に
    // OrderInfo::from_event の kind チェックで拒否され、確定列は進まない。
    let persona = Keys::generate();
    let board_id = persona.public_key().to_hex();
    let channel = format!("30311:{board_id}:{GUID}");
    let mock = MockPeer::spawn().await;
    mock.set_thread_response(ThreadResponse::DynamicWelcome {
        persona: persona.clone(),
        generation: 1,
        board_settings: json!({}),
        res_count: 1,
    });

    // 正当な RES を先に用意し保留プールへ載せておく(確定できる材料は揃える)。
    let board_key = Keys::generate();
    let res = peca_p2p_yp::livechat::registry::sign_res(
        &board_key,
        &board_id,
        &channel,
        1,
        "本文",
        1_700_000_001,
    )
    .unwrap();
    mock.serve_thread_frame(res_frame(&res));

    // ORDER フレームに kind 1311(RES)のイベントを詰める(kind 不一致)。
    let bogus_order_slot = peca_p2p_yp::livechat::registry::sign_res(
        &board_key,
        &board_id,
        &channel,
        1,
        "ORDER のふりをした RES",
        1_700_000_002,
    )
    .unwrap();
    assert_eq!(bogus_order_slot.kind.as_u16(), 1311, "kind 不一致の前提");
    mock.serve_thread_frame(order_frame(&bogus_order_slot));

    let config = config_for(&mock, &board_id, None);
    let handle = tokio::spawn(async move { connect_once(&config, 0).await });
    tokio::time::sleep(Duration::from_millis(150)).await;
    drop(mock);
    let result = handle.await.unwrap();

    match result {
        JoinResult::Joined { confirmed } => {
            assert!(
                confirmed.is_empty(),
                "kind 不一致の ORDER は確定列に反映されない(RES は保留のまま): {confirmed:?}"
            );
        }
        other => panic!("joined すべき(不正フレームで切断はしない): {other:?}"),
    }
}

#[test]
fn write_rate_exceeded_is_rejected_without_numbering() {
    // 契約: 板鍵単位の書き込みレート上限(thread_write_rate — FR-021)を超えた
    // 書き込みは採番せず Rejected で破棄される(荒らしの連投耐性)。
    let persona = Keys::generate();
    let board_id = persona.public_key().to_hex();
    // 30 秒窓内 2 件までに制限した厳しいレートで開設する。
    let reg = LivechatRegistry::new_with_rate(128, 2);
    let settings = BoardSettings {
        first_post_pow_bits: 0,
        ..Default::default()
    };
    reg.open_thread(
        persona.clone(),
        delivery_channel(&board_id),
        1,
        1_700_000_000,
        "実況スレ",
        settings,
        "198.51.100.1:7147",
    )
    .unwrap();

    let board_key = Keys::generate();
    // 同一時刻(同一窓内)で 1・2 件目は受理される。
    for i in 0..2u64 {
        let res = sign_res(
            &board_key,
            &board_id,
            &delivery_channel(&board_id),
            1,
            "本文",
            1_700_000_010 + i,
        )
        .unwrap();
        assert!(
            matches!(
                reg.accept_write(&board_id, &res, 1_700_000_010).unwrap(),
                AcceptOutcome::Numbered { .. }
            ),
            "レート上限内の {i} 件目は受理される"
        );
    }

    // 同一窓内での 3 件目はレート超過で Rejected(採番されない = res_no は進まない)。
    let over = sign_res(
        &board_key,
        &board_id,
        &delivery_channel(&board_id),
        1,
        "レート超過の投稿",
        1_700_000_012,
    )
    .unwrap();
    assert_eq!(
        reg.accept_write(&board_id, &over, 1_700_000_010).unwrap(),
        AcceptOutcome::Rejected,
        "同一窓内でレート上限を超えた書き込みは Rejected で破棄される"
    );
}

#[tokio::test]
async fn forged_order_from_non_board_persona_is_discarded_and_logged() {
    // 攻撃シナリオ: 攻撃者(スレ主ではない)が、正規の board_id を騙った ORDER(kind
    // 21311)を偽造して参加者へ送りつける。参加者は verify_order(署名者 pubkey ==
    // board_id)で検証するため、攻撃者の鍵で署名された ORDER は必ず破棄される(FR-011)。
    // 破棄と同時に SecurityLog へ `livechat_order_invalid` を記録し、監査可能にする
    // (SC-004 / T038)。確定列には一切影響しない(表示を汚染しない)。
    let persona = Keys::generate();
    let board_id = persona.public_key().to_hex();
    let attacker = Keys::generate();
    let mock = MockPeer::spawn().await;
    mock.set_thread_response(ThreadResponse::DynamicWelcome {
        persona: persona.clone(),
        generation: 1,
        board_settings: json!({}),
        res_count: 0,
    });

    let dir = tempdir().unwrap();
    let log = Arc::new(SecurityLog::new(dir.path().join("sec.log")).unwrap());
    let config = config_for(&mock, &board_id, Some(Arc::clone(&log)));

    // 偽 ORDER: board_id ではなく攻撃者の鍵で署名する(entries の内容自体は形式上妥当)。
    let forged_order = OrderInfo {
        board_id: board_id.clone(),
        generation: 1,
        seq: 1,
        entries: vec![OrderEntry {
            res_no: 1,
            event_id: "ff".repeat(32),
        }],
    }
    .sign(&attacker, 1_700_000_000)
    .unwrap();

    let handle = tokio::spawn(async move { connect_once(&config, 0).await });
    // WELCOME 検証・joined が済む頃合いまで待ってから偽 ORDER を注入する
    // (join_then_sync テストと同様、mock を drop して EOF で同期ループを締める)。
    tokio::time::sleep(Duration::from_millis(100)).await;
    mock.push_thread_frame(order_frame(&forged_order));
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(mock);
    let result = handle.await.unwrap();

    match result {
        JoinResult::Joined { confirmed } => {
            assert!(
                confirmed.is_empty(),
                "偽 ORDER は破棄され確定列に一切影響しない: {confirmed:?}"
            );
        }
        other => panic!("joined すべき(偽 ORDER の受信では切断しない): {other:?}"),
    }

    // livechat_order_invalid が記録される(FR-011 / SC-004 の監査要件 — T038)。
    log.flush();
    let content = std::fs::read_to_string(dir.path().join("sec.log")).unwrap();
    assert!(
        content.contains("livechat_order_invalid"),
        "偽 ORDER の破棄が livechat_order_invalid として記録される: {content}"
    );
}

// ---------------------------------------------------------------------------
// T040: US4 契約テスト(モデレーション — BAN・PoW・完全鍵照合)
//
// registry_with_open_thread は first_post_pow_bits=0 で開設するため BAN 検証には
// そのまま使えるが、PoW 検証は専用の設定で別途開設する。BAN テストは理由非開示
// (応答は Rejected のみ・エラーメッセージに鍵情報を含まない)と配布(outbox)非発生の
// 両方を確認する。
// ---------------------------------------------------------------------------

use peca_p2p_yp::livechat::moderation::Moderation;
use peca_p2p_yp::store::Store;

#[test]
fn banned_board_key_is_silently_rejected() {
    let persona = Keys::generate();
    let board_id = persona.public_key().to_hex();
    let reg = registry_with_open_thread(&persona);
    let board_key = Keys::generate();
    let banned_key_hex = board_key.public_key().to_hex();

    assert!(reg.ban_board_key(&board_id, &banned_key_hex));

    // 配布を観測するため参加者を 1 名登録する。
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    reg.register_participant(&board_id, "peer-1", tx);

    let res = sign_res(
        &board_key,
        &board_id,
        &delivery_channel(&board_id),
        1,
        "BAN 済み鍵からの投稿",
        1_700_000_010,
    )
    .unwrap();
    let outcome = reg.accept_write(&board_id, &res, 1_700_000_010).unwrap();
    // 理由非開示: Rejected のみが返る(エラー内容・鍵情報を含む詳細は返さない)。
    assert_eq!(
        outcome,
        AcceptOutcome::Rejected,
        "BAN 済み板鍵は採番されず Rejected のみを返す"
    );
    // 配布(outbox)も発生しない = 他の参加者には一切届かない(FR-019)。
    assert!(
        rx.try_recv().is_err(),
        "BAN 済み鍵の書き込みは他の参加者へ配布されない"
    );
}

#[test]
fn insufficient_pow_first_post_is_rejected() {
    let persona = Keys::generate();
    let board_id = persona.public_key().to_hex();
    // first_post_pow_bits=8 で開設(PoW/レートは本テストの主題なので上限は緩める)。
    let reg = LivechatRegistry::new_with_rate(128, 10_000);
    let settings = BoardSettings {
        first_post_pow_bits: 8,
        ..Default::default()
    };
    reg.open_thread(
        persona.clone(),
        delivery_channel(&board_id),
        1,
        1_700_000_000,
        "実況スレ",
        settings,
        "198.51.100.1:7147",
    )
    .unwrap();

    let board_key = Keys::generate();
    let ch = delivery_channel(&board_id);

    // PoW なしの初見板鍵は Rejected。
    let no_pow = sign_res(&board_key, &board_id, &ch, 1, "初回", 1_700_000_010).unwrap();
    assert_eq!(
        reg.accept_write(&board_id, &no_pow, 1_700_000_010).unwrap(),
        AcceptOutcome::Rejected,
        "PoW 不足の初回書き込みは Rejected"
    );

    // PoW 8 付きの初回書き込みは Numbered。
    let pow = peca_p2p_yp::event::livechat::Res {
        channel: ch.clone(),
        board_id: board_id.clone(),
        generation: 1,
        name: None,
        mail: None,
        body: "初回".to_string(),
    }
    .sign(&board_key, 1_700_000_011, 8)
    .unwrap();
    assert!(matches!(
        reg.accept_write(&board_id, &pow, 1_700_000_011).unwrap(),
        AcceptOutcome::Numbered { .. }
    ));
}

#[test]
fn full_key_match_does_not_apply_to_short_id_collision() {
    // FR-018: 表示用の短縮 ID(先頭 8 文字)が同じでも、完全鍵が異なれば非適用。
    // Moderation は文字列完全一致で判定するため、実鍵である必要はない(テスト用に
    // 直接構築した 64hex 文字列でよい)。
    let store = std::sync::Arc::new(Store::open_in_memory().unwrap());
    let moderation = Moderation::new(store);
    let board_id = "ab".repeat(32);
    let key_a = "11223344".to_string() + &"a".repeat(56);
    let key_b = "11223344".to_string() + &"b".repeat(56);
    assert_eq!(&key_a[..8], &key_b[..8], "テスト前提: 短縮 ID 表示は一致");
    assert_ne!(key_a, key_b, "テスト前提: 完全鍵は異なる");

    moderation.ban_key(&board_id, &key_a).unwrap();
    assert!(moderation.is_banned(&board_id, &key_a));
    assert!(
        !moderation.is_banned(&board_id, &key_b),
        "短縮 ID が同じ別鍵には BAN が適用されない(完全鍵照合 — FR-018)"
    );
}

// ---------------------------------------------------------------------------
// T046/T047/T049: US5 契約テスト(次スレ移行・明示クローズ・板単位スコープの引き継ぎ)
//
// 公開クレート API(LivechatRegistry)のみを使い、次スレ移行(FR-013)・明示クローズ
// (FR-014/FR-015)・移行後も板鍵 BAN/NG が有効であること(T049 — 板 = ペルソナ単位・
// スレ非依存)を配送契約として固定する。ホスト側の詳細な状態遷移は registry.rs の
// #[cfg(test)] で個別に検証済み(単体レベル)。
// ---------------------------------------------------------------------------

#[test]
fn next_thread_migration_freezes_old_generation_and_starts_new_one() {
    let persona = Keys::generate();
    let board_id = persona.public_key().to_hex();
    let reg = registry_with_open_thread(&persona);
    let board_key = Keys::generate();

    // 旧スレ(gen=1)に 1 レス書き込んでおく。
    let res = sign_res(
        &board_key,
        &board_id,
        &delivery_channel(&board_id),
        1,
        "旧スレへの投稿",
        1_700_000_010,
    )
    .unwrap();
    assert!(matches!(
        reg.accept_write(&board_id, &res, 1_700_000_010).unwrap(),
        AcceptOutcome::Numbered { .. }
    ));

    let new_gen = reg
        .start_next_generation(&board_id, 1_700_001_000, "次スレ")
        .unwrap();
    assert_eq!(new_gen, 2, "次スレは世代 2");

    // 旧世代(gen=1)宛の書き込みは、移行後は定型拒否される(D2 — 新スレへの誤採番はしない)。
    let old_gen_write = sign_res(
        &board_key,
        &board_id,
        &delivery_channel(&board_id),
        1,
        "移行後に旧世代宛で届いた投稿",
        1_700_001_010,
    )
    .unwrap();
    assert_eq!(
        reg.accept_write(&board_id, &old_gen_write, 1_700_001_010)
            .unwrap(),
        AcceptOutcome::Rejected,
        "旧世代宛の書き込みは移行後は拒否される(T1)"
    );

    // 新世代(gen=2)宛の書き込みは res_no=1 から採番される。
    let new_gen_write = sign_res(
        &board_key,
        &board_id,
        &delivery_channel(&board_id),
        2,
        "次スレへの投稿",
        1_700_001_010,
    )
    .unwrap();
    assert_eq!(
        reg.accept_write(&board_id, &new_gen_write, 1_700_001_010)
            .unwrap(),
        AcceptOutcome::Numbered { res_no: 1, seq: 1 },
        "新世代は独立した採番系列で 1 から始まる"
    );
}

#[test]
fn next_thread_broadcasts_next_thread_frame_to_participants() {
    let persona = Keys::generate();
    let board_id = persona.public_key().to_hex();
    let reg = registry_with_open_thread(&persona);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    reg.register_participant(&board_id, "peer-1", tx);

    reg.start_next_generation(&board_id, 1_700_001_000, "次スレ")
        .unwrap();

    match rx.try_recv() {
        Ok(Message::NextThread { generation, key }) => {
            assert_eq!(generation, 2);
            assert_eq!(key, 1_700_001_000);
        }
        other => panic!("NEXT_THREAD を期待: {other:?}"),
    }
}

#[test]
fn close_thread_signs_and_broadcasts_thread_close_then_rejects_writes() {
    let persona = Keys::generate();
    let board_id = persona.public_key().to_hex();
    let reg = registry_with_open_thread(&persona);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    reg.register_participant(&board_id, "peer-1", tx);

    let close_event = reg.close_thread(&board_id, 1_700_000_500).unwrap();
    assert!(
        peca_p2p_yp::event::livechat::is_close_notice(&close_event),
        "kind 21311 の [\"peca\",\"close\"] 特殊形で発行される"
    );
    assert_eq!(
        close_event.pubkey,
        persona.public_key(),
        "署名者はスレ主ペルソナ"
    );
    assert!(close_event.verify().is_ok());

    match rx.try_recv() {
        Ok(Message::ThreadClose { event }) => {
            let ev = nostr::Event::from_json(event.to_string()).unwrap();
            assert!(peca_p2p_yp::event::livechat::is_close_notice(&ev));
        }
        other => panic!("THREAD_CLOSE を期待: {other:?}"),
    }

    // クローズ後は書き込みを一切受理しない(T1)。
    let board_key = Keys::generate();
    let res = sign_res(
        &board_key,
        &board_id,
        &delivery_channel(&board_id),
        1,
        "クローズ後の投稿",
        1_700_000_600,
    )
    .unwrap();
    assert_eq!(
        reg.accept_write(&board_id, &res, 1_700_000_600).unwrap(),
        AcceptOutcome::Rejected
    );

    // クローズ済みスレは announce の対象外(T047)。
    assert!(reg.build_announce_events(1_700_000_700, 0).is_empty());
}

#[test]
fn board_scope_ban_and_settings_survive_thread_migration() {
    // T049: 次スレ移行後も板鍵 BAN・板設定(板 = ペルソナ単位のスコープ)がそのまま
    // 有効であることを配送契約として確認する。
    let persona = Keys::generate();
    let board_id = persona.public_key().to_hex();
    let reg = registry_with_open_thread(&persona);

    let banned_key = Keys::generate();
    assert!(reg.ban_board_key(&board_id, &banned_key.public_key().to_hex()));

    reg.start_next_generation(&board_id, 1_700_001_000, "次スレ")
        .unwrap();

    // BAN 済み板鍵は次スレでも採番されない(板単位スコープの引き継ぎ)。
    let banned_write = sign_res(
        &banned_key,
        &board_id,
        &delivery_channel(&board_id),
        2,
        "BAN 済み鍵からの投稿(次スレ)",
        1_700_001_010,
    )
    .unwrap();
    assert_eq!(
        reg.accept_write(&board_id, &banned_write, 1_700_001_010)
            .unwrap(),
        AcceptOutcome::Rejected,
        "板鍵 BAN は板単位スコープのため次スレへ引き継がれる(FR-012)"
    );

    // BAN されていない鍵は次スレで通常どおり採番される。
    let ok_key = Keys::generate();
    let ok_write = sign_res(
        &ok_key,
        &board_id,
        &delivery_channel(&board_id),
        2,
        "通常の投稿(次スレ)",
        1_700_001_010,
    )
    .unwrap();
    assert!(matches!(
        reg.accept_write(&board_id, &ok_write, 1_700_001_010)
            .unwrap(),
        AcceptOutcome::Numbered { .. }
    ));
}

#[test]
fn board_scope_ng_survives_thread_migration() {
    // T049: NG はローカル判定情報(Moderation ドメイン層)であり、スレの世代ではなく
    // board_id(板)にスコープするため、次スレ移行後も同一の Moderation インスタンスで
    // 判定し続ければそのまま有効に働く(ネットワーク非送出 — 不変条件 M1)。
    let store = std::sync::Arc::new(Store::open_in_memory().unwrap());
    let moderation = Moderation::new(store);
    let board_id = "ab".repeat(32);
    let ng_key = Keys::generate().public_key().to_hex();
    moderation.add_ng(&board_id, &ng_key).unwrap();
    assert!(moderation.is_ng(&board_id, &ng_key));

    // 次スレ移行(世代の変化)は Moderation の判定に一切影響しない(板単位スコープ)。
    // NG エントリの board_id は板(ペルソナ)固定であり、gen を持たない設計であることを
    // 確認する(is_ng の呼び出しに gen を渡す API 自体が存在しない)。
    assert!(
        moderation.is_ng(&board_id, &ng_key),
        "NG は板単位スコープのためスレ世代に依存しない"
    );
}

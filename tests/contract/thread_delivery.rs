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
        BoardSettings::default(),
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
    let persona = Keys::generate();
    let board_id = persona.public_key().to_hex();
    let reg = LivechatRegistry::new(128);
    let settings = BoardSettings {
        res_limit: 100,
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
        // 配布分(RES+ORDER)を読み捨てる。
        let _ = rx.try_recv();
        let _ = rx.try_recv();
    }

    // 101 件目は res_limit(100)超過で Rejected。
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
        "res_limit 到達後は Rejected"
    );
    // 上限超過分は配布されない(outbox に何も届かない)。
    assert!(rx.try_recv().is_err(), "上限超過は配布されない");
}

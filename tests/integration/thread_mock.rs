//! モックピアの THREAD_* 対応(T015)のスモークテスト。
//!
//! contracts/thread-delivery.md のホスト役として、参加者(生 `TcpStream`)からの
//! HELLO → THREAD_JOIN に対し WELCOME/REJECT を返し、RES/ORDER を配布でき、受信した
//! スレメッセージを記録できることを検証する。固定フィクスチャ・実ノードによる本格的な
//! 契約検証は後続タスク(T018 `tests/contract/thread_delivery.rs`)で行う。

#[path = "../common/mock_peer.rs"]
mod mock_peer;

use std::time::Duration;

use serde_json::json;
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

use mock_peer::{MockPeer, ThreadResponse, unix_now};
use peca_p2p_yp::p2p::frame::{Hello, Message, read_frame, thread_reject_reason, write_frame};

const PROTOCOL_VERSION: u32 = 1;

/// モックへ接続し HELLO/HELLO_ACK を済ませて分割ソケットを返す(参加者役)。
async fn connect_and_hello(addr: &str) -> (OwnedReadHalf, OwnedWriteHalf) {
    let stream = TcpStream::connect(addr).await.expect("connect");
    let (mut reader, mut writer) = stream.into_split();
    let hello = Message::Hello(Hello {
        version: PROTOCOL_VERSION,
        listen_port: 0,
        features: vec!["livechat1".into()],
        nonce: 0x0000_ABCD_0000_ABCD,
        ts: unix_now() as i64,
    });
    write_frame(&mut writer, &hello).await.expect("send HELLO");
    let ack = read_frame(&mut reader)
        .await
        .expect("read ack")
        .expect("ack frame");
    assert!(
        matches!(ack.message, Message::HelloAck(_)),
        "最初の応答は HELLO_ACK"
    );
    (reader, writer)
}

fn join(thread: &str) -> Message {
    Message::ThreadJoin {
        thread: thread.into(),
        challenge: "00ff".into(),
        since_seq: 0,
    }
}

#[tokio::test]
async fn thread_join_welcome_and_initial_sync() {
    let mock = MockPeer::spawn().await;
    mock.set_thread_response(ThreadResponse::Welcome {
        thread: "board:1".into(),
        sig: "deadbeef".into(),
        board_settings: json!({ "title": "実況板" }),
        res_count: 2,
    });
    // 接続時同期で配布する RES/ORDER を仕込む(seq 順)。
    mock.serve_thread_frame(Message::Res {
        event: json!({ "kind": 1311 }),
    });
    mock.serve_thread_frame(Message::Order {
        event: json!({ "kind": 21311 }),
    });

    let (mut reader, mut writer) = connect_and_hello(mock.addr()).await;
    write_frame(&mut writer, &join("board:1")).await.unwrap();

    // WELCOME → RES → ORDER の順で受信できる。
    let welcome = read_frame(&mut reader).await.unwrap().unwrap();
    match welcome.message {
        Message::ThreadWelcome {
            thread, res_count, ..
        } => {
            assert_eq!(thread, "board:1");
            assert_eq!(res_count, 2);
        }
        other => panic!("expected WELCOME, got {other:?}"),
    }
    assert!(matches!(
        read_frame(&mut reader).await.unwrap().unwrap().message,
        Message::Res { .. }
    ));
    assert!(matches!(
        read_frame(&mut reader).await.unwrap().unwrap().message,
        Message::Order { .. }
    ));

    // モックは受信した THREAD_JOIN を記録している。
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        mock.received_thread_messages()
            .iter()
            .any(|m| matches!(m, Message::ThreadJoin { .. })),
        "THREAD_JOIN が記録される"
    );
}

#[tokio::test]
async fn thread_join_reject() {
    let mock = MockPeer::spawn().await;
    mock.set_thread_response(ThreadResponse::Reject {
        reason: thread_reject_reason::FULL.into(),
    });
    let (mut reader, mut writer) = connect_and_hello(mock.addr()).await;
    write_frame(&mut writer, &join("board:1")).await.unwrap();

    let reject = read_frame(&mut reader).await.unwrap().unwrap();
    match reject.message {
        Message::ThreadReject { reason } => assert_eq!(reason, thread_reject_reason::FULL),
        other => panic!("expected REJECT, got {other:?}"),
    }
}

#[tokio::test]
async fn push_thread_frame_reaches_participant() {
    // 偽 ORDER・不正フレーム注入経路(push_thread_frame)が接続中の参加者へ届く。
    let mock = MockPeer::spawn().await;
    mock.set_thread_response(ThreadResponse::Welcome {
        thread: "board:1".into(),
        sig: "sig".into(),
        board_settings: json!({}),
        res_count: 0,
    });
    let (mut reader, mut writer) = connect_and_hello(mock.addr()).await;
    write_frame(&mut writer, &join("board:1")).await.unwrap();
    // WELCOME を消費(以降は接続確立済み = push の購読者が存在する)。
    assert!(matches!(
        read_frame(&mut reader).await.unwrap().unwrap().message,
        Message::ThreadWelcome { .. }
    ));

    mock.push_thread_frame(Message::Order {
        event: json!({ "kind": 21311, "fake": true }),
    });
    assert!(matches!(
        read_frame(&mut reader).await.unwrap().unwrap().message,
        Message::Order { .. }
    ));
}

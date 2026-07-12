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

use nostr::Keys;
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

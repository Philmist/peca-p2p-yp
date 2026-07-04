//! T017 gossip フレーミング契約テスト(contracts/p2p-gossip.md §検証方法)。
//!
//! テストベクタ `fixtures/gossip_vectors.json` は本実装とモックピア(T033)で共有し、
//! 契約書とモック実装の乖離を検出する。検査対象:
//! - フレーム境界(分割受信・結合受信・過大長 > 64KB → p2p_oversize)
//! - メッセージ種別の往復(valid_messages が構造的に等価にデコードされる)
//! - 前方互換(未知フィールド・未知 feature を無視して正しい種別へデコード)
//! - 不正 JSON・未知 type(検査 3 → p2p_invalid_frame)
//! - HELLO 順序違反(established 前の他メッセージ → 即切断 p2p_invalid_frame)

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use serde_json::Value;
use tokio::io::{AsyncRead, ReadBuf};

use peca_p2p_yp::p2p::frame::{self, FrameError, MAX_FRAME_PAYLOAD, Message};
use peca_p2p_yp::p2p::session::{Session, SessionAction, SessionConfig, SessionState};
use peca_p2p_yp::security::SecurityCategory;

// ---------------------------------------------------------------------------
// テスト補助
// ---------------------------------------------------------------------------

/// フィクスチャ全体を読み込む。
fn load_fixtures() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/contract/fixtures/gossip_vectors.json"
    );
    let text = std::fs::read_to_string(path).expect("フィクスチャを読めること");
    serde_json::from_str(&text).expect("フィクスチャが有効な JSON であること")
}

/// 生ペイロードを 4 バイト BE 長さ前置フレームに包む。
fn frame_payload(payload: &[u8]) -> Vec<u8> {
    let mut out = (payload.len() as u32).to_be_bytes().to_vec();
    out.extend_from_slice(payload);
    out
}

/// JSON 値をメッセージフレームのバイト列にする。
fn frame_json(value: &Value) -> Vec<u8> {
    frame_payload(&serde_json::to_vec(value).unwrap())
}

/// `poll_read` あたり最大 `chunk` バイトだけ返す AsyncRead モック。
/// chunk=1 で分割到着、大きな chunk で結合到着を再現する。
struct ChunkedReader {
    data: Vec<u8>,
    pos: usize,
    chunk: usize,
}

impl ChunkedReader {
    fn new(data: Vec<u8>, chunk: usize) -> Self {
        Self {
            data,
            pos: 0,
            chunk: chunk.max(1),
        }
    }
}

impl AsyncRead for ChunkedReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let remaining = &self.data[self.pos..];
        let n = remaining.len().min(self.chunk).min(buf.remaining());
        buf.put_slice(&remaining[..n]);
        self.pos += n;
        Poll::Ready(Ok(()))
    }
}

fn test_config(nonce: u64) -> SessionConfig {
    SessionConfig {
        local_nonce: nonce,
        local_listen_port: 7147,
        ..SessionConfig::default()
    }
}

// ---------------------------------------------------------------------------
// メッセージ種別の往復
// ---------------------------------------------------------------------------

#[tokio::test]
async fn valid_messages_round_trip() {
    let fx = load_fixtures();
    for case in fx["valid_messages"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let original = &case["message"];
        let bytes = frame_json(original);
        let mut reader = ChunkedReader::new(bytes, usize::MAX);
        let incoming = frame::read_frame(&mut reader)
            .await
            .unwrap_or_else(|e| panic!("{name}: デコード失敗 {e:?}"))
            .unwrap_or_else(|| panic!("{name}: 予期しない EOF"));
        let re = serde_json::to_value(&incoming.message).unwrap();
        assert_eq!(&re, original, "{name}: 往復で構造が変わってはならない");
    }
}

#[tokio::test]
async fn forward_compat_unknown_fields_and_features_ignored() {
    let fx = load_fixtures();
    for case in fx["forward_compat"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let expected_type = case["decodes_as"].as_str().unwrap();
        let bytes = frame_json(&case["message"]);
        let mut reader = ChunkedReader::new(bytes, usize::MAX);
        let incoming = frame::read_frame(&mut reader)
            .await
            .unwrap_or_else(|e| panic!("{name}: 前方互換デコード失敗 {e:?}"))
            .unwrap();
        let re = serde_json::to_value(&incoming.message).unwrap();
        assert_eq!(
            re["type"], expected_type,
            "{name}: 未知フィールドがあっても種別は保たれる"
        );
        // 未知フィールドは再シリアライズで消える(既定で無視)
        assert!(
            re.get("future_field").is_none(),
            "{name}: 未知フィールドは無視される"
        );
    }
}

#[tokio::test]
async fn invalid_frames_rejected_as_invalid_frame() {
    let fx = load_fixtures();
    for case in fx["invalid_frames"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let bytes = if let Some(raw) = case.get("raw_utf8").and_then(|v| v.as_str()) {
            frame_payload(raw.as_bytes())
        } else {
            frame_json(&case["message"])
        };
        let mut reader = ChunkedReader::new(bytes, usize::MAX);
        let result = frame::read_frame(&mut reader).await;
        match result {
            Err(FrameError::InvalidFrame) => {}
            other => panic!("{name}: InvalidFrame を期待したが {other:?}"),
        }
        // 検査 3 のログ対応
        assert_eq!(
            FrameError::InvalidFrame.security(),
            Some((SecurityCategory::P2pInvalidFrame, "invalid_frame"))
        );
    }
}

// ---------------------------------------------------------------------------
// フレーム境界
// ---------------------------------------------------------------------------

#[tokio::test]
async fn frame_split_arrival_reassembles() {
    let fx = load_fixtures();
    let msg = &fx["valid_messages"][0]["message"];
    let bytes = frame_json(msg);
    // 1 バイトずつ到着してもフレームを再構成できる
    let mut reader = ChunkedReader::new(bytes, 1);
    let incoming = frame::read_frame(&mut reader).await.unwrap().unwrap();
    let re = serde_json::to_value(&incoming.message).unwrap();
    assert_eq!(&re, msg);
}

#[tokio::test]
async fn frame_combined_arrival_reads_sequentially() {
    let fx = load_fixtures();
    let m0 = &fx["valid_messages"][0]["message"];
    let m1 = &fx["valid_messages"][5]["message"]; // sync_done
    let mut bytes = frame_json(m0);
    bytes.extend_from_slice(&frame_json(m1));
    // 2 フレームが 1 度に到着しても順に読める
    let mut reader = ChunkedReader::new(bytes, usize::MAX);
    let first = frame::read_frame(&mut reader).await.unwrap().unwrap();
    let second = frame::read_frame(&mut reader).await.unwrap().unwrap();
    assert_eq!(serde_json::to_value(&first.message).unwrap(), *m0);
    assert_eq!(serde_json::to_value(&second.message).unwrap(), *m1);
    // 末尾は正常な EOF
    assert!(frame::read_frame(&mut reader).await.unwrap().is_none());
}

#[tokio::test]
async fn frame_oversize_rejected_before_payload() {
    // 長さ前置が上限超過(> 64KB)。ペイロードを読む前に拒否する。
    let over = (MAX_FRAME_PAYLOAD as u32 + 1).to_be_bytes().to_vec();
    let mut reader = ChunkedReader::new(over, usize::MAX);
    match frame::read_frame(&mut reader).await {
        Err(FrameError::Oversize) => {}
        other => panic!("Oversize を期待したが {other:?}"),
    }
    assert_eq!(
        FrameError::Oversize.security(),
        Some((SecurityCategory::P2pOversize, "oversize"))
    );
}

#[tokio::test]
async fn frame_at_limit_is_accepted() {
    // ちょうど上限のペイロードは受理する(境界は payload バイト数)。
    let payload = vec![b' '; MAX_FRAME_PAYLOAD];
    // 空白のみは JSON として不正なので、JSON パースは失敗するがフレーム境界検査は通過する。
    let mut reader = ChunkedReader::new(frame_payload(&payload), usize::MAX);
    let result = frame::read_frame(&mut reader).await;
    assert!(
        matches!(result, Err(FrameError::InvalidFrame)),
        "上限ちょうどは Oversize にならず、JSON 不正で弾かれる: {result:?}"
    );
}

// ---------------------------------------------------------------------------
// セッション状態機械
// ---------------------------------------------------------------------------

#[test]
fn inbound_hello_ordering_violation_disconnects() {
    // established 前に HELLO 以外(PING)を受信 → 即切断 p2p_invalid_frame
    let mut session = Session::new_inbound(test_config(1), "198.51.100.2:7147".into(), None);
    let ping = Message::Ping { nonce: 5 };
    let err = session
        .on_frame(16, ping)
        .expect_err("HELLO 前の PING は切断されるべき");
    assert_eq!(err.category, Some(SecurityCategory::P2pInvalidFrame));
    assert_eq!(session.state(), SessionState::Closed);
}

#[test]
fn outbound_wrong_handshake_message_disconnects() {
    // outbound は HELLO_ACK を待つ。HELLO を受けたら順序違反として切断。
    let mut session = Session::new_outbound(test_config(1), "198.51.100.3:7147".into(), None);
    let _hello = session.start().expect("outbound は HELLO を送る");
    let wrong = Message::Hello(frame::Hello {
        version: 1,
        listen_port: 7147,
        features: vec![],
        nonce: 2,
        ts: 1720000000,
    });
    let err = session
        .on_frame(64, wrong)
        .expect_err("HELLO_ACK 以外は切断");
    assert_eq!(err.category, Some(SecurityCategory::P2pInvalidFrame));
}

#[test]
fn inbound_handshake_completes_and_acks() {
    let mut session = Session::new_inbound(test_config(1), "198.51.100.4:7147".into(), None);
    assert!(session.start().is_none(), "inbound は HELLO を待つ");
    let hello = Message::Hello(frame::Hello {
        version: 1,
        listen_port: 7200,
        features: vec!["future".into()],
        nonce: 42,
        ts: 1720000000,
    });
    let actions = session.on_frame(64, hello).expect("正常 HELLO");
    assert_eq!(session.state(), SessionState::Established);
    // HELLO_ACK 送信と established 通知が返る
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, SessionAction::Send(Message::HelloAck(_))))
    );
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, SessionAction::Established))
    );
    // 相手の申告値が保持される(ts は未検証・診断用)
    let peer = session.peer().expect("HELLO 交換後は peer 情報を持つ");
    assert_eq!(peer.nonce, 42);
    assert_eq!(peer.listen_port, 7200);
    assert_eq!(peer.ts, 1720000000);
}

#[test]
fn version_mismatch_closes_incompatible() {
    let mut session = Session::new_inbound(test_config(1), "198.51.100.5:7147".into(), None);
    let hello = Message::Hello(frame::Hello {
        version: 2,
        listen_port: 7147,
        features: vec![],
        nonce: 7,
        ts: 1720000000,
    });
    let err = session
        .on_frame(64, hello)
        .expect_err("非互換バージョンは切断");
    assert_eq!(err.reason, "incompatible");
    // 非互換は攻撃ではないためセキュリティカテゴリなし
    assert_eq!(err.category, None);
}

#[test]
fn self_connection_detected_by_nonce() {
    // 自ノードの nonce と一致 → 自己接続として切断
    let mut session = Session::new_inbound(test_config(0xABCD), "198.51.100.6:7147".into(), None);
    let hello = Message::Hello(frame::Hello {
        version: 1,
        listen_port: 7147,
        features: vec![],
        nonce: 0xABCD,
        ts: 1720000000,
    });
    let err = session.on_frame(64, hello).expect_err("自己接続は切断");
    assert_eq!(err.reason, "self_connect");
}

#[test]
fn established_delivers_data_messages() {
    let mut session = Session::new_inbound(test_config(1), "198.51.100.7:7147".into(), None);
    let hello = Message::Hello(frame::Hello {
        version: 1,
        listen_port: 7147,
        features: vec![],
        nonce: 9,
        ts: 1720000000,
    });
    session.on_frame(64, hello).expect("HELLO");
    // established 後は EVENT/SYNC 等を上位へ委譲(内容検証はしない)
    let ev = Message::SyncReq { since: 100 };
    let actions = session.on_frame(32, ev).expect("established 後の受信");
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, SessionAction::Deliver(Message::SyncReq { since: 100 })))
    );
}

#[test]
fn rate_limit_disconnects_on_message_flood() {
    // クロックを固定し、同一秒窓で 200 msg 超 → p2p_rate_limited
    let clock = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let c2 = std::sync::Arc::clone(&clock);
    let mut session = Session::new_inbound(test_config(1), "198.51.100.8:7147".into(), None)
        .with_clock(Box::new(move || {
            c2.load(std::sync::atomic::Ordering::SeqCst) as f64
        }));
    let hello = Message::Hello(frame::Hello {
        version: 1,
        listen_port: 7147,
        features: vec![],
        nonce: 3,
        ts: 1720000000,
    });
    session.on_frame(64, hello).expect("HELLO");
    let mut hit = None;
    for i in 0..500 {
        match session.on_frame(16, Message::Ping { nonce: i }) {
            Ok(_) => {}
            Err(d) => {
                hit = Some(d);
                break;
            }
        }
    }
    let d = hit.expect("メッセージ数上限で切断されるべき");
    assert_eq!(d.category, Some(SecurityCategory::P2pRateLimited));
}

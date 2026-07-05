//! T054 外向きのみ参加の統合テスト(FR-016 / SC-009)
//!
//! P2P 待受を無効化(`p2p_bind` 空 = listener None)したノードが**外向き接続のみ**で、
//! 掲載(US1)・発見(US2)・PEX の全機能を成立させ、状態表示が「外向き接続のみで
//! 参加中」(= 着信不可)となることを検証する。共有ハーネス
//! [`mock_peer::TestNode`](外向きのみ)・[`mock_peer::MockPeer`] を用いる。

use std::time::{Duration, Instant};

use nostr::{Event, Keys};

use peca_p2p_yp::event::schema::{ChannelListing, ChannelStatus, Track};

#[path = "../common/mock_peer.rs"]
mod mock_peer;

use mock_peer::{MockPeer, TestNode, unix_now};

// ---------------------------------------------------------------------------
// 補助
// ---------------------------------------------------------------------------

const CH_ANNOUNCE: &str = "0123456789abcdef0123456789abcda1";
const CH_DISCOVER: &str = "0123456789abcdef0123456789abcdb2";
// PEX で共有する第三者ピア(TEST-NET-3 — 実接続はしない)。
const PEX_ADDR: &str = "203.0.113.50:7147";
const TIP: &str = "198.51.100.1:7144";

fn listing(channel_id: &str, title: &str) -> ChannelListing {
    ChannelListing {
        channel_id: channel_id.into(),
        title: title.into(),
        summary: Some("説明".into()),
        genre: Some("game".into()),
        status: ChannelStatus::Live,
        starts: unix_now(),
        current_participants: 3,
        streaming: Some(format!("pcp://{TIP}/{channel_id}")),
        bitrate_kbps: Some(1500),
        content_type: Some("FLV".into()),
        tip: Some(TIP.into()),
        contact: Some("https://example.com/".into()),
        relays: 1,
        track: Some(Track::default()),
    }
}

fn signed(keys: &Keys, channel_id: &str, title: &str) -> Event {
    listing(channel_id, title)
        .sign(keys, unix_now(), 0)
        .unwrap()
}

/// 外向き established を得るまで待つ。
async fn wait_established(node: &TestNode, timeout: Duration) -> bool {
    let start = Instant::now();
    loop {
        if node.established_counts().1 > 0 {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// モックの受信 EVENT に channel_id を含むものが現れるまで待つ。
async fn wait_mock_received(mock: &MockPeer, channel_id: &str, timeout: Duration) -> bool {
    let start = Instant::now();
    loop {
        if mock
            .received()
            .iter()
            .any(|v| v.to_string().contains(channel_id))
        {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// SC-009: 外向き接続のみで掲載(US1)が成立する
// ---------------------------------------------------------------------------

#[tokio::test]
async fn outbound_only_announce_propagates_to_peer() {
    let keys = Keys::generate();
    let mock = MockPeer::spawn().await;
    let node = TestNode::spawn(1).await;
    node.add_manual_peer(mock.addr());

    assert!(
        wait_established(&node, Duration::from_secs(5)).await,
        "外向き接続のみで established に達するべき"
    );

    // ローカル発行 → established ピア(モック)へ伝搬する(掲載機能)。
    node.hub()
        .publish_local(signed(&keys, CH_ANNOUNCE, "配信A"));
    assert!(
        wait_mock_received(&mock, CH_ANNOUNCE, Duration::from_secs(5)).await,
        "外向き接続のみでも掲載イベントが伝搬先ピアへ届くべき(US1 / SC-009)"
    );
}

// ---------------------------------------------------------------------------
// SC-009: 外向き接続のみで発見(US2)が成立する
// ---------------------------------------------------------------------------

#[tokio::test]
async fn outbound_only_discovers_via_sync() {
    let keys = Keys::generate();
    let mock = MockPeer::spawn().await;
    mock.serve_signed(&signed(&keys, CH_DISCOVER, "発見A"));

    let node = TestNode::spawn(1).await;
    node.add_manual_peer(mock.addr());

    assert!(
        node.wait_for_channel(CH_DISCOVER, Duration::from_secs(5))
            .await,
        "外向き接続のみでも SYNC で発見したチャンネルが一覧へ現れるべき(US2 / SC-009)"
    );
}

// ---------------------------------------------------------------------------
// SC-009: 外向き接続のみで PEX が成立する
// ---------------------------------------------------------------------------

#[tokio::test]
async fn outbound_only_pex_expands_candidates() {
    let mock = MockPeer::spawn().await;
    // モックが GET_PEERS へ第三者ピアを返す。
    mock.share_peer(PEX_ADDR);

    let node = TestNode::spawn(1).await;
    node.add_manual_peer(mock.addr());

    // established 後に GET_PEERS を送り、返ってきた PEERS を候補登録する。
    assert!(
        node.wait_for_peer(PEX_ADDR, Duration::from_secs(5)).await,
        "PEX で受信した候補が既知ピアへ登録されるべき(FR-015 / SC-009)"
    );
    let registered = node
        .known_peers()
        .into_iter()
        .find(|p| p.addr == PEX_ADDR)
        .expect("PEX 候補が登録済み");
    // 受信候補は未検証(verified=1 は自ノードの外向き接続成功時のみ)。
    assert!(!registered.verified, "PEX 受信候補は未検証で登録される");
}

// ---------------------------------------------------------------------------
// SC-009: 状態表示が「外向き接続のみで参加中」(= 着信不可)となる
// ---------------------------------------------------------------------------

#[tokio::test]
async fn outbound_only_status_shows_outbound_only() {
    let node = TestNode::spawn(1).await;
    // 待受なし(p2p_bind 空 = listener None)のため着信不可。
    assert!(
        !node.inbound_reachable(),
        "外向きのみノードは着信不可(状態表示は「外向き接続のみで参加中」)"
    );
}

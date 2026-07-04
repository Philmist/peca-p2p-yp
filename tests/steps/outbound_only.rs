//! 着信不可ノードの参加(FR-016 / SC-009)のステップ定義(T051 骨格 → T054 で実装)
//!
//! UPnP 失敗下(着信不可)でも外向き接続のみで掲載(US1)・発見(US2)が成立し、
//! 状態表示が「外向き接続のみで参加中」となることを検証する。共有ハーネス
//! [`crate::mock_peer`] の `TestNode`(外向きのみ)・`MockPeer` を用いる。

use std::time::{Duration, Instant};

use cucumber::{given, then, when};
use nostr::{Event, Keys};

use peca_p2p_yp::event::schema::{ChannelListing, ChannelStatus, Track};

use crate::AppWorld;
use crate::mock_peer::{MockPeer, TestNode, unix_now};

const CH_DISCOVER: &str = "0123456789abcdef0123456789abcdc3";
const CH_ANNOUNCE: &str = "0123456789abcdef0123456789abcdd4";
const TIP: &str = "198.51.100.1:7144";

/// outbound_only シナリオの状態(各シナリオで新規に生成される)。
pub struct OutboundWorld {
    mock: Option<MockPeer>,
    node: Option<TestNode>,
    keys: Keys,
}

impl Default for OutboundWorld {
    fn default() -> Self {
        Self {
            mock: None,
            node: None,
            keys: Keys::generate(),
        }
    }
}

impl std::fmt::Debug for OutboundWorld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutboundWorld")
            .field("has_mock", &self.mock.is_some())
            .field("has_node", &self.node.is_some())
            .finish()
    }
}

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
    listing(channel_id, title).sign(keys, unix_now(), 0).unwrap()
}

fn ctx(world: &mut AppWorld) -> &mut OutboundWorld {
    world.outbound.get_or_insert_with(OutboundWorld::default)
}

// ---------------------------------------------------------------------------
// Given
// ---------------------------------------------------------------------------

#[given("UPnP が利用できない環境で本ソフトウェアが起動している")]
async fn app_started_without_upnp(world: &mut AppWorld) {
    // テスト環境では UPnP は利用不能。待受なし(外向きのみ)のノードを起動する。
    let node = TestNode::spawn(1).await;
    ctx(world).node = Some(node);
}

// ---------------------------------------------------------------------------
// When
// ---------------------------------------------------------------------------

#[when("外向き接続のみでモックピアと established になる")]
async fn outbound_only_established(world: &mut AppWorld) {
    let mock = MockPeer::spawn().await;
    // 発見用イベントを SYNC 応答で返せるよう事前に用意する。
    {
        let keys = ctx(world).keys.clone();
        mock.serve_signed(&signed(&keys, CH_DISCOVER, "発見A"));
    }
    let addr = mock.addr().to_string();
    ctx(world).mock = Some(mock);

    let node = ctx(world).node.as_ref().expect("Given でノード起動済み");
    node.add_manual_peer(&addr);

    // 外向き established を待つ。
    let start = Instant::now();
    loop {
        if node.established_counts().1 > 0 {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "外向き接続のみで established に達するべき"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// Then
// ---------------------------------------------------------------------------

#[then("チャンネル掲載の機能が成立する")]
async fn channel_announce_works(world: &mut AppWorld) {
    let event = {
        let c = ctx(world);
        signed(&c.keys, CH_ANNOUNCE, "掲載A")
    };
    let node = ctx(world).node.as_ref().unwrap();
    node.hub().publish_local(event);

    // established ピア(モック)へ掲載イベントが伝搬する。
    let mock = ctx(world).mock.as_ref().unwrap();
    let start = Instant::now();
    loop {
        if mock
            .received()
            .iter()
            .any(|v| v.to_string().contains(CH_ANNOUNCE))
        {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "外向き接続のみでも掲載イベントが伝搬先ピアへ届くべき(US1)"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[then("チャンネル発見の機能が成立する")]
async fn channel_discovery_works(world: &mut AppWorld) {
    let node = ctx(world).node.as_ref().unwrap();
    assert!(
        node.wait_for_channel(CH_DISCOVER, Duration::from_secs(5)).await,
        "外向き接続のみでも SYNC で発見したチャンネルが一覧へ現れるべき(US2)"
    );
}

#[then("状態表示が「外向き接続のみで参加中」となる")]
async fn status_shows_outbound_only(world: &mut AppWorld) {
    let node = ctx(world).node.as_ref().unwrap();
    assert!(
        !node.inbound_reachable(),
        "待受なしノードは着信不可(状態表示は「外向き接続のみで参加中」)"
    );
}

//! US3(接続ピア障害時の継続性)のステップ定義(T045 骨格 → T049 で実装)
//!
//! 複数ピア接続下でのピア障害耐性と全ピア断からの自動回復を検証する:
//! - 接続ピアの 1 つが停止しても掲載伝搬・一覧取得が継続する(SC-002 単一障害点排除)
//! - 全ピア到達不能を検出して通知フラグを立て、ピア回復時に自動再接続する(US3 シナリオ 3)
//!
//! インプロセスのモックピア(gossip 契約参照実装)と実 [`P2pRuntime`](外向きのみの
//! [`TestNode`])で構成する。掲載は [`GossipHub::publish_local`] で行う。

use std::time::{Duration, Instant};

use cucumber::{given, then, when};
use nostr::{Event, Keys};
use serde_json::Value;

use peca_p2p_yp::event::schema::{ChannelListing, ChannelStatus, Track};

use crate::AppWorld;
use crate::mock_peer::{MockPeer, TestNode, unix_now};

const CH_A: &str = "0123456789abcdef0123456789abcdef";
const CH_B: &str = "0123456789abcdef0123456789abcdee";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// US3 シナリオ 1 個分の状態(cucumber は各シナリオで新規 World を生成する)。
pub struct Us3World {
    /// 接続中のモックピア。停止は Vec からの除去(Drop で shutdown)で表す。
    mocks: Vec<MockPeer>,
    node: Option<TestNode>,
    keys: Keys,
}

impl Default for Us3World {
    fn default() -> Self {
        Self {
            mocks: Vec::new(),
            node: None,
            keys: Keys::generate(),
        }
    }
}

impl std::fmt::Debug for Us3World {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Us3World")
            .field("mocks", &self.mocks.len())
            .field("has_node", &self.node.is_some())
            .finish()
    }
}

fn ctx(world: &mut AppWorld) -> &mut Us3World {
    world.us3.get_or_insert_with(Us3World::default)
}

fn listing(channel_id: &str, title: &str) -> ChannelListing {
    ChannelListing {
        channel_id: channel_id.into(),
        title: title.into(),
        summary: Some("説明".into()),
        genre: Some("game".into()),
        status: ChannelStatus::Live,
        starts: unix_now(),
        current_participants: 1,
        streaming: Some("pcp://198.51.100.1:7144/x".into()),
        bitrate_kbps: Some(1500),
        content_type: Some("FLV".into()),
        tip: Some("198.51.100.1:7144".into()),
        contact: None,
        relays: 0,
        track: Some(Track::default()),
    }
}

fn signed(keys: &Keys, channel_id: &str, title: &str) -> Event {
    listing(channel_id, title)
        .sign(keys, unix_now(), 0)
        .unwrap()
}

/// EVENT(生 JSON)の `d` タグ(= channel_id)。
fn d_tag(event: &Value) -> Option<String> {
    event["tags"].as_array()?.iter().find_map(|t| {
        let arr = t.as_array()?;
        (arr.first()?.as_str()? == "d")
            .then(|| arr.get(1)?.as_str().map(str::to_string))
            .flatten()
    })
}

/// 述語が真になるまで最大 `timeout` ポーリングする。
async fn wait_until(timeout: Duration, mut pred: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    loop {
        if pred() {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// モックピアが指定 channel_id の EVENT を受信するまで最大 `timeout` 待つ。
async fn wait_mock_received(mock: &MockPeer, channel_id: &str, timeout: Duration) -> bool {
    wait_until(timeout, || {
        mock.received()
            .iter()
            .any(|v| d_tag(v).as_deref() == Some(channel_id))
    })
    .await
}

/// 外向き established が `n` 本以上になるまで待つ。
async fn wait_established(node: &TestNode, n: usize, timeout: Duration) -> bool {
    wait_until(timeout, || node.established_counts().1 >= n).await
}

// ---------------------------------------------------------------------------
// Given
// ---------------------------------------------------------------------------

#[given("本ソフトウェアが複数のモックピアと established になっている")]
async fn app_running_with_multiple_mock_peers(world: &mut AppWorld) {
    let p1 = MockPeer::spawn().await;
    let p2 = MockPeer::spawn().await;
    let node = TestNode::spawn(0xA301).await;
    node.add_manual_peer(p1.addr());
    node.add_manual_peer(p2.addr());
    assert!(
        wait_established(&node, 2, CONNECT_TIMEOUT).await,
        "2 つのモックピアと established になるべき"
    );
    let c = ctx(world);
    c.mocks = vec![p1, p2];
    c.node = Some(node);
}

#[given("複数接続下でチャンネルが掲載中である")]
async fn channel_announced_with_multiple_peers(world: &mut AppWorld) {
    let c = ctx(world);
    let event = signed(&c.keys, CH_A, "掲載中チャンネル");
    c.node.as_ref().expect("ノード").hub().publish_local(event);
    // 全ての接続ピアが最初の掲載イベントを受信していること。
    let mocks = &ctx(world).mocks;
    for (i, m) in mocks.iter().enumerate() {
        assert!(
            wait_mock_received(m, CH_A, CONNECT_TIMEOUT).await,
            "掲載中の前提: モックピア {i} が掲載イベントを受信しているべき"
        );
    }
}

#[given("複数のモックピアがチャンネルイベントを保持している")]
async fn multiple_mock_peers_hold_channel_events(world: &mut AppWorld) {
    let c = ctx(world);
    let p1 = MockPeer::spawn().await;
    let p2 = MockPeer::spawn().await;
    // 両ピアが同一チャンネルを保持(冗長経路 — どちらか一方の停止に耐える)。
    let e = signed(&c.keys, CH_A, "配信A");
    p1.serve_signed(&e);
    p2.serve_signed(&e);
    c.mocks = vec![p1, p2];
}

#[given("本ソフトウェアが複数のモックピアへ接続している")]
async fn app_connected_to_multiple_mock_peers(world: &mut AppWorld) {
    let node = TestNode::spawn(0xA302).await;
    {
        let c = ctx(world);
        for m in &c.mocks {
            node.add_manual_peer(m.addr());
        }
        c.node = Some(node);
    }
    let node = ctx(world).node.as_ref().unwrap();
    assert!(
        wait_established(node, 2, CONNECT_TIMEOUT).await,
        "複数のモックピアと established になるべき"
    );
    assert!(
        node.wait_for_channel(CH_A, CONNECT_TIMEOUT).await,
        "接続 → SYNC で一覧へ反映されるべき"
    );
}

#[given("本ソフトウェアが単一のモックピアと established になっている")]
async fn app_running_with_single_mock_peer(world: &mut AppWorld) {
    let p1 = MockPeer::spawn().await;
    let node = TestNode::spawn(0xA303).await;
    node.add_manual_peer(p1.addr());
    assert!(
        wait_established(&node, 1, CONNECT_TIMEOUT).await,
        "単一のモックピアと established になるべき"
    );
    let c = ctx(world);
    c.mocks = vec![p1];
    c.node = Some(node);
}

// ---------------------------------------------------------------------------
// When
// ---------------------------------------------------------------------------

#[when("接続ピアの1つを停止する")]
async fn stop_one_connected_peer(world: &mut AppWorld) {
    let c = ctx(world);
    assert!(c.mocks.len() >= 2, "停止対象を含む複数ピアが必要");
    // 先頭のモックピアを停止(Drop が shutdown を送る)。
    let _stopped = c.mocks.remove(0);
    drop(_stopped);
    // 残ピアのみになるのを待つ(established が減る)。
    let node = c.node.as_ref().expect("ノード");
    wait_until(CONNECT_TIMEOUT, || node.established_counts().1 <= 1).await;
}

#[when("全ての接続ピアを停止する")]
async fn stop_all_connected_peers(world: &mut AppWorld) {
    let c = ctx(world);
    // 全モックピアを停止する(Drop で shutdown)。以後その待受アドレスは接続拒否になる。
    c.mocks.clear();
}

// ---------------------------------------------------------------------------
// Then
// ---------------------------------------------------------------------------

#[then("残りのモックピアは引き続きチャンネルイベントを受信する")]
async fn remaining_mock_peer_still_receives_events(world: &mut AppWorld) {
    let c = ctx(world);
    // 掲載を続ける → 残るピアへ引き続き伝搬される(SC-002)。
    let event = signed(&c.keys, CH_B, "配信B");
    c.node.as_ref().expect("ノード").hub().publish_local(event);
    let remaining = c.mocks.first().expect("残ピア");
    assert!(
        wait_mock_received(remaining, CH_B, CONNECT_TIMEOUT).await,
        "ピア 1 停止後も残ピアへの掲載伝搬は継続するべき(単一障害点排除 — SC-002)"
    );
}

#[then("一覧のチャンネルは引き続き表示される")]
async fn channel_list_remains_available(world: &mut AppWorld) {
    let node = ctx(world).node.as_ref().expect("ノード");
    // 一部ピア停止後も鮮度窓内の一覧は維持される。
    assert!(
        node.snapshot().iter().any(|ch| ch.channel_id == CH_A),
        "ピア 1 停止後も一覧のチャンネルは表示され続けるべき"
    );
}

#[then("全ピア断の通知が出る")]
async fn all_peers_disconnected_notification(world: &mut AppWorld) {
    let node = ctx(world).node.as_ref().expect("ノード");
    let reachability = node.reachability();
    // 接続拒否が続き、全ピア到達不能フラグ(= UI の到達不能バナー契機)が立つ。
    assert!(
        wait_until(Duration::from_secs(15), || reachability
            .is_all_unreachable())
        .await,
        "全ピア到達不能の通知(status フラグ)が出るべき"
    );
}

#[then("ピアが回復したとき自動で再接続する")]
async fn auto_reconnect_on_peer_recovery(world: &mut AppWorld) {
    let c = ctx(world);
    // 生きたピアが現れる → 外向き維持ループが自動再接続して到達可能へ回復する。
    let peer = MockPeer::spawn().await;
    c.node
        .as_ref()
        .expect("ノード")
        .add_manual_peer(peer.addr());
    c.mocks.push(peer);
    let node = c.node.as_ref().unwrap();
    let reachability = node.reachability();
    assert!(
        wait_until(Duration::from_secs(10), || !reachability
            .is_all_unreachable())
        .await,
        "回復可能なピア出現で自動再接続し到達可能へ戻るべき"
    );
    assert!(
        wait_established(node, 1, CONNECT_TIMEOUT).await,
        "回復後は established を持つ"
    );
}

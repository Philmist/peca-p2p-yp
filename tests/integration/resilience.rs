//! US3 統合テスト(T049)— 接続ピア障害時の継続性
//!
//! インプロセスのモックピア(gossip 契約参照実装)と実 [`P2pRuntime`]
//! ([`TestNode`] — 外向きのみ/待受あり)で、次を検証する:
//! - **SC-002 単一障害点排除**: 接続ピアの 1 つが停止しても、残るピアへの掲載伝搬が継続する
//! - **多段伝搬の継続**: 実ノード 4 台のメッシュ(チェーン + 冗長経路)で中間ノードを
//!   停止しても、冗長経路経由の再伝搬で一覧が届き続ける
//! - **全断検出と自動回復**: 全ピア到達不能を検出して通知フラグを立て、ピア回復時に
//!   自動再接続して回復通知(= 掲載の自動再開トリガ)を出す
//! - **churn**: ピアの参加・離脱の反復下でも一覧が維持される
//!
//! contracts/p2p-gossip.md §接続管理・§検証方法、spec US3 / SC-002 に対応する。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use nostr::{Event, Keys};
use serde_json::Value;
use tokio::net::TcpListener;

use peca_p2p_yp::event::schema::{ChannelListing, ChannelStatus, Track};

#[path = "../common/mock_peer.rs"]
mod mock_peer;

use mock_peer::{MockPeer, TestNode, unix_now};

// ---------------------------------------------------------------------------
// 補助
// ---------------------------------------------------------------------------

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

/// モックピアが指定 channel_id の EVENT を受信するまで最大 `timeout` 待つ。
async fn wait_mock_received(mock: &MockPeer, channel_id: &str, timeout: Duration) -> bool {
    let start = Instant::now();
    loop {
        if mock
            .received()
            .iter()
            .any(|v| d_tag(v).as_deref() == Some(channel_id))
        {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 述語が真になるまで最大 `timeout` 待つ(汎用ポーリング)。
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

/// 待受をバインドして即座に閉じ、接続が拒否される(到達不能な)アドレスを得る。
async fn dead_addr() -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    drop(l); // ポートを解放 → 以後の connect は拒否される
    addr
}

const CH_A: &str = "0123456789abcdef0123456789abcdef";
const CH_B: &str = "0123456789abcdef0123456789abcdee";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// SC-002: 接続ピアの 1 つが停止しても掲載伝搬が継続する(単一障害点排除)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn propagation_continues_when_one_peer_stops() {
    let keys = Keys::generate();

    // 掲載ノード(外向きのみ)を 2 つのモックピアへ接続する。
    let p1 = MockPeer::spawn().await;
    let p2 = MockPeer::spawn().await;
    let node = TestNode::spawn(0xA001).await;
    node.add_manual_peer(p1.addr());
    node.add_manual_peer(p2.addr());

    // 双方と established になるのを待つ(established 2 本)。
    assert!(
        wait_until(CONNECT_TIMEOUT, || node.established_counts().1 >= 2).await,
        "2 つのピアと established になるべき"
    );

    // 掲載(ローカル発行)→ 両ピアが受信する。
    node.hub().publish_local(signed(&keys, CH_A, "配信A"));
    assert!(
        wait_mock_received(&p1, CH_A, CONNECT_TIMEOUT).await,
        "p1 が最初の掲載イベントを受信する"
    );
    assert!(
        wait_mock_received(&p2, CH_A, CONNECT_TIMEOUT).await,
        "p2 が最初の掲載イベントを受信する"
    );

    // 接続ピアの 1 つ(p1)を停止する。
    drop(p1);
    assert!(
        wait_until(CONNECT_TIMEOUT, || node.established_counts().1 <= 1).await,
        "p1 停止で established が減る"
    );

    // 掲載を続ける → 残る p2 へは引き続き伝搬される(SC-002)。
    node.hub().publish_local(signed(&keys, CH_B, "配信B"));
    assert!(
        wait_mock_received(&p2, CH_B, CONNECT_TIMEOUT).await,
        "p1 停止後も p2 への掲載伝搬は継続する(単一障害点排除 — SC-002)"
    );
}

// ---------------------------------------------------------------------------
// SC-002: 実ノード 4 台のメッシュで中間ノード停止後も多段伝搬が継続する
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mesh_propagation_survives_middle_node_stop() {
    let keys = Keys::generate();

    // 実ノード 4 台のトポロジ(チェーン + 冗長経路 — contracts §検証方法):
    //   n2 → n1、n3 → n2、n4 → n3、n4 → n2(冗長経路)
    let n1 = TestNode::spawn_listening(0xD001).await;
    let n2 = TestNode::spawn_listening(0xD002).await;
    let n3 = TestNode::spawn_listening(0xD003).await;
    let n4 = TestNode::spawn_listening(0xD004).await;
    n2.add_manual_peer(n1.listen_addr());
    n3.add_manual_peer(n2.listen_addr());
    n4.add_manual_peer(n3.listen_addr());
    n4.add_manual_peer(n2.listen_addr());

    // 全リンクが established になるのを待つ。
    assert!(
        wait_until(CONNECT_TIMEOUT, || {
            n2.established_counts().1 >= 1
                && n3.established_counts().1 >= 1
                && n4.established_counts().1 >= 2
        })
        .await,
        "メッシュの全リンクが established になるべき"
    );

    // n1 で掲載 → 多段再伝搬(n1→n2→n3/n4)で全ノードの一覧へ届く。
    n1.hub().publish_local(signed(&keys, CH_A, "配信A"));
    for (name, node) in [("n2", &n2), ("n3", &n3), ("n4", &n4)] {
        assert!(
            node.wait_for_channel(CH_A, CONNECT_TIMEOUT).await,
            "{name} へ多段再伝搬で掲載イベントが届くべき"
        );
    }

    // 中間ノード n3 を停止 → n4 への経路は冗長経路(n2→n4)が残る。
    drop(n3);
    assert!(
        wait_until(CONNECT_TIMEOUT, || n4.established_counts().1 <= 1).await,
        "n3 停止で n4 の established が減る"
    );

    // 掲載を続ける → n4 へは冗長経路経由で引き続き伝搬される(SC-002)。
    n1.hub().publish_local(signed(&keys, CH_B, "配信B"));
    assert!(
        n4.wait_for_channel(CH_B, CONNECT_TIMEOUT).await,
        "中間ノード停止後も冗長経路で多段伝搬が継続する(単一障害点排除 — SC-002)"
    );
}

// ---------------------------------------------------------------------------
// 全断検出 → 自動回復(通知フラグと回復通知)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn all_peers_unreachable_detected_then_recovers() {
    // 到達不能アドレスのみを手動ピアに持つノード(外向きのみ)。
    let node = TestNode::spawn(0xB002).await;
    let dead = dead_addr().await;
    node.add_manual_peer(&dead);

    // 回復通知(全断→再接続)を捕捉するリスナーを張る。
    let reachability = node.reachability();
    let recovered = Arc::new(AtomicBool::new(false));
    {
        let reachability = Arc::clone(&reachability);
        let flag = Arc::clone(&recovered);
        tokio::spawn(async move {
            reachability.recovered().await;
            flag.store(true, Ordering::SeqCst);
        });
    }

    // 接続拒否が続き、全ピア到達不能が検出される(通知フラグが立つ)。
    assert!(
        wait_until(Duration::from_secs(15), || reachability
            .is_all_unreachable())
        .await,
        "到達不能ピアのみのとき全ピア到達不能を検出するべき"
    );

    // 生きたピアが現れる → 自動再接続 → 到達可能へ回復。
    let peer = MockPeer::spawn().await;
    node.add_manual_peer(peer.addr());
    assert!(
        wait_until(Duration::from_secs(10), || !reachability
            .is_all_unreachable())
        .await,
        "回復可能なピアが現れたら自動再接続して到達可能へ戻るべき"
    );
    assert!(
        wait_until(Duration::from_secs(2), || recovered.load(Ordering::SeqCst)).await,
        "全断からの回復通知(掲載の自動再開トリガ)が出るべき"
    );
    assert!(
        node.established_counts().1 >= 1,
        "回復後は established を持つ"
    );
}

// ---------------------------------------------------------------------------
// churn: ピアの参加・離脱の反復下でも一覧が維持される
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_survives_peer_churn() {
    let keys = Keys::generate();
    let node = TestNode::spawn(0xC003).await;

    // 参加 → イベント受信 → 離脱を数サイクル反復する。毎回一覧に反映されることを確認。
    for round in 0..3u8 {
        let mock = MockPeer::spawn().await;
        mock.serve_signed(&signed(&keys, CH_A, "配信A"));
        node.add_manual_peer(mock.addr());
        assert!(
            node.wait_for_channel(CH_A, CONNECT_TIMEOUT).await,
            "ラウンド {round}: 接続 → SYNC で一覧へ反映されるべき"
        );
        // ピア離脱(churn)。
        drop(mock);
        // established が落ちても一覧(鮮度窓内)は維持される。
        assert!(
            node.snapshot().iter().any(|c| c.channel_id == CH_A),
            "ラウンド {round}: ピア離脱後も鮮度窓内の一覧は維持される"
        );
    }
}

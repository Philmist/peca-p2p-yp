//! US1(配信者によるチャンネル掲載)のステップ定義(T022 骨格 → T033 で実装)
//!
//! 掲載側ハーネス([`announcer::AnnouncerNode`] + [`announcer::PcpClient`])と
//! モックピア(gossip 契約参照実装)で spec US1 受け入れシナリオ 1〜3 を検証する。

use std::time::Duration;

use cucumber::{given, then, when};
use serde_json::Value;

use peca_p2p_yp::event::schema::{VerifyConfig, verify_incoming};

use crate::AppWorld;

#[path = "../common/announcer.rs"]
mod announcer;

use crate::mock_peer::{MockPeer, unix_now};
use announcer::{AnnouncerNode, PcpClient};

const CID: [u8; 16] = [0x5A; 16];

fn cid_hex() -> String {
    CID.iter().map(|b| format!("{b:02x}")).collect()
}

/// US1 シナリオ 1 個分の状態。
pub struct Us1World {
    node: AnnouncerNode,
    mock: MockPeer,
    client: Option<PcpClient>,
}

impl std::fmt::Debug for Us1World {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Us1World")
            .field("has_client", &self.client.is_some())
            .finish()
    }
}

fn ctx(world: &mut AppWorld) -> &mut Us1World {
    world.us1.as_mut().expect("Background で初期化済みのはず")
}

/// タグ `[name, value]` の値。
fn tag_value(event: &Value, name: &str) -> Option<String> {
    event["tags"].as_array()?.iter().find_map(|t| {
        let arr = t.as_array()?;
        (arr.first()?.as_str()? == name)
            .then(|| arr.get(1)?.as_str().map(str::to_string))
            .flatten()
    })
}

/// モックピアが述語を満たす EVENT を受信するまで待つ。
async fn wait_received(
    mock: &MockPeer,
    timeout: Duration,
    pred: impl Fn(&Value) -> bool,
) -> Option<Value> {
    let start = std::time::Instant::now();
    loop {
        if let Some(found) = mock.received().into_iter().rev().find(|v| pred(v)) {
            return Some(found);
        }
        if start.elapsed() >= timeout {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 受信イベントが受信検証 1〜6 を通過することを検査する。
fn assert_verifiable(event: &Value) {
    let raw = serde_json::to_string(event).unwrap();
    verify_incoming(&raw, &VerifyConfig::default(), unix_now())
        .expect("モックピアが受信したイベントは検証可能であるべき");
}

// ---------------------------------------------------------------------------
// Background / Given
// ---------------------------------------------------------------------------

#[given("本ソフトウェアが起動しモックピアと established になっている")]
async fn app_running_with_mock_peer(world: &mut AppWorld) {
    let mock = MockPeer::spawn().await;
    let node = AnnouncerNode::spawn(0xC0FFEE).await;
    node.add_manual_peer(mock.addr());
    assert!(
        node.wait_established(Duration::from_secs(5)).await,
        "モックピアと established になるべき"
    );
    world.us1 = Some(Us1World {
        node,
        mock,
        client: None,
    });
}

#[given("チャンネルが掲載中である")]
async fn channel_announced(world: &mut AppWorld) {
    let c = ctx(world);
    let mut client = PcpClient::connect(c.node.pcp_addr(), [0x10; 16]).await;
    client
        .broadcast(&CID, "掲載中チャンネル", "game", "説明")
        .await;
    c.client = Some(client);
    let mock = &ctx(world).mock;
    assert!(
        wait_received(mock, Duration::from_secs(10), |v| {
            tag_value(v, "d").as_deref() == Some(cid_hex().as_str())
                && tag_value(v, "status").as_deref() == Some("live")
        })
        .await
        .is_some(),
        "掲載中の前提: live イベントがモックピアへ届いているべき"
    );
}

// ---------------------------------------------------------------------------
// When
// ---------------------------------------------------------------------------

#[when("PCP 疑似クライアントが配信開始を通知する")]
async fn pcp_client_announces(world: &mut AppWorld) {
    let c = ctx(world);
    let mut client = PcpClient::connect(c.node.pcp_addr(), [0x11; 16]).await;
    client.broadcast(&CID, "新規配信", "game", "説明").await;
    c.client = Some(client);
}

#[when("配信者がチャンネル詳細を変更する")]
async fn broadcaster_updates_details(world: &mut AppWorld) {
    let c = ctx(world);
    let client = c.client.as_mut().expect("掲載中の PCP 接続");
    client
        .broadcast(&CID, "掲載中チャンネル", "talk", "変更後の説明")
        .await;
}

#[when("配信者が配信を終了する")]
async fn broadcaster_stops(world: &mut AppWorld) {
    let c = ctx(world);
    let client = c.client.take().expect("掲載中の PCP 接続");
    client.quit().await;
}

// ---------------------------------------------------------------------------
// Then
// ---------------------------------------------------------------------------

#[then("モックピアは 60 秒以内に検証可能な署名済みチャンネルイベントを受信する")]
async fn mock_peer_receives_signed_event(world: &mut AppWorld) {
    let c = ctx(world);
    let event = wait_received(&c.mock, Duration::from_secs(60), |v| {
        tag_value(v, "d").as_deref() == Some(cid_hex().as_str())
            && tag_value(v, "status").as_deref() == Some("live")
    })
    .await
    .expect("60 秒以内に署名済みイベントを受信するべき");
    assert_verifiable(&event);
    assert_eq!(
        event["pubkey"].as_str().unwrap(),
        c.node.persona_pubkey,
        "署名鍵は選択中ペルソナのもの"
    );
}

#[then("モックピアは 60 秒以内に変更内容を反映したイベントを受信する")]
async fn mock_peer_receives_updated_event(world: &mut AppWorld) {
    let c = ctx(world);
    let event = wait_received(&c.mock, Duration::from_secs(60), |v| {
        tag_value(v, "d").as_deref() == Some(cid_hex().as_str())
            && tag_value(v, "t").as_deref() == Some("talk")
    })
    .await
    .expect("60 秒以内に変更(ジャンル talk)を反映したイベントを受信するべき");
    assert_verifiable(&event);
    assert_eq!(
        tag_value(&event, "summary").as_deref(),
        Some("変更後の説明"),
        "説明の変更も反映される"
    );
}

#[then("モックピアは status が ended の最終イベントを受信する")]
async fn mock_peer_receives_ended_event(world: &mut AppWorld) {
    let c = ctx(world);
    let event = wait_received(&c.mock, Duration::from_secs(10), |v| {
        tag_value(v, "d").as_deref() == Some(cid_hex().as_str())
            && tag_value(v, "status").as_deref() == Some("ended")
    })
    .await
    .expect("status=ended の最終イベントを受信するべき");
    assert_verifiable(&event);
}

#[then("自ノードの一覧から当該チャンネルが除去される")]
async fn channel_removed_from_local_list(world: &mut AppWorld) {
    let c = ctx(world);
    assert!(
        c.node
            .wait_until(Duration::from_secs(2), |rows| !rows
                .iter()
                .any(|r| r.channel_id == cid_hex()))
            .await,
        "終了後は自ノードの一覧から除去されるべき"
    );
}

//! US2(視聴者によるチャンネル発見)のステップ定義(T034 骨格 → T044 で実装)
//!
//! モックピア([`mock_peer`])から署名済み/鮮度切れイベントを投入し、接続直後 SYNC で
//! 一覧が構築されること(SC-004)・不正分の不可視(SC-005)・鮮度切れ除去を検証する。
//! index.txt 反映の検証は web の T042 完了後に組み込む(現状は ChannelDirectory 経由の
//! 一覧検証で代替 — 統括へ報告済み)。

use std::time::Duration;

use cucumber::{given, then, when};
use nostr::{Event, Keys};

use peca_p2p_yp::config::IndexEncoding;
use peca_p2p_yp::event::schema::{ChannelListing, ChannelStatus, Track};
use peca_p2p_yp::event::view::DiscoveredChannel;
use peca_p2p_yp::yp::index_txt::generate;

use crate::AppWorld;

use crate::mock_peer::{MockPeer, TestNode, unix_now};

const CH_LIVE: &str = "0123456789abcdef0123456789abcdef";
const CH_STALE: &str = "0123456789abcdef0123456789abcde0";
const TIP: &str = "198.51.100.1:7144";

/// 前提(Given)の接続 → SYNC 待ちに使う余裕を持ったタイムアウト。
/// SC-004 の「5 秒以内」検証(下記 Then)とは別物で、遅い CI ランナー
/// (windows-latest)でのプロセス起動 / TCP 確立オーバーヘッドを吸収する。
/// ポーリングは条件成立で即 return するため green run のコストは実質ゼロ。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// US2 シナリオ 1 個分の状態(cucumber は各シナリオで新規 World を生成する)。
pub struct Us2World {
    mock: Option<MockPeer>,
    node: Option<TestNode>,
    keys: Keys,
    last_snapshot: Vec<DiscoveredChannel>,
    /// 直近に生成した index.txt 互換出力(UTF-8 テキスト)。
    index_txt: String,
}

impl Default for Us2World {
    fn default() -> Self {
        Self {
            mock: None,
            node: None,
            keys: Keys::generate(),
            last_snapshot: Vec::new(),
            index_txt: String::new(),
        }
    }
}

impl std::fmt::Debug for Us2World {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Us2World")
            .field("has_mock", &self.mock.is_some())
            .field("has_node", &self.node.is_some())
            .field("rows", &self.last_snapshot.len())
            .finish()
    }
}

fn listing(
    channel_id: &str,
    title: &str,
    status: ChannelStatus,
    tip: Option<&str>,
) -> ChannelListing {
    ChannelListing {
        channel_id: channel_id.into(),
        title: title.into(),
        summary: Some("説明".into()),
        genre: Some("game".into()),
        status,
        starts: unix_now(),
        current_participants: 3,
        streaming: tip.map(|t| format!("pcp://{t}/{channel_id}")),
        bitrate_kbps: Some(1500),
        content_type: Some("FLV".into()),
        tip: tip.map(|t| t.to_string()),
        contact: Some("https://example.com/".into()),
        relays: 1,
        track: Some(Track::default()),
    }
}

fn signed(keys: &Keys, channel_id: &str, title: &str, created: u64, tip: Option<&str>) -> Event {
    listing(channel_id, title, ChannelStatus::Live, tip)
        .sign(keys, created, 0)
        .unwrap()
}

/// World から us2 状態を取り出す(未初期化なら生成する)。
fn ctx(world: &mut AppWorld) -> &mut Us2World {
    world.us2.get_or_insert_with(Us2World::default)
}

// ---------------------------------------------------------------------------
// Given
// ---------------------------------------------------------------------------

#[given("モックピアが配信中チャンネルの署名済みイベントを保持している")]
async fn mock_peer_holds_live_event(world: &mut AppWorld) {
    let mock = MockPeer::spawn().await;
    let c = ctx(world);
    mock.serve_signed(&signed(&c.keys, CH_LIVE, "配信A", unix_now(), Some(TIP)));
    c.mock = Some(mock);
}

#[given("トラッカー接続先つきのチャンネルが一覧に表示されている")]
async fn channel_with_tip_listed(world: &mut AppWorld) {
    let mock = MockPeer::spawn().await;
    let node = TestNode::spawn(1).await;
    node.add_manual_peer(mock.addr());
    {
        let c = ctx(world);
        mock.serve_signed(&signed(&c.keys, CH_LIVE, "配信A", unix_now(), Some(TIP)));
        c.mock = Some(mock);
        c.node = Some(node);
    }
    // 一覧へ現れるまで待つ(接続 → SYNC)。
    let node = ctx(world).node.as_ref().unwrap();
    assert!(
        node.wait_for_channel(CH_LIVE, CONNECT_TIMEOUT).await,
        "トラッカー接続先つきチャンネルが一覧へ現れるべき"
    );
}

#[given("鮮度窓を超えて経過したチャンネルイベントを受信済みである")]
async fn stale_event_received(world: &mut AppWorld) {
    let mock = MockPeer::spawn().await;
    let node = TestNode::spawn(1).await;
    node.add_manual_peer(mock.addr());
    {
        let c = ctx(world);
        // 鮮度窓(600 秒)超のイベント + 接続確認用の live イベント。
        mock.serve_signed(&signed(
            &c.keys,
            CH_STALE,
            "鮮度切れ",
            unix_now() - 700,
            Some(TIP),
        ));
        mock.serve_signed(&signed(&c.keys, CH_LIVE, "現行", unix_now(), Some(TIP)));
        c.mock = Some(mock);
        c.node = Some(node);
    }
    let node = ctx(world).node.as_ref().unwrap();
    assert!(
        node.wait_for_channel(CH_LIVE, CONNECT_TIMEOUT).await,
        "現行チャンネルで接続確立を確認"
    );
}

// ---------------------------------------------------------------------------
// When
// ---------------------------------------------------------------------------

#[when("本ソフトウェアがそのモックピアへ接続する")]
async fn app_connects_to_mock_peer(world: &mut AppWorld) {
    let addr = ctx(world).mock.as_ref().unwrap().addr().to_string();
    let node = TestNode::spawn(1).await;
    node.add_manual_peer(&addr);
    ctx(world).node = Some(node);
}

#[when("視聴者が index.txt 互換出力を取得する")]
async fn viewer_fetches_index_txt(world: &mut AppWorld) {
    // T042 の実出力(yp::index_txt::generate)で index.txt を生成する。
    let c = ctx(world);
    c.last_snapshot = c.node.as_ref().unwrap().snapshot();
    let bytes = generate(&c.last_snapshot, IndexEncoding::Utf8, unix_now());
    c.index_txt = String::from_utf8(bytes).expect("UTF-8 出力");
}

#[when("視聴者が一覧を更新する")]
async fn viewer_refreshes_list(world: &mut AppWorld) {
    let c = ctx(world);
    c.last_snapshot = c.node.as_ref().unwrap().snapshot();
}

// ---------------------------------------------------------------------------
// Then
// ---------------------------------------------------------------------------

#[then("5 秒以内にチャンネルが名称とジャンルとともに一覧へ表示される")]
async fn channel_listed_within_5s(world: &mut AppWorld) {
    let node = ctx(world).node.as_ref().unwrap();
    assert!(
        node.wait_for_channel(CH_LIVE, Duration::from_secs(5)).await,
        "5 秒以内に一覧へ表示されるべき(SC-004)"
    );
    let rows = node.snapshot();
    let row = rows
        .iter()
        .find(|c| c.channel_id == CH_LIVE)
        .expect("一覧に存在");
    assert_eq!(row.listing.title, "配信A", "名称が表示される");
    assert_eq!(
        row.listing.genre.as_deref(),
        Some("game"),
        "ジャンルが表示される"
    );
}

#[then("出力にはチャンネル ID とトラッカー接続先が含まれる")]
async fn index_txt_contains_id_and_tip(world: &mut AppWorld) {
    let c = ctx(world);
    // ID は出力時に大文字化される(contracts/http-yp.md 変換規則)。
    let upper_id = CH_LIVE.to_uppercase();
    assert!(
        c.index_txt.contains(&upper_id),
        "index.txt 出力にチャンネル ID(大文字)が含まれる: {:?}",
        c.index_txt
    );
    assert!(
        c.index_txt.contains(TIP),
        "index.txt 出力にトラッカー接続先(TIP)が含まれる: {:?}",
        c.index_txt
    );
}

#[then("当該チャンネルは一覧に表示されない")]
async fn stale_channel_not_listed(world: &mut AppWorld) {
    let c = ctx(world);
    assert!(
        !c.last_snapshot.iter().any(|r| r.channel_id == CH_STALE),
        "鮮度切れチャンネルは表示されない"
    );
}

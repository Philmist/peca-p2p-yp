//! US2 統合テスト(T044)
//!
//! モックピア(gossip 契約の参照実装)から署名済み/不正イベントを投入し、次を
//! 実ランタイム(外向きのみ)で検証する: 接続直後 SYNC での初期一覧構築(SC-004。
//! 典型時 1 秒未満)、不正イベントの不可視(SC-005)、鮮度切れイベントの除去。
//! あわせて共有フィクスチャ
//! `gossip_vectors.json` がモックのフレーム層と乖離しないこと(research R11)を検査する。

use std::time::Duration;

use nostr::{Event, Keys};
use serde_json::Value;

use peca_p2p_yp::event::schema::{ChannelListing, ChannelStatus, Track};
use peca_p2p_yp::config::IndexEncoding;
use peca_p2p_yp::yp::index_txt::generate;

#[path = "../common/mock_peer.rs"]
mod mock_peer;

use mock_peer::{unix_now, MockPeer, TestNode};

// ---------------------------------------------------------------------------
// 補助
// ---------------------------------------------------------------------------

fn listing(channel_id: &str, title: &str, status: ChannelStatus, tip: Option<&str>) -> ChannelListing {
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

fn signed(keys: &Keys, channel_id: &str, title: &str, created: u64) -> Event {
    listing(channel_id, title, ChannelStatus::Live, Some("198.51.100.1:7144"))
        .sign(keys, created, 0)
        .unwrap()
}

const CH_A: &str = "0123456789abcdef0123456789abcdef";
const CH_B: &str = "0123456789abcdef0123456789abcdee";
const CH_STALE: &str = "0123456789abcdef0123456789abcde0";

// ---------------------------------------------------------------------------
// SC-004: 接続直後 SYNC で一覧が構築される
// ---------------------------------------------------------------------------

#[tokio::test]
async fn channel_appears_via_sync_within_5s() {
    let keys = Keys::generate();
    let now = unix_now();

    let mock = MockPeer::spawn().await;
    mock.serve_signed(&signed(&keys, CH_A, "配信A", now));

    let node = TestNode::spawn(1).await;
    node.add_manual_peer(mock.addr());

    // 接続 → SYNC → 一覧反映(典型時 1 秒未満だが余裕をみて 5 秒)。
    assert!(
        node.wait_for_channel(CH_A, Duration::from_secs(5)).await,
        "SYNC で受信したチャンネルが 5 秒以内に一覧へ現れるべき(SC-004)"
    );

    let rows = node.snapshot();
    let row = rows.iter().find(|c| c.channel_id == CH_A).unwrap();
    assert_eq!(row.listing.title, "配信A", "名称が反映される");
    assert_eq!(row.listing.genre.as_deref(), Some("game"), "ジャンルが反映される");
    // 受信ピア(モック)が source_peers に記録される。
    assert!(
        row.source_peers.iter().any(|p| p == mock.addr()),
        "受信元ピアが source_peers に記録される: {:?}",
        row.source_peers
    );
}

// ---------------------------------------------------------------------------
// SC-005: 不正イベントは一覧に現れない(正当分のみ可視)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn invalid_event_is_not_visible() {
    let keys = Keys::generate();
    let now = unix_now();

    let mock = MockPeer::spawn().await;
    // 正当なイベント。
    mock.serve_signed(&signed(&keys, CH_A, "正当", now));
    // 署名不正イベント(content を改竄 → id/sig 不一致)。
    let valid_b = signed(&keys, CH_B, "改竄前", now);
    let mut tampered: Value = serde_json::to_value(&valid_b).unwrap();
    tampered["content"] = Value::String("tampered".into());
    mock.serve_value(tampered);

    let node = TestNode::spawn(1).await;
    node.add_manual_peer(mock.addr());

    assert!(
        node.wait_for_channel(CH_A, Duration::from_secs(5)).await,
        "正当なチャンネルは一覧へ現れる"
    );
    // 少し待って不正分が現れないことを確認する。
    tokio::time::sleep(Duration::from_millis(300)).await;
    let rows = node.snapshot();
    assert!(
        rows.iter().any(|c| c.channel_id == CH_A),
        "正当分は可視"
    );
    assert!(
        !rows.iter().any(|c| c.channel_id == CH_B),
        "署名不正イベントは不可視(SC-005)"
    );
}

// ---------------------------------------------------------------------------
// 鮮度切れイベントは一覧に現れない
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stale_event_is_not_listed() {
    let keys = Keys::generate();
    let now = unix_now();

    let mock = MockPeer::spawn().await;
    // 鮮度窓(600 秒)を超えて古いイベント(署名は正当・未来方向ずれなし)。
    mock.serve_signed(&signed(&keys, CH_STALE, "鮮度切れ", now - 700));
    // 対照の live イベント(接続確立の確認用)。
    mock.serve_signed(&signed(&keys, CH_A, "現行", now));

    let node = TestNode::spawn(1).await;
    node.add_manual_peer(mock.addr());

    assert!(
        node.wait_for_channel(CH_A, Duration::from_secs(5)).await,
        "現行チャンネルで接続確立を確認"
    );
    let rows = node.snapshot();
    assert!(
        !rows.iter().any(|c| c.channel_id == CH_STALE),
        "鮮度窓を超えたイベントは一覧に現れない"
    );
}

// ---------------------------------------------------------------------------
// T044: index.txt 実出力に SYNC で発見したチャンネルが反映される
// ---------------------------------------------------------------------------

#[tokio::test]
async fn index_txt_reflects_discovered_channel() {
    let keys = Keys::generate();
    let now = unix_now();

    let mock = MockPeer::spawn().await;
    mock.serve_signed(&signed(&keys, CH_A, "配信A", now));

    let node = TestNode::spawn(1).await;
    node.add_manual_peer(mock.addr());
    assert!(
        node.wait_for_channel(CH_A, Duration::from_secs(5)).await,
        "接続 → SYNC で一覧へ反映される"
    );

    // yp::index_txt::generate(T042)の実出力に反映されること。
    let bytes = generate(&node.snapshot(), IndexEncoding::Utf8, now);
    let text = String::from_utf8(bytes).expect("UTF-8 出力");
    // ID は出力時に大文字化される(contracts/http-yp.md)。
    assert!(
        text.contains(&CH_A.to_uppercase()),
        "index.txt にチャンネル ID(大文字)が含まれる: {text:?}"
    );
    assert!(
        text.contains("198.51.100.1:7144"),
        "index.txt にトラッカー接続先(TIP)が含まれる: {text:?}"
    );
    // 18 フィールド(`<>` 区切り 17 個)である。
    let first_line = text.lines().next().expect("少なくとも 1 行");
    assert_eq!(first_line.split("<>").count(), 18, "18 フィールド構成");
}

// ---------------------------------------------------------------------------
// research R11: 共有フィクスチャとモックのフレーム層が乖離しない
// ---------------------------------------------------------------------------

#[test]
fn mock_peer_frame_layer_matches_shared_fixtures() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/contract/fixtures/gossip_vectors.json"
    );
    let text = std::fs::read_to_string(path).expect("フィクスチャ読込");
    let fx: Value = serde_json::from_str(&text).unwrap();

    // valid_messages: モックが用いる frame 層で往復して構造が保たれる。
    for case in fx["valid_messages"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let original = &case["message"];
        let payload = serde_json::to_vec(original).unwrap();
        let message = mock_peer::decode_message(&payload)
            .unwrap_or_else(|| panic!("{name}: モックのデコードに失敗"));
        let re = serde_json::to_value(&message).unwrap();
        assert_eq!(&re, original, "{name}: モックのフレーム層で往復が保たれる");
    }

    // invalid_frames: モックのデコードが拒否する(契約と一致)。
    for case in fx["invalid_frames"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let payload = if let Some(raw) = case.get("raw_utf8").and_then(|v| v.as_str()) {
            raw.as_bytes().to_vec()
        } else {
            serde_json::to_vec(&case["message"]).unwrap()
        };
        assert!(
            mock_peer::decode_message(&payload).is_none(),
            "{name}: 不正フレームはモックでも拒否される"
        );
    }
}

//! US1 統合テスト(T033)
//!
//! PCP 疑似クライアント([`announcer::PcpClient`])+インプロセスモックピア
//! (gossip 契約参照実装 — `tests/common/mock_peer.rs`、共有フィクスチャ適用済み)で
//! announce → 署名済み EVENT 受信 → 詳細変更 → ended までの一連を実配線
//! (PCP serve → レジストリ → 掲載エンジン → gossip ハブ → 外向き P2P)で検証する。

use std::time::Duration;

use serde_json::Value;

use peca_p2p_yp::config::IndexEncoding;
use peca_p2p_yp::event::schema::{VerifyConfig, verify_incoming};
use peca_p2p_yp::yp::index_txt::generate;

#[path = "../common/mock_peer.rs"]
mod mock_peer;
#[path = "../common/announcer.rs"]
mod announcer;

use announcer::{AnnouncerNode, PcpClient};
use mock_peer::{MockPeer, TestNode, unix_now};

const CID: [u8; 16] = [0xA5; 16];

fn cid_hex() -> String {
    CID.iter().map(|b| format!("{b:02x}")).collect()
}

/// タグ `[name, value]` の値を JSON イベントから取り出す。
fn tag_value(event: &Value, name: &str) -> Option<String> {
    event["tags"].as_array()?.iter().find_map(|t| {
        let arr = t.as_array()?;
        (arr.first()?.as_str()? == name)
            .then(|| arr.get(1)?.as_str().map(str::to_string))
            .flatten()
    })
}

/// モックピアが述語を満たす EVENT を受信するまで最大 `timeout` 待つ。
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

/// 掲載側ノード+モックピアを established まで進める共通セットアップ。
async fn setup() -> (AnnouncerNode, MockPeer) {
    let mock = MockPeer::spawn().await;
    let node = AnnouncerNode::spawn(0xF00D).await;
    node.add_manual_peer(mock.addr());
    assert!(
        node.wait_established(Duration::from_secs(5)).await,
        "モックピアと established になるべき"
    );
    (node, mock)
}

/// シナリオ 1: 配信開始 → モックピアが検証可能な署名済みイベントを受信する。
#[tokio::test]
async fn announce_delivers_verifiable_signed_event() {
    let (node, mock) = setup().await;
    let mut client = PcpClient::connect(node.pcp_addr(), [0x01; 16]).await;
    client.broadcast(&CID, "統合テスト配信", "game", "説明").await;

    let event = wait_received(&mock, Duration::from_secs(10), |v| {
        tag_value(v, "d").as_deref() == Some(cid_hex().as_str())
            && tag_value(v, "status").as_deref() == Some("live")
    })
    .await
    .expect("60 秒以内(実際は数秒)に署名済みイベントを受信するべき");

    // 受信側と同じ検証パイプラインで「検証可能」であることを確かめる。
    let raw = serde_json::to_string(&event).unwrap();
    let verified = verify_incoming(&raw, &VerifyConfig::default(), unix_now())
        .expect("モックピアが受信したイベントは検証 1〜6 を通過するべき");
    assert_eq!(verified.listing.title, "統合テスト配信");
    assert_eq!(verified.listing.genre.as_deref(), Some("game"));
    assert_eq!(
        verified.event.pubkey.to_hex(),
        node.persona_pubkey,
        "署名鍵は選択中ペルソナのもの"
    );
    assert_eq!(
        verified.listing.tip.as_deref(),
        Some("198.51.100.1:7144"),
        "PCP_HOST 由来のトラッカーが tip に写像される"
    );

    // 自ノードの一覧にも掲載が反映される(EventStore 経由)。
    assert!(
        node.wait_until(Duration::from_secs(2), |rows| rows
            .iter()
            .any(|c| c.channel_id == cid_hex()))
            .await,
        "自ノードの一覧に掲載チャンネルが現れる"
    );
}

/// シナリオ 2: 詳細変更 → 変更内容を反映したイベントが届く。
#[tokio::test]
async fn detail_change_is_republished() {
    let (node, mock) = setup().await;
    let mut client = PcpClient::connect(node.pcp_addr(), [0x02; 16]).await;
    client.broadcast(&CID, "配信", "game", "説明").await;
    assert!(
        wait_received(&mock, Duration::from_secs(10), |v| {
            tag_value(v, "d").as_deref() == Some(cid_hex().as_str())
        })
        .await
        .is_some(),
        "初回イベントを受信"
    );

    // ジャンル・説明を変更した BCST(60 秒以内の反映 — 実際は即時再発行)。
    client.broadcast(&CID, "配信", "talk", "新しい説明").await;
    let updated = wait_received(&mock, Duration::from_secs(10), |v| {
        tag_value(v, "d").as_deref() == Some(cid_hex().as_str())
            && tag_value(v, "t").as_deref() == Some("talk")
    })
    .await
    .expect("変更内容(ジャンル talk)を反映したイベントを受信するべき");

    let raw = serde_json::to_string(&updated).unwrap();
    let verified = verify_incoming(&raw, &VerifyConfig::default(), unix_now()).unwrap();
    assert_eq!(verified.listing.summary.as_deref(), Some("新しい説明"));
    drop(node);
}

/// シナリオ 3: 配信終了 → status=ended の最終イベント発行+自ノード一覧から除去。
#[tokio::test]
async fn quit_publishes_ended_and_removes_from_local_list() {
    let (node, mock) = setup().await;
    let mut client = PcpClient::connect(node.pcp_addr(), [0x03; 16]).await;
    client.broadcast(&CID, "終了テスト", "game", "説明").await;
    assert!(
        node.wait_until(Duration::from_secs(10), |rows| rows
            .iter()
            .any(|c| c.channel_id == cid_hex()))
            .await,
        "掲載中は自ノードの一覧に現れる"
    );

    client.quit().await;

    let ended = wait_received(&mock, Duration::from_secs(10), |v| {
        tag_value(v, "d").as_deref() == Some(cid_hex().as_str())
            && tag_value(v, "status").as_deref() == Some("ended")
    })
    .await
    .expect("status=ended の最終イベントを受信するべき");
    let raw = serde_json::to_string(&ended).unwrap();
    assert!(
        verify_incoming(&raw, &VerifyConfig::default(), unix_now()).is_ok(),
        "ended イベントも検証可能な署名を持つ"
    );

    // 一覧から除去される(ended は tombstone として供給・表示から即時除外)。
    assert!(
        node.wait_until(Duration::from_secs(2), |rows| !rows
            .iter()
            .any(|c| c.channel_id == cid_hex()))
            .await,
        "終了後は自ノードの一覧から除去される"
    );
}

/// Phase 4 チェックポイント(SC-003): 掲載→伝搬→発見→視聴情報の一連が
/// **実ノード 2 つ**(掲載側 = P2P 待受つき、視聴側 = 外向きのみ)で成立する。
#[tokio::test]
async fn sc003_two_node_chain_from_announce_to_index_txt() {
    // 掲載側(P2P 待受あり)と視聴側(外向きのみ)。
    let announcer = AnnouncerNode::spawn_listening(0xAAAA).await;
    let viewer = TestNode::spawn(0xBBBB).await;
    viewer.add_manual_peer(announcer.p2p_addr().expect("待受つき起動"));
    assert!(
        announcer.wait_established(Duration::from_secs(5)).await,
        "視聴側からの接続で established になる"
    );

    // PeerCastStation 相当が掲載側へ announce する。
    let mut client = PcpClient::connect(announcer.pcp_addr(), [0x30; 16]).await;
    client
        .broadcast(&CID, "SC003 連鎖テスト", "game", "説明")
        .await;

    // gossip 伝搬で視聴側の一覧に現れる(SC-004 の 5 秒以内)。
    assert!(
        viewer
            .wait_for_channel(&cid_hex(), Duration::from_secs(5))
            .await,
        "掲載が gossip 経由で視聴側の一覧へ伝搬する"
    );

    // 視聴側の index.txt 実出力に、既存クライアントの視聴開始に必要な
    // チャンネル ID(大文字)と TIP が含まれる(contracts/http-yp.md)。
    let bytes = generate(&viewer.snapshot(), IndexEncoding::Utf8, unix_now());
    let text = String::from_utf8(bytes).expect("UTF-8 出力");
    assert!(
        text.contains(&cid_hex().to_uppercase()),
        "index.txt にチャンネル ID(大文字)が含まれる: {text}"
    );
    assert!(
        text.contains("198.51.100.1:7144"),
        "index.txt に TIP(トラッカー接続先)が含まれる: {text}"
    );
}

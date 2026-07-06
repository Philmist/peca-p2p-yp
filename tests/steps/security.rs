//! セキュリティシナリオのステップ定義(T055)
//!
//! spec.md セキュリティシナリオ 5 件 + quickstart 手順 7 の各項
//! (64KB 超フレーム / 16KB 超イベント / 署名不正 / PEX 不正アドレス)を検証する。
//!
//! - 過大イベント・署名不正・スパム・URL 警告はモックピア([`crate::mock_peer`])からの
//!   悪性入力注入で検証する(SC-005 / SC-007)
//! - 64KB 超フレームは待受ノードへの生 TCP クライアントで注入する(フレーム層は
//!   `write_frame` が送信側で過大を拒否するため、生バイト列でしか再現できない)
//! - ペルソナの切替と破棄はネットワーク非依存([`IdentityManager`] + 署名イベントの
//!   JSON 突合)で検証する(FR-013)

use std::sync::Arc;
use std::time::Duration;

use cucumber::{given, then, when};
use nostr::{Event, JsonUtil, Keys};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

use peca_p2p_yp::event::schema::{ChannelListing, ChannelStatus, Track};
use peca_p2p_yp::identity::{IdentityManager, Keystore};
use peca_p2p_yp::p2p::frame::{
    Hello, MAX_FRAME_PAYLOAD, Message, close_reason, read_frame, write_frame,
};
use peca_p2p_yp::p2p::session::PROTOCOL_VERSION;
use peca_p2p_yp::store::{MuteKind, Store};

use crate::AppWorld;
use crate::mock_peer::{MockPeer, TestNode, unix_now};

/// 正当な配信のチャンネル ID。
const CH_LEGIT: &str = "00000000000000000000000000000001";
/// なりすまし(署名不正)イベントのチャンネル ID。
const CH_FORGED: &str = "00000000000000000000000000000002";
/// 過大イベントのチャンネル ID。
const CH_OVERSIZE: &str = "00000000000000000000000000000003";
/// 危険 URL イベントのチャンネル ID。
const CH_URL: &str = "00000000000000000000000000000004";
/// ペルソナ A / B の掲載チャンネル ID。
const CH_PERSONA_A: &str = "00000000000000000000000000000005";
const CH_PERSONA_B: &str = "00000000000000000000000000000006";

/// スパム pubkey が保持できるイベント上限(EventStore の pubkey 単位クォータ — ADR-0004 §2)。
const PUBKEY_QUOTA: usize = 64;

// 接続確立・セキュリティイベント記録待ちは遅い CI ランナー(windows-latest)の
// オーバーヘッドを吸収できるよう余裕を持たせる。条件成立で即 return するため
// green run のコストは実質ゼロ。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const LOG_TIMEOUT: Duration = Duration::from_secs(15);

/// セキュリティシナリオ 1 個分の状態。
#[derive(Default)]
pub struct SecurityWorld {
    /// 悪性入力の注入元モックピア。
    mock: Option<MockPeer>,
    /// 再伝搬観測用の第 2 モックピア(なりすましシナリオ)。
    observer: Option<MockPeer>,
    /// 被検体ノード。
    node: Option<TestNode>,
    /// 正当な配信者の鍵。
    keys: Option<Keys>,
    /// 生 TCP クライアント(64KB 超フレームシナリオ)。
    raw_read: Option<OwnedReadHalf>,
    raw_write: Option<OwnedWriteHalf>,
    /// 受信した CLOSE の reason。
    close_reason: Option<String>,
    /// 一覧スナップショット(チャンネル一覧取得ステップの結果)。
    api_rows: Vec<Value>,
    /// スパム発行者の pubkey。
    spam_pubkey: Option<String>,
    /// ペルソナ管理(FR-013 シナリオ)。
    identity: Option<IdentityManager>,
    persona_a: Option<String>,
    persona_b: Option<String>,
    event_a: Option<Event>,
    event_b: Option<Event>,
}

impl std::fmt::Debug for SecurityWorld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecurityWorld")
            .field("has_mock", &self.mock.is_some())
            .field("has_node", &self.node.is_some())
            .field("close_reason", &self.close_reason)
            .finish()
    }
}

fn listing(channel_id: &str, title: &str, contact: Option<&str>) -> ChannelListing {
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
        contact: contact.map(str::to_string),
        relays: 0,
        track: Some(Track::default()),
    }
}

fn signed(keys: &Keys, channel_id: &str, title: &str, contact: Option<&str>) -> Event {
    listing(channel_id, title, contact)
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

/// World から security 状態を取り出す(未初期化なら生成する)。
fn ctx(world: &mut AppWorld) -> &mut SecurityWorld {
    world.security.get_or_insert_with(SecurityWorld::default)
}

/// モックピア 1 つと接続した被検体ノードを立ち上げる(established まで待つ)。
async fn node_with_mock(nonce: u64) -> (MockPeer, TestNode) {
    let mock = MockPeer::spawn().await;
    let node = TestNode::spawn(nonce).await;
    node.add_manual_peer(mock.addr());
    let ok = {
        let start = std::time::Instant::now();
        loop {
            if node.established_counts().1 >= 1 {
                break true;
            }
            if start.elapsed() >= CONNECT_TIMEOUT {
                break false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    assert!(ok, "モックピアと established になるべき");
    (mock, node)
}

// ---------------------------------------------------------------------------
// シナリオ: 過大なペイロードの拒否(16KB 超イベント → event_oversize)
// ---------------------------------------------------------------------------

#[given("ネットワークに悪意ある参加者が存在する")]
async fn malicious_participant_exists(world: &mut AppWorld) {
    let (mock, node) = node_with_mock(0x5EC0_0001).await;
    let c = ctx(world);
    c.keys = Some(Keys::generate());
    c.mock = Some(mock);
    c.node = Some(node);
}

#[when("上限を超えるサイズのチャンネル掲載データを受信した")]
async fn oversize_event_received(world: &mut AppWorld) {
    let c = ctx(world);
    let keys = c.keys.as_ref().unwrap();
    // 有効な 30311 イベントの content を 20KB に膨らませる(サイズ検証はパイプライン
    // 先頭のため、署名以前に event_oversize として拒否される — contracts/nostr-events.md 検証 1)。
    let mut value = serde_json::to_value(signed(keys, CH_OVERSIZE, "過大", None)).unwrap();
    value["content"] = Value::String("x".repeat(20 * 1024));
    c.mock.as_ref().unwrap().push_value(value);
}

#[then("そのデータは拒否されなければならない")]
async fn oversize_event_rejected(world: &mut AppWorld) {
    let node = ctx(world).node.as_ref().unwrap();
    // 拒否 = セキュリティイベントの記録を待ってから、一覧に現れないことを確認する。
    assert!(
        node.wait_for_security("event_oversize", LOG_TIMEOUT).await,
        "event_oversize が記録されるべき"
    );
    assert!(
        !node.snapshot().iter().any(|r| r.channel_id == CH_OVERSIZE),
        "過大イベントは一覧に現れてはならない(SC-007)"
    );
}

#[then("エラー応答は内部情報を漏洩してはならない")]
async fn no_internal_info_leak(world: &mut AppWorld) {
    let node = ctx(world).node.as_ref().unwrap();
    // セキュリティログの detail にパス・スタックトレース等の内部情報が含まれないこと
    // (Principle II — SecurityLog の契約)。
    for line in node.security_log_text().lines() {
        let entry: Value = serde_json::from_str(line).expect("ログは JSON Lines");
        let detail = entry["detail"].as_str().unwrap_or("");
        for needle in ["\\", "src/", ".rs", "panic", "backtrace"] {
            assert!(
                !detail.contains(needle),
                "detail に内部情報が含まれてはならない: {detail:?}"
            );
        }
    }
}

#[then("セキュリティイベントとしてログに記録されなければならない")]
async fn oversize_event_logged(world: &mut AppWorld) {
    let node = ctx(world).node.as_ref().unwrap();
    assert!(
        node.security_log_text().contains("event_oversize"),
        "event_oversize がセキュリティイベントとして記録されるべき(FR-007)"
    );
}

// ---------------------------------------------------------------------------
// シナリオ: 過大な P2P フレームの切断(64KB 超フレーム → p2p_oversize)
// ---------------------------------------------------------------------------

#[given("悪意あるピアが P2P 接続を established にしている")]
async fn malicious_peer_established(world: &mut AppWorld) {
    let node = TestNode::spawn_listening(0x5EC0_0002).await;
    let stream = TcpStream::connect(node.listen_addr())
        .await
        .expect("待受ノードへ接続");
    let (mut r, mut w) = stream.into_split();
    // 生クライアントとして HELLO → HELLO_ACK のハンドシェイクを行う。
    let hello = Message::Hello(Hello {
        version: PROTOCOL_VERSION,
        listen_port: 0,
        features: vec![],
        nonce: 0xBAD_5EED,
        ts: unix_now() as i64,
    });
    write_frame(&mut w, &hello).await.expect("HELLO 送信");
    let ack = read_frame(&mut r).await.expect("応答").expect("フレーム");
    assert!(
        matches!(ack.message, Message::HelloAck(_)),
        "HELLO_ACK で established になるべき: {:?}",
        ack.message
    );
    let c = ctx(world);
    c.node = Some(node);
    c.raw_read = Some(r);
    c.raw_write = Some(w);
}

#[when("悪意あるピアが 64KB を超える長さ前置フレームを送信した")]
async fn send_oversize_frame(world: &mut AppWorld) {
    let c = ctx(world);
    let w = c.raw_write.as_mut().unwrap();
    let over = (MAX_FRAME_PAYLOAD as u32 + 1).to_be_bytes();
    w.write_all(&over).await.expect("過大長の書き込み");
    w.flush().await.expect("flush");
}

#[then("接続は定型 reason の CLOSE で切断されなければならない")]
async fn connection_closed_with_fixed_reason(world: &mut AppWorld) {
    let c = ctx(world);
    let r = c.raw_read.as_mut().unwrap();
    // established 直後の SYNC_REQ / GET_PEERS 等を読み飛ばし、CLOSE か EOF まで読む。
    let deadline = tokio::time::Instant::now() + LOG_TIMEOUT;
    loop {
        let frame = tokio::time::timeout_at(deadline, read_frame(r))
            .await
            .expect("CLOSE または EOF が届くべき");
        match frame {
            Ok(Some(f)) => {
                if let Message::Close { reason } = f.message {
                    c.close_reason = Some(reason);
                    break;
                }
            }
            // EOF・I/O エラー = 切断。
            _ => break,
        }
    }
    if let Some(reason) = &c.close_reason {
        assert_eq!(
            reason,
            close_reason::OVERSIZE,
            "CLOSE reason は定型コード oversize であるべき(内部情報を含めない)"
        );
    }
    // CLOSE 後は EOF で閉じられる。
    let end = tokio::time::timeout(LOG_TIMEOUT, read_frame(r)).await;
    assert!(
        matches!(end, Ok(Ok(None)) | Ok(Err(_))),
        "CLOSE 後に接続が閉じられるべき"
    );
}

#[then("フレーム長超過がセキュリティイベントとして記録されなければならない")]
async fn oversize_frame_logged(world: &mut AppWorld) {
    let node = ctx(world).node.as_ref().unwrap();
    assert!(
        node.wait_for_security("p2p_oversize", LOG_TIMEOUT).await,
        "p2p_oversize が記録されるべき(SC-007)"
    );
}

// ---------------------------------------------------------------------------
// シナリオ: なりすまし掲載の検出(署名不正 → event_invalid_sig)
// ---------------------------------------------------------------------------

#[given("配信者Aのチャンネルが掲載されている")]
async fn broadcaster_a_listed(world: &mut AppWorld) {
    let (mock, node) = node_with_mock(0x5EC0_0003).await;
    // 再伝搬の観測用に第 2 のモックピアも接続する。
    let observer = MockPeer::spawn().await;
    node.add_manual_peer(observer.addr());
    let keys = Keys::generate();
    mock.push_signed(&signed(&keys, CH_LEGIT, "配信A", None));
    assert!(
        node.wait_for_channel(CH_LEGIT, CONNECT_TIMEOUT).await,
        "配信者Aのチャンネルが一覧に掲載されるべき"
    );
    // 観測ピアも established になるまで待つ(未検証候補の接続は 1 件/秒スロットル)。
    let start = std::time::Instant::now();
    while node.established_counts().1 < 2 {
        assert!(
            start.elapsed() < CONNECT_TIMEOUT,
            "観測ピアとも established になるべき"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let c = ctx(world);
    c.keys = Some(keys);
    c.mock = Some(mock);
    c.observer = Some(observer);
    c.node = Some(node);
}

#[when("第三者が配信者Aを騙る検証不能なチャンネル情報を流通させた")]
async fn forged_event_circulated(world: &mut AppWorld) {
    let c = ctx(world);
    let keys = c.keys.as_ref().unwrap();
    // 配信者Aの署名済みイベントのタグを改ざんする(pubkey は A のまま、id/sig 不整合)。
    let mut value = serde_json::to_value(signed(keys, CH_FORGED, "本物の配信", None)).unwrap();
    for tag in value["tags"].as_array_mut().unwrap() {
        let arr = tag.as_array_mut().unwrap();
        if arr.first().and_then(Value::as_str) == Some("title") {
            arr[1] = Value::String("偽の配信".into());
        }
    }
    c.mock.as_ref().unwrap().push_value(value);
}

#[then("検証に失敗した情報は一覧に表示されてはならない")]
async fn forged_event_not_listed(world: &mut AppWorld) {
    let node = ctx(world).node.as_ref().unwrap();
    // 拒否の証拠(セキュリティイベント)を待ってから一覧の不在を確認する。
    assert!(
        node.wait_for_security("event_invalid_sig", LOG_TIMEOUT)
            .await,
        "署名検証失敗が記録されるべき"
    );
    let rows = node.snapshot();
    assert!(
        !rows.iter().any(|r| r.channel_id == CH_FORGED),
        "検証失敗イベントは一覧に現れてはならない(SC-005)"
    );
    assert!(
        rows.iter()
            .any(|r| r.channel_id == CH_LEGIT && r.listing.title == "配信A"),
        "正当なチャンネルは表示され続けるべき"
    );
}

#[then("検証失敗はセキュリティイベントとして記録されなければならない")]
async fn forgery_logged(world: &mut AppWorld) {
    let node = ctx(world).node.as_ref().unwrap();
    assert!(
        node.security_log_text().contains("event_invalid_sig"),
        "event_invalid_sig が記録されるべき(FR-005 / FR-007)"
    );
}

#[then("検証に失敗した情報は他のピアへ再伝搬されてはならない")]
async fn forged_event_not_repropagated(world: &mut AppWorld) {
    // 拒否済み(前ステップで確認)の後、観測ピアへ届いていないことを確認する。
    tokio::time::sleep(Duration::from_millis(300)).await;
    let c = ctx(world);
    let observer = c.observer.as_ref().unwrap();
    assert!(
        !observer
            .received()
            .iter()
            .any(|v| d_tag(v).as_deref() == Some(CH_FORGED)),
        "検証失敗イベントは再伝搬されてはならない(伝搬規則 — 格納成功のみ再伝搬)"
    );
}

// ---------------------------------------------------------------------------
// シナリオ: 大量の偽チャンネル登録への耐性(FR-008)
// ---------------------------------------------------------------------------

#[given("悪意ある参加者が短時間に大量の偽チャンネルを掲載した")]
async fn mass_fake_channels_published(world: &mut AppWorld) {
    let (mock, node) = node_with_mock(0x5EC0_0004).await;
    let legit = Keys::generate();
    let spam = Keys::generate();
    mock.push_signed(&signed(&legit, CH_LEGIT, "正当な配信", None));
    // スパム 70 件(単一 pubkey)。受信レート上限(200 msg/秒)未満で注入する。
    for i in 0..70u32 {
        let ch = format!("{:032x}", 0xF000_0000u64 + u64::from(i));
        mock.push_signed(&signed(&spam, &ch, &format!("偽チャンネル{i}"), None));
    }
    assert!(
        node.wait_for_channel(CH_LEGIT, CONNECT_TIMEOUT).await,
        "正当なチャンネルが一覧に反映されるべき"
    );
    let spam_pk = spam.public_key().to_hex();
    {
        let pk = spam_pk.clone();
        let ok = node
            .wait_until(CONNECT_TIMEOUT, move |rows| {
                rows.iter().filter(|r| r.author_pubkey == pk).count() >= 1
            })
            .await;
        assert!(ok, "スパムイベントも(既定オープン型のため)一覧に入る");
    }
    let c = ctx(world);
    c.mock = Some(mock);
    c.node = Some(node);
    c.spam_pubkey = Some(spam_pk);
}

#[when("視聴者がチャンネル一覧を取得する")]
async fn viewer_gets_list(world: &mut AppWorld) {
    // 注入が落ち着くまで待ってからスナップショットを取る。
    tokio::time::sleep(Duration::from_millis(500)).await;
    let c = ctx(world);
    c.api_rows = c
        .node
        .as_ref()
        .unwrap()
        .snapshot()
        .iter()
        .map(|r| {
            serde_json::json!({
                "channel_id": r.channel_id,
                "author_pubkey": r.author_pubkey,
                "title": r.listing.title,
            })
        })
        .collect();
}

#[then("利用者は緩和策により正当なチャンネルを識別・閲覧し続けられる")]
async fn mitigations_keep_list_usable(world: &mut AppWorld) {
    let c = ctx(world);
    let spam_pk = c.spam_pubkey.clone().unwrap();
    // 第 1 の緩和(自動): pubkey 単位クォータにより単一発行者の占有は 64 件以下(ADR-0004 §2)。
    let spam_rows = c
        .api_rows
        .iter()
        .filter(|r| r["author_pubkey"] == spam_pk.as_str())
        .count();
    assert!(
        spam_rows <= PUBKEY_QUOTA,
        "単一 pubkey の掲載はクォータ({PUBKEY_QUOTA})以下に抑制される: {spam_rows}"
    );
    assert!(
        c.api_rows
            .iter()
            .any(|r| r["channel_id"] == CH_LEGIT && r["title"] == "正当な配信"),
        "正当なチャンネルはスパム流入下でも閲覧できる"
    );
    // 第 2 の緩和(利用者操作): 発行者ミュートでスパムを一括非表示にできる(FR-008)。
    let node = c.node.as_ref().unwrap();
    node.store()
        .insert_mute(MuteKind::Pubkey, &spam_pk)
        .expect("ミュート登録");
    let rows = node.snapshot();
    assert!(
        !rows.iter().any(|r| r.author_pubkey == spam_pk),
        "ミュート後はスパム発行者の全チャンネルが非表示になる"
    );
    assert!(
        rows.iter().any(|r| r.channel_id == CH_LEGIT),
        "ミュート後も正当なチャンネルは表示され続ける"
    );
}

// ---------------------------------------------------------------------------
// シナリオ: 危険なコンタクト URL の警告(FR-012)
// ---------------------------------------------------------------------------

#[given("コンタクト URL のスキームが http/https 以外の掲載イベントが流通している")]
async fn dangerous_url_event_circulating(world: &mut AppWorld) {
    let (mock, node) = node_with_mock(0x5EC0_0005).await;
    let keys = Keys::generate();
    mock.push_signed(&signed(
        &keys,
        CH_URL,
        "危険URL配信",
        Some("javascript:alert(1)"),
    ));
    assert!(
        node.wait_for_channel(CH_URL, CONNECT_TIMEOUT).await,
        "イベントが一覧に反映されるべき(既定オープン型)"
    );
    let c = ctx(world);
    c.mock = Some(mock);
    c.node = Some(node);
}

#[when("視聴者がチャンネル一覧を表示する")]
async fn viewer_views_channels_api(world: &mut AppWorld) {
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::{Method, Request, header};
    use peca_p2p_yp::web::{self, AppState, RateLimiter};
    use tower::ServiceExt;

    // 実 API(GET /api/v1/channels)を hub を供給元として実行する(T041 の実配線)。
    let c = ctx(world);
    let hub = Arc::clone(c.node.as_ref().unwrap().hub());
    let store = Arc::new(Store::open_in_memory().unwrap());
    let dir = tempfile::tempdir().unwrap();
    let security =
        Arc::new(peca_p2p_yp::security::SecurityLog::new(dir.path().join("s.log")).unwrap());
    let mut hosts = std::collections::HashSet::new();
    hosts.insert("127.0.0.1:7180".to_string());
    let limiter = RateLimiter::with_clock(web::RATE_LIMIT_PER_SEC, Box::new(|| 1_000));
    let state =
        AppState::with_parts(store, security, "test-token", hosts, limiter).with_directory(hub);
    let app = web::build_router(state);
    let mut req = Request::builder()
        .method(Method::GET)
        .uri("/api/v1/channels")
        .header(header::HOST, "127.0.0.1:7180")
        .body(Body::empty())
        .unwrap();
    let addr: std::net::SocketAddr = "127.0.0.1:50000".parse().unwrap();
    req.extensions_mut().insert(ConnectInfo(addr));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let rows: Vec<Value> = serde_json::from_slice(&bytes).unwrap();
    c.api_rows = rows;
}

#[then("当該チャンネルのコンタクト URL には警告が表示されなければならない")]
async fn url_warning_flag_present(world: &mut AppWorld) {
    let c = ctx(world);
    let row = c
        .api_rows
        .iter()
        .find(|r| r["channel_id"] == CH_URL)
        .expect("当該チャンネルが一覧に存在する");
    assert_eq!(row["contact_url"], "javascript:alert(1)");
    assert_eq!(
        row["url_warning"], true,
        "http/https 以外の URL には警告フラグが付与される(FR-012)"
    );
    // ネットワーク境界での警告判定の発動も記録される(data-model §SecurityEvent url_warning)。
    let node = c.node.as_ref().unwrap();
    assert!(
        node.wait_for_security("url_warning", LOG_TIMEOUT).await,
        "url_warning がセキュリティイベントとして記録されるべき"
    );
}

#[then("リンクは利用者の明示操作なしに開かれてはならない")]
async fn link_requires_explicit_action(_world: &mut AppWorld) {
    // UI 契約の検証: 警告付き URL は直接リンク(<a href>)ではなく確認ボタン+
    // 確認ダイアログを経由してのみ開かれる(ui/channels.html — FR-012)。
    let html = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/channels.html"))
        .expect("ui/channels.html を読めるべき");
    assert!(
        html.contains("if (ch.url_warning)"),
        "UI は url_warning フラグで表示を分岐するべき"
    );
    assert!(
        html.contains("open-url-btn") && html.contains("confirm-dialog"),
        "警告付き URL は確認ボタン+確認ダイアログ経由でのみ開かれるべき"
    );
}

// ---------------------------------------------------------------------------
// シナリオ: ペルソナの切替と破棄(FR-013)
// ---------------------------------------------------------------------------

#[given("利用者がペルソナAでチャンネルを掲載している")]
async fn persona_a_publishes(world: &mut AppWorld) {
    let manager = IdentityManager::new(
        Arc::new(Store::open_in_memory().unwrap()),
        Keystore::ephemeral(),
    );
    let a = manager.create("らべるA").expect("ペルソナA作成");
    let keys = manager.signing_keys(&a.pubkey).expect("署名鍵");
    let event = signed(&keys, CH_PERSONA_A, "ペルソナAの配信", None);
    let c = ctx(world);
    c.identity = Some(manager);
    c.persona_a = Some(a.pubkey);
    c.event_a = Some(event);
}

#[when("利用者が新しいペルソナBを作成して切り替え、別のチャンネルを掲載する")]
async fn persona_b_created_and_publishes(world: &mut AppWorld) {
    let c = ctx(world);
    let manager = c.identity.as_ref().unwrap();
    let b = manager.create("らべるB").expect("ペルソナB作成");
    manager.select(&b.pubkey).expect("ペルソナBへ切替");
    let keys = manager.signing_keys(&b.pubkey).expect("署名鍵");
    let event = signed(&keys, CH_PERSONA_B, "ペルソナBの配信", None);
    c.persona_b = Some(b.pubkey);
    c.event_b = Some(event);
}

#[then("2つの掲載イベントには両ペルソナを相互に紐づける情報が含まれてはならない")]
async fn events_do_not_link_personas(world: &mut AppWorld) {
    let c = ctx(world);
    let pk_a = c.persona_a.as_ref().unwrap();
    let pk_b = c.persona_b.as_ref().unwrap();
    let json_a = c.event_a.as_ref().unwrap().as_json();
    let json_b = c.event_b.as_ref().unwrap().as_json();
    assert_ne!(pk_a, pk_b, "ペルソナは独立の鍵ペアであるべき");
    assert!(
        !json_a.contains(pk_b.as_str()),
        "ペルソナAのイベントにペルソナBの識別子が含まれてはならない"
    );
    assert!(
        !json_b.contains(pk_a.as_str()),
        "ペルソナBのイベントにペルソナAの識別子が含まれてはならない"
    );
    // ローカル表示名(label)はネットワークに出ない(FR-013)。
    for (json, label) in [(&json_a, "らべるA"), (&json_b, "らべるB")] {
        assert!(
            !json.contains(label),
            "ローカル表示名はイベントに含まれてはならない: {label}"
        );
    }
}

#[then("ペルソナAを破棄した後、ペルソナAの秘密鍵は本ソフトウェアから復元できない")]
async fn persona_a_destroyed_irreversibly(world: &mut AppWorld) {
    let c = ctx(world);
    let manager = c.identity.as_ref().unwrap();
    let pk_a = c.persona_a.clone().unwrap();
    // 破棄前はエクスポート可能(前提の確認)。
    assert!(manager.export_nsec(&pk_a).is_ok());
    manager.delete(&pk_a).expect("ペルソナA破棄");
    // 破棄 = 行削除。以後は署名鍵もエクスポートも得られない(復元不可 — ADR-0003 §3)。
    assert!(
        manager.signing_keys(&pk_a).is_err(),
        "破棄後に署名鍵を復元できてはならない"
    );
    assert!(
        manager.export_nsec(&pk_a).is_err(),
        "破棄後に nsec をエクスポートできてはならない"
    );
    assert!(
        !manager.list().unwrap().iter().any(|p| p.pubkey == pk_a),
        "破棄済みペルソナは一覧から消える"
    );
}

// ---------------------------------------------------------------------------
// シナリオ: ピア交換で受信した不正アドレスの破棄(FR-015)
// ---------------------------------------------------------------------------

/// PEX 応答に混ぜる正当なアドレス(TEST-NET-3 — 実接続は成立しない)。
const PEX_VALID: &str = "203.0.113.5:7147";
/// 不正アドレス: ポート 0(形式不正)・ブラケットなし複数コロン IPv6(パース不能)。
const PEX_BAD_PORT: &str = "203.0.113.9:0";
const PEX_BAD_V6: &str = "2001:db8::1:7147";

#[given("ピア交換で不正なアドレスを応答するモックピアが存在する")]
async fn mock_with_bad_pex(world: &mut AppWorld) {
    let mock = MockPeer::spawn().await;
    mock.share_peer(PEX_BAD_PORT);
    mock.share_peer(PEX_BAD_V6);
    mock.share_peer(&format!("{}:7147", "a".repeat(300))); // 長さ > 256
    mock.share_peer(PEX_VALID);
    ctx(world).mock = Some(mock);
}

#[when("本ソフトウェアがそのモックピアとピア交換を行う")]
async fn node_performs_pex(world: &mut AppWorld) {
    let addr = ctx(world).mock.as_ref().unwrap().addr().to_string();
    let node = TestNode::spawn(0x5EC0_0006).await;
    // established 直後に GET_PEERS が自動送信され、モックが上記 PEERS を返す。
    node.add_manual_peer(&addr);
    ctx(world).node = Some(node);
}

#[then("不正なアドレスは破棄されセキュリティイベントとして記録されなければならない")]
async fn bad_pex_rejected_and_logged(world: &mut AppWorld) {
    let node = ctx(world).node.as_ref().unwrap();
    assert!(
        node.wait_for_security("pex_rejected", CONNECT_TIMEOUT)
            .await,
        "pex_rejected が記録されるべき(FR-015)"
    );
    let known: Vec<String> = node.known_peers().iter().map(|p| p.addr.clone()).collect();
    assert!(
        !known
            .iter()
            .any(|a| a == PEX_BAD_PORT || a.contains("2001:db8::1:7147") || a.len() > 256),
        "不正アドレスは接続候補に登録されてはならない: {known:?}"
    );
}

#[then("正当なアドレスのみが接続候補に登録される")]
async fn valid_pex_registered(world: &mut AppWorld) {
    let node = ctx(world).node.as_ref().unwrap();
    assert!(
        node.wait_for_peer(PEX_VALID, CONNECT_TIMEOUT).await,
        "正当なアドレスは source=pex の候補として登録されるべき"
    );
}

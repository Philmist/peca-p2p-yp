//! 掲載前のペルソナ選択と配信中ロックのステップ定義(T012 / T021)
//!
//! ネットワーク非依存で、実 API(`build_router` の `PUT/DELETE /personas`・`GET /status`)を
//! 直接叩いて検証する:
//! - US1: active+usable の選択(204)/ archived の選択拒否(409 persona_not_selectable)
//! - US2: 配信中の切替/破棄/アーカイブ拒否(409 broadcasting_locked)、label と他ペルソナ操作の
//!   許可、停止後の解錠、古い画面状態からの制限操作 → 409 + `GET /status` の broadcasting=true
//!
//! 「配信中」は掲載エンジン([`PublishEngine::publish_listing`])が selected ペルソナの署名で
//! チャンネルを発行し、共有 [`BroadcastState`] へ予約されることで成立させる(ADR-0011 予約先行)。

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, header};
use cucumber::{given, then, when};
use serde_json::Value;
use tower::ServiceExt;

use peca_p2p_yp::broadcast::BroadcastState;
use peca_p2p_yp::event::publish::{EventSink, PublishEngine};
use peca_p2p_yp::event::schema::{ChannelListing, ChannelStatus, Track};
use peca_p2p_yp::identity::{IdentityManager, Keystore};
use peca_p2p_yp::security::SecurityLog;
use peca_p2p_yp::store::{PersonaState, Store};
use peca_p2p_yp::web::{self, AppState, RateLimiter};

use crate::AppWorld;

const HOST: &str = "127.0.0.1:7180";
const TOKEN: &str = "test-token";
/// 発行に使うチャンネル ID(hex32 小文字)。
const CH: &str = "000000000000000000000000000000aa";

/// 発行イベントを捨てるだけの sink(発行の成否のみが本ステップの関心事)。
struct NullSink;
impl EventSink for NullSink {
    fn publish_local(&self, _event: nostr::Event) -> bool {
        true
    }
}

/// 1 シナリオ分の状態。identity・engine・AppState は同一の共有 Arc を用いる。
pub struct PersonaSelectionWorld {
    store: Arc<Store>,
    identity: Arc<IdentityManager>,
    broadcast: Arc<BroadcastState>,
    engine: Arc<PublishEngine>,
    security: Arc<SecurityLog>,
    /// ラベル(A/B/…)→ pubkey。
    personas: HashMap<String, String>,
    status: u16,
    body: Value,
}

impl std::fmt::Debug for PersonaSelectionWorld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PersonaSelectionWorld")
            .field("personas", &self.personas.keys().collect::<Vec<_>>())
            .field("status", &self.status)
            .finish()
    }
}

impl PersonaSelectionWorld {
    fn new() -> Self {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let broadcast = Arc::new(BroadcastState::new());
        let identity = Arc::new(
            IdentityManager::new(Arc::clone(&store), Keystore::ephemeral())
                .with_broadcast_state(Arc::clone(&broadcast)),
        );
        let sink: Arc<dyn EventSink> = Arc::new(NullSink);
        let engine = Arc::new(PublishEngine::new(
            Arc::clone(&identity),
            sink,
            60,
            Arc::clone(&broadcast),
        ));
        let dir = tempfile::tempdir().unwrap();
        let security = Arc::new(SecurityLog::new(dir.path().join("s.log")).unwrap());
        std::mem::forget(dir);
        Self {
            store,
            identity,
            broadcast,
            engine,
            security,
            personas: HashMap::new(),
            status: 0,
            body: Value::Null,
        }
    }

    fn create(&mut self, label: &str) -> String {
        let p = self.identity.create(label).expect("ペルソナ作成");
        self.personas.insert(label.to_string(), p.pubkey.clone());
        p.pubkey
    }

    fn pk(&self, label: &str) -> String {
        self.personas
            .get(label)
            .cloned()
            .unwrap_or_else(|| panic!("未作成のペルソナ: {label}"))
    }

    /// selected ペルソナで 1 チャンネルを発行し、配信中集合へ予約させる。
    fn start_broadcast(&self) {
        let published = self
            .engine
            .publish_listing(&listing())
            .expect("発行に失敗しない");
        assert!(published, "selected があるので発行される");
        assert!(self.broadcast.is_broadcasting(), "発行後は配信中");
    }
}

fn listing() -> ChannelListing {
    ChannelListing {
        channel_id: CH.into(),
        title: "配信".into(),
        summary: None,
        genre: Some("game".into()),
        status: ChannelStatus::Live,
        starts: 1_700_000_000,
        current_participants: 1,
        streaming: None,
        bitrate_kbps: Some(500),
        content_type: Some("FLV".into()),
        tip: Some("198.51.100.1:7144".into()),
        contact: None,
        relays: 0,
        track: Some(Track::default()),
    }
}

fn ctx(world: &mut AppWorld) -> &mut PersonaSelectionWorld {
    world
        .persona_selection
        .as_mut()
        .expect("Background で環境を初期化しているべき")
}

/// 実 API を 1 回叩き、ステータスとボディを World に記録する。
async fn send(c: &mut PersonaSelectionWorld, method: Method, uri: &str, token: bool, body: Body) {
    let mut hosts = HashSet::new();
    hosts.insert(HOST.to_string());
    let limiter = RateLimiter::with_clock(web::RATE_LIMIT_PER_SEC, Box::new(|| 1_000));
    let state = AppState::with_parts(
        Arc::clone(&c.store),
        Arc::clone(&c.security),
        TOKEN,
        hosts,
        limiter,
    )
    .with_identity(Arc::clone(&c.identity))
    .with_broadcast(Arc::clone(&c.broadcast));
    let app = web::build_router(state);

    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, HOST);
    if token {
        b = b.header("X-Api-Token", TOKEN);
    }
    let mut req = b.body(body).unwrap();
    let addr: SocketAddr = "127.0.0.1:50000".parse().unwrap();
    req.extensions_mut().insert(ConnectInfo(addr));

    let resp = app.oneshot(req).await.unwrap();
    c.status = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    c.body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
}

// ---------------------------------------------------------------------------
// Given
// ---------------------------------------------------------------------------

#[given("ペルソナ選択のテスト環境を初期化する")]
async fn init_env(world: &mut AppWorld) {
    world.persona_selection = Some(PersonaSelectionWorld::new());
}

#[given(expr = "active かつ usable なペルソナ {string} と {string} が存在し、何も発行していない")]
async fn two_active_personas(world: &mut AppWorld, a: String, b: String) {
    let c = ctx(world);
    c.create(&a); // 最初の作成で自動選択される
    c.create(&b);
    assert!(!c.broadcast.is_broadcasting());
}

#[given(expr = "archived なペルソナ {string} が存在する")]
async fn archived_persona(world: &mut AppWorld, d: String) {
    let c = ctx(world);
    let pk = c.create(&d);
    c.identity
        .set_state(&pk, PersonaState::Archived)
        .expect("非配信中はアーカイブできる");
}

#[given(expr = "ペルソナ {string} が選択中で発行中、別ペルソナ {string} が存在する")]
async fn selected_and_broadcasting(world: &mut AppWorld, selected: String, other: String) {
    let c = ctx(world);
    c.create(&selected); // 自動選択(selected)
    c.create(&other);
    c.start_broadcast();
}

// ---------------------------------------------------------------------------
// When
// ---------------------------------------------------------------------------

#[when(expr = "ペルソナ {string} を選択する API を送る")]
async fn select_api(world: &mut AppWorld, label: String) {
    let c = ctx(world);
    let uri = format!("/api/v1/personas/{}", c.pk(&label));
    send(c, Method::PUT, &uri, true, Body::from(r#"{"select":true}"#)).await;
}

#[when(expr = "ペルソナ {string} をアーカイブする API を送る")]
async fn archive_api(world: &mut AppWorld, label: String) {
    let c = ctx(world);
    let uri = format!("/api/v1/personas/{}", c.pk(&label));
    send(
        c,
        Method::PUT,
        &uri,
        true,
        Body::from(r#"{"state":"archived"}"#),
    )
    .await;
}

#[when(expr = "ペルソナ {string} を破棄する API を送る")]
async fn delete_api(world: &mut AppWorld, label: String) {
    let c = ctx(world);
    let uri = format!("/api/v1/personas/{}?confirm=true", c.pk(&label));
    send(c, Method::DELETE, &uri, true, Body::empty()).await;
}

#[when(expr = "ペルソナ {string} の label を {string} に変更する API を送る")]
async fn set_label_api(world: &mut AppWorld, label: String, new_label: String) {
    let c = ctx(world);
    let uri = format!("/api/v1/personas/{}", c.pk(&label));
    let body = serde_json::json!({ "label": new_label }).to_string();
    send(c, Method::PUT, &uri, true, Body::from(body)).await;
}

#[when("すべてのチャンネルが終了する")]
async fn all_channels_ended(world: &mut AppWorld) {
    let c = ctx(world);
    c.engine.publish_ended(&listing()).expect("終了発行");
    assert!(!c.broadcast.is_broadcasting(), "終了後は非配信中(解錠)");
}

// ---------------------------------------------------------------------------
// Then
// ---------------------------------------------------------------------------

#[then(expr = "ステータス {int} が返る")]
async fn status_is(world: &mut AppWorld, code: u16) {
    let c = ctx(world);
    assert_eq!(c.status, code, "ボディ: {}", c.body);
}

#[then(expr = "ステータス {int} とエラーコード {string} が返る")]
async fn status_and_error(world: &mut AppWorld, code: u16, err: String) {
    let c = ctx(world);
    assert_eq!(c.status, code, "ボディ: {}", c.body);
    assert_eq!(c.body["error"], Value::String(err));
    // 内部情報を漏らさない: error キーのみ(Principle II)。
    assert_eq!(c.body.as_object().map(|o| o.len()), Some(1));
}

#[then(expr = "選択中ペルソナは {string} である")]
async fn selected_is(world: &mut AppWorld, label: String) {
    let c = ctx(world);
    let expected = c.pk(&label);
    assert_eq!(
        c.identity.selected().unwrap(),
        Some(expected),
        "選択中ペルソナが一致するべき"
    );
}

#[then(expr = "GET status の broadcasting は {string} である")]
async fn status_broadcasting_is(world: &mut AppWorld, expected: String) {
    let c = ctx(world);
    send(c, Method::GET, "/api/v1/status", false, Body::empty()).await;
    assert_eq!(c.status, 200);
    let want = expected == "true";
    assert_eq!(c.body["broadcasting"], Value::Bool(want));
}

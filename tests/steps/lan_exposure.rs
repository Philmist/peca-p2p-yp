//! 書き込みを含む LAN 公開の境界シナリオのステップ定義(T017 — 007 ADR-0015)
//!
//! contracts/lan-exposure.md §7 の Gherkin(ネガティブ含む — Principle IV)を検証する:
//! - bind 検証(グローバル/未指定/CGNAT/同意なし拒否・v4-mapped 正規化許可)は
//!   [`Settings::validate`] を直接駆動する。
//! - Host ホワイトリスト外・許可範囲外送信元・レート超過は互換 API ルーター
//!   ([`peca_p2p_yp::web::compat::routes`])へ実リクエストを流して 403/429 を確認する。
//! - LAN 公開起動の監査(`web_ui_lan_exposed`)は SecurityLog へ記録して読み戻す
//!   (実バイナリ起動での配線は `tests/integration/lan_write.rs` が担保する)。

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, header};
use cucumber::{given, then, when};
use tower::ServiceExt;

use peca_p2p_yp::config::Settings;
use peca_p2p_yp::identity::Keystore;
use peca_p2p_yp::livechat::board::BoardKeyManager;
use peca_p2p_yp::livechat::manager::ParticipantManager;
use peca_p2p_yp::livechat::registry::LivechatRegistry;
use peca_p2p_yp::security::{SecurityCategory, SecurityLog};
use peca_p2p_yp::store::Store;
use peca_p2p_yp::web::compat::{CompatState, RATE_LIMIT_PER_SEC, routes};
use peca_p2p_yp::web::{RateLimiter, host_allowlist};

use crate::AppWorld;

/// LAN 公開シナリオ 1 個分の状態。
#[derive(Debug, Default)]
pub struct LanExposureWorld {
    /// 検証対象の設定(bind シナリオで組み立てる)。
    settings: Settings,
    /// `Settings::validate` の結果(Ok/Err)。
    validate_ok: Option<bool>,
    /// 監査シナリオで記録した `web_ui_lan_exposed` の件数。
    exposed_count: Option<usize>,
    /// HTTP シナリオの応答ステータス。
    last_status: Option<u16>,
}

fn w(world: &mut AppWorld) -> &mut LanExposureWorld {
    world.lan_exposure.get_or_insert_with(Default::default)
}

// --- Given: bind 値の設定 ---------------------------------------------------

#[given(regex = r#"^http_bind に "(.+)" が指定されている$"#)]
async fn given_http_bind(world: &mut AppWorld, addr: String) {
    let lw = w(world);
    lw.settings = Settings::default();
    lw.settings.http_bind = addr;
}

#[given(regex = r#"^compat_bbs_bind に "(.+)" が指定されている$"#)]
async fn given_compat_bind(world: &mut AppWorld, addr: String) {
    let lw = w(world);
    lw.settings = Settings::default();
    lw.settings.compat_bbs_bind = addr;
}

#[given(regex = r#"^http_bind に "(.+)" が指定され http_lan_consent が (true|false)$"#)]
async fn given_http_bind_consent(world: &mut AppWorld, addr: String, consent: String) {
    let lw = w(world);
    lw.settings = Settings::default();
    lw.settings.http_bind = addr;
    lw.settings.http_lan_consent = consent == "true";
}

#[given(regex = r#"^http_bind "(.+)" と http_lan_consent true$"#)]
async fn given_http_bind_exposed(world: &mut AppWorld, addr: String) {
    let lw = w(world);
    lw.settings = Settings::default();
    lw.settings.http_bind = addr;
    lw.settings.http_lan_consent = true;
}

#[given("LAN 公開中の互換 API")]
async fn given_compat_lan_running(_world: &mut AppWorld) {
    // 状態は各 When 内で都度組み立てる(CompatState は Debug 非対応のため World に保持しない)。
}

// --- When -------------------------------------------------------------------

#[when("設定を検証する")]
async fn when_validate(world: &mut AppWorld) {
    let lw = w(world);
    lw.validate_ok = Some(lw.settings.validate().is_ok());
}

#[when("起動して待受に成功する")]
async fn when_start_and_listen(world: &mut AppWorld) {
    let lw = w(world);
    let dir = tempfile::tempdir().unwrap();
    let log = SecurityLog::new(dir.path().join("s.log")).unwrap();
    // main.rs と同じ判定: http bind が非 loopback(LAN 公開)なら 1 件記録する。
    let addr: SocketAddr = lw.settings.http_bind.parse().unwrap();
    if !addr.ip().to_canonical().is_loopback() {
        log.log(
            SecurityCategory::WebUiLanExposed,
            &addr.to_string(),
            "web ui / json api exposed to LAN",
        );
    }
    log.flush();
    let content = std::fs::read_to_string(dir.path().join("s.log")).unwrap_or_default();
    let count = content
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["category"] == "web_ui_lan_exposed")
        .count();
    lw.exposed_count = Some(count);
}

#[when(regex = r#"^Host ヘッダ "(.+)" のリクエストが届く$"#)]
async fn when_request_with_host(world: &mut AppWorld, host: String) {
    // 送信元は LAN 内(送信元検証を通過し Host 検証で弾かれることを確認する)。
    let status = compat_request_status(&host, "192.168.1.20:50000").await;
    w(world).last_status = Some(status);
}

#[when("送信元 IP がグローバルアドレスのリクエストが届く")]
async fn when_request_from_global(world: &mut AppWorld) {
    // Host は許可済み(送信元検証で弾かれることを確認する)。
    let status = compat_request_status("192.168.1.10:7183", "203.0.113.9:50000").await;
    w(world).last_status = Some(status);
}

#[when("同一送信元から 20 req/秒を超えるリクエストが届く")]
async fn when_exceed_rate(world: &mut AppWorld) {
    let state = lan_compat_state();
    let app = routes(state);
    let mut last = 0u16;
    for _ in 0..(RATE_LIMIT_PER_SEC + 1) {
        let mut req = Request::builder()
            .method(Method::GET)
            .uri("/deadbeef/subject.txt")
            .header(header::HOST, "192.168.1.10:7183")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(ConnectInfo::<SocketAddr>(
            "192.168.1.20:50000".parse().unwrap(),
        ));
        last = app.clone().oneshot(req).await.unwrap().status().as_u16();
    }
    w(world).last_status = Some(last);
}

// --- Then -------------------------------------------------------------------

#[then("設定エラーとなり待受は開始されない")]
async fn then_config_error_no_listen(world: &mut AppWorld) {
    assert_eq!(w(world).validate_ok, Some(false), "検証エラーになるべき");
}

#[then("設定エラーとなる")]
async fn then_config_error(world: &mut AppWorld) {
    assert_eq!(w(world).validate_ok, Some(false), "検証エラーになるべき");
}

#[then("設定エラーとなる（明示確認なしに公開されない）")]
async fn then_config_error_consent(world: &mut AppWorld) {
    assert_eq!(w(world).validate_ok, Some(false), "同意なしは検証エラー");
}

#[then("正規化後にプライベートと判定され許可される")]
async fn then_validate_ok(world: &mut AppWorld) {
    assert_eq!(w(world).validate_ok, Some(true), "v4-mapped は許可される");
}

#[then("SecurityEvent WebUiLanExposed が 1 件記録される")]
async fn then_exposed_recorded(world: &mut AppWorld) {
    assert_eq!(w(world).exposed_count, Some(1), "監査イベントは 1 件");
}

#[then("403 で拒否される")]
async fn then_forbidden(world: &mut AppWorld) {
    assert_eq!(w(world).last_status, Some(403), "403 で拒否されるべき");
}

#[then("429 で拒否される（loopback 側と同じ上限）")]
async fn then_too_many(world: &mut AppWorld) {
    assert_eq!(w(world).last_status, Some(429), "21 件目は 429");
}

// --- 補助 -------------------------------------------------------------------

/// LAN 公開相当の互換 API 状態(Host ホワイトリスト拡張 + 送信元検証 + 20 req/秒・固定クロック)。
fn lan_compat_state() -> CompatState {
    let registry = LivechatRegistry::new(128);
    let board_keys = Arc::new(BoardKeyManager::new(
        Arc::new(Store::open_in_memory().unwrap()),
        Keystore::ephemeral(),
    ));
    let dir = tempfile::tempdir().unwrap();
    let security = Arc::new(SecurityLog::new(dir.path().join("s.log")).unwrap());
    std::mem::forget(dir);
    let manager = ParticipantManager::new(Arc::clone(&board_keys), None);
    CompatState {
        registry,
        board_keys,
        manager,
        security,
        allowed_hosts: Arc::new(host_allowlist("192.168.1.10:7183".parse().unwrap())),
        rate_limiter: Arc::new(RateLimiter::with_clock(
            RATE_LIMIT_PER_SEC,
            Box::new(|| 1_000),
        )),
        enforce_lan_source: true,
    }
}

/// 指定 Host・送信元で互換 API へ GET し、応答ステータスを返す。
async fn compat_request_status(host: &str, src: &str) -> u16 {
    let app = routes(lan_compat_state());
    let mut req = Request::builder()
        .method(Method::GET)
        .uri("/deadbeef/subject.txt")
        .header(header::HOST, host)
        .body(Body::empty())
        .unwrap();
    req.extensions_mut()
        .insert(ConnectInfo::<SocketAddr>(src.parse().unwrap()));
    app.oneshot(req).await.unwrap().status().as_u16()
}

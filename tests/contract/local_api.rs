//! T061 local API 契約テスト(contracts/local-api.md §保護方針・§検証方法)
//!
//! 横断的な保護方針(Web 骨格 T019 が担う層)を検証する:
//! - 変更系(POST/PUT/DELETE)での `X-Api-Token` 欠落 → 401
//! - 過大 JSON ボディ(64KB 超)→ 413(境界 = 65536 バイト)
//! - `Host` ヘッダ検証失敗 → 403(DNS rebinding 対策)
//! - `/api/v1` 全体のレート制限: 同一接続元 20 req/秒超過 → 429 + `http_rate_limited` ログ
//! - トークン取得: `GET /api/v1/token` は Host 検証下で返す(GET はトークン不要)
//! - エラー応答は `{"error":"<code>"}` のみ(内部情報を含まない)
//!
//! 個別エンドポイント(personas/peers/channels 等)のスキーマ検証は後続タスク
//! (T021/T030/T040/T062)の実装時に本ファイルへ追記する。

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode, header};
use serde_json::Value;
use tower::ServiceExt;

use peca_p2p_yp::security::SecurityLog;
use peca_p2p_yp::store::Store;
use peca_p2p_yp::web::{self, AppState, RateLimiter};

const GOOD_HOST: &str = "127.0.0.1:7180";
const TOKEN: &str = "test-session-token";

/// テスト用の SecurityLog(一時ファイルへ書き出す)と、その読み出し用パスを返す。
fn temp_security_log() -> (Arc<SecurityLog>, std::path::PathBuf, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("security.log");
    let log = SecurityLog::new(&path).unwrap();
    (Arc::new(log), path, dir)
}

/// 既定のレート上限・固定クロック(全リクエストを同一秒窓に収める)で AppState を作る。
fn test_state(security: Arc<SecurityLog>) -> AppState {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let mut hosts = HashSet::new();
    hosts.insert(GOOD_HOST.to_string());
    hosts.insert("localhost:7180".to_string());
    hosts.insert("[::1]:7180".to_string());
    let limiter = RateLimiter::with_clock(web::RATE_LIMIT_PER_SEC, Box::new(|| 1_000));
    AppState::with_parts(store, security, TOKEN, hosts, limiter)
}

/// リクエストを組み立てる。`host` / `token` は任意、接続元 IP は固定。
fn build_request(
    method: Method,
    uri: &str,
    host: Option<&str>,
    token: Option<&str>,
    body: Body,
) -> Request<Body> {
    let mut b = Request::builder().method(method).uri(uri);
    if let Some(h) = host {
        b = b.header(header::HOST, h);
    }
    if let Some(t) = token {
        b = b.header("X-Api-Token", t);
    }
    let mut req = b.body(body).unwrap();
    let addr: SocketAddr = "127.0.0.1:50000".parse().unwrap();
    req.extensions_mut().insert(ConnectInfo(addr));
    req
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

// ---------------------------------------------------------------------------
// トークン取得 + Host 検証
// ---------------------------------------------------------------------------

#[tokio::test]
async fn token_returned_under_valid_host() {
    let (security, _p, _d) = temp_security_log();
    let state = test_state(security);
    let app = web::build_router(state);
    let resp = app
        .oneshot(build_request(
            Method::GET,
            "/api/v1/token",
            Some(GOOD_HOST),
            None,
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["token"], TOKEN);
}

#[tokio::test]
async fn invalid_host_rejected() {
    let (security, _p, _d) = temp_security_log();
    let app = web::build_router(test_state(security));
    let resp = app
        .oneshot(build_request(
            Method::GET,
            "/api/v1/token",
            Some("evil.example.com"),
            None,
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let json = body_json(resp).await;
    assert!(json.get("error").is_some());
    // 内部情報を漏らさない: error キーのみ
    assert_eq!(json.as_object().unwrap().len(), 1);
}

#[tokio::test]
async fn missing_host_rejected() {
    let (security, _p, _d) = temp_security_log();
    let app = web::build_router(test_state(security));
    let resp = app
        .oneshot(build_request(
            Method::GET,
            "/api/v1/token",
            None,
            None,
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// X-Api-Token(変更系のみ必須)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn change_method_without_token_is_401() {
    let (security, _p, _d) = temp_security_log();
    let app = web::build_router(test_state(security));
    let resp = app
        .oneshot(build_request(
            Method::POST,
            "/api/v1/personas",
            Some(GOOD_HOST),
            None,
            Body::from("{}"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(resp).await;
    assert!(json.get("error").is_some());
}

#[tokio::test]
async fn change_method_with_wrong_token_is_401() {
    let (security, _p, _d) = temp_security_log();
    let app = web::build_router(test_state(security));
    let resp = app
        .oneshot(build_request(
            Method::POST,
            "/api/v1/personas",
            Some(GOOD_HOST),
            Some("wrong-token"),
            Body::from("{}"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn change_method_with_valid_token_passes_auth() {
    // 正しいトークンなら認証を通過する(ルート未実装のため 404 だが 401 ではない)。
    let (security, _p, _d) = temp_security_log();
    let app = web::build_router(test_state(security));
    let resp = app
        .oneshot(build_request(
            Method::POST,
            "/api/v1/personas",
            Some(GOOD_HOST),
            Some(TOKEN),
            Body::from("{}"),
        ))
        .await
        .unwrap();
    assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// ボディサイズ上限(64KB = 65536 バイト境界)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn oversize_body_is_413() {
    let (security, _p, _d) = temp_security_log();
    let app = web::build_router(test_state(security));
    let body = vec![b'a'; web::MAX_BODY_BYTES + 1]; // 65537 バイト
    let resp = app
        .oneshot(build_request(
            Method::POST,
            "/api/v1/personas",
            Some(GOOD_HOST),
            Some(TOKEN),
            Body::from(body),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let json = body_json(resp).await;
    assert!(json.get("error").is_some());
}

#[tokio::test]
async fn body_at_limit_is_not_413() {
    // ちょうど上限(65536 バイト)は 413 にならず、認証・ルーティングへ進む。
    let (security, _p, _d) = temp_security_log();
    let app = web::build_router(test_state(security));
    let body = vec![b'a'; web::MAX_BODY_BYTES]; // 65536 バイト
    let resp = app
        .oneshot(build_request(
            Method::POST,
            "/api/v1/personas",
            Some(GOOD_HOST),
            Some(TOKEN),
            Body::from(body),
        ))
        .await
        .unwrap();
    assert_ne!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

// ---------------------------------------------------------------------------
// レート制限(20 req/秒 → 21 件目 429 + http_rate_limited ログ)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rate_limit_triggers_429_and_logs() {
    let (security, log_path, _d) = temp_security_log();
    let state = test_state(security);

    // 同一 AppState(共有 Arc<RateLimiter>・固定クロック)で 20 件は通過。
    for i in 0..web::RATE_LIMIT_PER_SEC {
        let app = web::build_router(state.clone());
        let resp = app
            .oneshot(build_request(
                Method::GET,
                "/api/v1/token",
                Some(GOOD_HOST),
                None,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{i} 件目は通過するべき");
    }

    // 21 件目は 429。
    let app = web::build_router(state.clone());
    let resp = app
        .oneshot(build_request(
            Method::GET,
            "/api/v1/token",
            Some(GOOD_HOST),
            None,
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    let json = body_json(resp).await;
    assert!(json.get("error").is_some());

    // http_rate_limited がセキュリティログへ記録される。
    let content = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        content.contains("http_rate_limited"),
        "http_rate_limited が記録されるべき: {content}"
    );
}

// ---------------------------------------------------------------------------
// 静的アセット配信
// ---------------------------------------------------------------------------

#[tokio::test]
async fn serves_index_html() {
    let (security, _p, _d) = temp_security_log();
    let app = web::build_router(test_state(security));
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ---------------------------------------------------------------------------
// 設定 API(T062 — GET/PUT /settings)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_settings_returns_all_keys_with_defaults() {
    let (security, _p, _d) = temp_security_log();
    let app = web::build_router(test_state(security));
    let resp = app
        .oneshot(build_request(
            Method::GET,
            "/api/v1/settings",
            Some(GOOD_HOST),
            None,
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    // 13 キー(data-model §Settings)
    assert_eq!(json.as_object().unwrap().len(), 13);
    assert_eq!(json["pcp_bind"], "127.0.0.1:7146");
    assert_eq!(json["http_bind"], "127.0.0.1:7180");
    assert_eq!(json["p2p_bind"], "0.0.0.0:7147,[::]:7147");
    assert_eq!(json["event_store_max"], 4096);
    assert_eq!(json["pex_enabled"], true);
    assert_eq!(json["index_txt_encoding"], "utf-8");
}

#[tokio::test]
async fn put_settings_non_bind_change_no_restart() {
    let (security, _p, _d) = temp_security_log();
    let state = test_state(security);

    let app = web::build_router(state.clone());
    let resp = app
        .oneshot(build_request(
            Method::PUT,
            "/api/v1/settings",
            Some(GOOD_HOST),
            Some(TOKEN),
            Body::from(r#"{"min_pow_bits":12}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["restart_required"], false);
    assert_eq!(json["restart_keys"].as_array().unwrap().len(), 0);

    // 保存されたことを GET で確認(同一ストアを共有)。
    let app = web::build_router(state);
    let resp = app
        .oneshot(build_request(
            Method::GET,
            "/api/v1/settings",
            Some(GOOD_HOST),
            None,
            Body::empty(),
        ))
        .await
        .unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["min_pow_bits"], 12);
}

#[tokio::test]
async fn put_settings_bind_change_requires_restart() {
    let (security, _p, _d) = temp_security_log();
    let app = web::build_router(test_state(security));
    let resp = app
        .oneshot(build_request(
            Method::PUT,
            "/api/v1/settings",
            Some(GOOD_HOST),
            Some(TOKEN),
            Body::from(r#"{"p2p_bind":"0.0.0.0:7157"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["restart_required"], true);
    let keys: Vec<String> = json["restart_keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(keys, vec!["p2p_bind"]);
}

#[tokio::test]
async fn put_settings_non_loopback_bind_rejected_400() {
    let (security, _p, _d) = temp_security_log();
    let state = test_state(security);
    let app = web::build_router(state.clone());
    let resp = app
        .oneshot(build_request(
            Method::PUT,
            "/api/v1/settings",
            Some(GOOD_HOST),
            Some(TOKEN),
            Body::from(r#"{"pcp_bind":"0.0.0.0:7146"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = body_json(resp).await;
    assert_eq!(json["error"], "non_loopback_bind");
    assert_eq!(json.as_object().unwrap().len(), 1);

    // 拒否された値は保存されていない。
    let app = web::build_router(state);
    let resp = app
        .oneshot(build_request(
            Method::GET,
            "/api/v1/settings",
            Some(GOOD_HOST),
            None,
            Body::empty(),
        ))
        .await
        .unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["pcp_bind"], "127.0.0.1:7146");
}

#[tokio::test]
async fn put_settings_unknown_key_rejected_400() {
    let (security, _p, _d) = temp_security_log();
    let app = web::build_router(test_state(security));
    let resp = app
        .oneshot(build_request(
            Method::PUT,
            "/api/v1/settings",
            Some(GOOD_HOST),
            Some(TOKEN),
            Body::from(r#"{"bogus_key":1}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = body_json(resp).await;
    assert_eq!(json["error"], "invalid_request");
}

#[tokio::test]
async fn put_settings_without_token_is_401() {
    // 変更系は 4 層保護のトークン検証を継承する。
    let (security, _p, _d) = temp_security_log();
    let app = web::build_router(test_state(security));
    let resp = app
        .oneshot(build_request(
            Method::PUT,
            "/api/v1/settings",
            Some(GOOD_HOST),
            None,
            Body::from(r#"{"min_pow_bits":1}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// ピア API(T021 — GET/POST/PUT/DELETE /peers・GET /peers/export)
// ---------------------------------------------------------------------------

use peca_p2p_yp::store::PeerSource;

async fn body_text(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn get_peers_returns_health_schema() {
    let (security, _p, _d) = temp_security_log();
    let state = test_state(security);
    state
        .store
        .upsert_peer("192.0.2.5:7147", PeerSource::Manual)
        .unwrap();

    let app = web::build_router(state);
    let resp = app
        .oneshot(build_request(
            Method::GET,
            "/api/v1/peers",
            Some(GOOD_HOST),
            None,
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    let p = &arr[0];
    assert_eq!(p["addr"], "192.0.2.5:7147");
    assert_eq!(p["source"], "manual");
    assert_eq!(p["verified"], false);
    assert_eq!(p["enabled"], true);
    assert_eq!(p["connected"], false);
    assert_eq!(p["fail_count"], 0);
    assert!(p["last_ok_at"].is_null());
    assert!(p["id"].is_i64());
}

#[tokio::test]
async fn post_peers_bulk_add_reports_individual_errors() {
    let (security, _p, _d) = temp_security_log();
    let state = test_state(security);
    let app = web::build_router(state.clone());
    let resp = app
        .oneshot(build_request(
            Method::POST,
            "/api/v1/peers",
            Some(GOOD_HOST),
            Some(TOKEN),
            Body::from(r#"{"addrs":["192.0.2.9:7147","garbage","host:notaport"]}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    // 正常 1 件・不正 2 件を個別に返す(全体は失敗しない)
    assert_eq!(json["added"].as_array().unwrap().len(), 1);
    let errors = json["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 2);
    assert_eq!(json["added"][0]["source"], "manual");
    let bad_addrs: Vec<&str> = errors.iter().map(|e| e["addr"].as_str().unwrap()).collect();
    assert!(bad_addrs.contains(&"garbage"));

    // 正常分のみ登録されている
    let app = web::build_router(state);
    let resp = app
        .oneshot(build_request(
            Method::GET,
            "/api/v1/peers",
            Some(GOOD_HOST),
            None,
            Body::empty(),
        ))
        .await
        .unwrap();
    let json = body_json(resp).await;
    assert_eq!(json.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn post_peers_without_token_is_401() {
    // 新規ルートも 4 層保護のトークン検証を継承する。
    let (security, _p, _d) = temp_security_log();
    let app = web::build_router(test_state(security));
    let resp = app
        .oneshot(build_request(
            Method::POST,
            "/api/v1/peers",
            Some(GOOD_HOST),
            None,
            Body::from(r#"{"addrs":["192.0.2.9:7147"]}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn put_peer_toggles_enabled() {
    let (security, _p, _d) = temp_security_log();
    let state = test_state(security);
    let peer = state
        .store
        .upsert_peer("192.0.2.6:7147", PeerSource::Manual)
        .unwrap();

    let app = web::build_router(state.clone());
    let resp = app
        .oneshot(build_request(
            Method::PUT,
            &format!("/api/v1/peers/{}", peer.id),
            Some(GOOD_HOST),
            Some(TOKEN),
            Body::from(r#"{"enabled":false}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["enabled"], false);

    // 永続化を確認
    assert!(
        !state
            .store
            .get_peer("192.0.2.6:7147")
            .unwrap()
            .unwrap()
            .enabled
    );
}

#[tokio::test]
async fn put_peer_unknown_id_is_404() {
    let (security, _p, _d) = temp_security_log();
    let app = web::build_router(test_state(security));
    let resp = app
        .oneshot(build_request(
            Method::PUT,
            "/api/v1/peers/999999",
            Some(GOOD_HOST),
            Some(TOKEN),
            Body::from(r#"{"enabled":false}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let json = body_json(resp).await;
    assert_eq!(json.as_object().unwrap().len(), 1);
}

#[tokio::test]
async fn delete_peer_removes_it() {
    let (security, _p, _d) = temp_security_log();
    let state = test_state(security);
    let peer = state
        .store
        .upsert_peer("192.0.2.7:7147", PeerSource::Manual)
        .unwrap();

    let app = web::build_router(state.clone());
    let resp = app
        .oneshot(build_request(
            Method::DELETE,
            &format!("/api/v1/peers/{}", peer.id),
            Some(GOOD_HOST),
            Some(TOKEN),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["deleted"], true);
    assert!(state.store.get_peer("192.0.2.7:7147").unwrap().is_none());
}

#[tokio::test]
async fn export_peers_returns_verified_only_text_plain() {
    let (security, _p, _d) = temp_security_log();
    let state = test_state(security);
    // verified(接続成功実績あり)と未検証の 2 件
    state
        .store
        .upsert_peer("192.0.2.20:7147", PeerSource::Manual)
        .unwrap();
    state
        .store
        .record_peer_success("192.0.2.20:7147", 1000)
        .unwrap();
    state
        .store
        .upsert_peer("192.0.2.21:7147", PeerSource::Pex)
        .unwrap();

    let app = web::build_router(state);
    let resp = app
        .oneshot(build_request(
            Method::GET,
            "/api/v1/peers/export",
            Some(GOOD_HOST),
            None,
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ctype = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(ctype.starts_with("text/plain"));
    let text = body_text(resp).await;
    // verified のみ。未検証(192.0.2.21)は含まれない
    assert!(text.contains("192.0.2.20:7147"));
    assert!(!text.contains("192.0.2.21:7147"));
    assert!(text.ends_with('\n'));
}

// ---------------------------------------------------------------------------
// index.txt HTTP 契約(T042 — contracts/http-yp.md)
// ---------------------------------------------------------------------------

/// `GET /index.txt` は Host 検証・トークン不要で 200 を返す。
/// `/api/v1` 保護層の外側にルートが配置されていることを確認する。
#[tokio::test]
async fn index_txt_get_bypasses_api_protection() {
    let (security, _p, _d) = temp_security_log();
    let app = web::build_router(test_state(security));
    // Host ヘッダなし・X-Api-Token なしで GET → 200 であることを確認
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/index.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "GET /index.txt は /api/v1 保護層の外側のため Host/Token 不要"
    );
    // Content-Type は text/plain であること
    let ctype = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .expect("Content-Type ヘッダが必要")
        .to_str()
        .unwrap();
    assert!(ctype.starts_with("text/plain"), "Content-Type: {ctype}");
}

/// `HEAD /index.txt` はボディなしで 200 を返す。
#[tokio::test]
async fn index_txt_head_returns_empty_body() {
    let (security, _p, _d) = temp_security_log();
    let app = web::build_router(test_state(security));
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::HEAD)
                .uri("/index.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // HEAD ではボディが空
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(bytes.is_empty(), "HEAD のボディは空であること");
}

/// `POST /index.txt` は 405 Method Not Allowed を返す(GET/HEAD のみ受け付ける)。
#[tokio::test]
async fn index_txt_post_is_405() {
    let (security, _p, _d) = temp_security_log();
    let app = web::build_router(test_state(security));
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/index.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::METHOD_NOT_ALLOWED,
        "GET/HEAD 以外は 405"
    );
}

//! Web 骨格(T019 — contracts/local-api.md §保護方針)
//!
//! axum(0.8)ルーターと、`/api/v1` 全体に適用する横断的保護層を提供する。
//! 個別エンドポイント(personas/peers/channels/settings 等)は後続タスク
//! (T021/T030/T040/T041/T062)が [`api_router`] に追加する。
//!
//! 保護層(`/api/v1` に対し、外側から順に評価):
//! 1. **Host 検証** — バインド由来のホワイトリスト以外は 403(DNS rebinding / CSRF 対策)
//! 2. **レート制限** — 同一接続元 20 req/秒超過は 429 + `http_rate_limited` ログ
//! 3. **トークン検証** — 変更系(POST/PUT/DELETE)は起動時生成の `X-Api-Token` 必須(欠落/不一致は 401)
//! 4. **ボディサイズ上限** — 64KB 超は 413(ボディ読取前に認証を通すため token 検証の後段)
//!
//! エラー応答は `{"error":"<code>"}` のみ(内部情報を含めない — Principle II)。
//! 静的アセット(`ui/`)は保護層の外側でバイナリ埋め込み配信する
//! (DNS rebinding の標的は API であり、UI HTML はどの Host へ返しても情報を漏らさない)。

use std::collections::HashMap;
use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};

use crate::broadcast::BroadcastState;
use crate::event::view::ChannelDirectory;
use crate::identity::IdentityManager;
use crate::security::{SecurityCategory, SecurityLog};
use crate::store::Store;

pub mod announced;
pub mod channels;
pub mod compat;
pub mod livechat;
pub mod mutes;
pub mod peers;
pub mod personas;
pub mod settings;

use crate::web::announced::{AnnouncedProvider, NodeStatusProvider};

/// `/api/v1` のボディサイズ上限(64KB。境界含む = 65536 バイトまで許容)。
pub const MAX_BODY_BYTES: usize = 64 * 1024;

/// `/api/v1` 全体のレート上限(同一接続元・秒あたり)。
pub const RATE_LIMIT_PER_SEC: u32 = 20;

// ---------------------------------------------------------------------------
// アプリケーション状態
// ---------------------------------------------------------------------------

/// axum の共有状態。全フィールドは `Arc` で安価に複製できる。
///
/// 後続タスクのハンドラはこの状態から [`Store`]・[`SecurityLog`] を参照する。
#[derive(Clone)]
pub struct AppState {
    /// 永続ストア(T012)。
    pub store: Arc<Store>,
    /// セキュリティイベントログ(T014)。
    pub security: Arc<SecurityLog>,
    /// 変更系の認可に用いるセッショントークン(起動時生成)。
    pub token: Arc<str>,
    /// 受理する `Host` ヘッダのホワイトリスト。
    pub allowed_hosts: Arc<HashSet<String>>,
    /// 接続元ごとのレート制限器。
    pub rate_limiter: Arc<RateLimiter>,
    /// index.txt 専用レート制限器(10 req/秒 — contracts/http-yp.md)。
    pub index_txt_rate_limiter: Arc<RateLimiter>,
    /// チャンネル一覧の供給元(T039)。未配線時は `None`(空一覧として扱う)。
    pub directory: Option<Arc<dyn ChannelDirectory>>,
    /// ペルソナ管理(T028)。未配線時は `None`(ペルソナ API は 503 相当を返す)。
    pub identity: Option<Arc<IdentityManager>>,
    /// 掲載中チャンネルの供給元(T031)。未配線時は `None`(空一覧)。
    pub announced: Option<Arc<dyn AnnouncedProvider>>,
    /// ノード状態の供給元(T031)。未配線時は `None`(全て 0/false)。
    pub node_status: Option<Arc<dyn NodeStatusProvider>>,
    /// 配信中ロックの共有状態(T025 — `GET /status` の `broadcasting`)。
    /// 未配線時は `None`(= `broadcasting: false`。contracts §3)。
    pub broadcast: Option<Arc<BroadcastState>>,
    /// index.txt の LAN 公開状態(ADR-0012)。`None` = 機能無効(`index_bind` 空)。
    /// 起動時に一度だけ確定する不変値のため `Mutex` を持たない(research R3)。
    pub index_lan: Option<Arc<IndexLanStatus>>,
}

/// index.txt LAN 公開の実行時状態(data-model §3)。起動時に一度だけ確定する不変値。
///
/// `GET /api/v1/status` の `index_txt_lan` オブジェクトの供給元となる。3 状態
/// (無効 = `AppState.index_lan` が `None` / 露出中 = `listening: true` /
/// 設定有効だが bind 失敗 = `listening: false` + `error`)を表す。
#[derive(Debug, Clone)]
pub struct IndexLanStatus {
    /// 設定されたバインド先(検証済み値の文字列表現)。
    pub bind: String,
    /// bind に成功して待受中か。
    pub listening: bool,
    /// 失敗理由の定型コード(`addr_in_use` / `permission_denied` /
    /// `addr_not_available` / `unknown`)。`listening: true` なら `None`。
    pub error: Option<&'static str>,
}

impl AppState {
    /// 本番用。トークンを乱数生成し、実クロックのレート制限器と HTTP ポート由来の
    /// Host ホワイトリストを構成する。
    pub fn new(store: Arc<Store>, security: Arc<SecurityLog>, http_port: u16) -> Self {
        AppState {
            store,
            security,
            token: Arc::from(generate_token().as_str()),
            allowed_hosts: Arc::new(loopback_hosts(http_port)),
            rate_limiter: Arc::new(RateLimiter::per_second(RATE_LIMIT_PER_SEC)),
            index_txt_rate_limiter: Arc::new(RateLimiter::per_second(10)),
            directory: None,
            identity: None,
            announced: None,
            node_status: None,
            broadcast: None,
            index_lan: None,
        }
    }

    /// テスト・多ノード用。トークン・Host 集合・レート制限器を注入する。
    pub fn with_parts(
        store: Arc<Store>,
        security: Arc<SecurityLog>,
        token: impl Into<String>,
        allowed_hosts: HashSet<String>,
        rate_limiter: RateLimiter,
    ) -> Self {
        AppState {
            store,
            security,
            token: Arc::from(token.into().as_str()),
            allowed_hosts: Arc::new(allowed_hosts),
            rate_limiter: Arc::new(rate_limiter),
            index_txt_rate_limiter: Arc::new(RateLimiter::per_second(10)),
            directory: None,
            identity: None,
            announced: None,
            node_status: None,
            broadcast: None,
            index_lan: None,
        }
    }

    /// チャンネル一覧の供給元を配線する(起動配線・テストで使用)。
    pub fn with_directory(mut self, directory: Arc<dyn ChannelDirectory>) -> Self {
        self.directory = Some(directory);
        self
    }

    /// ペルソナ管理を配線する(起動配線・テストで使用)。
    pub fn with_identity(mut self, identity: Arc<IdentityManager>) -> Self {
        self.identity = Some(identity);
        self
    }

    /// 掲載中チャンネルの供給元を配線する(起動配線・テストで使用)。
    pub fn with_announced(mut self, announced: Arc<dyn AnnouncedProvider>) -> Self {
        self.announced = Some(announced);
        self
    }

    /// ノード状態の供給元を配線する(起動配線・テストで使用)。
    pub fn with_node_status(mut self, node_status: Arc<dyn NodeStatusProvider>) -> Self {
        self.node_status = Some(node_status);
        self
    }

    /// 配信中ロックの共有状態を配線する(T025 — `GET /status` の `broadcasting`)。
    pub fn with_broadcast(mut self, broadcast: Arc<BroadcastState>) -> Self {
        self.broadcast = Some(broadcast);
        self
    }

    /// index.txt の LAN 公開状態を配線する(ADR-0012 — `GET /status` の `index_txt_lan`)。
    pub fn with_index_lan(mut self, index_lan: Arc<IndexLanStatus>) -> Self {
        self.index_lan = Some(index_lan);
        self
    }

    /// セッショントークン。
    pub fn token(&self) -> &str {
        &self.token
    }
}

/// ポート `port` に対する loopback Host ホワイトリスト
/// (`127.0.0.1:port` / `localhost:port` / `[::1]:port`)。
pub fn loopback_hosts(port: u16) -> HashSet<String> {
    let mut set = HashSet::new();
    set.insert(format!("127.0.0.1:{port}"));
    set.insert(format!("localhost:{port}"));
    set.insert(format!("[::1]:{port}"));
    set
}

/// 起動時セッショントークン(乱数 32 バイトの hex 表現)。
fn generate_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    to_hex(&bytes)
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

// ---------------------------------------------------------------------------
// レート制限器(固定 1 秒窓・接続元ごと)
// ---------------------------------------------------------------------------

struct RateWindow {
    window: u64,
    count: u32,
}

/// エントリ数がこれ以上のとき、`check` は過去秒の死んだエントリを回収してから処理する。
/// 固定 1 秒窓では `window` が現在秒でないエントリは次アクセスで必ずリセットされるため、
/// 回収してもレート制限の判定は変わらない。メモリはおおよそ
/// 「しきい値 + 同一秒内の新規送信元数」に有界化される(LAN 公開時の送信元多様化対策)。
const RATE_LIMITER_SWEEP_THRESHOLD: usize = 1024;

/// 接続元 IP ごとの固定 1 秒窓レート制限器。クロックは注入可能(テスト用)。
pub struct RateLimiter {
    max_per_sec: u32,
    now: Box<dyn Fn() -> u64 + Send + Sync>,
    state: Mutex<HashMap<IpAddr, RateWindow>>,
}

impl RateLimiter {
    /// 実時刻(unix 秒)を用いる制限器。
    pub fn per_second(max_per_sec: u32) -> Self {
        Self::with_clock(max_per_sec, Box::new(unix_now))
    }

    /// クロックを注入して作る(テスト用)。
    pub fn with_clock(max_per_sec: u32, now: Box<dyn Fn() -> u64 + Send + Sync>) -> Self {
        RateLimiter {
            max_per_sec,
            now,
            state: Mutex::new(HashMap::new()),
        }
    }

    /// 現在保持している接続元エントリ数(テスト用)。
    #[cfg(test)]
    fn entry_count(&self) -> usize {
        self.state.lock().map(|s| s.len()).unwrap_or(0)
    }

    /// 接続元 `ip` のリクエストを許可するなら `true`、上限超過なら `false`。
    pub fn check(&self, ip: IpAddr) -> bool {
        let now = (self.now)();
        let Ok(mut state) = self.state.lock() else {
            // ロック異常時は安全側(拒否)に倒す
            return false;
        };
        if state.len() >= RATE_LIMITER_SWEEP_THRESHOLD {
            state.retain(|_, w| w.window == now);
        }
        let entry = state.entry(ip).or_insert(RateWindow {
            window: now,
            count: 0,
        });
        if entry.window != now {
            entry.window = now;
            entry.count = 0;
        }
        if entry.count < self.max_per_sec {
            entry.count += 1;
            true
        } else {
            false
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// ルーター構築
// ---------------------------------------------------------------------------

/// アプリケーション全体のルーターを構築する。
///
/// `/api/v1` は保護層つきのサブルーター([`api_router`])、それ以外は `ui/` の
/// 静的アセット([`static_handler`])へフォールバックする。
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .nest("/api/v1", api_router(state.clone()))
        .merge(crate::yp::index_txt::routes())
        .fallback(static_handler)
        .with_state(state)
}

/// index.txt 専用の第 2 リスナー(LAN 公開)のルーターを構築する(ADR-0012)。
///
/// loopback 側と**同一の** [`crate::yp::index_txt::routes`] を再マウントするだけで、
/// `/api/v1`(API)や静的アセット(UI)のルートは物理的に持たない。これにより
/// 「経路フィルタのバグで API が LAN 露出する」という故障モードが構造的に存在しない
/// (research R2 — Principle II)。
///
/// URL 長 ≤ 1KB・ヘッダ ≤ 8KB の上限とレート制限(10 req/秒)は `index_txt` の
/// ハンドラ内部に実装されており、`routes()` を再マウントするだけで第 2 リスナーにも
/// そのまま適用される。レート制限器は [`AppState::index_txt_rate_limiter`] を共有する。
///
/// index.txt 以外の全パス(`/api/v1/...`・`/`・静的アセット)は fallback の定型 404
/// `{"error":"not_found"}` を返す(contract §1.1)。`/index.txt` への GET/HEAD 以外は
/// axum が 405(空ボディ + `Allow`)を自動応答する。
pub fn build_index_router(state: AppState) -> Router {
    Router::new()
        .merge(crate::yp::index_txt::routes())
        .fallback(index_not_found)
        .with_state(state)
}

/// LAN リスナーの未定義パスに対する定型 404(index.txt 以外は API/UI ともに存在しない)。
async fn index_not_found() -> Response {
    error_response(StatusCode::NOT_FOUND, "not_found")
}

/// `/api/v1` サブルーター。後続タスクはここへルートを追加する。
pub(crate) fn api_router(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/token", get(token_handler))
        .route(
            "/settings",
            get(settings::get_settings).put(settings::put_settings),
        )
        .merge(channels::routes())
        .merge(mutes::routes())
        .merge(peers::routes())
        .merge(personas::routes())
        .merge(announced::routes())
        // 未定義パスも保護層を通し定型 404 を返す(layer は未マッチ経路には及ばないため
        // fallback をルートとして持たせ、全 `/api/v1` パスで保護層が評価されるようにする)
        .fallback(api_not_found)
        // 保護層(最後に付けた layer が最外周 = 最初に評価される)
        .layer(middleware::from_fn(body_limit))
        .layer(middleware::from_fn_with_state(state.clone(), require_token))
        .layer(middleware::from_fn_with_state(state.clone(), rate_limit))
        .layer(middleware::from_fn_with_state(state, host_guard))
}

// ---------------------------------------------------------------------------
// ハンドラ
// ---------------------------------------------------------------------------

#[derive(serde::Serialize)]
struct TokenResponse<'a> {
    token: &'a str,
}

/// `GET /api/v1/token` — UI 初回ロード時にセッショントークンを渡す。
/// Host 検証下でのみ到達する(GET はトークン不要)。
async fn token_handler(State(state): State<AppState>) -> Response {
    Json(TokenResponse {
        token: state.token(),
    })
    .into_response()
}

/// `/api/v1` の未定義パスに対する定型 404。
async fn api_not_found() -> Response {
    error_response(StatusCode::NOT_FOUND, "not_found")
}

/// 静的アセット配信(バイナリ埋め込み)。既知の HTML パスを返す。
async fn static_handler(req: Request) -> Response {
    const INDEX_HTML: &str = include_str!("../../ui/index.html");
    const SETTINGS_HTML: &str = include_str!("../../ui/settings.html");
    const PEERS_HTML: &str = include_str!("../../ui/peers.html");
    const CHANNELS_HTML: &str = include_str!("../../ui/channels.html");
    const PERSONAS_HTML: &str = include_str!("../../ui/personas.html");
    let html = match req.uri().path() {
        "/" | "/index.html" => Some(INDEX_HTML),
        "/settings.html" => Some(SETTINGS_HTML),
        "/peers.html" => Some(PEERS_HTML),
        "/channels.html" => Some(CHANNELS_HTML),
        "/personas.html" => Some(PERSONAS_HTML),
        _ => None,
    };
    match html {
        Some(body) => ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], body).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// ---------------------------------------------------------------------------
// 保護層ミドルウェア
// ---------------------------------------------------------------------------

/// 定型エラー応答 `{"error":"<code>"}`(内部情報を含めない)。
pub(crate) fn error_response(status: StatusCode, code: &'static str) -> Response {
    (status, Json(serde_json::json!({ "error": code }))).into_response()
}

/// Host ヘッダ検証(ホワイトリスト外・欠落は 403)。
async fn host_guard(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let ok = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(|h| state.allowed_hosts.contains(h))
        .unwrap_or(false);
    if ok {
        next.run(req).await
    } else {
        error_response(StatusCode::FORBIDDEN, "forbidden_host")
    }
}

/// 接続元ごとのレート制限(超過は 429 + `http_rate_limited` ログ)。
async fn rate_limit(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let ip = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip())
        .unwrap_or(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    if state.rate_limiter.check(ip) {
        next.run(req).await
    } else {
        state.security.log(
            SecurityCategory::HttpRateLimited,
            &ip.to_string(),
            "rate limit exceeded",
        );
        error_response(StatusCode::TOO_MANY_REQUESTS, "rate_limited")
    }
}

/// 変更系(POST/PUT/DELETE)にセッショントークンを要求する(欠落/不一致は 401)。
async fn require_token(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let needs_token = matches!(
        *req.method(),
        Method::POST | Method::PUT | Method::DELETE | Method::PATCH
    );
    if !needs_token {
        return next.run(req).await;
    }
    let presented = req
        .headers()
        .get("X-Api-Token")
        .and_then(|v| v.to_str().ok());
    if presented == Some(state.token()) {
        next.run(req).await
    } else {
        error_response(StatusCode::UNAUTHORIZED, "unauthorized")
    }
}

/// ボディサイズ上限(64KB 超は 413)。以降のハンドラはバッファ済みボディを受け取る。
async fn body_limit(req: Request, next: Next) -> Response {
    let (parts, body) = req.into_parts();
    match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(bytes) => {
            let req = Request::from_parts(parts, Body::from(bytes));
            next.run(req).await
        }
        Err(_) => error_response(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large"),
    }
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limiter_fixed_window() {
        let limiter = RateLimiter::with_clock(3, Box::new(|| 42));
        let ip = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
        assert!(limiter.check(ip));
        assert!(limiter.check(ip));
        assert!(limiter.check(ip));
        assert!(!limiter.check(ip), "上限超過は拒否");
    }

    #[test]
    fn rate_limiter_resets_next_window() {
        let clock = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let c2 = Arc::clone(&clock);
        let limiter = RateLimiter::with_clock(
            1,
            Box::new(move || c2.load(std::sync::atomic::Ordering::SeqCst)),
        );
        let ip = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
        assert!(limiter.check(ip));
        assert!(!limiter.check(ip));
        clock.store(2, std::sync::atomic::Ordering::SeqCst);
        assert!(limiter.check(ip), "窓が変われば回復");
    }

    #[test]
    fn distinct_ips_have_separate_budgets() {
        let limiter = RateLimiter::with_clock(1, Box::new(|| 7));
        let a = IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1));
        let b = IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 2));
        assert!(limiter.check(a));
        assert!(limiter.check(b), "別 IP は独立予算");
        assert!(!limiter.check(a));
    }

    #[test]
    fn rate_limiter_evicts_stale_entries_when_over_threshold() {
        let clock = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let c2 = Arc::clone(&clock);
        let limiter = RateLimiter::with_clock(
            10,
            Box::new(move || c2.load(std::sync::atomic::Ordering::SeqCst)),
        );
        // 秒 1 にしきい値超の多数送信元からアクセス(敵対的な送信元多様化を模す)
        let total = RATE_LIMITER_SWEEP_THRESHOLD * 2;
        for i in 0..total {
            let ip = IpAddr::V4(std::net::Ipv4Addr::from(0x0a00_0000u32 + i as u32));
            assert!(limiter.check(ip));
        }
        assert_eq!(limiter.entry_count(), total, "秒内はエントリが保持される");
        // 秒 2 の最初のアクセスで、過去秒の死んだエントリが回収される
        clock.store(2, std::sync::atomic::Ordering::SeqCst);
        let ip = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
        assert!(limiter.check(ip));
        assert_eq!(
            limiter.entry_count(),
            1,
            "window が過去のエントリは全て回収され、現在秒の 1 件のみ残る"
        );
    }

    #[test]
    fn loopback_hosts_covers_three_forms() {
        let hosts = loopback_hosts(7180);
        assert!(hosts.contains("127.0.0.1:7180"));
        assert!(hosts.contains("localhost:7180"));
        assert!(hosts.contains("[::1]:7180"));
    }

    #[test]
    fn hex_encoding_is_lowercase_and_full_width() {
        assert_eq!(to_hex(&[0x00, 0xff, 0x1a]), "00ff1a");
        assert_eq!(generate_token().len(), 64);
    }
}

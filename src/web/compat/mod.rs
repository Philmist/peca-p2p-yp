//! 2ch 互換 API(T052/T054 — contracts/compat-api.md)
//!
//! 専用 loopback リスナー(`compat_bbs_bind`)で subject.txt / dat / SETTING.TXT / head.txt /
//! bbs.cgi を Shift_JIS で提供する。既存 `/api/v1`(AppState)とは**独立した専用状態**
//! ([`CompatState`])を持つ — index.txt の LAN リスナー(ADR-0012)と同じ設計思想で、
//! 「経路フィルタのバグで API がこちらに露出する」「逆にこちらのバグでトークン保護 API が
//! 露出する」という故障モードを構造的に排除する(本リスナーは `/api/v1` のルートを
//! 物理的に持たない)。
//!
//! ## 対象範囲(T069 で拡張)
//!
//! 互換 API は**自ノードがホストしている板**(`registry`)に加え、**「スレを開く」で常駐セッションを
//! 維持している他ノード板**(`manager` — 接続中・凍結中)も対象とする(compat-api.md §板の URL 対応
//! 「接続中・凍結中のスレを持つ板のみが解決される」)。[`resolve_snapshot`] が registry を優先し、
//! 無ければ参加者セッションの [`SessionView`] から [`BoardSnapshot`] を合成する。どちらも保持して
//! いない板(未開設・未接続・クローズ揮発)は `UnknownBoard` 相当の 404 になる。
//!
//! リモート板の書き込み(bbs.cgi)は常駐セッション経由で RES を送る(FR-028 — 通常経路と同一)。
//! 参加者セッションは前世代を保持しないため、リモート板の dat は現行世代のみ(旧世代 key は 404)。
//!
//! ## 保護層(FR-026)
//!
//! 1. **Host 検証**: `127.0.0.1[:port]` / `localhost[:port]` 以外は定型 403
//! 2. **レート制限**: 同一接続元・秒あたり([`RATE_LIMIT_PER_SEC`])
//! 3. **ボディ上限**: ≤ 64KB(bbs.cgi の POST のみ関係する)
//!
//! 違反はすべて `compat_bbs_denied` として記録する(内部情報を含めない)。

pub mod bbs_cgi;
pub mod dat;
pub mod sjis;

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Path, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

use crate::livechat::board::BoardKeyManager;
use crate::livechat::manager::ParticipantManager;
use crate::livechat::participant::{SessionLiveState, SessionView};
use crate::livechat::registry::{BoardSnapshot, LivechatRegistry};
use crate::livechat::thread::{Thread, ThreadState};
use crate::security::{SecurityCategory, SecurityLog};
use crate::web::RateLimiter;

/// 互換 API のレート上限(同一接続元・秒あたり)。loopback 限定のトークンなし受け口の
/// 代替防御(FR-026)。index.txt(10 req/秒)より緩め — 専ブラの通常巡回頻度を妨げない。
pub const RATE_LIMIT_PER_SEC: u32 = 20;

/// bbs.cgi の POST ボディ上限(64KB — FR-026)。
pub const MAX_BODY_BYTES: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// 状態
// ---------------------------------------------------------------------------

/// 互換 API 専用の共有状態(既存 `/api/v1` の `AppState` とは独立)。
#[derive(Clone)]
pub struct CompatState {
    /// 自ノードホスト板のレジストリ(自板の読み出し・書き込み対象)。
    pub registry: Arc<LivechatRegistry>,
    /// 板鍵の自動管理(bbs.cgi の自動署名 — T056)。
    pub board_keys: Arc<BoardKeyManager>,
    /// 他ノード板の常駐セッション(T069 — 接続中・凍結中のリモート板を解決する)。
    pub manager: Arc<ParticipantManager>,
    /// セキュリティイベントログ(`compat_bbs_denied` の記録先)。
    pub security: Arc<SecurityLog>,
    /// 受理する `Host` ヘッダのホワイトリスト(バインドポート由来)。
    pub allowed_hosts: Arc<std::collections::HashSet<String>>,
    /// 接続元ごとのレート制限器。
    pub rate_limiter: Arc<RateLimiter>,
}

/// 自板(registry)優先、無ければ他ノード板の常駐セッション(manager)から板スナップショットを
/// 解決する(T069 — 接続中・凍結中のリモート板に互換 API を対応させる)。
///
/// リモート板の [`SessionView`] から [`BoardSnapshot`] を合成する。参加者セッションは前世代を
/// 保持しないため `frozen` は常に `None`(dat は現行世代のみ — compat-api.md の 404 許容範囲)。
/// クローズ・終端でデータ削除済み(揮発)は `None`(定型 404)。
fn resolve_snapshot(state: &CompatState, board: &str) -> Option<BoardSnapshot> {
    if let Some(snap) = state.registry.board_snapshot(board) {
        return Some(snap);
    }
    let view = state.manager.view(board)?;
    session_view_to_snapshot(board, &view)
}

/// 他ノード板の [`SessionView`] から [`BoardSnapshot`] を合成する(T069)。
///
/// クローズ(揮発)は `None`。状態は Frozen/Active を写す。`last_confirmed_at` は確定レスの
/// 申告 created_at の最大値(自板と異なりホスト時計を持たないためベストエフォート)。
fn session_view_to_snapshot(board: &str, view: &SessionView) -> Option<BoardSnapshot> {
    if matches!(view.state, SessionLiveState::Closed) {
        return None;
    }
    let settings = view.settings.clone().unwrap_or_default();
    let state = match view.state {
        SessionLiveState::Frozen => ThreadState::Frozen,
        _ => ThreadState::Active,
    };
    let last_confirmed_at = view
        .confirmed
        .iter()
        .map(|r| r.created_at)
        .max()
        .unwrap_or(view.key as i64);
    let active = Thread {
        board_id: board.to_string(),
        channel: String::new(),
        generation: view.generation,
        key: view.key,
        title: settings.title.clone(),
        res_limit: settings.res_limit,
        state,
        res: view.confirmed.clone(),
        last_confirmed_at,
    };
    Some(BoardSnapshot {
        active,
        frozen: None,
        settings,
    })
}

// ---------------------------------------------------------------------------
// ルーター
// ---------------------------------------------------------------------------

/// 互換 API 専用リスナーのルーター(T052)。`/api/v1`・静的アセットは物理的に持たない。
pub fn routes(state: CompatState) -> Router {
    Router::new()
        .route("/{board}/subject.txt", get(subject_txt))
        .route("/{board}/SETTING.TXT", get(setting_txt))
        .route("/{board}/head.txt", get(head_txt))
        .route("/{board}/dat/{key_dat}", get(dat_file))
        .route("/test/bbs.cgi", post(bbs_cgi_handler))
        .fallback(compat_not_found)
        .layer(middleware::from_fn_with_state(state.clone(), rate_limit))
        .layer(middleware::from_fn_with_state(state.clone(), host_guard))
        .with_state(state)
}

/// 未定義パスに対する定型 404。
async fn compat_not_found() -> Response {
    error_response(StatusCode::NOT_FOUND)
}

/// 定型エラー応答(本文なし・ステータスのみ)。内部情報を含めない(Principle II)。
fn error_response(status: StatusCode) -> Response {
    status.into_response()
}

// ---------------------------------------------------------------------------
// 保護層(T052)
// ---------------------------------------------------------------------------

/// Host ヘッダ検証(ホワイトリスト外・欠落は 403 — DNS rebinding 対策)。
async fn host_guard(State(state): State<CompatState>, req: Request, next: Next) -> Response {
    let ok = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(|h| state.allowed_hosts.contains(h))
        .unwrap_or(false);
    if ok {
        next.run(req).await
    } else {
        let ip = client_ip(&req);
        state.security.log(
            SecurityCategory::CompatBbsDenied,
            &ip.to_string(),
            "invalid host header",
        );
        error_response(StatusCode::FORBIDDEN)
    }
}

/// 接続元ごとのレート制限(超過は 429 + `compat_bbs_denied` 記録)。
async fn rate_limit(State(state): State<CompatState>, req: Request, next: Next) -> Response {
    let ip = client_ip(&req);
    if state.rate_limiter.check(ip) {
        next.run(req).await
    } else {
        state.security.log(
            SecurityCategory::CompatBbsDenied,
            &ip.to_string(),
            "rate limit exceeded",
        );
        error_response(StatusCode::TOO_MANY_REQUESTS)
    }
}

fn client_ip(req: &Request) -> IpAddr {
    req.extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip())
        .unwrap_or(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
}

// ---------------------------------------------------------------------------
// 共通レスポンスヘルパ
// ---------------------------------------------------------------------------

const CONTENT_TYPE_TEXT: &str = "text/plain; charset=Shift_JIS";
const CONTENT_TYPE_HTML: &str = "text/html; charset=Shift_JIS";

/// SJIS テキストを `text/plain; charset=Shift_JIS` として返す(gzip なし — identity 固定)。
fn text_response(text: &str) -> Response {
    let body = sjis::encode(text);
    (
        [
            (header::CONTENT_TYPE, CONTENT_TYPE_TEXT),
            (header::CONTENT_ENCODING, "identity"),
        ],
        body,
    )
        .into_response()
}

/// SJIS HTML を `text/html; charset=Shift_JIS` として返す(bbs.cgi 応答ページ用)。
fn html_response(html: &str) -> Response {
    let body = sjis::encode(html);
    ([(header::CONTENT_TYPE, CONTENT_TYPE_HTML)], body).into_response()
}

// ---------------------------------------------------------------------------
// T054: 読み出し系エンドポイント
// ---------------------------------------------------------------------------

/// `GET /{board}/subject.txt` — スレ一覧(アクティブ + 凍結)。
async fn subject_txt(State(state): State<CompatState>, Path(board): Path<String>) -> Response {
    let Some(snapshot) = resolve_snapshot(&state, &board) else {
        return error_response(StatusCode::NOT_FOUND);
    };
    let mut out = String::new();
    // アクティブスレ 1 行(Closed は一覧から外す — クローズ済みは「保持していない」扱い)。
    if snapshot.active.state != ThreadState::Closed {
        push_subject_line(&mut out, &snapshot.active);
    }
    // 凍結スレを保持していればその行も追加する(compat-api.md §subject.txt)。
    if let Some(frozen) = &snapshot.frozen {
        push_subject_line(&mut out, frozen);
    }
    text_response(&out)
}

/// subject.txt の 1 行(`<key>.dat<>スレタイトル (レス数)`)を追加する。
///
/// タイトルは dat 本文(dat::format_line)と同じ一意規則でエスケープする(T054 レビュー
/// 対応 — 未エスケープだとタイトルに `<>` が含まれた場合に subject.txt のフィールド
/// 区切りが壊れる)。
fn push_subject_line(out: &mut String, thread: &crate::livechat::thread::Thread) {
    use std::fmt::Write as _;
    let _ = writeln!(
        out,
        "{}.dat<>{} ({})",
        thread.key,
        dat::escape(&thread.title),
        thread.res.len()
    );
}

/// `GET /{board}/SETTING.TXT` — 板設定提示(FR-027)。
async fn setting_txt(State(state): State<CompatState>, Path(board): Path<String>) -> Response {
    let Some(snapshot) = resolve_snapshot(&state, &board) else {
        return error_response(StatusCode::NOT_FOUND);
    };
    let s = &snapshot.settings;
    let text = format!(
        "BBS_TITLE={}\nBBS_NONAME_NAME={}\nBBS_LINE_NUMBER=32\nBBS_MESSAGE_COUNT=2048\nBBS_NAME_COUNT=64\nBBS_MAIL_COUNT=64\nBBS_MAX_RES={}\n",
        s.title, s.noname_name, s.res_limit
    );
    text_response(&text)
}

/// `GET /{board}/head.txt` — ローカルルール(Markdown を平文のまま返す — research R7)。
async fn head_txt(State(state): State<CompatState>, Path(board): Path<String>) -> Response {
    let Some(snapshot) = resolve_snapshot(&state, &board) else {
        return error_response(StatusCode::NOT_FOUND);
    };
    text_response(&snapshot.settings.local_rules)
}

/// `GET /{board}/dat/{key}.dat` — スレ本文(T055 — dat.rs へ委譲)。
async fn dat_file(
    State(state): State<CompatState>,
    Path((board, key_dat)): Path<(String, String)>,
    req: Request,
) -> Response {
    let Some(key_str) = key_dat.strip_suffix(".dat") else {
        return error_response(StatusCode::NOT_FOUND);
    };
    let Ok(key) = key_str.parse::<u64>() else {
        return error_response(StatusCode::NOT_FOUND);
    };
    // 自板は registry の thread_by_key(現行 + 直近凍結世代)。無ければ他ノード板の
    // 常駐セッション(T069)から現行世代を合成する(前世代は保持しないため key 不一致は 404)。
    let Some(thread) = state.registry.thread_by_key(&board, key).or_else(|| {
        resolve_snapshot(&state, &board)
            .map(|snap| snap.active)
            .filter(|t| t.key == key)
    }) else {
        return error_response(StatusCode::NOT_FOUND);
    };
    // Closed(クローズ済み・データ削除済み)は取得済み分も含めて 404
    // (close_thread がホスト側の確定レスも揮発させるため、res は既に空になっている —
    // ここでの明示チェックは意図を読みやすくするための防御的な二重確認)。
    if thread.state == ThreadState::Closed {
        return error_response(StatusCode::NOT_FOUND);
    }

    // T055 レビュー対応: dat 出力は Thread.res(既に確定時点の名無し名を焼き込み済み)
    // のみから決まり、現行の板設定(noname_name)には一切依存しない
    // (dat.rs モジュール doc §dat 追記不変性 参照)。
    let body_text = dat::render(&thread);
    let body_bytes = sjis::encode(&body_text);

    // 条件付き GET(If-Modified-Since)— dat の Last-Modified は
    // Thread::last_confirmed_at(ホスト時計基準で単調 — T055 レビュー対応)。
    // 投稿者申告の created_at は未検証のため使わない(キャッシュ汚染攻撃の防止)。
    let last_modified_unix = thread.last_confirmed_at;
    if let Some(since) = req
        .headers()
        .get(header::IF_MODIFIED_SINCE)
        .and_then(|v| v.to_str().ok())
        .and_then(dat::parse_http_date)
        && since >= last_modified_unix
    {
        return not_modified(last_modified_unix);
    }

    // Range 対応(206/416)。
    if let Some(range_header) = req
        .headers()
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
    {
        return match dat::parse_range(range_header, body_bytes.len()) {
            Some((from, to)) => partial_content(&body_bytes, from, to, last_modified_unix),
            None => range_not_satisfiable(body_bytes.len()),
        };
    }

    let mut resp = ([(header::CONTENT_ENCODING, "identity")], body_bytes).into_response();
    apply_dat_headers(resp.headers_mut(), last_modified_unix);
    resp
}

fn apply_dat_headers(headers: &mut axum::http::HeaderMap, last_modified_unix: i64) {
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(CONTENT_TYPE_TEXT),
    );
    if let Ok(v) = HeaderValue::from_str(&dat::format_http_date(last_modified_unix)) {
        headers.insert(header::LAST_MODIFIED, v);
    }
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
}

fn not_modified(last_modified_unix: i64) -> Response {
    let mut resp = StatusCode::NOT_MODIFIED.into_response();
    if let Ok(v) = HeaderValue::from_str(&dat::format_http_date(last_modified_unix)) {
        resp.headers_mut().insert(header::LAST_MODIFIED, v);
    }
    resp
}

fn partial_content(body: &[u8], from: usize, to: usize, last_modified_unix: i64) -> Response {
    let slice = body[from..=to].to_vec();
    let mut resp = (
        StatusCode::PARTIAL_CONTENT,
        [(header::CONTENT_ENCODING, "identity")],
        slice,
    )
        .into_response();
    apply_dat_headers(resp.headers_mut(), last_modified_unix);
    if let Ok(v) = HeaderValue::from_str(&format!("bytes {from}-{to}/{}", body.len())) {
        resp.headers_mut().insert(header::CONTENT_RANGE, v);
    }
    resp
}

fn range_not_satisfiable(content_len: usize) -> Response {
    let mut resp = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
    if let Ok(v) = HeaderValue::from_str(&format!("bytes */{content_len}")) {
        resp.headers_mut().insert(header::CONTENT_RANGE, v);
    }
    resp
}

// ---------------------------------------------------------------------------
// T056: bbs.cgi
// ---------------------------------------------------------------------------

/// `POST /test/bbs.cgi` — 書き込み(bbs_cgi.rs へ委譲)。
async fn bbs_cgi_handler(State(state): State<CompatState>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(b) => b,
        Err(_) => {
            let ip = client_ip(&Request::from_parts(parts, Body::empty()));
            state.security.log(
                SecurityCategory::CompatBbsDenied,
                &ip.to_string(),
                "body too large",
            );
            return error_response(StatusCode::PAYLOAD_TOO_LARGE);
        }
    };

    let form = match bbs_cgi::parse_form(&bytes) {
        Ok(f) => f,
        Err(e) => return html_response(&bbs_cgi::error_page(e)),
    };

    let created_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    match bbs_cgi::submit(&state.registry, &state.board_keys, &form, created_at) {
        Ok(_) => html_response(&bbs_cgi::success_page()),
        // 他ノード板(T069): registry に無い板は、開いている常駐セッション経由で書き込む
        // (FR-028 — 通常経路と同一の RES 送信。板鍵解決・初回 PoW はマネージャが担う)。未
        // オープン・未知板は元の定型エラーを返す(内部状態を開示しない)。
        Err(bbs_cgi::BbsCgiError::UnknownBoard) => {
            match state.manager.write(
                &form.bbs,
                form.from.clone(),
                form.mail.clone(),
                form.message.clone(),
            ) {
                Ok(()) => html_response(&bbs_cgi::success_page()),
                Err(_) => html_response(&bbs_cgi::error_page(bbs_cgi::BbsCgiError::UnknownBoard)),
            }
        }
        Err(e) => html_response(&bbs_cgi::error_page(e)),
    }
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::livechat::thread::BoardSettings;
    use axum::body::to_bytes;
    use axum::http::Method;
    use nostr::Keys;
    use tower::ServiceExt;

    const GUID: &str = "0123456789abcdef0123456789abcdef";
    const GOOD_HOST: &str = "127.0.0.1:7183";

    fn channel_of(board_id: &str) -> String {
        format!("30311:{board_id}:{GUID}")
    }

    fn test_state() -> CompatState {
        let registry = LivechatRegistry::new(128);
        let board_keys = Arc::new(BoardKeyManager::new(
            Arc::new(crate::store::Store::open_in_memory().unwrap()),
            crate::identity::Keystore::ephemeral(),
        ));
        let dir = tempfile::tempdir().unwrap();
        let security = Arc::new(SecurityLog::new(dir.path().join("s.log")).unwrap());
        std::mem::forget(dir);
        let mut hosts = std::collections::HashSet::new();
        hosts.insert(GOOD_HOST.to_string());
        hosts.insert("localhost:7183".to_string());
        let manager = ParticipantManager::new(Arc::clone(&board_keys), None);
        CompatState {
            registry,
            board_keys,
            manager,
            security,
            allowed_hosts: Arc::new(hosts),
            rate_limiter: Arc::new(RateLimiter::with_clock(1000, Box::new(|| 1_000))),
        }
    }

    fn open_test_board(state: &CompatState, persona: &Keys, settings: BoardSettings) -> String {
        let board_id = persona.public_key().to_hex();
        state
            .registry
            .open_thread(
                persona.clone(),
                channel_of(&board_id),
                1,
                1_700_000_000,
                "実況スレ",
                settings,
                "198.51.100.1:7147",
            )
            .unwrap();
        board_id
    }

    fn get_request(uri: &str, host: Option<&str>) -> Request<Body> {
        let mut b = Request::builder().method(Method::GET).uri(uri);
        if let Some(h) = host {
            b = b.header(header::HOST, h);
        }
        let mut req = b.body(Body::empty()).unwrap();
        let addr: SocketAddr = "127.0.0.1:50001".parse().unwrap();
        req.extensions_mut().insert(ConnectInfo(addr));
        req
    }

    async fn body_text(resp: Response) -> String {
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        sjis::decode(&bytes)
    }

    // --- Host 検証 -----------------------------------------------------------

    #[tokio::test]
    async fn unknown_host_is_rejected_with_403() {
        let state = test_state();
        let app = routes(state);
        let resp = app
            .oneshot(get_request(
                "/ab/subject.txt",
                Some("evil.example.com:7183"),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn missing_host_header_is_rejected() {
        let state = test_state();
        let app = routes(state);
        let resp = app
            .oneshot(get_request("/ab/subject.txt", None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn localhost_host_header_is_accepted() {
        let state = test_state();
        let mut hosts = std::collections::HashSet::new();
        hosts.insert("localhost:7183".to_string());
        let state = CompatState {
            allowed_hosts: Arc::new(hosts),
            ..state
        };
        let app = routes(state);
        let resp = app
            .oneshot(get_request(
                "/unknownboard/subject.txt",
                Some("localhost:7183"),
            ))
            .await
            .unwrap();
        // Host 検証は通り、板が未知のため 404(403 ではない)。
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // --- subject.txt -----------------------------------------------------------

    #[tokio::test]
    async fn subject_txt_unknown_board_is_404() {
        let state = test_state();
        let app = routes(state);
        let resp = app
            .oneshot(get_request("/ff/subject.txt", Some(GOOD_HOST)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn subject_txt_lists_active_thread() {
        let state = test_state();
        let persona = Keys::generate();
        let board_id = open_test_board(&state, &persona, BoardSettings::default());
        let app = routes(state);
        let resp = app
            .oneshot(get_request(
                &format!("/{board_id}/subject.txt"),
                Some(GOOD_HOST),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let content_type = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert_eq!(content_type, CONTENT_TYPE_TEXT);
        let text = body_text(resp).await;
        assert_eq!(text, "1700000000.dat<>実況スレ (0)\n");
    }

    #[tokio::test]
    async fn subject_txt_escapes_title_with_angle_brackets() {
        // T054 レビュー対応: タイトルに <> が含まれると subject.txt のフィールド区切りが
        // 壊れるため、dat 本文と同じ一意規則でエスケープされることを確認する。
        let state = test_state();
        let persona = Keys::generate();
        let board_id = persona.public_key().to_hex();
        state
            .registry
            .open_thread(
                persona.clone(),
                channel_of(&board_id),
                1,
                1_700_000_000,
                "実況<スレ>",
                BoardSettings::default(),
                "198.51.100.1:7147",
            )
            .unwrap();
        let app = routes(state);
        let resp = app
            .oneshot(get_request(
                &format!("/{board_id}/subject.txt"),
                Some(GOOD_HOST),
            ))
            .await
            .unwrap();
        let text = body_text(resp).await;
        assert_eq!(text, "1700000000.dat<>実況&lt;スレ&gt; (0)\n");
        // エスケープ後、行内の <> はフィールド区切り(1 個)としてのみ現れる。
        let line = text.lines().next().unwrap();
        assert_eq!(line.matches("<>").count(), 1);
    }

    // --- SETTING.TXT -------------------------------------------------------------

    #[tokio::test]
    async fn setting_txt_reports_board_settings() {
        let state = test_state();
        let persona = Keys::generate();
        let board_id = open_test_board(
            &state,
            &persona,
            BoardSettings {
                title: "実況板".into(),
                res_limit: 500,
                noname_name: "名無しさん".into(),
                ..Default::default()
            },
        );
        let app = routes(state);
        let resp = app
            .oneshot(get_request(
                &format!("/{board_id}/SETTING.TXT"),
                Some(GOOD_HOST),
            ))
            .await
            .unwrap();
        let text = body_text(resp).await;
        assert!(text.contains("BBS_TITLE=実況板"));
        assert!(text.contains("BBS_NONAME_NAME=名無しさん"));
        assert!(text.contains("BBS_MAX_RES=500"));
        assert!(text.contains("BBS_MESSAGE_COUNT=2048"));
    }

    // --- head.txt ------------------------------------------------------------

    #[tokio::test]
    async fn head_txt_returns_plain_markdown() {
        let state = test_state();
        let persona = Keys::generate();
        let board_id = open_test_board(
            &state,
            &persona,
            BoardSettings {
                local_rules: "# ルール\n**荒らし禁止**".into(),
                ..Default::default()
            },
        );
        let app = routes(state);
        let resp = app
            .oneshot(get_request(
                &format!("/{board_id}/head.txt"),
                Some(GOOD_HOST),
            ))
            .await
            .unwrap();
        let text = body_text(resp).await;
        assert_eq!(
            text, "# ルール\n**荒らし禁止**",
            "Markdown を平文のまま返す(描画しない)"
        );
    }

    // --- dat -------------------------------------------------------------------

    #[tokio::test]
    async fn dat_unknown_key_is_404() {
        let state = test_state();
        let persona = Keys::generate();
        let board_id = open_test_board(&state, &persona, BoardSettings::default());
        let app = routes(state);
        let resp = app
            .oneshot(get_request(
                &format!("/{board_id}/dat/9999999999.dat"),
                Some(GOOD_HOST),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn dat_returns_confirmed_res_only() {
        let state = test_state();
        let persona = Keys::generate();
        let board_id = open_test_board(&state, &persona, BoardSettings::default());
        let board_key = Keys::generate();
        state
            .registry
            .seed_confirmed_res(
                &board_id,
                &crate::livechat::registry::sign_res(
                    &board_key,
                    &board_id,
                    &channel_of(&board_id),
                    1,
                    "一つ目",
                    1_700_000_001,
                )
                .unwrap(),
                1_700_000_001,
            )
            .unwrap();
        let app = routes(state);
        let resp = app
            .oneshot(get_request(
                &format!("/{board_id}/dat/1700000000.dat"),
                Some(GOOD_HOST),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let text = body_text(resp).await;
        assert!(text.contains("一つ目"));
    }

    #[tokio::test]
    async fn dat_supports_range_partial_content() {
        let state = test_state();
        let persona = Keys::generate();
        let board_id = open_test_board(&state, &persona, BoardSettings::default());
        let board_key = Keys::generate();
        state
            .registry
            .seed_confirmed_res(
                &board_id,
                &crate::livechat::registry::sign_res(
                    &board_key,
                    &board_id,
                    &channel_of(&board_id),
                    1,
                    "本文",
                    1_700_000_001,
                )
                .unwrap(),
                1_700_000_001,
            )
            .unwrap();
        let app = routes(state);
        let mut req = get_request(&format!("/{board_id}/dat/1700000000.dat"), Some(GOOD_HOST));
        req.headers_mut()
            .insert(header::RANGE, HeaderValue::from_static("bytes=0-9"));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    }

    #[tokio::test]
    async fn dat_range_unsatisfiable_returns_416() {
        let state = test_state();
        let persona = Keys::generate();
        let board_id = open_test_board(&state, &persona, BoardSettings::default());
        let board_key = Keys::generate();
        state
            .registry
            .seed_confirmed_res(
                &board_id,
                &crate::livechat::registry::sign_res(
                    &board_key,
                    &board_id,
                    &channel_of(&board_id),
                    1,
                    "本文",
                    1_700_000_001,
                )
                .unwrap(),
                1_700_000_001,
            )
            .unwrap();
        let app = routes(state);
        let mut req = get_request(&format!("/{board_id}/dat/1700000000.dat"), Some(GOOD_HOST));
        req.headers_mut().insert(
            header::RANGE,
            HeaderValue::from_static("bytes=99999-100000"),
        );
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    }

    #[tokio::test]
    async fn dat_conditional_get_returns_304() {
        let state = test_state();
        let persona = Keys::generate();
        let board_id = open_test_board(&state, &persona, BoardSettings::default());
        let board_key = Keys::generate();
        state
            .registry
            .seed_confirmed_res(
                &board_id,
                &crate::livechat::registry::sign_res(
                    &board_key,
                    &board_id,
                    &channel_of(&board_id),
                    1,
                    "本文",
                    1_700_000_001,
                )
                .unwrap(),
                1_700_000_001,
            )
            .unwrap();
        let app = routes(state);
        let mut req = get_request(&format!("/{board_id}/dat/1700000000.dat"), Some(GOOD_HOST));
        // 最終レスの created_at(1_700_000_001)以降の If-Modified-Since を指定する。
        req.headers_mut().insert(
            header::IF_MODIFIED_SINCE,
            HeaderValue::from_str(&dat::format_http_date(1_700_000_001)).unwrap(),
        );
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
    }

    #[tokio::test]
    async fn dat_closed_thread_is_404() {
        let state = test_state();
        let persona = Keys::generate();
        let board_id = open_test_board(&state, &persona, BoardSettings::default());
        state
            .registry
            .close_thread(&board_id, 1_700_000_500)
            .unwrap();
        let app = routes(state);
        let resp = app
            .oneshot(get_request(
                &format!("/{board_id}/dat/1700000000.dat"),
                Some(GOOD_HOST),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // --- dat 追記不変性(HTTP 層 — MUST)-----------------------------------------

    #[tokio::test]
    async fn dat_response_is_prefix_stable_across_appends() {
        let state = test_state();
        let persona = Keys::generate();
        let board_id = open_test_board(&state, &persona, BoardSettings::default());
        let board_key = Keys::generate();
        state
            .registry
            .seed_confirmed_res(
                &board_id,
                &crate::livechat::registry::sign_res(
                    &board_key,
                    &board_id,
                    &channel_of(&board_id),
                    1,
                    "一つ目",
                    1_700_000_001,
                )
                .unwrap(),
                1_700_000_001,
            )
            .unwrap();

        let app1 = routes(state.clone());
        let resp1 = app1
            .oneshot(get_request(
                &format!("/{board_id}/dat/1700000000.dat"),
                Some(GOOD_HOST),
            ))
            .await
            .unwrap();
        let before = to_bytes(resp1.into_body(), usize::MAX).await.unwrap();

        state
            .registry
            .seed_confirmed_res(
                &board_id,
                &crate::livechat::registry::sign_res(
                    &board_key,
                    &board_id,
                    &channel_of(&board_id),
                    1,
                    "二つ目",
                    1_700_000_002,
                )
                .unwrap(),
                1_700_000_002,
            )
            .unwrap();

        let app2 = routes(state);
        let resp2 = app2
            .oneshot(get_request(
                &format!("/{board_id}/dat/1700000000.dat"),
                Some(GOOD_HOST),
            ))
            .await
            .unwrap();
        let after = to_bytes(resp2.into_body(), usize::MAX).await.unwrap();

        assert!(
            after.starts_with(&before),
            "追記後も既存部分は HTTP 応答としてバイト列不変(dat 追記不変性 MUST)"
        );
    }

    // --- bbs.cgi ---------------------------------------------------------------

    #[tokio::test]
    async fn bbs_cgi_success_returns_expected_title() {
        let state = test_state();
        let persona = Keys::generate();
        let board_id = open_test_board(
            &state,
            &persona,
            BoardSettings {
                first_post_pow_bits: 0,
                ..Default::default()
            },
        );
        let app = routes(state);
        let body = format!("bbs={board_id}&key=1700000000&FROM=&mail=&MESSAGE=%96%7B%95%B6");
        let req = Request::builder()
            .method(Method::POST)
            .uri("/test/bbs.cgi")
            .header(header::HOST, GOOD_HOST)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap();
        let mut req = req;
        let addr: SocketAddr = "127.0.0.1:50002".parse().unwrap();
        req.extensions_mut().insert(ConnectInfo(addr));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let text = body_text(resp).await;
        assert!(text.contains("書きこみました。"));
    }

    #[tokio::test]
    async fn bbs_cgi_subject_thread_creation_is_rejected() {
        let state = test_state();
        let persona = Keys::generate();
        let board_id = open_test_board(&state, &persona, BoardSettings::default());
        let app = routes(state);
        let body = format!("bbs={board_id}&MESSAGE=test&subject=new");
        let req = Request::builder()
            .method(Method::POST)
            .uri("/test/bbs.cgi")
            .header(header::HOST, GOOD_HOST)
            .body(Body::from(body))
            .unwrap();
        let mut req = req;
        let addr: SocketAddr = "127.0.0.1:50003".parse().unwrap();
        req.extensions_mut().insert(ConnectInfo(addr));
        let resp = app.oneshot(req).await.unwrap();
        let text = body_text(resp).await;
        assert!(text.contains("<title>ERROR!</title>"));
        assert!(text.contains("ERROR:"));
    }

    #[tokio::test]
    async fn bbs_cgi_never_sets_cookie() {
        let state = test_state();
        let persona = Keys::generate();
        let board_id = open_test_board(
            &state,
            &persona,
            BoardSettings {
                first_post_pow_bits: 0,
                ..Default::default()
            },
        );
        let app = routes(state);
        let body = format!("bbs={board_id}&key=1700000000&MESSAGE=test");
        let req = Request::builder()
            .method(Method::POST)
            .uri("/test/bbs.cgi")
            .header(header::HOST, GOOD_HOST)
            .body(Body::from(body))
            .unwrap();
        let mut req = req;
        let addr: SocketAddr = "127.0.0.1:50004".parse().unwrap();
        req.extensions_mut().insert(ConnectInfo(addr));
        let resp = app.oneshot(req).await.unwrap();
        assert!(
            resp.headers().get(header::SET_COOKIE).is_none(),
            "Cookie は発行しない(loopback 限定のため不要 — FR-026)"
        );
    }

    // --- レート制限 ----------------------------------------------------------

    #[tokio::test]
    async fn rate_limit_exceeded_returns_429() {
        let mut state = test_state();
        state.rate_limiter = Arc::new(RateLimiter::with_clock(1, Box::new(|| 1_000)));
        let app = routes(state);
        let resp1 = app
            .clone()
            .oneshot(get_request("/ab/subject.txt", Some(GOOD_HOST)))
            .await
            .unwrap();
        assert_eq!(resp1.status(), StatusCode::NOT_FOUND); // 板未知だが 1 件目は通る
        let resp2 = app
            .oneshot(get_request("/ab/subject.txt", Some(GOOD_HOST)))
            .await
            .unwrap();
        assert_eq!(resp2.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    // --- 定型 404 --------------------------------------------------------------

    #[tokio::test]
    async fn undefined_path_is_404() {
        let state = test_state();
        let app = routes(state);
        let resp = app
            .oneshot(get_request("/nonexistent/path", Some(GOOD_HOST)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}

//! 実況スレ Web UI / ローカル API(T024/T025 — spec.md `livechat-thread`)
//!
//! - [`render_local_rules_html`](T025): ローカルルールの Markdown を安全な
//!   サブセットのみ HTML へ描画する(FR-025 / research R7)。
//! - (T024) `GET /api/v1/livechat/threads` / `GET /api/v1/livechat/threads/{board_id}`:
//!   announce 由来のスレ一覧・板設定参照(タイトル・名無しのデフォルト名・
//!   ローカルルール)・確定レス閲覧。供給元は [`LivechatDirectory`] を注入する
//!   (`web/announced.rs` の `AnnouncedProvider` と同一パターン)。
//!
//! ## 供給元の配線(本タスクでは自己完結)
//!
//! [`LivechatDirectory`] の**トレイト定義のみ**を本モジュールが持ち、具体的な実体
//! (自板 = `crate::livechat::registry::LivechatRegistry`、他ノード板 = gossip 受信
//! 31311)を束ねた実装は別担当が配線する(`src/main.rs` の起動配線箇所に TODO
//! コメントあり)。`registry.rs`・`event/view.rs`・`p2p/hub.rs` はいずれも本タスクの
//! 変更範囲外(並行編集中)であり、本モジュールはそれらに依存しない。
//!
//! ## US1 のスコープ(重要な制約)
//!
//! - **閲覧専用**: 確定レスの表示・板設定参照のみ。書き込み・継続受信ループは US2。
//! - **「スレを開く」操作は本タスクではスタブ**: 実接続(`crate::livechat::participant`)
//!   の起動は非同期 TCP ハンドシェイクを伴い、結果の保持・ポーリング方式の設計が
//!   US1 の一覧/閲覧 API より優先度が低いため、シグネチャのみ用意し 501 を返す。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, TagEnd, html};
use serde::Serialize;

use super::{AppState, error_response};

/// ローカルルールの Markdown を安全なサブセットのみ HTML へ描画する(FR-025)。
///
/// 見出し・強調・リスト・引用・コード・段落・http(s) リンクは通常どおり描画するが、
/// 以下の 2 点でイベントストリームを加工し、生成前に危険な要素を除去する
/// (生成後のサニタイズより攻撃面が小さい — research R7):
///
/// 1. **raw HTML の破棄**: Markdown 中に埋め込まれた生 HTML(`Event::Html` /
///    `Event::InlineHtml`。`<script>` 等)は捨てる。pulldown-cmark はデフォルトで
///    raw HTML をそのまま透過するため、ここで明示的にフィルタしないと
///    `push_html` がそのまま出力に混ぜてしまう。
/// 2. **非 http(s) リンクの無効化**: リンク先スキームが http/https 以外
///    (`javascript:` / `data:` / `mailto:` 等)なら `Tag::Link` / `TagEnd::Link`
///    イベントごと除去し、リンクテキストのみを平文として残す(001 FR-012 の
///    URL 安全性規則と同じ判定基準)。
///
/// 通常テキストは pulldown-cmark の [`html::push_html`] が既定で HTML エスケープ
/// するため、生成した HTML をそのまま UI に挿入しても XSS を起こさない。
pub fn render_local_rules_html(markdown: &str) -> String {
    let parser = Parser::new_ext(markdown, Options::empty());
    let mut skip_link = false;
    let events = parser.filter_map(|event| match event {
        // raw HTML は破棄(要件 1)。
        Event::Html(_) | Event::InlineHtml(_) => None,
        Event::Start(Tag::Link { dest_url, .. }) if !is_http_or_https(&dest_url) => {
            // 対応する End(Link) も揃えて捨てるため状態を持つ(要件 2)。
            skip_link = true;
            None
        }
        Event::End(TagEnd::Link) if skip_link => {
            skip_link = false;
            None
        }
        other => Some(other),
    });
    let mut html_out = String::new();
    html::push_html(&mut html_out, events);
    html_out
}

/// URL のスキームが http/https か(大文字小文字を区別しない)。
/// 001 FR-012([`crate::security::url_needs_warning`])と同じ判定基準を用いる。
fn is_http_or_https(url: &CowStr<'_>) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

// ---------------------------------------------------------------------------
// T024: スレ一覧・板設定参照
// ---------------------------------------------------------------------------

/// スレ一覧の 1 行(`GET /api/v1/livechat/threads` の要素 — announce 相当)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ThreadSummary {
    /// スレ主(配信者)ペルソナの公開鍵(hex 64)。板の識別子。
    pub board_id: String,
    /// 対象チャンネル(`30311:<pubkey>:<guid>`)。
    pub channel: String,
    /// スレタイトル。
    pub title: String,
    /// スレ世代。
    pub generation: u32,
    /// 現在の確定レス数(参考値 — announce の `res_count` と同じ扱い)。
    pub res_count: u64,
    /// ホスト接続先 `ip:port`(「スレを開く」操作の接続先)。
    pub tip: String,
    /// 自ノードがホストしている板か(`true` = 自板、`false` = 他ノード板)。
    pub is_local: bool,
}

/// 板設定参照(タイトル・名無しのデフォルト名・ローカルルール等 — FR-022)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BoardSettingsView {
    pub title: String,
    pub noname_name: String,
    pub res_limit: u16,
    /// ローカルルールの Markdown 原文(互換 API 用途 — compat-api.md は平文出力)。
    pub local_rules: String,
    /// [`render_local_rules_html`] で描画した安全な HTML(UI が直接挿入する値 — FR-025)。
    pub local_rules_html: String,
    pub first_post_pow_bits: u8,
}

impl BoardSettingsView {
    /// [`crate::livechat::thread::BoardSettings`] から表示用ビューを組み立てる
    /// (Markdown → HTML 描画を一度だけ行う純粋関数)。
    pub fn from_settings(settings: &crate::livechat::thread::BoardSettings) -> Self {
        BoardSettingsView {
            title: settings.title.clone(),
            noname_name: settings.noname_name.clone(),
            res_limit: settings.res_limit,
            local_rules: settings.local_rules.clone(),
            local_rules_html: render_local_rules_html(&settings.local_rules),
            first_post_pow_bits: settings.first_post_pow_bits,
        }
    }
}

/// 確定レス 1 件の表示用ビュー(`GET /api/v1/livechat/threads/{board_id}` の要素)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResView {
    /// 確定後のレス番号(1 始まり)。未確定分はこのビューに含めない(閲覧専用 — US1)。
    pub res_no: u16,
    /// 名前欄(空・省略時は確定時点の `noname_name` で表示済みの値 — FR-023)。
    pub name: String,
    /// メール欄(表示互換のみ — FR-029)。
    pub mail: String,
    /// 本文。
    pub body: String,
    /// 参考情報(正となる順序は `res_no` — spec Edge Case)。
    pub created_at: i64,
}

impl ResView {
    /// [`crate::livechat::thread::Res`] から表示用ビューを組み立てる。
    ///
    /// `res_no` が `None`(未確定)のレスは呼び出し側でフィルタする前提のため、
    /// 本関数は `res_no: Some(_)` のみを受け付ける(`None` は `None` を返す)。
    pub fn from_res(res: &crate::livechat::thread::Res, noname_name: &str) -> Option<Self> {
        let res_no = res.res_no?;
        Some(ResView {
            res_no,
            name: res
                .name
                .clone()
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| noname_name.to_string()),
            mail: res.mail.clone().unwrap_or_default(),
            body: res.body.clone(),
            created_at: res.created_at,
        })
    }
}

/// スレ詳細(`GET /api/v1/livechat/threads/{board_id}` の応答本体)。
///
/// 板設定と確定レス一覧をまとめて返す(閲覧に板鍵は不要 — FR-016)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ThreadDetail {
    pub settings: BoardSettingsView,
    /// 確定レス一覧(res_no 昇順)。
    pub res: Vec<ResView>,
}

/// 実況スレ一覧・詳細の供給元(自板 = ホストレジストリ、他ノード板 = gossip 受信 31311)。
///
/// `web/announced.rs` の `AnnouncedProvider` と同じ注入パターン。**本モジュールはトレイト
/// 定義のみを持ち、具体的な実装(レジストリ・gossip ハブを束ねる適合層)は別担当が
/// `src/main.rs` で配線する**(モジュール doc 参照)。
pub trait LivechatDirectory: Send + Sync {
    /// 見えている全スレの一覧(自板 + 他ノード板)。
    fn threads(&self) -> Vec<ThreadSummary>;
    /// 指定 board_id のスレ詳細(板設定 + 確定レス一覧)。未知 board_id は `None`。
    fn thread(&self, board_id: &str) -> Option<ThreadDetail>;
}

/// `/api/v1/livechat` エンドポイント群のサブルーター。[`super::api_router`] が `.merge` する。
pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/livechat/threads", get(list_threads))
        .route("/livechat/threads/{board_id}", get(get_thread))
        .route(
            "/livechat/threads/{board_id}/join",
            axum::routing::post(join_thread),
        )
}

/// `GET /api/v1/livechat/threads` — 見えているスレ一覧。
///
/// 供給元未配線(`AppState.livechat_directory` が `None` — スレ機能無効時 or 未配線)は
/// 空一覧。
async fn list_threads(State(state): State<AppState>) -> Response {
    let threads = state
        .livechat_directory
        .as_ref()
        .map(|d| d.threads())
        .unwrap_or_default();
    Json(threads).into_response()
}

/// `GET /api/v1/livechat/threads/{board_id}` — 板設定参照 + 確定レス閲覧。
///
/// 板鍵は閲覧に不要(FR-016)。認証は既存保護層(Host 検証・レート制限)に委ねる。
/// 未知 board_id・供給元未配線は `not_found`。
async fn get_thread(State(state): State<AppState>, Path(board_id): Path<String>) -> Response {
    let Some(directory) = state.livechat_directory.as_ref() else {
        return error_response(StatusCode::NOT_FOUND, "not_found");
    };
    match directory.thread(&board_id) {
        Some(detail) => Json(detail).into_response(),
        None => error_response(StatusCode::NOT_FOUND, "not_found"),
    }
}

/// `POST /api/v1/livechat/threads/{board_id}/join` — スレを開く操作(スタブ)。
///
/// モジュール doc の制約を参照。実接続(`crate::livechat::participant::connect_once`)の
/// 起動・結果保持はまだ配線していないため、シグネチャのみ用意し 501 を返す。
async fn join_thread(State(_state): State<AppState>, Path(_board_id): Path<String>) -> Response {
    error_response(StatusCode::NOT_IMPLEMENTED, "not_implemented")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::livechat::thread::BoardSettings;

    #[test]
    fn renders_heading_emphasis_list_and_link() {
        let md =
            "# 見出し\n\n**強調**と*斜体*\n\n- 項目1\n- 項目2\n\n[example](http://example.com)";
        let out = render_local_rules_html(md);
        assert!(out.contains("<h1>"), "見出しが h1 になる: {out}");
        assert!(out.contains("<strong>強調</strong>"), "強調: {out}");
        assert!(out.contains("<em>斜体</em>"), "斜体: {out}");
        assert!(out.contains("<li>項目1</li>"), "箇条書き: {out}");
        assert!(
            out.contains(r#"href="http://example.com""#),
            "http リンクは残る: {out}"
        );
    }

    #[test]
    fn strips_raw_script_tag() {
        let md = "本文\n\n<script>alert(1)</script>\n\n続き";
        let out = render_local_rules_html(md);
        assert!(!out.contains("<script"), "raw HTML は破棄される: {out}");
    }

    #[test]
    fn strips_inline_raw_html() {
        let md = "テキスト <b onclick=\"alert(1)\">太字</b> です";
        let out = render_local_rules_html(md);
        assert!(!out.contains("<b "), "インライン raw HTML も破棄: {out}");
        assert!(!out.contains("onclick"), "属性ごと消える: {out}");
    }

    #[test]
    fn disables_javascript_scheme_link() {
        let md = "[x](javascript:alert(1))";
        let out = render_local_rules_html(md);
        assert!(
            !out.contains("href=\"javascript:"),
            "javascript: リンクは無効化: {out}"
        );
    }

    #[test]
    fn allows_https_link() {
        let md = "[x](https://example.com/path)";
        let out = render_local_rules_html(md);
        assert!(
            out.contains(r#"href="https://example.com/path""#),
            "https リンクは残る: {out}"
        );
    }

    #[test]
    fn disables_data_scheme_link() {
        let md = "[x](data:text/html,<script>alert(1)</script>)";
        let out = render_local_rules_html(md);
        assert!(!out.contains("href=\"data:"), "data: リンクは無効化: {out}");
    }

    #[test]
    fn disables_mailto_scheme_link() {
        let md = "[x](mailto:a@example.com)";
        let out = render_local_rules_html(md);
        assert!(
            !out.contains("href=\"mailto:"),
            "mailto: リンクは無効化(http/https のみ許可): {out}"
        );
    }

    #[test]
    fn does_not_panic_on_2048_char_input() {
        let md = "あ".repeat(2048);
        let out = render_local_rules_html(&md);
        assert!(!out.is_empty());
    }

    #[test]
    fn escapes_plain_text_by_default() {
        // raw HTML タグではなく、地の文としての `<`/`>` はエスケープされる。
        let md = "1 < 2 かつ 3 > 2";
        let out = render_local_rules_html(md);
        assert!(out.contains("&lt;"), "< はエスケープ: {out}");
        assert!(out.contains("&gt;"), "> はエスケープ: {out}");
    }

    // --- T024: BoardSettingsView --------------------------------------------

    #[test]
    fn board_settings_view_renders_local_rules_html() {
        let settings = BoardSettings {
            title: "実況スレ".into(),
            res_limit: 1000,
            noname_name: "名無しさん".into(),
            local_rules: "# ルール\n\n**荒らし禁止**".into(),
            first_post_pow_bits: 20,
        };
        let view = BoardSettingsView::from_settings(&settings);
        assert_eq!(view.title, "実況スレ");
        assert_eq!(view.noname_name, "名無しさん");
        assert_eq!(view.res_limit, 1000);
        assert_eq!(view.first_post_pow_bits, 20);
        // local_rules は原文のまま保持しつつ、local_rules_html は安全 HTML 化される。
        assert_eq!(view.local_rules, "# ルール\n\n**荒らし禁止**");
        assert!(view.local_rules_html.contains("<h1>ルール</h1>"));
        assert!(
            view.local_rules_html
                .contains("<strong>荒らし禁止</strong>")
        );
    }

    #[test]
    fn board_settings_view_strips_dangerous_markdown_in_html_only() {
        let settings = BoardSettings {
            local_rules: "<script>alert(1)</script>".into(),
            ..BoardSettings::default()
        };
        let view = BoardSettingsView::from_settings(&settings);
        // 原文(local_rules)は保持するが、HTML 化した方には raw HTML が残らない。
        assert!(view.local_rules.contains("<script>"));
        assert!(!view.local_rules_html.contains("<script"));
    }

    // --- T024: ResView ---------------------------------------------------------

    fn confirmed_res(res_no: Option<u16>, name: Option<&str>) -> crate::livechat::thread::Res {
        crate::livechat::thread::Res {
            event_id: "11".repeat(32),
            board_key: "22".repeat(32),
            name: name.map(str::to_string),
            mail: None,
            body: "本文".into(),
            created_at: 1_700_000_000,
            res_no,
            pending: false,
        }
    }

    #[test]
    fn res_view_uses_noname_name_when_name_is_empty() {
        let res = confirmed_res(Some(1), None);
        let view = ResView::from_res(&res, "名無しさん").unwrap();
        assert_eq!(view.res_no, 1);
        assert_eq!(view.name, "名無しさん");
        assert_eq!(view.body, "本文");
    }

    #[test]
    fn res_view_keeps_explicit_name() {
        let res = confirmed_res(Some(2), Some("コテハン"));
        let view = ResView::from_res(&res, "名無しさん").unwrap();
        assert_eq!(view.name, "コテハン");
    }

    #[test]
    fn res_view_is_none_for_unconfirmed_res() {
        // res_no が None(未確定)のレスは閲覧 API に含めない(US1 は確定分のみ)。
        let res = confirmed_res(None, None);
        assert!(ResView::from_res(&res, "名無しさん").is_none());
    }

    // --- T024: LivechatDirectory 経由のハンドラ疎通 -----------------------------

    struct FakeDirectory {
        threads: Vec<ThreadSummary>,
    }

    impl LivechatDirectory for FakeDirectory {
        fn threads(&self) -> Vec<ThreadSummary> {
            self.threads.clone()
        }

        fn thread(&self, board_id: &str) -> Option<ThreadDetail> {
            if board_id == "ab".repeat(32) {
                Some(ThreadDetail {
                    settings: BoardSettingsView::from_settings(&BoardSettings {
                        title: "実況スレ".into(),
                        ..BoardSettings::default()
                    }),
                    res: vec![
                        ResView::from_res(&confirmed_res(Some(1), None), "名無しさん").unwrap(),
                    ],
                })
            } else {
                None
            }
        }
    }

    fn sample_summary() -> ThreadSummary {
        ThreadSummary {
            board_id: "ab".repeat(32),
            channel: format!("30311:{}:{}", "ab".repeat(32), "cd".repeat(16)),
            title: "実況スレ".into(),
            generation: 1,
            res_count: 3,
            tip: "198.51.100.1:7147".into(),
            is_local: true,
        }
    }

    fn state_with_directory(directory: Option<FakeDirectory>) -> AppState {
        use crate::security::SecurityLog;
        use crate::store::Store;
        use std::collections::HashSet;
        use std::sync::Arc;

        let store = Arc::new(Store::open_in_memory().unwrap());
        let dir = tempfile::tempdir().unwrap();
        let security = Arc::new(SecurityLog::new(dir.path().join("s.log")).unwrap());
        std::mem::forget(dir);
        let mut state = AppState::with_parts(
            store,
            security,
            "test-token",
            HashSet::new(),
            super::super::RateLimiter::per_second(100),
        );
        if let Some(d) = directory {
            state = state.with_livechat_directory(std::sync::Arc::new(d));
        }
        state
    }

    async fn body_json(resp: Response) -> serde_json::Value {
        use http_body_util::BodyExt;
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn list_threads_returns_directory_summaries() {
        let directory = FakeDirectory {
            threads: vec![sample_summary()],
        };
        let resp = list_threads(State(state_with_directory(Some(directory)))).await;
        let json = body_json(resp).await;
        assert_eq!(json[0]["title"], "実況スレ");
        assert_eq!(json[0]["res_count"], 3);
        assert_eq!(json[0]["is_local"], true);
    }

    #[tokio::test]
    async fn list_threads_returns_empty_when_unwired() {
        let resp = list_threads(State(state_with_directory(None))).await;
        let json = body_json(resp).await;
        assert_eq!(json.as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn get_thread_returns_settings_and_res_for_known_board() {
        let directory = FakeDirectory { threads: vec![] };
        let state = state_with_directory(Some(directory));
        let resp = get_thread(State(state), Path("ab".repeat(32))).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        assert_eq!(json["settings"]["title"], "実況スレ");
        assert!(json["settings"]["local_rules_html"].is_string());
        assert_eq!(json["res"][0]["res_no"], 1);
        assert_eq!(json["res"][0]["body"], "本文");
    }

    #[tokio::test]
    async fn get_thread_returns_not_found_for_unknown_board() {
        let directory = FakeDirectory { threads: vec![] };
        let state = state_with_directory(Some(directory));
        let resp = get_thread(State(state), Path("ff".repeat(32))).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn get_thread_returns_not_found_when_unwired() {
        let resp = get_thread(State(state_with_directory(None)), Path("ab".repeat(32))).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn join_thread_is_not_implemented_stub() {
        let state = state_with_directory(None);
        let resp = join_thread(State(state), Path("ab".repeat(32))).await;
        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    }
}

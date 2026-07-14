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
//!
//! ## US4(T045)の追加分
//!
//! - [`board_compat_url`]: 互換 API 板 URL(コピー用)を返す純粋関数。モデレーション
//!   (NG/BAN/ローテーション)の HTTP エンドポイントは `AppState` 配線が未完(T024 の
//!   宿題)のため本タスクでは追加しない — ドメイン層([`crate::livechat::moderation`] /
//!   [`crate::livechat::registry::LivechatRegistry`])はテスト経由で直接検証する。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, TagEnd, html};
use serde::{Deserialize, Serialize};

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

/// 互換 API 板 URL(T045)。`http://127.0.0.1:7183/<board_id>/` — コピー用の表示値。
///
/// 互換 API(FR-026 — loopback 限定)は既定ポート 7183 の固定値を用いる
/// (contracts/compat-api.md)。モデレーション HTTP エンドポイントは AppState 配線が
/// 未完(T024 の宿題)のため、本タスクでは独立した純粋関数として追加するに留め、
/// `ThreadSummary`/`AppState` へは組み込まない(既存の破壊を避ける)。
pub fn board_compat_url(board_id: &str) -> String {
    format!("http://127.0.0.1:7183/{board_id}/")
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

/// 操作 API(変更系)の失敗理由(定型 — 内部情報を漏らさない Principle II)。
///
/// HTTP へは [`Self::into_response`] で写像する。存在しない対象と未サポートは区別せず
/// `not_found` に丸める(内部の配線状態を攻撃者へ開示しない)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LivechatOpError {
    /// 供給元が当該操作を持たない(未配線・読み取り専用フェイク)。→ 404
    Unsupported,
    /// 対象チャンネル/板が見つからない(未掲載・他ノード板・未開設)。→ 404
    NotFound,
    /// 入力が不正(板設定の値域違反など)。→ 400
    Invalid,
    /// 機能無効・鍵/keystore 利用不可・到達不能(tip 未確定)で実行できない。→ 503
    Unavailable,
}

impl LivechatOpError {
    fn into_response(self) -> Response {
        let (status, code) = match self {
            LivechatOpError::Unsupported | LivechatOpError::NotFound => {
                (StatusCode::NOT_FOUND, "not_found")
            }
            LivechatOpError::Invalid => (StatusCode::BAD_REQUEST, "invalid"),
            LivechatOpError::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
        };
        error_response(status, code)
    }
}

/// 板設定の入力(POST スレ開設 / PUT 設定変更の本体)。全項目を持つ**全置換**入力
/// (省略項目は [`Default`] = [`crate::livechat::thread::BoardSettings::default`] と一致)。
///
/// レジストリ側 [`crate::livechat::registry::LivechatRegistry::update_settings`] は設定を
/// 丸ごと差し替えるため、UI は現行値を取得・編集して全項目を送る。値域検証・制御文字除去は
/// レジストリ側([`crate::livechat::thread::BoardSettings::validate`] / `sanitized`)が行う。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct BoardSettingsInput {
    pub title: String,
    pub res_limit: u16,
    pub noname_name: String,
    pub local_rules: String,
    pub first_post_pow_bits: u8,
}

impl Default for BoardSettingsInput {
    fn default() -> Self {
        let d = crate::livechat::thread::BoardSettings::default();
        BoardSettingsInput {
            title: d.title,
            res_limit: d.res_limit,
            noname_name: d.noname_name,
            local_rules: d.local_rules,
            first_post_pow_bits: d.first_post_pow_bits,
        }
    }
}

impl From<BoardSettingsInput> for crate::livechat::thread::BoardSettings {
    fn from(i: BoardSettingsInput) -> Self {
        crate::livechat::thread::BoardSettings {
            title: i.title,
            res_limit: i.res_limit,
            noname_name: i.noname_name,
            local_rules: i.local_rules,
            first_post_pow_bits: i.first_post_pow_bits,
        }
    }
}

/// スレ開設要求(T063 — `POST /api/v1/livechat/threads`)。
#[derive(Debug, Clone, Deserialize)]
pub struct OpenThreadRequest {
    /// 対象チャンネル(掲載中 30311 の `d` タグ = channel_id hex 32 小文字)。
    pub channel_id: String,
    /// スレタイトル(省略時は板設定の title を用いる)。
    #[serde(default)]
    pub title: Option<String>,
    /// 初期板設定(省略時は既定値)。
    #[serde(default)]
    pub settings: Option<BoardSettingsInput>,
}

/// スレ開設の結果(開設できた自板の board_id と世代)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OpenThreadResult {
    pub board_id: String,
    pub generation: u32,
}

/// 実況スレ一覧・詳細の供給元(自板 = ホストレジストリ、他ノード板 = gossip 受信 31311)。
///
/// `web/announced.rs` の `AnnouncedProvider` と同じ注入パターン。具体的な実装(レジストリ・
/// gossip ハブ・ペルソナ・板鍵を束ねる適合層)は `src/main.rs` の `LivechatAdapter` が配線する。
///
/// - **読み取り**(`threads`/`thread`): 自板 + 他ノード板。
/// - **US5 操作**(`next_thread`/`close_thread`): 自板のみ(他ノード板の操作は本ノードの権限外)。
/// - **T063/T067/T068 操作**(`open_thread`/`update_settings`/BAN/ローテーション): 既定実装は
///   [`LivechatOpError::Unsupported`] を返す(未配線・読み取り専用フェイク向け)。配線側が上書きする。
pub trait LivechatDirectory: Send + Sync {
    /// 見えている全スレの一覧(自板 + 他ノード板)。
    fn threads(&self) -> Vec<ThreadSummary>;
    /// 指定 board_id のスレ詳細(板設定 + 確定レス一覧)。未知 board_id は `None`。
    fn thread(&self, board_id: &str) -> Option<ThreadDetail>;
    /// 次スレ操作(T046 — FR-013)。自板の Active スレを Frozen にし新世代を開始する。
    /// 新世代の番号を返す。未知 board_id・他ノード板・非 Active は `None`。
    ///
    /// `title` 引数を持たない: 配線側の実装は
    /// [`crate::livechat::registry::LivechatRegistry::start_next_generation`] を呼ぶ際、
    /// **現行スレの title をそのまま引き継ぐ**こと(res_limit 到達時の自動移行 —
    /// `LivechatRegistry::accept_write` 内の自動呼び出し — と挙動を一貫させるため。
    /// 自動移行側も現行スレの title を引き継ぐ実装になっている)。
    fn next_thread(&self, board_id: &str) -> Option<u32>;
    /// クローズ操作(T047 — FR-014)。自板の Active/Frozen スレを明示クローズする。
    /// 成功なら `true`。未知 board_id・他ノード板・既 Closed は `false`。
    fn close_thread(&self, board_id: &str) -> bool;

    /// スレ開設(T063 — FR-001)。掲載中チャンネル(スレ主 = 掲載ペルソナ限定)に対して
    /// kind 31311 の発行対象となるスレを開設する。既定は未サポート。
    fn open_thread(&self, _req: OpenThreadRequest) -> Result<OpenThreadResult, LivechatOpError> {
        Err(LivechatOpError::Unsupported)
    }
    /// 板設定変更(T068 — FR-022)。自板の板設定を全置換し SETTINGS を即時配布する。既定は未サポート。
    fn update_settings(
        &self,
        _board_id: &str,
        _settings: BoardSettingsInput,
    ) -> Result<(), LivechatOpError> {
        Err(LivechatOpError::Unsupported)
    }
    /// 板鍵 BAN(T067 — FR-018/spec Edge Case)。以後の当該板鍵の書き込みを採番拒否する。既定は未サポート。
    fn ban_board_key(&self, _board_id: &str, _board_key: &str) -> Result<(), LivechatOpError> {
        Err(LivechatOpError::Unsupported)
    }
    /// 板鍵 BAN 解除(T067)。既定は未サポート。
    fn unban_board_key(&self, _board_id: &str, _board_key: &str) -> Result<(), LivechatOpError> {
        Err(LivechatOpError::Unsupported)
    }
    /// 接続元 ConnBan(T067 — FR-019)。以後の当該接続元を接続拒否する。既定は未サポート。
    fn ban_connection(&self, _board_id: &str, _addr: &str) -> Result<(), LivechatOpError> {
        Err(LivechatOpError::Unsupported)
    }
    /// 接続元 ConnBan 解除(T067)。既定は未サポート。
    fn unban_connection(&self, _board_id: &str, _addr: &str) -> Result<(), LivechatOpError> {
        Err(LivechatOpError::Unsupported)
    }
    /// 板鍵ローテーション(T067 — FR-017)。**視聴者自身**の当該板向け書き込み鍵を再生成し、
    /// 新しい公開鍵(hex)を返す(旧鍵は破棄。初回 PoW は次回書き込み時にクライアントが計算)。
    /// 既定は未サポート。
    fn rotate_board_key(&self, _board_id: &str) -> Result<String, LivechatOpError> {
        Err(LivechatOpError::Unsupported)
    }
}

/// `/api/v1/livechat` エンドポイント群のサブルーター。[`super::api_router`] が `.merge` する。
pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        // 一覧(GET)+ スレ開設(POST — T063)。
        .route("/livechat/threads", get(list_threads).post(open_thread))
        .route("/livechat/threads/{board_id}", get(get_thread))
        .route("/livechat/threads/{board_id}/join", post(join_thread))
        .route("/livechat/threads/{board_id}/next", post(next_thread))
        .route("/livechat/threads/{board_id}/close", post(close_thread))
        // 板設定変更(T068)。
        .route(
            "/livechat/threads/{board_id}/settings",
            put(update_settings),
        )
        // モデレーション(T067)。
        .route("/livechat/threads/{board_id}/ban", post(ban_board_key))
        .route("/livechat/threads/{board_id}/unban", post(unban_board_key))
        .route("/livechat/threads/{board_id}/connban", post(ban_connection))
        .route(
            "/livechat/threads/{board_id}/unconnban",
            post(unban_connection),
        )
        // 視聴者自身の板鍵ローテーション(T067 — FR-017)。
        .route(
            "/livechat/boards/{board_id}/rotate-key",
            post(rotate_board_key),
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

/// 次スレ操作・クローズ操作の応答本体(T046/T047)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct NextThreadResponse {
    /// 新しく開始した世代番号。
    generation: u32,
}

/// `POST /api/v1/livechat/threads/{board_id}/next` — 次スレ操作(T046 — FR-013)。
///
/// 配信者の明示操作を起点に次スレへ移行する(res_limit 到達時の自動移行はホスト側
/// [`crate::livechat::host`]/[`crate::livechat::registry`] が別途行う)。供給元未配線・
/// 未知 board_id・他ノード板・非 Active スレは `not_found`(内部情報を開示しない)。
async fn next_thread(State(state): State<AppState>, Path(board_id): Path<String>) -> Response {
    let Some(directory) = state.livechat_directory.as_ref() else {
        return error_response(StatusCode::NOT_FOUND, "not_found");
    };
    match directory.next_thread(&board_id) {
        Some(generation) => Json(NextThreadResponse { generation }).into_response(),
        None => error_response(StatusCode::NOT_FOUND, "not_found"),
    }
}

/// `POST /api/v1/livechat/threads/{board_id}/close` — クローズ操作(T047 — FR-014)。
///
/// 配信者の明示操作でスレをクローズする(スレ主署名付き THREAD_CLOSE の配布はレジストリ側 —
/// [`crate::livechat::registry::LivechatRegistry::close_thread`])。供給元未配線・未知
/// board_id・他ノード板・既 Closed は `not_found`。
async fn close_thread(State(state): State<AppState>, Path(board_id): Path<String>) -> Response {
    let Some(directory) = state.livechat_directory.as_ref() else {
        return error_response(StatusCode::NOT_FOUND, "not_found");
    };
    if directory.close_thread(&board_id) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        error_response(StatusCode::NOT_FOUND, "not_found")
    }
}

// ---------------------------------------------------------------------------
// T063/T067/T068: 変更系操作エンドポイント(X-Api-Token 保護は api_router が付与)
// ---------------------------------------------------------------------------

/// BAN/ConnBan の対象(板鍵 hex または接続元 `ip:port`)を運ぶ共通ボディ。
#[derive(Debug, Clone, Deserialize)]
struct TargetBody {
    target: String,
}

/// ローテーション結果(新しい板鍵の公開鍵 hex)。
#[derive(Debug, Clone, Serialize)]
struct RotateResult {
    pubkey: String,
}

/// 供給元(`livechat_directory`)未配線時の定型 `not_found`(内部状態を開示しない)。
fn not_wired() -> Response {
    error_response(StatusCode::NOT_FOUND, "not_found")
}

/// `POST /api/v1/livechat/threads` — スレ開設(T063 — FR-001)。
///
/// 掲載中チャンネル(スレ主 = 掲載ペルソナ限定)に対しスレを開設する。成功で 201 +
/// `{board_id, generation}`。未掲載・非ペルソナ・tip 未確定などは定型エラー。
async fn open_thread(
    State(state): State<AppState>,
    Json(req): Json<OpenThreadRequest>,
) -> Response {
    let Some(directory) = state.livechat_directory.as_ref() else {
        return not_wired();
    };
    match directory.open_thread(req) {
        Ok(result) => (StatusCode::CREATED, Json(result)).into_response(),
        Err(e) => e.into_response(),
    }
}

/// `PUT /api/v1/livechat/threads/{board_id}/settings` — 板設定変更(T068 — FR-022)。
async fn update_settings(
    State(state): State<AppState>,
    Path(board_id): Path<String>,
    Json(settings): Json<BoardSettingsInput>,
) -> Response {
    let Some(directory) = state.livechat_directory.as_ref() else {
        return not_wired();
    };
    match directory.update_settings(&board_id, settings) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => e.into_response(),
    }
}

/// `POST /api/v1/livechat/threads/{board_id}/ban` — 板鍵 BAN(T067)。
async fn ban_board_key(
    State(state): State<AppState>,
    Path(board_id): Path<String>,
    Json(body): Json<TargetBody>,
) -> Response {
    let Some(directory) = state.livechat_directory.as_ref() else {
        return not_wired();
    };
    match directory.ban_board_key(&board_id, &body.target) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => e.into_response(),
    }
}

/// `POST /api/v1/livechat/threads/{board_id}/unban` — 板鍵 BAN 解除(T067)。
async fn unban_board_key(
    State(state): State<AppState>,
    Path(board_id): Path<String>,
    Json(body): Json<TargetBody>,
) -> Response {
    let Some(directory) = state.livechat_directory.as_ref() else {
        return not_wired();
    };
    match directory.unban_board_key(&board_id, &body.target) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => e.into_response(),
    }
}

/// `POST /api/v1/livechat/threads/{board_id}/connban` — 接続元 ConnBan(T067 — FR-019)。
async fn ban_connection(
    State(state): State<AppState>,
    Path(board_id): Path<String>,
    Json(body): Json<TargetBody>,
) -> Response {
    let Some(directory) = state.livechat_directory.as_ref() else {
        return not_wired();
    };
    match directory.ban_connection(&board_id, &body.target) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => e.into_response(),
    }
}

/// `POST /api/v1/livechat/threads/{board_id}/unconnban` — 接続元 ConnBan 解除(T067)。
async fn unban_connection(
    State(state): State<AppState>,
    Path(board_id): Path<String>,
    Json(body): Json<TargetBody>,
) -> Response {
    let Some(directory) = state.livechat_directory.as_ref() else {
        return not_wired();
    };
    match directory.unban_connection(&board_id, &body.target) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => e.into_response(),
    }
}

/// `POST /api/v1/livechat/boards/{board_id}/rotate-key` — 板鍵ローテーション(T067 — FR-017)。
///
/// **視聴者自身**の当該板向け書き込み鍵を再生成し、新しい公開鍵 hex を返す。
async fn rotate_board_key(State(state): State<AppState>, Path(board_id): Path<String>) -> Response {
    let Some(directory) = state.livechat_directory.as_ref() else {
        return not_wired();
    };
    match directory.rotate_board_key(&board_id) {
        Ok(pubkey) => (StatusCode::OK, Json(RotateResult { pubkey })).into_response(),
        Err(e) => e.into_response(),
    }
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

    #[derive(Default)]
    struct FakeDirectory {
        threads: Vec<ThreadSummary>,
        /// 次スレ操作を許可する board_id(T046 のハンドラ疎通テスト用)。
        next_ok_board: Option<String>,
        /// クローズ操作を許可する board_id(T047 のハンドラ疎通テスト用)。
        close_ok_board: Option<String>,
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

        fn next_thread(&self, board_id: &str) -> Option<u32> {
            if self.next_ok_board.as_deref() == Some(board_id) {
                Some(2)
            } else {
                None
            }
        }

        fn close_thread(&self, board_id: &str) -> bool {
            self.close_ok_board.as_deref() == Some(board_id)
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

    /// 任意の [`LivechatDirectory`] 実装を注入した [`AppState`](操作ハンドラ疎通テスト用)。
    fn state_with_dyn(directory: std::sync::Arc<dyn LivechatDirectory>) -> AppState {
        use crate::security::SecurityLog;
        use crate::store::Store;
        use std::collections::HashSet;
        use std::sync::Arc;

        let store = Arc::new(Store::open_in_memory().unwrap());
        let dir = tempfile::tempdir().unwrap();
        let security = Arc::new(SecurityLog::new(dir.path().join("s.log")).unwrap());
        std::mem::forget(dir);
        AppState::with_parts(
            store,
            security,
            "test-token",
            HashSet::new(),
            super::super::RateLimiter::per_second(100),
        )
        .with_livechat_directory(directory)
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
            ..Default::default()
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
        let directory = FakeDirectory::default();
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
        let directory = FakeDirectory::default();
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

    // --- T046: 次スレ操作 API ------------------------------------------------

    #[tokio::test]
    async fn next_thread_returns_new_generation_for_allowed_board() {
        let board_id = "ab".repeat(32);
        let directory = FakeDirectory {
            next_ok_board: Some(board_id.clone()),
            ..Default::default()
        };
        let state = state_with_directory(Some(directory));
        let resp = next_thread(State(state), Path(board_id)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        assert_eq!(json["generation"], 2);
    }

    #[tokio::test]
    async fn next_thread_returns_not_found_for_unknown_board() {
        let directory = FakeDirectory::default();
        let state = state_with_directory(Some(directory));
        let resp = next_thread(State(state), Path("ff".repeat(32))).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn next_thread_returns_not_found_when_unwired() {
        let resp = next_thread(State(state_with_directory(None)), Path("ab".repeat(32))).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // --- T047: クローズ操作 API ----------------------------------------------

    #[tokio::test]
    async fn close_thread_returns_no_content_for_allowed_board() {
        let board_id = "ab".repeat(32);
        let directory = FakeDirectory {
            close_ok_board: Some(board_id.clone()),
            ..Default::default()
        };
        let state = state_with_directory(Some(directory));
        let resp = close_thread(State(state), Path(board_id)).await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn close_thread_returns_not_found_for_unknown_board() {
        let directory = FakeDirectory::default();
        let state = state_with_directory(Some(directory));
        let resp = close_thread(State(state), Path("ff".repeat(32))).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn close_thread_returns_not_found_when_unwired() {
        let resp = close_thread(State(state_with_directory(None)), Path("ab".repeat(32))).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // --- T063/T067/T068: 変更系操作ハンドラの疎通 ------------------------------

    /// 操作(open/settings/ban/rotate)の結果を制御できるフェイク供給元。
    #[derive(Default)]
    struct OpsDirectory {
        open_ok: bool,
        settings_result: Option<Result<(), LivechatOpError>>,
        ban_ok_board: Option<String>,
        rotate_pubkey: Option<String>,
    }

    impl LivechatDirectory for OpsDirectory {
        fn threads(&self) -> Vec<ThreadSummary> {
            Vec::new()
        }
        fn thread(&self, _board_id: &str) -> Option<ThreadDetail> {
            None
        }
        fn next_thread(&self, _board_id: &str) -> Option<u32> {
            None
        }
        fn close_thread(&self, _board_id: &str) -> bool {
            false
        }
        fn open_thread(&self, req: OpenThreadRequest) -> Result<OpenThreadResult, LivechatOpError> {
            if self.open_ok {
                Ok(OpenThreadResult {
                    board_id: format!("board-{}", req.channel_id),
                    generation: 1,
                })
            } else {
                Err(LivechatOpError::NotFound)
            }
        }
        fn update_settings(
            &self,
            _board_id: &str,
            _settings: BoardSettingsInput,
        ) -> Result<(), LivechatOpError> {
            self.settings_result
                .unwrap_or(Err(LivechatOpError::NotFound))
        }
        fn ban_board_key(&self, board_id: &str, _board_key: &str) -> Result<(), LivechatOpError> {
            if self.ban_ok_board.as_deref() == Some(board_id) {
                Ok(())
            } else {
                Err(LivechatOpError::NotFound)
            }
        }
        fn rotate_board_key(&self, _board_id: &str) -> Result<String, LivechatOpError> {
            self.rotate_pubkey
                .clone()
                .ok_or(LivechatOpError::Unavailable)
        }
    }

    fn open_req(channel_id: &str) -> OpenThreadRequest {
        OpenThreadRequest {
            channel_id: channel_id.to_string(),
            title: None,
            settings: None,
        }
    }

    #[tokio::test]
    async fn open_thread_returns_created_with_board_id() {
        let dir = std::sync::Arc::new(OpsDirectory {
            open_ok: true,
            ..Default::default()
        });
        let resp = open_thread(
            State(state_with_dyn(dir)),
            Json(open_req("cd".repeat(16).as_str())),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let json = body_json(resp).await;
        assert_eq!(json["generation"], 1);
        assert!(json["board_id"].as_str().unwrap().starts_with("board-"));
    }

    #[tokio::test]
    async fn open_thread_maps_not_found() {
        let dir = std::sync::Arc::new(OpsDirectory::default());
        let resp = open_thread(State(state_with_dyn(dir)), Json(open_req("x"))).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn open_thread_unwired_is_not_found() {
        let resp = open_thread(State(state_with_directory(None)), Json(open_req("x"))).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn update_settings_maps_ok_and_invalid() {
        let ok = std::sync::Arc::new(OpsDirectory {
            settings_result: Some(Ok(())),
            ..Default::default()
        });
        let resp = update_settings(
            State(state_with_dyn(ok)),
            Path("ab".repeat(32)),
            Json(BoardSettingsInput::default()),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let invalid = std::sync::Arc::new(OpsDirectory {
            settings_result: Some(Err(LivechatOpError::Invalid)),
            ..Default::default()
        });
        let resp = update_settings(
            State(state_with_dyn(invalid)),
            Path("ab".repeat(32)),
            Json(BoardSettingsInput::default()),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn ban_board_key_maps_allowed_and_unknown() {
        let board = "ab".repeat(32);
        let dir = std::sync::Arc::new(OpsDirectory {
            ban_ok_board: Some(board.clone()),
            ..Default::default()
        });
        let resp = ban_board_key(
            State(state_with_dyn(dir.clone())),
            Path(board),
            Json(TargetBody {
                target: "cc".repeat(32),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let resp = ban_board_key(
            State(state_with_dyn(dir)),
            Path("ff".repeat(32)),
            Json(TargetBody {
                target: "cc".repeat(32),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn rotate_board_key_returns_new_pubkey() {
        let dir = std::sync::Arc::new(OpsDirectory {
            rotate_pubkey: Some("dd".repeat(32)),
            ..Default::default()
        });
        let resp = rotate_board_key(State(state_with_dyn(dir)), Path("ab".repeat(32))).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        assert_eq!(json["pubkey"], "dd".repeat(32));
    }

    #[tokio::test]
    async fn rotate_board_key_unavailable_maps_503() {
        let dir = std::sync::Arc::new(OpsDirectory::default());
        let resp = rotate_board_key(State(state_with_dyn(dir)), Path("ab".repeat(32))).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    // --- T045: 互換 API 板 URL ---------------------------------------------

    #[test]
    fn board_compat_url_uses_loopback_and_default_port() {
        let board_id = "ab".repeat(32);
        assert_eq!(
            board_compat_url(&board_id),
            format!("http://127.0.0.1:7183/{board_id}/")
        );
    }
}

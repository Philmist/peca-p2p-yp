//! ペルソナ API(T030 — contracts/local-api.md `personas` エンドポイント群)
//!
//! - `GET /api/v1/personas`: ペルソナ一覧(秘密鍵は返さない)。
//!   利用不可ペルソナ(DPAPI 復号失敗)は `usable: false` として一覧に含める。
//! - `POST /api/v1/personas`: 新規作成(`{"label":"..."}` → `{"pubkey":"..."}`)。
//! - `PUT /api/v1/personas/{pubkey}`: label 変更 / archive / チャンネルへの割当 /
//!   「現在選択中」設定。
//! - `DELETE /api/v1/personas/{pubkey}?confirm=true`: 破棄(確認フラグ必須、復元不可)。
//! - `POST /api/v1/personas/{pubkey}/export`:
//!   受け入れ基準:
//!   (1) ボディに `{"confirm":true}` 必須 — 欠落は 400。
//!   (2) 応答に警告文を含める。
//!   (3) nsec は応答本文のみ — ログ・セキュリティイベントへの記録は MUST NOT。
//!
//! ルートは [`routes`] が返すサブルーターに定義し、[`super::api_router`] が `.merge` して
//! 4 層の保護(Host 検証・レート制限・トークン検証・ボディ上限)を自動継承する。
//! エラー応答は `{"error":"<code>"}` のみ(内部情報を含めない — Principle II)。

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;

use crate::identity::{IdentityError, PersonaInfo};
use crate::store::PersonaState;

use super::{AppState, error_response};

/// `personas` エンドポイント群のサブルーター。[`super::api_router`] が `.merge` する。
pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        // `/personas/{pubkey}/export` は `/{pubkey}` より先に宣言して優先させる。
        .route(
            "/personas/{pubkey}/export",
            axum::routing::post(export_persona),
        )
        .route("/personas", get(list_personas).post(create_persona))
        .route(
            "/personas/{pubkey}",
            axum::routing::put(update_persona).delete(delete_persona),
        )
}

/// identity が未配線のときのエラー応答(503 Service Unavailable)。
fn no_identity() -> Response {
    error_response(StatusCode::SERVICE_UNAVAILABLE, "service_unavailable")
}

/// IdentityError を HTTP 応答へ写像する。
fn identity_err(e: IdentityError) -> Response {
    match e {
        IdentityError::NotFound => error_response(StatusCode::NOT_FOUND, "not_found"),
        IdentityError::Unusable => {
            error_response(StatusCode::UNPROCESSABLE_ENTITY, "persona_unusable")
        }
        IdentityError::Crypto | IdentityError::Store(_) => {
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal")
        }
    }
}

/// PersonaInfo を応答用 JSON へ変換する(秘密鍵を含まない)。
fn persona_to_json(p: &PersonaInfo) -> Value {
    json!({
        "pubkey": p.pubkey,
        "label": p.label,
        "state": match p.state {
            PersonaState::Active => "active",
            PersonaState::Archived => "archived",
        },
        "usable": p.usable,
        "created_at": p.created_at,
        "selected": p.selected,
    })
}

/// `GET /api/v1/personas` — ペルソナ一覧(秘密鍵は返さない)。
async fn list_personas(State(state): State<AppState>) -> Response {
    let Some(identity) = &state.identity else {
        return no_identity();
    };
    match identity.list() {
        Ok(personas) => {
            let arr: Vec<Value> = personas.iter().map(persona_to_json).collect();
            Json(arr).into_response()
        }
        Err(e) => identity_err(e),
    }
}

/// ペルソナ作成リクエスト。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreatePersonaRequest {
    label: String,
}

/// `POST /api/v1/personas` — 新規ペルソナ作成。
async fn create_persona(State(state): State<AppState>, body: Bytes) -> Response {
    let Some(identity) = &state.identity else {
        return no_identity();
    };

    let req: CreatePersonaRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid_request"),
    };

    if req.label.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "invalid_request");
    }

    match identity.create(&req.label) {
        Ok(persona) => (
            StatusCode::CREATED,
            Json(json!({ "pubkey": persona.pubkey })),
        )
            .into_response(),
        Err(e) => identity_err(e),
    }
}

/// ペルソナ更新リクエスト(部分更新 — 指定フィールドのみ適用)。
#[derive(Debug, Deserialize)]
struct UpdatePersonaRequest {
    /// 表示名の変更(省略可)。
    label: Option<String>,
    /// 状態変更(省略可): `"active"` または `"archived"`。
    state: Option<String>,
    /// 「現在選択中」への設定(省略可): `true` のみ有効。
    select: Option<bool>,
    /// チャンネルへの割当(省略可): channel_id hex32。
    channel_id: Option<String>,
}

/// `PUT /api/v1/personas/{pubkey}` — label / state / 割当 / 選択の更新。
async fn update_persona(
    State(state): State<AppState>,
    Path(pubkey): Path<String>,
    body: Bytes,
) -> Response {
    let Some(identity) = &state.identity else {
        return no_identity();
    };

    let req: UpdatePersonaRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid_request"),
    };

    // label 更新
    if let Some(label) = &req.label
        && let Err(e) = identity.set_label(&pubkey, label)
    {
        return identity_err(e);
    }

    // state 更新
    if let Some(state_str) = &req.state {
        let new_state = match state_str.as_str() {
            "active" => PersonaState::Active,
            "archived" => PersonaState::Archived,
            _ => return error_response(StatusCode::BAD_REQUEST, "invalid_request"),
        };
        if let Err(e) = identity.set_state(&pubkey, new_state) {
            return identity_err(e);
        }
    }

    // 「現在選択中」設定
    if req.select == Some(true)
        && let Err(e) = identity.select(&pubkey)
    {
        return identity_err(e);
    }

    // チャンネルへの割当
    if let Some(channel_id) = &req.channel_id
        && let Err(e) = identity.assign_channel(channel_id, &pubkey)
    {
        return identity_err(e);
    }

    StatusCode::NO_CONTENT.into_response()
}

/// `DELETE /api/v1/personas/{pubkey}?confirm=true` — 破棄(確認フラグ必須)。
async fn delete_persona(
    State(state): State<AppState>,
    Path(pubkey): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(identity) = &state.identity else {
        return no_identity();
    };

    // 確認フラグが必須
    if params.get("confirm").map(String::as_str) != Some("true") {
        return error_response(StatusCode::BAD_REQUEST, "confirm_required");
    }

    match identity.delete(&pubkey) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => identity_err(e),
    }
}

/// nsec エクスポートリクエスト。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportRequest {
    /// `true` 必須 — 欠落・false は 400。
    confirm: bool,
}

/// `POST /api/v1/personas/{pubkey}/export` — nsec エクスポート。
///
/// 受け入れ基準(contracts/local-api.md):
/// (1) ボディに `{"confirm":true}` 必須 — 欠落は 400。
/// (2) 応答に警告文を含める。
/// (3) nsec は応答本文のみ — ログ・セキュリティイベントへの記録は MUST NOT。
async fn export_persona(
    State(state): State<AppState>,
    Path(pubkey): Path<String>,
    body: Bytes,
) -> Response {
    let Some(identity) = &state.identity else {
        return no_identity();
    };

    // (1) confirm: true 必須
    let req: ExportRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid_request"),
    };
    if !req.confirm {
        return error_response(StatusCode::BAD_REQUEST, "confirm_required");
    }

    // nsec 取得 — 成功してもログ・セキュリティイベントへの記録は MUST NOT (ADR-0003 §2)
    let nsec = match identity.export_nsec(&pubkey) {
        Ok(n) => n,
        Err(e) => return identity_err(e),
    };

    // (2) 応答に警告文を含める
    let warning = "秘密鍵を知る者はこのペルソナとして掲載できます。\
        ペルソナを破棄した後は復元できず、これが唯一のバックアップ手段です。\
        安全な場所に保管してください。";

    // (3) nsec は応答本文のみ — Json に格納して返す(ログ出力禁止)
    Json(json!({
        "nsec": nsec,
        "warning": warning,
    }))
    .into_response()
}

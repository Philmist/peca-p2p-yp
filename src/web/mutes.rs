//! ミュート API(T040 — contracts/local-api.md `mutes` エンドポイント群)
//!
//! - `GET /api/v1/mutes`: ミュート一覧(pubkey / channel 単位、id 昇順)。
//! - `POST /api/v1/mutes`: ミュート登録(`{"kind":"pubkey"|"channel","value":"..."}`)。
//!   同一 (kind, value) は冪等(再登録なし)。
//! - `DELETE /api/v1/mutes/{id}`: id で削除。
//!
//! ミュートは両単位独立評価・OR 適用(data-model §MuteEntry 適用規則)。
//! 適用(非表示フィルタ)はビュー側(T039)の責務であり、ここは CRUD のみ。
//! ネットワーク非公開・ローカル保存のみ(FR-008)。
//!
//! ルートは [`routes`] が返すサブルーターに定義し、[`super::api_router`] が `.merge` して
//! 4 層の保護(Host 検証・レート制限・トークン検証・ボディ上限)を自動継承する。
//! エラー応答は `{"error":"<code>"}` のみ(内部情報を含めない — Principle II)。

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::store::{MuteEntry, MuteKind};

use super::{AppState, error_response};

/// `mutes` エンドポイント群のサブルーター。[`super::api_router`] が `.merge` する。
pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/mutes", get(list_mutes).post(add_mute))
        .route("/mutes/{id}", axum::routing::delete(delete_mute))
}

/// MuteEntry を応答用 JSON へ変換する。
fn mute_to_json(m: &MuteEntry) -> Value {
    json!({
        "id": m.id,
        "kind": match m.kind {
            MuteKind::Pubkey => "pubkey",
            MuteKind::Channel => "channel",
        },
        "value": m.value,
        "created_at": m.created_at,
    })
}

/// `GET /api/v1/mutes` — ミュート一覧(id 昇順)。
async fn list_mutes(State(state): State<AppState>) -> Response {
    match state.store.list_mutes() {
        Ok(mutes) => {
            let arr: Vec<Value> = mutes.iter().map(mute_to_json).collect();
            Json(arr).into_response()
        }
        Err(_) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal"),
    }
}

/// ミュート登録リクエスト。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AddMuteRequest {
    /// `"pubkey"` または `"channel"`。
    kind: String,
    /// ミュート対象の値(pubkey hex または channel ID hex)。
    value: String,
}

/// `POST /api/v1/mutes` — ミュート登録。
async fn add_mute(State(state): State<AppState>, body: Bytes) -> Response {
    let req: AddMuteRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid_request"),
    };

    let kind = match req.kind.as_str() {
        "pubkey" => MuteKind::Pubkey,
        "channel" => MuteKind::Channel,
        _ => return error_response(StatusCode::BAD_REQUEST, "invalid_request"),
    };

    if req.value.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "invalid_request");
    }

    match state.store.insert_mute(kind, &req.value) {
        Ok(entry) => (StatusCode::CREATED, Json(mute_to_json(&entry))).into_response(),
        Err(_) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal"),
    }
}

/// `DELETE /api/v1/mutes/{id}` — id で削除。
async fn delete_mute(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    match state.store.delete_mute(id) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => error_response(StatusCode::NOT_FOUND, "not_found"),
        Err(_) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal"),
    }
}

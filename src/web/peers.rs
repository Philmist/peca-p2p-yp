//! ピア API(T021 — contracts/local-api.md `peers` エンドポイント群)
//!
//! - `GET /api/v1/peers`: 一覧+健全性(source / verified / enabled / last_ok_at /
//!   fail_count / 接続中か)。
//! - `POST /api/v1/peers`: **貼り付け一括登録**(`{"addrs":["host:port",…]}` — research R10)。
//!   各アドレスは T018 の [`crate::p2p::peers::parse_addr`] で検証・正規化し、
//!   **不正アドレスは個別にエラー返却**(全体を失敗させない)。source=manual で登録する。
//! - `PUT /api/v1/peers/{id}`: enabled 変更。
//! - `DELETE /api/v1/peers/{id}`: 削除。
//! - `GET /api/v1/peers/export`: verified のみ 1 行 1 アドレスの text/plain(research R10)。
//!
//! ルートは [`routes`] が返すサブルーターに定義し、[`super::api_router`] が `.merge` して
//! 4 層の保護(Host 検証・レート制限・トークン検証・ボディ上限)を自動継承する。
//! エラー応答は `{"error":"<code>"}` のみ(内部情報を含めない — Principle II)。
//!
//! ## 「接続中か」の扱い
//! 接続レイヤ(T020 の配線)との結線は本タスクの範囲外のため、`connected` は現状**常に
//! false** を返す([`is_connected`])。T031(status API)/ T048(全断通知)で
//! PeerManager の接続中アドレス集合を AppState 経由で参照して結線する。
//!
//! ## store のキーは addr
//! 契約のルートは `{id}` だが [`crate::store::Store`] のピア API は addr キーのため、
//! [`resolve_addr`] が `list_peers()` から id→addr を解決してから store を呼ぶ。

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::p2p::peers::parse_addr;
use crate::store::{PeerEndpoint, PeerSource};

use super::{error_response, AppState};

/// `peers` エンドポイント群のサブルーター。[`super::api_router`] が `.merge` する。
pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        // 静的セグメント `/peers/export` は `/peers/{id}` より優先される(axum 0.8)。
        .route("/peers", get(list_peers).post(bulk_add))
        .route("/peers/export", get(export_peers))
        .route("/peers/{id}", put(set_enabled).delete(delete_peer))
}

/// ピアが現在接続中か。
///
/// 接続レイヤとの結線が未実装のため常に false を返す。
/// TODO(T031/T048): PeerManager の outbound/inbound 集合を AppState 経由で参照する。
fn is_connected(_addr: &str) -> bool {
    false
}

/// ピア 1 件を応答用 JSON へ変換する。
fn peer_to_json(peer: &PeerEndpoint) -> Value {
    json!({
        "id": peer.id,
        "addr": peer.addr,
        "source": match peer.source {
            PeerSource::Manual => "manual",
            PeerSource::Pex => "pex",
        },
        "verified": peer.verified,
        "enabled": peer.enabled,
        "last_ok_at": peer.last_ok_at,
        "fail_count": peer.fail_count,
        "connected": is_connected(&peer.addr),
    })
}

/// path の id からストア上の addr を解決する(store のキーは addr のため)。
fn resolve_addr(state: &AppState, id: i64) -> Option<String> {
    state
        .store
        .list_peers()
        .ok()?
        .into_iter()
        .find(|p| p.id == id)
        .map(|p| p.addr)
}

/// `GET /api/v1/peers` — 一覧+健全性。
async fn list_peers(State(state): State<AppState>) -> Response {
    match state.store.list_peers() {
        Ok(peers) => {
            let arr: Vec<Value> = peers.iter().map(peer_to_json).collect();
            Json(arr).into_response()
        }
        Err(_) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal"),
    }
}

/// 貼り付け一括登録のリクエスト。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BulkAddRequest {
    /// `host:port` の配列(1 行 1 件想定)。
    addrs: Vec<String>,
}

/// `POST /api/v1/peers` — 貼り付け一括登録(不正アドレスは個別にエラー返却)。
async fn bulk_add(State(state): State<AppState>, body: Bytes) -> Response {
    let req: BulkAddRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid_request"),
    };

    let mut added: Vec<Value> = Vec::new();
    let mut errors: Vec<Value> = Vec::new();
    for raw in &req.addrs {
        match parse_addr(raw) {
            Ok(parsed) => {
                let canonical = parsed.canonical();
                match state.store.upsert_peer(&canonical, PeerSource::Manual) {
                    Ok(peer) => added.push(peer_to_json(&peer)),
                    // 入力アドレス(利用者自身の入力)のみを返す。内部情報は含めない。
                    Err(_) => errors.push(json!({ "addr": raw, "error": "store_error" })),
                }
            }
            Err(_) => errors.push(json!({ "addr": raw, "error": "invalid_addr" })),
        }
    }

    Json(json!({ "added": added, "errors": errors })).into_response()
}

/// enabled 変更のリクエスト。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnabledUpdate {
    /// 有効(true)/無効(false)。
    enabled: bool,
}

/// `PUT /api/v1/peers/{id}` — enabled 変更。
async fn set_enabled(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    body: Bytes,
) -> Response {
    let update: EnabledUpdate = match serde_json::from_slice(&body) {
        Ok(u) => u,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid_request"),
    };
    let Some(addr) = resolve_addr(&state, id) else {
        return error_response(StatusCode::NOT_FOUND, "not_found");
    };
    match state.store.set_peer_enabled(&addr, update.enabled) {
        Ok(true) => match state.store.get_peer(&addr) {
            Ok(Some(peer)) => Json(peer_to_json(&peer)).into_response(),
            _ => error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        },
        Ok(false) => error_response(StatusCode::NOT_FOUND, "not_found"),
        Err(_) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal"),
    }
}

/// `DELETE /api/v1/peers/{id}` — 削除。
async fn delete_peer(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let Some(addr) = resolve_addr(&state, id) else {
        return error_response(StatusCode::NOT_FOUND, "not_found");
    };
    match state.store.delete_peer(&addr) {
        Ok(true) => Json(json!({ "deleted": true })).into_response(),
        Ok(false) => error_response(StatusCode::NOT_FOUND, "not_found"),
        Err(_) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal"),
    }
}

/// `GET /api/v1/peers/export` — verified のみ 1 行 1 アドレスの text/plain。
async fn export_peers(State(state): State<AppState>) -> Response {
    match state.store.verified_peers_by_recency(None) {
        Ok(peers) => {
            let mut body = String::new();
            for peer in &peers {
                body.push_str(&peer.addr);
                body.push('\n');
            }
            (
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                body,
            )
                .into_response()
        }
        Err(_) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal"),
    }
}

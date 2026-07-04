//! チャンネル一覧 API(T041 — contracts/local-api.md `GET /channels`)
//!
//! - `GET /api/v1/channels`: 発見済みチャンネル一覧(視聴者向け)。
//!   - `AppState.directory` から取得(None は空一覧)
//!   - ミュート除外は ChannelDirectory.list() が保証(data-model §ChannelDirectory 契約)
//!   - `url_warning` フラグを付与(FR-012 — コンタクト URL が http/https 以外なら true)
//!
//! ルートは [`routes`] が返すサブルーターに定義し、[`super::api_router`] が `.merge` して
//! 4 層の保護(Host 検証・レート制限・トークン検証・ボディ上限)を自動継承する。
//! エラー応答は `{"error":"<code>"}` のみ(内部情報を含めない — Principle II)。

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::event::view::DiscoveredChannel;
use crate::security;

use super::AppState;

/// `channels` エンドポイント群のサブルーター。[`super::api_router`] が `.merge` する。
pub(crate) fn routes() -> Router<AppState> {
    Router::new().route("/channels", get(list_channels))
}

/// DiscoveredChannel を応答用 JSON へ変換する。`url_warning` フラグを付与する(FR-012)。
fn channel_to_json(ch: &DiscoveredChannel) -> Value {
    let l = &ch.listing;
    let contact_url = l.contact.as_deref().unwrap_or("");
    let url_warning = security::url_needs_warning(contact_url);

    json!({
        "channel_id": ch.channel_id,
        "author_pubkey": ch.author_pubkey,
        "title": l.title,
        "genre": l.genre,
        "detail": l.summary,
        "tip": l.tip,
        "contact_url": l.contact,
        "url_warning": url_warning,
        "listener_num": l.current_participants,
        "relay_num": l.relays,
        "bitrate": l.bitrate_kbps.unwrap_or(0),
        "content_type": l.content_type,
        "status": match l.status {
            crate::event::schema::ChannelStatus::Live => "live",
            crate::event::schema::ChannelStatus::Ended => "ended",
        },
        "starts": l.starts,
        "created_at": ch.created_at,
        "source_peers": ch.source_peers,
        "track_title": l.track.as_ref().map(|t| t.title.as_str()).unwrap_or(""),
        "track_artist": l.track.as_ref().map(|t| t.artist.as_str()).unwrap_or(""),
        "track_album": l.track.as_ref().map(|t| t.album.as_str()).unwrap_or(""),
        "track_contact_url": l.track.as_ref().map(|t| t.url.as_str()).unwrap_or(""),
    })
}

/// `GET /api/v1/channels` — 発見済みチャンネル一覧。
async fn list_channels(State(state): State<AppState>) -> Response {
    let channels = state
        .directory
        .as_ref()
        .map(|d| d.list())
        .unwrap_or_default();

    let arr: Vec<Value> = channels.iter().map(channel_to_json).collect();
    (StatusCode::OK, Json(arr)).into_response()
}

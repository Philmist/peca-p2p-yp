//! 設定 API(T062 — contracts/local-api.md `GET/PUT /settings`)
//!
//! - `GET /api/v1/settings`: data-model §Settings の全 13 キーを返す。
//! - `PUT /api/v1/settings`: 検証つき更新。`pcp_bind` / `http_bind` の非 loopback 値は
//!   400 で拒否する(ADR-0006 決定 4 — [`crate::config::Settings::validate`] を活用)。
//!   バインド系キー(`pcp_bind` / `http_bind` / `p2p_bind`)の変更は保存のうえ
//!   応答に再起動要求(`restart_required` / `restart_keys`)を含める。
//!
//! ルートは [`super::api_router`] へ登録され、4 層の保護(Host 検証・レート制限・
//! トークン検証・ボディ上限)を自動継承する。エラー応答は `{"error":"<code>"}` のみ。
//!
//! LAN 公開オプトインは v1 非実装のため、§保護方針の警告 2 項目は扱わない
//! (ADR-0006 決定 4)。

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::config::{ConfigError, Settings};

use super::{AppState, error_response};

/// バインド系キー(変更時に再起動が必要)。
const BIND_KEYS: [&str; 3] = ["pcp_bind", "http_bind", "p2p_bind"];

/// `GET /api/v1/settings` — 全設定キーを JSON で返す。
pub async fn get_settings(State(state): State<AppState>) -> Response {
    match Settings::load(&state.store) {
        Ok(settings) => Json(settings_to_json(&settings)).into_response(),
        Err(_) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal"),
    }
}

/// `PUT /api/v1/settings` — 提供されたキーのみ更新する(部分更新)。
///
/// 手順: 現行値をロード → 提供キーを適用 → 検証(非 loopback バインド等は 400)→
/// 変更されたバインド系キーを算出 → 保存 → 再起動要求を応答。
pub async fn put_settings(State(state): State<AppState>, body: Bytes) -> Response {
    let update: SettingsUpdate = match serde_json::from_slice(&body) {
        Ok(u) => u,
        // 不正 JSON・未知キー・型不一致はすべて 400(内部情報なし)
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid_request"),
    };

    let current = match Settings::load(&state.store) {
        Ok(s) => s,
        Err(_) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal"),
    };

    let next = update.apply_to(current.clone());
    if let Err(e) = next.validate() {
        return validation_error_response(e);
    }

    let restart_keys = changed_bind_keys(&current, &next);

    if next.save(&state.store).is_err() {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal");
    }

    Json(serde_json::json!({
        "restart_required": !restart_keys.is_empty(),
        "restart_keys": restart_keys,
    }))
    .into_response()
}

/// 検証エラーを定型 400/500 応答へ写像する(内部情報は含めない)。
fn validation_error_response(e: ConfigError) -> Response {
    let code = match e {
        ConfigError::NonLoopbackBind { .. } => "non_loopback_bind",
        ConfigError::NonLanBind { .. } => "non_lan_bind",
        ConfigError::InvalidBind { .. } => "invalid_bind",
        ConfigError::InvalidEncoding => "invalid_encoding",
        ConfigError::InvalidArgument => "invalid_request",
        // ストア障害のみ 500、それ以外の検証エラーは 400
        ConfigError::Store(_) => {
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal");
        }
    };
    error_response(StatusCode::BAD_REQUEST, code)
}

/// 変更されたバインド系キー名を返す。
fn changed_bind_keys(current: &Settings, next: &Settings) -> Vec<&'static str> {
    let mut keys = Vec::new();
    if current.pcp_bind != next.pcp_bind {
        keys.push(BIND_KEYS[0]);
    }
    if current.http_bind != next.http_bind {
        keys.push(BIND_KEYS[1]);
    }
    if current.p2p_bind != next.p2p_bind {
        keys.push(BIND_KEYS[2]);
    }
    keys
}

/// Settings を JSON オブジェクト(ネイティブ型)へ変換する。
fn settings_to_json(s: &Settings) -> serde_json::Value {
    serde_json::json!({
        "pcp_bind": s.pcp_bind,
        "http_bind": s.http_bind,
        "p2p_bind": s.p2p_bind,
        "p2p_outbound_target": s.p2p_outbound_target,
        "p2p_inbound_max": s.p2p_inbound_max,
        "pex_enabled": s.pex_enabled,
        "upnp_enabled": s.upnp_enabled,
        "freshness_window_sec": s.freshness_window_sec,
        "republish_interval_sec": s.republish_interval_sec,
        "max_clock_skew_sec": s.max_clock_skew_sec,
        "min_pow_bits": s.min_pow_bits,
        "event_store_max": s.event_store_max,
        "index_txt_encoding": s.index_txt_encoding,
    })
}

/// 部分更新リクエスト。未知キーは拒否(`deny_unknown_fields`)。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsUpdate {
    pcp_bind: Option<String>,
    http_bind: Option<String>,
    p2p_bind: Option<String>,
    p2p_outbound_target: Option<u32>,
    p2p_inbound_max: Option<u32>,
    pex_enabled: Option<bool>,
    upnp_enabled: Option<bool>,
    freshness_window_sec: Option<u64>,
    republish_interval_sec: Option<u64>,
    max_clock_skew_sec: Option<u64>,
    min_pow_bits: Option<u32>,
    event_store_max: Option<u64>,
    index_txt_encoding: Option<String>,
}

impl SettingsUpdate {
    /// 提供されたフィールドのみを `base` に上書きする。
    fn apply_to(self, mut base: Settings) -> Settings {
        if let Some(v) = self.pcp_bind {
            base.pcp_bind = v;
        }
        if let Some(v) = self.http_bind {
            base.http_bind = v;
        }
        if let Some(v) = self.p2p_bind {
            base.p2p_bind = v;
        }
        if let Some(v) = self.p2p_outbound_target {
            base.p2p_outbound_target = v;
        }
        if let Some(v) = self.p2p_inbound_max {
            base.p2p_inbound_max = v;
        }
        if let Some(v) = self.pex_enabled {
            base.pex_enabled = v;
        }
        if let Some(v) = self.upnp_enabled {
            base.upnp_enabled = v;
        }
        if let Some(v) = self.freshness_window_sec {
            base.freshness_window_sec = v;
        }
        if let Some(v) = self.republish_interval_sec {
            base.republish_interval_sec = v;
        }
        if let Some(v) = self.max_clock_skew_sec {
            base.max_clock_skew_sec = v;
        }
        if let Some(v) = self.min_pow_bits {
            base.min_pow_bits = v;
        }
        if let Some(v) = self.event_store_max {
            base.event_store_max = v;
        }
        if let Some(v) = self.index_txt_encoding {
            base.index_txt_encoding = v;
        }
        base
    }
}

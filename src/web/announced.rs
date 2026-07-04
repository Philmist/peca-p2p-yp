//! 掲載状態 API(T031 — contracts/local-api.md)
//!
//! - `GET /api/v1/announced` — 自分が掲載中のチャンネル + 伝搬先(established ピア)数
//! - `GET /api/v1/status` — 全体状態の基本形(PCP 待受・established ピア数 in/out)。
//!   着信可否(UPnP — T053)・全ピア到達不能フラグ(T048)は後続タスクで拡張する
//!
//! PCP レジストリ・gossip ハブへの直接依存を避けるため、供給元は
//! [`AnnouncedProvider`] / [`NodeStatusProvider`] として注入する(main.rs で適合)。

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::web::AppState;

/// 掲載中チャンネルの API 表現(AnnouncedChannel のビュー)。
#[derive(Debug, Clone, Serialize)]
pub struct AnnouncedSummary {
    /// チャンネル GUID(hex 32 小文字)。
    pub channel_id: String,
    /// チャンネル名。
    pub name: String,
    /// ジャンル。
    pub genre: String,
    /// 説明。
    pub description: String,
    /// コンタクト URL。
    pub contact_url: String,
    /// ビットレート kbps(不明 0)。
    pub bitrate_kbps: u64,
    /// コンテンツ種別。
    pub content_type: String,
    /// トラッカー ip:port(firewalled 時は空)。
    pub tracker: String,
    /// 直接視聴者数(不明 -1)。
    pub listeners: i64,
    /// リレー数(不明 -1)。
    pub relays: i64,
    /// 配信開始時刻(unix 秒)。
    pub started_at: u64,
    /// セッション状態(`announced` / `updating` / `ended`)。
    pub state: String,
    /// 署名に使うペルソナ pubkey(未選択 = 掲載保留中なら `None`)。
    pub persona_pubkey: Option<String>,
}

/// 掲載中チャンネルの供給元(PCP レジストリ+掲載エンジンの適合層が実装)。
pub trait AnnouncedProvider: Send + Sync {
    /// 現在掲載中(ended 前)のチャンネル一覧。
    fn list(&self) -> Vec<AnnouncedSummary>;
}

/// ノード状態の供給元(gossip ハブ・起動配線が実装)。
pub trait NodeStatusProvider: Send + Sync {
    /// PCP 待受が有効か。
    fn pcp_listening(&self) -> bool;
    /// established セッション数(inbound, outbound)。
    fn established(&self) -> (usize, usize);
}

/// `/api/v1` へ合流するルート。
pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/announced", get(get_announced))
        .route("/status", get(get_status))
}

#[derive(Serialize)]
struct AnnouncedResponse {
    channels: Vec<AnnouncedSummary>,
    /// 伝搬先となる established ピア数(in + out)。
    propagation_peers: usize,
}

/// `GET /api/v1/announced`。
async fn get_announced(State(state): State<AppState>) -> Response {
    let channels = state
        .announced
        .as_ref()
        .map(|p| p.list())
        .unwrap_or_default();
    let propagation_peers = state
        .node_status
        .as_ref()
        .map(|s| {
            let (i, o) = s.established();
            i + o
        })
        .unwrap_or(0);
    Json(AnnouncedResponse {
        channels,
        propagation_peers,
    })
    .into_response()
}

#[derive(Serialize)]
struct PeerCounts {
    r#in: usize,
    out: usize,
}

#[derive(Serialize)]
struct StatusResponse {
    pcp_listening: bool,
    established: PeerCounts,
}

/// `GET /api/v1/status`(基本形)。
async fn get_status(State(state): State<AppState>) -> Response {
    let (pcp_listening, (inbound, outbound)) = state
        .node_status
        .as_ref()
        .map(|s| (s.pcp_listening(), s.established()))
        .unwrap_or((false, (0, 0)));
    Json(StatusResponse {
        pcp_listening,
        established: PeerCounts {
            r#in: inbound,
            out: outbound,
        },
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;

    use http_body_util::BodyExt;

    use super::*;
    use crate::security::SecurityLog;
    use crate::store::Store;
    use crate::web::RateLimiter;

    struct FakeAnnounced;
    impl AnnouncedProvider for FakeAnnounced {
        fn list(&self) -> Vec<AnnouncedSummary> {
            vec![AnnouncedSummary {
                channel_id: "0123456789abcdef0123456789abcdef".into(),
                name: "テスト配信".into(),
                genre: "game".into(),
                description: String::new(),
                contact_url: String::new(),
                bitrate_kbps: 500,
                content_type: "FLV".into(),
                tracker: "198.51.100.1:7144".into(),
                listeners: 3,
                relays: -1,
                started_at: 1_700_000_000,
                state: "announced".into(),
                persona_pubkey: Some("ab".repeat(32)),
            }]
        }
    }

    struct FakeStatus;
    impl NodeStatusProvider for FakeStatus {
        fn pcp_listening(&self) -> bool {
            true
        }
        fn established(&self) -> (usize, usize) {
            (2, 5)
        }
    }

    fn state_with_providers(wire: bool) -> AppState {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let dir = tempfile::tempdir().unwrap();
        let security = Arc::new(SecurityLog::new(dir.path().join("s.log")).unwrap());
        // tempdir はテスト終了まで保持不要(ログは書き込み時に失敗しても致命ではない)
        std::mem::forget(dir);
        let mut state = AppState::with_parts(
            store,
            security,
            "test-token",
            HashSet::new(),
            RateLimiter::per_second(100),
        );
        if wire {
            state.announced = Some(Arc::new(FakeAnnounced));
            state.node_status = Some(Arc::new(FakeStatus));
        }
        state
    }

    async fn body_json(resp: Response) -> serde_json::Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn announced_returns_channels_and_propagation_count() {
        let resp = get_announced(State(state_with_providers(true))).await;
        let json = body_json(resp).await;
        assert_eq!(json["propagation_peers"], 7);
        assert_eq!(json["channels"][0]["name"], "テスト配信");
        assert_eq!(json["channels"][0]["relays"], -1);
        assert_eq!(json["channels"][0]["state"], "announced");
    }

    #[tokio::test]
    async fn status_reports_pcp_and_established_counts() {
        let resp = get_status(State(state_with_providers(true))).await;
        let json = body_json(resp).await;
        assert_eq!(json["pcp_listening"], true);
        assert_eq!(json["established"]["in"], 2);
        assert_eq!(json["established"]["out"], 5);
    }

    #[tokio::test]
    async fn unwired_providers_return_empty_defaults() {
        let resp = get_announced(State(state_with_providers(false))).await;
        let json = body_json(resp).await;
        assert_eq!(json["propagation_peers"], 0);
        assert_eq!(json["channels"].as_array().unwrap().len(), 0);

        let resp = get_status(State(state_with_providers(false))).await;
        let json = body_json(resp).await;
        assert_eq!(json["pcp_listening"], false);
        assert_eq!(json["established"]["in"], 0);
    }
}

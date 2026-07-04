//! 掲載状態 API(T031 — contracts/local-api.md)
//!
//! - `GET /api/v1/announced` — 自分が掲載中のチャンネル + 伝搬先(established ピア)数
//! - `GET /api/v1/status` — 全体状態(PCP 待受・established ピア数 in/out・
//!   全ピア到達不能フラグ(T048)・時計ずれ自己診断(T048)・着信可否(UPnP — T053))
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

/// 時計ずれ自己診断の結果(T048 — contracts/p2p-gossip.md §メッセージ種別 `ts`)。
///
/// established ピアの申告 `ts` から推定した自ノードの時計ずれ。**未検証の申告値に
/// 基づく通知専用の指標**であり、イベント検証・接続判断には用いない(MUST NOT)。
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ClockSkewStatus {
    /// 時計ずれ(秒)の中央値。established ピアがなければ `None`。
    pub median_sec: Option<i64>,
    /// 中央値の絶対値が `max_clock_skew_sec` を超え、時刻同期を促すべきか。
    pub warning: bool,
}

/// 時計ずれ標本(秒)の中央値としきい値超過判定を求める(T048)。
///
/// 中央値は少数の虚偽申告に頑健。標本が空なら `median_sec = None`・`warning = false`。
/// しきい値は受信検証(schema)と一致させるため `max_clock_skew_sec` を渡す。
pub fn clock_skew_status(samples: &[i64], threshold_sec: i64) -> ClockSkewStatus {
    if samples.is_empty() {
        return ClockSkewStatus {
            median_sec: None,
            warning: false,
        };
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let n = sorted.len();
    let median = if n % 2 == 1 {
        sorted[n / 2]
    } else {
        // 偶数個は中央 2 値の平均(整数、ゼロ方向へ丸め)。
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2
    };
    ClockSkewStatus {
        median_sec: Some(median),
        warning: median.abs() > threshold_sec,
    }
}

/// ノード状態の供給元(gossip ハブ・起動配線が実装)。
pub trait NodeStatusProvider: Send + Sync {
    /// PCP 待受が有効か。
    fn pcp_listening(&self) -> bool;
    /// established セッション数(inbound, outbound)。
    fn established(&self) -> (usize, usize);
    /// 全ピア到達不能か(T048 / US3 — UI の到達不能バナー)。
    fn all_peers_unreachable(&self) -> bool;
    /// 自ノードの時計ずれ自己診断(T048)。
    fn clock_skew(&self) -> ClockSkewStatus;
    /// 着信可能か(UPnP マッピング成功・直接待受 — T053 / FR-016)。
    ///
    /// `false` のとき UI は「外向き接続のみで参加中」を表示する(SC-009)。待受なし
    /// (`p2p_bind` 空)や UPnP マッピング失敗・更新失敗で `false` になる。
    fn inbound_reachable(&self) -> bool;
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
    /// 全ピア到達不能フラグ(T048 / US3 — UI が到達不能バナーを表示)。
    all_peers_unreachable: bool,
    /// 時計ずれ自己診断(T048)。
    clock_skew: ClockSkewStatus,
    /// 着信可能か(T053 / FR-016 — `false` は「外向き接続のみで参加中」)。
    inbound_reachable: bool,
}

/// `GET /api/v1/status`。
///
/// established 数(in/out)に加え、全ピア到達不能フラグ(T048 / US3)と
/// 時計ずれ自己診断(T048)を返す。供給元未配線時は既定(到達不能でない・
/// 時計ずれ標本なし)を返す。
async fn get_status(State(state): State<AppState>) -> Response {
    let response = match state.node_status.as_ref() {
        Some(s) => StatusResponse {
            pcp_listening: s.pcp_listening(),
            established: {
                let (inbound, outbound) = s.established();
                PeerCounts {
                    r#in: inbound,
                    out: outbound,
                }
            },
            all_peers_unreachable: s.all_peers_unreachable(),
            clock_skew: s.clock_skew(),
            inbound_reachable: s.inbound_reachable(),
        },
        None => StatusResponse {
            pcp_listening: false,
            established: PeerCounts { r#in: 0, out: 0 },
            all_peers_unreachable: false,
            clock_skew: ClockSkewStatus {
                median_sec: None,
                warning: false,
            },
            inbound_reachable: false,
        },
    };
    Json(response).into_response()
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
        fn all_peers_unreachable(&self) -> bool {
            false
        }
        fn clock_skew(&self) -> ClockSkewStatus {
            ClockSkewStatus {
                median_sec: Some(3),
                warning: false,
            }
        }
        fn inbound_reachable(&self) -> bool {
            true
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
        // T048: 全ピア到達不能フラグ・時計ずれ診断も返る。
        assert_eq!(json["all_peers_unreachable"], false);
        assert_eq!(json["clock_skew"]["median_sec"], 3);
        assert_eq!(json["clock_skew"]["warning"], false);
        // T053: 着信可否も返る。
        assert_eq!(json["inbound_reachable"], true);
    }

    #[test]
    fn clock_skew_median_and_warning() {
        // 標本なし → 診断不能・警告なし。
        let none = clock_skew_status(&[], 300);
        assert_eq!(none.median_sec, None);
        assert!(!none.warning);
        // 奇数個: 中央値。少数の外れ値(1000)に頑健で警告は出ない。
        let odd = clock_skew_status(&[-5, 2, 1000], 300);
        assert_eq!(odd.median_sec, Some(2));
        assert!(!odd.warning);
        // 偶数個: 中央 2 値の平均。
        let even = clock_skew_status(&[10, 20, 30, 40], 300);
        assert_eq!(even.median_sec, Some(25));
        assert!(!even.warning);
        // 中央値がしきい値超過 → 警告(負方向も対称)。
        let skewed = clock_skew_status(&[-400, -350, -320], 300);
        assert_eq!(skewed.median_sec, Some(-350));
        assert!(skewed.warning);
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

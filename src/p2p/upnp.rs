//! UPnP ポートマッピング(FR-016 / research R15)
//!
//! `igd-next`(async)で起動時に P2P 待受ポートの UPnP IGD マッピングを試行し、
//! **lease 3,600 秒・その半分 = 1,800 秒間隔で定期更新**する。失敗時は**警告なしで**
//! 外向き接続のみモードへフォールバックし、着信可否の共有状態([`InboundReachable`])を
//! 更新する。**定期更新の失敗は着信性の喪失として検出**し即時に共有状態へ反映する
//! (`GET /api/v1/status` の「外向き接続のみで参加中」表示に用いる)。
//!
//! **UPnP の成否は HELLO の `listen_port` 申告値に影響しない**(contracts/p2p-gossip.md
//! §メッセージ種別)。着信可能かは受信側が当該 `host:listen_port` へ接続して検証する。

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use igd_next::PortMappingProtocol;
use igd_next::aio::tokio::search_gateway;
use tokio::sync::watch;

/// UPnP マッピングの lease 時間(秒 — research R15)。
pub const LEASE_SECS: u32 = 3600;
/// 定期更新の間隔(lease の半分 = 1,800 秒 — research R15)。
pub const RENEW_INTERVAL: Duration = Duration::from_secs(1800);
/// マッピングの説明(ルーターの管理画面に表示される。内部情報は含めない)。
const MAPPING_DESC: &str = "peca-p2p-yp";

/// 着信可否の共有状態。
///
/// - 待受なし(`p2p_bind` 空)のノードは常に「到達不能(外向きのみ)」。
/// - 待受ありで UPnP 有効なノードは、マッピング成功で到達可能・失敗/更新失敗で到達不能。
/// - 待受ありで UPnP 無効なノードは、直接待受として到達可能とみなす([`decide_initial`])。
///
/// `GET /api/v1/status`(T031/T053)がこの値を読み、UI が「外向き接続のみで参加中」を表示する。
#[derive(Clone)]
pub struct InboundReachable {
    flag: Arc<AtomicBool>,
}

impl InboundReachable {
    /// 初期状態を指定して作る。
    pub fn new(initial: bool) -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(initial)),
        }
    }

    /// 現在の着信可否。
    pub fn get(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }

    /// 着信可否を設定する(UPnP タスクがマッピング成否で更新する)。
    pub fn set(&self, reachable: bool) {
        self.flag.store(reachable, Ordering::Relaxed);
    }
}

/// 起動直後の着信可否の初期値を決める(純粋関数 — テスト可能)。
///
/// - 待受なし → 常に不可。
/// - 待受あり + UPnP 無効 → 直接待受として可(操作者がポート転送を用意している前提)。
/// - 待受あり + UPnP 有効 → マッピング成功まで不可(タスクが後で可へ更新する)。
pub fn decide_initial(has_listener: bool, upnp_enabled: bool) -> bool {
    has_listener && !upnp_enabled
}

/// プライマリなローカル LAN アドレスを推定する。
///
/// UDP ソケットを外部宛に「接続」して選択されたインターフェースのローカルアドレスを読む
/// (実際にはパケットを送出しない)。取得できなければ `None`。
fn local_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    // 実送信はしない。到達可能な公開アドレスへ connect して経路とインターフェースを選ばせる。
    socket.connect(("8.8.8.8", 80)).ok()?;
    Some(socket.local_addr().ok()?.ip())
}

/// 1 回のマッピング試行。成功で `Ok(())`。失敗は原因を握りつぶして `Err(())`
/// (Principle II — 内部情報を漏らさない。best-effort のため警告も出さない)。
async fn try_map(listen_port: u16) -> Result<(), ()> {
    let gateway = search_gateway(Default::default()).await.map_err(|_| ())?;
    let ip = local_ip().ok_or(())?;
    let local = SocketAddr::new(ip, listen_port);
    gateway
        .add_port(
            PortMappingProtocol::TCP,
            listen_port,
            local,
            LEASE_SECS,
            MAPPING_DESC,
        )
        .await
        .map_err(|_| ())
}

/// UPnP マッピングの定期更新ループを駆動する。
///
/// 起動直後に 1 回試行し、以後 [`RENEW_INTERVAL`] ごとに更新する。各試行の成否を
/// `reachable` へ反映する(**更新失敗は着信性喪失として即時反映**)。`shutdown` で終了する。
/// `listen_port == 0`(待受なし)では何もしない。
pub async fn run(
    listen_port: u16,
    reachable: InboundReachable,
    mut shutdown: watch::Receiver<bool>,
) {
    if listen_port == 0 {
        return;
    }
    // interval の初回 tick は即時に発火するため、起動直後のマッピングを兼ねる。
    let mut ticker = tokio::time::interval(RENEW_INTERVAL);
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = ticker.tick() => {
                if *shutdown.borrow() {
                    break;
                }
                match try_map(listen_port).await {
                    Ok(()) => {
                        reachable.set(true);
                        tracing::debug!(target: "p2p", port = listen_port, "UPnP マッピングを更新しました");
                    }
                    Err(()) => {
                        // 更新失敗 = 着信性の喪失(即時反映)。警告は出さない(best-effort)。
                        reachable.set(false);
                        tracing::debug!(target: "p2p", "UPnP マッピングに失敗しました(外向きのみで継続)");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_reachability_rules() {
        // 待受なし → 常に不可
        assert!(!decide_initial(false, true));
        assert!(!decide_initial(false, false));
        // 待受あり + UPnP 有効 → 成功まで不可
        assert!(!decide_initial(true, true));
        // 待受あり + UPnP 無効 → 直接待受として可
        assert!(decide_initial(true, false));
    }

    #[test]
    fn reachable_flag_roundtrips() {
        let r = InboundReachable::new(false);
        assert!(!r.get());
        r.set(true);
        assert!(r.get());
        // clone は同じ内部状態を共有する
        let r2 = r.clone();
        r.set(false);
        assert!(!r2.get());
    }

    #[tokio::test]
    async fn run_returns_immediately_without_listener() {
        let r = InboundReachable::new(false);
        let (_tx, rx) = watch::channel(false);
        // listen_port == 0 は即座に戻る(UPnP を試行しない)。
        tokio::time::timeout(Duration::from_secs(1), run(0, r.clone(), rx))
            .await
            .expect("待受なしでは即時終了する");
        assert!(!r.get());
    }
}

//! gossip ハブ(T037)— 受信処理・再伝搬・ローカル発行・一覧供給の結節点
//!
//! established な各接続は自分の送信キュー([`tokio::sync::mpsc::UnboundedSender`])を
//! ハブへ登録する。ハブは:
//! - 受信 EVENT を [`crate::p2p::ingest::IngestState`] に通し、格納成功イベントを
//!   **受信元を除く** established 全ピアへ再伝搬する(伝搬規則 4)
//! - ローカル発行([`GossipHub::publish_local`])を格納し **除外なしで** 全ピアへ送る(T029 用)
//! - [`crate::event::view::ChannelDirectory`] を実装し Web 層へ一覧を供給する(T041/T042 用)
//! - established 接続数(in/out)を報告する(T031 status 用)
//!
//! ロック順序: `state`(IngestState)と `peers`(送信キュー表)は**入れ子で保持しない**。
//! 受信処理は state ロック内で格納判定まで済ませ、解放後に peers ロックで再伝搬する。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use nostr::Event;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

use crate::event::schema::{ChannelListing, VerifyConfig, VerifyReject};
use crate::event::store::{InsertOutcome, StoreConfig};
use crate::event::view::{ChannelDirectory, DiscoveredChannel};
use crate::p2p::frame::Message;
use crate::p2p::ingest::IngestState;
use crate::p2p::session::Direction;
use crate::security::{self, SecurityCategory, SecurityLog};
use crate::store::Store;

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// ポイズン時も内部値を回収してロックを返す(パニックしない)。
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// established 接続 1 本分の送信口。
struct PeerLink {
    addr: String,
    direction: Direction,
    tx: UnboundedSender<Message>,
    /// 接続確立時に観測した相手の時計ずれ(相手申告 `ts` − 自ノード時刻、秒)。
    ///
    /// 未検証の申告値であり、時計ずれ自己診断(T048)にのみ用いる。イベント検証・
    /// 接続可否には使わない(MUST NOT — Principle II / contracts §メッセージ種別)。
    clock_skew_sec: i64,
}

/// 署名済みイベントを `EVENT` メッセージへ包む。
///
/// `serde_json::to_value` は nostr の正準オブジェクト(id/pubkey/created_at/kind/tags/
/// content/sig)を生成する。受信側は再直列化して `verify_incoming` で id/sig を
/// 再計算するため、キー順は保存不要。
pub fn event_to_message(event: &Event) -> Message {
    Message::Event {
        event: serde_json::to_value(event).unwrap_or(Value::Null),
    }
}

/// gossip ハブ。`Arc<GossipHub>` として待受・各接続・Web 層で共有する。
pub struct GossipHub {
    state: Mutex<IngestState>,
    store: Arc<Store>,
    security: Arc<SecurityLog>,
    peers: Mutex<HashMap<u64, PeerLink>>,
    next_id: AtomicU64,
    store_config: StoreConfig,
    verify: VerifyConfig,
    clock: Box<dyn Fn() -> u64 + Send + Sync>,
}

impl GossipHub {
    /// 実時刻クロックで作る(本番配線)。
    pub fn new(
        store: Arc<Store>,
        security: Arc<SecurityLog>,
        store_config: StoreConfig,
        verify: VerifyConfig,
    ) -> Arc<Self> {
        let state = IngestState::new(store_config, verify);
        Arc::new(Self {
            state: Mutex::new(state),
            store,
            security,
            peers: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            store_config,
            verify,
            clock: Box::new(unix_now),
        })
    }

    /// 状態・クロックを注入して作る(テスト用)。
    pub fn with_state_and_clock(
        store: Arc<Store>,
        security: Arc<SecurityLog>,
        state: IngestState,
        clock: Box<dyn Fn() -> u64 + Send + Sync>,
    ) -> Arc<Self> {
        let store_config = state.store_config();
        Arc::new(Self {
            state: Mutex::new(state),
            store,
            security,
            peers: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            store_config,
            verify: VerifyConfig::default(),
            clock,
        })
    }

    /// 現在時刻(unix 秒)。
    pub fn now(&self) -> u64 {
        (self.clock)()
    }

    /// EventStore 設定(SYNC 閾値・鮮度窓の参照用)。
    pub fn store_config(&self) -> StoreConfig {
        self.store_config
    }

    /// 受信検証設定。
    pub fn verify_config(&self) -> VerifyConfig {
        self.verify
    }

    // ---------------------------------------------------------- 接続の登録

    /// 新しい established 接続に一意な id を割り当てる。
    pub fn next_conn_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }

    /// established 接続の送信口を登録する。
    ///
    /// `clock_skew_sec` は接続確立時に観測した相手の時計ずれ(相手申告 `ts` − 自ノード時刻)。
    /// 時計ずれ自己診断(T048)にのみ用いる未検証値。
    pub fn register_peer(
        &self,
        conn_id: u64,
        addr: &str,
        direction: Direction,
        tx: UnboundedSender<Message>,
        clock_skew_sec: i64,
    ) {
        lock(&self.peers).insert(
            conn_id,
            PeerLink {
                addr: addr.to_string(),
                direction,
                tx,
                clock_skew_sec,
            },
        );
    }

    /// established 接続を登録解除する(切断時)。
    pub fn unregister_peer(&self, conn_id: u64) {
        lock(&self.peers).remove(&conn_id);
    }

    /// established 接続の相手アドレス一覧(status 表示・診断用。順序は不定)。
    pub fn established_addrs(&self) -> Vec<String> {
        lock(&self.peers).values().map(|l| l.addr.clone()).collect()
    }

    /// established 各ピアの時計ずれ標本(秒)。時計ずれ自己診断(T048)の中央値算出に使う。
    ///
    /// 相手申告 `ts` に基づく未検証値であり、通知のみに用いる(MUST NOT 検証・接続判断)。
    pub fn clock_skew_samples(&self) -> Vec<i64> {
        lock(&self.peers)
            .values()
            .map(|l| l.clock_skew_sec)
            .collect()
    }

    /// established 接続数 `(inbound, outbound)`(T031 status API 用)。
    pub fn established_counts(&self) -> (usize, usize) {
        let peers = lock(&self.peers);
        let mut inbound = 0;
        let mut outbound = 0;
        for link in peers.values() {
            match link.direction {
                Direction::Inbound => inbound += 1,
                Direction::Outbound => outbound += 1,
            }
        }
        (inbound, outbound)
    }

    // ---------------------------------------------------------- 受信処理

    /// 受信した EVENT を処理する(検証→重複判定→格納→再伝搬)。
    ///
    /// `raw_json` は `EVENT` の `event` を直列化した文字列、`source` は受信元アドレス、
    /// `conn_id` は再伝搬時に除外する受信接続。検証失敗はここでセキュリティイベントを記録する。
    pub fn on_event(&self, raw_json: &str, source: &str, conn_id: u64) {
        let now = self.now();
        let result = { lock(&self.state).ingest(raw_json, source, now) };
        match result {
            Ok(Some(event)) => {
                // URL 警告判定の発動を記録する(FR-012 — data-model §SecurityEvent
                // `url_warning`)。表示側(channels API / UI)の警告フラグとは独立に、
                // ネットワーク境界での検出をセキュリティイベントとして残す。
                if let Ok(listing) = ChannelListing::from_event(&event)
                    && security::url_needs_warning(listing.contact.as_deref().unwrap_or(""))
                {
                    self.security.log(
                        SecurityCategory::UrlWarning,
                        source,
                        "contact url scheme is not http/https",
                    );
                }
                // 格納成功 → 受信元を除く established 全ピアへ再伝搬(伝搬規則 4)
                self.broadcast_event(&event, Some(conn_id));
            }
            // 重複・旧版・鮮度切れ等: 破棄(再伝搬しない・ログもしない)
            Ok(None) => {}
            // 受信検証の失敗はセキュリティイベントとして記録する(Principle II)
            Err(reject) => self.log_reject(&reject, source),
        }
    }

    fn log_reject(&self, reject: &VerifyReject, source: &str) {
        self.security
            .log(reject.category(), source, reject.detail());
    }

    /// イベントを established 全ピアへ送る。`exclude` の接続へは送らない(再伝搬時の受信元)。
    fn broadcast_event(&self, event: &Event, exclude: Option<u64>) {
        let message = event_to_message(event);
        let peers = lock(&self.peers);
        for (id, link) in peers.iter() {
            if Some(*id) == exclude {
                continue;
            }
            // 送信失敗(受信側キュー破棄 = 切断途上)は無視。unregister は pump が行う。
            let _ = link.tx.send(message.clone());
        }
    }

    /// 指定接続へ 1 メッセージ送る(存在すれば `true`)。
    pub fn send_to(&self, conn_id: u64, message: Message) -> bool {
        let peers = lock(&self.peers);
        match peers.get(&conn_id) {
            Some(link) => link.tx.send(message).is_ok(),
            None => false,
        }
    }

    /// 指定接続の送信口の複製を得る(SYNC 応答の平滑化タスクへ渡す)。
    pub fn peer_sender(&self, conn_id: u64) -> Option<UnboundedSender<Message>> {
        lock(&self.peers).get(&conn_id).map(|l| l.tx.clone())
    }

    // ---------------------------------------------------------- ローカル発行

    /// ローカル発行イベントを格納し、should_propagate なら **除外なしで** 全ピアへ送る。
    ///
    /// T029(掲載エンジン)の公開 API。戻り値の [`InsertOutcome`] で格納結果を判別できる。
    pub fn publish_local(&self, event: Event) -> InsertOutcome {
        let outcome = { lock(&self.state).publish_local(event.clone()) };
        if outcome.should_propagate() {
            self.broadcast_event(&event, None);
        }
        outcome
    }

    // ---------------------------------------------------------- SYNC 供給

    /// SYNC_REQ への応答イベント列(`Message::Event`)と件数を組み立てる(T038 が平滑送信)。
    pub fn sync_response(&self, since: i64, now: u64) -> (Vec<Message>, u32) {
        let events = { lock(&self.state).sync_events(since, now) };
        let count = events.len() as u32;
        let messages = events.iter().map(event_to_message).collect();
        (messages, count)
    }

    // ---------------------------------------------------------- 一覧供給・保守

    /// 現在の一覧スナップショット(ミュートは Store から取得して適用)。
    pub fn snapshot(&self) -> Vec<DiscoveredChannel> {
        let mutes = self.store.list_mutes().unwrap_or_default();
        lock(&self.state).snapshot(&mutes)
    }

    /// 鮮度切れ/期限切れイベントを物理回収する(周期保守 — 戻り値は回収件数)。
    pub fn sweep(&self) -> usize {
        lock(&self.state).sweep()
    }
}

impl ChannelDirectory for GossipHub {
    fn list(&self) -> Vec<DiscoveredChannel> {
        self.snapshot()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::schema::{ChannelListing, ChannelStatus};
    use crate::event::store::{DedupCache, EventStore};
    use nostr::{JsonUtil, Keys};
    use std::sync::atomic::AtomicU64 as StdAtomicU64;
    use tokio::sync::mpsc;

    const D1: &str = "0123456789abcdef0123456789abcdef";
    const D2: &str = "0123456789abcdef0123456789abcdee";

    fn listing(d: &str, status: ChannelStatus, title: &str) -> ChannelListing {
        ChannelListing {
            channel_id: d.into(),
            title: title.into(),
            summary: None,
            genre: None,
            status,
            starts: 1_700_000_000,
            current_participants: -1,
            streaming: None,
            bitrate_kbps: None,
            content_type: None,
            tip: None,
            contact: None,
            relays: -1,
            track: None,
        }
    }

    fn mk(keys: &Keys, d: &str, created: u64, status: ChannelStatus, title: &str) -> Event {
        listing(d, status, title).sign(keys, created, 0).unwrap()
    }

    fn hub_at(now: u64) -> Arc<GossipHub> {
        let dir = tempfile::tempdir().unwrap();
        // tempdir をリークさせてログファイルを生かす(テスト内のみ)。
        hub_at_with_log(now, dir.keep().join("sec.log"))
    }

    fn hub_at_with_log(now: u64, path: std::path::PathBuf) -> Arc<GossipHub> {
        let clock = Arc::new(StdAtomicU64::new(now));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let security = Arc::new(SecurityLog::new(path).unwrap());
        let cfg = StoreConfig::default();
        let c2 = Arc::clone(&clock);
        let estore = EventStore::with_clock(cfg, Box::new(move || c2.load(Ordering::SeqCst)));
        let c3 = Arc::clone(&clock);
        let dedup = DedupCache::with_clock(
            cfg.freshness_window_sec,
            Box::new(move || c3.load(Ordering::SeqCst)),
        );
        let state = IngestState::with_parts(estore, dedup, VerifyConfig::default(), cfg);
        let c4 = Arc::clone(&clock);
        GossipHub::with_state_and_clock(
            store,
            security,
            state,
            Box::new(move || c4.load(Ordering::SeqCst)),
        )
    }

    #[tokio::test]
    async fn on_event_stores_and_rerpopagates_excluding_source() {
        let hub = hub_at(1_700_000_050);
        let keys = Keys::generate();

        // 2 本の established 接続を登録(conn 1 = 受信元、conn 2 = 他ピア)
        let (tx1, mut rx1) = mpsc::unbounded_channel();
        let (tx2, mut rx2) = mpsc::unbounded_channel();
        hub.register_peer(1, "peer1:7147", Direction::Inbound, tx1, 0);
        hub.register_peer(2, "peer2:7147", Direction::Outbound, tx2, 0);

        let e = mk(&keys, D1, 1_700_000_050, ChannelStatus::Live, "x");
        hub.on_event(&e.as_json(), "peer1:7147", 1);

        // 受信元(conn 1)へは再送しない
        assert!(rx1.try_recv().is_err(), "受信元へは再伝搬しない");
        // 他ピア(conn 2)へは EVENT が届く
        match rx2.try_recv() {
            Ok(Message::Event { .. }) => {}
            other => panic!("他ピアへ EVENT が届くべき: {other:?}"),
        }
        assert_eq!(hub.snapshot().len(), 1);
    }

    #[tokio::test]
    async fn duplicate_event_is_not_rerpopagated() {
        let hub = hub_at(1_700_000_050);
        let keys = Keys::generate();
        let (tx2, mut rx2) = mpsc::unbounded_channel();
        hub.register_peer(2, "peer2:7147", Direction::Outbound, tx2, 0);

        let e = mk(&keys, D1, 1_700_000_050, ChannelStatus::Live, "x");
        hub.on_event(&e.as_json(), "peer1:7147", 1);
        let _ = rx2.try_recv().unwrap(); // 初回は伝搬
        // 同一 id 再受信 → 破棄(伝搬しない)
        hub.on_event(&e.as_json(), "peer1:7147", 1);
        assert!(rx2.try_recv().is_err(), "重複は再伝搬しない");
    }

    #[tokio::test]
    async fn publish_local_broadcasts_to_all() {
        let hub = hub_at(1_700_000_050);
        let keys = Keys::generate();
        let (tx1, mut rx1) = mpsc::unbounded_channel();
        let (tx2, mut rx2) = mpsc::unbounded_channel();
        hub.register_peer(1, "peer1:7147", Direction::Inbound, tx1, 0);
        hub.register_peer(2, "peer2:7147", Direction::Outbound, tx2, 0);

        let e = mk(&keys, D1, 1_700_000_050, ChannelStatus::Live, "x");
        let outcome = hub.publish_local(e);
        assert!(outcome.should_propagate());
        // 除外なしで両ピアへ届く
        assert!(matches!(rx1.try_recv(), Ok(Message::Event { .. })));
        assert!(matches!(rx2.try_recv(), Ok(Message::Event { .. })));
    }

    #[test]
    fn established_counts_split_by_direction() {
        let hub = hub_at(1_700_000_050);
        let (tx1, _r1) = mpsc::unbounded_channel();
        let (tx2, _r2) = mpsc::unbounded_channel();
        let (tx3, _r3) = mpsc::unbounded_channel();
        hub.register_peer(1, "a:1", Direction::Inbound, tx1, -2);
        hub.register_peer(2, "b:1", Direction::Outbound, tx2, 5);
        hub.register_peer(3, "c:1", Direction::Outbound, tx3, 400);
        assert_eq!(hub.established_counts(), (1, 2));
        let mut addrs = hub.established_addrs();
        addrs.sort();
        assert_eq!(addrs, vec!["a:1", "b:1", "c:1"]);
        // 時計ずれ標本は登録した各ピアの値を返す(順不同)。
        let mut skews = hub.clock_skew_samples();
        skews.sort();
        assert_eq!(skews, vec![-2, 5, 400]);
        hub.unregister_peer(2);
        assert_eq!(hub.established_counts(), (1, 1));
    }

    #[tokio::test]
    async fn invalid_event_is_logged_and_not_propagated() {
        let hub = hub_at(1_700_000_050);
        let keys = Keys::generate();
        let (tx2, mut rx2) = mpsc::unbounded_channel();
        hub.register_peer(2, "peer2:7147", Direction::Outbound, tx2, 0);
        let e = mk(&keys, D1, 1_700_000_050, ChannelStatus::Live, "x");
        let tampered = e.as_json().replace("\"content\":\"\"", "\"content\":\"x\"");
        hub.on_event(&tampered, "peer1:7147", 1);
        assert!(rx2.try_recv().is_err(), "検証失敗は伝搬しない");
    }

    #[tokio::test]
    async fn warned_contact_url_is_logged_on_ingest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sec.log");
        let hub = hub_at_with_log(1_700_000_050, path.clone());
        let keys = Keys::generate();

        // http/https 以外のコンタクト URL を持つ検証済みイベント → url_warning を記録。
        let mut l = listing(D1, ChannelStatus::Live, "x");
        l.contact = Some("javascript:alert(1)".into());
        let e = l.sign(&keys, 1_700_000_050, 0).unwrap();
        hub.on_event(&e.as_json(), "peer1:7147", 1);
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("url_warning"), "記録内容: {content}");

        // http/https の URL では発火しない。
        let mut l2 = listing(D2, ChannelStatus::Live, "y");
        l2.contact = Some("https://example.com/".into());
        let e2 = l2.sign(&keys, 1_700_000_051, 0).unwrap();
        hub.on_event(&e2.as_json(), "peer1:7147", 1);
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            content.matches("url_warning").count(),
            1,
            "正当な URL では url_warning を記録しない: {content}"
        );
    }

    #[test]
    fn sync_response_wraps_events() {
        let hub = hub_at(1_700_000_100);
        let keys = Keys::generate();
        let e = mk(&keys, D1, 1_700_000_090, ChannelStatus::Live, "x");
        hub.on_event(&e.as_json(), "p", 9);
        let (msgs, count) = hub.sync_response(0, 1_700_000_100);
        assert_eq!(count, 1);
        assert!(matches!(msgs[0], Message::Event { .. }));
    }
}

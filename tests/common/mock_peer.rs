//! gossip 契約の参照実装 = インプロセス・モックピア(research R11)
//!
//! 本ソフトウェアの実装([`peca_p2p_yp::p2p::frame`] / `session`)と**同一のフレーム
//! 入出力**を用いる TCP サーバとして、契約書とモック実装の乖離を検出する。共有フィクスチャ
//! `tests/contract/fixtures/gossip_vectors.json` は本実装とモックの双方に適用できる
//! ([`encode_message`] / [`decode_message`] は frame モジュールをそのまま使う)。
//!
//! 機能: HELLO/HELLO_ACK ハンドシェイク(inbound = 相手からのダイヤルを受ける)、
//! SYNC_REQ への応答(保持イベント + SYNC_DONE)、EVENT の送受・記録、PING→PONG。
//!
//! あわせて [`TestNode`] を提供する(実 [`P2pRuntime`] を外向きのみで起動する軽量ハーネス。
//! T044 統合テストと cucumber の両方から使える共通ユーティリティ)。
//!
//! 統括の T033(US1 統合テスト)も本ファイルを `#[path]` で取り込む前提。

#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nostr::Event;
use serde_json::Value;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

use peca_p2p_yp::event::schema::VerifyConfig;
use peca_p2p_yp::event::store::StoreConfig;
use peca_p2p_yp::event::view::DiscoveredChannel;
use peca_p2p_yp::p2p::frame::{Hello, Message, read_frame, write_frame};
use peca_p2p_yp::p2p::hub::GossipHub;
use peca_p2p_yp::p2p::peers::{PeerManager, PeerManagerConfig, ReachabilityState};
use peca_p2p_yp::p2p::runtime::P2pRuntime;
use peca_p2p_yp::p2p::upnp::{self, InboundReachable};
use peca_p2p_yp::security::SecurityLog;
use peca_p2p_yp::store::{PeerEndpoint, PeerSource, Store};

/// mock の固定 HELLO nonce(実ノードの nonce と衝突しない値 — 自己接続と誤検出させない)。
pub const MOCK_NONCE: u64 = 0x00CD_EFAB_1234_5678;
/// プロトコルバージョン(v1)。
const PROTOCOL_VERSION: u32 = 1;

/// 現在の unix 秒。
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// フレームのエンコード/デコード(frame モジュールの薄いラッパ — フィクスチャ共有点)
// ---------------------------------------------------------------------------

/// メッセージをフレームのバイト列へ(契約フィクスチャ検証で使用)。
pub fn encode_message(message: &Message) -> Vec<u8> {
    peca_p2p_yp::p2p::frame::encode(message).expect("フレーム符号化")
}

/// フレームのペイロード JSON をメッセージへ(契約フィクスチャ検証で使用)。
pub fn decode_message(payload: &[u8]) -> Option<Message> {
    peca_p2p_yp::p2p::frame::decode_payload(payload).ok()
}

// ---------------------------------------------------------------------------
// MockPeer
// ---------------------------------------------------------------------------

/// モックピアの共有状態(接続ハンドラと制御 API で共有)。
#[derive(Clone)]
struct Shared {
    /// SYNC_REQ 応答で返す保持イベント(生 JSON 値)。
    served: Arc<Mutex<Vec<Value>>>,
    /// 受信した EVENT の記録(生 JSON 値)。
    received: Arc<Mutex<Vec<Value>>>,
    /// 接続中ノードへの非請求 EVENT 送出チャネル。
    push_tx: broadcast::Sender<Value>,
    /// 自ノードの待受ポート(HELLO で申告)。
    listen_port: u16,
    /// GET_PEERS 応答で返すアドレス列(PEX 検証用)。
    pex_peers: Arc<Mutex<Vec<String>>>,
}

/// 契約参照実装としてのモックピア(TCP でダイヤルを受ける)。
pub struct MockPeer {
    addr: String,
    shared: Shared,
    shutdown: watch::Sender<bool>,
    _accept: JoinHandle<()>,
}

impl MockPeer {
    /// 127.0.0.1 の任意ポートで待ち受け、接続受付ループを起動する。
    pub async fn spawn() -> MockPeer {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("mock bind");
        let addr = listener.local_addr().unwrap().to_string();
        let port = listener.local_addr().unwrap().port();
        let (push_tx, _rx) = broadcast::channel(256);
        let shared = Shared {
            served: Arc::new(Mutex::new(Vec::new())),
            received: Arc::new(Mutex::new(Vec::new())),
            push_tx,
            listen_port: port,
            pex_peers: Arc::new(Mutex::new(Vec::new())),
        };
        let (sd_tx, sd_rx) = watch::channel(false);
        let accept = {
            let shared = shared.clone();
            tokio::spawn(async move { accept_loop(listener, shared, sd_rx).await })
        };
        MockPeer {
            addr,
            shared,
            shutdown: sd_tx,
            _accept: accept,
        }
    }

    /// 待受アドレス(`127.0.0.1:PORT`)。
    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// SYNC_REQ 応答で返す署名済みイベントを追加する。
    pub fn serve_signed(&self, event: &Event) {
        self.serve_value(serde_json::to_value(event).unwrap());
    }

    /// SYNC_REQ 応答で返す生 JSON 値を追加する(不正イベント注入にも使う)。
    pub fn serve_value(&self, value: Value) {
        self.shared.served.lock().unwrap().push(value);
    }

    /// 接続中ノードへ非請求 EVENT を即時送出する(署名済み)。
    pub fn push_signed(&self, event: &Event) {
        let _ = self
            .shared
            .push_tx
            .send(serde_json::to_value(event).unwrap());
    }

    /// 接続中ノードへ非請求 EVENT として生 JSON 値を即時送出する
    /// (署名不正・過大イベント等の悪性入力の注入に使う — T055)。
    pub fn push_value(&self, value: Value) {
        let _ = self.shared.push_tx.send(value);
    }

    /// これまでに受信した EVENT(生 JSON 値)。
    pub fn received(&self) -> Vec<Value> {
        self.shared.received.lock().unwrap().clone()
    }

    /// GET_PEERS 応答で返すアドレスを追加する(PEX 検証用)。
    pub fn share_peer(&self, addr: &str) {
        self.shared.pex_peers.lock().unwrap().push(addr.to_string());
    }
}

impl Drop for MockPeer {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

/// 接続受付ループ(接続ごとにハンドラを起動)。
async fn accept_loop(listener: TcpListener, shared: Shared, mut shutdown: watch::Receiver<bool>) {
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _peer)) => {
                        let shared = shared.clone();
                        let sd = shutdown.clone();
                        tokio::spawn(async move { handle_conn(stream, shared, sd).await; });
                    }
                    Err(_) => continue,
                }
            }
        }
    }
}

/// 1 接続を処理する(inbound: HELLO を待ち HELLO_ACK を返して established)。
async fn handle_conn(stream: TcpStream, shared: Shared, mut shutdown: watch::Receiver<bool>) {
    let (mut reader, mut writer) = stream.into_split();

    // 最初のフレームは HELLO でなければならない。
    match read_frame(&mut reader).await {
        Ok(Some(frame)) => match frame.message {
            Message::Hello(_) => {}
            _ => return,
        },
        _ => return,
    }
    let ack = Message::HelloAck(Hello {
        version: PROTOCOL_VERSION,
        listen_port: shared.listen_port,
        features: vec![],
        nonce: MOCK_NONCE,
        ts: unix_now() as i64,
    });
    if write_frame(&mut writer, &ack).await.is_err() {
        return;
    }

    let mut push_rx = shared.push_tx.subscribe();
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            pushed = push_rx.recv() => {
                match pushed {
                    Ok(value) => {
                        if write_frame(&mut writer, &Message::Event { event: value }).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => {}
                }
            }
            res = read_frame(&mut reader) => {
                let frame = match res {
                    Ok(Some(f)) => f,
                    _ => break, // EOF / エラーで終了
                };
                match frame.message {
                    Message::SyncReq { .. } => {
                        // 保持イベントを EVENT で返し、末尾に SYNC_DONE。
                        let events = shared.served.lock().unwrap().clone();
                        let count = events.len() as u32;
                        let mut sent_ok = true;
                        for value in events {
                            if write_frame(&mut writer, &Message::Event { event: value }).await.is_err() {
                                sent_ok = false;
                                break;
                            }
                        }
                        if !sent_ok {
                            break;
                        }
                        if write_frame(&mut writer, &Message::SyncDone { count }).await.is_err() {
                            break;
                        }
                    }
                    Message::Event { event } => {
                        shared.received.lock().unwrap().push(event);
                    }
                    Message::Ping { nonce } => {
                        if write_frame(&mut writer, &Message::Pong { nonce }).await.is_err() {
                            break;
                        }
                    }
                    Message::GetPeers => {
                        // 設定された PEX 候補を PEERS で返す(未設定なら空)。
                        let peers = shared.pex_peers.lock().unwrap().clone();
                        if write_frame(&mut writer, &Message::Peers { peers }).await.is_err() {
                            break;
                        }
                    }
                    Message::Close { .. } => break,
                    // SYNC_DONE・PONG・PEERS などは無視(前方互換)。
                    _ => {}
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// TestNode(実 P2pRuntime を外向きのみで起動する軽量ハーネス)
// ---------------------------------------------------------------------------

/// 実 gossip ランタイムを外向き接続のみ(待受なし = FR-016)で駆動するテストノード。
///
/// [`add_manual_peer`](TestNode::add_manual_peer) でモックピアを手動登録すると、外向き
/// 維持ループが接続し、established 直後の SYNC で一覧を構築する。一覧は [`snapshot`] /
/// [`wait_for_channel`] で観測する。
pub struct TestNode {
    hub: Arc<GossipHub>,
    peers: Arc<PeerManager>,
    reachability: Arc<ReachabilityState>,
    /// 着信可否の共有状態(main.rs と同じ配線 — 待受なしノードは常に不可)。
    inbound: InboundReachable,
    /// P2P 待受アドレス(待受ありノードのみ)。
    listen_addr: Option<String>,
    /// 永続ストア(ミュート等の利用者操作をテストから行う — T055)。
    store: Arc<Store>,
    /// セキュリティイベントログ(flush 用)とそのファイルパス(検証用 — T055)。
    security: Arc<SecurityLog>,
    security_path: std::path::PathBuf,
    shutdown: watch::Sender<bool>,
    _handles: Vec<JoinHandle<()>>,
    _dir: tempfile::TempDir,
}

impl TestNode {
    /// 外向きのみのノードを起動する(nonce は自己接続回避のため引数指定)。
    pub async fn spawn(nonce: u64) -> TestNode {
        Self::spawn_inner(nonce, false).await
    }

    /// P2P 待受ありの実ノードを起動する(複数実ノードのトポロジ構成用 — T049)。
    ///
    /// 待受アドレスは [`listen_addr`](TestNode::listen_addr) で得られ、他ノードの
    /// [`add_manual_peer`](TestNode::add_manual_peer) に渡してメッシュ/チェーンを組む。
    pub async fn spawn_listening(nonce: u64) -> TestNode {
        Self::spawn_inner(nonce, true).await
    }

    async fn spawn_inner(nonce: u64, listening: bool) -> TestNode {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let peers = Arc::new(PeerManager::new(
            Arc::clone(&store),
            PeerManagerConfig::default(),
        ));
        let dir = tempfile::tempdir().unwrap();
        let security_path = dir.path().join("security.log");
        let security = Arc::new(SecurityLog::new(&security_path).unwrap());
        let hub = GossipHub::new(
            Arc::clone(&store),
            Arc::clone(&security),
            StoreConfig::default(),
            VerifyConfig::default(),
        );
        // 待受ありなら 127.0.0.1 の任意ポートへバインドする。
        let (listener, listen_port, listen_addr) = if listening {
            let l = TcpListener::bind("127.0.0.1:0").await.expect("p2p bind");
            let addr = l.local_addr().unwrap();
            (Some(l), addr.port(), Some(addr.to_string()))
        } else {
            (None, 0, None)
        };
        let runtime = Arc::new(P2pRuntime::new(
            Arc::clone(&peers),
            Arc::clone(&security),
            Arc::clone(&hub),
            nonce,
            listen_port,
            true, // PEX 有効
        ));
        let reachability = runtime.reachability();
        let (sd_tx, sd_rx) = watch::channel(false);
        let handles = runtime.spawn(listener, sd_rx);
        // 着信可否は main.rs と同じ初期化(待受なし → 常に不可)。
        let inbound = InboundReachable::new(upnp::decide_initial(listening, true));
        TestNode {
            hub,
            peers,
            reachability,
            inbound,
            listen_addr,
            store,
            security,
            security_path,
            shutdown: sd_tx,
            _handles: handles,
            _dir: dir,
        }
    }

    /// 永続ストア(ミュート登録など利用者側の緩和操作をテストから行う — T055)。
    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    /// セキュリティイベントログの現在の内容(JSON Lines)。集約分は flush してから読む。
    pub fn security_log_text(&self) -> String {
        self.security.flush();
        std::fs::read_to_string(&self.security_path).unwrap_or_default()
    }

    /// 指定カテゴリのセキュリティイベントが記録されるまで最大 `timeout` 待つ。
    pub async fn wait_for_security(&self, category: &str, timeout: Duration) -> bool {
        let start = Instant::now();
        loop {
            if self.security_log_text().contains(category) {
                return true;
            }
            if start.elapsed() >= timeout {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// P2P 待受アドレス(`127.0.0.1:PORT` — [`spawn_listening`](TestNode::spawn_listening) 時のみ)。
    pub fn listen_addr(&self) -> &str {
        self.listen_addr.as_deref().expect("待受ありノードのみ")
    }

    /// 全ピア到達不能状態と回復通知の共有ハンドル(US3 障害耐性テスト用)。
    pub fn reachability(&self) -> Arc<ReachabilityState> {
        Arc::clone(&self.reachability)
    }

    /// モックピアを手動ピアとして登録する(外向き接続の候補になる)。
    pub fn add_manual_peer(&self, addr: &str) {
        self.peers
            .add_peer(addr, PeerSource::Manual)
            .expect("手動ピア登録");
    }

    /// 現在の一覧スナップショット。
    pub fn snapshot(&self) -> Vec<DiscoveredChannel> {
        self.hub.snapshot()
    }

    /// established 接続数 `(inbound, outbound)`。
    pub fn established_counts(&self) -> (usize, usize) {
        self.hub.established_counts()
    }

    /// 既知ピア一覧(PEX で獲得した候補の確認用)。
    pub fn known_peers(&self) -> Vec<PeerEndpoint> {
        self.peers.list_peers().unwrap_or_default()
    }

    /// 指定アドレスが既知ピアに現れるまで最大 `timeout` 待つ(PEX 候補登録の確認用)。
    pub async fn wait_for_peer(&self, addr: &str, timeout: Duration) -> bool {
        let start = Instant::now();
        loop {
            if self.known_peers().iter().any(|p| p.addr == addr) {
                return true;
            }
            if start.elapsed() >= timeout {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// 着信可否(外向きのみノードは常に `false` = 「外向き接続のみで参加中」)。
    ///
    /// main.rs と同じ [`InboundReachable`] 共有状態を経由して読む(待受なしのため
    /// [`upnp::decide_initial`] の初期値 `false` のまま)。
    pub fn inbound_reachable(&self) -> bool {
        self.inbound.get()
    }

    /// 共有ハブ(ローカル発行など細かな操作をテストから行う場合)。
    pub fn hub(&self) -> &Arc<GossipHub> {
        &self.hub
    }

    /// 指定 `channel_id` が一覧に現れるまで最大 `timeout` 待つ(現れれば `true`)。
    pub async fn wait_for_channel(&self, channel_id: &str, timeout: Duration) -> bool {
        self.wait_until(timeout, |rows| {
            rows.iter().any(|c| c.channel_id == channel_id)
        })
        .await
    }

    /// 一覧が述語を満たすまで最大 `timeout` ポーリングする。
    pub async fn wait_until(
        &self,
        timeout: Duration,
        pred: impl Fn(&[DiscoveredChannel]) -> bool,
    ) -> bool {
        let start = Instant::now();
        loop {
            let rows = self.snapshot();
            if pred(&rows) {
                return true;
            }
            if start.elapsed() >= timeout {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Drop for TestNode {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

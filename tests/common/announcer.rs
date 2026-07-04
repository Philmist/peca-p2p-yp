//! US1 統合テスト用の掲載側ハーネス(T033)
//!
//! main.rs の起動配線と同じ構成(PCP 待受 → レジストリ → 掲載エンジン → gossip ハブ →
//! 外向き P2P)をインプロセスで再現する [`AnnouncerNode`] と、PeerCastStation の
//! announce 接続を模す [`PcpClient`](契約フィクスチャと同じ atom 構造)を提供する。
//!
//! `tests/integration/announce_flow.rs` と cucumber(`tests/steps/us1.rs`)の双方から
//! `#[path]` で取り込む。

#![allow(dead_code)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

use peca_p2p_yp::event::publish::{EventSink, PublishEngine};
use peca_p2p_yp::event::schema::VerifyConfig;
use peca_p2p_yp::event::store::StoreConfig;
use peca_p2p_yp::event::view::DiscoveredChannel;
use peca_p2p_yp::identity::IdentityManager;
use peca_p2p_yp::p2p::hub::GossipHub;
use peca_p2p_yp::p2p::peers::{PeerManager, PeerManagerConfig};
use peca_p2p_yp::p2p::runtime::P2pRuntime;
use peca_p2p_yp::pcp::atom::Atom;
use peca_p2p_yp::pcp::channel::{ChannelChange, ChannelRegistry};
use peca_p2p_yp::pcp::session;
use peca_p2p_yp::security::SecurityLog;
use peca_p2p_yp::store::{PeerSource, Store};

/// PCP_HOST flg1: playing(RECV)。
pub const FLG1_RECV: u8 = 0x10;
/// PCP_HOST flg1: 直接接続可(DIRECT)。
pub const FLG1_DIRECT: u8 = 0x04;
/// PCP_HOST flg1: firewalled(PUSH)。
pub const FLG1_PUSH: u8 = 0x08;

/// [`PublishEngine`] → [`GossipHub`] の発行受け口(main.rs の HubSink と同じ配線)。
struct HubSink(Arc<GossipHub>);

impl EventSink for HubSink {
    fn publish_local(&self, event: nostr::Event) -> bool {
        self.0.publish_local(event).should_propagate()
    }
}

/// 掲載側ノード(PCP 待受+掲載エンジン+外向き gossip)。
pub struct AnnouncerNode {
    pub hub: Arc<GossipHub>,
    pub identity: Arc<IdentityManager>,
    pub registry: Arc<ChannelRegistry>,
    peers: Arc<PeerManager>,
    /// PCP 待受アドレス(`127.0.0.1:PORT`)。
    pcp_addr: String,
    /// P2P 待受アドレス(`spawn_listening` で起動した場合のみ)。
    p2p_addr: Option<String>,
    /// 掲載に使うペルソナ pubkey(spawn 時に作成・自動選択)。
    pub persona_pubkey: String,
    shutdown: watch::Sender<bool>,
    _handles: Vec<JoinHandle<()>>,
    _dir: tempfile::TempDir,
}

impl AnnouncerNode {
    /// 掲載側ノードを起動する(P2P は外向きのみ。ペルソナ 1 つ作成済み)。
    pub async fn spawn(nonce: u64) -> AnnouncerNode {
        Self::spawn_inner(nonce, false).await
    }

    /// P2P 待受つきで起動する(SC-003 の 2 ノード連鎖検証用)。
    pub async fn spawn_listening(nonce: u64) -> AnnouncerNode {
        Self::spawn_inner(nonce, true).await
    }

    async fn spawn_inner(nonce: u64, with_p2p_listener: bool) -> AnnouncerNode {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let peers = Arc::new(PeerManager::new(
            Arc::clone(&store),
            PeerManagerConfig::default(),
        ));
        let dir = tempfile::tempdir().unwrap();
        let security = Arc::new(SecurityLog::new(dir.path().join("security.log")).unwrap());
        let hub = GossipHub::new(
            Arc::clone(&store),
            Arc::clone(&security),
            StoreConfig::default(),
            VerifyConfig::default(),
        );
        // P2P 待受(SC-003 検証時のみ。それ以外は外向きのみ = FR-016 相当)。
        let (p2p_listener, p2p_addr, listen_port) = if with_p2p_listener {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            (Some(listener), Some(addr.to_string()), addr.port())
        } else {
            (None, None, 0)
        };
        let runtime = Arc::new(P2pRuntime::new(
            Arc::clone(&peers),
            Arc::clone(&security),
            Arc::clone(&hub),
            nonce,
            listen_port,
            true, // PEX 有効
        ));
        let (sd_tx, sd_rx) = watch::channel(false);
        let mut handles = runtime.spawn(p2p_listener, sd_rx.clone());

        // PCP 待受(loopback 任意ポート)。
        let pcp_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let pcp_addr = pcp_listener.local_addr().unwrap().to_string();
        let registry = ChannelRegistry::new();
        {
            let registry = Arc::clone(&registry);
            let security = Arc::clone(&security);
            let sd = sd_rx.clone();
            handles.push(tokio::spawn(async move {
                session::serve(pcp_listener, registry, security, sd).await;
            }));
        }

        // ペルソナ(自動選択)+ 掲載エンジン + 変更契機ブリッジ。
        let identity = Arc::new(IdentityManager::new(Arc::clone(&store)));
        let persona_pubkey = identity.create("US1 テスト").unwrap().pubkey;
        let sink: Arc<dyn EventSink> = Arc::new(HubSink(Arc::clone(&hub)));
        let engine = Arc::new(PublishEngine::new(Arc::clone(&identity), sink, 60));
        handles.push(spawn_publish_bridge(
            registry.subscribe(),
            engine,
            sd_rx.clone(),
        ));

        AnnouncerNode {
            hub,
            identity,
            registry,
            peers,
            pcp_addr,
            p2p_addr,
            persona_pubkey,
            shutdown: sd_tx,
            _handles: handles,
            _dir: dir,
        }
    }

    /// PCP 待受アドレス。
    pub fn pcp_addr(&self) -> &str {
        &self.pcp_addr
    }

    /// P2P 待受アドレス(`spawn_listening` で起動した場合のみ)。
    pub fn p2p_addr(&self) -> Option<&str> {
        self.p2p_addr.as_deref()
    }

    /// モックピアを手動ピアとして登録する(外向き接続の候補になる)。
    pub fn add_manual_peer(&self, addr: &str) {
        self.peers
            .add_peer(addr, PeerSource::Manual)
            .expect("手動ピア登録");
    }

    /// established 接続が現れるまで最大 `timeout` 待つ。
    pub async fn wait_established(&self, timeout: Duration) -> bool {
        let start = Instant::now();
        loop {
            let (i, o) = self.hub.established_counts();
            if i + o > 0 {
                return true;
            }
            if start.elapsed() >= timeout {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// 自ノードの一覧スナップショット。
    pub fn snapshot(&self) -> Vec<DiscoveredChannel> {
        self.hub.snapshot()
    }

    /// 一覧が述語を満たすまで最大 `timeout` ポーリングする。
    pub async fn wait_until(
        &self,
        timeout: Duration,
        pred: impl Fn(&[DiscoveredChannel]) -> bool,
    ) -> bool {
        let start = Instant::now();
        loop {
            if pred(&self.snapshot()) {
                return true;
            }
            if start.elapsed() >= timeout {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Drop for AnnouncerNode {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

/// PCP 変更通知 → 掲載エンジンのブリッジ(main.rs と同じ配線)。
fn spawn_publish_bridge(
    mut rx: broadcast::Receiver<ChannelChange>,
    engine: Arc<PublishEngine>,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                msg = rx.recv() => match msg {
                    Ok(ChannelChange::Announced(ch)) | Ok(ChannelChange::Updated(ch)) => {
                        let _ = engine.publish_listing(&ch.to_listing());
                    }
                    Ok(ChannelChange::Ended(ch)) => {
                        let _ = engine.publish_ended(&ch.to_listing());
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            }
        }
    })
}

// ---------------------------------------------------------------------------
// PCP 疑似クライアント(PeerCastStation の announce 接続を模す)
// ---------------------------------------------------------------------------

/// PCP announce 接続の疑似クライアント。
pub struct PcpClient {
    stream: TcpStream,
    session_id: [u8; 16],
}

impl PcpClient {
    /// 接続して HELO→OLEH を完了する。
    pub async fn connect(addr: &str, session_id: [u8; 16]) -> PcpClient {
        let mut stream = TcpStream::connect(addr).await.expect("PCP 接続");
        let helo = Atom::parent(
            "helo",
            vec![
                Atom::bytes("sid", &session_id),
                Atom::str("agnt", "PseudoStation/1.0"),
            ],
        );
        stream.write_all(&helo.to_bytes()).await.expect("HELO 送信");
        stream.flush().await.unwrap();
        let oleh = read_atom(&mut stream).await.expect("OLEH 受信");
        assert!(oleh.id().matches("oleh"), "応答は OLEH であるべき");
        PcpClient { stream, session_id }
    }

    /// BCST を送る(配信開始・詳細変更の両方に使う)。
    pub async fn broadcast(&mut self, cid: &[u8; 16], name: &str, genre: &str, desc: &str) {
        let bcst = Atom::parent(
            "bcst",
            vec![Atom::parent(
                "chan",
                vec![
                    Atom::bytes("cid", cid),
                    Atom::bytes("bcid", &self.session_id),
                    Atom::parent(
                        "info",
                        vec![
                            Atom::str("name", name),
                            Atom::str("gnre", genre),
                            Atom::str("desc", desc),
                            Atom::str("url", "https://example.com/"),
                            Atom::i32("bitr", 500),
                            Atom::str("type", "FLV"),
                        ],
                    ),
                    Atom::parent(
                        "host",
                        vec![
                            Atom::bytes("ip", &[198, 51, 100, 1]),
                            Atom::i16("port", 7144),
                            Atom::i32("numl", 2),
                            Atom::i32("numr", 1),
                            Atom::u8v("flg1", FLG1_RECV | FLG1_DIRECT),
                        ],
                    ),
                ],
            )],
        );
        self.stream
            .write_all(&bcst.to_bytes())
            .await
            .expect("BCST 送信");
        self.stream.flush().await.unwrap();
    }

    /// PCP_QUIT を送って切断する(配信終了)。
    pub async fn quit(mut self) {
        let quit = Atom::i32("quit", 1000);
        let _ = self.stream.write_all(&quit.to_bytes()).await;
        let _ = self.stream.flush().await;
        let _ = self.stream.shutdown().await;
        // drop で TCP 切断 → サーバ側は全チャンネルを即 ended にする
    }
}

/// ストリームから atom を 1 つ読み取る(分割着信対応)。
async fn read_atom(stream: &mut TcpStream) -> Option<Atom> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match Atom::try_decode(&buf) {
            Ok(Some((atom, used))) => {
                buf.drain(..used);
                return Some(atom);
            }
            Ok(None) => {}
            Err(_) => return None,
        }
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

//! スレホストノード(006-livechat-thread US1 統合テスト用ハーネス — T026)
//!
//! `P2pRuntime::new_with_livechat` に [`LivechatRegistry`] を配線した**実ランタイム**を
//! P2P 待受つきで起動し、スレホスト役(配信者)を模す。gossip ハブ(announce 伝搬)と
//! スレ配送(THREAD_JOIN → WELCOME → 同期)を単一の P2P 待受で多重化する
//! (session.rs の「1 TCP 接続 = 1 用途」分岐 — contracts/thread-delivery.md)。
//!
//! `tests/integration/livechat.rs` と cucumber(`tests/steps/livechat.rs`)の双方から
//! `#[path]` で取り込む。視聴者側は [`crate::mock_peer::TestNode`](gossip 一覧受信)+
//! `peca_p2p_yp::livechat::participant`(明示スレ接続)で構成する。

#![allow(dead_code)]

use std::sync::Arc;

use nostr::Keys;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use peca_p2p_yp::event::schema::VerifyConfig;
use peca_p2p_yp::event::store::StoreConfig;
use peca_p2p_yp::livechat::registry::{LivechatRegistry, sign_res};
use peca_p2p_yp::livechat::thread::BoardSettings;
use peca_p2p_yp::p2p::hub::GossipHub;
use peca_p2p_yp::p2p::peers::{PeerManager, PeerManagerConfig};
use peca_p2p_yp::p2p::runtime::P2pRuntime;
use peca_p2p_yp::security::SecurityLog;
use peca_p2p_yp::store::{PeerSource, Store};

const GUID: &str = "0123456789abcdef0123456789abcdef";

/// スレホストノード(実 P2P 待受 + gossip ハブ + LivechatRegistry)。
pub struct LivechatHostNode {
    hub: Arc<GossipHub>,
    peers: Arc<PeerManager>,
    registry: Arc<LivechatRegistry>,
    /// スレ主ペルソナ鍵(掲載ペルソナ = board_id の源。announce/WELCOME/ORDER 署名)。
    persona: Keys,
    /// P2P 待受アドレス(`127.0.0.1:PORT`)。gossip・スレ配送を多重化する。tip でもある。
    listen_addr: String,
    shutdown: watch::Sender<bool>,
    _handles: Vec<JoinHandle<()>>,
    _dir: tempfile::TempDir,
}

impl LivechatHostNode {
    /// スレホストを起動する(P2P 待受つき・スレ機能有効)。ペルソナ鍵は自動生成。
    pub async fn spawn(nonce: u64) -> LivechatHostNode {
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
        // P2P 待受(127.0.0.1 任意ポート)。gossip とスレ配送を同一待受で受ける。
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listen_addr = listener.local_addr().unwrap().to_string();
        let listen_port = listener.local_addr().unwrap().port();

        // スレ機能有効: ホストレジストリを配線する。
        let registry = LivechatRegistry::new(128);
        let runtime = Arc::new(P2pRuntime::new_with_livechat(
            Arc::clone(&peers),
            Arc::clone(&security),
            Arc::clone(&hub),
            nonce,
            listen_port,
            true,
            Some(Arc::clone(&registry)),
        ));
        let (sd_tx, sd_rx) = watch::channel(false);
        let handles = runtime.spawn(vec![listener], sd_rx);

        let persona = Keys::generate();
        LivechatHostNode {
            hub,
            peers,
            registry,
            persona,
            listen_addr,
            shutdown: sd_tx,
            _handles: handles,
            _dir: dir,
        }
    }

    /// P2P 待受アドレス(視聴者の manual peer / スレ接続先 tip に使う)。
    pub fn listen_addr(&self) -> &str {
        &self.listen_addr
    }

    /// スレ主ペルソナ pubkey(= board_id)。
    pub fn board_id(&self) -> String {
        self.persona.public_key().to_hex()
    }

    /// 対象チャンネル参照(`30311:<board_id>:<guid>`)。
    pub fn channel(&self) -> String {
        format!("30311:{}:{GUID}", self.board_id())
    }

    /// スレを開設する(gen=1・tip=自ノード待受)。board_settings は既定 or 引数指定。
    pub fn open_thread(&self, title: &str, settings: BoardSettings) {
        self.registry
            .open_thread(
                self.persona.clone(),
                self.channel(),
                1,
                1_700_000_000,
                title,
                settings,
                self.listen_addr.clone(),
            )
            .expect("スレ開設");
    }

    /// 確定済みレスを 1 件積む(板鍵で署名した kind 1311 を confirm + ORDER 記録)。
    /// `board_key` は視聴者側で不要(閲覧は署名検証のみ)。戻り値は採番された res_no。
    pub fn seed_res(&self, board_key: &Keys, body: &str, created_at: u64) -> u16 {
        let event = sign_res(
            board_key,
            &self.board_id(),
            &self.channel(),
            1,
            body,
            created_at,
        )
        .expect("レス署名");
        self.registry
            .seed_confirmed_res(&self.board_id(), &event, created_at)
            .expect("レス seed")
    }

    /// 開設中の全スレの announce を gossip へローカル発行する(定期発行 1 回分に相当)。
    pub fn publish_announce(&self, created_at: u64) {
        for event in self.registry.build_announce_events(created_at, 0) {
            self.hub.publish_local(event);
        }
    }

    /// 視聴者ノードを gossip ピアとして手動登録する(相互接続用)。
    pub fn add_manual_peer(&self, addr: &str) {
        self.peers
            .add_peer(addr, PeerSource::Manual)
            .expect("手動ピア登録");
    }

    /// 共有ハブ(gossip 伝搬の観測用)。
    pub fn hub(&self) -> &Arc<GossipHub> {
        &self.hub
    }

    /// ホストレジストリ(US4 — BAN/PoW 等のドメイン層検証用アクセサ)。
    pub fn registry(&self) -> &Arc<LivechatRegistry> {
        &self.registry
    }

    /// 現在 established なスレ参加者を含む接続数 `(inbound, outbound)`。
    pub fn established_counts(&self) -> (usize, usize) {
        self.hub.established_counts()
    }
}

impl Drop for LivechatHostNode {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

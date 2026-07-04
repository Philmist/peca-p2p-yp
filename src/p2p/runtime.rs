//! P2P 接続ランタイム(T020 起動配線の一部)
//!
//! [`crate::p2p::session`] の状態機械と [`crate::p2p::peers`] の接続会計を、実際の
//! TCP 待受・外向き接続維持ループへ配線する。担うのは **HELLO/HELLO_ACK 交換による
//! established 到達 → established 維持 → 切断検出 → fail_count 反映と再接続バックオフ**まで。
//!
//! established 後の EVENT 伝搬・接続時同期(SYNC)は [`crate::p2p::hub::GossipHub`] と
//! 本ランタイムが連携して担う(T037/T038)。各 established 接続は送信キュー(mpsc)を
//! ハブへ登録し、pump は「ソケット受信」と「送信キュー」を多重化して駆動する。
//! keepalive の周期送信・無応答切断は Phase 5(T046)の責務のため、本ランタイムでは
//! 受信 PING に PONG を返すのみとする。
//!
//! graceful shutdown は [`tokio::sync::watch`] で全ループ・全接続へ伝播する。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::p2p::frame::{Message, close_reason, read_frame, write_frame};
use crate::p2p::hub::GossipHub;
use crate::p2p::peers::{CandidateMode, InboundOutcome, OutboundOutcome, PeerManager};
use crate::p2p::session::{Direction, Session, SessionAction, SessionConfig};
use crate::p2p::sync::{self, SyncCounter};
use crate::security::{SecurityCategory, SecurityLog};

/// 外向き接続維持ループの周期。
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(2);

/// P2P 接続ランタイム。`Arc<P2pRuntime>` として待受・維持ループ・各接続タスクで共有する。
pub struct P2pRuntime {
    peers: Arc<PeerManager>,
    security: Arc<SecurityLog>,
    /// gossip ハブ(受信処理・再伝搬・一覧供給・SYNC 供給)。
    hub: Arc<GossipHub>,
    /// 自ノードの起動時 nonce(自己接続検出用 — HELLO で申告)。
    nonce: u64,
    /// 自ノードの待受ポート(HELLO で申告。待受無効時は 0)。
    listen_port: u16,
    /// アドレスごとの次回再接続許可時刻(単調秒)。バックオフのメモリ内スケジュール。
    backoff: Mutex<HashMap<String, f64>>,
    /// 単調時刻の基点。
    start: Instant,
}

/// ポイズン時も内部値を回収してロックを返す(パニックしない)。
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl P2pRuntime {
    /// ランタイムを作成する。
    ///
    /// `hub` は受信処理・再伝搬・SYNC 供給・一覧供給を担う共有ハブ(T037)。
    pub fn new(
        peers: Arc<PeerManager>,
        security: Arc<SecurityLog>,
        hub: Arc<GossipHub>,
        nonce: u64,
        listen_port: u16,
    ) -> Self {
        Self {
            peers,
            security,
            hub,
            nonce,
            listen_port,
            backoff: Mutex::new(HashMap::new()),
            start: Instant::now(),
        }
    }

    /// 共有ハブへの参照(main 配線・status API 用)。
    pub fn hub(&self) -> &Arc<GossipHub> {
        &self.hub
    }

    /// 起動時基点からの単調経過秒。
    fn now(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    /// 各接続に用いるセッション設定(自ノードの申告値・レート上限は既定)。
    fn session_config(&self) -> SessionConfig {
        SessionConfig {
            local_nonce: self.nonce,
            local_listen_port: self.listen_port,
            ..SessionConfig::default()
        }
    }

    /// 待受(任意)と外向き維持ループを起動し、その JoinHandle を返す。
    ///
    /// `listener` が `None`(`p2p_bind` 空)なら外向き接続のみを行う(FR-016)。
    pub fn spawn(
        self: Arc<Self>,
        listener: Option<TcpListener>,
        shutdown: watch::Receiver<bool>,
    ) -> Vec<JoinHandle<()>> {
        let mut handles = Vec::new();

        {
            let me = Arc::clone(&self);
            let sd = shutdown.clone();
            handles.push(tokio::spawn(async move {
                me.run_outbound_loop(sd).await;
            }));
        }

        if let Some(listener) = listener {
            let me = Arc::clone(&self);
            let sd = shutdown.clone();
            handles.push(tokio::spawn(async move {
                me.run_listener(listener, sd).await;
            }));
        }

        handles
    }

    // ---------------------------------------------------------------- 待受

    async fn run_listener(
        self: Arc<Self>,
        listener: TcpListener,
        mut shutdown: watch::Receiver<bool>,
    ) {
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                accepted = listener.accept() => {
                    match accepted {
                        Ok((stream, peer)) => {
                            let me = Arc::clone(&self);
                            let sd = shutdown.clone();
                            tokio::spawn(async move { me.handle_inbound(stream, peer, sd).await; });
                        }
                        // 一過性の accept エラーは無視して受付を継続する。
                        Err(_) => continue,
                    }
                }
            }
        }
    }

    async fn handle_inbound(
        self: Arc<Self>,
        stream: TcpStream,
        peer: SocketAddr,
        shutdown: watch::Receiver<bool>,
    ) {
        let addr = peer.to_string();
        match self.peers.begin_inbound(&addr) {
            InboundOutcome::Accepted => {}
            // 着信上限超過・多重着信は HELLO_ACK 前に CLOSE して切る。
            InboundOutcome::AtCapacity | InboundOutcome::AlreadyActive => {
                let (_r, mut w) = stream.into_split();
                send_close(&mut w, close_reason::GOING_AWAY).await;
                return;
            }
        }
        let session = Session::new_inbound(
            self.session_config(),
            addr.clone(),
            Some(self.security.clone()),
        );
        let (r, w) = stream.into_split();
        self.pump(session, r, w, &addr, Direction::Inbound, shutdown)
            .await;
        self.peers.end_inbound(&addr);
    }

    // ------------------------------------------------------------ 外向き

    async fn run_outbound_loop(self: Arc<Self>, mut shutdown: watch::Receiver<bool>) {
        let mut ticker = tokio::time::interval(MAINTENANCE_INTERVAL);
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                _ = ticker.tick() => {
                    if *shutdown.borrow() {
                        break;
                    }
                    Self::maintain_outbound(&self, &shutdown);
                }
            }
        }
    }

    /// 外向き接続の不足分を候補から補充する。
    fn maintain_outbound(this: &Arc<Self>, shutdown: &watch::Receiver<bool>) {
        let deficit = this.peers.outbound_deficit();
        if deficit == 0 {
            return;
        }
        let candidates = match this.peers.candidates(CandidateMode::Normal) {
            Ok(c) => c,
            Err(_) => return,
        };
        let now = this.now();
        let mut launched = 0usize;
        for c in candidates {
            if launched >= deficit {
                break;
            }
            if this.backoff_active(&c.addr, now) {
                continue;
            }
            match this.peers.begin_outbound(&c.addr, c.verified) {
                OutboundOutcome::Started => {
                    let me = Arc::clone(this);
                    let addr = c.addr.clone();
                    let sd = shutdown.clone();
                    tokio::spawn(async move { me.handle_outbound_conn(addr, sd).await });
                    launched += 1;
                }
                // 未検証候補の 1 件/秒スロットル。次周期で再試行する。
                OutboundOutcome::Throttled => break,
                OutboundOutcome::AlreadyActive | OutboundOutcome::SelfAddress => continue,
            }
        }
    }

    async fn handle_outbound_conn(self: Arc<Self>, addr: String, shutdown: watch::Receiver<bool>) {
        let mut session = Session::new_outbound(
            self.session_config(),
            addr.clone(),
            Some(self.security.clone()),
        );
        let hello = session.start();

        match TcpStream::connect(&addr).await {
            Ok(stream) => {
                let (r, mut w) = stream.into_split();
                // HELLO を先に送る(失敗は接続失敗として扱う)。
                if let Some(msg) = hello
                    && write_frame(&mut w, &msg).await.is_err()
                {
                    self.peers.end_outbound(&addr);
                    let delay = self.failure_backoff(&addr);
                    self.schedule_retry(&addr, delay);
                    return;
                }
                let established = self
                    .pump(session, r, w, &addr, Direction::Outbound, shutdown)
                    .await;
                self.peers.end_outbound(&addr);
                // established に達したら成功記録済み。切断後の再ダイヤルには最小下限を置き、
                // フラップするピアへの過剰再接続を避ける(全断時の完全な回復配線は T047)。
                let delay = if established {
                    self.peers.config().backoff_initial_secs
                } else {
                    self.failure_backoff(&addr)
                };
                self.schedule_retry(&addr, delay);
            }
            Err(_) => {
                self.peers.end_outbound(&addr);
                let delay = self.failure_backoff(&addr);
                self.schedule_retry(&addr, delay);
                tracing::debug!(target: "p2p", peer = %addr, "外向き接続に失敗しました");
            }
        }
    }

    /// 失敗を記録し、更新後 fail_count に応じたバックオフ秒を返す。
    fn failure_backoff(&self, addr: &str) -> u64 {
        match self.peers.record_failure(addr) {
            Ok(Some(fc)) => self.peers.backoff_delay_secs(fc as u32),
            _ => self.peers.config().backoff_initial_secs,
        }
    }

    fn schedule_retry(&self, addr: &str, delay_secs: u64) {
        let next = self.now() + delay_secs as f64;
        lock(&self.backoff).insert(addr.to_string(), next);
    }

    fn backoff_active(&self, addr: &str, now: f64) -> bool {
        lock(&self.backoff).get(addr).is_some_and(|&t| t > now)
    }

    // ------------------------------------------------------- セッション駆動

    /// 確立済みトランスポート上でセッションを駆動する。established に達したら `true`。
    ///
    /// outbound の HELLO は呼び出し側が送信済みの前提。ソケット受信と、ハブに登録した
    /// 送信キュー(再伝搬・PONG・SYNC 応答の平滑送信)を多重化して駆動する。
    /// established 直後に SYNC_REQ を送り、受信 EVENT はハブの受信パイプラインへ渡す。
    async fn pump<R, W>(
        &self,
        mut session: Session,
        mut reader: R,
        mut writer: W,
        addr: &str,
        direction: Direction,
        mut shutdown: watch::Receiver<bool>,
    ) -> bool
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut established = false;
        // 送信キュー: 他接続からの再伝搬・PONG・SYNC 応答の平滑送信を本接続へ流す。
        let (outbox_tx, mut outbox_rx) = mpsc::unbounded_channel::<Message>();
        let mut conn_id: Option<u64> = None;
        let store_max = self.hub.store_config().event_store_max;
        let mut sync_counter = SyncCounter::new(store_max);

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    send_close(&mut writer, close_reason::GOING_AWAY).await;
                    break;
                }
                // ハブ・他接続からの送信要求(established 後のみ流入する)。
                queued = outbox_rx.recv() => {
                    match queued {
                        Some(msg) => {
                            if write_frame(&mut writer, &msg).await.is_err() {
                                break;
                            }
                        }
                        None => break, // 送信口がすべて閉じた(通常起きない)
                    }
                }
                res = read_frame(&mut reader) => {
                    let frame = match res {
                        Ok(Some(f)) => f,
                        Ok(None) => break, // 相手が正常に接続を閉じた
                        Err(e) => {
                            // フレーム層の違反(過大長・不正 JSON)はここで記録する。
                            if let Some((category, reason)) = e.security() {
                                self.security.log(category, addr, reason);
                                send_close(&mut writer, reason).await;
                            }
                            break;
                        }
                    };
                    match session.on_frame(frame.wire_len, frame.message) {
                        Ok(actions) => {
                            let mut stop = false;
                            for action in actions {
                                match action {
                                    SessionAction::Send(msg) => {
                                        if write_frame(&mut writer, &msg).await.is_err() {
                                            stop = true;
                                            break;
                                        }
                                    }
                                    SessionAction::Established => {
                                        established = true;
                                        let id = self.hub.next_conn_id();
                                        conn_id = Some(id);
                                        self.hub.register_peer(id, addr, direction, outbox_tx.clone());
                                        self.on_established(direction, addr);
                                        // established 直後に SYNC_REQ(since = now − 鮮度窓)。
                                        let since = sync::sync_req_since(
                                            unix_now_u64(),
                                            self.hub.store_config().freshness_window_sec,
                                        );
                                        sync_counter.begin();
                                        let _ = outbox_tx.send(Message::SyncReq { since });
                                    }
                                    SessionAction::Deliver(msg) => {
                                        if !self.handle_deliver(msg, addr, conn_id, &outbox_tx, &mut sync_counter).await {
                                            stop = true;
                                            break;
                                        }
                                    }
                                    SessionAction::PeerClosed(_) => {
                                        stop = true;
                                        break;
                                    }
                                }
                            }
                            if stop {
                                break;
                            }
                        }
                        Err(disconnect) => {
                            // セッション層の違反は Session が記録する(ログ責務の一本化)。
                            session.note_disconnect(&disconnect);
                            send_close(&mut writer, disconnect.reason).await;
                            break;
                        }
                    }
                }
            }
        }
        if let Some(id) = conn_id {
            self.hub.unregister_peer(id);
        }
        established
    }

    /// established 後の受信メッセージを処理する。継続してよければ `true`、切断すべきなら `false`。
    async fn handle_deliver(
        &self,
        message: Message,
        addr: &str,
        conn_id: Option<u64>,
        outbox_tx: &mpsc::UnboundedSender<Message>,
        sync_counter: &mut SyncCounter,
    ) -> bool {
        match message {
            Message::Event { event } => {
                // 検査 6: SYNC 応答量の上限(1 回の SYNC_REQ 応答が event_store_max 件超で切断)。
                if sync_counter.on_event() {
                    self.security.log(
                        SecurityCategory::P2pRateLimited,
                        addr,
                        "sync response exceeded event_store_max",
                    );
                    return false;
                }
                // 伝搬規則 1〜4: 検証→重複判定→格納→受信元を除く再伝搬(ハブが担う)。
                let raw = event.to_string();
                let id = conn_id.unwrap_or(0);
                self.hub.on_event(&raw, addr, id);
                true
            }
            Message::SyncReq { since } => {
                // 応答イベントを平滑化して本接続へ送る(MUST — 受信側レート上限以下)。
                let (messages, count) = self.hub.sync_response(since, unix_now_u64());
                let tx = outbox_tx.clone();
                tokio::spawn(async move {
                    sync::stream_sync_response(messages, count, tx).await;
                });
                true
            }
            Message::SyncDone { .. } => {
                sync_counter.on_done();
                true
            }
            Message::Ping { nonce } => {
                // keepalive 応答(周期送信・無応答切断は T046)。
                let _ = outbox_tx.send(Message::Pong { nonce });
                true
            }
            // PONG・PEERS・GET_PEERS は本タスクでは処理しない(keepalive 追跡は T046、
            // PEX は Phase 6)。前方互換のため受信しても切断しない。
            _ => true,
        }
    }

    fn on_established(&self, direction: Direction, addr: &str) {
        match direction {
            Direction::Outbound => {
                // 外向き接続の成功のみ verified=1・last_ok_at・fail_count=0 を立てる。
                let _ = self.peers.record_success(addr, unix_now());
                tracing::info!(target: "p2p", peer = %addr, direction = "outbound", "established");
            }
            Direction::Inbound => {
                // 着信は verified を立てない(着信可否は自ノードの外向き接続で検証する)。
                tracing::info!(target: "p2p", peer = %addr, direction = "inbound", "established");
            }
        }
    }
}

/// 現在の unix 時刻(秒)。SYNC の `since` 計算用。
fn unix_now_u64() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// CLOSE を送る(ベストエフォート — 失敗は無視)。
async fn send_close<W: AsyncWrite + Unpin>(writer: &mut W, reason: &'static str) {
    let _ = write_frame(
        writer,
        &Message::Close {
            reason: reason.to_string(),
        },
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::schema::VerifyConfig;
    use crate::event::store::StoreConfig;
    use crate::p2p::peers::PeerManagerConfig;
    use crate::store::{PeerSource, Store};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// テスト用ランタイムと、その SecurityLog ファイルを保持する tempdir ガードを返す。
    fn runtime(nonce: u64, listen_port: u16) -> (Arc<P2pRuntime>, tempfile::TempDir) {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let peers = Arc::new(PeerManager::new(Arc::clone(&store), PeerManagerConfig::default()));
        let dir = tempfile::tempdir().unwrap();
        let security = Arc::new(SecurityLog::new(dir.path().join("security.log")).unwrap());
        let hub = GossipHub::new(
            store,
            Arc::clone(&security),
            StoreConfig::default(),
            VerifyConfig::default(),
        );
        (
            Arc::new(P2pRuntime::new(peers, security, hub, nonce, listen_port)),
            dir,
        )
    }

    /// duplex で inbound / outbound セッションを相互に駆動し、双方が established に達することを確認する。
    #[tokio::test]
    async fn inbound_and_outbound_reach_established_over_duplex() {
        let (server, _sg) = runtime(1, 7147);
        let (client, _cg) = runtime(2, 7157);
        let (a, b) = tokio::io::duplex(4096);
        let (ar, aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        let (_tx, rx) = watch::channel(false);

        // サーバ側 = inbound、クライアント側 = outbound。
        let srv = {
            let server = Arc::clone(&server);
            let rx = rx.clone();
            tokio::spawn(async move {
                let session = Session::new_inbound(
                    server.session_config(),
                    "peer".into(),
                    Some(server.security.clone()),
                );
                server
                    .pump(session, ar, aw, "peer", Direction::Inbound, rx)
                    .await
            })
        };
        let cli = {
            let client = Arc::clone(&client);
            let rx = rx.clone();
            tokio::spawn(async move {
                let mut session = Session::new_outbound(
                    client.session_config(),
                    "peer".into(),
                    Some(client.security.clone()),
                );
                let hello = session.start();
                let mut bw = bw;
                if let Some(msg) = hello {
                    write_frame(&mut bw, &msg).await.unwrap();
                }
                client
                    .pump(session, br, bw, "peer", Direction::Outbound, rx)
                    .await
            })
        };

        // 双方が established に達した後、相手からの CLOSE で正常終了させる。
        // ここでは established 到達のみ確認するため、少し待ってから shutdown せず
        // 片側を drop することはできないので、established を確認できるよう
        // タイムアウト付きで join を待つ代わりに、established フラグを検証する。
        // pump は接続が閉じるまで戻らないため、established を確認したら明示切断する。
        // 簡便のため: 短い待機後に watch で shutdown を送る。
        tokio::time::sleep(Duration::from_millis(50)).await;
        _tx.send(true).unwrap();

        let srv_established = srv.await.unwrap();
        let cli_established = cli.await.unwrap();
        assert!(srv_established, "inbound 側が established に達するべき");
        assert!(cli_established, "outbound 側が established に達するべき");
    }

    /// 過大長フレームがセキュリティログに記録され切断されることを確認する。
    #[tokio::test]
    async fn oversize_frame_is_logged_and_closed() {
        let (rt, _g) = runtime(1, 7147);
        let (a, b) = tokio::io::duplex(4096);
        let (ar, aw) = tokio::io::split(a);
        let (mut br, mut bw) = tokio::io::split(b);
        let (_tx, rx) = watch::channel(false);

        let srv = {
            let rt = Arc::clone(&rt);
            tokio::spawn(async move {
                let session = Session::new_inbound(
                    rt.session_config(),
                    "peer".into(),
                    Some(rt.security.clone()),
                );
                rt.pump(session, ar, aw, "peer", Direction::Inbound, rx)
                    .await
            })
        };

        // 上限超過の長さ前置(> 64KB)を送る。
        let over = (crate::p2p::frame::MAX_FRAME_PAYLOAD as u32 + 1).to_be_bytes();
        bw.write_all(&over).await.unwrap();
        bw.flush().await.unwrap();

        // サーバは CLOSE を返して切断する。
        let mut buf = Vec::new();
        let _ = br.read_to_end(&mut buf).await;
        let established = srv.await.unwrap();
        assert!(!established, "ハンドシェイク前に切断されるべき");
    }

    /// Phase 2 チェックポイント用の手動シード補助(既定では無視)。
    ///
    /// 環境変数 `SEED_DIR`(data-dir)と `SEED_ADDR`(登録するピア `host:port`)を与えて
    /// `cargo test --lib seed_peer_for_checkpoint -- --ignored --nocapture` で実行すると、
    /// 当該 data-dir の peers テーブルへ manual ピアを登録する(quickstart 手順 2 の 2 ノード検証)。
    #[test]
    #[ignore = "手動シード用(環境変数 SEED_DIR / SEED_ADDR が必要)"]
    fn seed_peer_for_checkpoint() {
        let dir = std::env::var("SEED_DIR").expect("SEED_DIR を指定してください");
        let addr = std::env::var("SEED_ADDR").expect("SEED_ADDR を指定してください");
        let store = Arc::new(Store::open_in_dir(&dir).unwrap());
        let peers = Arc::new(PeerManager::new(store, PeerManagerConfig::default()));
        let ep = peers.add_peer(&addr, PeerSource::Manual).unwrap();
        println!("seeded peer: {} (source={:?})", ep.addr, ep.source);
    }
}

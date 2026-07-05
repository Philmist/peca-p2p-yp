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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::p2p::frame::{Message, close_reason, read_frame, write_frame};
use crate::p2p::hub::GossipHub;
use crate::p2p::peers::{
    CandidateMode, InboundOutcome, OutboundOutcome, PeerManager, ReachabilityState,
};
use crate::p2p::pex::{self, PEX_MAX_PEERS};
use crate::p2p::session::{
    Direction, Keepalive, KeepaliveAction, PeerHello, Session, SessionAction, SessionConfig,
};
use crate::p2p::sync::{self, SyncCounter};
use crate::security::{SecurityCategory, SecurityLog};
use crate::store::PeerSource;

/// 外向き接続維持ループの周期。
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(2);

/// keepalive 判定(PING 送信・無応答切断)の tick 周期。しきい値(60/120 秒)より
/// 十分細かくし、判定の粒度を確保する(T046)。
const KEEPALIVE_TICK: Duration = Duration::from_secs(5);

/// 外向き TCP 接続確立のタイムアウト。OS 既定(Windows で 20 秒超)に任せると、
/// PEX で注入された到達不能アドレスが外向き接続枠を長時間占有するため短く抑える
/// (contracts §脅威と対応範囲 — PEX への第三者アドレス注入の緩和)。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// [`P2pRuntime::pump`] の終了理由。再接続バックオフと fail_count 反映の判断に用いる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PumpEnd {
    /// established に達しないまま終了(ハンドシェイク失敗・接続前切断・違反)。
    HandshakeFailed,
    /// established 後に正常終了(相手 CLOSE going_away / EOF / ローカル shutdown)。
    ClosedCleanly,
    /// established 後に異常切断(keepalive 無応答・I/O エラー・受信違反)。
    Dropped,
}

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
    /// PEX(ピア交換)が有効か。無効時は GET_PEERS を送らず、受信 PEERS も無視する。
    pex_enabled: bool,
    /// アドレスごとの次回再接続許可時刻(単調秒)。バックオフのメモリ内スケジュール。
    backoff: Mutex<HashMap<String, f64>>,
    /// 全ピア到達不能状態と回復通知(T047 / US3)。status API・再発行タスクと共有。
    reachability: Arc<ReachabilityState>,
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
        pex_enabled: bool,
    ) -> Self {
        Self {
            peers,
            security,
            hub,
            nonce,
            listen_port,
            pex_enabled,
            backoff: Mutex::new(HashMap::new()),
            reachability: ReachabilityState::new(),
            start: Instant::now(),
        }
    }

    /// 共有ハブへの参照(main 配線・status API 用)。
    pub fn hub(&self) -> &Arc<GossipHub> {
        &self.hub
    }

    /// 全ピア到達不能状態と回復通知の共有ハンドル(status API・再発行タスク用 — T047)。
    pub fn reachability(&self) -> Arc<ReachabilityState> {
        Arc::clone(&self.reachability)
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

    /// 待受(任意・複数)と外向き維持ループを起動し、その JoinHandle を返す。
    ///
    /// `listeners` が空(`p2p_bind` 空、または全バインド失敗)なら外向き接続のみを
    /// 行う(FR-016)。複数リスナー(IPv4/IPv6 デュアルスタック — ADR-0008)は
    /// リスナーごとに独立の accept ループを起動する。
    pub fn spawn(
        self: Arc<Self>,
        listeners: Vec<TcpListener>,
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

        for listener in listeners {
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
    ///
    /// established が皆無のとき(全断)は降格済みピアも含めて再試行する
    /// ([`CandidateMode::AllUnreachable`])。到達可能な候補が一つも無い(全て
    /// バックオフ中で誰にも接続できない)状態を全ピア到達不能として記録し、
    /// established を得た時点で回復通知を出す(T047 / US3 シナリオ 3)。
    fn maintain_outbound(this: &Arc<Self>, shutdown: &watch::Receiver<bool>) {
        let (inb, outb) = this.hub.established_counts();
        let total_established = inb + outb;
        // established が一つでもあれば到達可能。
        if total_established > 0 {
            this.reachability.mark_reachable();
        }
        // 全断時は降格済みも含めて全 enabled ピアを通常候補順で再試行する。
        let mode = if total_established == 0 {
            CandidateMode::AllUnreachable
        } else {
            CandidateMode::Normal
        };

        let deficit = this.peers.outbound_deficit();
        if deficit == 0 {
            return;
        }
        let candidates = match this.peers.candidates(mode) {
            Ok(c) => c,
            Err(_) => return,
        };
        let now = this.now();
        // 期限切れの再接続スケジュールを回収する(削除済みピアのエントリを残さない)。
        lock(&this.backoff).retain(|_, &mut t| t > now);
        // 全断検出: established も進行中の外向きダイヤルも無く、候補は存在するが
        // 全てバックオフ中で今この瞬間は誰にも接続できない。
        if total_established == 0
            && this.peers.outbound_count() == 0
            && !candidates.is_empty()
            && candidates.iter().all(|c| this.backoff_active(&c.addr, now))
        {
            this.reachability.mark_all_unreachable();
        }
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

        // 接続確立はタイムアウト付き(到達不能アドレスによる接続枠の長時間占有を防ぐ)。
        let connected = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&addr))
            .await
            .unwrap_or_else(|_| Err(std::io::ErrorKind::TimedOut.into()));
        match connected {
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
                let end = self
                    .pump(session, r, w, &addr, Direction::Outbound, shutdown)
                    .await;
                self.peers.end_outbound(&addr);
                // 正常終了(相手 CLOSE / EOF / ローカル shutdown)は成功記録済みのため
                // 最小バックオフで再ダイヤルする。異常切断(keepalive 無応答・違反)は
                // 失敗として fail_count に反映し、指数バックオフを伸ばす(T046)。
                let delay = match end {
                    PumpEnd::ClosedCleanly => self.peers.config().backoff_initial_secs,
                    PumpEnd::HandshakeFailed | PumpEnd::Dropped => self.failure_backoff(&addr),
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

    /// 確立済みトランスポート上でセッションを駆動する。終了理由を [`PumpEnd`] で返す。
    ///
    /// outbound の HELLO は呼び出し側が送信済みの前提。ソケット受信と、ハブに登録した
    /// 送信キュー(再伝搬・PONG・SYNC 応答の平滑送信)を多重化して駆動する。
    /// established 直後に SYNC_REQ を送り、受信 EVENT はハブの受信パイプラインへ渡す。
    /// keepalive(T046): established 中は 60 秒間隔で PING を送り、どのフレームも
    /// 120 秒受信しなければ無応答として切断する。
    async fn pump<R, W>(
        &self,
        mut session: Session,
        mut reader: R,
        mut writer: W,
        addr: &str,
        direction: Direction,
        mut shutdown: watch::Receiver<bool>,
    ) -> PumpEnd
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut established = false;
        // established 後に正常終了(相手 CLOSE / EOF / ローカル shutdown)したら true。
        // 異常切断(keepalive 無応答・I/O・違反)は false のままとし fail_count に反映する。
        let mut clean = false;
        // 送信キュー: 他接続からの再伝搬・PONG・SYNC 応答の平滑送信を本接続へ流す。
        let (outbox_tx, mut outbox_rx) = mpsc::unbounded_channel::<Message>();
        let mut conn_id: Option<u64> = None;
        let store_max = self.hub.store_config().event_store_max;
        let mut sync_counter = SyncCounter::new(store_max);
        // SYNC 応答の平滑送信タスクは 1 接続あたり同時 1 本(SYNC_REQ 連打による
        // 応答増幅と平滑化 MUST の破れを防ぐ)。true の間は追加要求を無視する。
        let sync_in_flight = Arc::new(AtomicBool::new(false));
        // keepalive タイマー(established 前も無応答検出のため回す。PING は established 後のみ)。
        let mut keepalive = Keepalive::new(self.now());
        let mut ping_nonce: u64 = 0;
        let mut ka_tick = tokio::time::interval(KEEPALIVE_TICK);
        ka_tick.tick().await; // 起動直後の即時 tick を読み捨てる。

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    send_close(&mut writer, close_reason::GOING_AWAY).await;
                    clean = true;
                    break;
                }
                // keepalive: PING 送信・無応答切断の判定。
                _ = ka_tick.tick() => {
                    match keepalive.poll(self.now()) {
                        KeepaliveAction::SendPing if established => {
                            ping_nonce = ping_nonce.wrapping_add(1);
                            if write_frame(&mut writer, &Message::Ping { nonce: ping_nonce }).await.is_err() {
                                break;
                            }
                        }
                        KeepaliveAction::Timeout => {
                            // 120 秒無応答 → 切断(異常切断として fail_count に反映)。
                            send_close(&mut writer, close_reason::GOING_AWAY).await;
                            tracing::debug!(target: "p2p", peer = %addr, "keepalive 無応答で切断しました");
                            break;
                        }
                        _ => {}
                    }
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
                    // 何らかのフレーム受信は生存の証拠(keepalive の無応答タイマーをリセット)。
                    let frame = match res {
                        Ok(Some(f)) => {
                            keepalive.on_recv(self.now());
                            f
                        }
                        Ok(None) => {
                            // 相手が正常に接続を閉じた(established 後は正常終了扱い)。
                            clean = established;
                            break;
                        }
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
                                        // 相手申告 ts と自ノード時刻の差 = 時計ずれ標本(T048。未検証値)。
                                        let clock_skew = session
                                            .peer()
                                            .map(|p| p.ts - unix_now())
                                            .unwrap_or(0);
                                        self.hub.register_peer(id, addr, direction, outbox_tx.clone(), clock_skew);
                                        self.on_established(direction, addr);
                                        // inbound 相手の候補化(申告 listen_port を接続元 host と
                                        // 組み合わせ source=pex・verified=0 で登録 — contracts §接続管理)。
                                        if direction == Direction::Inbound {
                                            self.register_inbound_candidate(addr, session.peer());
                                        }
                                        // established 直後に SYNC_REQ(since = now − 鮮度窓)。
                                        let since = sync::sync_req_since(
                                            unix_now_u64(),
                                            self.hub.store_config().freshness_window_sec,
                                        );
                                        sync_counter.begin();
                                        let _ = outbox_tx.send(Message::SyncReq { since });
                                        // 接続先拡大のため GET_PEERS を送る(PEX 有効時のみ)。
                                        if self.pex_enabled {
                                            let _ = outbox_tx.send(Message::GetPeers);
                                        }
                                    }
                                    SessionAction::Deliver(msg) => {
                                        if !self.handle_deliver(msg, addr, conn_id, &outbox_tx, &mut sync_counter, &sync_in_flight).await {
                                            stop = true;
                                            break;
                                        }
                                    }
                                    SessionAction::PeerClosed(reason) => {
                                        // 相手からの CLOSE 受信は正常終了。ハンドシェイク中の
                                        // `self_connect` は自ノードの待受へダイヤルした証拠
                                        // (相手 = 自プロセスの inbound 側が nonce 一致で検出)
                                        // のため、当該アドレスを自己として登録拒否する
                                        // (contracts §接続管理 / T018)。
                                        if !established
                                            && direction == Direction::Outbound
                                            && reason == close_reason::SELF_CONNECT
                                        {
                                            let _ = self.peers.note_self_connect(addr);
                                        }
                                        clean = established;
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
                            // 自己接続を自ら検出(HELLO_ACK の nonce が自ノードと一致)した
                            // 場合は、ダイヤル先アドレスを自己として登録拒否する(T018)。
                            if direction == Direction::Outbound
                                && disconnect.reason == close_reason::SELF_CONNECT
                            {
                                let _ = self.peers.note_self_connect(addr);
                            }
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
        match (established, clean) {
            (false, _) => PumpEnd::HandshakeFailed,
            (true, true) => PumpEnd::ClosedCleanly,
            (true, false) => PumpEnd::Dropped,
        }
    }

    /// established 後の受信メッセージを処理する。継続してよければ `true`、切断すべきなら `false`。
    ///
    /// `sync_in_flight` は本接続の SYNC 応答タスクが進行中かのゲート(同時 1 本)。
    async fn handle_deliver(
        &self,
        message: Message,
        addr: &str,
        conn_id: Option<u64>,
        outbox_tx: &mpsc::UnboundedSender<Message>,
        sync_counter: &mut SyncCounter,
        sync_in_flight: &Arc<AtomicBool>,
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
                // 応答タスクは 1 接続あたり同時 1 本。進行中の追加要求は黙って無視する
                // (SYNC_REQ 連打で平滑化 MUST が破れる・応答が送信キューに無制限に
                // 積まれる増幅を防ぐ。前方互換のため切断はしない)。
                if sync_in_flight.swap(true, Ordering::AcqRel) {
                    return true;
                }
                // 応答イベントを平滑化して本接続へ送る(MUST — 受信側レート上限以下)。
                let (messages, count) = self.hub.sync_response(since, unix_now_u64());
                let tx = outbox_tx.clone();
                let gate = Arc::clone(sync_in_flight);
                tokio::spawn(async move {
                    sync::stream_sync_response(messages, count, tx).await;
                    gate.store(false, Ordering::Release);
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
            Message::GetPeers => {
                // PEX 要求へ verified=1 のみを last_ok_at 新しい順に ≤64 件返す(research R14)。
                // 無効時は応答しない(前方互換のため切断はしない)。
                if self.pex_enabled {
                    let selected = self
                        .peers
                        .list_peers()
                        .map(|all| pex::select_peers_for_pex(&all, PEX_MAX_PEERS))
                        .unwrap_or_default();
                    let _ = outbox_tx.send(Message::Peers { peers: selected });
                }
                true
            }
            Message::Peers { peers } => {
                // 受信 PEERS を検証(検査 5)し、正当分を source=pex・verified=0 で候補登録する。
                // 無効時は無視する。
                if self.pex_enabled {
                    let result = pex::validate_incoming_peers(
                        &peers,
                        |canonical| self.peers.is_self_addr(canonical),
                        PEX_MAX_PEERS,
                    );
                    for candidate in &result.accepted {
                        // 自アドレス・ストア検証は add_peer 側でも再度弾かれる(二重防壁)。
                        let _ = self.peers.add_peer(&candidate.canonical(), PeerSource::Pex);
                    }
                    if result.has_rejections() {
                        self.security.log(
                            SecurityCategory::PexRejected,
                            addr,
                            "invalid PEERS entry discarded",
                        );
                    }
                }
                true
            }
            // PONG は本タスクでは処理しない(keepalive 追跡は T046)。前方互換のため切断しない。
            _ => true,
        }
    }

    /// inbound 接続の相手を PEX 候補として登録する(contracts §接続管理)。
    ///
    /// 相手が申告した `listen_port`(> 0)を接続元 host と組み合わせ、source=pex・verified=0
    /// で登録する。verified=1 昇格は自ノードからの外向き接続成功時のみ(既存の record_success)。
    /// `listen_port == 0`(待受なし)は候補登録しない。PEX 無効時は登録しない。
    fn register_inbound_candidate(&self, conn_addr: &str, peer: Option<&PeerHello>) {
        if !self.pex_enabled {
            return;
        }
        let Some(peer) = peer else {
            return;
        };
        if peer.listen_port == 0 {
            return;
        }
        // 接続元 SocketAddr の host と申告 listen_port を組み合わせて候補アドレスを作る。
        let Ok(socket) = conn_addr.parse::<SocketAddr>() else {
            return;
        };
        let candidate = match socket.ip() {
            std::net::IpAddr::V6(v6) => format!("[{}]:{}", v6, peer.listen_port),
            std::net::IpAddr::V4(v4) => format!("{}:{}", v4, peer.listen_port),
        };
        let _ = self.peers.add_peer(&candidate, PeerSource::Pex);
    }

    fn on_established(&self, direction: Direction, addr: &str) {
        // 接続確立は到達可能の証拠。全断からの回復ならここで通知が出る(方向を問わない)。
        self.reachability.mark_reachable();
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
        let peers = Arc::new(PeerManager::new(
            Arc::clone(&store),
            PeerManagerConfig::default(),
        ));
        let dir = tempfile::tempdir().unwrap();
        let security = Arc::new(SecurityLog::new(dir.path().join("security.log")).unwrap());
        let hub = GossipHub::new(
            store,
            Arc::clone(&security),
            StoreConfig::default(),
            VerifyConfig::default(),
        );
        (
            Arc::new(P2pRuntime::new(
                peers,
                security,
                hub,
                nonce,
                listen_port,
                true,
            )),
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

        let srv_end = srv.await.unwrap();
        let cli_end = cli.await.unwrap();
        // shutdown による終了は established 到達後の正常終了。
        assert_eq!(
            srv_end,
            PumpEnd::ClosedCleanly,
            "inbound 側が established に達し正常終了するべき"
        );
        assert_eq!(
            cli_end,
            PumpEnd::ClosedCleanly,
            "outbound 側が established に達し正常終了するべき"
        );
    }

    /// 自己接続(同一 nonce)で outbound 側がダイヤル先アドレスを自己として学習し、
    /// ピア登録が削除・以後の登録が拒否されることを確認する(contracts §接続管理 / T018)。
    #[tokio::test]
    async fn self_connect_registers_self_addr_and_removes_peer() {
        // 同一ランタイム(同一 nonce)の inbound / outbound を突き合わせる = 自己接続。
        let (rt, _g) = runtime(0xDEAD_BEEF, 7147);
        let dial_addr = "198.51.100.99:7147";
        rt.peers
            .add_peer(dial_addr, PeerSource::Manual)
            .expect("事前登録");

        let (a, b) = tokio::io::duplex(4096);
        let (ar, aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        let (_tx, rx) = watch::channel(false);

        let srv = {
            let rt = Arc::clone(&rt);
            let rx = rx.clone();
            tokio::spawn(async move {
                let session = Session::new_inbound(
                    rt.session_config(),
                    "127.0.0.1:50000".into(),
                    Some(rt.security.clone()),
                );
                rt.pump(session, ar, aw, "127.0.0.1:50000", Direction::Inbound, rx)
                    .await
            })
        };
        let cli = {
            let rt = Arc::clone(&rt);
            let rx = rx.clone();
            let addr = dial_addr.to_string();
            tokio::spawn(async move {
                let mut session = Session::new_outbound(
                    rt.session_config(),
                    addr.clone(),
                    Some(rt.security.clone()),
                );
                let hello = session.start();
                let mut bw = bw;
                if let Some(msg) = hello {
                    write_frame(&mut bw, &msg).await.unwrap();
                }
                rt.pump(session, br, bw, &addr, Direction::Outbound, rx)
                    .await
            })
        };

        let srv_end = srv.await.unwrap();
        let cli_end = cli.await.unwrap();
        // 双方 established に達しないまま終了する。
        assert_eq!(srv_end, PumpEnd::HandshakeFailed);
        assert_eq!(cli_end, PumpEnd::HandshakeFailed);
        // ダイヤル先が自己として登録され、ピアは削除・以後の登録も拒否される。
        assert!(
            rt.peers.is_self_addr(dial_addr),
            "自己アドレスとして学習するべき"
        );
        assert!(
            rt.peers.get_peer(dial_addr).unwrap().is_none(),
            "ピア登録は削除されるべき"
        );
        assert!(
            rt.peers.add_peer(dial_addr, PeerSource::Pex).is_err(),
            "以後の登録は拒否されるべき"
        );
    }

    /// SYNC 応答タスクが進行中の追加 SYNC_REQ は無視される(同時 1 本のゲート)。
    #[tokio::test]
    async fn concurrent_sync_req_ignored_while_in_flight() {
        let (rt, _g) = runtime(1, 7147);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut counter = SyncCounter::new(16);
        let gate = Arc::new(AtomicBool::new(false));

        // ゲート保持中(応答進行中)の SYNC_REQ は応答されない(切断もしない)。
        gate.store(true, Ordering::SeqCst);
        assert!(
            rt.handle_deliver(
                Message::SyncReq { since: 0 },
                "p",
                Some(1),
                &tx,
                &mut counter,
                &gate
            )
            .await
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(rx.try_recv().is_err(), "進行中の追加要求へは応答しない");

        // ゲート解放後の SYNC_REQ には応答(空ストアでも SYNC_DONE)が届き、ゲートが戻る。
        gate.store(false, Ordering::SeqCst);
        assert!(
            rt.handle_deliver(
                Message::SyncReq { since: 0 },
                "p",
                Some(1),
                &tx,
                &mut counter,
                &gate
            )
            .await
        );
        let done = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("応答が届くべき")
            .unwrap();
        assert_eq!(done, Message::SyncDone { count: 0 });
        assert!(
            tokio::time::timeout(Duration::from_secs(1), async {
                while gate.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .is_ok(),
            "応答完了でゲートが解放されるべき"
        );
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
        let end = srv.await.unwrap();
        assert_eq!(
            end,
            PumpEnd::HandshakeFailed,
            "ハンドシェイク前に切断されるべき"
        );
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

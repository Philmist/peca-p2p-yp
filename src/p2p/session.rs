//! gossip セッション状態機械と受信レート制限(T017)
//!
//! contracts/p2p-gossip.md §セッション状態機械・受信検証パイプライン検査 2 の正実装。
//!
//! 本モジュールはトランスポート非依存の**状態遷移・ハンドシェイク・受信レート制限**のみを担う。
//! established 後の EVENT/SYNC_REQ/PEERS/PING 等は解釈せず、上位レイヤへそのまま委譲する
//! ([`SessionAction::Deliver`])。以下は本タスクの責務ではない:
//! イベント検証・重複抑制・伝搬(T037)、PEERS 内容検証(T018 検査 5)、
//! SYNC 応答量検査(T038 検査 6)、keepalive の周期送信・無応答切断(T046)。
//!
//! ## セキュリティイベント記録の契約
//! 全てのセキュリティイベントは Session を通じて記録する。[`Session::on_frame`] が返す
//! [`Disconnect`] と、フレーム層エラー([`crate::p2p::frame::FrameError`])のいずれも、
//! 呼び出し側(T037 の受信ループ)が [`Session::note_disconnect`] を呼んで記録する。
//! `on_frame` は記録しない(記録責務を呼び出し側に一本化し、二重記録を避ける)。

use std::sync::Arc;
use std::time::Instant;

use crate::p2p::frame::{Hello, Message, close_reason};
use crate::security::{SecurityCategory, SecurityLog};

/// プロトコルバージョン(v1)。互換判定は完全一致。
pub const PROTOCOL_VERSION: u32 = 1;

/// 受信レート上限(検査 2): 1 ピアあたり 256KB/秒。
pub const DEFAULT_MAX_BYTES_PER_SEC: usize = 256 * 1024;
/// 受信レート上限(検査 2): 1 ピアあたり 200 メッセージ/秒。
pub const DEFAULT_MAX_MSGS_PER_SEC: usize = 200;

/// 接続方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// 自ノードから接続(HELLO を送り HELLO_ACK を待つ)。
    Outbound,
    /// 相手からの着信(HELLO を待ち HELLO_ACK を返す)。
    Inbound,
}

/// セッション状態(contracts/p2p-gossip.md §セッション状態機械 / data-model §PeerSession)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// 接続直後。outbound は [`Session::start`] 前、inbound は HELLO 待ち。
    Connecting,
    /// outbound が HELLO 送信済み・HELLO_ACK 待ち。
    HelloSent,
    /// inbound が HELLO 受信済み(HELLO_ACK 送信直前の一過状態)。
    HelloReceived,
    /// ハンドシェイク完了。
    Established,
    /// 切断済み。
    Closed,
}

/// 自ノードのハンドシェイク設定。
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// 自ノードのプロトコルバージョン(= [`PROTOCOL_VERSION`])。
    pub local_version: u32,
    /// 起動時生成の乱数(自己接続検出用)。外部から与える。
    pub local_nonce: u64,
    /// 自ノードの待受ポート(待受なしは 0)。
    pub local_listen_port: u16,
    /// 自ノードの機能フラグ(v1 は空)。
    pub local_features: Vec<String>,
    /// 受信バイトレート上限(バイト/秒)。
    pub max_bytes_per_sec: usize,
    /// 受信メッセージレート上限(メッセージ/秒)。
    pub max_msgs_per_sec: usize,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            local_version: PROTOCOL_VERSION,
            local_nonce: 0,
            local_listen_port: 0,
            local_features: Vec::new(),
            max_bytes_per_sec: DEFAULT_MAX_BYTES_PER_SEC,
            max_msgs_per_sec: DEFAULT_MAX_MSGS_PER_SEC,
        }
    }
}

/// 相手が HELLO / HELLO_ACK で申告した情報(すべて未検証 — Principle II)。
///
/// `ts` は時計ずれ自己診断にのみ用いる(T048)。接続可否・イベント検証には使わない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerHello {
    /// 相手のプロトコルバージョン。
    pub version: u32,
    /// 相手の待受ポート(0 = 待受なし)。
    pub listen_port: u16,
    /// 相手の機能フラグ。v1 は解釈しない(未知値は無視 — MUST)。
    pub features: Vec<String>,
    /// 相手の nonce(自己接続検出用)。
    pub nonce: u64,
    /// 相手の時刻申告(unix 秒、未検証)。
    pub ts: i64,
}

/// [`Session::on_frame`] が上位へ返す動作。
#[derive(Debug, Clone, PartialEq)]
pub enum SessionAction {
    /// 相手へ送るべきフレーム(HELLO_ACK 等)。
    Send(Message),
    /// established 到達通知(接続時同期 SYNC の開始契機 — T038)。
    Established,
    /// established 後に受信し、上位レイヤ(T037/T018/T038)へ委譲する受信メッセージ。
    Deliver(Message),
    /// 相手からの CLOSE 受信(定型 reason コード)。
    PeerClosed(String),
}

/// 切断を伴う条件。呼び出し側は `reason` で CLOSE を送って切断する。
///
/// `category` が `Some` のときはセキュリティイベントとして記録すべき違反
/// ([`Session::note_disconnect`])。`None` は通常切断(バージョン非互換・自己接続)。
#[derive(Debug, Clone, PartialEq)]
pub struct Disconnect {
    /// CLOSE で送る定型 reason コード([`close_reason`])。
    pub reason: &'static str,
    /// 記録すべきセキュリティカテゴリ(通常切断は `None`)。
    pub category: Option<SecurityCategory>,
}

impl Disconnect {
    fn security(category: SecurityCategory, reason: &'static str) -> Self {
        Self {
            reason,
            category: Some(category),
        }
    }

    fn benign(reason: &'static str) -> Self {
        Self {
            reason,
            category: None,
        }
    }
}

/// 固定 1 秒窓の受信レート制限(検査 2)。
///
/// クロックは秒単位の単調時刻(`f64`)を注入でき、テスト可能。窓を跨ぐと計数をリセットする。
struct RateLimiter {
    window_start: f64,
    bytes: usize,
    msgs: usize,
    max_bytes_per_sec: usize,
    max_msgs_per_sec: usize,
    initialized: bool,
}

impl RateLimiter {
    fn new(max_bytes_per_sec: usize, max_msgs_per_sec: usize) -> Self {
        Self {
            window_start: 0.0,
            bytes: 0,
            msgs: 0,
            max_bytes_per_sec,
            max_msgs_per_sec,
            initialized: false,
        }
    }

    /// 1 フレーム受信を計上する。上限超過なら記録すべきカテゴリを返す。
    fn charge(&mut self, now: f64, frame_bytes: usize) -> Result<(), Disconnect> {
        if !self.initialized || now - self.window_start >= 1.0 {
            self.window_start = now;
            self.bytes = 0;
            self.msgs = 0;
            self.initialized = true;
        }
        self.bytes += frame_bytes;
        self.msgs += 1;
        if self.msgs > self.max_msgs_per_sec || self.bytes > self.max_bytes_per_sec {
            return Err(Disconnect::security(
                SecurityCategory::P2pRateLimited,
                close_reason::RATE_LIMITED,
            ));
        }
        Ok(())
    }
}

/// gossip セッションの状態機械。
///
/// トランスポートは持たず、受信済みフレーム([`Message`] + ワイヤバイト数)を
/// [`Session::on_frame`] で与えると、状態遷移と上位への [`SessionAction`] を返す。
pub struct Session {
    direction: Direction,
    state: SessionState,
    config: SessionConfig,
    source: String,
    log: Option<Arc<SecurityLog>>,
    peer: Option<PeerHello>,
    rate: RateLimiter,
    clock: Box<dyn Fn() -> f64 + Send>,
}

/// 既定クロック: 生成時点からの単調経過秒。
fn default_clock() -> Box<dyn Fn() -> f64 + Send> {
    let start = Instant::now();
    Box::new(move || start.elapsed().as_secs_f64())
}

impl Session {
    /// 外向き接続のセッションを作る(状態 = `Connecting`)。
    pub fn new_outbound(
        config: SessionConfig,
        source: String,
        log: Option<Arc<SecurityLog>>,
    ) -> Self {
        Self::new(Direction::Outbound, config, source, log)
    }

    /// 着信接続のセッションを作る(状態 = `Connecting`、HELLO 待ち)。
    pub fn new_inbound(
        config: SessionConfig,
        source: String,
        log: Option<Arc<SecurityLog>>,
    ) -> Self {
        Self::new(Direction::Inbound, config, source, log)
    }

    fn new(
        direction: Direction,
        config: SessionConfig,
        source: String,
        log: Option<Arc<SecurityLog>>,
    ) -> Self {
        let rate = RateLimiter::new(config.max_bytes_per_sec, config.max_msgs_per_sec);
        Self {
            direction,
            state: SessionState::Connecting,
            config,
            source,
            log,
            peer: None,
            rate,
            clock: default_clock(),
        }
    }

    /// クロックを差し替える(テスト用: レート窓の時刻を注入する)。
    pub fn with_clock(mut self, clock: Box<dyn Fn() -> f64 + Send>) -> Self {
        self.clock = clock;
        self
    }

    /// 現在の状態。
    pub fn state(&self) -> SessionState {
        self.state
    }

    /// 接続方向。
    pub fn direction(&self) -> Direction {
        self.direction
    }

    /// 相手の申告情報(HELLO/HELLO_ACK 受信後に得られる)。
    pub fn peer(&self) -> Option<&PeerHello> {
        self.peer.as_ref()
    }

    /// ハンドシェイクを開始する。
    ///
    /// outbound は送るべき HELLO を返し `HelloSent` へ遷移する。inbound は HELLO を待つため
    /// `None`(状態は `Connecting` のまま)。
    pub fn start(&mut self) -> Option<Message> {
        match self.direction {
            Direction::Outbound => {
                self.state = SessionState::HelloSent;
                Some(Message::Hello(self.local_hello()))
            }
            Direction::Inbound => None,
        }
    }

    fn local_hello(&self) -> Hello {
        Hello {
            version: self.config.local_version,
            listen_port: self.config.local_listen_port,
            features: self.config.local_features.clone(),
            nonce: self.config.local_nonce,
            ts: unix_now(),
        }
    }

    /// 受信フレームを処理する。
    ///
    /// `wire_len` はフレーム全体のバイト数(受信レート計上用)。`Ok` は上位への動作列、
    /// `Err` は切断条件。呼び出し側は `Err` 時に [`Session::note_disconnect`] で記録し、
    /// `reason` で CLOSE を送って切断する。
    pub fn on_frame(
        &mut self,
        wire_len: usize,
        message: Message,
    ) -> Result<Vec<SessionAction>, Disconnect> {
        // 検査 2: 受信レート制限(接続時同期の期間中も含め全状態で適用)。
        let now = (self.clock)();
        if let Err(d) = self.rate.charge(now, wire_len) {
            self.state = SessionState::Closed;
            return Err(d);
        }

        match self.state {
            SessionState::Connecting | SessionState::HelloSent => self.handle_handshake(message),
            SessionState::Established => self.handle_established(message),
            SessionState::HelloReceived | SessionState::Closed => {
                // HelloReceived は on_frame を跨がない一過状態、Closed は受信しない。
                self.fail_invalid_frame()
            }
        }
    }

    /// ハンドシェイク中の受信を処理する。
    fn handle_handshake(&mut self, message: Message) -> Result<Vec<SessionAction>, Disconnect> {
        match (self.direction, message) {
            // inbound: HELLO を受けたら検証し HELLO_ACK を返して established。
            (Direction::Inbound, Message::Hello(hello)) => {
                self.state = SessionState::HelloReceived;
                self.accept_peer_hello(hello)?;
                self.state = SessionState::Established;
                Ok(vec![
                    SessionAction::Send(Message::HelloAck(self.local_hello())),
                    SessionAction::Established,
                ])
            }
            // outbound: HELLO_ACK を受けたら検証し established。
            (Direction::Outbound, Message::HelloAck(hello)) => {
                self.accept_peer_hello(hello)?;
                self.state = SessionState::Established;
                Ok(vec![SessionAction::Established])
            }
            // 方向違反(outbound が HELLO、inbound が HELLO_ACK)を含め、
            // established 前の他メッセージはすべて順序違反として即切断。
            _ => self.fail_invalid_frame(),
        }
    }

    /// 相手の HELLO/HELLO_ACK を検証して保持する。
    fn accept_peer_hello(&mut self, hello: Hello) -> Result<(), Disconnect> {
        // バージョンは完全一致のみ受理(非互換は CLOSE incompatible)。
        if hello.version != self.config.local_version {
            self.state = SessionState::Closed;
            return Err(Disconnect::benign(close_reason::INCOMPATIBLE));
        }
        // 自己接続検出: nonce が自ノードの値と一致。
        if hello.nonce == self.config.local_nonce {
            self.state = SessionState::Closed;
            return Err(Disconnect::benign(close_reason::SELF_CONNECT));
        }
        self.peer = Some(PeerHello {
            version: hello.version,
            listen_port: hello.listen_port,
            features: hello.features,
            nonce: hello.nonce,
            ts: hello.ts,
        });
        Ok(())
    }

    /// established 後の受信を上位へ委譲する(内容検証はしない)。
    fn handle_established(&mut self, message: Message) -> Result<Vec<SessionAction>, Disconnect> {
        match message {
            // established 後の再ハンドシェイクは順序違反。
            Message::Hello(_) | Message::HelloAck(_) => self.fail_invalid_frame(),
            // CLOSE は正常切断として通知。
            Message::Close { reason } => {
                self.state = SessionState::Closed;
                Ok(vec![SessionAction::PeerClosed(reason)])
            }
            // その他は上位レイヤへ委譲(伝搬・同期・PEX・keepalive は担当外)。
            other => Ok(vec![SessionAction::Deliver(other)]),
        }
    }

    fn fail_invalid_frame(&mut self) -> Result<Vec<SessionAction>, Disconnect> {
        self.state = SessionState::Closed;
        Err(Disconnect::security(
            SecurityCategory::P2pInvalidFrame,
            close_reason::INVALID_FRAME,
        ))
    }

    /// 切断条件をセキュリティイベントとして記録する(`category` が `Some` かつログ設定時)。
    ///
    /// `on_frame` の `Err` とフレーム層エラー由来の [`Disconnect`] の両方に用いる。
    pub fn note_disconnect(&self, disconnect: &Disconnect) {
        if let (Some(category), Some(log)) = (disconnect.category, &self.log) {
            log.log(category, &self.source, disconnect.reason);
        }
    }
}

/// 現在の unix 時刻(秒)。HELLO の `ts` 用。
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn cfg(nonce: u64) -> SessionConfig {
        SessionConfig {
            local_nonce: nonce,
            local_listen_port: 7147,
            ..SessionConfig::default()
        }
    }

    fn hello(version: u32, nonce: u64) -> Message {
        Message::Hello(Hello {
            version,
            listen_port: 7200,
            features: vec![],
            nonce,
            ts: 1720000000,
        })
    }

    #[test]
    fn outbound_start_emits_hello() {
        let mut s = Session::new_outbound(cfg(1), "p:1".into(), None);
        assert_eq!(s.state(), SessionState::Connecting);
        let msg = s.start().unwrap();
        assert!(matches!(msg, Message::Hello(_)));
        assert_eq!(s.state(), SessionState::HelloSent);
    }

    #[test]
    fn inbound_completes_on_hello() {
        let mut s = Session::new_inbound(cfg(1), "p:2".into(), None);
        assert!(s.start().is_none());
        let actions = s.on_frame(64, hello(1, 42)).unwrap();
        assert_eq!(s.state(), SessionState::Established);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, SessionAction::Send(Message::HelloAck(_))))
        );
        assert!(actions.contains(&SessionAction::Established));
        assert_eq!(s.peer().unwrap().nonce, 42);
    }

    #[test]
    fn outbound_completes_on_hello_ack() {
        let mut s = Session::new_outbound(cfg(1), "p:3".into(), None);
        s.start();
        let ack = Message::HelloAck(Hello {
            version: 1,
            listen_port: 0,
            features: vec![],
            nonce: 99,
            ts: 1,
        });
        let actions = s.on_frame(64, ack).unwrap();
        assert_eq!(s.state(), SessionState::Established);
        assert!(actions.contains(&SessionAction::Established));
    }

    #[test]
    fn pre_established_other_message_disconnects() {
        let mut s = Session::new_inbound(cfg(1), "p:4".into(), None);
        let err = s.on_frame(16, Message::Ping { nonce: 1 }).unwrap_err();
        assert_eq!(err.category, Some(SecurityCategory::P2pInvalidFrame));
        assert_eq!(s.state(), SessionState::Closed);
    }

    #[test]
    fn outbound_wrong_handshake_direction_disconnects() {
        let mut s = Session::new_outbound(cfg(1), "p:5".into(), None);
        s.start();
        let err = s.on_frame(64, hello(1, 2)).unwrap_err();
        assert_eq!(err.category, Some(SecurityCategory::P2pInvalidFrame));
    }

    #[test]
    fn version_mismatch_is_benign_incompatible() {
        let mut s = Session::new_inbound(cfg(1), "p:6".into(), None);
        let err = s.on_frame(64, hello(2, 5)).unwrap_err();
        assert_eq!(err.reason, close_reason::INCOMPATIBLE);
        assert_eq!(err.category, None);
    }

    #[test]
    fn self_connection_detected() {
        let mut s = Session::new_inbound(cfg(0xABCD), "p:7".into(), None);
        let err = s.on_frame(64, hello(1, 0xABCD)).unwrap_err();
        assert_eq!(err.reason, close_reason::SELF_CONNECT);
    }

    #[test]
    fn established_delivers_and_closes() {
        let mut s = Session::new_inbound(cfg(1), "p:8".into(), None);
        s.on_frame(64, hello(1, 3)).unwrap();
        let a = s.on_frame(16, Message::SyncReq { since: 1 }).unwrap();
        assert!(
            a.iter()
                .any(|x| matches!(x, SessionAction::Deliver(Message::SyncReq { since: 1 })))
        );
        let c = s
            .on_frame(
                16,
                Message::Close {
                    reason: "going_away".into(),
                },
            )
            .unwrap();
        assert!(
            c.iter()
                .any(|x| matches!(x, SessionAction::PeerClosed(r) if r == "going_away"))
        );
        assert_eq!(s.state(), SessionState::Closed);
    }

    #[test]
    fn rate_limit_message_count() {
        let clock = Arc::new(AtomicU64::new(0));
        let c2 = Arc::clone(&clock);
        let mut s = Session::new_inbound(cfg(1), "p:9".into(), None)
            .with_clock(Box::new(move || c2.load(Ordering::SeqCst) as f64));
        s.on_frame(16, hello(1, 3)).unwrap();
        let mut hit = false;
        for i in 0..500u64 {
            if s.on_frame(16, Message::Ping { nonce: i }).is_err() {
                hit = true;
                break;
            }
        }
        assert!(hit, "200 msg/秒 超過で切断されるべき");
    }

    #[test]
    fn rate_limit_byte_volume() {
        let mut s = Session::new_inbound(cfg(1), "p:10".into(), None).with_clock(Box::new(|| 0.0));
        s.on_frame(64, hello(1, 3)).unwrap();
        // 256KB を 1 メッセージで超える(メッセージ数上限より先にバイト上限)。
        let err = s
            .on_frame(DEFAULT_MAX_BYTES_PER_SEC + 1, Message::Ping { nonce: 1 })
            .unwrap_err();
        assert_eq!(err.category, Some(SecurityCategory::P2pRateLimited));
    }

    #[test]
    fn rate_window_resets_after_one_second() {
        let clock = Arc::new(AtomicU64::new(0));
        let c2 = Arc::clone(&clock);
        let mut s = Session::new_inbound(cfg(1), "p:11".into(), None)
            .with_clock(Box::new(move || c2.load(Ordering::SeqCst) as f64));
        s.on_frame(16, hello(1, 3)).unwrap();
        // 窓内で 150 件(上限未満)
        for i in 0..150u64 {
            s.on_frame(16, Message::Ping { nonce: i }).unwrap();
        }
        // 次の秒窓へ進めばリセットされ、さらに 150 件送れる
        clock.store(2, Ordering::SeqCst);
        for i in 0..150u64 {
            s.on_frame(16, Message::Ping { nonce: i }).unwrap();
        }
    }

    #[test]
    fn note_disconnect_records_to_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("security.log");
        let log = Arc::new(SecurityLog::new(&path).unwrap());
        let mut s = Session::new_inbound(cfg(1), "198.51.100.9:7147".into(), Some(log.clone()));
        let err = s.on_frame(16, Message::Ping { nonce: 1 }).unwrap_err();
        s.note_disconnect(&err);
        log.flush();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("p2p_invalid_frame"), "記録内容: {content}");
    }
}

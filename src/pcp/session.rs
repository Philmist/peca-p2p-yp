//! PCP announce セッション(T026)
//!
//! contracts/pcp-announce.md のセッションフローを実装する。
//! `accept → PCP_HELO 受信 → PCP_OLEH 応答 → PCP_BCST 継続処理 → 終了` の状態機械と、
//! 待受サーバの起動エントリ [`serve`] を提供する。
//!
//! ## 責務
//! - HELO(BroadcastID)→ OLEH 応答(gist 準拠の応答 atom + agent 名 `peca-p2p-yp/<semver>`)
//! - BCST 解析(name/gnre/desc/url/bitr/type/titl/crea/albm + PCP_HOST)を [`RawChannelInfo`]
//!   へ写し、[`ChannelRegistry`] へ upsert
//! - 1 セッション内の複数チャンネル(ChannelID 単位、≤ 16。超過は無視+`pcp_reject`)
//! - 状態機械 announced→updating⇄…→ended。**playing=false / PCP_QUIT / TCP 異常切断**の
//!   いずれでもチャンネルを即 ended とし、鮮度切れを待たない
//! - loopback 外接続の即切断(LAN 公開オプトイン無効の間)
//! - セッションレート ≤ 64KB/秒・同時セッション ≤ 32(TCP 接続単位)
//! - 文字列長超過は切詰め許容([`crate::pcp::channel`] の検証で実施)
//!
//! ## 設計判断: playing=false の適用範囲
//! contracts/pcp-announce.md は「playing=false / PCP_QUIT / TCP 切断のいずれでも当該セッションの
//! 全チャンネルを ended」とする。本実装では **playing=false の BCST はその ChannelID の
//! チャンネルのみを ended** とし(多チャンネルセッションで無関係な live チャンネルを巻き込まない)、
//! **PCP_QUIT / TCP 切断はチャンネル粒度の情報がないため全チャンネルを ended** とする。
//! 単一チャンネルセッション(通常運用)では両者は一致する。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, watch};

use crate::pcp::atom::Atom;
use crate::pcp::channel::{AnnouncedChannel, ChannelRegistry, ChannelState, RawChannelInfo};
use crate::security::{SecurityCategory, SecurityLog};

/// PCP プロトコルバージョン(OLEH の `ver`)。
pub const PCP_PROTOCOL_VERSION: i32 = 1;
/// 1 セッションあたりの同時掲載チャンネル数上限。
pub const MAX_CHANNELS_PER_SESSION: usize = 16;
/// 同時アナウンスセッション数上限(TCP 接続単位)。
pub const MAX_CONCURRENT_SESSIONS: usize = 32;
/// 1 セッションの累積受信レート上限(64KB/秒)。
pub const MAX_SESSION_BYTES_PER_SEC: usize = 64 * 1024;
/// 単一 atom を待つ間に保持してよい未処理バイトの上限(無制限バッファリング防止)。
pub const MAX_PENDING_BYTES: usize = 256 * 1024;

/// PCP_HOST flg1: PUSH(firewalled — 直接到達不可)。
const FLG1_PUSH: u8 = 0x08;
/// PCP_HOST flg1: RECV(受信中 = playing)。
const FLG1_RECV: u8 = 0x10;

/// agent 名 `peca-p2p-yp/<semver>`(互換性検証の識別のため固定書式 — contracts/pcp-announce.md)。
pub fn agent_name() -> String {
    format!("peca-p2p-yp/{}", env!("CARGO_PKG_VERSION"))
}

/// 接続元が loopback か(LAN 公開オプトイン無効時は loopback のみ受理)。
pub fn source_is_loopback(addr: &SocketAddr) -> bool {
    addr.ip().is_loopback()
}

/// [`AnnounceSession::on_atom`] が上位(pump)へ返す動作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnnounceAction {
    /// 相手へ送るべき atom(OLEH 応答など)。
    Send(Atom),
}

/// 切断を伴う条件。`category` が `Some` ならセキュリティイベントとして記録すべき違反。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PcpDisconnect {
    /// ログ用の短い理由(内部情報を含めてはならない — Principle II)。
    pub reason: &'static str,
    /// 記録すべきセキュリティカテゴリ(通常切断は `None`)。
    pub category: Option<SecurityCategory>,
}

impl PcpDisconnect {
    fn reject(reason: &'static str) -> Self {
        Self {
            reason,
            category: Some(SecurityCategory::PcpReject),
        }
    }

    fn benign(reason: &'static str) -> Self {
        Self {
            reason,
            category: None,
        }
    }
}

/// ハンドシェイク状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Handshake {
    /// HELO 待ち。
    BeforeHelo,
    /// HELO 受信済み(BCST 処理中)。
    Established,
}

/// 固定 1 秒窓のセッション受信レート制限(バイトのみ)。
struct RateLimiter {
    window: u64,
    bytes: usize,
    max_bytes_per_sec: usize,
    initialized: bool,
}

impl RateLimiter {
    fn new(max_bytes_per_sec: usize) -> Self {
        Self {
            window: 0,
            bytes: 0,
            max_bytes_per_sec,
            initialized: false,
        }
    }

    fn charge(&mut self, now: u64, n: usize) -> Result<(), PcpDisconnect> {
        if !self.initialized || now != self.window {
            self.window = now;
            self.bytes = 0;
            self.initialized = true;
        }
        self.bytes += n;
        if self.bytes > self.max_bytes_per_sec {
            return Err(PcpDisconnect::reject("session receive rate exceeded"));
        }
        Ok(())
    }
}

/// PCP announce セッションの状態機械(トランスポート非依存)。
///
/// 復号済み atom を [`on_atom`](AnnounceSession::on_atom) で与えると、状態遷移・レジストリ更新・
/// 上位への [`AnnounceAction`] を返す。ended-on-disconnect は [`end_all`](AnnounceSession::end_all)。
pub struct AnnounceSession {
    registry: Arc<ChannelRegistry>,
    security: Arc<SecurityLog>,
    source: String,
    source_addr: Option<SocketAddr>,
    state: Handshake,
    broadcast_id: Option<[u8; 16]>,
    /// このセッションが掲載中の ChannelID → 初回受信時刻(started_at)。
    channels: HashMap<[u8; 16], u64>,
    rate: RateLimiter,
    clock: Box<dyn Fn() -> u64 + Send>,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl AnnounceSession {
    /// セッションを作る。`source` は接続元(loopback アドレス)。
    pub fn new(registry: Arc<ChannelRegistry>, security: Arc<SecurityLog>, source: String) -> Self {
        let source_addr = source.parse().ok();
        Self {
            registry,
            security,
            source,
            source_addr,
            state: Handshake::BeforeHelo,
            broadcast_id: None,
            channels: HashMap::new(),
            rate: RateLimiter::new(MAX_SESSION_BYTES_PER_SEC),
            clock: Box::new(unix_now),
        }
    }

    /// クロック(unix 秒)を差し替える(テスト用)。
    pub fn with_clock(mut self, clock: Box<dyn Fn() -> u64 + Send>) -> Self {
        self.clock = clock;
        self
    }

    /// このセッションが識別している BroadcastID(HELO 受信後)。
    pub fn broadcast_id(&self) -> Option<[u8; 16]> {
        self.broadcast_id
    }

    /// このセッションが掲載中のチャンネル数。
    pub fn tracked_channels(&self) -> usize {
        self.channels.len()
    }

    /// 受信 atom を処理する。`wire_len` はフレーム全体のバイト数(レート計上用)。
    pub fn on_atom(
        &mut self,
        wire_len: usize,
        atom: Atom,
    ) -> Result<Vec<AnnounceAction>, PcpDisconnect> {
        // セッション受信レート(≤ 64KB/秒)。
        self.rate.charge((self.clock)(), wire_len)?;

        match self.state {
            Handshake::BeforeHelo => {
                if atom.id().matches("helo") {
                    let sid = required_guid(&atom, "sid")?;
                    self.broadcast_id = Some(sid);
                    self.state = Handshake::Established;
                    Ok(vec![AnnounceAction::Send(self.build_oleh(&sid))])
                } else {
                    // HELO 前のその他 atom は無視(切断しない)。
                    tracing::debug!(target: "pcp", atom = %atom.id().name(), "HELO 前の atom を無視");
                    Ok(vec![])
                }
            }
            Handshake::Established => {
                let name = atom.id();
                if name.matches("bcst") {
                    self.handle_bcst(&atom)
                } else if name.matches("quit") {
                    // 全チャンネルの ended は呼び出し側の end_all に委ねる(TCP 切断と統一)。
                    Err(PcpDisconnect::benign("quit"))
                } else {
                    // 未知・非対応 atom は無視(セキュリティイベントとしない — 前方互換)。
                    tracing::debug!(target: "pcp", atom = %atom.id().name(), "未知 atom を無視");
                    Ok(vec![])
                }
            }
        }
    }

    /// BCST を解析してチャンネルを登録/更新/終了する。
    fn handle_bcst(&mut self, atom: &Atom) -> Result<Vec<AnnounceAction>, PcpDisconnect> {
        let Some(chan) = atom.find("chan") else {
            // チャンネル情報を伴わない BCST は無視する。
            return Ok(vec![]);
        };
        let cid = required_guid(chan, "cid")?;

        let host = chan.find("host");
        let flg1 = host
            .and_then(|h| h.find("flg1"))
            .and_then(Atom::as_i32)
            .unwrap_or(0) as u8;
        // host が無ければ playing とみなす(announce。tracker 不明)。
        let playing = match host {
            Some(_) => flg1 & FLG1_RECV != 0,
            None => true,
        };
        let firewalled = flg1 & FLG1_PUSH != 0;

        if !playing {
            // playing=false → その ChannelID のみ ended。
            if self.channels.remove(&cid).is_some() {
                self.registry.end(&cid);
            }
            return Ok(vec![]);
        }

        // ≤ 16 チャンネル。新規 cid が上限超過なら無視+pcp_reject(切断しない)。
        let is_known = self.channels.contains_key(&cid);
        if !is_known && self.channels.len() >= MAX_CHANNELS_PER_SESSION {
            self.security.log(
                SecurityCategory::PcpReject,
                &self.source,
                "session channel limit exceeded",
            );
            return Ok(vec![]);
        }

        let now = (self.clock)();
        let started_at = *self.channels.entry(cid).or_insert(now);
        let state = if is_known {
            ChannelState::Updating
        } else {
            ChannelState::Announced
        };

        let info = chan.find("info");
        let trck = chan.find("trck");
        let tracker = if firewalled {
            None
        } else {
            host.and_then(build_tracker)
        };

        let raw = RawChannelInfo {
            channel_id: cid,
            name: child_str(info, "name"),
            genre: child_str(info, "gnre"),
            description: child_str(info, "desc"),
            contact_url: child_str(info, "url"),
            bitrate: child_i32(info, "bitr", 0),
            content_type: child_str(info, "type"),
            track_title: child_str(trck, "titl"),
            track_creator: child_str(trck, "crea"),
            track_album: child_str(trck, "albm"),
            tracker,
            listeners: child_i32(host, "numl", -1),
            relays_cnt: child_i32(host, "numr", -1),
            started_at,
        };
        // 状態は registry が権威(既存有無で Announced/Updating を確定する)。
        self.registry.upsert(AnnouncedChannel::from_raw(raw, state));
        Ok(vec![])
    }

    /// このセッションの全チャンネルを ended にする(PCP_QUIT / TCP 切断時)。
    pub fn end_all(&mut self) {
        let ids: Vec<[u8; 16]> = self.channels.keys().copied().collect();
        for cid in ids {
            self.registry.end(&cid);
        }
        self.channels.clear();
    }

    /// 切断条件をセキュリティイベントとして記録する(`category` が `Some` のとき)。
    pub fn note_disconnect(&self, disconnect: &PcpDisconnect) {
        if let Some(category) = disconnect.category {
            self.security.log(category, &self.source, disconnect.reason);
        }
    }

    /// pcp_reject を記録する(コーデックエラー・バッファ超過など)。
    pub fn log_reject(&self, reason: &str) {
        self.security
            .log(SecurityCategory::PcpReject, &self.source, reason);
    }

    /// OLEH 応答を組み立てる(sid エコー・agent 名・ver・接続元から観測した rip/port)。
    fn build_oleh(&self, sid: &[u8; 16]) -> Atom {
        let mut children = vec![
            Atom::bytes("sid", sid),
            Atom::str("agnt", &agent_name()),
            Atom::i32("ver", PCP_PROTOCOL_VERSION),
        ];
        if let Some(SocketAddr::V4(v4)) = self.source_addr {
            children.push(Atom::bytes("rip", &v4.ip().octets()));
            children.push(Atom::u16v("port", v4.port()));
        }
        Atom::parent("oleh", children)
    }
}

/// 親 atom の子(文字列)を読む。欠落は空文字列。
fn child_str(parent: Option<&Atom>, name: &str) -> String {
    parent
        .and_then(|p| p.find(name))
        .and_then(Atom::as_str)
        .unwrap_or_default()
}

/// 親 atom の子(整数)を読む。欠落は `default`。
fn child_i32(parent: Option<&Atom>, name: &str, default: i64) -> i64 {
    parent
        .and_then(|p| p.find(name))
        .and_then(Atom::as_i32)
        .map(i64::from)
        .unwrap_or(default)
}

/// PCP_HOST から `ip:port`(IPv4)を組む。ip が 4 バイトでない/port 不正なら `None`。
fn build_tracker(host: &Atom) -> Option<String> {
    let ip = host.find("ip")?.payload()?;
    if ip.len() != 4 {
        return None;
    }
    let port = host.find("port")?.as_i32()?;
    if !(0..=65535).contains(&port) {
        return None;
    }
    Some(format!("{}.{}.{}.{}:{}", ip[0], ip[1], ip[2], ip[3], port))
}

/// 親 atom から 16 バイト GUID を取り出す(欠落・長さ不正は `pcp_reject` 切断)。
fn required_guid(parent: &Atom, name: &str) -> Result<[u8; 16], PcpDisconnect> {
    let payload = parent
        .find(name)
        .and_then(Atom::payload)
        .ok_or_else(|| PcpDisconnect::reject("missing guid atom"))?;
    if payload.len() != 16 {
        return Err(PcpDisconnect::reject("invalid guid length"));
    }
    let mut guid = [0u8; 16];
    guid.copy_from_slice(payload);
    Ok(guid)
}

// ---------------------------------------------------------------------------
// 待受サーバ
// ---------------------------------------------------------------------------

/// PCP アナウンス待受サーバを駆動する(T020 の起動配線から呼ばれる)。
///
/// loopback 以外からの接続は即切断+`pcp_reject`、同時セッションは ≤ 32(超過は新規接続拒否)。
/// `shutdown` が `true` になると受付を止める。各接続は独立タスクで処理し、切断時に
/// そのセッションの全チャンネルを ended にする。
pub async fn serve(
    listener: TcpListener,
    registry: Arc<ChannelRegistry>,
    security: Arc<SecurityLog>,
    mut shutdown: watch::Receiver<bool>,
) {
    let sessions = Arc::new(Semaphore::new(MAX_CONCURRENT_SESSIONS));
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(v) => v,
                    Err(_) => continue, // 一過性 accept エラーは受付継続
                };
                if !source_is_loopback(&peer) {
                    security.log(SecurityCategory::PcpReject, &peer.to_string(), "non-loopback source");
                    continue; // stream drop = 切断
                }
                // 同時セッション ≤ 32。超過は新規接続拒否。
                let Ok(permit) = Arc::clone(&sessions).try_acquire_owned() else {
                    continue;
                };
                let registry = Arc::clone(&registry);
                let security = Arc::clone(&security);
                let sd = shutdown.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    handle_conn(stream, peer, registry, security, sd).await;
                });
            }
        }
    }
}

async fn handle_conn(
    stream: tokio::net::TcpStream,
    peer: SocketAddr,
    registry: Arc<ChannelRegistry>,
    security: Arc<SecurityLog>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut session = AnnounceSession::new(registry, security, peer.to_string());
    let (mut reader, mut writer) = stream.into_split();
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];

    'conn: loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            read = reader.read(&mut chunk) => {
                let n = match read {
                    Ok(0) => break,            // TCP 正常/異常切断
                    Ok(n) => n,
                    Err(_) => break,
                };
                buf.extend_from_slice(&chunk[..n]);
                loop {
                    match Atom::try_decode(&buf) {
                        Ok(Some((atom, used))) => {
                            buf.drain(..used);
                            match session.on_atom(used, atom) {
                                Ok(actions) => {
                                    for AnnounceAction::Send(resp) in actions {
                                        if write_atom(&mut writer, &resp).await.is_err() {
                                            break 'conn;
                                        }
                                    }
                                }
                                Err(disconnect) => {
                                    session.note_disconnect(&disconnect);
                                    break 'conn;
                                }
                            }
                        }
                        Ok(None) => {
                            if buf.len() > MAX_PENDING_BYTES {
                                session.log_reject("pending atom buffer overflow");
                                break 'conn;
                            }
                            break; // 追加受信を待つ
                        }
                        Err(err) => {
                            session.log_reject(err.reason());
                            break 'conn;
                        }
                    }
                }
            }
        }
    }

    // 切断(QUIT / TCP 切断 / エラー / shutdown)時に全チャンネルを即 ended とする。
    session.end_all();
}

/// atom をワイヤバイト列として書き出す。
async fn write_atom<W: AsyncWrite + Unpin>(writer: &mut W, atom: &Atom) -> std::io::Result<()> {
    let bytes = atom.to_bytes();
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pcp::channel::ChannelChange;

    fn security() -> (Arc<SecurityLog>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(SecurityLog::new(dir.path().join("security.log")).unwrap());
        (log, dir)
    }

    fn helo(sid: &[u8; 16]) -> Atom {
        Atom::parent("helo", vec![Atom::bytes("sid", sid)])
    }

    fn bcst(cid: &[u8; 16], name: &str, flg1: u8) -> Atom {
        Atom::parent(
            "bcst",
            vec![Atom::parent(
                "chan",
                vec![
                    Atom::bytes("cid", cid),
                    Atom::parent("info", vec![Atom::str("name", name), Atom::i32("bitr", 500)]),
                    Atom::parent(
                        "host",
                        vec![
                            Atom::bytes("ip", &[192, 0, 2, 5]),
                            Atom::u16v("port", 7144),
                            Atom::i32("numl", 4),
                            Atom::i32("numr", 1),
                            Atom::u8v("flg1", flg1),
                        ],
                    ),
                ],
            )],
        )
    }

    fn feed(session: &mut AnnounceSession, atom: &Atom) -> Vec<AnnounceAction> {
        session.on_atom(atom.to_bytes().len(), atom.clone()).unwrap()
    }

    #[test]
    fn helo_transitions_to_established_and_returns_oleh() {
        let (sec, _g) = security();
        let registry = ChannelRegistry::new();
        let mut s = AnnounceSession::new(registry, sec, "127.0.0.1:40000".into());
        let actions = feed(&mut s, &helo(&[1u8; 16]));
        assert_eq!(s.broadcast_id(), Some([1u8; 16]));
        assert_eq!(actions.len(), 1);
        let AnnounceAction::Send(oleh) = &actions[0];
        assert!(oleh.id().matches("oleh"));
        assert_eq!(oleh.find("agnt").and_then(|a| a.as_str()), Some(agent_name()));
    }

    #[test]
    fn bcst_registers_channel() {
        let (sec, _g) = security();
        let registry = ChannelRegistry::new();
        let mut s = AnnounceSession::new(registry.clone(), sec, "127.0.0.1:40001".into());
        feed(&mut s, &helo(&[2u8; 16]));
        feed(&mut s, &bcst(&[9u8; 16], "A", FLG1_RECV));
        assert_eq!(registry.len(), 1);
        assert_eq!(s.tracked_channels(), 1);
        let snap = registry.snapshot();
        assert_eq!(snap[0].tracker.as_deref(), Some("192.0.2.5:7144"));
    }

    #[test]
    fn playing_false_ends_channel() {
        let (sec, _g) = security();
        let registry = ChannelRegistry::new();
        let mut s = AnnounceSession::new(registry.clone(), sec, "127.0.0.1:40002".into());
        feed(&mut s, &helo(&[3u8; 16]));
        feed(&mut s, &bcst(&[9u8; 16], "A", FLG1_RECV));
        feed(&mut s, &bcst(&[9u8; 16], "A", 0)); // playing=false
        assert!(registry.is_empty());
        assert_eq!(s.tracked_channels(), 0);
    }

    #[test]
    fn quit_returns_benign_disconnect() {
        let (sec, _g) = security();
        let registry = ChannelRegistry::new();
        let mut s = AnnounceSession::new(registry.clone(), sec, "127.0.0.1:40003".into());
        feed(&mut s, &helo(&[4u8; 16]));
        feed(&mut s, &bcst(&[9u8; 16], "A", FLG1_RECV));
        let quit = Atom::parent("quit", vec![]);
        let err = s.on_atom(quit.to_bytes().len(), quit).unwrap_err();
        assert!(err.category.is_none());
        s.end_all();
        assert!(registry.is_empty());
    }

    #[test]
    fn end_all_emits_ended_with_final_state() {
        let (sec, _g) = security();
        let registry = ChannelRegistry::new();
        let mut rx = registry.subscribe();
        let mut s = AnnounceSession::new(registry.clone(), sec, "127.0.0.1:40004".into());
        feed(&mut s, &helo(&[5u8; 16]));
        feed(&mut s, &bcst(&[9u8; 16], "A", FLG1_RECV));
        let _ = rx.try_recv(); // announced
        s.end_all();
        assert!(matches!(rx.try_recv(), Ok(ChannelChange::Ended(_))));
    }

    #[test]
    fn firewalled_has_no_tracker() {
        let (sec, _g) = security();
        let registry = ChannelRegistry::new();
        let mut s = AnnounceSession::new(registry.clone(), sec, "127.0.0.1:40005".into());
        feed(&mut s, &helo(&[6u8; 16]));
        feed(&mut s, &bcst(&[9u8; 16], "A", FLG1_RECV | FLG1_PUSH));
        assert_eq!(registry.snapshot()[0].tracker, None);
    }

    #[test]
    fn invalid_guid_rejects() {
        let (sec, _g) = security();
        let registry = ChannelRegistry::new();
        let mut s = AnnounceSession::new(registry, sec, "127.0.0.1:40006".into());
        let bad = Atom::parent("helo", vec![Atom::bytes("sid", &[0u8; 15])]);
        let err = s.on_atom(bad.to_bytes().len(), bad).unwrap_err();
        assert_eq!(err.category, Some(SecurityCategory::PcpReject));
    }
}

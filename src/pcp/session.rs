//! PCP announce セッション(T026)
//!
//! contracts/pcp-announce.md のセッションフローを実装する。
//! `accept → PCP_HELO 受信 → PCP_OLEH 応答 → PCP_BCST 継続処理 → 終了` の状態機械と、
//! 待受サーバの起動エントリ [`serve`] を提供する。
//!
//! ## 責務
//! - HELO(sid・bcid)→ OLEH 応答(**自ノードの SessionID** + agent 名 `peca-p2p-yp/<semver>`。
//!   sid のエコーはクライアント側の自己接続判定を誤発火させるため禁止 — 本家 PeerCast 互換)
//! - BCST 解析(name/gnre/desc/url/bitr/type/titl/crea/albm + PCP_HOST)を [`RawChannelInfo`]
//!   へ写し、[`ChannelRegistry`] へ upsert。`chan`(情報)と `host`(リスナー数・トラッカー)は
//!   **別々の BCST** で届きうるためセッション内でマージする(実機 PeerCastStation の挙動)
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
    /// このセッションが掲載中の ChannelID → 累積チャンネル情報。
    /// PeerCastStation はチャンネル情報(`chan`)とホスト情報(`host`)を**別々の
    /// BCST** で送るため、セッション側で 1 レコードへマージしてから registry へ渡す。
    channels: HashMap<[u8; 16], RawChannelInfo>,
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
                    tracing::debug!(
                        target: "pcp",
                        source = %self.source,
                        sid = %guid_debug(&atom, "sid"),
                        bcid = %guid_debug(&atom, "bcid"),
                        children = %children_debug(&atom),
                        "PCP_HELO 受信"
                    );
                    let sid = required_guid(&atom, "sid", "missing sid atom in helo")?;
                    // セッション識別は bcid(BroadcastID)を優先する。HELO の sid は
                    // 接続ごとの SessionID であり配信者の識別子ではない(bcid を
                    // 送らないクライアントは sid で代用)。
                    let bcid = atom
                        .find("bcid")
                        .and_then(Atom::payload)
                        .filter(|p| p.len() == 16)
                        .map(|p| {
                            let mut g = [0u8; 16];
                            g.copy_from_slice(p);
                            g
                        })
                        .unwrap_or(sid);
                    self.broadcast_id = Some(bcid);
                    self.state = Handshake::Established;
                    Ok(vec![AnnounceAction::Send(self.build_oleh(&atom))])
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
    ///
    /// 実 PCP(PeerCastStation、2026-07-04 実機検証)は 1 チャンネルにつき
    /// **`chan`(チャンネル情報)と `host`(リスナー数・トラッカー)を別々の BCST**
    /// で送るため、どちらか一方だけの BCST は [`Self::channels`] の累積レコードへ
    /// マージしてから upsert する。ChannelID の所在も実 PCP 準拠:
    /// `chan` 直下は **`id`**、`bcst` 直下と `host` 内は `cid`。
    fn handle_bcst(&mut self, atom: &Atom) -> Result<Vec<AnnounceAction>, PcpDisconnect> {
        let chan = atom.find("chan");
        // host は bcst 直下(実 PCP のホスト通知)または chan 内(単一 BCST に
        // まとめるクライアント)のどちらでも受ける。
        let host = chan
            .and_then(|c| c.find("host"))
            .or_else(|| atom.find("host"));
        if chan.is_none() && host.is_none() {
            // チャンネル情報を伴わない BCST は無視する。
            return Ok(vec![]);
        }
        tracing::debug!(
            target: "pcp",
            source = %self.source,
            bcst = %children_debug(atom),
            chan = %chan.map(children_debug).unwrap_or_default(),
            host = %host.map(children_debug).unwrap_or_default(),
            "PCP_BCST 受信"
        );

        let cid_atom = chan
            .and_then(|c| c.find("id"))
            .or_else(|| atom.find("cid"))
            .or_else(|| host.and_then(|h| h.find("cid")));
        let Some(payload) = cid_atom.and_then(Atom::payload) else {
            return Err(PcpDisconnect::reject("missing channel id in bcst"));
        };
        if payload.len() != 16 {
            return Err(PcpDisconnect::reject("invalid guid length"));
        }
        let mut cid = [0u8; 16];
        cid.copy_from_slice(payload);

        // playing 判定は host を伴う BCST でのみ行う(chan のみの BCST は情報更新)。
        let flg1 = host
            .and_then(|h| h.find("flg1"))
            .and_then(Atom::as_i32)
            .unwrap_or(0) as u8;
        if host.is_some() && flg1 & FLG1_RECV == 0 {
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
        let entry = self.channels.entry(cid).or_insert_with(|| RawChannelInfo {
            channel_id: cid,
            name: String::new(),
            genre: String::new(),
            description: String::new(),
            contact_url: String::new(),
            bitrate: 0,
            content_type: String::new(),
            track_title: String::new(),
            track_creator: String::new(),
            track_album: String::new(),
            tracker: None,
            listeners: -1,
            relays_cnt: -1,
            started_at: now,
        });
        if let Some(chan) = chan {
            if let Some(info) = chan.find("info") {
                let info = Some(info);
                entry.name = child_str(info, "name");
                entry.genre = child_str(info, "gnre");
                entry.description = child_str(info, "desc");
                entry.contact_url = child_str(info, "url");
                entry.bitrate = child_i32(info, "bitr", 0);
                entry.content_type = child_str(info, "type");
            }
            if let Some(trck) = chan.find("trck") {
                let trck = Some(trck);
                entry.track_title = child_str(trck, "titl");
                entry.track_creator = child_str(trck, "crea");
                entry.track_album = child_str(trck, "albm");
            }
        }
        if let Some(host) = host {
            let firewalled = flg1 & FLG1_PUSH != 0;
            entry.tracker = if firewalled { None } else { build_tracker(host) };
            entry.listeners = child_i32(Some(host), "numl", entry.listeners);
            entry.relays_cnt = child_i32(Some(host), "numr", entry.relays_cnt);
        }

        let state = if is_known {
            ChannelState::Updating
        } else {
            ChannelState::Announced
        };
        // 状態は registry が権威(既存有無で Announced/Updating を確定する)。
        self.registry
            .upsert(AnnouncedChannel::from_raw(entry.clone(), state));
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

    /// OLEH 応答を組み立てる(自ノード SessionID・agent 名・ver・観測 rip・申告 port エコー)。
    ///
    /// `sid` には**自ノード自身の SessionID** を入れる。PCP クライアント
    /// (本家 PeerCast / PeerCastStation)は OLEH の sid が自分の SessionID と
    /// 一致すると自己接続と判定して切断するため、HELO の sid をエコーしてはならない。
    /// `port` は HELO で申告された待受ポートのエコー(loopback 専用 YP のため
    /// connect-back による到達性検証は行わない。申告が無ければ省略)。
    fn build_oleh(&self, helo: &Atom) -> Atom {
        let mut children = vec![
            Atom::bytes("sid", node_session_id()),
            Atom::str("agnt", &agent_name()),
            Atom::i32("ver", PCP_PROTOCOL_VERSION),
        ];
        if let Some(SocketAddr::V4(v4)) = self.source_addr {
            // PCP の IPv4 はリトルエンディアン格納(ワイヤ上はオクテット逆順)。
            let o = v4.ip().octets();
            children.push(Atom::bytes("rip", &[o[3], o[2], o[1], o[0]]));
        }
        if let Some(port) = helo.find("port").and_then(Atom::as_i32)
            && (0..=65535).contains(&port)
        {
            children.push(Atom::u16v("port", port as u16));
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

/// PCP_HOST から `ip:port` を組む。ip が 4/16 バイトでない・port 不正なら `None`。
/// PCP の IP atom はバイト逆順格納(IPv4 = 32bit 整数のリトルエンディアン、
/// IPv6 も同様に 16 バイトを逆順で格納 — PeerCastStation 実機 2026-07-04 確認)。
/// IPv6 は `[addr]:port` のブラケット形式で返す(SocketAddr 互換)。
fn build_tracker(host: &Atom) -> Option<String> {
    let ip = host.find("ip")?.payload()?;
    let port = host.find("port")?.as_i32()?;
    if !(0..=65535).contains(&port) {
        return None;
    }
    match ip.len() {
        4 => Some(format!("{}.{}.{}.{}:{}", ip[3], ip[2], ip[1], ip[0], port)),
        16 => {
            let mut bytes = [0u8; 16];
            for (dst, src) in bytes.iter_mut().zip(ip.iter().rev()) {
                *dst = *src;
            }
            let addr = std::net::Ipv6Addr::from(bytes);
            Some(format!("[{addr}]:{port}"))
        }
        _ => None,
    }
}

/// 自ノードの PCP SessionID(プロセス起動ごとにランダム生成)。
/// OLEH の sid として返す(クライアントの自己接続判定に使われる)。
fn node_session_id() -> &'static [u8; 16] {
    static ID: std::sync::OnceLock<[u8; 16]> = std::sync::OnceLock::new();
    ID.get_or_init(rand::random)
}

/// GUID 子 atom のデバッグ表現(16 バイトなら hex、欠落・長さ不正はその旨)。
fn guid_debug(parent: &Atom, name: &str) -> String {
    match parent.find(name).and_then(Atom::payload) {
        Some(p) if p.len() == 16 => p.iter().map(|b| format!("{b:02x}")).collect(),
        Some(p) => format!("<len={}>", p.len()),
        None => "<absent>".to_string(),
    }
}

/// 親 atom の子構成のデバッグ表現(`名前(ペイロード長)` の列。親 atom は `[parent]`)。
fn children_debug(atom: &Atom) -> String {
    match atom.children() {
        Some(children) => children
            .iter()
            .map(|c| match c.payload() {
                Some(p) => format!("{}({})", c.id().name(), p.len()),
                None => format!("{}[parent]", c.id().name()),
            })
            .collect::<Vec<_>>()
            .join(" "),
        None => "<data atom>".to_string(),
    }
}

/// 親 atom から 16 バイト GUID を取り出す(欠落・長さ不正は `pcp_reject` 切断)。
/// `missing` は欠落時のログ理由(どの atom の欠落か区別できる固定文字列)。
fn required_guid(
    parent: &Atom,
    name: &str,
    missing: &'static str,
) -> Result<[u8; 16], PcpDisconnect> {
    let payload = parent
        .find(name)
        .and_then(Atom::payload)
        .ok_or_else(|| PcpDisconnect::reject(missing))?;
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

    /// 実 PCP の単一 BCST 形(chan の ID は `id`、host は bcst 直下、IP は LE 格納)。
    fn bcst(cid: &[u8; 16], name: &str, flg1: u8) -> Atom {
        Atom::parent(
            "bcst",
            vec![
                Atom::bytes("cid", cid),
                Atom::parent(
                    "chan",
                    vec![
                        Atom::bytes("id", cid),
                        Atom::parent("info", vec![Atom::str("name", name), Atom::i32("bitr", 500)]),
                    ],
                ),
                Atom::parent(
                    "host",
                    vec![
                        Atom::bytes("cid", cid),
                        Atom::bytes("ip", &[5, 2, 0, 192]), // 192.0.2.5(LE 格納)
                        Atom::u16v("port", 7144),
                        Atom::i32("numl", 4),
                        Atom::i32("numr", 1),
                        Atom::u8v("flg1", flg1),
                    ],
                ),
            ],
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
        assert_eq!(s.broadcast_id(), Some([1u8; 16])); // bcid 無し → sid で代用
        assert_eq!(actions.len(), 1);
        let AnnounceAction::Send(oleh) = &actions[0];
        assert!(oleh.id().matches("oleh"));
        assert_eq!(oleh.find("agnt").and_then(|a| a.as_str()), Some(agent_name()));
        // sid はエコーではなく自ノードの SessionID(エコーするとクライアントが
        // 自己接続と誤判定して切断する)。
        let oleh_sid = oleh.find("sid").and_then(Atom::payload).unwrap();
        assert_eq!(oleh_sid.len(), 16);
        assert_ne!(oleh_sid, &[1u8; 16][..]);
    }

    #[test]
    fn bcid_identifies_session_and_declared_port_is_echoed() {
        let (sec, _g) = security();
        let registry = ChannelRegistry::new();
        let mut s = AnnounceSession::new(registry, sec, "127.0.0.1:40006".into());
        let helo = Atom::parent(
            "helo",
            vec![
                Atom::bytes("sid", &[1u8; 16]),
                Atom::bytes("bcid", &[7u8; 16]),
                Atom::u16v("port", 7144),
            ],
        );
        let actions = feed(&mut s, &helo);
        assert_eq!(s.broadcast_id(), Some([7u8; 16]));
        let AnnounceAction::Send(oleh) = &actions[0];
        // OLEH port は HELO 申告値のエコー(接続元の一時ポート 40006 ではない)。
        assert_eq!(oleh.find("port").and_then(Atom::as_i32), Some(7144));
    }

    #[test]
    fn separate_chan_and_host_bcsts_are_merged() {
        let (sec, _g) = security();
        let registry = ChannelRegistry::new();
        let mut s = AnnounceSession::new(registry.clone(), sec, "127.0.0.1:40007".into());
        feed(&mut s, &helo(&[8u8; 16]));
        let cid = [9u8; 16];
        // PeerCastStation 実機の形: chan のみの BCST → host のみの BCST が別便で届く。
        let chan_only = Atom::parent(
            "bcst",
            vec![
                Atom::bytes("cid", &cid),
                Atom::parent(
                    "chan",
                    vec![
                        Atom::bytes("id", &cid),
                        Atom::parent("info", vec![Atom::str("name", "A"), Atom::i32("bitr", 500)]),
                    ],
                ),
            ],
        );
        let host_only = Atom::parent(
            "bcst",
            vec![
                Atom::bytes("cid", &cid),
                Atom::parent(
                    "host",
                    vec![
                        Atom::bytes("cid", &cid),
                        Atom::bytes("ip", &[5, 2, 0, 192]), // 192.0.2.5(LE 格納)
                        Atom::u16v("port", 7144),
                        Atom::i32("numl", 4),
                        Atom::i32("numr", 1),
                        Atom::u8v("flg1", FLG1_RECV),
                    ],
                ),
            ],
        );
        feed(&mut s, &chan_only);
        feed(&mut s, &host_only);
        assert_eq!(s.tracked_channels(), 1);
        let snap = registry.snapshot();
        assert_eq!(snap.len(), 1);
        // chan 便の情報が host 便で消えず、host 便の情報が加わる。
        assert_eq!(snap[0].name, "A");
        assert_eq!(snap[0].tracker.as_deref(), Some("192.0.2.5:7144"));
    }

    #[test]
    fn ipv6_host_yields_bracketed_tracker() {
        let (sec, _g) = security();
        let registry = ChannelRegistry::new();
        let mut s = AnnounceSession::new(registry.clone(), sec, "127.0.0.1:40008".into());
        feed(&mut s, &helo(&[10u8; 16]));
        let cid = [11u8; 16];
        // 2001:db8::1 のワイヤ表現(ネットワークオーダーの逆順格納)
        let ip_wire: [u8; 16] = [
            0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xb8, 0x0d, 0x01, 0x20,
        ];
        let bcst = Atom::parent(
            "bcst",
            vec![
                Atom::bytes("cid", &cid),
                Atom::parent(
                    "chan",
                    vec![
                        Atom::bytes("id", &cid),
                        Atom::parent("info", vec![Atom::str("name", "V6"), Atom::i32("bitr", 500)]),
                    ],
                ),
                Atom::parent(
                    "host",
                    vec![
                        Atom::bytes("cid", &cid),
                        Atom::bytes("ip", &ip_wire),
                        Atom::u16v("port", 7144),
                        Atom::u8v("flg1", FLG1_RECV),
                    ],
                ),
            ],
        );
        feed(&mut s, &bcst);
        let snap = registry.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].tracker.as_deref(), Some("[2001:db8::1]:7144"));
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

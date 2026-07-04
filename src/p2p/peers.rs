//! ピア管理(T018)
//!
//! contracts/p2p-gossip.md §接続管理・§脅威と対応範囲、data-model.md §PeerEndpoint の
//! 接続層ロジック。永続化は [`crate::store::Store`](T012)へ委譲し、本モジュールは
//! **アドレス検証・自己アドレス/自己接続の扱い・接続枠の会計・候補選定・再接続バックオフ・
//! 未検証候補の接続試行レート制限**という判断ロジックとメモリ上状態のみを担う。
//!
//! 実際の TCP 接続確立・タスク起動・全断検出の配線は本タスクの責務ではない
//! (それぞれ T020 main 配線・T047 フェイルオーバーが本モジュールの API を使う)。
//!
//! ## 乱数と時刻の注入
//! 再接続バックオフ(初期 5 秒・係数 2・上限 300 秒)は contracts の定数どおり**決定的**で、
//! ジッタ(乱数)を持たない。P2P 層で唯一の乱数である自ノード nonce は外部生成値を
//! [`crate::p2p::session`] へ渡す方式(本モジュールは扱わない)。テスト可能性のために
//! 注入するのは**時刻**(未検証候補の 1 件/秒スロットル窓)のみ。

use std::collections::HashSet;
use std::net::Ipv6Addr;
use std::sync::Mutex;
use std::time::Instant;

use crate::store::{PeerEndpoint, PeerSource, Store, StoreError};

/// ピアアドレスの最大バイト長(data-model §PeerEndpoint)。
pub const ADDR_MAX_LEN: usize = 256;

/// 維持する外向き接続数の目標(Settings `p2p_outbound_target` 既定)。
pub const DEFAULT_OUTBOUND_TARGET: usize = 8;
/// 着信接続の上限(Settings `p2p_inbound_max` 既定)。
pub const DEFAULT_INBOUND_MAX: usize = 32;
/// 連続失敗で平常時候補から降格する閾値(data-model §PeerEndpoint)。
pub const DEFAULT_FAIL_DEMOTE_THRESHOLD: i64 = 8;
/// 再接続バックオフの初期値(秒)。
pub const DEFAULT_BACKOFF_INITIAL_SECS: u64 = 5;
/// 再接続バックオフの係数。
pub const DEFAULT_BACKOFF_FACTOR: u64 = 2;
/// 再接続バックオフの上限(秒)。
pub const DEFAULT_BACKOFF_CAP_SECS: u64 = 300;
/// 未検証候補への接続試行の最小間隔(秒 — 反射攻撃緩和で 1 件/秒以下)。
pub const DEFAULT_NEW_CANDIDATE_MIN_INTERVAL_SECS: f64 = 1.0;

// ---------------------------------------------------------------------------
// アドレス検証
// ---------------------------------------------------------------------------

/// アドレス検証のエラー(内部情報・入力値は含めない — Principle II)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddrError {
    /// 空文字列。
    Empty,
    /// 256 バイト超。
    TooLong,
    /// ポート区切りがない。
    MissingPort,
    /// ポートが 1..=65535 でない。
    InvalidPort,
    /// ブラケットなしで複数コロンを含み、ポート境界が曖昧。
    AmbiguousColons,
    /// `[...]` の中身が IPv6 リテラルとして不正。
    InvalidIpv6,
    /// ホスト部が空・不正。
    InvalidHost,
}

impl std::fmt::Display for AddrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            AddrError::Empty => "アドレスが空です",
            AddrError::TooLong => "アドレスが長すぎます",
            AddrError::MissingPort => "ポートが指定されていません",
            AddrError::InvalidPort => "ポート番号が不正です",
            AddrError::AmbiguousColons => "IPv6 はブラケット表記が必要です",
            AddrError::InvalidIpv6 => "IPv6 アドレスの形式が不正です",
            AddrError::InvalidHost => "ホストの形式が不正です",
        })
    }
}

impl std::error::Error for AddrError {}

/// 検証済みピアアドレス。`canonical()` を UNIQUE キー・多重接続統合の基準に用いる。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerAddr {
    /// 正規化済みホスト(IPv6 は圧縮小文字・ブラケットなし、ホスト名/IPv4 は小文字)。
    pub host: String,
    /// ポート(1..=65535)。
    pub port: u16,
    /// IPv6 リテラルか。
    pub is_ipv6: bool,
}

impl PeerAddr {
    /// 正規表記(IPv6 は `[host]:port`、その他は `host:port`)。
    pub fn canonical(&self) -> String {
        if self.is_ipv6 {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// `host:port` を検証・正規化する。
///
/// - IPv6 リテラルは `[addr]:port` のブラケット表記のみ許容し、圧縮小文字へ正規化する。
/// - ブラケットなしで複数コロンを含む文字列はポート境界が曖昧なため拒否する。
/// - ホスト名・IPv4 のホスト部は小文字化して正規化する(大小の違いによる多重登録を防ぐ)。
pub fn parse_addr(input: &str) -> std::result::Result<PeerAddr, AddrError> {
    if input.is_empty() {
        return Err(AddrError::Empty);
    }
    if input.len() > ADDR_MAX_LEN {
        return Err(AddrError::TooLong);
    }

    if let Some(rest) = input.strip_prefix('[') {
        // IPv6 ブラケット表記: [addr]:port
        let close = rest.find(']').ok_or(AddrError::InvalidIpv6)?;
        let inner = &rest[..close];
        let after = &rest[close + 1..];
        let port_str = after.strip_prefix(':').ok_or(AddrError::MissingPort)?;
        let port = parse_port(port_str)?;
        let ip: Ipv6Addr = inner.parse().map_err(|_| AddrError::InvalidIpv6)?;
        return Ok(PeerAddr {
            host: ip.to_string(),
            port,
            is_ipv6: true,
        });
    }

    // ブラケットなし: コロンはちょうど 1 個(ポート区切り)でなければならない。
    let colons = input.bytes().filter(|&b| b == b':').count();
    if colons == 0 {
        return Err(AddrError::MissingPort);
    }
    if colons > 1 {
        return Err(AddrError::AmbiguousColons);
    }
    let (host, port_str) = input.rsplit_once(':').ok_or(AddrError::MissingPort)?;
    if host.is_empty() {
        return Err(AddrError::InvalidHost);
    }
    if host.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(AddrError::InvalidHost);
    }
    let port = parse_port(port_str)?;
    Ok(PeerAddr {
        host: host.to_ascii_lowercase(),
        port,
        is_ipv6: false,
    })
}

fn parse_port(s: &str) -> std::result::Result<u16, AddrError> {
    let port: u16 = s.parse().map_err(|_| AddrError::InvalidPort)?;
    if port == 0 {
        return Err(AddrError::InvalidPort);
    }
    Ok(port)
}

// ---------------------------------------------------------------------------
// 設定・エラー・列挙
// ---------------------------------------------------------------------------

/// ピア管理の設定(既定は contracts / data-model の値)。
///
/// Settings からの実値注入は T020 の責務。本モジュールは `config.rs` に結合しない。
#[derive(Debug, Clone)]
pub struct PeerManagerConfig {
    /// 維持する外向き接続数の目標。
    pub outbound_target: usize,
    /// 着信接続の上限。
    pub inbound_max: usize,
    /// 平常時候補から降格する連続失敗閾値。
    pub fail_demote_threshold: i64,
    /// バックオフ初期値(秒)。
    pub backoff_initial_secs: u64,
    /// バックオフ係数。
    pub backoff_factor: u64,
    /// バックオフ上限(秒)。
    pub backoff_cap_secs: u64,
    /// 未検証候補への接続試行の最小間隔(秒)。
    pub new_candidate_min_interval_secs: f64,
}

impl Default for PeerManagerConfig {
    fn default() -> Self {
        Self {
            outbound_target: DEFAULT_OUTBOUND_TARGET,
            inbound_max: DEFAULT_INBOUND_MAX,
            fail_demote_threshold: DEFAULT_FAIL_DEMOTE_THRESHOLD,
            backoff_initial_secs: DEFAULT_BACKOFF_INITIAL_SECS,
            backoff_factor: DEFAULT_BACKOFF_FACTOR,
            backoff_cap_secs: DEFAULT_BACKOFF_CAP_SECS,
            new_candidate_min_interval_secs: DEFAULT_NEW_CANDIDATE_MIN_INTERVAL_SECS,
        }
    }
}

/// ピア管理操作のエラー。
#[derive(Debug)]
pub enum PeerError {
    /// アドレス検証違反。
    Addr(AddrError),
    /// 自ノード自身のアドレス(登録拒否)。
    SelfAddress,
    /// 永続化(ストア)エラー。
    Store(StoreError),
}

impl std::fmt::Display for PeerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PeerError::Addr(e) => write!(f, "{e}"),
            PeerError::SelfAddress => f.write_str("自ノード自身のアドレスは登録できません"),
            PeerError::Store(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for PeerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PeerError::Addr(e) => Some(e),
            PeerError::Store(e) => Some(e),
            PeerError::SelfAddress => None,
        }
    }
}

impl From<StoreError> for PeerError {
    fn from(e: StoreError) -> Self {
        PeerError::Store(e)
    }
}

/// `Result` 別名。
pub type Result<T> = std::result::Result<T, PeerError>;

/// 候補選定のモード。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateMode {
    /// 平常時: 降格済み(fail_count ≥ 閾値)ピアは最下位へ沈める(除外はしない)。
    Normal,
    /// 全ピア到達不能時(US3 シナリオ 3): 降格を無視して通常の候補順で全 enabled ピアを対象にする。
    AllUnreachable,
}

/// 外向き接続開始の判定結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboundOutcome {
    /// 開始してよい(接続枠を確保した)。
    Started,
    /// 同一アドレスへ既に接続中(多重接続の統合 — 開始しない)。
    AlreadyActive,
    /// 自ノード自身のアドレス。
    SelfAddress,
    /// 未検証候補への接続試行が 1 件/秒の上限に達している。
    Throttled,
}

/// 着信接続受理の判定結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboundOutcome {
    /// 受理してよい(接続枠を確保した)。
    Accepted,
    /// 同一アドレスから既に接続中。
    AlreadyActive,
    /// 着信上限に達している(HELLO_ACK 前に CLOSE すべき)。
    AtCapacity,
}

// ---------------------------------------------------------------------------
// PeerManager
// ---------------------------------------------------------------------------

/// メモリ上の接続状態(自己アドレス集合・接続中アドレス・未検証候補スロットル)。
struct Inner {
    /// 自ノード自身と判明したアドレス(自己接続検出で観測 — nonce 一致)。
    self_addrs: HashSet<String>,
    /// 接続中の外向きアドレス(正規表記)。
    outbound: HashSet<String>,
    /// 接続中の着信アドレス(正規表記)。
    inbound: HashSet<String>,
    /// 未検証候補への直近の接続試行時刻(秒)。
    last_new_dial: Option<f64>,
}

/// ピア管理。`Arc<PeerManager>` として非同期タスク・接続ドライバ(T020/T047)で共有する。
pub struct PeerManager {
    store: std::sync::Arc<Store>,
    config: PeerManagerConfig,
    inner: Mutex<Inner>,
    clock: Box<dyn Fn() -> f64 + Send + Sync>,
}

/// 既定クロック: 生成時点からの単調経過秒。
fn default_clock() -> Box<dyn Fn() -> f64 + Send + Sync> {
    let start = Instant::now();
    Box::new(move || start.elapsed().as_secs_f64())
}

/// ポイズン時も内部値を回収してロックを返す(パニックしない)。
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl PeerManager {
    /// 既定クロックで作成する。
    pub fn new(store: std::sync::Arc<Store>, config: PeerManagerConfig) -> Self {
        Self::with_clock(store, config, default_clock())
    }

    /// クロックを注入して作成する(テスト用)。
    pub fn with_clock(
        store: std::sync::Arc<Store>,
        config: PeerManagerConfig,
        clock: Box<dyn Fn() -> f64 + Send + Sync>,
    ) -> Self {
        Self {
            store,
            config,
            inner: Mutex::new(Inner {
                self_addrs: HashSet::new(),
                outbound: HashSet::new(),
                inbound: HashSet::new(),
                last_new_dial: None,
            }),
            clock,
        }
    }

    /// 設定への参照。
    pub fn config(&self) -> &PeerManagerConfig {
        &self.config
    }

    // -------------------------------------------------------------- 自己アドレス

    /// 自ノード自身と判明したアドレスを登録する(以後の登録・候補選定から除外)。
    ///
    /// 既定 bind は `0.0.0.0:7147` で外部から観測される自アドレスと一致しないため、
    /// 自己検出は主に nonce 駆動([`PeerManager::note_self_connect`])で行う。
    /// 不正なアドレスは静かに無視する(自己集合は安全側の除外にのみ用いるため)。
    pub fn add_self_addr(&self, addr: &str) {
        if let Ok(parsed) = parse_addr(addr) {
            lock(&self.inner).self_addrs.insert(parsed.canonical());
        }
    }

    /// 正規表記が自ノード自身のアドレスか。
    pub fn is_self_addr(&self, canonical: &str) -> bool {
        lock(&self.inner).self_addrs.contains(canonical)
    }

    /// 自己接続を検出した(HELLO の nonce が自ノードと一致 — [`crate::p2p::session`] が判定)。
    ///
    /// 観測した相手アドレスを自己集合へ記録し、ストアの当該エントリを削除(登録拒否)する。
    /// 削除できたら `true`。アドレスが不正なら何もせず `false`。
    pub fn note_self_connect(&self, addr: &str) -> Result<bool> {
        let parsed = match parse_addr(addr) {
            Ok(p) => p,
            Err(_) => return Ok(false),
        };
        let canonical = parsed.canonical();
        lock(&self.inner).self_addrs.insert(canonical.clone());
        Ok(self.store.delete_peer(&canonical)?)
    }

    // -------------------------------------------------------------- 登録(CRUD)

    /// ピア候補を登録する(手動シードまたは PEX 獲得)。
    ///
    /// アドレスを検証・正規化し、自ノード自身のアドレスは拒否する。永続化と LRU 降格・
    /// pex→manual 昇格はストアが行う。
    pub fn add_peer(&self, addr: &str, source: PeerSource) -> Result<PeerEndpoint> {
        let parsed = parse_addr(addr).map_err(PeerError::Addr)?;
        let canonical = parsed.canonical();
        if self.is_self_addr(&canonical) {
            return Err(PeerError::SelfAddress);
        }
        Ok(self.store.upsert_peer(&canonical, source)?)
    }

    /// アドレスでピアを取得する(正規化して照会)。
    pub fn get_peer(&self, addr: &str) -> Result<Option<PeerEndpoint>> {
        let canonical = parse_addr(addr).map_err(PeerError::Addr)?.canonical();
        Ok(self.store.get_peer(&canonical)?)
    }

    /// 全ピアを列挙する。
    pub fn list_peers(&self) -> Result<Vec<PeerEndpoint>> {
        Ok(self.store.list_peers()?)
    }

    /// ピアの有効/無効を設定する。
    pub fn set_enabled(&self, addr: &str, enabled: bool) -> Result<bool> {
        let canonical = parse_addr(addr).map_err(PeerError::Addr)?.canonical();
        Ok(self.store.set_peer_enabled(&canonical, enabled)?)
    }

    /// ピアを削除する。
    pub fn remove_peer(&self, addr: &str) -> Result<bool> {
        let canonical = parse_addr(addr).map_err(PeerError::Addr)?.canonical();
        Ok(self.store.delete_peer(&canonical)?)
    }

    /// 外向き接続の成功を記録する(verified=1・last_ok_at・fail_count=0 リセット)。
    pub fn record_success(&self, addr: &str, at: i64) -> Result<bool> {
        let canonical = parse_addr(addr).map_err(PeerError::Addr)?.canonical();
        Ok(self.store.record_peer_success(&canonical, at)?)
    }

    /// 接続失敗を記録し、更新後の fail_count を返す(ピアが無ければ `None`)。
    pub fn record_failure(&self, addr: &str) -> Result<Option<i64>> {
        let canonical = parse_addr(addr).map_err(PeerError::Addr)?.canonical();
        Ok(self.store.record_peer_failure(&canonical)?)
    }

    /// fail_count が平常時候補からの降格閾値に達しているか。
    pub fn is_demoted(&self, fail_count: i64) -> bool {
        fail_count >= self.config.fail_demote_threshold
    }

    // ---------------------------------------------------------------- 候補選定

    /// 外向き接続の候補を優先順に返す。
    ///
    /// 基本順序は **manual 優先 → last_ok_at 新しい順 → fail_count 少ない順**
    /// (contracts §接続管理)。`Normal` では降格済み(fail_count ≥ 閾値)ピアを最下位へ
    /// 沈める。`AllUnreachable` では降格を無視し全 enabled ピアを基本順で対象にする
    /// (US3 シナリオ 3 の全断時再試行)。
    ///
    /// 無効(enabled=0)・自ノード自身・現在接続中・ポート 0/解析不能のアドレスは除外する。
    pub fn candidates(&self, mode: CandidateMode) -> Result<Vec<PeerEndpoint>> {
        let all = self.store.list_peers()?;
        let (self_addrs, active): (HashSet<String>, HashSet<String>) = {
            let inner = lock(&self.inner);
            let active = inner
                .outbound
                .iter()
                .chain(inner.inbound.iter())
                .cloned()
                .collect();
            (inner.self_addrs.clone(), active)
        };

        let mut out: Vec<PeerEndpoint> = all
            .into_iter()
            .filter(|p| p.enabled)
            .filter(|p| !self_addrs.contains(&p.addr))
            .filter(|p| !active.contains(&p.addr))
            .filter(|p| parse_addr(&p.addr).is_ok())
            .collect();

        let threshold = self.config.fail_demote_threshold;
        out.sort_by(|a, b| {
            if mode == CandidateMode::Normal {
                let da = (a.fail_count >= threshold) as u8;
                let db = (b.fail_count >= threshold) as u8;
                match da.cmp(&db) {
                    std::cmp::Ordering::Equal => cmp_base(a, b),
                    non_eq => non_eq,
                }
            } else {
                cmp_base(a, b)
            }
        });
        Ok(out)
    }

    // -------------------------------------------------------- 接続枠の会計

    /// 現在の外向き接続数。
    pub fn outbound_count(&self) -> usize {
        lock(&self.inner).outbound.len()
    }

    /// 現在の着信接続数。
    pub fn inbound_count(&self) -> usize {
        lock(&self.inner).inbound.len()
    }

    /// 目標に対して不足している外向き接続数。
    pub fn outbound_deficit(&self) -> usize {
        self.config
            .outbound_target
            .saturating_sub(self.outbound_count())
    }

    /// 外向き接続の開始可否を判定し、開始してよければ接続枠を確保する。
    ///
    /// `verified` が false(未検証候補)の場合は 1 件/秒スロットルを適用する(反射攻撃緩和)。
    /// 自ノード自身・接続中(多重)は開始しない。
    pub fn begin_outbound(&self, addr: &str, verified: bool) -> OutboundOutcome {
        let canonical = match parse_addr(addr) {
            Ok(p) => p.canonical(),
            Err(_) => return OutboundOutcome::SelfAddress,
        };
        let now = (self.clock)();
        let mut inner = lock(&self.inner);
        if inner.self_addrs.contains(&canonical) {
            return OutboundOutcome::SelfAddress;
        }
        if inner.outbound.contains(&canonical) || inner.inbound.contains(&canonical) {
            return OutboundOutcome::AlreadyActive;
        }
        if !verified {
            let interval = self.config.new_candidate_min_interval_secs;
            if let Some(last) = inner.last_new_dial
                && now - last < interval
            {
                return OutboundOutcome::Throttled;
            }
            inner.last_new_dial = Some(now);
        }
        inner.outbound.insert(canonical);
        OutboundOutcome::Started
    }

    /// 外向き接続の終了を記録する(接続枠を解放)。
    pub fn end_outbound(&self, addr: &str) {
        if let Ok(p) = parse_addr(addr) {
            lock(&self.inner).outbound.remove(&p.canonical());
        }
    }

    /// 着信接続の受理可否を判定し、受理してよければ接続枠を確保する。
    ///
    /// 着信上限超過は `AtCapacity`(呼び出し側は HELLO_ACK 前に CLOSE する)。
    /// 同一アドレスからの多重着信は `AlreadyActive`。
    pub fn begin_inbound(&self, addr: &str) -> InboundOutcome {
        let canonical = match parse_addr(addr) {
            Ok(p) => p.canonical(),
            // 解析不能でも着信ソケットは存在するため、生の文字列で枠会計する。
            Err(_) => addr.to_string(),
        };
        let mut inner = lock(&self.inner);
        if inner.inbound.contains(&canonical) || inner.outbound.contains(&canonical) {
            return InboundOutcome::AlreadyActive;
        }
        if inner.inbound.len() >= self.config.inbound_max {
            return InboundOutcome::AtCapacity;
        }
        inner.inbound.insert(canonical);
        InboundOutcome::Accepted
    }

    /// 着信接続の終了を記録する(接続枠を解放)。
    pub fn end_inbound(&self, addr: &str) {
        let canonical = parse_addr(addr)
            .map(|p| p.canonical())
            .unwrap_or_else(|_| addr.to_string());
        lock(&self.inner).inbound.remove(&canonical);
    }

    // ------------------------------------------------------------- バックオフ

    /// 連続失敗回数に対する再接続バックオフ(秒)。
    ///
    /// `min(cap, initial * factor^(failures-1))`。失敗 0 回は 0(即時)。
    /// 決定的でジッタを持たない(contracts §セッション状態機械)。
    pub fn backoff_delay_secs(&self, consecutive_failures: u32) -> u64 {
        if consecutive_failures == 0 {
            return 0;
        }
        let mut delay = self.config.backoff_initial_secs;
        for _ in 1..consecutive_failures {
            delay = delay.saturating_mul(self.config.backoff_factor);
            if delay >= self.config.backoff_cap_secs {
                return self.config.backoff_cap_secs;
            }
        }
        delay.min(self.config.backoff_cap_secs)
    }
}

/// 基本候補順の比較: manual 優先 → last_ok_at 新しい順 → fail_count 少ない順 → id 昇順。
fn cmp_base(a: &PeerEndpoint, b: &PeerEndpoint) -> std::cmp::Ordering {
    let manual_rank = |p: &PeerEndpoint| {
        if p.source == PeerSource::Manual {
            0u8
        } else {
            1u8
        }
    };
    manual_rank(a)
        .cmp(&manual_rank(b))
        .then_with(|| {
            b.last_ok_at
                .unwrap_or(i64::MIN)
                .cmp(&a.last_ok_at.unwrap_or(i64::MIN))
        })
        .then_with(|| a.fail_count.cmp(&b.fail_count))
        .then_with(|| a.id.cmp(&b.id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn manager() -> PeerManager {
        let store = Arc::new(Store::open_in_memory().unwrap());
        PeerManager::new(store, PeerManagerConfig::default())
    }

    fn manager_with_clock(clock: Box<dyn Fn() -> f64 + Send + Sync>) -> PeerManager {
        let store = Arc::new(Store::open_in_memory().unwrap());
        PeerManager::with_clock(store, PeerManagerConfig::default(), clock)
    }

    // ---- アドレス検証 ----

    #[test]
    fn parse_ipv4_and_hostname() {
        let a = parse_addr("192.0.2.10:7147").unwrap();
        assert_eq!(a.canonical(), "192.0.2.10:7147");
        assert!(!a.is_ipv6);
        let h = parse_addr("Example.COM:7147").unwrap();
        assert_eq!(h.canonical(), "example.com:7147");
    }

    #[test]
    fn parse_ipv6_bracket_canonicalized() {
        let a = parse_addr("[2001:DB8::0:1]:7147").unwrap();
        assert!(a.is_ipv6);
        assert_eq!(a.canonical(), "[2001:db8::1]:7147");
    }

    #[test]
    fn reject_bracketless_multicolon() {
        assert_eq!(
            parse_addr("2001:db8::1:7147").unwrap_err(),
            AddrError::AmbiguousColons
        );
    }

    #[test]
    fn reject_bad_forms() {
        assert_eq!(parse_addr("").unwrap_err(), AddrError::Empty);
        assert_eq!(parse_addr("host").unwrap_err(), AddrError::MissingPort);
        assert_eq!(parse_addr("host:abc").unwrap_err(), AddrError::InvalidPort);
        assert_eq!(parse_addr("host:0").unwrap_err(), AddrError::InvalidPort);
        assert_eq!(
            parse_addr("host:70000").unwrap_err(),
            AddrError::InvalidPort
        );
        assert_eq!(parse_addr(":7147").unwrap_err(), AddrError::InvalidHost);
        assert_eq!(
            parse_addr("[zzzz]:7147").unwrap_err(),
            AddrError::InvalidIpv6
        );
        let long = format!("{}:7147", "a".repeat(ADDR_MAX_LEN));
        assert_eq!(parse_addr(&long).unwrap_err(), AddrError::TooLong);
    }

    // ---- 自己アドレス / 自己接続 ----

    #[test]
    fn self_address_registration_rejected() {
        let m = manager();
        m.add_self_addr("198.51.100.7:7147");
        assert!(m.is_self_addr("198.51.100.7:7147"));
        assert!(matches!(
            m.add_peer("198.51.100.7:7147", PeerSource::Pex)
                .unwrap_err(),
            PeerError::SelfAddress
        ));
    }

    #[test]
    fn note_self_connect_records_and_removes() {
        let m = manager();
        m.add_peer("198.51.100.8:7147", PeerSource::Pex).unwrap();
        assert!(m.note_self_connect("198.51.100.8:7147").unwrap());
        assert!(m.is_self_addr("198.51.100.8:7147"));
        assert!(m.get_peer("198.51.100.8:7147").unwrap().is_none());
        // 以後の登録も拒否される
        assert!(matches!(
            m.add_peer("198.51.100.8:7147", PeerSource::Pex)
                .unwrap_err(),
            PeerError::SelfAddress
        ));
    }

    // ---- 登録・正規化経由の CRUD ----

    #[test]
    fn add_peer_normalizes_before_store() {
        let m = manager();
        m.add_peer("[2001:DB8::1]:7147", PeerSource::Manual)
            .unwrap();
        // 別表記(完全展開形)でも同一エントリとして引ける(多重登録防止)
        let got = m.get_peer("[2001:0db8:0:0:0:0:0:1]:7147").unwrap().unwrap();
        assert_eq!(got.addr, "[2001:db8::1]:7147");
    }

    // ---- 候補選定 ----

    #[test]
    fn candidate_order_normal() {
        let m = manager();
        // manual(古い実績) と pex(新しい実績・失敗少) の順序
        m.add_peer("manual:7147", PeerSource::Manual).unwrap();
        m.record_success("manual:7147", 100).unwrap();
        m.add_peer("pexnew:7147", PeerSource::Pex).unwrap();
        m.record_success("pexnew:7147", 500).unwrap();
        m.add_peer("pexold:7147", PeerSource::Pex).unwrap();
        m.record_success("pexold:7147", 200).unwrap();

        let c = m.candidates(CandidateMode::Normal).unwrap();
        let addrs: Vec<&str> = c.iter().map(|p| p.addr.as_str()).collect();
        // manual 優先 → 残りは last_ok_at 新しい順
        assert_eq!(addrs, vec!["manual:7147", "pexnew:7147", "pexold:7147"]);
    }

    #[test]
    fn candidate_excludes_disabled_and_self_and_active() {
        let m = manager();
        m.add_peer("a:7147", PeerSource::Pex).unwrap();
        m.add_peer("b:7147", PeerSource::Pex).unwrap();
        m.add_peer("c:7147", PeerSource::Pex).unwrap();
        m.set_enabled("b:7147", false).unwrap();
        m.add_self_addr("c:7147");
        assert_eq!(m.begin_outbound("a:7147", false), OutboundOutcome::Started);
        let c = m.candidates(CandidateMode::Normal).unwrap();
        let addrs: Vec<&str> = c.iter().map(|p| p.addr.as_str()).collect();
        // a=接続中, b=無効, c=自己 → いずれも除外され空
        assert!(addrs.is_empty(), "残候補: {addrs:?}");
    }

    #[test]
    fn demoted_sinks_in_normal_but_present_in_all_unreachable() {
        let m = manager();
        // healthy(pex, 失敗0) と demoted(manual だが失敗8回)
        m.add_peer("healthy:7147", PeerSource::Pex).unwrap();
        m.record_success("healthy:7147", 100).unwrap();
        m.add_peer("demoted:7147", PeerSource::Manual).unwrap();
        for _ in 0..8 {
            m.record_failure("demoted:7147").unwrap();
        }
        let demoted = m.get_peer("demoted:7147").unwrap().unwrap();
        assert!(m.is_demoted(demoted.fail_count));

        // 平常時: 降格済み manual は healthy pex より下(不変条件: 降格は非降格の後)
        let normal = m.candidates(CandidateMode::Normal).unwrap();
        let n_addrs: Vec<&str> = normal.iter().map(|p| p.addr.as_str()).collect();
        assert_eq!(n_addrs, vec!["healthy:7147", "demoted:7147"]);
        // ただし除外はされない(両方存在する)
        assert_eq!(normal.len(), 2);

        // 全断時: 降格済みも含まれる(存在する)
        let all = m.candidates(CandidateMode::AllUnreachable).unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|p| p.addr == "demoted:7147"));
    }

    // ---- 接続枠 ----

    #[test]
    fn outbound_multiplicity_and_deficit() {
        let m = manager();
        assert_eq!(m.outbound_deficit(), DEFAULT_OUTBOUND_TARGET);
        assert_eq!(m.begin_outbound("p:7147", true), OutboundOutcome::Started);
        assert_eq!(
            m.begin_outbound("p:7147", true),
            OutboundOutcome::AlreadyActive
        );
        assert_eq!(m.outbound_count(), 1);
        assert_eq!(m.outbound_deficit(), DEFAULT_OUTBOUND_TARGET - 1);
        m.end_outbound("p:7147");
        assert_eq!(m.outbound_count(), 0);
    }

    #[test]
    fn self_address_blocks_outbound() {
        let m = manager();
        m.add_self_addr("self:7147");
        assert_eq!(
            m.begin_outbound("self:7147", true),
            OutboundOutcome::SelfAddress
        );
    }

    #[test]
    fn inbound_capacity_and_multiplicity() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let cfg = PeerManagerConfig {
            inbound_max: 2,
            ..PeerManagerConfig::default()
        };
        let m = PeerManager::new(store, cfg);
        assert_eq!(m.begin_inbound("a:1"), InboundOutcome::Accepted);
        assert_eq!(m.begin_inbound("a:1"), InboundOutcome::AlreadyActive);
        assert_eq!(m.begin_inbound("b:1"), InboundOutcome::Accepted);
        assert_eq!(m.begin_inbound("c:1"), InboundOutcome::AtCapacity);
        m.end_inbound("a:1");
        assert_eq!(m.begin_inbound("c:1"), InboundOutcome::Accepted);
    }

    // ---- 未検証候補スロットル(1 件/秒) ----

    #[test]
    fn new_candidate_throttled_to_one_per_second() {
        let clock = Arc::new(AtomicU64::new(0));
        let c2 = Arc::clone(&clock);
        let m = manager_with_clock(Box::new(move || c2.load(Ordering::SeqCst) as f64 / 1000.0));
        // t=0.0 未検証候補 → 開始
        assert_eq!(m.begin_outbound("n1:1", false), OutboundOutcome::Started);
        // t=0.5s 別の未検証候補 → スロットル
        clock.store(500, Ordering::SeqCst);
        assert_eq!(m.begin_outbound("n2:1", false), OutboundOutcome::Throttled);
        // 検証済みはスロットル対象外
        assert_eq!(m.begin_outbound("v1:1", true), OutboundOutcome::Started);
        // t=1.1s 未検証候補 → 開始
        clock.store(1100, Ordering::SeqCst);
        assert_eq!(m.begin_outbound("n3:1", false), OutboundOutcome::Started);
    }

    // ---- バックオフ ----

    #[test]
    fn backoff_is_deterministic_capped() {
        let m = manager();
        assert_eq!(m.backoff_delay_secs(0), 0);
        assert_eq!(m.backoff_delay_secs(1), 5);
        assert_eq!(m.backoff_delay_secs(2), 10);
        assert_eq!(m.backoff_delay_secs(3), 20);
        assert_eq!(m.backoff_delay_secs(6), 160);
        assert_eq!(m.backoff_delay_secs(7), 300); // 320 → 上限 300
        assert_eq!(m.backoff_delay_secs(100), 300);
    }
}

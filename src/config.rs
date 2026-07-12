//! 設定管理(T013)
//!
//! [`Settings`] は data-model §Settings の各キーを保持し、既定値を単一出典として持つ。
//!
//! - 読込([`Settings::load`])は settings テーブル(T012)から取得し、未保存・解釈不能な
//!   キーは既定値へフォールバックする(lenient)。
//! - 保存([`Settings::save`])は全キーを settings テーブルへ書き出す。
//! - 検証([`Settings::validate`])は唯一の厳格ゲート。**`pcp_bind` / `http_bind` は
//!   loopback アドレスのみ受理**し、非 loopback 値は拒否する(ADR-0006 決定 4。
//!   LAN 公開オプトインは v1 非実装)。`p2p_bind` は任意バインド可+空文字で待受無効。
//! - コマンドライン上書き([`CliOverrides`])は quickstart 手順 2 の同一 PC 多ノード起動用。
//!   外部クレートを増やさず std の args パースで実装する。

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use crate::store::{Store, StoreError};

// ---------------------------------------------------------------------------
// 既定値(data-model §Settings — 時刻関連定数の単一出典)
// ---------------------------------------------------------------------------

const DEFAULT_PCP_BIND: &str = "127.0.0.1:7146";
const DEFAULT_HTTP_BIND: &str = "127.0.0.1:7180";
const DEFAULT_P2P_BIND: &str = "0.0.0.0:7147,[::]:7147";
const DEFAULT_P2P_OUTBOUND_TARGET: u32 = 8;
const DEFAULT_P2P_INBOUND_MAX: u32 = 32;
const DEFAULT_PEX_ENABLED: bool = true;
const DEFAULT_UPNP_ENABLED: bool = true;
const DEFAULT_FRESHNESS_WINDOW_SEC: u64 = 600;
const DEFAULT_REPUBLISH_INTERVAL_SEC: u64 = 60;
const DEFAULT_MAX_CLOCK_SKEW_SEC: u64 = 300;
const DEFAULT_MIN_POW_BITS: u32 = 0;
const DEFAULT_EVENT_STORE_MAX: u64 = 4096;
// 2026-07-04 実機検証で改訂: 現行 YP は UTF-8 が既定(contracts/http-yp.md)。
const DEFAULT_INDEX_TXT_ENCODING: &str = "utf-8";
// 空文字 = index.txt の LAN 公開を無効(既定 — ADR-0012 はオプトイン)。
const DEFAULT_INDEX_BIND: &str = "";

// 006-livechat-thread data-model §Settings 追加分。
const DEFAULT_LIVECHAT_ENABLED: bool = true;
const DEFAULT_THREAD_MAX_PARTICIPANTS: u32 = 128;
// 窓は 30 秒固定(data-model §Settings)。値そのものは窓内のレス数上限で、
// 窓の長さは定数化せずここにコメントとして明記する(FR-021)。
const DEFAULT_THREAD_WRITE_RATE: u32 = 4;
// 接続あたり msg/秒(制御メッセージ込み — FR-021)。
const DEFAULT_THREAD_MSG_RATE: u32 = 16;
const DEFAULT_ANNOUNCE_STORE_QUOTA: u64 = 2048;
// 空文字 = 互換 API の待受無効(既定)。非空は loopback のみ受理(research R5)。
const DEFAULT_COMPAT_BBS_BIND: &str = "127.0.0.1:7183";

// settings テーブルのキー名(data-model §Settings と一致)。
const KEY_PCP_BIND: &str = "pcp_bind";
const KEY_HTTP_BIND: &str = "http_bind";
const KEY_P2P_BIND: &str = "p2p_bind";
const KEY_P2P_OUTBOUND_TARGET: &str = "p2p_outbound_target";
const KEY_P2P_INBOUND_MAX: &str = "p2p_inbound_max";
const KEY_PEX_ENABLED: &str = "pex_enabled";
const KEY_UPNP_ENABLED: &str = "upnp_enabled";
const KEY_FRESHNESS_WINDOW_SEC: &str = "freshness_window_sec";
const KEY_REPUBLISH_INTERVAL_SEC: &str = "republish_interval_sec";
const KEY_MAX_CLOCK_SKEW_SEC: &str = "max_clock_skew_sec";
const KEY_MIN_POW_BITS: &str = "min_pow_bits";
const KEY_EVENT_STORE_MAX: &str = "event_store_max";
const KEY_INDEX_TXT_ENCODING: &str = "index_txt_encoding";
const KEY_INDEX_BIND: &str = "index_bind";
const KEY_LIVECHAT_ENABLED: &str = "livechat_enabled";
const KEY_THREAD_MAX_PARTICIPANTS: &str = "thread_max_participants";
const KEY_THREAD_WRITE_RATE: &str = "thread_write_rate";
const KEY_THREAD_MSG_RATE: &str = "thread_msg_rate";
const KEY_ANNOUNCE_STORE_QUOTA: &str = "announce_store_quota";
const KEY_COMPAT_BBS_BIND: &str = "compat_bbs_bind";

// ---------------------------------------------------------------------------
// エラー
// ---------------------------------------------------------------------------

/// 設定エラー。`Display` は内部情報を漏らさない(Principle II)。
/// 設定キー名は利用者に返して差し支えない情報として含める。
#[derive(Debug)]
pub enum ConfigError {
    /// ストア操作の失敗。
    Store(StoreError),
    /// 非 loopback バインドの拒否(ADR-0006 決定 4)。
    NonLoopbackBind { key: &'static str },
    /// LAN 外バインドの拒否(ADR-0012。loopback / LAN 内プライベートアドレス以外)。
    NonLanBind { key: &'static str },
    /// バインドアドレスの書式不正。
    InvalidBind { key: &'static str },
    /// 不明なコマンドライン引数・値の欠落。
    InvalidArgument,
    /// index_txt_encoding が未対応の値。
    InvalidEncoding,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Store(_) => f.write_str("設定の読み書きに失敗しました"),
            ConfigError::NonLoopbackBind { key } => write!(
                f,
                "{key} は loopback アドレス(127.0.0.1 等)のみ指定できます"
            ),
            ConfigError::NonLanBind { key } => write!(
                f,
                "{key} は loopback または LAN 内のプライベートアドレスのみ指定できます"
            ),
            ConfigError::InvalidBind { key } => {
                write!(f, "{key} のアドレス書式が不正です")
            }
            ConfigError::InvalidArgument => f.write_str("コマンドライン引数が不正です"),
            ConfigError::InvalidEncoding => {
                f.write_str("index_txt_encoding は shift_jis または utf-8 を指定してください")
            }
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Store(e) => Some(e),
            _ => None,
        }
    }
}

impl From<StoreError> for ConfigError {
    fn from(e: StoreError) -> Self {
        ConfigError::Store(e)
    }
}

/// index.txt の出力エンコーディング。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexEncoding {
    ShiftJis,
    Utf8,
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// アプリケーション設定(data-model §Settings)。
///
/// バインド系はパース都合と「空文字で待受無効」の表現のため `String` で保持し、
/// [`Settings::pcp_addr`] 等でパース済みアドレスを得る。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub pcp_bind: String,
    pub http_bind: String,
    pub p2p_bind: String,
    pub p2p_outbound_target: u32,
    pub p2p_inbound_max: u32,
    pub pex_enabled: bool,
    pub upnp_enabled: bool,
    pub freshness_window_sec: u64,
    pub republish_interval_sec: u64,
    pub max_clock_skew_sec: u64,
    pub min_pow_bits: u32,
    pub event_store_max: u64,
    pub index_txt_encoding: String,
    /// index.txt の LAN 公開バインド先(ADR-0012)。空文字 = 機能無効(既定)。
    /// 非空時は loopback または LAN 内プライベートアドレスのみ受理する。
    pub index_bind: String,

    // --- 006-livechat-thread data-model §Settings 追加分 ---
    /// false でスレ機能全体を無効化する(announce は検証のみ行い不可視 — 006 data-model)。
    pub livechat_enabled: bool,
    /// ホストの受入接続上限。超過は定型拒否する(FR-006)。
    pub thread_max_participants: u32,
    /// 板鍵あたりの書き込みレート上限(FR-021)。窓は 30 秒固定、値は窓内のレス数上限。
    pub thread_write_rate: u32,
    /// 接続あたりの msg/秒上限(制御メッセージ込み — FR-021)。
    pub thread_msg_rate: u32,
    /// kind 31311(announce)の EventStore 独立保持枠(research R3)。
    pub announce_store_quota: u64,
    /// 互換 BBS API の待受アドレス(research R5)。既定は loopback で有効
    /// (`127.0.0.1:7183`)。空文字で機能無効、非空時は loopback のみ受理する。
    pub compat_bbs_bind: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            pcp_bind: DEFAULT_PCP_BIND.to_string(),
            http_bind: DEFAULT_HTTP_BIND.to_string(),
            p2p_bind: DEFAULT_P2P_BIND.to_string(),
            p2p_outbound_target: DEFAULT_P2P_OUTBOUND_TARGET,
            p2p_inbound_max: DEFAULT_P2P_INBOUND_MAX,
            pex_enabled: DEFAULT_PEX_ENABLED,
            upnp_enabled: DEFAULT_UPNP_ENABLED,
            freshness_window_sec: DEFAULT_FRESHNESS_WINDOW_SEC,
            republish_interval_sec: DEFAULT_REPUBLISH_INTERVAL_SEC,
            max_clock_skew_sec: DEFAULT_MAX_CLOCK_SKEW_SEC,
            min_pow_bits: DEFAULT_MIN_POW_BITS,
            event_store_max: DEFAULT_EVENT_STORE_MAX,
            index_txt_encoding: DEFAULT_INDEX_TXT_ENCODING.to_string(),
            index_bind: DEFAULT_INDEX_BIND.to_string(),
            livechat_enabled: DEFAULT_LIVECHAT_ENABLED,
            thread_max_participants: DEFAULT_THREAD_MAX_PARTICIPANTS,
            thread_write_rate: DEFAULT_THREAD_WRITE_RATE,
            thread_msg_rate: DEFAULT_THREAD_MSG_RATE,
            announce_store_quota: DEFAULT_ANNOUNCE_STORE_QUOTA,
            compat_bbs_bind: DEFAULT_COMPAT_BBS_BIND.to_string(),
        }
    }
}

impl Settings {
    /// settings テーブルから読み込む。未保存・解釈不能なキーは既定値へフォールバックする。
    pub fn load(store: &Store) -> Result<Self, ConfigError> {
        let stored = store.all_settings()?;
        let d = Settings::default();
        let s = |key: &str, default: &str| -> String {
            stored
                .get(key)
                .cloned()
                .unwrap_or_else(|| default.to_string())
        };
        Ok(Settings {
            pcp_bind: s(KEY_PCP_BIND, &d.pcp_bind),
            http_bind: s(KEY_HTTP_BIND, &d.http_bind),
            p2p_bind: s(KEY_P2P_BIND, &d.p2p_bind),
            p2p_outbound_target: parse_or(&stored, KEY_P2P_OUTBOUND_TARGET, d.p2p_outbound_target),
            p2p_inbound_max: parse_or(&stored, KEY_P2P_INBOUND_MAX, d.p2p_inbound_max),
            pex_enabled: parse_bool_or(&stored, KEY_PEX_ENABLED, d.pex_enabled),
            upnp_enabled: parse_bool_or(&stored, KEY_UPNP_ENABLED, d.upnp_enabled),
            freshness_window_sec: parse_or(
                &stored,
                KEY_FRESHNESS_WINDOW_SEC,
                d.freshness_window_sec,
            ),
            republish_interval_sec: parse_or(
                &stored,
                KEY_REPUBLISH_INTERVAL_SEC,
                d.republish_interval_sec,
            ),
            max_clock_skew_sec: parse_or(&stored, KEY_MAX_CLOCK_SKEW_SEC, d.max_clock_skew_sec),
            min_pow_bits: parse_or(&stored, KEY_MIN_POW_BITS, d.min_pow_bits),
            event_store_max: parse_or(&stored, KEY_EVENT_STORE_MAX, d.event_store_max),
            index_txt_encoding: s(KEY_INDEX_TXT_ENCODING, &d.index_txt_encoding),
            index_bind: s(KEY_INDEX_BIND, &d.index_bind),
            livechat_enabled: parse_bool_or(&stored, KEY_LIVECHAT_ENABLED, d.livechat_enabled),
            thread_max_participants: parse_or(
                &stored,
                KEY_THREAD_MAX_PARTICIPANTS,
                d.thread_max_participants,
            ),
            thread_write_rate: parse_or(&stored, KEY_THREAD_WRITE_RATE, d.thread_write_rate),
            thread_msg_rate: parse_or(&stored, KEY_THREAD_MSG_RATE, d.thread_msg_rate),
            announce_store_quota: parse_or(
                &stored,
                KEY_ANNOUNCE_STORE_QUOTA,
                d.announce_store_quota,
            ),
            compat_bbs_bind: s(KEY_COMPAT_BBS_BIND, &d.compat_bbs_bind),
        })
    }

    /// 全キーを settings テーブルへ書き出す。
    pub fn save(&self, store: &Store) -> Result<(), ConfigError> {
        store.set_setting(KEY_PCP_BIND, &self.pcp_bind)?;
        store.set_setting(KEY_HTTP_BIND, &self.http_bind)?;
        store.set_setting(KEY_P2P_BIND, &self.p2p_bind)?;
        store.set_setting(
            KEY_P2P_OUTBOUND_TARGET,
            &self.p2p_outbound_target.to_string(),
        )?;
        store.set_setting(KEY_P2P_INBOUND_MAX, &self.p2p_inbound_max.to_string())?;
        store.set_setting(KEY_PEX_ENABLED, bool_to_str(self.pex_enabled))?;
        store.set_setting(KEY_UPNP_ENABLED, bool_to_str(self.upnp_enabled))?;
        store.set_setting(
            KEY_FRESHNESS_WINDOW_SEC,
            &self.freshness_window_sec.to_string(),
        )?;
        store.set_setting(
            KEY_REPUBLISH_INTERVAL_SEC,
            &self.republish_interval_sec.to_string(),
        )?;
        store.set_setting(KEY_MAX_CLOCK_SKEW_SEC, &self.max_clock_skew_sec.to_string())?;
        store.set_setting(KEY_MIN_POW_BITS, &self.min_pow_bits.to_string())?;
        store.set_setting(KEY_EVENT_STORE_MAX, &self.event_store_max.to_string())?;
        store.set_setting(KEY_INDEX_TXT_ENCODING, &self.index_txt_encoding)?;
        store.set_setting(KEY_INDEX_BIND, &self.index_bind)?;
        store.set_setting(KEY_LIVECHAT_ENABLED, bool_to_str(self.livechat_enabled))?;
        store.set_setting(
            KEY_THREAD_MAX_PARTICIPANTS,
            &self.thread_max_participants.to_string(),
        )?;
        store.set_setting(KEY_THREAD_WRITE_RATE, &self.thread_write_rate.to_string())?;
        store.set_setting(KEY_THREAD_MSG_RATE, &self.thread_msg_rate.to_string())?;
        store.set_setting(
            KEY_ANNOUNCE_STORE_QUOTA,
            &self.announce_store_quota.to_string(),
        )?;
        store.set_setting(KEY_COMPAT_BBS_BIND, &self.compat_bbs_bind)?;
        Ok(())
    }

    /// コマンドライン上書きを適用する(指定されたキーのみ差し替え)。
    pub fn apply_overrides(&mut self, overrides: &CliOverrides) {
        if let Some(v) = &overrides.pcp_bind {
            self.pcp_bind = v.clone();
        }
        if let Some(v) = &overrides.http_bind {
            self.http_bind = v.clone();
        }
        if let Some(v) = &overrides.p2p_bind {
            self.p2p_bind = v.clone();
        }
        if let Some(v) = &overrides.index_bind {
            self.index_bind = v.clone();
        }
    }

    /// 設定の厳格検証(唯一のゲート)。
    ///
    /// - `pcp_bind` / `http_bind` は loopback アドレスのみ受理(非 loopback は拒否 —
    ///   ADR-0006 決定 4)。空・書式不正も拒否する。
    /// - `p2p_bind` は空文字(待受無効)を許容し、非空は任意アドレスとしてパース可能なこと。
    /// - `index_txt_encoding` は shift_jis / utf-8 のいずれか。
    /// - `index_bind` は空文字(機能無効)を許容し、非空は loopback / LAN 内
    ///   プライベートアドレスのみ受理(ADR-0012)。
    /// - `compat_bbs_bind` は空文字(機能無効)を許容し、非空は loopback のみ受理
    ///   (006-livechat-thread data-model §Settings — research R5)。
    pub fn validate(&self) -> Result<(), ConfigError> {
        require_loopback(KEY_PCP_BIND, &self.pcp_bind)?;
        require_loopback(KEY_HTTP_BIND, &self.http_bind)?;
        // p2p_bind: 空は待受無効、非空はカンマ区切り各要素がパース可能であること
        // (loopback 強制なし — ADR-0008)。
        parse_bind_list(KEY_P2P_BIND, &self.p2p_bind)?;
        self.index_encoding()?;
        // index_bind: 空は機能無効(検証スキップ)、非空は LAN/loopback 許可リスト検証。
        if !self.index_bind.is_empty() {
            require_lan_or_loopback(KEY_INDEX_BIND, &self.index_bind)?;
        }
        // compat_bbs_bind: 空は機能無効(検証スキップ)、非空は loopback のみ受理。
        if !self.compat_bbs_bind.is_empty() {
            require_loopback(KEY_COMPAT_BBS_BIND, &self.compat_bbs_bind)?;
        }
        Ok(())
    }

    /// PCP 待受アドレス(検証済み前提)。
    pub fn pcp_addr(&self) -> Result<SocketAddr, ConfigError> {
        parse_bind(KEY_PCP_BIND, &self.pcp_bind)
    }

    /// HTTP 待受アドレス(検証済み前提)。
    pub fn http_addr(&self) -> Result<SocketAddr, ConfigError> {
        parse_bind(KEY_HTTP_BIND, &self.http_bind)
    }

    /// P2P 待受アドレス列(ADR-0008)。カンマ区切りの各要素をパースする。
    /// 空文字・空要素のみ(待受無効=外向きのみ — FR-016)は空 `Vec`。
    pub fn p2p_addr(&self) -> Result<Vec<SocketAddr>, ConfigError> {
        parse_bind_list(KEY_P2P_BIND, &self.p2p_bind)
    }

    /// index.txt の出力エンコーディング。
    pub fn index_encoding(&self) -> Result<IndexEncoding, ConfigError> {
        match self.index_txt_encoding.as_str() {
            "shift_jis" => Ok(IndexEncoding::ShiftJis),
            "utf-8" | "utf8" => Ok(IndexEncoding::Utf8),
            _ => Err(ConfigError::InvalidEncoding),
        }
    }
}

// ---------------------------------------------------------------------------
// コマンドライン上書き
// ---------------------------------------------------------------------------

/// コマンドライン引数による上書き(quickstart 手順 2 の多ノード起動用)。
///
/// 対応フラグ: `--pcp-bind` / `--http-bind` / `--p2p-bind` / `--index-bind` / `--data-dir`。
/// `--key value` と `--key=value` の両形式を受理する。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CliOverrides {
    pub pcp_bind: Option<String>,
    pub http_bind: Option<String>,
    pub p2p_bind: Option<String>,
    /// index.txt の LAN 公開バインド先(ADR-0012)。検証は `Settings::validate` で実施。
    pub index_bind: Option<String>,
    /// データディレクトリ(`app.db` の配置先)。未指定ならプラットフォーム別の
    /// 既定パスを使う(解決順は `--help` / cli-config.md §1 を参照)。
    pub data_dir: Option<PathBuf>,
}

impl CliOverrides {
    /// 引数列(実行ファイル名を除く)をパースする。
    ///
    /// 未知のフラグ・値欠落は [`ConfigError::InvalidArgument`] で拒否する
    /// (タイプミスを黙って無視しない)。
    pub fn parse<I, S>(args: I) -> Result<Self, ConfigError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut out = CliOverrides::default();
        let mut iter = args.into_iter().map(Into::into);
        while let Some(arg) = iter.next() {
            let (key, inline_value) = match arg.split_once('=') {
                Some((k, v)) => (k.to_string(), Some(v.to_string())),
                None => (arg, None),
            };
            let value = match inline_value {
                Some(v) => v,
                None => iter.next().ok_or(ConfigError::InvalidArgument)?,
            };
            match key.as_str() {
                "--pcp-bind" => out.pcp_bind = Some(value),
                "--http-bind" => out.http_bind = Some(value),
                "--p2p-bind" => out.p2p_bind = Some(value),
                "--index-bind" => out.index_bind = Some(value),
                "--data-dir" => out.data_dir = Some(PathBuf::from(value)),
                _ => return Err(ConfigError::InvalidArgument),
            }
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// 内部ヘルパ
// ---------------------------------------------------------------------------

fn parse_or<T: std::str::FromStr>(
    stored: &std::collections::HashMap<String, String>,
    key: &str,
    default: T,
) -> T {
    stored
        .get(key)
        .and_then(|v| v.parse::<T>().ok())
        .unwrap_or(default)
}

fn parse_bool_or(
    stored: &std::collections::HashMap<String, String>,
    key: &str,
    default: bool,
) -> bool {
    match stored.get(key).map(String::as_str) {
        Some("1") | Some("true") => true,
        Some("0") | Some("false") => false,
        _ => default,
    }
}

fn bool_to_str(b: bool) -> &'static str {
    if b { "1" } else { "0" }
}

fn parse_bind(key: &'static str, value: &str) -> Result<SocketAddr, ConfigError> {
    value
        .parse::<SocketAddr>()
        .map_err(|_| ConfigError::InvalidBind { key })
}

/// カンマ区切りのバインドアドレス列をパースする(ADR-0008)。
///
/// 各要素は前後空白をトリムし、空要素(連続カンマ・末尾カンマ)は無視する。
/// 非空の要素が一つでもパース不能なら [`ConfigError::InvalidBind`]。
/// 空文字・空要素のみは空 `Vec`(待受無効の表現)。
fn parse_bind_list(key: &'static str, value: &str) -> Result<Vec<SocketAddr>, ConfigError> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| parse_bind(key, s))
        .collect()
}

/// loopback バインド強制(ADR-0006 決定 4)。非 loopback・書式不正・空は拒否。
fn require_loopback(key: &'static str, value: &str) -> Result<(), ConfigError> {
    let addr = parse_bind(key, value)?;
    if addr.ip().is_loopback() {
        Ok(())
    } else {
        Err(ConfigError::NonLoopbackBind { key })
    }
}

/// loopback または LAN 内プライベートアドレスのみを許す許可リスト検証(ADR-0012)。
///
/// index.txt の LAN 公開(オプトイン)で使う。**許可リスト方式**を採るのは、
/// unspecified(`0.0.0.0` / `::`)・グローバルユニキャスト・CGNAT(100.64.0.0/10)
/// といった LAN 外への露出を、個別列挙ではなく構造的に弾くため(Principle II)。
/// 誤って許可すると index.txt が意図せず外部へ露出しうるセキュリティ上重要な判定である。
///
/// - パース失敗(ポート欠落・カンマ区切り複数・非数値ゾーン ID 等)は既存
///   [`ConfigError::InvalidBind`] を返す(`http_bind` 等と同じ書式不正の扱い)。
/// - IP を [`IpAddr::to_canonical`] で正規化し、`::ffff:192.168.1.1` のような
///   v4-mapped 表記の判定漏れを防ぐ。
/// - 判定基準(すべて data-model §2 の判定テーブルにゴールデン/ネガティブで固定):
///   - IPv4: loopback / RFC 1918 プライベート / リンクローカル(169.254/16)
///   - IPv6: loopback / ULA(fc00::/7)/ リンクローカル(fe80::/10)。
///     `is_unique_local` 等の unstable/新しめ API に依存せず、上位セグメントの
///     ビット判定で固定する(MSRV 依存の揺れを避ける — research R1)。
/// - 上記いずれにも該当しなければ [`ConfigError::NonLanBind`] で拒否する。
fn require_lan_or_loopback(key: &'static str, value: &str) -> Result<(), ConfigError> {
    let addr = parse_bind(key, value)?;
    let allowed = match addr.ip().to_canonical() {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            // fc00::/7(ULA)と fe80::/10(リンクローカル)を上位セグメントの
            // ビットマスクで判定する。
            v6.is_loopback()
                || v6.segments()[0] & 0xfe00 == 0xfc00
                || v6.segments()[0] & 0xffc0 == 0xfe80
        }
    };
    if allowed {
        Ok(())
    } else {
        Err(ConfigError::NonLanBind { key })
    }
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    #[test]
    fn defaults_match_data_model() {
        let d = Settings::default();
        assert_eq!(d.pcp_bind, "127.0.0.1:7146");
        assert_eq!(d.http_bind, "127.0.0.1:7180");
        assert_eq!(d.p2p_bind, "0.0.0.0:7147,[::]:7147");
        assert_eq!(d.p2p_outbound_target, 8);
        assert_eq!(d.p2p_inbound_max, 32);
        assert!(d.pex_enabled);
        assert!(d.upnp_enabled);
        assert_eq!(d.freshness_window_sec, 600);
        assert_eq!(d.republish_interval_sec, 60);
        assert_eq!(d.max_clock_skew_sec, 300);
        assert_eq!(d.min_pow_bits, 0);
        assert_eq!(d.event_store_max, 4096);
        assert_eq!(d.index_txt_encoding, "utf-8");
        assert!(d.livechat_enabled);
        assert_eq!(d.thread_max_participants, 128);
        assert_eq!(d.thread_write_rate, 4);
        assert_eq!(d.thread_msg_rate, 16);
        assert_eq!(d.announce_store_quota, 2048);
        assert_eq!(d.compat_bbs_bind, "127.0.0.1:7183");
    }

    #[test]
    fn defaults_are_valid() {
        assert!(Settings::default().validate().is_ok());
    }

    #[test]
    fn load_returns_defaults_when_empty() {
        let store = Store::open_in_memory().unwrap();
        let s = Settings::load(&store).unwrap();
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn save_then_load_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        let s = Settings {
            p2p_bind: "0.0.0.0:7157".to_string(),
            pex_enabled: false,
            min_pow_bits: 12,
            event_store_max: 8192,
            index_txt_encoding: "utf-8".to_string(),
            livechat_enabled: false,
            thread_max_participants: 64,
            thread_write_rate: 8,
            thread_msg_rate: 32,
            announce_store_quota: 4096,
            compat_bbs_bind: "127.0.0.2:7183".to_string(),
            ..Default::default()
        };
        s.save(&store).unwrap();
        let loaded = Settings::load(&store).unwrap();
        assert_eq!(loaded, s);
    }

    #[test]
    fn load_falls_back_on_unparseable_value() {
        let store = Store::open_in_memory().unwrap();
        store
            .set_setting("p2p_outbound_target", "not-a-number")
            .unwrap();
        store.set_setting("pex_enabled", "maybe").unwrap();
        let s = Settings::load(&store).unwrap();
        assert_eq!(s.p2p_outbound_target, 8); // 既定へフォールバック
        assert!(s.pex_enabled); // 既定へフォールバック
    }

    // --- loopback 強制(ADR-0006 決定 4)------------------------------------

    #[test]
    fn loopback_binds_accepted() {
        for addr in ["127.0.0.1:7146", "127.0.0.2:7146", "[::1]:7146"] {
            let s = Settings {
                pcp_bind: addr.to_string(),
                http_bind: addr.to_string(),
                ..Default::default()
            };
            assert!(s.validate().is_ok(), "{addr} は許容されるべき");
        }
    }

    #[test]
    fn non_loopback_pcp_bind_rejected() {
        let s = Settings {
            pcp_bind: "0.0.0.0:7146".to_string(),
            ..Default::default()
        };
        assert!(matches!(
            s.validate(),
            Err(ConfigError::NonLoopbackBind { key: "pcp_bind" })
        ));
    }

    #[test]
    fn non_loopback_http_bind_rejected() {
        let s = Settings {
            http_bind: "192.168.1.10:7180".to_string(),
            ..Default::default()
        };
        assert!(matches!(
            s.validate(),
            Err(ConfigError::NonLoopbackBind { key: "http_bind" })
        ));
    }

    #[test]
    fn empty_loopback_bind_rejected() {
        // pcp/http の空文字は待受無効を意味しない(loopback バインド必須)
        let s = Settings {
            pcp_bind: String::new(),
            ..Default::default()
        };
        assert!(matches!(
            s.validate(),
            Err(ConfigError::InvalidBind { key: "pcp_bind" })
        ));
    }

    #[test]
    fn p2p_bind_allows_any_and_empty() {
        // 非 loopback でも p2p は許容(単一)
        let mut s = Settings {
            p2p_bind: "0.0.0.0:7147".to_string(),
            ..Default::default()
        };
        assert!(s.validate().is_ok());
        assert_eq!(s.p2p_addr().unwrap(), vec!["0.0.0.0:7147".parse().unwrap()]);
        // 空文字 = 待受無効(空 Vec)
        s.p2p_bind = String::new();
        assert!(s.validate().is_ok());
        assert!(s.p2p_addr().unwrap().is_empty());
        // 書式不正は拒否
        s.p2p_bind = "not-an-addr".to_string();
        assert!(matches!(
            s.validate(),
            Err(ConfigError::InvalidBind { key: "p2p_bind" })
        ));
    }

    #[test]
    fn p2p_bind_parses_comma_separated_list() {
        // カンマ区切りで IPv4/IPv6 デュアルスタック(ADR-0008 決定1)
        let s = Settings {
            p2p_bind: "0.0.0.0:7147,[::]:7147".to_string(),
            ..Default::default()
        };
        assert!(s.validate().is_ok());
        assert_eq!(
            s.p2p_addr().unwrap(),
            vec![
                "0.0.0.0:7147".parse().unwrap(),
                "[::]:7147".parse().unwrap(),
            ]
        );
    }

    #[test]
    fn p2p_bind_trims_whitespace_and_skips_empty_elements() {
        // 前後空白のトリムと、連続カンマ・末尾カンマの空要素無視
        let s = Settings {
            p2p_bind: " 0.0.0.0:7147 , ,[::]:7147,".to_string(),
            ..Default::default()
        };
        assert!(s.validate().is_ok());
        assert_eq!(
            s.p2p_addr().unwrap(),
            vec![
                "0.0.0.0:7147".parse().unwrap(),
                "[::]:7147".parse().unwrap(),
            ]
        );
    }

    #[test]
    fn p2p_bind_rejects_when_any_element_invalid() {
        // 1 要素でもパース不能なら全体を InvalidBind として拒否
        let s = Settings {
            p2p_bind: "0.0.0.0:7147,not-an-addr".to_string(),
            ..Default::default()
        };
        assert!(matches!(
            s.validate(),
            Err(ConfigError::InvalidBind { key: "p2p_bind" })
        ));
    }

    // --- require_lan_or_loopback(data-model §2 判定テーブルと 1:1)-----------

    #[test]
    fn lan_or_loopback_accepts_loopback() {
        // loopback(v4/v6)
        for addr in ["127.0.0.1:7180", "[::1]:7180"] {
            assert!(
                require_lan_or_loopback("index_bind", addr).is_ok(),
                "{addr} は受理されるべき(loopback)"
            );
        }
    }

    #[test]
    fn lan_or_loopback_accepts_rfc1918() {
        // RFC 1918 各ブロック + 172.16/12 の上端境界
        for addr in [
            "192.168.1.10:7180",
            "10.0.0.5:7180",
            "172.16.0.1:7180",
            "172.31.255.254:7180",
        ] {
            assert!(
                require_lan_or_loopback("index_bind", addr).is_ok(),
                "{addr} は受理されるべき(RFC 1918)"
            );
        }
    }

    #[test]
    fn lan_or_loopback_accepts_link_local() {
        // IPv4 リンクローカル + IPv6 リンクローカル(fe80::/10)と上端境界
        for addr in ["169.254.10.1:7180", "[fe80::1]:7180", "[febf:ffff::1]:7180"] {
            assert!(
                require_lan_or_loopback("index_bind", addr).is_ok(),
                "{addr} は受理されるべき(リンクローカル)"
            );
        }
    }

    #[test]
    fn lan_or_loopback_accepts_numeric_zone_id() {
        // 数値ゾーン ID 付きリンクローカル(std がパース可能なら検証通過)
        assert!(
            require_lan_or_loopback("index_bind", "[fe80::1%3]:7180").is_ok(),
            "数値ゾーン ID 付きリンクローカルは受理されるべき"
        );
    }

    #[test]
    fn lan_or_loopback_accepts_ula() {
        // IPv6 ULA(fc00::/7)と上端境界
        for addr in [
            "[fd12:3456::1]:7180",
            "[fc00::1]:7180",
            "[fdff:ffff::1]:7180",
        ] {
            assert!(
                require_lan_or_loopback("index_bind", addr).is_ok(),
                "{addr} は受理されるべき(ULA)"
            );
        }
    }

    #[test]
    fn lan_or_loopback_accepts_v4_mapped() {
        // v4-mapped(canonical 化で private 判定)
        assert!(
            require_lan_or_loopback("index_bind", "[::ffff:192.168.1.10]:7180").is_ok(),
            "v4-mapped private は受理されるべき"
        );
    }

    #[test]
    fn lan_or_loopback_rejects_unspecified() {
        for addr in ["0.0.0.0:7180", "[::]:7180"] {
            assert!(
                matches!(
                    require_lan_or_loopback("index_bind", addr),
                    Err(ConfigError::NonLanBind { key: "index_bind" })
                ),
                "{addr} は NonLanBind で拒否されるべき(unspecified)"
            );
        }
    }

    #[test]
    fn lan_or_loopback_rejects_global() {
        for addr in ["203.0.113.5:7180", "[2001:db8::1]:7180"] {
            assert!(
                matches!(
                    require_lan_or_loopback("index_bind", addr),
                    Err(ConfigError::NonLanBind { key: "index_bind" })
                ),
                "{addr} は NonLanBind で拒否されるべき(グローバル)"
            );
        }
    }

    #[test]
    fn lan_or_loopback_rejects_cgnat() {
        // CGNAT / 共有アドレス空間(100.64/10)と直前(100.63.255.254 = グローバル扱い)
        for addr in [
            "100.64.0.1:7180",
            "100.127.255.254:7180",
            "100.63.255.254:7180",
        ] {
            assert!(
                matches!(
                    require_lan_or_loopback("index_bind", addr),
                    Err(ConfigError::NonLanBind { key: "index_bind" })
                ),
                "{addr} は NonLanBind で拒否されるべき(CGNAT/境界)"
            );
        }
    }

    #[test]
    fn lan_or_loopback_rejects_out_of_range() {
        // RFC 1918 の境界外・リンクローカルの直外・ULA の直外
        for addr in ["172.32.0.1:7180", "[fec0::1]:7180", "[fe00::1]:7180"] {
            assert!(
                matches!(
                    require_lan_or_loopback("index_bind", addr),
                    Err(ConfigError::NonLanBind { key: "index_bind" })
                ),
                "{addr} は NonLanBind で拒否されるべき(境界外)"
            );
        }
    }

    #[test]
    fn lan_or_loopback_rejects_malformed() {
        // ポート欠落・カンマ区切り複数・非数値ゾーン ID(std パース不可)・空白
        for addr in [
            "192.168.1.10",
            "192.168.1.10:7180,10.0.0.5:7180",
            "[fe80::1%eth0]:7180",
            "  ",
        ] {
            assert!(
                matches!(
                    require_lan_or_loopback("index_bind", addr),
                    Err(ConfigError::InvalidBind { key: "index_bind" })
                ),
                "{addr} は InvalidBind で拒否されるべき(書式不正)"
            );
        }
    }

    #[test]
    fn invalid_encoding_rejected() {
        let s = Settings {
            index_txt_encoding: "euc-jp".to_string(),
            ..Default::default()
        };
        assert!(matches!(s.validate(), Err(ConfigError::InvalidEncoding)));
        assert_eq!(
            Settings::default().index_encoding().unwrap(),
            IndexEncoding::Utf8
        );
    }

    // --- CLI 上書き --------------------------------------------------------

    #[test]
    fn cli_parse_space_and_equals_forms() {
        let o = CliOverrides::parse([
            "--p2p-bind",
            "0.0.0.0:7157",
            "--http-bind=127.0.0.1:7190",
            "--pcp-bind",
            "127.0.0.1:7156",
            "--data-dir=C:/tmp/nodeB",
        ])
        .unwrap();
        assert_eq!(o.p2p_bind.as_deref(), Some("0.0.0.0:7157"));
        assert_eq!(o.http_bind.as_deref(), Some("127.0.0.1:7190"));
        assert_eq!(o.pcp_bind.as_deref(), Some("127.0.0.1:7156"));
        assert_eq!(o.data_dir, Some(PathBuf::from("C:/tmp/nodeB")));
    }

    #[test]
    fn cli_parse_index_bind_both_forms() {
        let o = CliOverrides::parse(["--index-bind", "192.168.1.10:7180"]).unwrap();
        assert_eq!(o.index_bind.as_deref(), Some("192.168.1.10:7180"));
        let o = CliOverrides::parse(["--index-bind=192.168.1.10:7180"]).unwrap();
        assert_eq!(o.index_bind.as_deref(), Some("192.168.1.10:7180"));
    }

    #[test]
    fn index_bind_empty_skips_validation() {
        // 既定(空文字)は機能無効 — 検証をスキップして通過する
        assert!(Settings::default().validate().is_ok());
    }

    #[test]
    fn index_bind_rejects_non_lan_via_validate() {
        let s = Settings {
            index_bind: "0.0.0.0:7180".to_string(),
            ..Default::default()
        };
        assert!(matches!(
            s.validate(),
            Err(ConfigError::NonLanBind { key: "index_bind" })
        ));
    }

    #[test]
    fn index_bind_accepts_lan_via_validate() {
        let s = Settings {
            index_bind: "192.168.1.10:7180".to_string(),
            ..Default::default()
        };
        assert!(s.validate().is_ok());
    }

    // --- compat_bbs_bind(loopback 強制。006-livechat-thread data-model §Settings)---

    #[test]
    fn compat_bbs_bind_empty_skips_validation() {
        // 空文字は機能無効 — 非 loopback でも検証をスキップして通過する
        let s = Settings {
            compat_bbs_bind: String::new(),
            ..Default::default()
        };
        assert!(s.validate().is_ok());
    }

    #[test]
    fn compat_bbs_bind_accepts_loopback() {
        for addr in ["127.0.0.1:7183", "[::1]:7183"] {
            let s = Settings {
                compat_bbs_bind: addr.to_string(),
                ..Default::default()
            };
            assert!(s.validate().is_ok(), "{addr} は許容されるべき");
        }
    }

    #[test]
    fn compat_bbs_bind_rejects_non_loopback() {
        let s = Settings {
            compat_bbs_bind: "0.0.0.0:7183".to_string(),
            ..Default::default()
        };
        assert!(matches!(
            s.validate(),
            Err(ConfigError::NonLoopbackBind {
                key: "compat_bbs_bind"
            })
        ));
    }

    #[test]
    fn cli_empty_args_is_default() {
        let o = CliOverrides::parse(Vec::<String>::new()).unwrap();
        assert_eq!(o, CliOverrides::default());
    }

    #[test]
    fn cli_unknown_flag_rejected() {
        assert!(matches!(
            CliOverrides::parse(["--bogus", "x"]),
            Err(ConfigError::InvalidArgument)
        ));
    }

    #[test]
    fn cli_missing_value_rejected() {
        assert!(matches!(
            CliOverrides::parse(["--p2p-bind"]),
            Err(ConfigError::InvalidArgument)
        ));
    }

    #[test]
    fn apply_overrides_replaces_only_specified() {
        let mut s = Settings::default();
        let o = CliOverrides {
            p2p_bind: Some("0.0.0.0:7157".to_string()),
            http_bind: Some("127.0.0.1:7190".to_string()),
            pcp_bind: None,
            index_bind: None,
            data_dir: None,
        };
        s.apply_overrides(&o);
        assert_eq!(s.p2p_bind, "0.0.0.0:7157");
        assert_eq!(s.http_bind, "127.0.0.1:7190");
        assert_eq!(s.pcp_bind, "127.0.0.1:7146"); // 未指定は既定のまま
    }
}

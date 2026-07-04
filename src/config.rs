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

use std::net::SocketAddr;
use std::path::PathBuf;

use crate::store::{Store, StoreError};

// ---------------------------------------------------------------------------
// 既定値(data-model §Settings — 時刻関連定数の単一出典)
// ---------------------------------------------------------------------------

const DEFAULT_PCP_BIND: &str = "127.0.0.1:7146";
const DEFAULT_HTTP_BIND: &str = "127.0.0.1:7180";
const DEFAULT_P2P_BIND: &str = "0.0.0.0:7147";
const DEFAULT_P2P_OUTBOUND_TARGET: u32 = 8;
const DEFAULT_P2P_INBOUND_MAX: u32 = 32;
const DEFAULT_PEX_ENABLED: bool = true;
const DEFAULT_UPNP_ENABLED: bool = true;
const DEFAULT_FRESHNESS_WINDOW_SEC: u64 = 600;
const DEFAULT_REPUBLISH_INTERVAL_SEC: u64 = 60;
const DEFAULT_MAX_CLOCK_SKEW_SEC: u64 = 300;
const DEFAULT_MIN_POW_BITS: u32 = 0;
const DEFAULT_EVENT_STORE_MAX: u64 = 4096;
const DEFAULT_INDEX_TXT_ENCODING: &str = "shift_jis";

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
        }
    }
}

impl Settings {
    /// settings テーブルから読み込む。未保存・解釈不能なキーは既定値へフォールバックする。
    pub fn load(store: &Store) -> Result<Self, ConfigError> {
        let stored = store.all_settings()?;
        let d = Settings::default();
        let s = |key: &str, default: &str| -> String {
            stored.get(key).cloned().unwrap_or_else(|| default.to_string())
        };
        Ok(Settings {
            pcp_bind: s(KEY_PCP_BIND, &d.pcp_bind),
            http_bind: s(KEY_HTTP_BIND, &d.http_bind),
            p2p_bind: s(KEY_P2P_BIND, &d.p2p_bind),
            p2p_outbound_target: parse_or(&stored, KEY_P2P_OUTBOUND_TARGET, d.p2p_outbound_target),
            p2p_inbound_max: parse_or(&stored, KEY_P2P_INBOUND_MAX, d.p2p_inbound_max),
            pex_enabled: parse_bool_or(&stored, KEY_PEX_ENABLED, d.pex_enabled),
            upnp_enabled: parse_bool_or(&stored, KEY_UPNP_ENABLED, d.upnp_enabled),
            freshness_window_sec: parse_or(&stored, KEY_FRESHNESS_WINDOW_SEC, d.freshness_window_sec),
            republish_interval_sec: parse_or(
                &stored,
                KEY_REPUBLISH_INTERVAL_SEC,
                d.republish_interval_sec,
            ),
            max_clock_skew_sec: parse_or(&stored, KEY_MAX_CLOCK_SKEW_SEC, d.max_clock_skew_sec),
            min_pow_bits: parse_or(&stored, KEY_MIN_POW_BITS, d.min_pow_bits),
            event_store_max: parse_or(&stored, KEY_EVENT_STORE_MAX, d.event_store_max),
            index_txt_encoding: s(KEY_INDEX_TXT_ENCODING, &d.index_txt_encoding),
        })
    }

    /// 全キーを settings テーブルへ書き出す。
    pub fn save(&self, store: &Store) -> Result<(), ConfigError> {
        store.set_setting(KEY_PCP_BIND, &self.pcp_bind)?;
        store.set_setting(KEY_HTTP_BIND, &self.http_bind)?;
        store.set_setting(KEY_P2P_BIND, &self.p2p_bind)?;
        store.set_setting(KEY_P2P_OUTBOUND_TARGET, &self.p2p_outbound_target.to_string())?;
        store.set_setting(KEY_P2P_INBOUND_MAX, &self.p2p_inbound_max.to_string())?;
        store.set_setting(KEY_PEX_ENABLED, bool_to_str(self.pex_enabled))?;
        store.set_setting(KEY_UPNP_ENABLED, bool_to_str(self.upnp_enabled))?;
        store.set_setting(KEY_FRESHNESS_WINDOW_SEC, &self.freshness_window_sec.to_string())?;
        store.set_setting(
            KEY_REPUBLISH_INTERVAL_SEC,
            &self.republish_interval_sec.to_string(),
        )?;
        store.set_setting(KEY_MAX_CLOCK_SKEW_SEC, &self.max_clock_skew_sec.to_string())?;
        store.set_setting(KEY_MIN_POW_BITS, &self.min_pow_bits.to_string())?;
        store.set_setting(KEY_EVENT_STORE_MAX, &self.event_store_max.to_string())?;
        store.set_setting(KEY_INDEX_TXT_ENCODING, &self.index_txt_encoding)?;
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
    }

    /// 設定の厳格検証(唯一のゲート)。
    ///
    /// - `pcp_bind` / `http_bind` は loopback アドレスのみ受理(非 loopback は拒否 —
    ///   ADR-0006 決定 4)。空・書式不正も拒否する。
    /// - `p2p_bind` は空文字(待受無効)を許容し、非空は任意アドレスとしてパース可能なこと。
    /// - `index_txt_encoding` は shift_jis / utf-8 のいずれか。
    pub fn validate(&self) -> Result<(), ConfigError> {
        require_loopback(KEY_PCP_BIND, &self.pcp_bind)?;
        require_loopback(KEY_HTTP_BIND, &self.http_bind)?;
        // p2p_bind: 空は待受無効、非空はパース可能であること(loopback 強制なし)
        if !self.p2p_bind.is_empty() && self.p2p_bind.parse::<SocketAddr>().is_err() {
            return Err(ConfigError::InvalidBind { key: KEY_P2P_BIND });
        }
        self.index_encoding()?;
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

    /// P2P 待受アドレス。空文字(待受無効)は `None`。
    pub fn p2p_addr(&self) -> Result<Option<SocketAddr>, ConfigError> {
        if self.p2p_bind.is_empty() {
            return Ok(None);
        }
        parse_bind(KEY_P2P_BIND, &self.p2p_bind).map(Some)
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
/// 対応フラグ: `--pcp-bind` / `--http-bind` / `--p2p-bind` / `--data-dir`。
/// `--key value` と `--key=value` の両形式を受理する。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CliOverrides {
    pub pcp_bind: Option<String>,
    pub http_bind: Option<String>,
    pub p2p_bind: Option<String>,
    /// データディレクトリ(`app.db` の配置先)。未指定なら `%APPDATA%` を使う。
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
    if b {
        "1"
    } else {
        "0"
    }
}

fn parse_bind(key: &'static str, value: &str) -> Result<SocketAddr, ConfigError> {
    value
        .parse::<SocketAddr>()
        .map_err(|_| ConfigError::InvalidBind { key })
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
        assert_eq!(d.p2p_bind, "0.0.0.0:7147");
        assert_eq!(d.p2p_outbound_target, 8);
        assert_eq!(d.p2p_inbound_max, 32);
        assert!(d.pex_enabled);
        assert!(d.upnp_enabled);
        assert_eq!(d.freshness_window_sec, 600);
        assert_eq!(d.republish_interval_sec, 60);
        assert_eq!(d.max_clock_skew_sec, 300);
        assert_eq!(d.min_pow_bits, 0);
        assert_eq!(d.event_store_max, 4096);
        assert_eq!(d.index_txt_encoding, "shift_jis");
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
            ..Default::default()
        };
        s.save(&store).unwrap();
        let loaded = Settings::load(&store).unwrap();
        assert_eq!(loaded, s);
    }

    #[test]
    fn load_falls_back_on_unparseable_value() {
        let store = Store::open_in_memory().unwrap();
        store.set_setting("p2p_outbound_target", "not-a-number").unwrap();
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
        // 非 loopback でも p2p は許容
        let mut s = Settings {
            p2p_bind: "0.0.0.0:7147".to_string(),
            ..Default::default()
        };
        assert!(s.validate().is_ok());
        assert_eq!(s.p2p_addr().unwrap(), Some("0.0.0.0:7147".parse().unwrap()));
        // 空文字 = 待受無効
        s.p2p_bind = String::new();
        assert!(s.validate().is_ok());
        assert_eq!(s.p2p_addr().unwrap(), None);
        // 書式不正は拒否
        s.p2p_bind = "not-an-addr".to_string();
        assert!(matches!(
            s.validate(),
            Err(ConfigError::InvalidBind { key: "p2p_bind" })
        ));
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
            IndexEncoding::ShiftJis
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
            data_dir: None,
        };
        s.apply_overrides(&o);
        assert_eq!(s.p2p_bind, "0.0.0.0:7157");
        assert_eq!(s.http_bind, "127.0.0.1:7190");
        assert_eq!(s.pcp_bind, "127.0.0.1:7146"); // 未指定は既定のまま
    }
}

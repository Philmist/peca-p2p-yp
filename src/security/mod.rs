//! セキュリティ共通部(T014)
//!
//! - 入力検証ヘルパ(サイズ・制御文字・URL 警告判定 — FR-012)
//! - SecurityEvent カテゴリの一元定義(data-model.md §SecurityEvent を正とする全 21 カテゴリ。
//!   うち 6 件は 006-livechat-thread data-model §SecurityEvent 追加カテゴリ — T008)
//! - セキュリティイベントログ: サイズローテーション(10MB × 5 世代)+
//!   同一 `(category, source)` の高頻度イベントの 10 秒間隔件数集約
//!   (ログ洪水によるディスク枯渇の防止 — constitution §Security Requirements, Principle II)
//!
//! `detail` に内部情報(スタックトレース・ファイルパス)を含めてはならない (MUST NOT)。

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// セキュリティイベントのカテゴリ(data-model.md §SecurityEvent カテゴリ一覧(全量))。
///
/// 各契約書のログ名はここに列挙されたものだけを使う(FR-007)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SecurityCategory {
    /// PCP 入力検証違反(contracts/pcp-announce.md)
    PcpReject,
    /// フレーム/JSON/順序違反(contracts/p2p-gossip.md 検査 3、状態機械違反)
    P2pInvalidFrame,
    /// フレーム長超過(同 検査 1)
    P2pOversize,
    /// 受信レート・SYNC 応答量超過(同 検査 2・6)
    P2pRateLimited,
    /// イベントサイズ超過(contracts/nostr-events.md 検証 1)
    EventOversize,
    /// 署名・id 検証失敗(同 検証 2 — FR-005)
    EventInvalidSig,
    /// kind/タグ形式・内容範囲違反(同 検証 3・5)
    EventInvalidFormat,
    /// 時刻ずれ許容超過(同 検証 4)
    EventTimeSkew,
    /// PoW 難易度不足(同 検証 6、min_pow_bits > 0 時)
    EventPowInsufficient,
    /// PEERS 内容違反(contracts/p2p-gossip.md 検査 5)
    PexRejected,
    /// HTTP レート制限超過(contracts/http-yp.md・local-api.md)
    HttpRateLimited,
    /// URL 警告判定の発動(FR-012)
    UrlWarning,
    /// 保管物の緩いパーミッションを自動是正した(unix — FR-013 / cli-config §4)
    KeyPermissionFixed,
    /// 保管物のパーミッションを是正できず影響ペルソナを利用不可とした(unix — FR-013)
    KeyPermissionUnfixable,
    /// index.txt を LAN へ公開した(ADR-0012)。既存カテゴリと異なり「入力違反の拒否」
    /// ではなく、**利用者が明示的に選んだ露出状態の監査**である。起動時に非 loopback かつ
    /// bind 成功のとき 1 件だけ記録する(loopback 値・bind 失敗・機能無効では記録しない)。
    IndexTxtLanExposed,
    /// announce(kind 31311)の署名者がチャンネル掲載ペルソナと不一致・形式違反
    /// (006-livechat-thread data-model §SecurityEvent — FR-003)
    LivechatAnnounceInvalid,
    /// 接続時チャレンジの検証失敗(切断 + バックオフ — FR-005)
    LivechatChallengeFailed,
    /// スレ主以外の鍵で署名された順序確定情報(kind 21311 — FR-011)
    LivechatOrderInvalid,
    /// サイズ・形式・PoW・レート違反の書き込み(ホスト側)。BAN による採番拒否は
    /// 記録するが応答では理由を開示しない(FR-007/FR-021)
    LivechatWriteRejected,
    /// 検証に失敗する板設定の受信(FR-025)
    LivechatSettingsInvalid,
    /// 互換 API への loopback 外アクセス・Host 検証失敗・レート違反(FR-026)
    CompatBbsDenied,
}

impl SecurityCategory {
    /// 全カテゴリ(データモデルの全量 21 件)。リリース前ゲート(T035)の一致確認に使う。
    pub const ALL: [SecurityCategory; 21] = [
        SecurityCategory::PcpReject,
        SecurityCategory::P2pInvalidFrame,
        SecurityCategory::P2pOversize,
        SecurityCategory::P2pRateLimited,
        SecurityCategory::EventOversize,
        SecurityCategory::EventInvalidSig,
        SecurityCategory::EventInvalidFormat,
        SecurityCategory::EventTimeSkew,
        SecurityCategory::EventPowInsufficient,
        SecurityCategory::PexRejected,
        SecurityCategory::HttpRateLimited,
        SecurityCategory::UrlWarning,
        SecurityCategory::KeyPermissionFixed,
        SecurityCategory::KeyPermissionUnfixable,
        SecurityCategory::IndexTxtLanExposed,
        SecurityCategory::LivechatAnnounceInvalid,
        SecurityCategory::LivechatChallengeFailed,
        SecurityCategory::LivechatOrderInvalid,
        SecurityCategory::LivechatWriteRejected,
        SecurityCategory::LivechatSettingsInvalid,
        SecurityCategory::CompatBbsDenied,
    ];

    /// ログに書き出すカテゴリ名(data-model.md の表記と一致させる)。
    pub fn as_str(self) -> &'static str {
        match self {
            SecurityCategory::PcpReject => "pcp_reject",
            SecurityCategory::P2pInvalidFrame => "p2p_invalid_frame",
            SecurityCategory::P2pOversize => "p2p_oversize",
            SecurityCategory::P2pRateLimited => "p2p_rate_limited",
            SecurityCategory::EventOversize => "event_oversize",
            SecurityCategory::EventInvalidSig => "event_invalid_sig",
            SecurityCategory::EventInvalidFormat => "event_invalid_format",
            SecurityCategory::EventTimeSkew => "event_time_skew",
            SecurityCategory::EventPowInsufficient => "event_pow_insufficient",
            SecurityCategory::PexRejected => "pex_rejected",
            SecurityCategory::HttpRateLimited => "http_rate_limited",
            SecurityCategory::UrlWarning => "url_warning",
            SecurityCategory::KeyPermissionFixed => "key_permission_fixed",
            SecurityCategory::KeyPermissionUnfixable => "key_permission_unfixable",
            SecurityCategory::IndexTxtLanExposed => "index_txt_lan_exposed",
            SecurityCategory::LivechatAnnounceInvalid => "livechat_announce_invalid",
            SecurityCategory::LivechatChallengeFailed => "livechat_challenge_failed",
            SecurityCategory::LivechatOrderInvalid => "livechat_order_invalid",
            SecurityCategory::LivechatWriteRejected => "livechat_write_rejected",
            SecurityCategory::LivechatSettingsInvalid => "livechat_settings_invalid",
            SecurityCategory::CompatBbsDenied => "compat_bbs_denied",
        }
    }
}

impl std::fmt::Display for SecurityCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// 入力検証ヘルパ
// ---------------------------------------------------------------------------

/// バイト長が上限を超えているか。
pub fn exceeds_bytes(s: &str, max: usize) -> bool {
    s.len() > max
}

/// 制御文字(Unicode Cc — 0x00..0x1F, 0x7F, U+0080..U+009F)を除去する。
pub fn strip_control_chars(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// 制御文字を除去するが、Markdown の意味を持つ改行(`\n`)とタブ(`\t`)は残す。
///
/// `strip_control_chars` は `char::is_control()` が `true` を返す `\n`/`\r`/`\t` も
/// 削ってしまうため、Markdown 原文(板のローカルルール — data-model §BoardSettings)を
/// 正規化する用途には使えない。ここでは行構造を保つため `\n` と `\t` を残し、
/// `\r` は除去して CRLF を LF へ正規化する。描画時の XSS 対策は
/// [`crate::web::livechat::render_local_rules_html`] 側が担う。
pub fn strip_control_chars_keep_markdown(s: &str) -> String {
    s.chars()
        .filter(|&c| !c.is_control() || c == '\n' || c == '\t')
        .collect()
}

/// 制御文字を含むか。
pub fn contains_control_chars(s: &str) -> bool {
    s.chars().any(|c| c.is_control())
}

/// コンタクト URL の警告判定(FR-012)。
///
/// scheme が http/https 以外なら true(警告)。空文字列は URL なしとして警告しない。
pub fn url_needs_warning(url: &str) -> bool {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    !(lower.starts_with("http://") || lower.starts_with("https://"))
}

/// 指定長の小文字 hex 文字列か(イベント id・pubkey・チャンネル GUID の形式検証用)。
pub fn is_lower_hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

// ---------------------------------------------------------------------------
// セキュリティイベントログ
// ---------------------------------------------------------------------------

/// 既定のローテーション: 1 ファイル 10MB × 5 世代(data-model.md §SecurityEvent)。
const DEFAULT_MAX_BYTES: u64 = 10 * 1024 * 1024;
const DEFAULT_GENERATIONS: usize = 5;
/// 同一 (category, source) の集約間隔(秒)。
const AGGREGATION_WINDOW_SECS: u64 = 10;

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// サイズ上限つきローテーションファイル。
/// `base`(例 `security.log`)が上限を超えると `.1`〜`.{gen-1}` へ繰り上げ、最古を削除する。
struct RotatingFile {
    base: PathBuf,
    max_bytes: u64,
    generations: usize,
    current_len: u64,
}

impl RotatingFile {
    fn new(base: PathBuf, max_bytes: u64, generations: usize) -> io::Result<Self> {
        if let Some(parent) = base.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let current_len = fs::metadata(&base).map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            base,
            max_bytes,
            generations,
            current_len,
        })
    }

    fn generation_path(&self, index: usize) -> PathBuf {
        if index == 0 {
            self.base.clone()
        } else {
            let mut os = self.base.clone().into_os_string();
            os.push(format!(".{index}"));
            PathBuf::from(os)
        }
    }

    fn rotate(&mut self) -> io::Result<()> {
        let _ = fs::remove_file(self.generation_path(self.generations - 1));
        for i in (1..self.generations).rev() {
            let from = self.generation_path(i - 1);
            if from.exists() {
                let _ = fs::rename(&from, self.generation_path(i));
            }
        }
        self.current_len = 0;
        Ok(())
    }

    fn write_line(&mut self, line: &str) -> io::Result<()> {
        let bytes = line.len() as u64 + 1;
        if self.current_len > 0 && self.current_len + bytes > self.max_bytes {
            self.rotate()?;
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.base)?;
        use io::Write as _;
        writeln!(file, "{line}")?;
        self.current_len += bytes;
        Ok(())
    }
}

struct AggregationWindow {
    started_at: u64,
    suppressed: u64,
}

struct LogInner {
    file: RotatingFile,
    windows: HashMap<(SecurityCategory, String), AggregationWindow>,
    now: Box<dyn Fn() -> u64 + Send>,
}

/// セキュリティイベントログ(スレッド安全)。
///
/// 1 行 1 イベントの JSON Lines をローテーションファイルへ追記し、同一
/// `(category, source)` の高頻度イベントは 10 秒間隔で件数集約する。
pub struct SecurityLog {
    inner: Mutex<LogInner>,
}

impl SecurityLog {
    /// 既定設定(10MB × 5 世代・実時刻)で作成する。
    pub fn new(log_path: impl Into<PathBuf>) -> io::Result<Self> {
        Self::with_options(
            log_path,
            DEFAULT_MAX_BYTES,
            DEFAULT_GENERATIONS,
            Box::new(unix_now),
        )
    }

    /// ローテーション条件・時刻源を指定して作成する(テスト用)。
    pub fn with_options(
        log_path: impl Into<PathBuf>,
        max_bytes: u64,
        generations: usize,
        now: Box<dyn Fn() -> u64 + Send>,
    ) -> io::Result<Self> {
        Ok(Self {
            inner: Mutex::new(LogInner {
                file: RotatingFile::new(log_path.into(), max_bytes, generations.max(1))?,
                windows: HashMap::new(),
                now,
            }),
        })
    }

    /// セキュリティイベントを記録する。
    ///
    /// `detail` に内部情報(スタックトレース・パス)を含めてはならない (MUST NOT — Principle II)。
    pub fn log(&self, category: SecurityCategory, source: &str, detail: &str) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        let now = (inner.now)();
        Self::flush_expired(&mut inner, now);

        let key = (category, source.to_string());
        match inner.windows.get_mut(&key) {
            Some(window) if now.saturating_sub(window.started_at) < AGGREGATION_WINDOW_SECS => {
                window.suppressed += 1;
            }
            _ => {
                tracing::warn!(
                    target: "security",
                    category = category.as_str(),
                    source,
                    detail,
                    "security event"
                );
                let line = serde_json::json!({
                    "ts": now,
                    "category": category.as_str(),
                    "source": source,
                    "detail": detail,
                })
                .to_string();
                let _ = inner.file.write_line(&line);
                inner.windows.insert(
                    key,
                    AggregationWindow {
                        started_at: now,
                        suppressed: 0,
                    },
                );
            }
        }
    }

    /// 期限切れの集約ウィンドウを明示的に書き出す(定期呼び出し・終了時用)。
    pub fn flush(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            let now = (inner.now)();
            Self::flush_expired(&mut inner, now);
        }
    }

    fn flush_expired(inner: &mut LogInner, now: u64) {
        let expired: Vec<((SecurityCategory, String), u64)> = inner
            .windows
            .iter()
            .filter(|(_, w)| now.saturating_sub(w.started_at) >= AGGREGATION_WINDOW_SECS)
            .map(|(k, w)| (k.clone(), w.suppressed))
            .collect();
        for ((category, source), suppressed) in expired {
            inner.windows.remove(&(category, source.clone()));
            if suppressed > 0 {
                let line = serde_json::json!({
                    "ts": now,
                    "category": category.as_str(),
                    "source": source,
                    "aggregated_count": suppressed,
                })
                .to_string();
                let _ = inner.file.write_line(&line);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn all_21_categories_have_unique_names() {
        let names: HashSet<&str> = SecurityCategory::ALL.iter().map(|c| c.as_str()).collect();
        assert_eq!(names.len(), 21);
        assert!(names.contains("pcp_reject"));
        assert!(names.contains("p2p_invalid_frame"));
        assert!(names.contains("p2p_oversize"));
        assert!(names.contains("p2p_rate_limited"));
        assert!(names.contains("event_oversize"));
        assert!(names.contains("event_invalid_sig"));
        assert!(names.contains("event_invalid_format"));
        assert!(names.contains("event_time_skew"));
        assert!(names.contains("event_pow_insufficient"));
        assert!(names.contains("pex_rejected"));
        assert!(names.contains("http_rate_limited"));
        assert!(names.contains("url_warning"));
        assert!(names.contains("key_permission_fixed"));
        assert!(names.contains("key_permission_unfixable"));
        assert!(names.contains("index_txt_lan_exposed"));
        assert!(names.contains("livechat_announce_invalid"));
        assert!(names.contains("livechat_challenge_failed"));
        assert!(names.contains("livechat_order_invalid"));
        assert!(names.contains("livechat_write_rejected"));
        assert!(names.contains("livechat_settings_invalid"));
        assert!(names.contains("compat_bbs_denied"));
    }

    #[test]
    fn url_warning_flags_non_http_schemes() {
        assert!(!url_needs_warning("http://example.com/"));
        assert!(!url_needs_warning("HTTPS://example.com/"));
        assert!(!url_needs_warning(""));
        assert!(!url_needs_warning("   "));
        assert!(url_needs_warning("ftp://example.com/"));
        assert!(url_needs_warning("javascript:alert(1)"));
        assert!(url_needs_warning("example.com/no-scheme"));
    }

    #[test]
    fn control_chars_are_stripped() {
        assert_eq!(strip_control_chars("a\x00b\x1fc\x7fd"), "abcd");
        assert_eq!(strip_control_chars("改行\nタブ\t"), "改行タブ");
        assert!(contains_control_chars("a\nb"));
        assert!(!contains_control_chars("普通の文字列"));
    }

    #[test]
    fn markdown_keep_preserves_newlines_and_tabs() {
        // Markdown 用は \n / \t を残し、他の制御文字は除去する。
        assert_eq!(
            strip_control_chars_keep_markdown("改行\nタブ\t"),
            "改行\nタブ\t"
        );
        assert_eq!(
            strip_control_chars_keep_markdown("a\x00b\x1fc\x7fd"),
            "abcd"
        );
        // \r は除去され CRLF は LF へ正規化される。
        assert_eq!(
            strip_control_chars_keep_markdown("# 見出し\r\n\r\n本文"),
            "# 見出し\n\n本文"
        );
    }

    #[test]
    fn byte_length_and_hex_helpers() {
        assert!(!exceeds_bytes("abc", 3));
        assert!(exceeds_bytes("abcd", 3));
        // マルチバイトはバイト長で判定する
        assert!(exceeds_bytes("あい", 5));
        assert!(is_lower_hex("0123456789abcdef0123456789abcdef", 32));
        assert!(!is_lower_hex("0123456789ABCDEF0123456789ABCDEF", 32));
        assert!(!is_lower_hex("0123", 32));
        assert!(!is_lower_hex("xyz", 3));
    }

    #[test]
    fn log_rotation_keeps_bounded_generations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("security.log");
        let log = SecurityLog::with_options(&path, 200, 3, Box::new(|| 0)).unwrap();
        // 集約を避けるため source を変えながら大量に書く
        for i in 0..50 {
            log.log(
                SecurityCategory::P2pOversize,
                &format!("198.51.100.{i}:7147"),
                "frame too large",
            );
        }
        assert!(path.exists());
        assert!(dir.path().join("security.log.1").exists());
        assert!(dir.path().join("security.log.2").exists());
        assert!(!dir.path().join("security.log.3").exists());
    }

    #[test]
    fn high_frequency_events_are_aggregated_per_10s() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("security.log");
        let clock = Arc::new(AtomicU64::new(1000));
        let clock2 = Arc::clone(&clock);
        let log = SecurityLog::with_options(
            &path,
            DEFAULT_MAX_BYTES,
            DEFAULT_GENERATIONS,
            Box::new(move || clock2.load(Ordering::SeqCst)),
        )
        .unwrap();

        // t=1000: 初回は即時記録、以降 10 秒未満は抑制
        log.log(SecurityCategory::PcpReject, "127.0.0.1:5000", "bad atom");
        for i in 1..=5 {
            clock.store(1000 + i, Ordering::SeqCst);
            log.log(SecurityCategory::PcpReject, "127.0.0.1:5000", "bad atom");
        }
        // t=1015: ウィンドウ満了 → 集約行 + 新規行
        clock.store(1015, Ordering::SeqCst);
        log.log(SecurityCategory::PcpReject, "127.0.0.1:5000", "bad atom");

        let content = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3, "初回 + 集約 + 新規 = 3 行: {content}");
        let aggregated: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(aggregated["aggregated_count"], 5);
        assert_eq!(aggregated["category"], "pcp_reject");
    }

    #[test]
    fn distinct_sources_are_not_aggregated_together() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("security.log");
        let log = SecurityLog::with_options(
            &path,
            DEFAULT_MAX_BYTES,
            DEFAULT_GENERATIONS,
            Box::new(|| 1000),
        )
        .unwrap();
        log.log(SecurityCategory::PexRejected, "peer-a:7147", "bad addr");
        log.log(SecurityCategory::PexRejected, "peer-b:7147", "bad addr");
        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content.lines().count(), 2);
    }
}

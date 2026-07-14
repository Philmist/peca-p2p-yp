//! dat 出力(T055 — contracts/compat-api.md §GET /{board}/dat/{key}.dat)
//!
//! 1 レス 1 行の従来形式(`名前<>メール<>日付 ID:xxxxxxxx<>本文<>スレタイトル`)で
//! **確定済みレスのみ**を出力する。エスケープは一意規則(`&` `<` `>` `"` の順で実体参照・
//! 改行 `<br>`)で固定し、板鍵由来の短縮 ID(先頭 8 文字・表示専用)を付す。
//!
//! ## dat 追記不変性(MUST — contracts/compat-api.md §HTTP メタデータ)
//!
//! 一度応答した dat のバイト列は、以後の応答で**接頭辞として不変**でなければならない。
//! 本モジュールはこれを構造的に満たす: 確定レスは [`crate::livechat::thread::Thread::confirm`]
//! が res_no 順で不変条件 T3(欠番なし単調増加)を強制するため追記のみで並びが変わらず、
//! 各レスの表現(名前・エスケープ後の本文等)は本モジュールの純粋関数のみで決まり
//! **外部状態(現在の板設定・NG 等)に一切依存しない**。名無し名は
//! [`crate::livechat::registry::LivechatRegistry::accept_write`]/`seed_confirmed_res` が
//! **確定処理そのもの**で `Res::name` へ焼き込み済み(FR-023/FR-024 — 板設定変更を
//! 遡及させない基盤)。本モジュールは `res.name` を経由の判定なしにそのまま使うだけで
//! よく(常に `Some`)、`noname_name` を引数として受け取る必要がない(T055 レビュー対応 —
//! 従来は dat 出力時に *現在の* noname_name を都度解決しており、板主の設定変更が
//! 配信済み dat を書き換えてしまう MUST 違反を構造的に抱えていた)。

use crate::livechat::thread::{Res, Thread};

/// 1 レスぶんの dat 行を組み立てる(確定済みレスのみが呼び出し対象)。
///
/// `res.name` は確定処理([`crate::livechat::registry::LivechatRegistry::accept_write`])
/// が既に「当該レス確定時点の名無し名」まで解決済みの値(常に `Some`)。本関数は
/// 追加のフォールバック判定を行わず、`res.name` をそのままエスケープして使う
/// (外部の現行板設定に依存しないことが dat 追記不変性の根拠)。`thread_title` は
/// 1 レス目の行にのみ載せる(2 行目以降は空文字列を渡すこと)。
pub fn format_line(res: &Res, thread_title: &str) -> String {
    let name = escape(res.name.as_deref().unwrap_or_default());
    let mail = escape(res.mail.as_deref().unwrap_or_default());
    let date = format_date(res.created_at);
    let short_id = short_id(&res.board_key);
    let body = escape_body(&res.body);
    let title = escape(thread_title);
    format!("{name}<>{mail}<>{date} ID:{short_id}<>{body}<>{title}\n")
}

/// スレ全体の dat 本文を組み立てる(確定済みレスのみ・res_no 昇順)。
///
/// [`Thread::res`] は既に確定順(res_no 昇順・欠番なし — 不変条件 T3)で保持されている
/// ため、フィルタは「確定済み(`res_no.is_some()`)」のみでよい(未確定の「送信中」投稿は
/// ホスト側の [`Thread`] には元々含まれない — 確定して初めて `res` へ入る)。
pub fn render(thread: &Thread) -> String {
    let mut out = String::new();
    for (i, res) in thread.res.iter().enumerate() {
        let title = if i == 0 { thread.title.as_str() } else { "" };
        out.push_str(&format_line(res, title));
    }
    out
}

/// 板鍵(hex 64 桁)から表示用の短縮 ID(先頭 8 文字)を導出する(表示専用 — FR-018)。
///
/// 完全鍵照合(NG/BAN)には使わない([`crate::livechat::moderation::Moderation`] が別途
/// 完全一致で判定する)。板単位で固定であり日替わりしない(従来の日替わり ID と異なる
/// 挙動 — spec Assumptions)。
pub fn short_id(board_key: &str) -> &str {
    let end = board_key.len().min(8);
    &board_key[..end]
}

/// 投稿時刻(unix 秒)を `YYYY/MM/DD(曜) HH:MM:SS` 形式へフォーマットする(ローカル時刻に
/// 依存しない UTC 固定表示 — 検証容易性とタイムゾーン非依存を優先する)。
fn format_date(unix_secs: i64) -> String {
    let (y, mo, d, wd, h, mi, s) = civil_from_unix(unix_secs);
    const WEEKDAYS: [&str; 7] = ["日", "月", "火", "水", "木", "金", "土"];
    format!(
        "{y:04}/{mo:02}/{d:02}({weekday}) {h:02}:{mi:02}:{s:02}",
        weekday = WEEKDAYS[wd as usize]
    )
}

/// unix 秒を UTC のグレゴリオ暦(年・月・日・曜日 0=日曜・時・分・秒)へ変換する。
///
/// Howard Hinnant の `civil_from_days` アルゴリズム(公開ドメイン)を用いる。標準ライブラリ
/// に日付計算 API がないため、新規クレートを増やさず自前実装する(依存を最小に保つ方針)。
fn civil_from_unix(unix_secs: i64) -> (i64, u32, u32, u32, u32, u32, u32) {
    let days = unix_secs.div_euclid(86400);
    let secs_of_day = unix_secs.rem_euclid(86400);
    let h = (secs_of_day / 3600) as u32;
    let mi = ((secs_of_day % 3600) / 60) as u32;
    let s = (secs_of_day % 60) as u32;
    // 1970-01-01 は木曜(weekday index 4)。
    let weekday = ((days % 7 + 7 + 4) % 7) as u32;

    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };

    (y, m, d, weekday, h, mi, s)
}

/// 一意規則のエスケープ(`&` `<` `>` `"` の順)。名前・メール・タイトルに適用する。
///
/// 順序固定が重要: `&` を最後に処理すると、後続文字の実体参照(`&lt;` 等)を二重
/// エスケープしてしまう。`&` を最初に処理することでこれを避ける(一意規則)。
///
/// `pub`: subject.txt(`push_subject_line` — mod.rs)もタイトルのエスケープに同じ規則を
/// 使う(タイトルに `<>` が含まれると subject.txt のフィールド区切りが壊れるため —
/// T054 レビュー対応)。
pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// 本文用のエスケープ: [`escape`] に加えて改行を `<br>` へ変換する(アンカー `>>n` は
/// `escape` の結果 `&gt;&gt;n` として出力される — 専ブラが解釈する従来形式)。
fn escape_body(s: &str) -> String {
    escape(s).replace('\n', "<br>")
}

// ---------------------------------------------------------------------------
// HTTP メタデータ(Last-Modified / Range)— T055
// ---------------------------------------------------------------------------

/// RFC 7231 の HTTP-date(`Sun, 06 Nov 1994 08:49:37 GMT` 形式)へフォーマットする。
///
/// 標準ライブラリ・追加クレートに依存せず自前実装する([`civil_from_unix`] を再利用)。
pub fn format_http_date(unix_secs: i64) -> String {
    let (y, mo, d, wd, h, mi, s) = civil_from_unix(unix_secs);
    const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 13] = [
        "", "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!(
        "{}, {d:02} {} {y:04} {h:02}:{mi:02}:{s:02} GMT",
        WEEKDAYS[wd as usize], MONTHS[mo as usize]
    )
}

/// HTTP-date 文字列を unix 秒へパースする(`If-Modified-Since` の解釈用)。
///
/// RFC 7231 IMF-fixdate(`format_http_date` が生成する形式)のみを受理する。他の 2 形式
/// (asctime-date・RFC 850)は専ブラの送信実績が薄く、パース不能時は `None`(呼び出し側は
/// 条件付き GET を適用せず通常応答へフォールバックする — 安全側)。
pub fn parse_http_date(s: &str) -> Option<i64> {
    // "Sun, 06 Nov 1994 08:49:37 GMT"
    let s = s.trim();
    let rest = s.split_once(", ")?.1;
    let mut parts = rest.split_whitespace();
    let day: u32 = parts.next()?.parse().ok()?;
    let month_str = parts.next()?;
    let year: i64 = parts.next()?.parse().ok()?;
    let time = parts.next()?;
    let tz = parts.next()?;
    if tz != "GMT" {
        return None;
    }
    let mut time_parts = time.split(':');
    let h: u32 = time_parts.next()?.parse().ok()?;
    let mi: u32 = time_parts.next()?.parse().ok()?;
    let sec: u32 = time_parts.next()?.parse().ok()?;
    const MONTHS: [&str; 13] = [
        "", "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let month = MONTHS.iter().position(|m| *m == month_str)? as u32;
    if month == 0 {
        return None;
    }
    Some(unix_from_civil(year, month, day, h, mi, sec))
}

/// グレゴリオ暦(UTC)から unix 秒へ変換する([`civil_from_unix`] の逆変換)。
fn unix_from_civil(y: i64, m: u32, d: u32, h: u32, mi: u32, s: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d as u64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    let days = era * 146097 + doe as i64 - 719468;
    days * 86400 + h as i64 * 3600 + mi as i64 * 60 + s as i64
}

/// `Range: bytes=<from>-<to>` を解析する(サフィックス形式 `bytes=-N` にも対応)。
///
/// `content_len` は対象本文の全長(バイト数)。解決できない・充足不能な範囲は `None`
/// (呼び出し側は 416 を返す)。成功時は `(開始, 終了)` の**閉区間**(両端を含む・
/// 0 始まり)を返す(HTTP `Content-Range` の慣習に合わせる)。
pub fn parse_range(header_value: &str, content_len: usize) -> Option<(usize, usize)> {
    let spec = header_value.strip_prefix("bytes=")?;
    // 複数レンジ(カンマ区切り)は非対応(専ブラの実利用は単一レンジが通例)。
    if spec.contains(',') {
        return None;
    }
    if content_len == 0 {
        return None;
    }
    let (from_str, to_str) = spec.split_once('-')?;
    if from_str.is_empty() {
        // サフィックス形式: bytes=-500 → 末尾 500 バイト。
        let suffix_len: usize = to_str.parse().ok()?;
        if suffix_len == 0 {
            return None;
        }
        let from = content_len.saturating_sub(suffix_len);
        return Some((from, content_len - 1));
    }
    let from: usize = from_str.parse().ok()?;
    if from >= content_len {
        return None; // 充足不能(416)
    }
    let to: usize = if to_str.is_empty() {
        content_len - 1
    } else {
        to_str.parse().ok()?
    };
    let to = to.min(content_len - 1);
    if from > to {
        return None;
    }
    Some((from, to))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_res(name: Option<&str>, body: &str, board_key: &str, created_at: i64) -> Res {
        Res {
            event_id: "11".repeat(32),
            board_key: board_key.to_string(),
            name: name.map(str::to_string),
            mail: Some("sage".to_string()),
            body: body.to_string(),
            created_at,
            res_no: Some(1),
            pending: false,
        }
    }

    // --- エスケープ一意規則 --------------------------------------------------

    #[test]
    fn escape_order_prevents_double_escaping() {
        // & を最初に処理するため、後続の実体参照が二重エスケープされない。
        assert_eq!(escape("<"), "&lt;");
        assert_eq!(escape("&"), "&amp;");
        assert_eq!(
            escape("&lt;"),
            "&amp;lt;",
            "先に & がエスケープされる(意図した一意規則)"
        );
    }

    #[test]
    fn escape_covers_all_four_chars_in_order() {
        assert_eq!(escape(r#"&<>""#), "&amp;&lt;&gt;&quot;");
    }

    #[test]
    fn escape_body_converts_newline_to_br() {
        assert_eq!(escape_body("行1\n行2"), "行1<br>行2");
    }

    #[test]
    fn anchor_escapes_to_gt_gt_n() {
        // アンカー >>n は escape の結果 &gt;&gt;n として出力される(契約 §dat)。
        assert_eq!(escape_body(">>1"), "&gt;&gt;1");
    }

    // --- 短縮 ID ---------------------------------------------------------------

    #[test]
    fn short_id_is_first_eight_chars() {
        let key = "11223344".to_string() + &"a".repeat(56);
        assert_eq!(short_id(&key), "11223344");
    }

    // --- 日付フォーマット --------------------------------------------------------

    #[test]
    fn format_date_matches_expected_form() {
        // 2024-01-01 00:00:00 UTC は月曜日。
        let unix = 1_704_067_200i64;
        assert_eq!(format_date(unix), "2024/01/01(月) 00:00:00");
    }

    #[test]
    fn civil_from_unix_epoch_is_thursday() {
        let (y, mo, d, wd, h, mi, s) = civil_from_unix(0);
        assert_eq!((y, mo, d, wd, h, mi, s), (1970, 1, 1, 4, 0, 0, 0));
    }

    // --- 行フォーマット ----------------------------------------------------------

    #[test]
    fn format_line_uses_name_baked_in_at_confirm_time() {
        // T055 レビュー対応: format_line は現行の noname_name を解決しない。
        // Res.name は確定処理(registry.accept_write)が既に焼き込み済みの前提。
        let res = sample_res(Some("名無しさん"), "本文", &"ab".repeat(32), 1_704_067_200);
        let line = format_line(&res, "スレタイトル");
        assert!(line.starts_with(
            "名無しさん<>sage<>2024/01/01(月) 00:00:00 ID:abababab<>本文<>スレタイトル\n"
        ));
    }

    #[test]
    fn format_line_keeps_explicit_name() {
        let res = sample_res(Some("コテハン"), "本文", &"cd".repeat(32), 1_704_067_200);
        let line = format_line(&res, "");
        assert!(line.starts_with("コテハン<>"));
        assert!(
            line.ends_with("<>\n"),
            "2 行目以降相当ではタイトルは空: {line}"
        );
    }

    #[test]
    fn render_only_includes_title_on_first_line() {
        let mut thread = Thread::new(
            "ab".repeat(32),
            "30311:x:y",
            1,
            1_700_000_000,
            "実況スレ",
            1000,
        );
        thread
            .confirm(
                sample_res(
                    Some("名無しさん"),
                    "一つ目",
                    &"11".repeat(32),
                    1_700_000_001,
                ),
                1,
            )
            .unwrap();
        thread
            .confirm(
                sample_res(
                    Some("名無しさん"),
                    "二つ目",
                    &"22".repeat(32),
                    1_700_000_002,
                ),
                2,
            )
            .unwrap();
        let out = render(&thread);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(
            lines[0].ends_with("<>実況スレ"),
            "1 行目にタイトル: {}",
            lines[0]
        );
        assert!(lines[1].ends_with("<>"), "2 行目はタイトル空: {}", lines[1]);
    }

    #[test]
    fn render_appends_only_confirmed_res_in_order() {
        let mut thread = Thread::new(
            "ab".repeat(32),
            "30311:x:y",
            1,
            1_700_000_000,
            "実況スレ",
            1000,
        );
        for i in 1..=3u16 {
            thread
                .confirm(
                    sample_res(
                        Some("名無しさん"),
                        &format!("レス{i}"),
                        &"11".repeat(32),
                        1_700_000_000 + i as i64,
                    ),
                    i,
                )
                .unwrap();
        }
        let out = render(&thread);
        assert_eq!(out.lines().count(), 3);
        assert!(out.contains("レス1"));
        assert!(out.contains("レス2"));
        assert!(out.contains("レス3"));
    }

    // --- dat 追記不変性(構造的に満たされることの確認)-----------------------------

    #[test]
    fn render_is_prefix_stable_as_res_are_appended() {
        let mut thread = Thread::new(
            "ab".repeat(32),
            "30311:x:y",
            1,
            1_700_000_000,
            "実況スレ",
            1000,
        );
        thread
            .confirm(
                sample_res(
                    Some("名無しさん"),
                    "一つ目",
                    &"11".repeat(32),
                    1_700_000_001,
                ),
                1,
            )
            .unwrap();
        let before = render(&thread);

        thread
            .confirm(
                sample_res(
                    Some("名無しさん"),
                    "二つ目",
                    &"22".repeat(32),
                    1_700_000_002,
                ),
                2,
            )
            .unwrap();
        let after = render(&thread);

        assert!(
            after.starts_with(&before),
            "追記後も既存部分はバイト列として不変(dat 追記不変性 MUST)"
        );
    }

    #[test]
    fn format_line_is_pure_and_deterministic() {
        // 同一入力からは常に同一出力(呼び出し側の外部状態に依存しない)。
        let res = sample_res(Some("名前"), "本文", &"11".repeat(32), 1_700_000_000);
        let a = format_line(&res, "タイトル");
        let b = format_line(&res, "タイトル");
        assert_eq!(a, b);
    }

    // --- HTTP-date ---------------------------------------------------------------

    #[test]
    fn http_date_format_and_parse_roundtrip() {
        let unix = 1_704_067_200i64; // 2024-01-01 00:00:00 UTC (月曜)
        let formatted = format_http_date(unix);
        assert_eq!(formatted, "Mon, 01 Jan 2024 00:00:00 GMT");
        assert_eq!(parse_http_date(&formatted), Some(unix));
    }

    #[test]
    fn http_date_parse_rejects_malformed() {
        assert_eq!(parse_http_date("not a date"), None);
        assert_eq!(parse_http_date("Mon, 01 Jan 2024 00:00:00 JST"), None);
    }

    // --- Range ---------------------------------------------------------------

    #[test]
    fn parse_range_basic_from_to() {
        assert_eq!(parse_range("bytes=0-99", 200), Some((0, 99)));
        assert_eq!(parse_range("bytes=100-199", 200), Some((100, 199)));
    }

    #[test]
    fn parse_range_open_ended() {
        assert_eq!(parse_range("bytes=100-", 200), Some((100, 199)));
    }

    #[test]
    fn parse_range_suffix() {
        assert_eq!(parse_range("bytes=-50", 200), Some((150, 199)));
    }

    #[test]
    fn parse_range_clamps_to_content_len() {
        assert_eq!(parse_range("bytes=0-999", 200), Some((0, 199)));
    }

    #[test]
    fn parse_range_unsatisfiable_returns_none() {
        assert_eq!(parse_range("bytes=500-600", 200), None, "開始が本文長以上");
        assert_eq!(parse_range("bytes=0-99", 0), None, "本文が空");
    }

    #[test]
    fn parse_range_malformed_returns_none() {
        assert_eq!(parse_range("items=0-99", 200), None);
        assert_eq!(parse_range("bytes=abc-99", 200), None);
    }
}

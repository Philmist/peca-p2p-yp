//! 実況スレ Web UI / ローカル API(T024/T025 — spec.md `livechat-thread`)
//!
//! 本タスク(T025)ではローカルルール(板設定 `local_rules`、最大 2048 文字の
//! Markdown — FR-022)の安全 Markdown 描画を実装する。後続 T024 でスレ一覧・
//! 閲覧の API ハンドラを追加する。
//!
//! - [`render_local_rules_html`]: ローカルルールの Markdown を安全なサブセット
//!   のみ HTML へ描画する(FR-025 / research R7)。

use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, TagEnd, html};

/// ローカルルールの Markdown を安全なサブセットのみ HTML へ描画する(FR-025)。
///
/// 見出し・強調・リスト・引用・コード・段落・http(s) リンクは通常どおり描画するが、
/// 以下の 2 点でイベントストリームを加工し、生成前に危険な要素を除去する
/// (生成後のサニタイズより攻撃面が小さい — research R7):
///
/// 1. **raw HTML の破棄**: Markdown 中に埋め込まれた生 HTML(`Event::Html` /
///    `Event::InlineHtml`。`<script>` 等)は捨てる。pulldown-cmark はデフォルトで
///    raw HTML をそのまま透過するため、ここで明示的にフィルタしないと
///    `push_html` がそのまま出力に混ぜてしまう。
/// 2. **非 http(s) リンクの無効化**: リンク先スキームが http/https 以外
///    (`javascript:` / `data:` / `mailto:` 等)なら `Tag::Link` / `TagEnd::Link`
///    イベントごと除去し、リンクテキストのみを平文として残す(001 FR-012 の
///    URL 安全性規則と同じ判定基準)。
///
/// 通常テキストは pulldown-cmark の [`html::push_html`] が既定で HTML エスケープ
/// するため、生成した HTML をそのまま UI に挿入しても XSS を起こさない。
pub fn render_local_rules_html(markdown: &str) -> String {
    let parser = Parser::new_ext(markdown, Options::empty());
    let mut skip_link = false;
    let events = parser.filter_map(|event| match event {
        // raw HTML は破棄(要件 1)。
        Event::Html(_) | Event::InlineHtml(_) => None,
        Event::Start(Tag::Link { dest_url, .. }) if !is_http_or_https(&dest_url) => {
            // 対応する End(Link) も揃えて捨てるため状態を持つ(要件 2)。
            skip_link = true;
            None
        }
        Event::End(TagEnd::Link) if skip_link => {
            skip_link = false;
            None
        }
        other => Some(other),
    });
    let mut html_out = String::new();
    html::push_html(&mut html_out, events);
    html_out
}

/// URL のスキームが http/https か(大文字小文字を区別しない)。
/// 001 FR-012([`crate::security::url_needs_warning`])と同じ判定基準を用いる。
fn is_http_or_https(url: &CowStr<'_>) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_heading_emphasis_list_and_link() {
        let md =
            "# 見出し\n\n**強調**と*斜体*\n\n- 項目1\n- 項目2\n\n[example](http://example.com)";
        let out = render_local_rules_html(md);
        assert!(out.contains("<h1>"), "見出しが h1 になる: {out}");
        assert!(out.contains("<strong>強調</strong>"), "強調: {out}");
        assert!(out.contains("<em>斜体</em>"), "斜体: {out}");
        assert!(out.contains("<li>項目1</li>"), "箇条書き: {out}");
        assert!(
            out.contains(r#"href="http://example.com""#),
            "http リンクは残る: {out}"
        );
    }

    #[test]
    fn strips_raw_script_tag() {
        let md = "本文\n\n<script>alert(1)</script>\n\n続き";
        let out = render_local_rules_html(md);
        assert!(!out.contains("<script"), "raw HTML は破棄される: {out}");
    }

    #[test]
    fn strips_inline_raw_html() {
        let md = "テキスト <b onclick=\"alert(1)\">太字</b> です";
        let out = render_local_rules_html(md);
        assert!(!out.contains("<b "), "インライン raw HTML も破棄: {out}");
        assert!(!out.contains("onclick"), "属性ごと消える: {out}");
    }

    #[test]
    fn disables_javascript_scheme_link() {
        let md = "[x](javascript:alert(1))";
        let out = render_local_rules_html(md);
        assert!(
            !out.contains("href=\"javascript:"),
            "javascript: リンクは無効化: {out}"
        );
    }

    #[test]
    fn allows_https_link() {
        let md = "[x](https://example.com/path)";
        let out = render_local_rules_html(md);
        assert!(
            out.contains(r#"href="https://example.com/path""#),
            "https リンクは残る: {out}"
        );
    }

    #[test]
    fn disables_data_scheme_link() {
        let md = "[x](data:text/html,<script>alert(1)</script>)";
        let out = render_local_rules_html(md);
        assert!(!out.contains("href=\"data:"), "data: リンクは無効化: {out}");
    }

    #[test]
    fn disables_mailto_scheme_link() {
        let md = "[x](mailto:a@example.com)";
        let out = render_local_rules_html(md);
        assert!(
            !out.contains("href=\"mailto:"),
            "mailto: リンクは無効化(http/https のみ許可): {out}"
        );
    }

    #[test]
    fn does_not_panic_on_2048_char_input() {
        let md = "あ".repeat(2048);
        let out = render_local_rules_html(&md);
        assert!(!out.is_empty());
    }

    #[test]
    fn escapes_plain_text_by_default() {
        // raw HTML タグではなく、地の文としての `<`/`>` はエスケープされる。
        let md = "1 < 2 かつ 3 > 2";
        let out = render_local_rules_html(md);
        assert!(out.contains("&lt;"), "< はエスケープ: {out}");
        assert!(out.contains("&gt;"), "> はエスケープ: {out}");
    }
}

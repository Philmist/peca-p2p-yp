//! Shift_JIS 変換層(T053 — contracts/compat-api.md §受け口)
//!
//! 互換 API の全応答は Shift_JIS(CP932)で符号化する。SJIS へ変換不能な文字は
//! **数値文字参照**(`&#dddd;`)で保全する — `<>` を含まないため区切り(`<>`)を壊さず、
//! 現代の互換実装(jpnkn 系)の通例に倣う(research R5)。既存の `index_txt.rs` は
//! 変換不能文字を `?` に潰すが、互換 API は情報欠落を避けるため数値文字参照方式を採る
//! (2 モジュールは方針が異なる — 意図的な差異)。
//!
//! 受理側(bbs.cgi フォーム)は逆に、数値文字参照(`&#dddd;` / `&#xhhhh;`)を展開して
//! 元の文字へ戻す(専ブラが SJIS 外の文字を数値文字参照で送る通例への対応)。

use encoding_rs::SHIFT_JIS;

/// テキストを Shift_JIS バイト列へ符号化する。
///
/// 変換不能な文字は数値文字参照 `&#<10進コードポイント>;` に置換する(`index_txt.rs` の
/// `?` 置換とは異なる方針 — 互換 API は SJIS 外文字を保全する必要があるため)。
/// 文字単位で変換を試みるため、変換不能文字の前後にある変換可能な文字には影響しない。
pub fn encode(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    for ch in text.chars() {
        let mut buf = [0u8; 4];
        let s = ch.encode_utf8(&mut buf);
        let (encoded, _, had_replacements) = SHIFT_JIS.encode(s);
        if had_replacements {
            out.extend_from_slice(format!("&#{};", ch as u32).as_bytes());
        } else {
            out.extend_from_slice(&encoded);
        }
    }
    out
}

/// Shift_JIS バイト列を UTF-8 文字列へ復号する(bbs.cgi 受信フォーム用)。
///
/// 変換不能バイト列は `encoding_rs` の既定(置換文字 U+FFFD)に従う。専ブラが送信する
/// フォームは通常 SJIS で正しく符号化されているため、実運用での置換発生は稀。
pub fn decode(bytes: &[u8]) -> String {
    let (decoded, _, _) = SHIFT_JIS.decode(bytes);
    decoded.into_owned()
}

/// 文字列中の数値文字参照(`&#<10進>;` / `&#x<16進>;` / `&#X<16進>;`)を展開する
/// (T053 — bbs.cgi 受信時。contracts/compat-api.md §bbs.cgi「数値文字参照の展開」)。
///
/// 展開できない参照(コードポイントが不正・閉じタグ `;` が無い・空の数字列)はそのまま
/// 素通りする(壊れた入力で書き込み全体を失敗させない — 前方互換の精神)。他の実体参照
/// (`&amp;` 等の名前付き参照)は対象外(専ブラが送信フォームで使わない形式のため)。
pub fn decode_numeric_char_refs(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'&'
            && i + 1 < bytes.len()
            && bytes[i + 1] == b'#'
            && let Some((ch, consumed)) = parse_numeric_char_ref(&s[i..])
        {
            out.push(ch);
            i += consumed;
            continue;
        }
        // ASCII 境界を跨がないよう、char 単位で 1 文字進める。
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// `&#<10進>;` または `&#x<16進>;` を先頭から読み取り、`(文字, 消費バイト数)` を返す。
/// 形式不正・コードポイント不正は `None`(呼び出し側は `&` を素通りする)。
fn parse_numeric_char_ref(s: &str) -> Option<(char, usize)> {
    let rest = s.strip_prefix("&#")?;
    let (is_hex, digits_start) = if rest.starts_with('x') || rest.starts_with('X') {
        (true, 1)
    } else {
        (false, 0)
    };
    let digits_str = &rest[digits_start..];
    let semi_pos = digits_str.find(';')?;
    let digits = &digits_str[..semi_pos];
    if digits.is_empty() {
        return None;
    }
    let code = if is_hex {
        u32::from_str_radix(digits, 16).ok()?
    } else {
        digits.parse::<u32>().ok()?
    };
    let ch = char::from_u32(code)?;
    // 消費バイト数 = "&#" + (x/X の 1 バイト、あれば) + digits + ";"
    let consumed = 2 + digits_start + digits.len() + 1;
    Some((ch, consumed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_roundtrips_ascii_and_kana() {
        let out = encode("test");
        assert_eq!(out, b"test");
        // テ = 0x8365 (Shift_JIS)
        let out = encode("テスト");
        assert!(out.windows(2).any(|w| w == [0x83, 0x65]));
    }

    #[test]
    fn encode_unconvertible_char_becomes_numeric_char_ref() {
        // 🚀 (U+1F680) は Shift_JIS で表現できない。
        let out = encode("a🚀b");
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text, "a&#128640;b");
    }

    #[test]
    fn encode_numeric_char_ref_does_not_contain_angle_brackets() {
        // 数値文字参照は <> を含まないため dat の区切りと衝突しない(契約の要件)。
        let out = encode("🚀");
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains('<'));
        assert!(!text.contains('>'));
    }

    #[test]
    fn decode_roundtrips_shift_jis_bytes() {
        let encoded = encode("こんにちは");
        let decoded = decode(&encoded);
        assert_eq!(decoded, "こんにちは");
    }

    #[test]
    fn decode_numeric_char_refs_expands_decimal() {
        assert_eq!(decode_numeric_char_refs("a&#128640;b"), "a🚀b");
    }

    #[test]
    fn decode_numeric_char_refs_expands_hex() {
        assert_eq!(decode_numeric_char_refs("&#x1F680;"), "🚀");
        assert_eq!(decode_numeric_char_refs("&#X1f680;"), "🚀");
    }

    #[test]
    fn decode_numeric_char_refs_leaves_malformed_refs_untouched() {
        assert_eq!(decode_numeric_char_refs("&#nope;"), "&#nope;");
        assert_eq!(decode_numeric_char_refs("&#123"), "&#123"); // 閉じ ; なし
        assert_eq!(decode_numeric_char_refs("&#;"), "&#;"); // 数字なし
        assert_eq!(decode_numeric_char_refs("plain text"), "plain text");
    }

    #[test]
    fn decode_numeric_char_refs_mixed_with_normal_text() {
        assert_eq!(
            decode_numeric_char_refs("名前&#12354;テスト"),
            "名前あテスト"
        );
    }

    #[test]
    fn decode_numeric_char_refs_does_not_expand_named_entities() {
        // 名前付き実体参照(&amp; 等)は対象外 — そのまま素通りする。
        assert_eq!(decode_numeric_char_refs("&amp;"), "&amp;");
    }
}

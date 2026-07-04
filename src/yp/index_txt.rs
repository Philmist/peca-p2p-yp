//! index.txt 生成(T042 — contracts/http-yp.md)
//!
//! 18 フィールド(`<>` 区切り 17 個)のテキストを生成し、指定エンコーディングのバイト列で返す。
//! HTTP 配信ルート(GET/HEAD `/index.txt`)もここに含む。
//!
//! ## サニタイズ順序(contracts/http-yp.md §サニタイズ順序)
//!   1. フィールド値から区切り列 `<>` を除去
//!   2. エンコーディング変換で変換不能文字を `?` に置換
//!
//! この順序により `?` が `<>` 区切りを破壊しない。
//!
//! ## フィールドレイアウト(18 フィールド・区切り 17 個)
//! ```text
//! CHANNEL_NAME<>ID<>TIP<>CONTACT_URL<>GENRE<>DETAIL<>LISTENER_NUM<>RELAY_NUM
//! <>BITRATE<>TYPE<>TRACK_ARTIST<>TRACK_ALBUM<>TRACK_TITLE<>TRACK_CONTACT_URL
//! <><>BROADCAST_TIME<><>COMMENT
//! ```
//! フィールド 15・17 は位置互換の予約で常に空。

use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{body::Body, Router};
use encoding_rs::SHIFT_JIS;

use crate::config::{IndexEncoding, Settings};
use crate::event::view::DiscoveredChannel;
use crate::security::SecurityCategory;
use crate::web::{error_response, AppState};

// ---------------------------------------------------------------------------
// 生成ロジック
// ---------------------------------------------------------------------------

/// 既知のチャンネル集合から index.txt のバイト列を生成する。
///
/// - `channels` は [`crate::event::view::ChannelDirectory::list`] の保証する順
///   (live・鮮度窓内・非ミュートのみ、`created_at` 降順)
/// - `now` は unix 秒(テスト用に注入可能)
pub fn generate(channels: &[DiscoveredChannel], encoding: IndexEncoding, now: u64) -> Vec<u8> {
    if channels.is_empty() {
        return Vec::new();
    }
    let mut text = String::new();
    for ch in channels {
        text.push_str(&channel_line(ch, now));
        text.push('\n');
    }
    match encoding {
        IndexEncoding::Utf8 => text.into_bytes(),
        IndexEncoding::ShiftJis => encode_shift_jis(&text),
    }
}

/// 1 チャンネル分の行(18 フィールド、`<>` 区切り 17 個)を生成する。
fn channel_line(ch: &DiscoveredChannel, now: u64) -> String {
    let l = &ch.listing;

    // フィールド 1: CHANNEL_NAME
    let name = sanitize(&l.title);
    // フィールド 2: ID — 内部小文字・出力時のみ大文字化(contracts/http-yp.md 変換規則)
    let id = ch.channel_id.to_uppercase();
    // フィールド 3: TIP — firewalled は空文字列
    let tip = sanitize(l.tip.as_deref().unwrap_or(""));
    // フィールド 4: CONTACT_URL
    let contact = sanitize(l.contact.as_deref().unwrap_or(""));
    // フィールド 5: GENRE
    let genre = sanitize(l.genre.as_deref().unwrap_or(""));
    // フィールド 6: DETAIL (summary タグ)
    let detail = sanitize(l.summary.as_deref().unwrap_or(""));
    // フィールド 7: LISTENER_NUM — 不明(タグ省略)は -1
    let listener_num = l.current_participants.to_string();
    // フィールド 8: RELAY_NUM — 不明(タグ省略)は -1
    let relay_num = l.relays.to_string();
    // フィールド 9: BITRATE — 不明は 0
    let bitrate = l.bitrate_kbps.unwrap_or(0).to_string();
    // フィールド 10: TYPE
    let content_type = sanitize(l.content_type.as_deref().unwrap_or(""));
    // フィールド 11〜14: トラック情報
    let (track_artist, track_album, track_title, track_contact_url) = match &l.track {
        Some(t) => (
            sanitize(&t.artist),
            sanitize(&t.album),
            sanitize(&t.title),
            sanitize(&t.url),
        ),
        None => (
            String::new(),
            String::new(),
            String::new(),
            String::new(),
        ),
    };
    // フィールド 15: 予約(常に空)
    // フィールド 16: BROADCAST_TIME
    let broadcast_time = format_broadcast_time(now, l.starts);
    // フィールド 17: 予約(常に空)
    // フィールド 18: COMMENT — v1 は常に空

    format!(
        "{name}<>{id}<>{tip}<>{contact}<>{genre}<>{detail}<>{listener_num}<>{relay_num}<>{bitrate}<>{content_type}<>{track_artist}<>{track_album}<>{track_title}<>{track_contact_url}<><>{broadcast_time}<><>"
    )
}

/// フィールド値から区切り列 `<>` を除去する(サニタイズ手順 1)。
///
/// Shift_JIS 変換不能文字の `?` 置換はエンコーディング変換時に行う(手順 2)。
fn sanitize(s: &str) -> String {
    s.replace("<>", "")
}

/// BROADCAST_TIME を `H:MM` 形式にフォーマットする。
///
/// - `now < starts` の場合は `0:00`(アンダーフロー防止)
/// - 時間部は 24 超でもそのまま拡張する(`25:30` 等 — contracts/http-yp.md)
/// - 分は 2 桁固定(ゼロ埋め)
fn format_broadcast_time(now: u64, starts: u64) -> String {
    let duration_secs = now.saturating_sub(starts);
    let hours = duration_secs / 3600;
    let minutes = (duration_secs % 3600) / 60;
    format!("{hours}:{minutes:02}")
}

/// テキストを Shift_JIS に変換する。変換不能文字は `?`(0x3F)に置換(サニタイズ手順 2)。
///
/// 文字単位で変換を試み、変換不能なら `?` を出力する。
/// `?` は `<>` 区切りと衝突しないため、手順 1 後に安全に適用できる。
fn encode_shift_jis(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    for ch in text.chars() {
        let mut buf = [0u8; 4];
        let s = ch.encode_utf8(&mut buf);
        let (encoded, _, had_replacements) = SHIFT_JIS.encode(s);
        if had_replacements {
            out.push(b'?');
        } else {
            out.extend_from_slice(&encoded);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// HTTP ルート
// ---------------------------------------------------------------------------

/// index.txt の独自レート上限(同一接続元あたり秒 10 件)。
pub const INDEX_TXT_RATE_LIMIT_PER_SEC: u32 = 10;

/// `/index.txt` エンドポイントのルーター。
///
/// - GET/HEAD のみ受け付ける(他は 405 — axum が自動付与)
/// - 独自レート制限 10 req/秒(AppState.index_txt_rate_limiter)
/// - ヘッダ合計 ≤ 8KB・URL 長 ≤ 1KB(超過は 400)
pub(crate) fn routes() -> Router<AppState> {
    Router::new().route("/index.txt", get(handler))
}

/// GET /index.txt ハンドラ。
async fn handler(State(state): State<AppState>, req: Request<Body>) -> Response {
    // URL 長チェック(≤ 1KB)
    let uri_len = req.uri().path().len()
        + req.uri().query().map_or(0, |q| q.len() + 1);
    if uri_len > 1024 {
        return error_response(StatusCode::BAD_REQUEST, "request_too_large");
    }

    // ヘッダ合計サイズチェック(≤ 8KB)
    let header_bytes: usize = req.headers().iter().map(|(k, v)| k.as_str().len() + v.len() + 4).sum();
    if header_bytes > 8192 {
        return error_response(StatusCode::BAD_REQUEST, "request_too_large");
    }

    // レート制限(独自 10 req/秒)
    let ip = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip())
        .unwrap_or(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    if !state.index_txt_rate_limiter.check(ip) {
        state.security.log(
            SecurityCategory::HttpRateLimited,
            &ip.to_string(),
            "index.txt rate limit exceeded",
        );
        return error_response(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    }

    // HEAD リクエストはボディなしで返す(axum は GET を呼び出しボディを除去するが
    // ここでは明示的に扱う)
    let is_head = *req.method() == Method::HEAD;

    // エンコーディング設定を読み込む(失敗時は Shift_JIS にフォールバック)
    let encoding = Settings::load(&state.store)
        .and_then(|s| s.index_encoding())
        .unwrap_or(IndexEncoding::ShiftJis);

    // チャンネル一覧を取得(directory が未配線なら空一覧)
    let channels = state
        .directory
        .as_ref()
        .map(|d| d.list())
        .unwrap_or_default();

    // now は実時刻
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let body_bytes = generate(&channels, encoding, now);

    let content_type = match encoding {
        IndexEncoding::ShiftJis => "text/plain; charset=Shift_JIS",
        IndexEncoding::Utf8 => "text/plain; charset=UTF-8",
    };

    if is_head {
        (
            [(header::CONTENT_TYPE, content_type)],
            Body::empty(),
        )
            .into_response()
    } else {
        (
            [(header::CONTENT_TYPE, content_type)],
            body_bytes,
        )
            .into_response()
    }
}

// ---------------------------------------------------------------------------
// ユニットテスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::schema::{ChannelListing, ChannelStatus, UNKNOWN_COUNT};

    fn minimal_listing(channel_id: &str) -> ChannelListing {
        ChannelListing {
            channel_id: channel_id.to_string(),
            title: "test".to_string(),
            summary: None,
            genre: None,
            status: ChannelStatus::Live,
            starts: 0,
            current_participants: UNKNOWN_COUNT,
            streaming: None,
            bitrate_kbps: None,
            content_type: None,
            tip: None,
            contact: None,
            relays: UNKNOWN_COUNT,
            track: None,
        }
    }

    fn minimal_channel(channel_id: &str) -> DiscoveredChannel {
        DiscoveredChannel {
            author_pubkey: "a".repeat(64),
            channel_id: channel_id.to_string(),
            listing: minimal_listing(channel_id),
            created_at: 0,
            source_peers: vec![],
        }
    }

    #[test]
    fn sanitize_removes_delimiter() {
        assert_eq!(sanitize("foo<>bar"), "foobar");
        assert_eq!(sanitize("a<>b<>c"), "abc");
        assert_eq!(sanitize("no-delim"), "no-delim");
    }

    #[test]
    fn broadcast_time_format() {
        assert_eq!(format_broadcast_time(3600, 0), "1:00");
        assert_eq!(format_broadcast_time(7500, 0), "2:05");
        assert_eq!(format_broadcast_time(91800, 0), "25:30");
        assert_eq!(format_broadcast_time(0, 100), "0:00"); // アンダーフロー
    }

    #[test]
    fn field_count_is_18() {
        let ch = minimal_channel("aabbccdd00112233445566778899aabb");
        let line = channel_line(&ch, 3600);
        assert_eq!(line.split("<>").count(), 18);
    }

    #[test]
    fn empty_channels_returns_empty() {
        assert!(generate(&[], IndexEncoding::Utf8, 0).is_empty());
        assert!(generate(&[], IndexEncoding::ShiftJis, 0).is_empty());
    }

    #[test]
    fn shift_jis_encodes_katakana() {
        let ch = minimal_channel("00000000000000000000000000000001");
        let mut ch = ch;
        ch.listing.title = "テスト".to_string();
        let out = generate(&[ch], IndexEncoding::ShiftJis, 60);
        // テ=0x8365, ス=0x8358, ト=0x8367 (Shift_JIS)
        assert!(out.windows(2).any(|w| w == [0x83, 0x65]));
    }

    #[test]
    fn unencodable_char_becomes_question_mark() {
        let ch = minimal_channel("00000000000000000000000000000002");
        let mut ch = ch;
        ch.listing.title = "a🚀b".to_string(); // 🚀 は Shift_JIS 変換不能
        let out = generate(&[ch], IndexEncoding::ShiftJis, 60);
        assert!(out.contains(&b'?'));
        // `?` は <> と衝突しない
        assert!(!out.windows(2).any(|w| w == [b'?', b'>'] || w == [b'<', b'?']));
    }
}

//! index.txt ゴールデンテスト(T036 — contracts/http-yp.md §検証方法。
//! 2026-07-04 実機 YP 検証で 19 フィールド形式に改訂)
//!
//! 既知 DiscoveredChannel 集合 → 19 フィールド(区切り `<>` 18 個)出力比較。
//!
//! 検証ケース:
//! - UTF-8 ゴールデン比較
//! - Shift_JIS ゴールデン比較
//! - 空一覧 → 空出力
//! - firewalled チャンネル(TIP 空文字列)
//! - ID の大文字化(内部小文字 → 出力大文字)
//! - サニタイズ: テキストは `&`/`<`/`>` を HTML エスケープ → 変換不能文字 `?` 置換
//! - BROADCAST_TIME 24 時間超(25 時間 30 分 → `25:30`、分 2 桁固定)
//! - NAME_ENCODED(15 番目)= チャンネル名の percent エンコード、17 番目 = `click`、
//!   19 番目(DIRECT)= `0`
//! - 不明フィールド: listeners/relays = -1、bitrate = 0

use encoding_rs::SHIFT_JIS;

use peca_p2p_yp::config::IndexEncoding;
use peca_p2p_yp::event::schema::{ChannelListing, ChannelStatus, Track, UNKNOWN_COUNT};
use peca_p2p_yp::event::view::DiscoveredChannel;
use peca_p2p_yp::yp::index_txt::generate;

// ---------------------------------------------------------------------------
// ヘルパ
// ---------------------------------------------------------------------------

fn make_listing(channel_id: &str) -> ChannelListing {
    ChannelListing {
        channel_id: channel_id.to_string(),
        title: "テスト放送".to_string(),
        summary: Some("詳細説明".to_string()),
        genre: Some("テスト".to_string()),
        status: ChannelStatus::Live,
        starts: 1_000,
        current_participants: 5,
        streaming: None,
        bitrate_kbps: Some(128),
        content_type: Some("FLV".to_string()),
        tip: Some("192.168.1.1:7144".to_string()),
        contact: Some("http://example.com/".to_string()),
        relays: 3,
        track: Some(Track {
            title: "test song".to_string(),
            artist: "test artist".to_string(),
            album: "test album".to_string(),
            url: String::new(),
        }),
    }
}

fn make_channel(channel_id: &str, listing: ChannelListing, created_at: u64) -> DiscoveredChannel {
    DiscoveredChannel {
        author_pubkey: "a".repeat(64),
        channel_id: channel_id.to_string(),
        listing,
        created_at,
        source_peers: vec![],
    }
}

/// UTF-8 文字列から Shift_JIS バイト列へ変換する(変換不能文字は `?` 置換)。
/// generate() と同じ変換を行い、ゴールデン比較に使う。
fn to_sjis_bytes(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    for ch in s.chars() {
        let mut buf = [0u8; 4];
        let cs = ch.encode_utf8(&mut buf);
        let (encoded, _, had_replacements) = SHIFT_JIS.encode(cs);
        if had_replacements {
            out.push(b'?');
        } else {
            out.extend_from_slice(&encoded);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

/// 空一覧は空バイト列を返す。
#[test]
fn empty_list_produces_empty_output() {
    let out = generate(&[], IndexEncoding::Utf8, 9999);
    assert!(out.is_empty(), "空一覧は空出力");

    let out_sjis = generate(&[], IndexEncoding::ShiftJis, 9999);
    assert!(out_sjis.is_empty(), "空一覧(Shift_JIS)は空出力");
}

/// UTF-8 ゴールデン: 19 フィールドのレイアウトと内容を確認する。
///
/// now = 1000 + 2*3600 + 5*60 = 8500 → BROADCAST_TIME = 2:05
#[test]
fn utf8_golden_basic() {
    let cid = "abcdef0123456789abcdef0123456789";
    let listing = make_listing(cid);
    let ch = make_channel(cid, listing, 2_000);
    let now = 1_000 + 2 * 3600 + 5 * 60; // 8500 → 2:05

    let out = generate(&[ch], IndexEncoding::Utf8, now);
    let text = std::str::from_utf8(&out).expect("UTF-8 出力");
    let line = text.trim_end_matches('\n');

    let fields: Vec<&str> = line.split("<>").collect();
    assert_eq!(fields.len(), 19, "19 フィールドであること");

    // フィールド 1: CHANNEL_NAME
    assert_eq!(fields[0], "テスト放送");
    // フィールド 2: ID (大文字)
    assert_eq!(fields[1], "ABCDEF0123456789ABCDEF0123456789");
    // フィールド 3: TIP
    assert_eq!(fields[2], "192.168.1.1:7144");
    // フィールド 4: CONTACT_URL
    assert_eq!(fields[3], "http://example.com/");
    // フィールド 5: GENRE
    assert_eq!(fields[4], "テスト");
    // フィールド 6: DETAIL
    assert_eq!(fields[5], "詳細説明");
    // フィールド 7: LISTENER_NUM
    assert_eq!(fields[6], "5");
    // フィールド 8: RELAY_NUM
    assert_eq!(fields[7], "3");
    // フィールド 9: BITRATE
    assert_eq!(fields[8], "128");
    // フィールド 10: TYPE
    assert_eq!(fields[9], "FLV");
    // フィールド 11: TRACK_ARTIST
    assert_eq!(fields[10], "test artist");
    // フィールド 12: TRACK_ALBUM
    assert_eq!(fields[11], "test album");
    // フィールド 13: TRACK_TITLE
    assert_eq!(fields[12], "test song");
    // フィールド 14: TRACK_CONTACT_URL
    assert_eq!(fields[13], "");
    // フィールド 15: NAME_ENCODED(UTF-8 バイト列の percent エンコード)
    assert_eq!(
        fields[14], "%E3%83%86%E3%82%B9%E3%83%88%E6%94%BE%E9%80%81",
        "15 番目はチャンネル名の percent エンコード"
    );
    // フィールド 16: BROADCAST_TIME
    assert_eq!(fields[15], "2:05");
    // フィールド 17: 固定文字列 click
    assert_eq!(fields[16], "click", "17 番目は固定文字列 click");
    // フィールド 18: COMMENT
    assert_eq!(fields[17], "");
    // フィールド 19: DIRECT
    assert_eq!(fields[18], "0", "19 番目(DIRECT)は固定 0");
}

/// Shift_JIS ゴールデン: 日本語文字列が正しく Shift_JIS エンコードされる。
#[test]
fn shift_jis_golden_japanese() {
    let cid = "00000000000000000000000000000001";
    let listing = make_listing(cid);
    let ch = make_channel(cid, listing, 2_000);
    let now = 1_000 + 90; // 90 秒 → 0:01

    let out = generate(&[ch], IndexEncoding::ShiftJis, now);

    // 期待する UTF-8 文字列を構築してから Shift_JIS に変換し比較する
    // (NAME_ENCODED は Shift_JIS バイト列基準の古典 percent 形 %83e%83X%83g…)
    let expected_utf8 = "テスト放送<>00000000000000000000000000000001<>192.168.1.1:7144<>http://example.com/<>テスト<>詳細説明<>5<>3<>128<>FLV<>test artist<>test album<>test song<><>%83e%83X%83g%95%FA%91%97<>0:01<>click<><>0\n";
    let expected_bytes = to_sjis_bytes(expected_utf8);
    assert_eq!(out, expected_bytes, "Shift_JIS エンコードが一致する");
}

/// firewalled チャンネルは TIP フィールドが空文字列になる。
#[test]
fn firewalled_tip_is_empty() {
    let cid = "aabbccdd00112233445566778899aabb";
    let mut listing = make_listing(cid);
    listing.tip = None; // firewalled
    let ch = make_channel(cid, listing, 2_000);
    let now = 1_000 + 60;

    let out = generate(&[ch], IndexEncoding::Utf8, now);
    let text = std::str::from_utf8(&out).unwrap();
    let fields: Vec<&str> = text.trim_end_matches('\n').split("<>").collect();
    assert_eq!(fields.len(), 19);
    assert_eq!(fields[2], "", "firewalled チャンネルの TIP は空文字列");
}

/// ID は出力時のみ大文字化する(内部は小文字で保持)。
#[test]
fn id_is_uppercased_in_output() {
    let cid = "aabbccdd00112233445566778899aabb"; // 全小文字
    let listing = make_listing(cid);
    let ch = make_channel(cid, listing, 2_000);
    let now = 1_000 + 60;

    let out = generate(&[ch], IndexEncoding::Utf8, now);
    let text = std::str::from_utf8(&out).unwrap();
    let fields: Vec<&str> = text.trim_end_matches('\n').split("<>").collect();
    assert_eq!(fields.len(), 19);
    assert_eq!(
        fields[1], "AABBCCDD00112233445566778899AABB",
        "hex ID が大文字化されている"
    );
}

/// サニタイズ: テキストフィールドの `<`/`>` は HTML エスケープされ、
/// デリミタ解析を破壊しない(実運用 YP の出力形式)。
#[test]
fn sanitize_delimiter_in_title_is_escaped() {
    let cid = "00000000000000000000000000000002";
    // `<>` を含むタイトル → エスケープ後: "テスト&lt;&gt;放送"
    let mut listing = make_listing(cid);
    listing.title = "テスト<>放送".to_string();
    listing.tip = None;
    listing.track = None;
    listing.summary = None;
    listing.genre = None;

    let ch = make_channel(cid, listing, 2_000);
    let now = 1_000 + 60;

    let out = generate(&[ch], IndexEncoding::Utf8, now);
    let text = std::str::from_utf8(&out).unwrap();
    let fields: Vec<&str> = text.trim_end_matches('\n').split("<>").collect();
    // エスケープ後もフィールド数は 19 のまま(区切り解析が破壊されない)
    assert_eq!(fields.len(), 19, "エスケープ後もフィールド数は 19");
    assert_eq!(fields[0], "テスト&lt;&gt;放送", "`<`/`>` がエスケープされている");
}

/// Shift_JIS 変換不能文字(絵文字)は `?` に置換される。
#[test]
fn shift_jis_unencodable_becomes_question_mark() {
    let cid = "00000000000000000000000000000003";
    let mut listing = make_listing(cid);
    // 🚀 は Shift_JIS に変換不能 → `?` になる
    listing.title = "テスト🚀放送".to_string();
    listing.tip = None;
    listing.track = None;
    listing.summary = None;
    listing.genre = None;

    let ch = make_channel(cid, listing, 2_000);
    let now = 1_000 + 60;

    let ch2 = ch.clone(); // UTF-8 テスト用に先にクローン(ch は次行で move される)
    let out_sjis = generate(&[ch], IndexEncoding::ShiftJis, now);
    // Shift_JIS で "テスト?放送" の行が含まれるか確認(タイトル部分を検証)
    // UTF-8 に逆変換せず、バイト列レベルで "?" のバイト(0x3F)が含まれることを確認
    assert!(
        out_sjis.windows(1).any(|w| w == b"?"),
        "Shift_JIS 変換不能文字が `?` に置換されている"
    );
    // UTF-8 出力では 🚀 がそのまま含まれる
    let out_utf8 = generate(&[ch2], IndexEncoding::Utf8, now);
    let text = std::str::from_utf8(&out_utf8).unwrap();
    assert!(text.contains("テスト🚀放送"), "UTF-8 では絵文字が保持される");
}

/// `<>` と Shift_JIS 変換不能文字が両方含まれる場合のサニタイズ確認。
///
/// タイトル "A<>B🚀C" → エスケープ: "A&lt;&gt;B🚀C" → Shift_JIS: "A&lt;&gt;B?C"
/// `?` は `<>` と衝突しないため、フィールド数は常に 19 を保つ。
#[test]
fn sanitize_both_lt_gt_and_unencodable() {
    let cid = "00000000000000000000000000000004";
    let mut listing = make_listing(cid);
    listing.title = "A<>B🚀C".to_string();
    listing.tip = None;
    listing.track = None;
    listing.summary = None;
    listing.genre = None;

    let ch = make_channel(cid, listing, 2_000);
    let now = 1_000 + 60;

    let out_sjis = generate(&[ch], IndexEncoding::ShiftJis, now);
    // Shift_JIS バイト列なので行区切り LF を探してフィールド数を検証
    // `<>` は ASCII(0x3C 0x3E)のため Shift_JIS でもバイトパターンは同じ
    let pos = out_sjis.iter().position(|&b| b == b'\n').unwrap_or(out_sjis.len());
    let line_bytes = &out_sjis[..pos];
    // '<>' = [0x3C, 0x3E] で分割してフィールド数を数える
    let separators = line_bytes.windows(2).filter(|w| w == &[0x3C, 0x3E]).count();
    assert_eq!(separators, 18, "エスケープ後も区切りは 18 個(フィールド数 19)");
}

/// BROADCAST_TIME が 24 時間を超える場合: `25:30` 形式(分は 2 桁固定)。
#[test]
fn broadcast_time_over_24_hours() {
    let cid = "00000000000000000000000000000005";
    let mut listing = make_listing(cid);
    listing.starts = 1_000;
    listing.tip = None;
    listing.track = None;
    listing.summary = None;
    listing.genre = None;

    let ch = make_channel(cid, listing, 2_000);
    // 25 時間 30 分後
    let now = 1_000 + 25 * 3600 + 30 * 60; // 91800 + 1000 = 92800

    let out = generate(&[ch], IndexEncoding::Utf8, now);
    let text = std::str::from_utf8(&out).unwrap();
    let fields: Vec<&str> = text.trim_end_matches('\n').split("<>").collect();
    assert_eq!(fields.len(), 19);
    assert_eq!(fields[15], "25:30", "24 時間超の BROADCAST_TIME は時間部を拡張");
}

/// 分が 1 桁の場合も 2 桁固定でゼロ埋めされる。
#[test]
fn broadcast_time_minutes_zero_padded() {
    let cid = "00000000000000000000000000000006";
    let mut listing = make_listing(cid);
    listing.starts = 1_000;
    listing.tip = None;
    listing.track = None;
    listing.summary = None;
    listing.genre = None;

    let ch = make_channel(cid, listing, 2_000);
    let now = 1_000 + 3 * 3600 + 5 * 60; // 3:05

    let out = generate(&[ch], IndexEncoding::Utf8, now);
    let text = std::str::from_utf8(&out).unwrap();
    let fields: Vec<&str> = text.trim_end_matches('\n').split("<>").collect();
    assert_eq!(fields.len(), 19);
    assert_eq!(fields[15], "3:05", "分は 2 桁固定(ゼロ埋め)");
}

/// 不明フィールド: LISTENER_NUM / RELAY_NUM = -1、BITRATE = 0。
#[test]
fn unknown_counts_output_as_spec() {
    let cid = "00000000000000000000000000000007";
    let mut listing = make_listing(cid);
    listing.current_participants = UNKNOWN_COUNT; // -1
    listing.relays = UNKNOWN_COUNT;               // -1
    listing.bitrate_kbps = None;                  // → 0
    listing.tip = None;
    listing.track = None;
    listing.summary = None;
    listing.genre = None;

    let ch = make_channel(cid, listing, 2_000);
    let now = 1_000 + 60;

    let out = generate(&[ch], IndexEncoding::Utf8, now);
    let text = std::str::from_utf8(&out).unwrap();
    let fields: Vec<&str> = text.trim_end_matches('\n').split("<>").collect();
    assert_eq!(fields.len(), 19);
    assert_eq!(fields[6], "-1", "LISTENER_NUM 不明は -1");
    assert_eq!(fields[7], "-1", "RELAY_NUM 不明は -1");
    assert_eq!(fields[8], "0", "BITRATE 不明は 0");
}

/// 複数チャンネルが行数分の LF 区切り出力になる。
#[test]
fn multiple_channels_produce_multiple_lines() {
    let cid1 = "0000000000000000000000000000000a";
    let cid2 = "0000000000000000000000000000000b";
    let ch1 = make_channel(cid1, make_listing(cid1), 3_000);
    let ch2 = make_channel(cid2, make_listing(cid2), 2_000);
    let now = 1_000 + 3600;

    let out = generate(&[ch1, ch2], IndexEncoding::Utf8, now);
    let text = std::str::from_utf8(&out).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "2 チャンネル → 2 行");
    for line in &lines {
        let fields: Vec<&str> = line.split("<>").collect();
        assert_eq!(fields.len(), 19, "各行は 19 フィールド");
    }
}

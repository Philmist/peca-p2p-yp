//! 受信検証契約テスト(T035)
//!
//! contracts/nostr-events.md 受信検証 1〜6 の正常系+ネガティブと**ログ名の対応**を
//! 検査する。schema::verify_incoming(T015 実装)を契約の参照点として用いる。
//!
//! ログ名対応(data-model §SecurityEvent カテゴリ一覧を正とする):
//! - 16KB 超 → `event_oversize`
//! - 署名不正 → `event_invalid_sig`
//! - kind/タグ形式・内容範囲違反 → `event_invalid_format`
//! - created_at 未来 +300 秒超 → `event_time_skew`
//! - PoW 不足 → `event_pow_insufficient`
//!
//! さらにタグ省略 ⇔ 「不明 = -1」の往復復元(current_participants / relays)を検査する。

use nostr::{Event, EventBuilder, JsonUtil, Keys, Kind, Tag, Timestamp};

use peca_p2p_yp::event::schema::{CHANNEL_KIND, MAX_TAGS, verify_incoming};
use peca_p2p_yp::event::schema::{
    ChannelListing, ChannelStatus, DEFAULT_MAX_CLOCK_SKEW_SEC, MAX_BITRATE_KBPS, MAX_EVENT_BYTES,
    Track, UNKNOWN_COUNT, VerifyConfig, VerifyReject,
};
use peca_p2p_yp::security::SecurityCategory;

// ---------------------------------------------------------------------------
// 補助
// ---------------------------------------------------------------------------

const CREATED: u64 = 1_700_000_000;
const CHANNEL_ID: &str = "0123456789abcdef0123456789abcdef";

fn cfg() -> VerifyConfig {
    VerifyConfig::default()
}

/// 全タグを備えた正常な掲載情報。
fn full_listing() -> ChannelListing {
    ChannelListing {
        channel_id: CHANNEL_ID.into(),
        title: "テスト配信".into(),
        summary: Some("説明".into()),
        genre: Some("game".into()),
        status: ChannelStatus::Live,
        starts: CREATED,
        current_participants: 5,
        streaming: Some(format!("pcp://198.51.100.1:7144/{CHANNEL_ID}")),
        bitrate_kbps: Some(1500),
        content_type: Some("FLV".into()),
        tip: Some("198.51.100.1:7144".into()),
        contact: Some("https://example.com/".into()),
        relays: 3,
        track: Some(Track {
            title: "song".into(),
            artist: "artist".into(),
            album: "album".into(),
            url: String::new(),
        }),
    }
}

fn sign(listing: &ChannelListing, keys: &Keys, created: u64, pow: u8) -> Event {
    listing.sign(keys, created, pow).expect("署名できること")
}

/// 検証失敗の変種と、その [`SecurityCategory`] 名の対応を一括検証する。
fn assert_reject_maps_to(reject: &VerifyReject, expected_category: SecurityCategory, name: &str) {
    assert_eq!(
        reject.category(),
        expected_category,
        "{name}: VerifyReject が想定カテゴリに対応しない"
    );
    assert_eq!(
        reject.category().as_str(),
        expected_category.as_str(),
        "{name}: ログ名文字列が一致しない"
    );
}

// ---------------------------------------------------------------------------
// 正常系(受信検証 1〜6 通過)
// ---------------------------------------------------------------------------

#[test]
fn valid_event_passes_all_checks() {
    let keys = Keys::generate();
    let listing = full_listing();
    let event = sign(&listing, &keys, CREATED, 0);
    let verified = verify_incoming(&event.as_json(), &cfg(), CREATED).expect("正常系は通過する");
    assert_eq!(verified.listing, listing);
    assert_eq!(verified.event.pubkey, keys.public_key());
    assert_eq!(verified.event.kind.as_u16(), CHANNEL_KIND);
}

// ---------------------------------------------------------------------------
// 検証 1: サイズ → event_oversize
// ---------------------------------------------------------------------------

#[test]
fn check1_oversize_maps_to_event_oversize() {
    // 16KB を超える生バイト列はパース前に拒否される。
    let raw = "x".repeat(MAX_EVENT_BYTES + 1);
    let err = verify_incoming(&raw, &cfg(), CREATED).unwrap_err();
    assert_eq!(err, VerifyReject::Oversize);
    assert_reject_maps_to(&err, SecurityCategory::EventOversize, "oversize");
}

#[test]
fn check1_boundary_16kb_is_not_oversize() {
    // ちょうど 16KB(境界)は Oversize ではなく、中身の JSON 不正で弾かれる。
    let raw = "x".repeat(MAX_EVENT_BYTES);
    let err = verify_incoming(&raw, &cfg(), CREATED).unwrap_err();
    assert_ne!(
        err,
        VerifyReject::Oversize,
        "境界ちょうどは Oversize にしない"
    );
}

// ---------------------------------------------------------------------------
// 検証 2: 署名 → event_invalid_sig
// ---------------------------------------------------------------------------

#[test]
fn check2_tampered_content_maps_to_event_invalid_sig() {
    let keys = Keys::generate();
    let event = sign(&full_listing(), &keys, CREATED, 0);
    // content を改竄すると id 再計算が合わず署名検証に失敗する。
    let raw = event
        .as_json()
        .replace("\"content\":\"\"", "\"content\":\"x\"");
    let err = verify_incoming(&raw, &cfg(), CREATED).unwrap_err();
    assert_eq!(err, VerifyReject::InvalidSig);
    assert_reject_maps_to(&err, SecurityCategory::EventInvalidSig, "invalid_sig");
}

// ---------------------------------------------------------------------------
// 検証 3: kind/タグ形式 → event_invalid_format
// ---------------------------------------------------------------------------

#[test]
fn check3_wrong_kind_maps_to_invalid_format() {
    let keys = Keys::generate();
    let event = EventBuilder::new(Kind::Custom(1), "")
        .tags([
            Tag::parse(["d", CHANNEL_ID]).unwrap(),
            Tag::parse(["status", "live"]).unwrap(),
        ])
        .custom_created_at(Timestamp::from(CREATED))
        .sign_with_keys(&keys)
        .unwrap();
    let err = verify_incoming(&event.as_json(), &cfg(), CREATED).unwrap_err();
    assert_reject_maps_to(&err, SecurityCategory::EventInvalidFormat, "wrong_kind");
}

#[test]
fn check3_bad_d_tag_maps_to_invalid_format() {
    let keys = Keys::generate();
    let mut listing = full_listing();
    listing.channel_id = "NOTHEX".into();
    let event = sign(&listing, &keys, CREATED, 0);
    let err = verify_incoming(&event.as_json(), &cfg(), CREATED).unwrap_err();
    assert_reject_maps_to(&err, SecurityCategory::EventInvalidFormat, "bad_d");
}

#[test]
fn check3_too_many_tags_maps_to_invalid_format() {
    let keys = Keys::generate();
    let mut tags = vec![
        Tag::parse(["d", CHANNEL_ID]).unwrap(),
        Tag::parse(["title", "t"]).unwrap(),
        Tag::parse(["status", "live"]).unwrap(),
        Tag::parse(["starts", "1"]).unwrap(),
    ];
    while tags.len() <= MAX_TAGS {
        tags.push(Tag::parse(["t", &format!("g{}", tags.len())]).unwrap());
    }
    let keys2 = &keys;
    let event = EventBuilder::new(Kind::Custom(CHANNEL_KIND), "")
        .tags(tags)
        .custom_created_at(Timestamp::from(CREATED))
        .sign_with_keys(keys2)
        .unwrap();
    let err = verify_incoming(&event.as_json(), &cfg(), CREATED).unwrap_err();
    assert_reject_maps_to(&err, SecurityCategory::EventInvalidFormat, "too_many_tags");
}

// ---------------------------------------------------------------------------
// 検証 5: 内容範囲 → event_invalid_format
// ---------------------------------------------------------------------------

#[test]
fn check5_bitrate_out_of_range_maps_to_invalid_format() {
    let keys = Keys::generate();
    let mut listing = full_listing();
    listing.bitrate_kbps = Some(MAX_BITRATE_KBPS + 1);
    let event = sign(&listing, &keys, CREATED, 0);
    let err = verify_incoming(&event.as_json(), &cfg(), CREATED).unwrap_err();
    assert_reject_maps_to(&err, SecurityCategory::EventInvalidFormat, "bitrate");
}

#[test]
fn check5_bad_tip_maps_to_invalid_format() {
    let keys = Keys::generate();
    let mut listing = full_listing();
    listing.tip = Some("not-an-addr".into());
    let event = sign(&listing, &keys, CREATED, 0);
    let err = verify_incoming(&event.as_json(), &cfg(), CREATED).unwrap_err();
    assert_reject_maps_to(&err, SecurityCategory::EventInvalidFormat, "tip");
}

#[test]
fn check5_control_chars_map_to_invalid_format() {
    let keys = Keys::generate();
    let mut listing = full_listing();
    listing.title = "bad\u{7}title".into();
    let event = sign(&listing, &keys, CREATED, 0);
    let err = verify_incoming(&event.as_json(), &cfg(), CREATED).unwrap_err();
    assert_reject_maps_to(&err, SecurityCategory::EventInvalidFormat, "control");
}

// ---------------------------------------------------------------------------
// 検証 4: 時刻(未来方向のみ) → event_time_skew
// ---------------------------------------------------------------------------

#[test]
fn check4_future_skew_maps_to_event_time_skew() {
    let keys = Keys::generate();
    let event = sign(&full_listing(), &keys, CREATED, 0);
    // now がイベントより (skew + 1) 秒手前 = 許容超過の未来イベント。
    let now = CREATED - (DEFAULT_MAX_CLOCK_SKEW_SEC + 1);
    let err = verify_incoming(&event.as_json(), &cfg(), now).unwrap_err();
    assert_eq!(err, VerifyReject::TimeSkew);
    assert_reject_maps_to(&err, SecurityCategory::EventTimeSkew, "future_skew");
}

#[test]
fn check4_exact_skew_boundary_is_accepted() {
    let keys = Keys::generate();
    let event = sign(&full_listing(), &keys, CREATED, 0);
    // ちょうど許容境界(未来方向 = skew)は受理。
    let now = CREATED - DEFAULT_MAX_CLOCK_SKEW_SEC;
    assert!(verify_incoming(&event.as_json(), &cfg(), now).is_ok());
}

#[test]
fn check4_past_drift_is_not_time_skew() {
    // 過去方向のずれは検証 4 では拒否しない(鮮度窓の責務)。
    let keys = Keys::generate();
    let event = sign(&full_listing(), &keys, CREATED, 0);
    let now = CREATED + 10_000;
    assert!(verify_incoming(&event.as_json(), &cfg(), now).is_ok());
}

// ---------------------------------------------------------------------------
// 検証 6: PoW → event_pow_insufficient
// ---------------------------------------------------------------------------

#[test]
fn check6_pow_pass_and_insufficient() {
    let keys = Keys::generate();
    let event = sign(&full_listing(), &keys, CREATED, 8);
    let raw = event.as_json();

    let pass = VerifyConfig {
        max_clock_skew_sec: DEFAULT_MAX_CLOCK_SKEW_SEC,
        min_pow_bits: 8,
    };
    assert!(
        verify_incoming(&raw, &pass, CREATED).is_ok(),
        "8bit PoW は min 8 を満たす"
    );

    // 事実上到達不能な難易度は不足として拒否(決定的)。
    let strict = VerifyConfig {
        max_clock_skew_sec: DEFAULT_MAX_CLOCK_SKEW_SEC,
        min_pow_bits: 240,
    };
    let err = verify_incoming(&raw, &strict, CREATED).unwrap_err();
    assert_eq!(err, VerifyReject::PowInsufficient);
    assert_reject_maps_to(&err, SecurityCategory::EventPowInsufficient, "pow");
}

#[test]
fn check6_pow_disabled_by_default() {
    // min_pow_bits=0(既定)では PoW を要求しない。
    let keys = Keys::generate();
    let event = sign(&full_listing(), &keys, CREATED, 0);
    assert!(verify_incoming(&event.as_json(), &cfg(), CREATED).is_ok());
}

// ---------------------------------------------------------------------------
// タグ省略 ⇔ 不明(-1)の往復(current_participants / relays)
// ---------------------------------------------------------------------------

#[test]
fn unknown_counts_omit_tags_and_restore_as_minus_one() {
    let keys = Keys::generate();
    let mut listing = full_listing();
    listing.current_participants = UNKNOWN_COUNT;
    listing.relays = UNKNOWN_COUNT;
    let event = sign(&listing, &keys, CREATED, 0);

    // 省略が JSON に現れないこと(タグを直に走査)。
    let raw = event.as_json();
    assert!(
        !raw.contains("current_participants"),
        "不明時は current_participants タグを省略する"
    );
    assert!(!raw.contains("relays"), "不明時は relays タグを省略する");

    let verified = verify_incoming(&raw, &cfg(), CREATED).expect("省略は正常");
    assert_eq!(verified.listing.current_participants, UNKNOWN_COUNT);
    assert_eq!(verified.listing.relays, UNKNOWN_COUNT);
}

#[test]
fn known_counts_roundtrip_through_tags() {
    let keys = Keys::generate();
    let mut listing = full_listing();
    listing.current_participants = 12;
    listing.relays = 4;
    let event = sign(&listing, &keys, CREATED, 0);
    let verified = verify_incoming(&event.as_json(), &cfg(), CREATED).expect("既知値は正常");
    assert_eq!(verified.listing.current_participants, 12);
    assert_eq!(verified.listing.relays, 4);
}

#[test]
fn explicit_negative_count_tag_is_invalid_format() {
    // 負値は「省略」で表すため、明示的な負値タグは形式違反(往復規則の一貫性)。
    let keys = Keys::generate();
    let event = EventBuilder::new(Kind::Custom(CHANNEL_KIND), "")
        .tags([
            Tag::parse(["d", CHANNEL_ID]).unwrap(),
            Tag::parse(["title", "t"]).unwrap(),
            Tag::parse(["status", "live"]).unwrap(),
            Tag::parse(["starts", "1"]).unwrap(),
            Tag::parse(["current_participants", "-1"]).unwrap(),
        ])
        .custom_created_at(Timestamp::from(CREATED))
        .sign_with_keys(&keys)
        .unwrap();
    let err = verify_incoming(&event.as_json(), &cfg(), CREATED).unwrap_err();
    assert_reject_maps_to(
        &err,
        SecurityCategory::EventInvalidFormat,
        "explicit_negative",
    );
}

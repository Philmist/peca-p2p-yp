//! T017 スレイベント契約テスト(contracts/thread-events.md — kind 31311 announce)。
//!
//! 単体レベルの写像・タグ検証は `src/event/livechat.rs`・`src/event/schema.rs`・
//! `src/p2p/ingest.rs` の `#[cfg(test)]` が既に厚く覆っている。本ファイルは公開
//! クレート API のみを使い、gossip 受信パイプライン([`IngestState::ingest`])を
//! 通した **契約書レベルの振る舞い**を確認する:
//!
//! - 正常系: 署名済み announce が受信検証を通り、格納・再伝搬対象になる
//! - 置換規則: `(31311, pubkey, "livechat")` — 同一ペルソナの新しい announce が
//!   旧 announce を置換する(板 = ペルソナ単位で常に最新 1 本 — contract §31311)
//! - expiration 鮮度: `created_at + 600` を過ぎた announce は `sync_events` の
//!   対象から外れる(NIP-40 準拠・鮮度は受信ノードのローカル時計で判定)
//! - タグ形式: 必須タグ欠落・型不正・`tip` 不正形式は拒否され格納されない

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use nostr::{Event, JsonUtil, Keys};

use peca_p2p_yp::event::livechat::{ANNOUNCE_D, EXPIRATION_OFFSET_SECS, ThreadAnnounce};
use peca_p2p_yp::event::schema::{VerifyConfig, VerifyReject, verify_incoming_announce};
use peca_p2p_yp::event::store::{DedupCache, EventStore, StoreConfig};
use peca_p2p_yp::p2p::ingest::IngestState;

const CHANNEL_KIND: u16 = 30311;
const GUID: &str = "0123456789abcdef0123456789abcdef";

fn channel_ref(pubkey: &str) -> String {
    format!("{CHANNEL_KIND}:{pubkey}:{GUID}")
}

fn sample_announce(pubkey: &str, title: &str) -> ThreadAnnounce {
    ThreadAnnounce {
        channel: channel_ref(pubkey),
        title: title.into(),
        generation: 1,
        key: 1_700_000_000,
        res_count: Some(0),
        tip: "198.51.100.1:7147".into(),
    }
}

/// クロック注入済みの `IngestState`(時刻を明示的に進められる)。
fn state_at(clock: Arc<AtomicU64>) -> IngestState {
    let cfg = StoreConfig::default();
    let c2 = Arc::clone(&clock);
    let store = EventStore::with_clock(cfg, Box::new(move || c2.load(Ordering::SeqCst)));
    let c3 = Arc::clone(&clock);
    let dedup = DedupCache::with_clock(
        cfg.freshness_window_sec,
        Box::new(move || c3.load(Ordering::SeqCst)),
    );
    IngestState::with_parts(store, dedup, VerifyConfig::default(), cfg)
}

/// 単純タグ `[name, value]` の値。
fn tag_value<'a>(event: &'a Event, name: &str) -> Option<&'a str> {
    event.tags.iter().find_map(|tag| {
        let s = tag.as_slice();
        if s.first().map(String::as_str) == Some(name) {
            s.get(1).map(String::as_str)
        } else {
            None
        }
    })
}

// ---------------------------------------------------------------------------
// 正常系
// ---------------------------------------------------------------------------

#[test]
fn valid_announce_is_stored_and_marked_for_propagation() {
    let now = 1_700_000_000;
    let clock = Arc::new(AtomicU64::new(now));
    let mut st = state_at(Arc::clone(&clock));
    let keys = Keys::generate();
    let pubkey = keys.public_key().to_hex();
    let event = sample_announce(&pubkey, "実況スレ")
        .sign(&keys, now, 0)
        .unwrap();

    let out = st.ingest(&event.as_json(), "peer:1", now).unwrap();
    assert!(out.is_some(), "署名者一致の announce は再伝搬対象");
    assert_eq!(st.store_len(), 1);

    // sync_events(接続時同期)にも現れる — 鮮度窓内かつ live。
    let synced = st.sync_events(0, now);
    assert_eq!(synced.len(), 1);
    assert_eq!(synced[0].kind.as_u16(), 31311);
    assert_eq!(tag_value(&synced[0], "d"), Some(ANNOUNCE_D));
}

#[test]
fn announce_content_is_empty() {
    let now = 1_700_000_000;
    let keys = Keys::generate();
    let pubkey = keys.public_key().to_hex();
    let event = sample_announce(&pubkey, "実況スレ")
        .sign(&keys, now, 0)
        .unwrap();
    assert_eq!(event.content, "", "announce の content は空文字列");
}

#[test]
fn announce_sign_then_from_event_roundtrips_all_fields() {
    // sign → from_event で全フィールドが往復すること(contract §kind 31311 の写像表)。
    let now = 1_700_000_000;
    let keys = Keys::generate();
    let pubkey = keys.public_key().to_hex();
    let announce = sample_announce(&pubkey, "実況スレ");
    let event = announce.sign(&keys, now, 0).unwrap();
    let restored = ThreadAnnounce::from_event(&event).unwrap();
    assert_eq!(
        restored, announce,
        "sign/from_event の往復で全フィールド一致"
    );
}

// ---------------------------------------------------------------------------
// 置換規則: (31311, pubkey, "livechat") — ペルソナ単位で常に最新 1 件
// ---------------------------------------------------------------------------

#[test]
fn same_persona_new_announce_replaces_old_one() {
    let now = 1_700_000_000;
    let clock = Arc::new(AtomicU64::new(now));
    let mut st = state_at(Arc::clone(&clock));
    let keys = Keys::generate();
    let pubkey = keys.public_key().to_hex();

    let first = sample_announce(&pubkey, "1本目のスレ")
        .sign(&keys, now, 0)
        .unwrap();
    assert!(
        st.ingest(&first.as_json(), "peer:1", now)
            .unwrap()
            .is_some()
    );
    assert_eq!(st.store_len(), 1);

    // 同一ペルソナ・より新しい created_at で 2 本目を発行 → d タグが同じ("livechat")
    // なので置換され、常に 1 件のまま。
    let later = now + 10;
    clock.store(later, Ordering::SeqCst);
    let second = sample_announce(&pubkey, "2本目のスレ")
        .sign(&keys, later, 0)
        .unwrap();
    let out = st.ingest(&second.as_json(), "peer:1", later).unwrap();
    assert!(out.is_some(), "新しい announce は置換により再伝搬対象");
    assert_eq!(
        st.store_len(),
        1,
        "板はペルソナ単位で常に最新 1 本に置換される"
    );

    let synced = st.sync_events(0, later);
    assert_eq!(synced.len(), 1);
    assert_eq!(tag_value(&synced[0], "title"), Some("2本目のスレ"));
}

#[test]
fn older_announce_from_same_persona_does_not_replace_newer() {
    let now = 1_700_000_000;
    let clock = Arc::new(AtomicU64::new(now));
    let mut st = state_at(Arc::clone(&clock));
    let keys = Keys::generate();
    let pubkey = keys.public_key().to_hex();

    let newer = sample_announce(&pubkey, "新しい方")
        .sign(&keys, now + 10, 0)
        .unwrap();
    assert!(
        st.ingest(&newer.as_json(), "peer:1", now + 10)
            .unwrap()
            .is_some()
    );

    // より古い created_at の announce(同一ペルソナ)は既存の新しい方を破壊しない。
    let older = sample_announce(&pubkey, "古い方")
        .sign(&keys, now, 0)
        .unwrap();
    let out = st.ingest(&older.as_json(), "peer:2", now + 10).unwrap();
    assert!(
        out.is_none(),
        "旧版は格納も再伝搬もされない(last-write-wins)"
    );
    assert_eq!(st.store_len(), 1);

    let synced = st.sync_events(0, now + 10);
    assert_eq!(tag_value(&synced[0], "title"), Some("新しい方"));
}

#[test]
fn different_persona_announces_coexist() {
    let now = 1_700_000_000;
    let clock = Arc::new(AtomicU64::new(now));
    let mut st = state_at(Arc::clone(&clock));
    let keys_a = Keys::generate();
    let keys_b = Keys::generate();

    let a = sample_announce(&keys_a.public_key().to_hex(), "板A")
        .sign(&keys_a, now, 0)
        .unwrap();
    let b = sample_announce(&keys_b.public_key().to_hex(), "板B")
        .sign(&keys_b, now, 0)
        .unwrap();
    assert!(st.ingest(&a.as_json(), "peer:1", now).unwrap().is_some());
    assert!(st.ingest(&b.as_json(), "peer:1", now).unwrap().is_some());
    assert_eq!(
        st.store_len(),
        2,
        "置換キーに pubkey を含むため別ペルソナは共存する"
    );
}

// ---------------------------------------------------------------------------
// expiration 鮮度(NIP-40 — created_at + 600、ローカル時計で判定)
// ---------------------------------------------------------------------------

#[test]
fn announce_expires_after_offset_and_drops_from_sync() {
    let now = 1_700_000_000;
    let clock = Arc::new(AtomicU64::new(now));
    let mut st = state_at(Arc::clone(&clock));
    let keys = Keys::generate();
    let pubkey = keys.public_key().to_hex();
    let event = sample_announce(&pubkey, "実況スレ")
        .sign(&keys, now, 0)
        .unwrap();
    assert_eq!(
        tag_value(&event, "expiration"),
        Some((now + EXPIRATION_OFFSET_SECS).to_string().as_str())
    );
    assert!(
        st.ingest(&event.as_json(), "peer:1", now)
            .unwrap()
            .is_some()
    );

    // expiration ちょうどまでは live(鮮度窓は別軸)。
    let just_before = now + EXPIRATION_OFFSET_SECS - 1;
    assert_eq!(st.sync_events(0, just_before).len(), 1);

    // expiration を過ぎると live_fresh_events から外れる。
    let after = now + EXPIRATION_OFFSET_SECS + 1;
    assert!(
        st.sync_events(0, after).is_empty(),
        "expiration 超過の announce は同期対象から外れる"
    );
}

// ---------------------------------------------------------------------------
// タグ形式(必須タグ欠落・型不正・tip 形式)
// ---------------------------------------------------------------------------

#[test]
fn rejects_invalid_tip_format() {
    let now = 1_700_000_000;
    let keys = Keys::generate();
    let pubkey = keys.public_key().to_hex();
    let mut announce = sample_announce(&pubkey, "実況スレ");
    announce.tip = "not-an-addr".into();
    // sign 自体が tip 形式を検査するため、ここでは事前に拒否される(発行側検査)。
    assert!(announce.sign(&keys, now, 0).is_err());
}

#[test]
fn rejects_title_too_long() {
    let now = 1_700_000_000;
    let keys = Keys::generate();
    let pubkey = keys.public_key().to_hex();
    let mut announce = sample_announce(&pubkey, "実況スレ");
    announce.title = "あ".repeat(129);
    assert!(
        announce.sign(&keys, now, 0).is_err(),
        "128 文字超のタイトルは拒否"
    );
}

#[test]
fn rejects_missing_required_tag_via_raw_event() {
    // 受信側が任意の JSON を送れる前提のため、Rust の型では表現できない「タグ欠落」を
    // 生イベント JSON を組み立てて再現する(受信検証の防御をブラックボックスで確認)。
    use nostr::{EventBuilder, Kind, Tag, Timestamp};

    let now = 1_700_000_000;
    let clock = Arc::new(AtomicU64::new(now));
    let mut st = state_at(Arc::clone(&clock));
    let keys = Keys::generate();
    let pubkey = keys.public_key().to_hex();

    // tip タグを欠落させた 31311(必須タグ違反)。
    let tags = vec![
        Tag::parse(["d", ANNOUNCE_D]).unwrap(),
        Tag::parse(["a", &channel_ref(&pubkey)]).unwrap(),
        Tag::parse(["title", "実況スレ"]).unwrap(),
        Tag::parse(["gen", "1"]).unwrap(),
        Tag::parse(["key", &now.to_string()]).unwrap(),
        Tag::parse(["expiration", &(now + EXPIRATION_OFFSET_SECS).to_string()]).unwrap(),
    ];
    let event = EventBuilder::new(Kind::Custom(31311), "")
        .tags(tags)
        .custom_created_at(Timestamp::from(now))
        .sign_with_keys(&keys)
        .unwrap();

    let err = st.ingest(&event.as_json(), "peer:1", now).unwrap_err();
    assert_eq!(st.store_len(), 0, "tip 欠落の announce は格納されない");
    // 形式違反として拒否される(署名は正しいため InvalidSig ではない)。
    assert!(
        format!("{err:?}").contains("InvalidFormat"),
        "tip 欠落は形式違反として拒否される: {err:?}"
    );
}

#[test]
fn res_count_tag_is_optional() {
    let now = 1_700_000_000;
    let clock = Arc::new(AtomicU64::new(now));
    let mut st = state_at(Arc::clone(&clock));
    let keys = Keys::generate();
    let pubkey = keys.public_key().to_hex();
    let mut announce = sample_announce(&pubkey, "実況スレ");
    announce.res_count = None;
    let event = announce.sign(&keys, now, 0).unwrap();
    assert!(tag_value(&event, "res_count").is_none());
    assert!(
        st.ingest(&event.as_json(), "peer:1", now)
            .unwrap()
            .is_some()
    );
}

// ---------------------------------------------------------------------------
// ペルソナ一致(検査 #7 — a タグの pubkey == 署名者。FR-003)
// ---------------------------------------------------------------------------

#[test]
fn verify_incoming_announce_accepts_matching_persona() {
    let now = 1_700_000_000;
    let keys = Keys::generate();
    let pubkey = keys.public_key().to_hex();
    let event = sample_announce(&pubkey, "実況スレ")
        .sign(&keys, now, 0)
        .unwrap();
    let verified = verify_incoming_announce(&event.as_json(), &VerifyConfig::default(), now)
        .expect("a タグの pubkey と署名者が一致すれば受理される");
    assert_eq!(verified.event.pubkey.to_hex(), pubkey);
}

#[test]
fn verify_incoming_announce_rejects_persona_mismatch() {
    let now = 1_700_000_000;
    let signer = Keys::generate();
    let other = Keys::generate();
    // a タグは other の pubkey、署名は signer(不一致 → FR-003 違反)。
    let event = sample_announce(&other.public_key().to_hex(), "偽装スレ")
        .sign(&signer, now, 0)
        .unwrap();
    let err = verify_incoming_announce(&event.as_json(), &VerifyConfig::default(), now)
        .expect_err("a タグの pubkey と署名者が不一致なら拒否される");
    assert_eq!(err, VerifyReject::AnnouncePersonaMismatch);
}

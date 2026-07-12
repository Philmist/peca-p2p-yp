//! T017/T027 スレイベント契約テスト(contracts/thread-events.md)。
//!
//! 単体レベルの写像・タグ検証は `src/event/livechat.rs`・`src/event/schema.rs`・
//! `src/p2p/ingest.rs`・`src/livechat/registry.rs` の `#[cfg(test)]` が既に厚く
//! 覆っている。本ファイルは公開クレート API のみを使い、受信パイプラインを通した
//! **契約書レベルの振る舞い**を確認する:
//!
//! ## kind 31311 announce(T017)
//! - 正常系: 署名済み announce が受信検証を通り、格納・再伝搬対象になる
//! - 置換規則: `(31311, pubkey, "livechat")` — 同一ペルソナの新しい announce が
//!   旧 announce を置換する(板 = ペルソナ単位で常に最新 1 本 — contract §31311)
//! - expiration 鮮度: `created_at + 600` を過ぎた announce は `sync_events` の
//!   対象から外れる(NIP-40 準拠・鮮度は受信ノードのローカル時計で判定)
//! - タグ形式: 必須タグ欠落・型不正・`tip` 不正形式は拒否され格納されない
//!
//! ## kind 1311 レス・ホスト側受信検証(T027)
//! `contracts/thread-events.md §ホスト側受信検証` の検証順序 1〜7 のうち、
//! [`LivechatRegistry::verify_incoming_res`] が実装済みの 1(署名)〜4(スレ状態)を
//! [`LivechatRegistry`] を通した契約テストで確認する。5(BAN)〜7(レート)は US2/US4
//! で実装されるため、本ファイルでは「該当なしとして正常系を妨げない」ことのみ確認する
//! (BAN/PoW/レート機構自体の契約テストは US2/US4 のタスクで追加される)。
//! - name 欄の `#` 以降がホスト側でも除去される(二重防御 — FR-024)。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use nostr::{Event, JsonUtil, Keys};

use peca_p2p_yp::event::livechat::{
    ANNOUNCE_D, EXPIRATION_OFFSET_SECS, Res as ResEnvelope, ThreadAnnounce,
};
use peca_p2p_yp::event::schema::{VerifyConfig, VerifyReject, verify_incoming_announce};
use peca_p2p_yp::event::store::{DedupCache, EventStore, StoreConfig};
use peca_p2p_yp::livechat::registry::LivechatRegistry;
use peca_p2p_yp::livechat::thread::BoardSettings;
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

// ---------------------------------------------------------------------------
// T027: kind 1311 レス・ホスト側受信検証(contracts/thread-events.md §受信検証)
// ---------------------------------------------------------------------------

const THREAD_GUID: &str = "0123456789abcdef0123456789abcdef";

fn thread_channel_ref(board_id: &str) -> String {
    format!("30311:{board_id}:{THREAD_GUID}")
}

/// 開設済みスレを 1 本持つレジストリを作る(ホスト側受信検証のテスト用)。
fn registry_with_open_thread(persona: &Keys) -> Arc<LivechatRegistry> {
    let reg = LivechatRegistry::new(128);
    let board_id = persona.public_key().to_hex();
    reg.open_thread(
        persona.clone(),
        thread_channel_ref(&board_id),
        1,
        1_700_000_000,
        "実況スレ",
        BoardSettings::default(),
        "198.51.100.1:7147",
    )
    .unwrap();
    reg
}

/// 板鍵で kind 1311(レス)を署名する(name/mail を指定できる版 — registry.rs の
/// `sign_res` は name 固定 None のため、`#` 除去テスト用に本ファイルで別途用意する)。
fn sign_res_with_name(
    board_key: &Keys,
    board_id: &str,
    generation: u32,
    name: Option<&str>,
    body: &str,
    created_at: u64,
) -> Event {
    ResEnvelope {
        channel: thread_channel_ref(board_id),
        board_id: board_id.to_string(),
        generation,
        name: name.map(str::to_string),
        mail: None,
        body: body.to_string(),
    }
    .sign(board_key, created_at, 0)
    .unwrap()
}

/// `Res` 封筒をそのまま作る(sign 失敗を `Result` で受け取りたいテスト用)。
fn sample_res_envelope(board_id: &str, body: &str) -> ResEnvelope {
    ResEnvelope {
        channel: thread_channel_ref(board_id),
        board_id: board_id.to_string(),
        generation: 1,
        name: None,
        mail: None,
        body: body.to_string(),
    }
}

#[test]
fn verify_incoming_res_accepts_valid_write_checks_1_through_4() {
    // 検証 1(署名)〜4(スレ状態 Active)の正常系: 妥当な書き込みは受理される。
    let persona = Keys::generate();
    let board_id = persona.public_key().to_hex();
    let reg = registry_with_open_thread(&persona);
    let board_key = Keys::generate();

    let res = sign_res_with_name(&board_key, &board_id, 1, None, "書き込み", 1_700_000_005);
    assert!(
        reg.verify_incoming_res(&board_id, &res),
        "署名済み・形式正しい・Active スレ宛の RES は受理される"
    );
}

#[test]
fn verify_incoming_res_rejects_tampered_signature_check_1() {
    // 検証 1(署名): id/sig 改竄は拒否(封筒の真正性)。
    let persona = Keys::generate();
    let board_id = persona.public_key().to_hex();
    let reg = registry_with_open_thread(&persona);
    let board_key = Keys::generate();

    let res = sign_res_with_name(&board_key, &board_id, 1, None, "本文", 1_700_000_005);
    let tampered_json = res
        .as_json()
        .replace("\"content\":\"本文\"", "\"content\":\"改竄\"");
    let tampered = Event::from_json(&tampered_json).unwrap();
    assert!(
        !reg.verify_incoming_res(&board_id, &tampered),
        "id/sig が一致しない改竄イベントは拒否される"
    );
}

#[test]
fn verify_incoming_res_check_3_hash_stripped_before_signing_by_client() {
    // 検証 3(形式)の一部: 送信クライアントは `#` 以降を送信前に除去する仕様(FR-024)。
    // 正規のクライアント経路(ResEnvelope::sign)では署名前に既に除去済みであることを
    // イベント側で確認する(送信側の一次防御)。
    let board_key = Keys::generate();
    let board_id = "ab".repeat(32);
    let event = sign_res_with_name(&board_key, &board_id, 1, Some("コテハン#ひみつ"), "本文", 1);
    // peca name タグの値には `#` 以降が含まれない(送信前除去 — FR-024)。
    let name_tag = event
        .tags
        .iter()
        .map(|t| t.as_slice())
        .find(|s| {
            s.first().map(String::as_str) == Some("peca")
                && s.get(1).map(String::as_str) == Some("name")
        })
        .expect("name タグが付与される");
    assert_eq!(name_tag.get(2).map(String::as_str), Some("コテハン"));
}

#[test]
fn verify_incoming_res_check_3_host_side_removes_residual_hash() {
    // 検証 3(形式)の二重防御: クライアントの除去を経ずにタグへ直接 `#` 以降を残した
    // 生イベントを組み立て、ホスト側の復元(ResEnvelope::from_event)でも除去されることを
    // 確認する(thread-events.md: 「name に `#` が残っていればホスト側でも除去」)。
    use nostr::{EventBuilder, Kind, Tag, Timestamp};

    let board_key = Keys::generate();
    let board_id = "cd".repeat(32);
    let tags = vec![
        Tag::parse(["a", &thread_channel_ref(&board_id)]).unwrap(),
        Tag::parse(["peca", "thread", &board_id, "1"]).unwrap(),
        // クライアント側の除去をバイパスし `#` 以降を残したまま送信されたケースを模す。
        Tag::parse(["peca", "name", "コテハン#残存ひみつ"]).unwrap(),
    ];
    let event = EventBuilder::new(Kind::Custom(1311), "本文")
        .tags(tags)
        .custom_created_at(Timestamp::from(1u64))
        .sign_with_keys(&board_key)
        .unwrap();

    let restored = ResEnvelope::from_event(&event).expect("形式検証は通る");
    assert_eq!(
        restored.name.as_deref(),
        Some("コテハン"),
        "ホスト側復元でも `#` 以降が除去される(二重防御 — FR-024)"
    );
}

#[test]
fn verify_incoming_res_rejects_wrong_generation_check_3() {
    // 検証 3(形式・対象スレ一致の一部): 別世代宛の RES は対象スレ不一致で拒否。
    let persona = Keys::generate();
    let board_id = persona.public_key().to_hex();
    let reg = registry_with_open_thread(&persona);
    let board_key = Keys::generate();

    let res = sign_res_with_name(&board_key, &board_id, 2, None, "別世代", 1_700_000_005);
    assert!(
        !reg.verify_incoming_res(&board_id, &res),
        "スレ世代が一致しない RES は拒否される(不変条件 T1/T2)"
    );
}

#[test]
fn verify_incoming_res_rejects_unknown_board_check_3() {
    // 検証 3(対象スレ一致): 未開設の板宛の RES は拒否。
    let reg = LivechatRegistry::new(128);
    let board_key = Keys::generate();
    let board_id = "ef".repeat(32);

    let res = sign_res_with_name(&board_key, &board_id, 1, None, "未知板", 1_700_000_005);
    assert!(
        !reg.verify_incoming_res(&board_id, &res),
        "開設されていない板への書き込みは拒否される"
    );
}

#[test]
fn verify_incoming_res_accepts_only_while_active_check_4() {
    // 検証 4(スレ状態): 開設直後の Active スレへの書き込みは受理される(不変条件 T1 の
    // 順方向)。`LivechatRegistry` は板ごとのホスト状態を非公開に保持しており、契約テスト
    // (公開 API のみ)から Frozen/Closed へ直接遷移させる手段が無いため、否定側
    // (Frozen/Closed 拒否)の直接確認は `src/livechat/registry.rs` の
    // `verify_incoming_res_rejects_when_frozen`(内部状態へアクセスできる同一クレート内
    // テスト)が担う。本テストは Active な板が正常に受理されることを契約レベルで固定する。
    let persona = Keys::generate();
    let board_id = persona.public_key().to_hex();
    let reg = registry_with_open_thread(&persona);
    let board_key = Keys::generate();
    let res = sign_res_with_name(&board_key, &board_id, 1, None, "本文", 1_700_000_005);
    assert!(
        reg.verify_incoming_res(&board_id, &res),
        "開設直後(Active)のスレへの書き込みは受理される"
    );
}

#[test]
fn verify_incoming_res_size_within_common_limit_check_1() {
    // 検証 1 に先立つサイズ上限(共通 16KB)は 30311/31311 と同一の
    // MAX_EVENT_BYTES を踏襲する契約(thread-events.md 検証 1)。1311 の通常サイズの
    // レス(≤ 2048 文字本文)はこの上限に収まることを確認する(正常系)。
    use peca_p2p_yp::event::schema::MAX_EVENT_BYTES;

    let persona = Keys::generate();
    let board_id = persona.public_key().to_hex();
    let reg = registry_with_open_thread(&persona);
    let board_key = Keys::generate();

    // 本文上限ちょうど(2048 文字)でも直列化イベントは 16KB に収まる。
    let body = "あ".repeat(2048);
    let res = sign_res_with_name(&board_key, &board_id, 1, None, &body, 1_700_000_005);
    assert!(res.as_json().len() <= MAX_EVENT_BYTES);
    assert!(reg.verify_incoming_res(&board_id, &res));
}

#[test]
fn res_sign_then_from_event_roundtrips_all_fields() {
    // 正常系: sign → from_event で全フィールドが往復する(検証 1〜3 を通した基本契約)。
    // event.kind==1311・content==本文であることも併せて確認する。
    let board_key = Keys::generate();
    let board_id = "12".repeat(32);
    let envelope = ResEnvelope {
        channel: thread_channel_ref(&board_id),
        board_id: board_id.clone(),
        generation: 1,
        name: Some("名無し".into()),
        mail: Some("sage".into()),
        body: "本文\nテスト >>1".into(),
    };
    let event = envelope.sign(&board_key, 1_700_000_000, 0).unwrap();

    assert_eq!(event.kind.as_u16(), 1311);
    assert_eq!(event.content, "本文\nテスト >>1", "content == 本文");

    let restored = ResEnvelope::from_event(&event).unwrap();
    assert_eq!(
        restored, envelope,
        "sign/from_event の往復で全フィールド一致"
    );
}

#[test]
fn res_body_over_2048_chars_is_rejected_at_sign() {
    // 検証 3(本文制約): 本文 2048 文字超は sign 時点で拒否される(発行側検査)。
    let board_id = "34".repeat(32);
    let mut envelope = sample_res_envelope(&board_id, "");
    envelope.body = "あ".repeat(2049);
    let err = envelope
        .sign(&Keys::generate(), 1_700_000_000, 0)
        .unwrap_err();
    assert!(
        matches!(
            err,
            peca_p2p_yp::event::livechat::LivechatBuildError::Invalid(_)
        ),
        "本文 2048 文字超は Invalid で拒否される: {err:?}"
    );
}

#[test]
fn res_body_over_32_lines_is_rejected_at_sign() {
    // 検証 3(本文制約): 33 行(32 行超)は sign 時点で拒否される。
    let board_id = "56".repeat(32);
    let mut envelope = sample_res_envelope(&board_id, "");
    envelope.body = "x\n".repeat(32).trim_end().to_string() + "\ny"; // 33 行
    let err = envelope
        .sign(&Keys::generate(), 1_700_000_000, 0)
        .unwrap_err();
    assert!(
        matches!(
            err,
            peca_p2p_yp::event::livechat::LivechatBuildError::Invalid(_)
        ),
        "33 行(32 行超)は Invalid で拒否される: {err:?}"
    );
}

#[test]
fn res_body_control_chars_removed_but_newline_kept() {
    // 検証 3(本文制約): 制御文字は除去されるが改行(`\n`)は残る(data-model §Res)。
    let board_key = Keys::generate();
    let board_id = "78".repeat(32);
    let mut envelope = sample_res_envelope(&board_id, "");
    envelope.body = "行1\n\u{7}制御\t除去".into();
    let event = envelope.sign(&board_key, 1_700_000_000, 0).unwrap();
    assert_eq!(event.content, "行1\n制御除去");
}

#[test]
fn res_name_over_64_chars_is_rejected_at_sign() {
    // 検証 3(name 制約): 64 文字超は sign 時点で拒否される。
    let board_id = "9a".repeat(32);
    let mut envelope = sample_res_envelope(&board_id, "本文");
    envelope.name = Some("あ".repeat(65));
    let err = envelope
        .sign(&Keys::generate(), 1_700_000_000, 0)
        .unwrap_err();
    assert!(
        matches!(
            err,
            peca_p2p_yp::event::livechat::LivechatBuildError::Invalid(_)
        ),
        "name 64 文字超は Invalid で拒否される: {err:?}"
    );
}

#[test]
fn res_mail_over_64_chars_is_rejected_at_sign() {
    // 検証 3(mail 制約): 64 文字超は sign 時点で拒否される。
    let board_id = "bc".repeat(32);
    let mut envelope = sample_res_envelope(&board_id, "本文");
    envelope.mail = Some("あ".repeat(65));
    let err = envelope
        .sign(&Keys::generate(), 1_700_000_000, 0)
        .unwrap_err();
    assert!(
        matches!(
            err,
            peca_p2p_yp::event::livechat::LivechatBuildError::Invalid(_)
        ),
        "mail 64 文字超は Invalid で拒否される: {err:?}"
    );
}

#[test]
fn res_empty_name_becomes_none_noname() {
    // 検証 3: 名前欄が空文字列(明示的な空指定)は名無し(None)として扱われる
    // (省略時と同じ表現に正規化 — 表示側が noname_name で補完する契約)。
    let board_key = Keys::generate();
    let board_id = "de".repeat(32);
    let mut envelope = sample_res_envelope(&board_id, "本文");
    envelope.name = Some(String::new());
    let event = envelope.sign(&board_key, 1_700_000_000, 0).unwrap();

    // 空 name はタグ自体を付与しない(sign 側の仕様)。
    let has_name_tag = event.tags.iter().any(|t| {
        let s = t.as_slice();
        s.first().map(String::as_str) == Some("peca")
            && s.get(1).map(String::as_str) == Some("name")
    });
    assert!(!has_name_tag, "空 name はタグを付与しない");

    let restored = ResEnvelope::from_event(&event).unwrap();
    assert_eq!(
        restored.name, None,
        "空 name は名無し(None)として復元される"
    );
}

#[test]
fn res_from_event_requires_peca_thread_tag() {
    // 検証 3・5: `["peca","thread",board_id,gen]` は必須。欠落は生イベントを組み立てて
    // ブラックボックスで確認する(Rust の型では表現できない欠落を再現)。
    use nostr::{EventBuilder, Kind, Tag, Timestamp};

    let board_key = Keys::generate();
    let board_id = "e1".repeat(32);
    // peca thread タグを欠落させた 1311。
    let tags = vec![Tag::parse(["a", &thread_channel_ref(&board_id)]).unwrap()];
    let event = EventBuilder::new(Kind::Custom(1311), "本文")
        .tags(tags)
        .custom_created_at(Timestamp::from(1u64))
        .sign_with_keys(&board_key)
        .unwrap();

    assert!(
        ResEnvelope::from_event(&event).is_err(),
        "peca thread タグ欠落は形式違反として拒否される"
    );
}

#[test]
fn res_from_event_rejects_non_hex64_board_id_in_thread_tag() {
    // 検証 3: `["peca","thread",<board_id>,<gen>]` の board_id は hex64 でなければ
    // ならない。不正形式(短い・非 hex)の生イベントは拒否される。
    use nostr::{EventBuilder, Kind, Tag, Timestamp};

    let board_key = Keys::generate();
    let tags = vec![
        Tag::parse(["a", "30311:notahexpubkey:notahexguid"]).unwrap(),
        Tag::parse(["peca", "thread", "not-hex-64", "1"]).unwrap(),
    ];
    let event = EventBuilder::new(Kind::Custom(1311), "本文")
        .tags(tags)
        .custom_created_at(Timestamp::from(1u64))
        .sign_with_keys(&board_key)
        .unwrap();

    assert!(
        ResEnvelope::from_event(&event).is_err(),
        "board_id が hex64 でない peca thread タグは拒否される"
    );
}

#[test]
fn res_from_event_ignores_unknown_tags_and_peca_subtags() {
    // 前方互換(MUST): 未知タグ・未知 peca サブタグを付けても from_event は成功する
    // (001 の HELLO features / タグ規則と同一の前方互換規則 — thread-events.md)。
    use nostr::{EventBuilder, Kind, Tag, Timestamp};

    let board_key = Keys::generate();
    let board_id = "f2".repeat(32);
    let envelope = ResEnvelope {
        channel: thread_channel_ref(&board_id),
        board_id: board_id.clone(),
        generation: 1,
        name: None,
        mail: None,
        body: "本文".into(),
    };
    let base = envelope.sign(&board_key, 1_700_000_000, 0).unwrap();

    // 既存タグ + 未知タグ/未知 peca サブタグを付けて再署名。
    let mut tags: Vec<Tag> = base.tags.iter().cloned().collect();
    tags.push(Tag::parse(["futuretag", "value"]).unwrap());
    tags.push(Tag::parse(["peca", "unknownsub", "x"]).unwrap());
    let event = EventBuilder::new(Kind::Custom(1311), "本文")
        .tags(tags)
        .custom_created_at(Timestamp::from(1_700_000_000u64))
        .sign_with_keys(&board_key)
        .unwrap();

    let restored = ResEnvelope::from_event(&event).unwrap();
    assert_eq!(
        restored, envelope,
        "未知タグ・未知 peca サブタグを追加しても復元は成功し内容は変わらない(前方互換 MUST)"
    );
}

// ---------------------------------------------------------------------------
// T034: 契約ネガティブテスト(イベント) — なりすまし・不正耐性(US3)
//
// 「妥当に見えるが不正な」イベントが受理・伝搬・確定されないことを固定する。
// - 署名不一致 announce(ペルソナ不一致)は T017 の
//   `verify_incoming_announce_rejects_persona_mismatch` で既にカバー済みのため
//   本節では重複させない。
// - スレ主以外の鍵で署名した ORDER(21311)は偽の順序確定情報であり、参加者は
//   取り込んではならない(FR-011)。
// - 直列化イベント全体が共通上限(16KB)を超える announce/レスは受信検証で拒否される。
// - kind 1311/21311 が gossip(EVENT メッセージ)に混入した場合は破棄する
//   (thread-events.md §受信検証: 「gossip に流してはならない」の受信側規範)。
// ---------------------------------------------------------------------------

use peca_p2p_yp::event::livechat::{OrderEntry, OrderInfo};

#[test]
fn order_signed_by_non_board_persona_is_rejected_as_invalid() {
    // FR-011: ORDER の署名者はスレ主ペルソナ(board_id)でなければならない。
    // 別ペルソナが「スレ主のふりをして」署名した ORDER は偽物として拒否される。
    // 検証は OrderInfo::from_event(形式)ではなく、署名者 pubkey と board_id の一致を
    // 別途確認する配線側の責務(participant.rs の verify_order と同型)である点を、
    // 公開 API のみで固定する。
    let board_persona = Keys::generate();
    let board_id = board_persona.public_key().to_hex();
    let attacker = Keys::generate();

    let order_envelope = OrderInfo {
        board_id: board_id.clone(),
        generation: 1,
        seq: 1,
        entries: vec![OrderEntry {
            res_no: 1,
            event_id: "aa".repeat(32),
        }],
    };
    // 攻撃者の鍵で署名(board_id タグの値は本物のスレ主のものを詐称)。
    let event = order_envelope.sign(&attacker, 1_700_000_000).unwrap();

    // 形式検証(from_event)自体は署名者を見ないため通ってしまう(タグは正しい形式)。
    let restored = OrderInfo::from_event(&event).expect("形式検証は通る(署名者は見ない)");
    assert_eq!(restored.board_id, board_id);

    // 署名者 pubkey が board_id と一致しないことが偽 ORDER の判定根拠(FR-011)。
    // 受信側(participant 配線)はこの不一致を検出して破棄しなければならない。
    assert_ne!(
        event.pubkey.to_hex(),
        board_id,
        "攻撃者の署名鍵はスレ主(board_id)と一致しないため偽 ORDER として拒否されるべき"
    );
}

#[test]
fn order_signed_by_board_persona_matches_board_id() {
    // 対照(正常系): スレ主ペルソナ自身が署名した ORDER は署名者 == board_id が成立する。
    let board_persona = Keys::generate();
    let board_id = board_persona.public_key().to_hex();

    let order_envelope = OrderInfo {
        board_id: board_id.clone(),
        generation: 1,
        seq: 1,
        entries: vec![OrderEntry {
            res_no: 1,
            event_id: "bb".repeat(32),
        }],
    };
    let event = order_envelope.sign(&board_persona, 1_700_000_000).unwrap();
    assert_eq!(
        event.pubkey.to_hex(),
        board_id,
        "スレ主自身の署名は board_id と一致する(正当な ORDER)"
    );
}

#[test]
fn oversized_announce_event_is_rejected_before_signature_check() {
    // 検証 1(サイズ): 直列化イベント全体が共通上限(16KB)を超える announce は、
    // 署名検証より前にサイズで拒否される。受信側はサイズだけを見て即座に拒否する
    // ため(署名検証の前段)、ここでは有効な JSON である必要すらない — 巨大な文字列を
    // そのまま受信検証に渡すブラックボックス確認で十分(実装は raw_json.len() を見る)。
    use peca_p2p_yp::event::schema::MAX_EVENT_BYTES;

    let now = 1_700_000_000;
    let oversized_raw = "x".repeat(MAX_EVENT_BYTES + 1);
    assert!(oversized_raw.len() > MAX_EVENT_BYTES);

    let clock = Arc::new(AtomicU64::new(now));
    let mut st = state_at(Arc::clone(&clock));
    let err = st.ingest(&oversized_raw, "peer:1", now).unwrap_err();
    assert_eq!(
        err,
        VerifyReject::Oversize,
        "16KB 超のイベントはサイズ超過として拒否される(署名検証より前)"
    );
    assert_eq!(st.store_len(), 0);
}

#[test]
fn oversized_res_body_rejected_via_verify_incoming_res() {
    // 検証 3(本文制約)がホスト受信検証を通しても効くことを、送信前検査を迂回した
    // 生イベント(本文 2048 文字超を直接タグ無しで content に積む)で確認する。
    // ResEnvelope::sign は上限で弾くため、EventBuilder で直接組み立てて検証をバイパスした
    // ケースを模す(悪意ある/不具合のあるクライアントが規約破りの本文を送る想定)。
    use nostr::{EventBuilder, Kind, Tag, Timestamp};

    let persona = Keys::generate();
    let board_id = persona.public_key().to_hex();
    let reg = registry_with_open_thread(&persona);
    let board_key = Keys::generate();

    let oversized_body = "あ".repeat(2049);
    let tags = vec![
        Tag::parse(["a", &thread_channel_ref(&board_id)]).unwrap(),
        Tag::parse(["peca", "thread", &board_id, "1"]).unwrap(),
    ];
    let event = EventBuilder::new(Kind::Custom(1311), oversized_body)
        .tags(tags)
        .custom_created_at(Timestamp::from(1_700_000_005u64))
        .sign_with_keys(&board_key)
        .unwrap();

    assert!(
        !reg.verify_incoming_res(&board_id, &event),
        "本文 2048 文字超のレスはホスト側受信検証(from_event 経由)で拒否される"
    );
}

#[test]
fn res_kind_on_gossip_is_dropped_not_stored_or_propagated() {
    // gossip の許可 kind は {30311, 31311} のみ(thread-events.md)。kind 1311(レス)が
    // gossip の EVENT として送られてきた場合は破棄し、格納も再伝搬もしない
    // (受信側規範 — 送信側の「流してはならない」と対)。
    let now = 1_700_000_000;
    let clock = Arc::new(AtomicU64::new(now));
    let mut st = state_at(Arc::clone(&clock));
    let board_key = Keys::generate();
    let board_id = "12".repeat(32);

    let res = sign_res_with_name(&board_key, &board_id, 1, None, "本文", now);
    let err = st.ingest(&res.as_json(), "peer:1", now).unwrap_err();
    assert!(
        format!("{err:?}").contains("InvalidFormat"),
        "kind 1311 は gossip 上では形式違反として拒否される: {err:?}"
    );
    assert_eq!(st.store_len(), 0, "格納されない");
    assert!(
        st.sync_events(0, now).is_empty(),
        "再伝搬(sync_events への反映)もされない"
    );
}

#[test]
fn order_kind_on_gossip_is_dropped_not_stored_or_propagated() {
    // kind 21311(順序確定情報)も同様に gossip 上では破棄される。
    let now = 1_700_000_000;
    let clock = Arc::new(AtomicU64::new(now));
    let mut st = state_at(Arc::clone(&clock));
    let persona = Keys::generate();
    let board_id = persona.public_key().to_hex();

    let order = OrderInfo {
        board_id: board_id.clone(),
        generation: 1,
        seq: 1,
        entries: vec![OrderEntry {
            res_no: 1,
            event_id: "cc".repeat(32),
        }],
    }
    .sign(&persona, now)
    .unwrap();

    let err = st.ingest(&order.as_json(), "peer:1", now).unwrap_err();
    assert!(
        format!("{err:?}").contains("InvalidFormat"),
        "kind 21311 は gossip 上では形式違反として拒否される: {err:?}"
    );
    assert_eq!(st.store_len(), 0, "格納されない");
    assert!(
        st.sync_events(0, now).is_empty(),
        "再伝搬(sync_events への反映)もされない"
    );
}

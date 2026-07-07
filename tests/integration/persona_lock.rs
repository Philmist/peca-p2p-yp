//! 配信中ロックの並行性・解錠統合テスト(T018 / T019 — ADR-0011 代替担保、SC-005/SC-003)
//!
//! Principle V は非該当(ADR-0011)。その代替担保として、発行開始の「予約」と `select` を
//! 交錯させても不変条件「配信中の区間、当該チャンネルの署名ペルソナは変化しない」が
//! 保たれることを検証する(research R2 の 2 通りの場合分けを実測で確認)。

use std::sync::{Arc, Mutex};

use peca_p2p_yp::broadcast::BroadcastState;
use peca_p2p_yp::event::publish::{EventSink, PublishEngine};
use peca_p2p_yp::event::schema::{ChannelListing, ChannelStatus, Track};
use peca_p2p_yp::identity::{IdentityError, IdentityManager, Keystore};
use peca_p2p_yp::store::Store;

/// 署名イベントの pubkey(= どのペルソナで署名したか)を記録する sink。
#[derive(Default)]
struct CapturingSink {
    pubkeys: Mutex<Vec<String>>,
}

impl EventSink for CapturingSink {
    fn publish_local(&self, event: nostr::Event) -> bool {
        self.pubkeys.lock().unwrap().push(event.pubkey.to_hex());
        true
    }
}

fn listing() -> ChannelListing {
    ChannelListing {
        channel_id: "000000000000000000000000000000cc".into(),
        title: "配信".into(),
        summary: None,
        genre: Some("game".into()),
        status: ChannelStatus::Live,
        starts: 1_700_000_000,
        current_participants: 1,
        streaming: None,
        bitrate_kbps: Some(500),
        content_type: Some("FLV".into()),
        tip: Some("198.51.100.1:7144".into()),
        contact: None,
        relays: 0,
        track: Some(Track::default()),
    }
}

/// T018: 「発行開始(予約)」と「select(B)」を交錯させても、いずれの順でも不変条件が保たれる。
///
/// - select が成功 = 発行より先にロックを取った ⇒ 続く発行は B を解決して署名する(切替なし)。
/// - select が `BroadcastingLocked` = 発行が先にロックを取った ⇒ 発行は A のまま(切替拒否)。
///
/// 「発行は A で署名されたのに select(B) が成功して selected=B」という取り違えは、単一
/// ミューテックスの相互排他により発生しない。多数回反復して交錯機会を作り検証する。
#[test]
fn reserve_and_select_are_mutually_exclusive() {
    for _ in 0..300 {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let broadcast = Arc::new(BroadcastState::new());
        let identity = Arc::new(
            IdentityManager::new(store, Keystore::ephemeral())
                .with_broadcast_state(Arc::clone(&broadcast)),
        );
        let a = identity.create("A").unwrap(); // 自動選択
        let b = identity.create("B").unwrap();
        let sink = Arc::new(CapturingSink::default());
        let engine = Arc::new(PublishEngine::new(
            Arc::clone(&identity),
            Arc::clone(&sink) as Arc<dyn EventSink>,
            60,
            Arc::clone(&broadcast),
        ));

        // 発行開始(初回発行 → 予約先行)と select(B) を並行に走らせる。
        let publisher = {
            let engine = Arc::clone(&engine);
            std::thread::spawn(move || engine.publish_listing(&listing()).unwrap())
        };
        let selector = {
            let identity = Arc::clone(&identity);
            let bpk = b.pubkey.clone();
            std::thread::spawn(move || identity.select(&bpk))
        };
        assert!(publisher.join().unwrap(), "selected があるので発行される");
        let select_result = selector.join().unwrap();

        let signed = sink.pubkeys.lock().unwrap().clone();
        assert_eq!(signed.len(), 1, "初回発行は 1 件署名する");
        let signed_pk = &signed[0];

        match select_result {
            Ok(()) => {
                assert_eq!(
                    signed_pk, &b.pubkey,
                    "select が先に成立したら発行は B で署名される(配信中に A→B の入替は起きない)"
                );
                assert_eq!(identity.selected().unwrap(), Some(b.pubkey.clone()));
            }
            Err(IdentityError::BroadcastingLocked) => {
                assert_eq!(
                    signed_pk, &a.pubkey,
                    "発行が先に予約したら selected は A のまま・発行も A で署名される"
                );
                assert_eq!(identity.selected().unwrap(), Some(a.pubkey.clone()));
            }
            Err(other) => panic!("想定外のエラー: {other:?}"),
        }
    }
}

/// T019: 全チャンネルが `publish_ended` で集合から除去され、`is_broadcasting()==false` に
/// なった後は追加リセットなく `select` が成功する(SC-003 / FR-009)。
#[test]
fn release_after_ended_unlocks_select() {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let broadcast = Arc::new(BroadcastState::new());
    let identity = Arc::new(
        IdentityManager::new(store, Keystore::ephemeral())
            .with_broadcast_state(Arc::clone(&broadcast)),
    );
    let _a = identity.create("A").unwrap(); // selected
    let b = identity.create("B").unwrap();
    let sink = Arc::new(CapturingSink::default());
    let engine = Arc::new(PublishEngine::new(
        Arc::clone(&identity),
        Arc::clone(&sink) as Arc<dyn EventSink>,
        60,
        Arc::clone(&broadcast),
    ));

    assert!(engine.publish_listing(&listing()).unwrap());
    assert!(broadcast.is_broadcasting(), "発行後は配信中");
    assert!(
        matches!(
            identity.select(&b.pubkey),
            Err(IdentityError::BroadcastingLocked)
        ),
        "配信中は切替不可"
    );

    assert!(engine.publish_ended(&listing()).unwrap());
    assert!(!broadcast.is_broadcasting(), "終了後は解錠される");
    assert!(
        identity.select(&b.pubkey).is_ok(),
        "解錠後は追加リセットなく選択できる"
    );
    assert_eq!(identity.selected().unwrap(), Some(b.pubkey));
}

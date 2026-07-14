//! gossip 受信パイプライン(T037)
//!
//! contracts/p2p-gossip.md §受信検証パイプライン・§伝搬規則 1〜6 の中核ロジックを、
//! トランスポート非依存の状態([`IngestState`])として実装する。フレーム長(検査 1)・
//! 受信レート(検査 2)は [`crate::p2p::session`] が済ませた後段で、本モジュールは
//! **JSON→イベント検証(検査 4)→重複判定→格納→再伝搬判定**を担う。
//!
//! ## 伝搬規則との対応(ADR-0005 / gossip_propagation.tla 整合)
//!
//! 1. 受信 EVENT を [`crate::event::schema::verify_incoming`] に通す(検証失敗は破棄・非伝搬)
//! 2. **重複判定**: DedupCache に既知なら黙って破棄(再伝搬しない — BoundedPropagation)
//! 3. **格納**: EventStore の置換規則 + last-write-wins。既保持の同一 event id は
//!    格納も再伝搬もしない(第二の防壁 — DedupCache 期限切れ後のループ再発防止)
//! 4. **再伝搬**: 格納に成功したイベントのみ、受信元を除く established 全ピアへ再送
//!    (再送そのものはハブ [`crate::p2p::hub`] が行う。本状態は「再伝搬すべきイベント」を返す)
//!
//! ローカル発行([`IngestState::publish_local`])は検証をスキップし(自ノードが署名した
//! イベントのため)格納後に全ピアへ送る(除外なし)。DedupCache には記録し、ピアからの
//! エコー(自分が送ったイベントの返送)を破棄できるようにする。

use std::collections::HashMap;

use nostr::Event;

use crate::event::livechat::{ANNOUNCE_KIND, ORDER_KIND, RES_KIND};
use crate::event::schema::{
    EventSummary, VerifyConfig, VerifyReject, verify_incoming, verify_incoming_announce,
};
use crate::event::store::{DedupCache, EventStore, InsertOutcome, StoreConfig};
use crate::event::view::{self, DiscoveredChannel, MuteSet, SourceMap};
use crate::store::MuteEntry;

/// gossip 受信・発行の共有状態(EventStore + DedupCache + 受信ピア集合)。
///
/// 1 個の [`std::sync::Mutex`] 下で扱う想定(ハブが所有)。時刻は `now`(unix 秒)を
/// 呼び出し側から注入し、EventStore/DedupCache 内部クロックと概ね一致させる。
pub struct IngestState {
    store: EventStore,
    dedup: DedupCache,
    sources: SourceMap,
    verify: VerifyConfig,
    store_config: StoreConfig,
    /// スレ機能の有効/無効(Settings.livechat_enabled)。false のとき announce は検証のみ
    /// 行い不可視(格納・伝搬しない — 006 data-model §Settings)。
    livechat_enabled: bool,
}

impl IngestState {
    /// 設定から状態を作る(実時刻クロック)。
    pub fn new(store_config: StoreConfig, verify: VerifyConfig) -> Self {
        Self {
            store: EventStore::new(store_config),
            dedup: DedupCache::new(store_config.freshness_window_sec),
            sources: SourceMap::new(),
            verify,
            store_config,
            livechat_enabled: true,
        }
    }

    /// EventStore とその設定を差し替えて作る(テスト用 — 時刻注入した store を渡す)。
    pub fn with_parts(
        store: EventStore,
        dedup: DedupCache,
        verify: VerifyConfig,
        store_config: StoreConfig,
    ) -> Self {
        Self {
            store,
            dedup,
            sources: SourceMap::new(),
            verify,
            store_config,
            livechat_enabled: true,
        }
    }

    /// スレ機能の有効/無効を設定する(Settings.livechat_enabled — 起動時に配線側が反映)。
    pub fn set_livechat_enabled(&mut self, enabled: bool) {
        self.livechat_enabled = enabled;
    }

    /// 受信 EVENT を検証・重複判定・格納する(伝搬規則 1〜3)。
    ///
    /// - `Ok(Some(event))`: 格納に成功。**受信元を除く** established 全ピアへ再伝搬すべき(規則 4)
    /// - `Ok(None)`: 重複・旧版・鮮度切れ等で格納しなかった(再伝搬しない)
    /// - `Err(reject)`: 受信検証に失敗(呼び出し側がセキュリティイベントを記録する)
    ///
    /// `source` は受信元ピアの正規アドレス(source_peers 記録用)。
    pub fn ingest(
        &mut self,
        raw_json: &str,
        source: &str,
        now: u64,
    ) -> Result<Option<Event>, VerifyReject> {
        // gossip の許可 kind は {30311, 31311}(thread-events.md)。受信検証の前に
        // 種別を軽く覗いて分岐する。kind 1311/21311 は gossip に流してはならないため
        // 破棄し event_invalid_format として扱う(受信側規範)。未知/欠落 kind は
        // 30311 の通常検証へ委ね、そこで形式違反として拒否させる。
        match peek_kind(raw_json) {
            Some(ANNOUNCE_KIND) => self.ingest_announce(raw_json, source, now),
            Some(RES_KIND) | Some(ORDER_KIND) => {
                // スレ配送専用の kind が gossip に載っていた → 破棄(格納・再伝搬しない)。
                Err(VerifyReject::InvalidFormat(
                    "livechat delivery kind not allowed on gossip",
                ))
            }
            _ => self.ingest_channel(raw_json, source, now),
        }
    }

    /// kind 30311(チャンネル掲載)の受信処理(従来経路)。
    fn ingest_channel(
        &mut self,
        raw_json: &str,
        source: &str,
        now: u64,
    ) -> Result<Option<Event>, VerifyReject> {
        // 1. 受信検証(サイズ→署名→形式→時刻→内容→PoW)
        let verified = verify_incoming(raw_json, &self.verify, now)?;
        let event = verified.event;
        let id = event.id.to_hex();

        // 2. 重複判定(DedupCache に既知なら黙って破棄 — 再伝搬しない)
        if self.dedup.check_and_insert(&id) {
            return Ok(None);
        }

        // 3. 格納(置換規則 + 第二の防壁)
        let outcome = self.store.insert(event.clone());
        if outcome.should_propagate() {
            self.record_source(&event, source);
            Ok(Some(event))
        } else {
            Ok(None)
        }
    }

    /// kind 31311(スレ announce)の受信処理(T020 — FR-003)。
    ///
    /// 検証(検査 1〜7)を通した上で 30311 と同じ重複判定・置換格納・再伝搬に載せる。announce
    /// の置換キーは `(31311, pubkey, "livechat")`(EventStore の d タグ置換で自然に処理される)。
    ///
    /// `livechat_enabled=false` のときは**検証だけ行い不可視**にする(格納・伝搬しない)。
    /// この場合でも検証失敗は `Err` として返し(記録は配線側)、成功時のみ `Ok(None)` で
    /// 不可視化する(仕様: 「announce は検証のみ・不可視」)。
    fn ingest_announce(
        &mut self,
        raw_json: &str,
        source: &str,
        now: u64,
    ) -> Result<Option<Event>, VerifyReject> {
        // 1〜7. announce 受信検証(ペルソナ一致まで)。
        let verified = verify_incoming_announce(raw_json, &self.verify, now)?;
        let event = verified.event;

        // 機能無効時は検証のみで不可視(格納・重複記録・伝搬をしない)。
        if !self.livechat_enabled {
            return Ok(None);
        }

        let id = event.id.to_hex();
        // 2. 重複判定。
        if self.dedup.check_and_insert(&id) {
            return Ok(None);
        }
        // 3. 格納(置換 + 第二の防壁)。
        let outcome = self.store.insert(event.clone());
        if outcome.should_propagate() {
            self.record_source(&event, source);
            Ok(Some(event))
        } else {
            Ok(None)
        }
    }

    /// ローカル発行イベントを格納する(検証はしない — 自ノード署名)。
    ///
    /// 戻り値の [`InsertOutcome::should_propagate`] が真なら、ハブは **除外なしで**
    /// established 全ピアへ送る(発行 = 自ノード起点のため受信元がない)。
    pub fn publish_local(&mut self, event: Event) -> InsertOutcome {
        // 自分が送ったイベントのエコー返送を破棄できるよう DedupCache に記録する。
        let id = event.id.to_hex();
        self.dedup.check_and_insert(&id);
        let outcome = self.store.insert(event.clone());
        if outcome.should_propagate() {
            // ローカル発行は受信ピアなし(空集合を確保して古い source を消す)。
            if let Some(summary) = EventSummary::from_event(&event) {
                self.sources.insert(summary.key(), Vec::new());
            }
        }
        outcome
    }

    /// 受信元ピアを置換キー単位で記録する(置換で新イベントになった場合は集合を作り直す)。
    fn record_source(&mut self, event: &Event, source: &str) {
        if let Some(summary) = EventSummary::from_event(event) {
            self.sources.insert(summary.key(), vec![source.to_string()]);
        }
    }

    /// 接続時同期(SYNC_REQ)への応答イベントを選ぶ(T038 が使用)。
    ///
    /// live かつ鮮度窓内のイベントのうち `created_at ≥ max(since, now − freshness_window_sec)`
    /// のものを、上限 `event_store_max` 件まで返す(contracts/p2p-gossip.md §メッセージ種別)。
    /// `since` が非標準(過大・負)でも応答範囲は鮮度窓を超えて拡大しない。
    pub fn sync_events(&self, since: i64, now: u64) -> Vec<Event> {
        let window = self.store_config.freshness_window_sec;
        let floor = now.saturating_sub(window);
        let since_u = if since < 0 { 0 } else { since as u64 };
        let lower = since_u.max(floor);

        let mut events: Vec<Event> = self
            .store
            .live_fresh_events()
            .into_iter()
            .filter(|e| e.created_at.as_secs() >= lower)
            .cloned()
            .collect();
        // created_at 昇順(古い順)で返す — 収束の見た目の安定と上限切りの決定性のため。
        events.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        events.truncate(self.store_config.event_store_max);
        events
    }

    /// 現在の一覧スナップショットを構築する(T039 — ミュート適用は呼び出し側が渡す)。
    pub fn snapshot(&self, mutes: &[MuteEntry]) -> Vec<DiscoveredChannel> {
        let mute_set = MuteSet::from_entries(mutes);
        view::aggregate(self.store.live_fresh_events(), &self.sources, &mute_set)
    }

    /// 受信済み announce(kind 31311)の生存スナップショット(T065 — スレ一覧の他ノード板)。
    ///
    /// live かつ鮮度窓内の 31311 イベントのみを [`ThreadAnnounce`] へ復元して返す
    /// (`(スレ主 pubkey hex, 復元済み announce)` のペア)。鮮度切れ・期限切れは
    /// [`EventStore::live_fresh_events`] が除外するため、announce 鮮度切れのスレは自然に
    /// 一覧から落ちる(FR-002 の一覧鮮度規則)。`livechat_enabled=false` のときは
    /// [`Self::ingest_announce`] が格納自体をしないため、結果は空になる。
    pub fn announce_snapshot(&self) -> Vec<(String, crate::event::livechat::ThreadAnnounce)> {
        self.store
            .live_fresh_events()
            .into_iter()
            .filter(|e| e.kind.as_u16() == ANNOUNCE_KIND)
            .filter_map(|e| {
                crate::event::livechat::ThreadAnnounce::from_event(e)
                    .ok()
                    .map(|a| (e.pubkey.to_hex(), a))
            })
            .collect()
    }

    /// 鮮度切れ/期限切れエントリを物理回収し、対応する source_peers も掃除する。
    pub fn sweep(&mut self) -> usize {
        let removed = self.store.sweep();
        if removed > 0 {
            self.prune_sources();
        }
        removed
    }

    /// EventStore に存在しない置換キーの source_peers を削除する。
    fn prune_sources(&mut self) {
        let live: HashMap<(u16, String, String), ()> = self
            .store
            .live_fresh_events()
            .into_iter()
            .filter_map(|e| EventSummary::from_event(e).map(|s| (s.key(), ())))
            .collect();
        self.sources.retain(|k, _| live.contains_key(k));
    }

    /// 保持イベント件数(テスト・状態表示用)。
    pub fn store_len(&self) -> usize {
        self.store.len()
    }

    /// EventStore の設定(SYNC の閾値・鮮度窓の参照用)。
    pub fn store_config(&self) -> StoreConfig {
        self.store_config
    }
}

/// 直列化イベント JSON から `kind` フィールドだけを軽く覗く(署名検証前の種別分岐用)。
///
/// 本判定は「どの検証パイプラインへ振り分けるか」の入口にすぎず、値の真正性(署名・形式)は
/// 各パイプラインが改めて検証する。パース不能・kind 欠落・範囲外は `None`(呼び出し側は
/// 通常検証へ委ね、そこで形式違反として拒否させる)。
fn peek_kind(raw_json: &str) -> Option<u16> {
    let value: serde_json::Value = serde_json::from_str(raw_json).ok()?;
    let kind = value.get("kind")?.as_u64()?;
    u16::try_from(kind).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::schema::{ChannelListing, ChannelStatus};
    use crate::event::store::{DedupCache, EventStore, RejectReason, StoreConfig};
    use crate::security::SecurityCategory;
    use nostr::{JsonUtil, Keys};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    const D1: &str = "0123456789abcdef0123456789abcdef";
    const D2: &str = "0123456789abcdef0123456789abcdee";

    fn listing(d: &str, title: &str, status: ChannelStatus) -> ChannelListing {
        ChannelListing {
            channel_id: d.into(),
            title: title.into(),
            summary: None,
            genre: None,
            status,
            starts: 1_700_000_000,
            current_participants: -1,
            streaming: None,
            bitrate_kbps: None,
            content_type: None,
            tip: None,
            contact: None,
            relays: -1,
            track: None,
        }
    }

    fn mk(keys: &Keys, d: &str, created: u64, status: ChannelStatus, title: &str) -> Event {
        listing(d, title, status).sign(keys, created, 0).unwrap()
    }

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

    #[test]
    fn valid_event_stored_and_marked_for_propagation() {
        let clock = Arc::new(AtomicU64::new(1_700_000_050));
        let mut st = state_at(Arc::clone(&clock));
        let keys = Keys::generate();
        let e = mk(&keys, D1, 1_700_000_050, ChannelStatus::Live, "x");
        let out = st.ingest(&e.as_json(), "peer:1", 1_700_000_050).unwrap();
        assert!(out.is_some(), "新規は再伝搬対象");
        assert_eq!(st.store_len(), 1);
    }

    #[test]
    fn duplicate_via_dedup_is_dropped_not_propagated() {
        let clock = Arc::new(AtomicU64::new(1_700_000_050));
        let mut st = state_at(Arc::clone(&clock));
        let keys = Keys::generate();
        let e = mk(&keys, D1, 1_700_000_050, ChannelStatus::Live, "x");
        assert!(
            st.ingest(&e.as_json(), "peer:1", 1_700_000_050)
                .unwrap()
                .is_some()
        );
        // 2 回目(同一 id)は DedupCache で破棄 → 再伝搬しない
        assert!(
            st.ingest(&e.as_json(), "peer:2", 1_700_000_050)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn invalid_signature_is_rejected() {
        let clock = Arc::new(AtomicU64::new(1_700_000_050));
        let mut st = state_at(Arc::clone(&clock));
        let keys = Keys::generate();
        let e = mk(&keys, D1, 1_700_000_050, ChannelStatus::Live, "x");
        let tampered = e.as_json().replace("\"content\":\"\"", "\"content\":\"x\"");
        let err = st.ingest(&tampered, "peer:1", 1_700_000_050).unwrap_err();
        assert_eq!(err, VerifyReject::InvalidSig);
        assert_eq!(st.store_len(), 0);
    }

    #[test]
    fn older_replacement_is_not_propagated() {
        let clock = Arc::new(AtomicU64::new(1_700_000_100));
        let mut st = state_at(Arc::clone(&clock));
        let keys = Keys::generate();
        let newer = mk(&keys, D1, 1_700_000_090, ChannelStatus::Live, "new");
        let older = mk(&keys, D1, 1_700_000_080, ChannelStatus::Live, "old");
        assert!(
            st.ingest(&newer.as_json(), "p1", 1_700_000_100)
                .unwrap()
                .is_some()
        );
        // 旧版(別 id・同キー)は格納も再伝搬もしない
        assert!(
            st.ingest(&older.as_json(), "p2", 1_700_000_100)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn publish_local_stores_and_marks_echo_dedup() {
        let clock = Arc::new(AtomicU64::new(1_700_000_050));
        let mut st = state_at(Arc::clone(&clock));
        let keys = Keys::generate();
        let e = mk(&keys, D1, 1_700_000_050, ChannelStatus::Live, "x");
        let outcome = st.publish_local(e.clone());
        assert!(outcome.should_propagate());
        // 自分のイベントがピアからエコーされても DedupCache で破棄
        assert!(
            st.ingest(&e.as_json(), "peer:1", 1_700_000_050)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn sync_events_respects_since_and_window() {
        let now = 1_700_000_100;
        let clock = Arc::new(AtomicU64::new(now));
        let mut st = state_at(Arc::clone(&clock));
        let keys = Keys::generate();
        // 鮮度窓 600 内。created 040 と 090。
        let e_old = mk(&keys, D1, 1_700_000_040, ChannelStatus::Live, "old");
        let e_new = mk(&keys, D2, 1_700_000_090, ChannelStatus::Live, "new");
        st.ingest(&e_old.as_json(), "p", now).unwrap();
        st.ingest(&e_new.as_json(), "p", now).unwrap();

        // since=0 → 全件(鮮度窓 floor = now-600 = ...500 以降、両方該当)
        assert_eq!(st.sync_events(0, now).len(), 2);
        // since=...060 → created≥060 の 1 件のみ
        assert_eq!(st.sync_events(1_700_000_060, now).len(), 1);
    }

    #[test]
    fn ended_is_excluded_from_sync_and_snapshot() {
        let now = 1_700_000_100;
        let clock = Arc::new(AtomicU64::new(now));
        let mut st = state_at(Arc::clone(&clock));
        let keys = Keys::generate();
        let live = mk(&keys, D1, 1_700_000_050, ChannelStatus::Live, "live");
        st.ingest(&live.as_json(), "p", now).unwrap();
        assert_eq!(st.snapshot(&[]).len(), 1);
        // ended で置換 → 供給・表示から消える(tombstone は物理保持)
        let ended = mk(&keys, D1, 1_700_000_080, ChannelStatus::Ended, "ended");
        assert!(st.ingest(&ended.as_json(), "p", now).unwrap().is_some());
        assert!(st.snapshot(&[]).is_empty());
        assert!(st.sync_events(0, now).is_empty());
    }

    // --- T020: gossip announce 受信検証 ------------------------------------

    fn announce_channel(pubkey: &str) -> String {
        format!("30311:{pubkey}:0123456789abcdef0123456789abcdef")
    }

    fn mk_announce(keys: &Keys, created: u64, pubkey_in_a: &str) -> Event {
        crate::event::livechat::ThreadAnnounce {
            channel: announce_channel(pubkey_in_a),
            title: "実況スレ".into(),
            generation: 1,
            key: created,
            res_count: Some(0),
            tip: "198.51.100.1:7147".into(),
        }
        .sign(keys, created, 0)
        .unwrap()
    }

    #[test]
    fn announce_signer_match_stored_and_propagated() {
        let now = 1_700_000_050;
        let clock = Arc::new(AtomicU64::new(now));
        let mut st = state_at(Arc::clone(&clock));
        let keys = Keys::generate();
        let pk = keys.public_key().to_hex();
        let ev = mk_announce(&keys, now, &pk);
        let out = st.ingest(&ev.as_json(), "peer:1", now).unwrap();
        assert!(out.is_some(), "署名者一致の announce は格納・再伝搬される");
        assert_eq!(st.store_len(), 1);
    }

    #[test]
    fn announce_signer_mismatch_rejected_with_livechat_category() {
        let now = 1_700_000_050;
        let clock = Arc::new(AtomicU64::new(now));
        let mut st = state_at(Arc::clone(&clock));
        let keys = Keys::generate();
        let other = Keys::generate();
        // a タグは other、署名は keys(不一致 → 不可視)。
        let ev = mk_announce(&keys, now, &other.public_key().to_hex());
        let err = st.ingest(&ev.as_json(), "peer:1", now).unwrap_err();
        assert_eq!(err.category(), SecurityCategory::LivechatAnnounceInvalid);
        assert_eq!(st.store_len(), 0);
    }

    #[test]
    fn livechat_res_kind_on_gossip_is_dropped() {
        let now = 1_700_000_050;
        let clock = Arc::new(AtomicU64::new(now));
        let mut st = state_at(Arc::clone(&clock));
        let keys = Keys::generate();
        let board_id = "ab".repeat(32);
        // kind 1311(レス)を gossip へ流す → 破棄 + event_invalid_format。
        let res = crate::event::livechat::Res {
            channel: announce_channel(&board_id),
            board_id: board_id.clone(),
            generation: 1,
            name: None,
            mail: None,
            body: "本文".into(),
        }
        .sign(&keys, now, 0)
        .unwrap();
        let err = st.ingest(&res.as_json(), "peer:1", now).unwrap_err();
        assert_eq!(err.category(), SecurityCategory::EventInvalidFormat);
        assert_eq!(st.store_len(), 0);
    }

    #[test]
    fn announce_invisible_when_livechat_disabled() {
        let now = 1_700_000_050;
        let clock = Arc::new(AtomicU64::new(now));
        let mut st = state_at(Arc::clone(&clock));
        st.set_livechat_enabled(false);
        let keys = Keys::generate();
        let pk = keys.public_key().to_hex();
        let ev = mk_announce(&keys, now, &pk);
        // 検証は通るが不可視(格納・伝搬しない)。
        assert!(st.ingest(&ev.as_json(), "peer:1", now).unwrap().is_none());
        assert_eq!(st.store_len(), 0);
    }

    #[test]
    fn announce_disabled_still_rejects_invalid() {
        let now = 1_700_000_050;
        let clock = Arc::new(AtomicU64::new(now));
        let mut st = state_at(Arc::clone(&clock));
        st.set_livechat_enabled(false);
        let keys = Keys::generate();
        let other = Keys::generate();
        // 無効時でも不正 announce は Err(記録できるように)。
        let ev = mk_announce(&keys, now, &other.public_key().to_hex());
        let err = st.ingest(&ev.as_json(), "peer:1", now).unwrap_err();
        assert_eq!(err.category(), SecurityCategory::LivechatAnnounceInvalid);
    }

    #[test]
    fn existing_channel_ingest_unaffected() {
        // 30311 の従来経路が壊れていないこと(回帰)。
        let now = 1_700_000_050;
        let clock = Arc::new(AtomicU64::new(now));
        let mut st = state_at(Arc::clone(&clock));
        let keys = Keys::generate();
        let e = mk(&keys, D1, now, ChannelStatus::Live, "x");
        assert!(st.ingest(&e.as_json(), "peer:1", now).unwrap().is_some());
        assert_eq!(st.store_len(), 1);
    }

    #[test]
    fn second_barrier_rejects_reinsert_after_dedup_expiry() {
        // DedupCache 期限切れ後でも、EventStore が保持中なら同一 id を再格納しない。
        let created = 1_700_000_000;
        let clock = Arc::new(AtomicU64::new(created + 10));
        let mut st = state_at(Arc::clone(&clock));
        let keys = Keys::generate();
        let e = mk(&keys, D1, created, ChannelStatus::Live, "x");
        assert!(
            st.ingest(&e.as_json(), "p", created + 10)
                .unwrap()
                .is_some()
        );
        // 直接ストアの第二の防壁を確認(Dedupを迂回した経路の不変条件)
        assert_eq!(
            crate::event::store::InsertOutcome::Rejected(RejectReason::DuplicateId),
            {
                // publish_local 経由でも同一 id は DuplicateId
                st.publish_local(e.clone())
            }
        );
    }
}

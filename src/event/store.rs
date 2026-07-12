//! EventStore(署名済みイベントのローカル置換ストア)と DedupCache(T016)
//!
//! リレーが担っていた addressable 置換 `(kind, pubkey, d)` を各ノードで実装する
//! (data-model §EventStore、research R1)。gossip 受信・自ノード発行の両方のイベントを
//! 保持し、SYNC_REQ 応答・再伝搬の供給源となる。
//!
//! ## 置換とロールバック防止(ADR-0005 形式モデル整合)
//!
//! 置換は last-write-wins(created_at 最大、同値なら event id 辞書順大)。形式モデル
//! `gossip_propagation.tla` は EventStore を「LWW 勝者を保持し続ける単一スロット」として
//! モデル化し、`StoreMonotonic`(旧イベントへ後退しない)を検証済み。したがって
//! `status=ended` イベントも**LWW 勝者として保持**する(tombstone)。
//!
//! data-model §EventStore の「`status=ended` で除去」は、
//! - **供給・表示からの除去**: [`EventStore::live_fresh_events`] が ended を返さない(即時)、
//! - **物理削除**: 鮮度切れ / `expiration` 超過で [`EventStore::sweep`] が回収(遅延)、
//!
//! として実装する。ended を即時物理削除すると、E のみを受信し元の live L を見ていないノードで
//! 「まだ鮮度窓内の古い L のリプレイ」が空スロットへ格納され `StoreMonotonic` に反する
//! (ADR-0005 §「ended の巻き戻し」)。tombstone 保持でこれを防ぎ、tombstone が鮮度切れする
//! 頃には受信検証(鮮度・expiration)と DedupCache(保持 ≥ 鮮度窓)が同一/古いイベントの
//! 再格納を塞ぐ(ADR-0005 発見事項 1)。

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use nostr::Event;

use super::livechat::ANNOUNCE_KIND;
use super::schema::EventSummary;

/// EventStore 既定容量(data-model §Settings `event_store_max`)。
pub const DEFAULT_EVENT_STORE_MAX: usize = 4096;
/// 同一 pubkey の保持イベント上限(ADR-0004 §2)。
pub const DEFAULT_MAX_EVENTS_PER_PUBKEY: usize = 64;
/// 鮮度判定窓の既定値(data-model §Settings `freshness_window_sec`)。
pub const DEFAULT_FRESHNESS_WINDOW_SEC: u64 = 600;
/// kind 31311(スレ announce)の EventStore 独立保持枠の既定値(research R3)。
///
/// 30311 用の [`DEFAULT_EVENT_STORE_MAX`] とは別枠(容量カウント・退避を kind で分離)。
pub const DEFAULT_ANNOUNCE_STORE_QUOTA: usize = 2048;
/// DedupCache 保持期間の下限(research R16。ADR-0005 の連動制約の下限)。
pub const DEDUP_MIN_RETENTION_SEC: u64 = 600;

/// 置換キー `(kind, pubkey, d)`。
type StoreKey = (u16, String, String);

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// EventStore
// ---------------------------------------------------------------------------

/// EventStore の設定。
#[derive(Debug, Clone, Copy)]
pub struct StoreConfig {
    /// 鮮度判定窓(秒)。
    pub freshness_window_sec: u64,
    /// 容量上限(kind 31311 を除く)。超過時は created_at が古い順に破棄。
    pub event_store_max: usize,
    /// 同一 pubkey の保持上限(超過は当該 pubkey の古い順破棄。kind を問わず合算)。
    pub max_events_per_pubkey: usize,
    /// kind 31311(スレ announce)専用の容量上限(research R3)。
    ///
    /// [`Self::event_store_max`] とは独立のバケットで容量カウント・退避を行い、
    /// announce の流入が 30311 等の保持枠を消費しない。
    pub announce_store_quota: usize,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            freshness_window_sec: DEFAULT_FRESHNESS_WINDOW_SEC,
            event_store_max: DEFAULT_EVENT_STORE_MAX,
            max_events_per_pubkey: DEFAULT_MAX_EVENTS_PER_PUBKEY,
            announce_store_quota: DEFAULT_ANNOUNCE_STORE_QUOTA,
        }
    }
}

/// 格納結果。`Stored` / `Replaced` は再伝搬すべきイベント、`Rejected` は伝搬しない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InsertOutcome {
    /// 新規格納(置換キーが未保持だった)。再伝搬すべき。
    Stored,
    /// 既存を置換(より新しいイベント)。再伝搬すべき。
    Replaced,
    /// 格納しなかった(理由つき)。再伝搬しない。
    Rejected(RejectReason),
}

impl InsertOutcome {
    /// 再伝搬すべきか(`Stored` または `Replaced`)。
    pub fn should_propagate(&self) -> bool {
        matches!(self, InsertOutcome::Stored | InsertOutcome::Replaced)
    }
}

/// 格納を拒否した理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// 同一 event id を既に保持(第二の防壁 — DedupCache 期限切れ後のループ再発防止)。
    DuplicateId,
    /// 既存イベントより新しくない(created_at 劣後、または同値で id 辞書順劣後)。
    NotNewer,
    /// `expiration` 超過(受信時点で期限切れ)。
    Expired,
    /// 鮮度窓外(`now - created_at > freshness_window_sec`)。
    Stale,
    /// 置換キーを作れない(`d` タグ欠落など。検証済みイベントでは発生しない)。
    Malformed,
}

struct Entry {
    summary: EventSummary,
    event: Event,
}

/// 署名済みイベントのローカル置換ストア。
pub struct EventStore {
    config: StoreConfig,
    entries: HashMap<StoreKey, Entry>,
    /// 現在保持中の event id 集合(第二の防壁の判定用)。
    ids: HashSet<String>,
    clock: Box<dyn Fn() -> u64 + Send>,
}

impl EventStore {
    /// 実時刻で作成する。
    pub fn new(config: StoreConfig) -> Self {
        Self::with_clock(config, Box::new(unix_now))
    }

    /// 時刻源を指定して作成する(テスト用)。
    pub fn with_clock(config: StoreConfig, clock: Box<dyn Fn() -> u64 + Send>) -> Self {
        Self {
            config,
            entries: HashMap::new(),
            ids: HashSet::new(),
            clock,
        }
    }

    /// 保持件数。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 空か。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 指定 event id を現在保持しているか。
    pub fn contains_id(&self, event_id: &str) -> bool {
        self.ids.contains(event_id)
    }

    /// 置換キーで現在の保持イベントを引く(鮮度に関わらず保持中の値を返す)。
    pub fn get(&self, kind: u16, pubkey: &str, channel_id: &str) -> Option<&Event> {
        self.entries
            .get(&(kind, pubkey.to_string(), channel_id.to_string()))
            .map(|e| &e.event)
    }

    /// イベントを格納する(置換規則・第二の防壁・クォータ・鮮度ゲートを適用)。
    ///
    /// 戻り値で「新規格納 / 置換 / 拒否(理由)」を判別できる(T037 が再伝搬判定に使う)。
    pub fn insert(&mut self, event: Event) -> InsertOutcome {
        let now = (self.clock)();
        let Some(summary) = EventSummary::from_event(&event) else {
            return InsertOutcome::Rejected(RejectReason::Malformed);
        };

        // 第二の防壁: 既に保持している同一 event id は不格納・不再伝搬。
        if self.ids.contains(&summary.event_id) {
            return InsertOutcome::Rejected(RejectReason::DuplicateId);
        }

        // 鮮度・期限ゲート(過去方向の自然減衰 = ループ終端の要 — ADR-0005 発見事項 1)。
        if now > summary.expiration {
            return InsertOutcome::Rejected(RejectReason::Expired);
        }
        if now.saturating_sub(summary.created_at) > self.config.freshness_window_sec {
            return InsertOutcome::Rejected(RejectReason::Stale);
        }

        let key = summary.key();
        match self.entries.get(&key) {
            Some(existing) => {
                if !is_newer(&summary, &existing.summary) {
                    return InsertOutcome::Rejected(RejectReason::NotNewer);
                }
                // 置換: 旧 id を id 集合から除去し新 id を登録。
                self.ids.remove(&existing.summary.event_id);
                self.ids.insert(summary.event_id.clone());
                self.entries.insert(key, Entry { summary, event });
                InsertOutcome::Replaced
            }
            None => {
                let pubkey = summary.pubkey.clone();
                self.ids.insert(summary.event_id.clone());
                self.entries.insert(key, Entry { summary, event });
                self.enforce_pubkey_quota(&pubkey);
                self.enforce_capacity();
                InsertOutcome::Stored
            }
        }
    }

    /// 供給・表示用のイベント(live かつ鮮度窓内かつ expiration 内)を返す。
    ///
    /// SYNC_REQ 応答・DiscoveredChannel ビューの供給源。ended / 鮮度切れ / 期限切れは含めない。
    pub fn live_fresh_events(&self) -> Vec<&Event> {
        let now = (self.clock)();
        self.entries
            .values()
            .filter(|e| self.is_live_fresh(&e.summary, now))
            .map(|e| &e.event)
            .collect()
    }

    /// 鮮度切れ / `expiration` 超過のエントリを物理削除する(戻り値は削除件数)。
    ///
    /// ended だけでは削除しない(tombstone 保持 — モジュール冒頭の説明を参照)。
    /// ended エントリは鮮度切れ / expiration 超過に至った時点で本メソッドが回収する。
    pub fn sweep(&mut self) -> usize {
        let now = (self.clock)();
        let victims: Vec<StoreKey> = self
            .entries
            .iter()
            .filter(|(_, e)| self.is_removable(&e.summary, now))
            .map(|(k, _)| k.clone())
            .collect();
        for key in &victims {
            self.remove_key(key);
        }
        victims.len()
    }

    fn is_live_fresh(&self, summary: &EventSummary, now: u64) -> bool {
        !summary.ended
            && now <= summary.expiration
            && now.saturating_sub(summary.created_at) <= self.config.freshness_window_sec
    }

    fn is_removable(&self, summary: &EventSummary, now: u64) -> bool {
        now > summary.expiration
            || now.saturating_sub(summary.created_at) > self.config.freshness_window_sec
    }

    fn remove_key(&mut self, key: &StoreKey) {
        if let Some(entry) = self.entries.remove(key) {
            self.ids.remove(&entry.summary.event_id);
        }
    }

    /// 同一 pubkey の保持数が上限を超える場合、当該 pubkey の古い順に破棄する。
    ///
    /// ADR-0004 §2: 容量管理と同種のストア内部方針であり、セキュリティイベントにしない。
    fn enforce_pubkey_quota(&mut self, pubkey: &str) {
        loop {
            let keys_for_pubkey: Vec<&StoreKey> = self
                .entries
                .iter()
                .filter(|(_, e)| e.summary.pubkey == pubkey)
                .map(|(k, _)| k)
                .collect();
            if keys_for_pubkey.len() <= self.config.max_events_per_pubkey {
                break;
            }
            let Some(victim) = self.oldest_among(keys_for_pubkey.into_iter()) else {
                break;
            };
            self.remove_key(&victim);
        }
    }

    /// 容量上限を超える場合、古い順に破棄する。
    ///
    /// kind 31311(announce)は [`StoreConfig::announce_store_quota`] の独立バケットで
    /// カウント・退避し、kind 31311 以外は [`StoreConfig::event_store_max`] のバケットで
    /// 行う(research R3 — announce の流入が他 kind の保持枠を消費しない)。
    fn enforce_capacity(&mut self) {
        self.enforce_capacity_for(
            |kind| kind == ANNOUNCE_KIND,
            self.config.announce_store_quota,
        );
        self.enforce_capacity_for(|kind| kind != ANNOUNCE_KIND, self.config.event_store_max);
    }

    /// `in_bucket` に該当する保持数が `quota` を超える場合、そのバケット内で古い順に破棄する。
    fn enforce_capacity_for(&mut self, in_bucket: impl Fn(u16) -> bool, quota: usize) {
        loop {
            let bucket_keys: Vec<&StoreKey> = self
                .entries
                .iter()
                .filter(|(k, _)| in_bucket(k.0))
                .map(|(k, _)| k)
                .collect();
            if bucket_keys.len() <= quota {
                break;
            }
            let Some(victim) = self.oldest_among(bucket_keys.into_iter()) else {
                break;
            };
            self.remove_key(&victim);
        }
    }

    /// 与えたキー群のうち最古(created_at 最小、同値なら id 辞書順小)のキーを返す。
    fn oldest_among<'a, I>(&self, keys: I) -> Option<StoreKey>
    where
        I: Iterator<Item = &'a StoreKey>,
    {
        keys.filter_map(|k| self.entries.get(k).map(|e| (k, &e.summary)))
            .min_by(|(_, a), (_, b)| {
                a.created_at
                    .cmp(&b.created_at)
                    .then_with(|| a.event_id.cmp(&b.event_id))
            })
            .map(|(k, _)| k.clone())
    }
}

/// `new` が `old` より新しい(置換すべき)か。created_at 最大、同値なら event id 辞書順大。
fn is_newer(new: &EventSummary, old: &EventSummary) -> bool {
    match new.created_at.cmp(&old.created_at) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => new.event_id > old.event_id,
    }
}

// ---------------------------------------------------------------------------
// DedupCache
// ---------------------------------------------------------------------------

/// 重複抑制キャッシュ(受信済みイベントの再処理・再伝搬ループ防止)。
///
/// 保持期間 = `max(600 秒, freshness_window_sec)`(ADR-0005 設計制約 MUST。EventStore から
/// 消えたイベントが「第二の防壁」の保護外になる残余経路を、鮮度窓を覆う保持で塞ぐ)。
pub struct DedupCache {
    retention_sec: u64,
    seen: HashMap<String, u64>,
    clock: Box<dyn Fn() -> u64 + Send>,
}

impl DedupCache {
    /// 鮮度窓に連動した保持期間で作成する(実時刻)。
    pub fn new(freshness_window_sec: u64) -> Self {
        Self::with_clock(freshness_window_sec, Box::new(unix_now))
    }

    /// 時刻源を指定して作成する(テスト用)。
    pub fn with_clock(freshness_window_sec: u64, clock: Box<dyn Fn() -> u64 + Send>) -> Self {
        Self {
            retention_sec: freshness_window_sec.max(DEDUP_MIN_RETENTION_SEC),
            seen: HashMap::new(),
            clock,
        }
    }

    /// 実際の保持期間(秒)。
    pub fn retention_sec(&self) -> u64 {
        self.retention_sec
    }

    /// 保持中エントリ数(期限切れを含みうる。厳密には [`Self::purge_expired`] 後に評価)。
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    /// 空か。
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }

    /// 重複判定つき記録。既知(保持期間内)なら `true`、未知なら記録して `false`。
    pub fn check_and_insert(&mut self, event_id: &str) -> bool {
        let now = (self.clock)();
        self.purge_expired(now);
        if self.seen.contains_key(event_id) {
            true
        } else {
            self.seen.insert(event_id.to_string(), now);
            false
        }
    }

    /// 記録せずに保持期間内かを判定する。
    pub fn contains(&self, event_id: &str) -> bool {
        let now = (self.clock)();
        self.seen
            .get(event_id)
            .map(|&t| now.saturating_sub(t) <= self.retention_sec)
            .unwrap_or(false)
    }

    fn purge_expired(&mut self, now: u64) {
        let retention = self.retention_sec;
        self.seen
            .retain(|_, &mut t| now.saturating_sub(t) <= retention);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::livechat::{ANNOUNCE_D, ThreadAnnounce};
    use crate::event::schema::{ChannelListing, ChannelStatus};
    use nostr::Keys;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// テスト用の `a` タグ値(`30311:<pubkey>:<guid>`)。
    fn channel_ref(pubkey: &Keys) -> String {
        format!(
            "30311:{}:{}",
            pubkey.public_key().to_hex(),
            "0123456789abcdef0123456789abcdef"
        )
    }

    /// kind 31311(スレ announce)イベントを作る(板 = pubkey 単位で `d="livechat"` 固定)。
    fn mk_announce(keys: &Keys, created: u64, generation: u32, title: &str) -> Event {
        ThreadAnnounce {
            channel: channel_ref(keys),
            title: title.into(),
            generation,
            key: created,
            res_count: None,
            tip: "198.51.100.1:7147".into(),
        }
        .sign(keys, created, 0)
        .unwrap()
    }

    fn listing(d: &str, status: ChannelStatus, title: &str) -> ChannelListing {
        ChannelListing {
            channel_id: d.into(),
            title: title.into(),
            summary: None,
            genre: None,
            status,
            starts: 1_700_000_000,
            current_participants: UNKNOWN,
            streaming: None,
            bitrate_kbps: None,
            content_type: None,
            tip: None,
            contact: None,
            relays: UNKNOWN,
            track: None,
        }
    }

    const UNKNOWN: i64 = -1;
    const D1: &str = "0123456789abcdef0123456789abcdef";
    const D2: &str = "0123456789abcdef0123456789abcdee";
    const D3: &str = "0123456789abcdef0123456789abcdea";

    fn mk(keys: &Keys, d: &str, created: u64, status: ChannelStatus, title: &str) -> Event {
        listing(d, status, title).sign(keys, created, 0).unwrap()
    }

    fn store_at(config: StoreConfig, clock: Arc<AtomicU64>) -> EventStore {
        EventStore::with_clock(config, Box::new(move || clock.load(Ordering::SeqCst)))
    }

    #[test]
    fn replacement_is_last_write_wins() {
        let clock = Arc::new(AtomicU64::new(1_700_000_100));
        let mut store = store_at(StoreConfig::default(), Arc::clone(&clock));
        let keys = Keys::generate();

        let older = mk(&keys, D1, 1_700_000_050, ChannelStatus::Live, "old");
        let newer = mk(&keys, D1, 1_700_000_080, ChannelStatus::Live, "new");

        assert_eq!(store.insert(older.clone()), InsertOutcome::Stored);
        assert_eq!(store.insert(newer.clone()), InsertOutcome::Replaced);
        assert_eq!(
            store
                .get(30311, &keys.public_key().to_hex(), D1)
                .unwrap()
                .id,
            newer.id
        );
        // 古いイベント(別 id)を後から入れても後退しない
        let older2 = mk(&keys, D1, 1_700_000_040, ChannelStatus::Live, "older2");
        assert_eq!(
            store.insert(older2),
            InsertOutcome::Rejected(RejectReason::NotNewer)
        );
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn same_created_at_tiebreak_by_event_id() {
        let clock = Arc::new(AtomicU64::new(1_700_000_100));
        let mut store = store_at(StoreConfig::default(), Arc::clone(&clock));
        let keys = Keys::generate();
        let created = 1_700_000_090;

        let a = mk(&keys, D1, created, ChannelStatus::Live, "alpha");
        let b = mk(&keys, D1, created, ChannelStatus::Live, "beta");
        assert_ne!(a.id, b.id);
        let (small, large) = if a.id.to_hex() < b.id.to_hex() {
            (a, b)
        } else {
            (b, a)
        };

        // 小 id → 大 id は置換
        assert_eq!(store.insert(small.clone()), InsertOutcome::Stored);
        assert_eq!(store.insert(large.clone()), InsertOutcome::Replaced);
        assert_eq!(
            store
                .get(30311, &keys.public_key().to_hex(), D1)
                .unwrap()
                .id,
            large.id
        );
        // 大 id が既にある状態で小 id(別 id・同 created)は劣後
        assert_eq!(
            store.insert(small),
            InsertOutcome::Rejected(RejectReason::NotNewer)
        );
    }

    #[test]
    fn duplicate_event_id_is_rejected() {
        let clock = Arc::new(AtomicU64::new(1_700_000_100));
        let mut store = store_at(StoreConfig::default(), Arc::clone(&clock));
        let keys = Keys::generate();
        let e = mk(&keys, D1, 1_700_000_080, ChannelStatus::Live, "x");
        assert_eq!(store.insert(e.clone()), InsertOutcome::Stored);
        assert_eq!(
            store.insert(e.clone()),
            InsertOutcome::Rejected(RejectReason::DuplicateId)
        );
        assert!(!store.insert(e).should_propagate());
    }

    #[test]
    fn capacity_evicts_oldest_first() {
        let clock = Arc::new(AtomicU64::new(1_700_000_100));
        let config = StoreConfig {
            event_store_max: 2,
            ..StoreConfig::default()
        };
        let mut store = store_at(config, Arc::clone(&clock));
        let keys = Keys::generate();

        store.insert(mk(&keys, D1, 1_700_000_010, ChannelStatus::Live, "1"));
        store.insert(mk(&keys, D2, 1_700_000_020, ChannelStatus::Live, "2"));
        store.insert(mk(&keys, D3, 1_700_000_030, ChannelStatus::Live, "3"));

        assert_eq!(store.len(), 2);
        // 最古(D1)が破棄されている
        assert!(store.get(30311, &keys.public_key().to_hex(), D1).is_none());
        assert!(store.get(30311, &keys.public_key().to_hex(), D2).is_some());
        assert!(store.get(30311, &keys.public_key().to_hex(), D3).is_some());
    }

    #[test]
    fn pubkey_quota_evicts_oldest_of_that_pubkey() {
        let clock = Arc::new(AtomicU64::new(1_700_000_100));
        let config = StoreConfig {
            max_events_per_pubkey: 2,
            ..StoreConfig::default()
        };
        let mut store = store_at(config, Arc::clone(&clock));
        let keys = Keys::generate();

        store.insert(mk(&keys, D1, 1_700_000_010, ChannelStatus::Live, "1"));
        store.insert(mk(&keys, D2, 1_700_000_020, ChannelStatus::Live, "2"));
        store.insert(mk(&keys, D3, 1_700_000_030, ChannelStatus::Live, "3"));

        assert_eq!(store.len(), 2);
        assert!(store.get(30311, &keys.public_key().to_hex(), D1).is_none());
    }

    #[test]
    fn ended_is_tombstoned_and_excluded_from_live() {
        let clock = Arc::new(AtomicU64::new(1_700_000_100));
        let mut store = store_at(StoreConfig::default(), Arc::clone(&clock));
        let keys = Keys::generate();

        let live = mk(&keys, D1, 1_700_000_050, ChannelStatus::Live, "live");
        assert_eq!(store.insert(live), InsertOutcome::Stored);
        assert_eq!(store.live_fresh_events().len(), 1);

        let ended = mk(&keys, D1, 1_700_000_080, ChannelStatus::Ended, "ended");
        assert_eq!(store.insert(ended.clone()), InsertOutcome::Replaced);
        // 供給・表示からは除外(ended)
        assert!(store.live_fresh_events().is_empty());
        // 物理的には tombstone として保持(ロールバック防止)
        assert!(store.contains_id(&ended.id.to_hex()));

        // 古い live のリプレイ(別 id・同キー)は後退させない(StoreMonotonic)
        let old_live = mk(&keys, D1, 1_700_000_060, ChannelStatus::Live, "revive");
        assert_eq!(
            store.insert(old_live),
            InsertOutcome::Rejected(RejectReason::NotNewer)
        );
        assert!(store.live_fresh_events().is_empty());
    }

    #[test]
    fn sweep_removes_stale_and_expired() {
        let clock = Arc::new(AtomicU64::new(1_700_000_100));
        let mut store = store_at(StoreConfig::default(), Arc::clone(&clock));
        let keys = Keys::generate();
        store.insert(mk(&keys, D1, 1_700_000_050, ChannelStatus::Live, "x"));
        assert_eq!(store.len(), 1);

        // 鮮度窓(600)・expiration(created+600)を超える時刻へ進める
        clock.store(1_700_000_050 + 601, Ordering::SeqCst);
        assert_eq!(store.sweep(), 1);
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn insert_rejects_stale_events() {
        // 鮮度窓 100 秒・created から 200 秒後 → Stale(expiration=created+600 はまだ先)
        let created = 1_700_000_000;
        let clock = Arc::new(AtomicU64::new(created + 200));
        let config = StoreConfig {
            freshness_window_sec: 100,
            ..StoreConfig::default()
        };
        let mut store = store_at(config, Arc::clone(&clock));
        let keys = Keys::generate();
        let stale = mk(&keys, D1, created, ChannelStatus::Live, "stale");
        assert_eq!(
            store.insert(stale),
            InsertOutcome::Rejected(RejectReason::Stale)
        );
        assert!(store.is_empty());
    }

    #[test]
    fn insert_rejects_expired_events() {
        // 鮮度窓を大きく取り、expiration(created+600)超過のみを発火させる
        let created = 1_700_000_000;
        let clock = Arc::new(AtomicU64::new(created + 700));
        let config = StoreConfig {
            freshness_window_sec: 100_000,
            ..StoreConfig::default()
        };
        let mut store = store_at(config, Arc::clone(&clock));
        let keys = Keys::generate();
        let expired = mk(&keys, D1, created, ChannelStatus::Live, "expired");
        assert_eq!(
            store.insert(expired),
            InsertOutcome::Rejected(RejectReason::Expired)
        );
        assert!(store.is_empty());
    }

    #[test]
    fn dedup_retention_links_to_freshness_window() {
        assert_eq!(DedupCache::new(600).retention_sec(), 600);
        assert_eq!(DedupCache::new(900).retention_sec(), 900);
        // 下限 600 を下回らない
        assert_eq!(DedupCache::new(300).retention_sec(), 600);
    }

    #[test]
    fn dedup_detects_duplicate_until_expiry() {
        let clock = Arc::new(AtomicU64::new(1000));
        let clock2 = Arc::clone(&clock);
        let mut cache =
            DedupCache::with_clock(600, Box::new(move || clock2.load(Ordering::SeqCst)));

        let id = "abc";
        assert!(!cache.check_and_insert(id)); // 初回 = 未知
        assert!(cache.check_and_insert(id)); // 2 回目 = 重複
        assert!(cache.contains(id));

        // 保持期間(600)を超えると忘れる
        clock.store(1000 + 601, Ordering::SeqCst);
        assert!(!cache.contains(id));
        assert!(!cache.check_and_insert(id)); // 期限切れ後は再び未知扱い
    }

    // ---- kind 31311(スレ announce)の独立保持枠(T011 / research R3) ----

    #[test]
    fn announce_capacity_is_independent_from_event_store_max() {
        // 31311 用容量を 1 に絞っても、30311 用の event_store_max は消費されない。
        let clock = Arc::new(AtomicU64::new(1_700_000_100));
        let config = StoreConfig {
            announce_store_quota: 1,
            ..StoreConfig::default()
        };
        let mut store = store_at(config, Arc::clone(&clock));
        let keys30311 = Keys::generate();

        // 30311 側を 3 件格納(既定 event_store_max=4096 の枠内)。
        store.insert(mk(&keys30311, D1, 1_700_000_010, ChannelStatus::Live, "1"));
        store.insert(mk(&keys30311, D2, 1_700_000_020, ChannelStatus::Live, "2"));
        store.insert(mk(&keys30311, D3, 1_700_000_030, ChannelStatus::Live, "3"));
        assert_eq!(store.len(), 3);

        // 31311 側(別 pubkey = 別板)を 2 件格納 → announce_store_quota=1 のため
        // 古い方(board A)が退避され、30311 側の 3 件はそのまま残る。
        let board_a = Keys::generate();
        let board_b = Keys::generate();
        store.insert(mk_announce(&board_a, 1_700_000_040, 1, "board A"));
        store.insert(mk_announce(&board_b, 1_700_000_050, 1, "board B"));

        assert_eq!(store.len(), 4, "30311 の 3 件 + 31311 の 1 件");
        assert!(
            store
                .get(30311, &keys30311.public_key().to_hex(), D1)
                .is_some(),
            "31311 の容量退避が 30311 の枠を消費してはならない"
        );
        assert!(
            store
                .get(ANNOUNCE_KIND, &board_a.public_key().to_hex(), ANNOUNCE_D)
                .is_none(),
            "announce_store_quota 超過で古い board A の announce が退避されるべき"
        );
        assert!(
            store
                .get(ANNOUNCE_KIND, &board_b.public_key().to_hex(), ANNOUNCE_D)
                .is_some()
        );
    }

    #[test]
    fn announce_is_replaced_per_board_with_fixed_d() {
        // 同一板(同一 pubkey)の announce は d="livechat" 固定により常に最新 1 件へ置換される。
        let clock = Arc::new(AtomicU64::new(1_700_000_100));
        let mut store = store_at(StoreConfig::default(), Arc::clone(&clock));
        let keys = Keys::generate();

        let gen1 = mk_announce(&keys, 1_700_000_010, 1, "スレ1");
        let gen2 = mk_announce(&keys, 1_700_000_020, 2, "スレ2");
        assert_eq!(store.insert(gen1), InsertOutcome::Stored);
        assert_eq!(store.insert(gen2.clone()), InsertOutcome::Replaced);

        assert_eq!(store.len(), 1);
        let kept = store
            .get(ANNOUNCE_KIND, &keys.public_key().to_hex(), ANNOUNCE_D)
            .unwrap();
        assert_eq!(kept.id, gen2.id);
    }

    #[test]
    fn announce_capacity_evicts_oldest_first_within_own_bucket() {
        let clock = Arc::new(AtomicU64::new(1_700_000_100));
        let config = StoreConfig {
            announce_store_quota: 2,
            ..StoreConfig::default()
        };
        let mut store = store_at(config, Arc::clone(&clock));
        let a = Keys::generate();
        let b = Keys::generate();
        let c = Keys::generate();

        store.insert(mk_announce(&a, 1_700_000_010, 1, "A"));
        store.insert(mk_announce(&b, 1_700_000_020, 1, "B"));
        store.insert(mk_announce(&c, 1_700_000_030, 1, "C"));

        assert_eq!(store.len(), 2);
        assert!(
            store
                .get(ANNOUNCE_KIND, &a.public_key().to_hex(), ANNOUNCE_D)
                .is_none(),
            "最古の板 A の announce が退避されるべき"
        );
        assert!(
            store
                .get(ANNOUNCE_KIND, &b.public_key().to_hex(), ANNOUNCE_D)
                .is_some()
        );
        assert!(
            store
                .get(ANNOUNCE_KIND, &c.public_key().to_hex(), ANNOUNCE_D)
                .is_some()
        );
    }
}

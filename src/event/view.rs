//! DiscoveredChannel ビュー(T039)
//!
//! EventStore 上の検証済み kind 30311 イベントから視聴者の一覧を構築する
//! (data-model §DiscoveredChannel)。一覧のキーは `(author_pubkey, channel_id)`。
//! `status=ended` / 鮮度切れは除去し、ミュート一致は除外する(既定オープン型 — FR-008)。
//!
//! [`DiscoveredChannel`] と [`ChannelDirectory`] は Web 層(T041/T042)との
//! **凍結インターフェース**。フィールド・メソッドシグネチャを変更する場合は
//! 統括(orchestrator)の承認を得ること。
//!
//! ## 集約の実装(凍結部の下に追記 — T039)
//!
//! [`aggregate`] が EventStore の live かつ鮮度窓内イベント(供給元が
//! [`crate::event::store::EventStore::live_fresh_events`] で `ended`・鮮度切れを
//! 除外済み)にミュートを適用し、`(author_pubkey, channel_id)` 単位の一覧を
//! `created_at` 降順で構築する。受信ピア集合 `sources` は
//! `(kind, pubkey, d)` 置換キー単位で保持する(gossip 受信パイプライン T037 が更新)。

use std::collections::{HashMap, HashSet};

use nostr::Event;

use crate::event::schema::{ChannelListing, EventSummary};
use crate::store::{MuteEntry, MuteKind};

/// 発見したチャンネル(一覧の 1 行)。
///
/// 検証済みイベント(受信検証 1〜6 通過)からのみ構築する。
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredChannel {
    /// 発行者 pubkey(hex 64 小文字)。
    pub author_pubkey: String,
    /// チャンネル GUID(`d` タグ、hex 32 小文字)。
    pub channel_id: String,
    /// 復元済みタグ写像(title / genre / listeners / tip / track 等)。
    pub listing: ChannelListing,
    /// イベントの created_at(unix 秒)。
    pub created_at: u64,
    /// このイベントを受信したピアアドレス集合(UI 表示・品質判断用)。
    pub source_peers: Vec<String>,
}

/// チャンネル一覧の供給元(Web 層が参照する読み取り専用ビュー)。
///
/// 実装は gossip 受信パイプライン側(T037/T039)が担う。契約:
/// - `live` かつ鮮度窓内のチャンネルのみを返す(`ended`・鮮度切れは含めない — FR-006)
/// - ミュート一致(pubkey / channel の OR — data-model §MuteEntry)は含めない
/// - 更新の新しい順(`created_at` 降順)に整列して返す
pub trait ChannelDirectory: Send + Sync {
    /// 現在の一覧スナップショットを返す。
    fn list(&self) -> Vec<DiscoveredChannel>;
}

// ---------------------------------------------------------------------------
// 集約(T039)— 凍結部の下に追記
// ---------------------------------------------------------------------------

/// `(kind, pubkey, d)` 置換キーごとの受信ピア集合。
///
/// gossip 受信パイプライン(T037)が格納成功のたびに更新し、[`aggregate`] が
/// [`DiscoveredChannel::source_peers`] へ写す。ローカル発行イベントは空集合。
pub type SourceMap = HashMap<(u16, String, String), Vec<String>>;

/// ミュート判定集合(pubkey 単位・channel 単位を独立に保持)。
///
/// data-model §MuteEntry の適用規則: 両単位は独立評価・**いずれか一致で非表示(OR)**・
/// 優先順位なし。
#[derive(Debug, Default, Clone)]
pub struct MuteSet {
    pubkeys: HashSet<String>,
    channels: HashSet<String>,
}

impl MuteSet {
    /// ミュートエントリ列から判定集合を構築する。
    pub fn from_entries(entries: &[MuteEntry]) -> Self {
        let mut set = MuteSet::default();
        for e in entries {
            match e.kind {
                MuteKind::Pubkey => {
                    set.pubkeys.insert(e.value.clone());
                }
                MuteKind::Channel => {
                    set.channels.insert(e.value.clone());
                }
            }
        }
        set
    }

    /// `author_pubkey` または `channel_id` のいずれかがミュート対象なら `true`(OR)。
    pub fn is_muted(&self, author_pubkey: &str, channel_id: &str) -> bool {
        self.pubkeys.contains(author_pubkey) || self.channels.contains(channel_id)
    }
}

/// live かつ鮮度窓内のイベント列から一覧を構築する(T039)。
///
/// - `events`: [`crate::event::store::EventStore::live_fresh_events`] の結果
///   (`ended`・鮮度切れ・`expiration` 超過は供給元で除外済み)。
/// - `sources`: 置換キーごとの受信ピア集合。
/// - `mutes`: ミュート判定集合(一致行を除外)。
///
/// `(author_pubkey, channel_id)` を単位とし、**同名別 pubkey は別行**として併存する。
/// 返り値は `created_at` 降順(同値は pubkey→channel_id で決定的整列)。
pub fn aggregate<'a>(
    events: impl IntoIterator<Item = &'a Event>,
    sources: &SourceMap,
    mutes: &MuteSet,
) -> Vec<DiscoveredChannel> {
    let mut out: Vec<DiscoveredChannel> = Vec::new();
    for event in events {
        let Some(summary) = EventSummary::from_event(event) else {
            continue;
        };
        if mutes.is_muted(&summary.pubkey, &summary.channel_id) {
            continue;
        }
        // 検証済みイベントのため復元は成功する前提。念のため失敗行はスキップ。
        let Ok(listing) = ChannelListing::from_event(event) else {
            continue;
        };
        let source_peers = sources.get(&summary.key()).cloned().unwrap_or_default();
        out.push(DiscoveredChannel {
            author_pubkey: summary.pubkey,
            channel_id: summary.channel_id,
            listing,
            created_at: summary.created_at,
            source_peers,
        });
    }
    out.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| a.author_pubkey.cmp(&b.author_pubkey))
            .then_with(|| a.channel_id.cmp(&b.channel_id))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::schema::{ChannelListing, ChannelStatus};
    use nostr::Keys;

    const D1: &str = "0123456789abcdef0123456789abcdef";
    const D2: &str = "0123456789abcdef0123456789abcdee";

    fn listing(d: &str, title: &str, status: ChannelStatus) -> ChannelListing {
        ChannelListing {
            channel_id: d.into(),
            title: title.into(),
            summary: None,
            genre: Some("game".into()),
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

    fn event(keys: &Keys, d: &str, title: &str, created: u64) -> Event {
        listing(d, title, ChannelStatus::Live)
            .sign(keys, created, 0)
            .unwrap()
    }

    #[test]
    fn aggregates_and_sorts_by_created_desc() {
        let keys = Keys::generate();
        let e_old = event(&keys, D1, "old", 1_700_000_010);
        let e_new = event(&keys, D2, "new", 1_700_000_090);
        let sources = SourceMap::new();
        let rows = aggregate([&e_old, &e_new], &sources, &MuteSet::default());
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].channel_id, D2, "新しい方が先頭");
        assert_eq!(rows[1].channel_id, D1);
    }

    #[test]
    fn same_channel_id_different_pubkey_are_separate_rows() {
        let a = Keys::generate();
        let b = Keys::generate();
        let ea = event(&a, D1, "a", 1_700_000_010);
        let eb = event(&b, D1, "b", 1_700_000_010);
        let rows = aggregate([&ea, &eb], &SourceMap::new(), &MuteSet::default());
        assert_eq!(rows.len(), 2, "同名別 pubkey は別行");
    }

    #[test]
    fn mute_by_pubkey_or_channel_removes_row() {
        let keys = Keys::generate();
        let e1 = event(&keys, D1, "one", 1_700_000_010);
        let e2 = event(&keys, D2, "two", 1_700_000_020);

        // channel 単位ミュート: D1 のみ除外
        let mut mutes = MuteSet::default();
        mutes.channels.insert(D1.to_string());
        let rows = aggregate([&e1, &e2], &SourceMap::new(), &mutes);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].channel_id, D2);

        // pubkey 単位ミュート: 全行除外
        let mut mutes = MuteSet::default();
        mutes.pubkeys.insert(keys.public_key().to_hex());
        let rows = aggregate([&e1, &e2], &SourceMap::new(), &mutes);
        assert!(rows.is_empty(), "pubkey ミュートは全チャンネルを除外");
    }

    #[test]
    fn source_peers_are_attached_by_key() {
        let keys = Keys::generate();
        let e = event(&keys, D1, "x", 1_700_000_010);
        let summary = EventSummary::from_event(&e).unwrap();
        let mut sources = SourceMap::new();
        sources.insert(summary.key(), vec!["198.51.100.9:7147".to_string()]);
        let rows = aggregate([&e], &sources, &MuteSet::default());
        assert_eq!(rows[0].source_peers, vec!["198.51.100.9:7147".to_string()]);
    }

    #[test]
    fn mute_set_from_entries_maps_kinds() {
        let entries = vec![
            MuteEntry {
                id: 1,
                kind: MuteKind::Pubkey,
                value: "pk".into(),
                created_at: 0,
            },
            MuteEntry {
                id: 2,
                kind: MuteKind::Channel,
                value: "ch".into(),
                created_at: 0,
            },
        ];
        let set = MuteSet::from_entries(&entries);
        assert!(set.is_muted("pk", "other"));
        assert!(set.is_muted("other", "ch"));
        assert!(!set.is_muted("x", "y"));
    }
}

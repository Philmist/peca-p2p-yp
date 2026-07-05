//! AnnouncedChannel レジストリ(T027)
//!
//! data-model.md §AnnouncedChannel の検証ルール(文字列長・制御文字除去・数値範囲・
//! GUID 16 バイト)を適用し、掲載中チャンネルをメモリ上で管理する。**persona_id は保持しない**
//! — ペルソナ割当は掲載エンジン(T029)が channel_id→persona 対応表で管理する設計判断による
//! (PCP 層はペルソナを知らない)。
//!
//! [`ChannelRegistry`] は `Arc` 共有(Send + Sync)で、周期再発行のための [`snapshot`] と、
//! announced/updated/ended を伝える変更購読 API([`subscribe`])を提供する。ended 通知は
//! 最終状態の [`AnnouncedChannel`] を伴う(最終 `status=ended` イベント発行に使う)。
//!
//! [`snapshot`]: ChannelRegistry::snapshot
//! [`subscribe`]: ChannelRegistry::subscribe

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::broadcast;

use crate::event::schema::{ChannelListing, ChannelStatus, MAX_BITRATE_KBPS, Track, UNKNOWN_COUNT};
use crate::security;

/// チャンネルの状態(data-model §AnnouncedChannel 状態遷移)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelState {
    /// HELO/OLEH 完了後の初回 BCST。
    Announced,
    /// 2 回目以降の BCST(情報更新)。
    Updating,
    /// playing=false / PCP_QUIT / TCP 切断で終了。
    Ended,
}

/// 曲情報(`titl`/`crea`/`albm`)。track url は v1 では常に空(対応 atom を受信しない)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackInfo {
    /// 曲名(`titl`)。
    pub title: String,
    /// アーティスト(`crea`)。
    pub creator: String,
    /// アルバム(`albm`)。
    pub album: String,
}

impl TrackInfo {
    /// 全要素が空か。
    pub fn is_empty(&self) -> bool {
        self.title.is_empty() && self.creator.is_empty() && self.album.is_empty()
    }
}

/// 掲載中チャンネル(data-model §AnnouncedChannel、persona_id を除く)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnouncedChannel {
    /// ChannelID(GUID 16 バイト固定)。
    pub channel_id: [u8; 16],
    /// チャンネル名(`name`)。制御文字除去・1..256 バイト。
    pub name: String,
    /// ジャンル(`gnre`)。0..256 バイト。
    pub genre: String,
    /// 説明(`desc`)。0..1024 バイト。
    pub description: String,
    /// コンタクト URL(`url`)。0..512 バイト。
    pub contact_url: String,
    /// ビットレート kbps(`bitr`)。0..100_000。
    pub bitrate_kbps: u32,
    /// コンテンツ種別(`type`)。0..32 バイト英数。
    pub content_type: String,
    /// 曲情報(`titl`/`crea`/`albm`)。
    pub track: TrackInfo,
    /// トラッカー(グローバル `ip:port`)。firewalled 時は `None`。
    pub tracker: Option<String>,
    /// 直接視聴者数(`numl`)。不明は [`UNKNOWN_COUNT`](-1)。
    pub listeners: i64,
    /// リレー数(`numr`)。不明は [`UNKNOWN_COUNT`](-1)。
    pub relays_cnt: i64,
    /// 受信(初回掲載)時刻(unix 秒)。
    pub started_at: u64,
    /// 状態。
    pub state: ChannelState,
}

/// atom から抽出した検証前のチャンネル情報([`AnnouncedChannel::from_raw`] への入力)。
#[derive(Debug, Clone)]
pub struct RawChannelInfo {
    /// ChannelID(16 バイト。長さ検証はセッション側で済ませる前提)。
    pub channel_id: [u8; 16],
    /// `name`。
    pub name: String,
    /// `gnre`。
    pub genre: String,
    /// `desc`。
    pub description: String,
    /// `url`。
    pub contact_url: String,
    /// `bitr`。
    pub bitrate: i64,
    /// `type`。
    pub content_type: String,
    /// `titl`。
    pub track_title: String,
    /// `crea`。
    pub track_creator: String,
    /// `albm`。
    pub track_album: String,
    /// PCP_HOST から組んだ `ip:port`(firewalled 時は `None`)。
    pub tracker: Option<String>,
    /// `numl`。
    pub listeners: i64,
    /// `numr`。
    pub relays_cnt: i64,
    /// 受信時刻(unix 秒)。
    pub started_at: u64,
}

impl AnnouncedChannel {
    /// 検証前の情報に data-model の検証ルールを適用して構築する。
    ///
    /// 文字列は制御文字除去のうえバイト長上限で切詰め(切詰め許容 — loopback の利用者自身の
    /// ソフトウェアのため)、数値は範囲内へクランプ、トラッカーは `ip:port` 形式を検証する。
    pub fn from_raw(raw: RawChannelInfo, state: ChannelState) -> Self {
        Self {
            channel_id: raw.channel_id,
            name: sanitize_text(&raw.name, 256),
            genre: sanitize_text(&raw.genre, 256),
            description: sanitize_text(&raw.description, 1024),
            contact_url: sanitize_text(&raw.contact_url, 512),
            bitrate_kbps: clamp_bitrate(raw.bitrate),
            content_type: sanitize_content_type(&raw.content_type),
            track: TrackInfo {
                title: sanitize_text(&raw.track_title, 256),
                creator: sanitize_text(&raw.track_creator, 256),
                album: sanitize_text(&raw.track_album, 256),
            },
            tracker: raw.tracker.as_deref().and_then(validate_tracker),
            listeners: clamp_count(raw.listeners),
            relays_cnt: clamp_count(raw.relays_cnt),
            started_at: raw.started_at,
            state,
        }
    }

    /// ChannelID の hex 表現(32 桁小文字)= 30311 の `d` タグ。
    pub fn channel_id_hex(&self) -> String {
        let mut s = String::with_capacity(32);
        for b in self.channel_id {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    /// kind 30311 の掲載情報(タグ写像)へ変換する(contracts/nostr-events.md)。
    ///
    /// 署名は行わない(ペルソナ割当は掲載エンジンの責務)。firewalled(tracker なし)時は
    /// `streaming` / `tip` を省略し、listeners/relays が負値なら [`UNKNOWN_COUNT`] として
    /// タグを省略させる。track url 要素は常に空。
    pub fn to_listing(&self) -> ChannelListing {
        let channel_id = self.channel_id_hex();
        let streaming = self
            .tracker
            .as_ref()
            .map(|tip| format!("pcp://{tip}/{channel_id}"));
        ChannelListing {
            channel_id,
            title: self.name.clone(),
            summary: non_empty(&self.description),
            genre: non_empty(&self.genre).map(|g| g.to_ascii_lowercase()),
            status: match self.state {
                ChannelState::Ended => ChannelStatus::Ended,
                ChannelState::Announced | ChannelState::Updating => ChannelStatus::Live,
            },
            starts: self.started_at,
            current_participants: self.listeners,
            streaming,
            bitrate_kbps: (self.bitrate_kbps > 0).then_some(self.bitrate_kbps as u64),
            content_type: non_empty(&self.content_type),
            tip: self.tracker.clone(),
            contact: non_empty(&self.contact_url),
            relays: self.relays_cnt,
            track: (!self.track.is_empty()).then(|| Track {
                title: self.track.title.clone(),
                artist: self.track.creator.clone(),
                album: self.track.album.clone(),
                url: String::new(),
            }),
        }
    }
}

/// 空文字列を `None`、非空を `Some` に変換する。
fn non_empty(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

// ---------------------------------------------------------------------------
// 検証ヘルパ(data-model §AnnouncedChannel の検証)
// ---------------------------------------------------------------------------

/// バイト長上限で UTF-8 境界を保って切詰める。
fn truncate_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// 制御文字を除去し、バイト長上限で切詰める。
pub fn sanitize_text(raw: &str, max_bytes: usize) -> String {
    truncate_bytes(&security::strip_control_chars(raw), max_bytes)
}

/// コンテンツ種別を英数のみへ絞り 32 バイトで切詰める(data-model: 0..32 バイト英数)。
pub fn sanitize_content_type(raw: &str) -> String {
    let filtered: String = raw.chars().filter(char::is_ascii_alphanumeric).collect();
    truncate_bytes(&filtered, 32)
}

/// ビットレートを 0..=100_000 へクランプする。
pub fn clamp_bitrate(raw: i64) -> u32 {
    raw.clamp(0, MAX_BITRATE_KBPS as i64) as u32
}

/// カウント(listeners/relays)を下限 [`UNKNOWN_COUNT`](-1)へクランプする。
pub fn clamp_count(raw: i64) -> i64 {
    raw.max(UNKNOWN_COUNT)
}

/// トラッカー `ip:port` の妥当性検証(不正なら `None`)。
pub fn validate_tracker(raw: &str) -> Option<String> {
    raw.parse::<std::net::SocketAddr>()
        .ok()
        .map(|a| a.to_string())
}

// ---------------------------------------------------------------------------
// レジストリ
// ---------------------------------------------------------------------------

/// レジストリの変更通知(掲載エンジン T029 が購読する)。
#[derive(Debug, Clone)]
pub enum ChannelChange {
    /// 新規掲載。
    Announced(AnnouncedChannel),
    /// 既存チャンネルの情報更新。
    Updated(AnnouncedChannel),
    /// 終了(最終状態を伴う。最終 `status=ended` 発行に使う)。
    Ended(AnnouncedChannel),
}

/// [`ChannelRegistry::upsert`] の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpsertOutcome {
    /// 新規掲載として登録した。
    Announced,
    /// 既存チャンネルを更新した。
    Updated,
}

/// 掲載中チャンネルのレジストリ(`Arc` 共有・Send + Sync)。
pub struct ChannelRegistry {
    inner: Mutex<HashMap<[u8; 16], AnnouncedChannel>>,
    tx: broadcast::Sender<ChannelChange>,
}

/// ポイズン時も内部値を回収してロックを返す(パニックしない)。
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl ChannelRegistry {
    /// 空のレジストリを `Arc` で作る。
    pub fn new() -> Arc<Self> {
        let (tx, _rx) = broadcast::channel(256);
        Arc::new(Self {
            inner: Mutex::new(HashMap::new()),
            tx,
        })
    }

    /// 変更通知を購読する(announced/updated/ended)。
    pub fn subscribe(&self) -> broadcast::Receiver<ChannelChange> {
        self.tx.subscribe()
    }

    /// 現在掲載中の全チャンネルのスナップショット(周期再発行用)。
    pub fn snapshot(&self) -> Vec<AnnouncedChannel> {
        lock(&self.inner).values().cloned().collect()
    }

    /// 掲載中チャンネル数。
    pub fn len(&self) -> usize {
        lock(&self.inner).len()
    }

    /// 掲載中チャンネルがないか。
    pub fn is_empty(&self) -> bool {
        lock(&self.inner).is_empty()
    }

    /// 指定 ChannelID が掲載中か。
    pub fn contains(&self, channel_id: &[u8; 16]) -> bool {
        lock(&self.inner).contains_key(channel_id)
    }

    /// チャンネルを登録または更新する。状態(Announced/Updating)は自動設定する。
    pub fn upsert(&self, mut channel: AnnouncedChannel) -> UpsertOutcome {
        let mut guard = lock(&self.inner);
        let outcome = if guard.contains_key(&channel.channel_id) {
            UpsertOutcome::Updated
        } else {
            UpsertOutcome::Announced
        };
        channel.state = match outcome {
            UpsertOutcome::Announced => ChannelState::Announced,
            UpsertOutcome::Updated => ChannelState::Updating,
        };
        guard.insert(channel.channel_id, channel.clone());
        drop(guard);

        let change = match outcome {
            UpsertOutcome::Announced => ChannelChange::Announced(channel),
            UpsertOutcome::Updated => ChannelChange::Updated(channel),
        };
        let _ = self.tx.send(change);
        outcome
    }

    /// チャンネルを終了(ended)にし、最終状態を通知して除去する。存在すれば `true`。
    pub fn end(&self, channel_id: &[u8; 16]) -> bool {
        let mut guard = lock(&self.inner);
        let Some(mut channel) = guard.remove(channel_id) else {
            return false;
        };
        drop(guard);
        channel.state = ChannelState::Ended;
        let _ = self.tx.send(ChannelChange::Ended(channel));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(id: u8, name: &str) -> RawChannelInfo {
        RawChannelInfo {
            channel_id: [id; 16],
            name: name.into(),
            genre: "Game".into(),
            description: "説明".into(),
            contact_url: "http://example.com/".into(),
            bitrate: 500,
            content_type: "FLV".into(),
            track_title: "t".into(),
            track_creator: "c".into(),
            track_album: "a".into(),
            tracker: Some("192.0.2.1:7144".into()),
            listeners: 3,
            relays_cnt: 1,
            started_at: 1_700_000_000,
        }
    }

    #[test]
    fn sanitize_strips_controls_and_truncates() {
        assert_eq!(sanitize_text("a\u{7}b", 256), "ab");
        // 3 バイト文字は境界を割らずに切詰める
        assert_eq!(sanitize_text("ああ", 5), "あ");
    }

    #[test]
    fn content_type_keeps_alnum_only() {
        assert_eq!(sanitize_content_type("FL V!"), "FLV");
    }

    #[test]
    fn bitrate_and_count_clamping() {
        assert_eq!(clamp_bitrate(-5), 0);
        assert_eq!(clamp_bitrate(200_000), 100_000);
        assert_eq!(clamp_count(-9), -1);
        assert_eq!(clamp_count(7), 7);
    }

    #[test]
    fn invalid_tracker_is_dropped() {
        assert_eq!(validate_tracker("not-an-addr"), None);
        assert_eq!(
            validate_tracker("192.0.2.1:7144").as_deref(),
            Some("192.0.2.1:7144")
        );
    }

    #[test]
    fn from_raw_applies_validation() {
        let ch = AnnouncedChannel::from_raw(raw(0xAA, "配信"), ChannelState::Announced);
        assert_eq!(ch.name, "配信");
        assert_eq!(ch.bitrate_kbps, 500);
        assert_eq!(ch.channel_id_hex(), "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    }

    #[test]
    fn upsert_then_update_emits_changes() {
        let registry = ChannelRegistry::new();
        let mut rx = registry.subscribe();
        let ch = AnnouncedChannel::from_raw(raw(1, "A"), ChannelState::Announced);
        assert_eq!(registry.upsert(ch.clone()), UpsertOutcome::Announced);
        assert!(matches!(rx.try_recv(), Ok(ChannelChange::Announced(_))));

        let ch2 = AnnouncedChannel::from_raw(raw(1, "A2"), ChannelState::Announced);
        assert_eq!(registry.upsert(ch2), UpsertOutcome::Updated);
        match rx.try_recv() {
            Ok(ChannelChange::Updated(c)) => assert_eq!(c.name, "A2"),
            other => panic!("updated を期待: {other:?}"),
        }
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn end_removes_and_notifies_final_state() {
        let registry = ChannelRegistry::new();
        let mut rx = registry.subscribe();
        registry.upsert(AnnouncedChannel::from_raw(
            raw(2, "B"),
            ChannelState::Announced,
        ));
        let _ = rx.try_recv();
        assert!(registry.end(&[2u8; 16]));
        match rx.try_recv() {
            Ok(ChannelChange::Ended(c)) => {
                assert_eq!(c.state, ChannelState::Ended);
                assert_eq!(c.channel_id, [2u8; 16]);
            }
            other => panic!("ended を期待: {other:?}"),
        }
        assert!(registry.is_empty());
        assert!(!registry.end(&[2u8; 16]), "二重 end は false");
    }

    #[test]
    fn to_listing_maps_and_omits_when_firewalled() {
        let mut ch = AnnouncedChannel::from_raw(raw(3, "C"), ChannelState::Announced);
        ch.tracker = None;
        ch.listeners = -1;
        let listing = ch.to_listing();
        assert!(listing.streaming.is_none());
        assert!(listing.tip.is_none());
        assert_eq!(listing.current_participants, UNKNOWN_COUNT);
        assert_eq!(listing.title, "C");
    }
}

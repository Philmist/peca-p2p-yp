//! kind 30311(チャンネル掲載イベント)のスキーマ・署名・受信検証(T015)
//!
//! contracts/nostr-events.md の「タグ写像」「発行規則」「受信検証 1〜7」を実装する。
//! nostr(NIP-01/13/40/53)の**データ構造のみ**を援用し、リレー通信は行わない(ADR-0002 §3)。
//!
//! 受信検証は 1(サイズ)→ 2(署名)→ 3(kind/タグ形式)→ 4(時刻)→ 5(内容)→
//! 6(PoW)の順に行い、違反は [`VerifyReject`] として返す。ログ書き込みは呼び出し側の責務。

use std::net::SocketAddr;
use std::str::FromStr;

use nostr::{Event, EventBuilder, JsonUtil, Keys, Kind, Tag, Timestamp};

use crate::security::{self, SecurityCategory};

/// kind 30311(NIP-53 Live Streaming Event)。
pub const CHANNEL_KIND: u16 = 30311;
/// 直列化イベント全体のサイズ上限(受信検証 1)。
pub const MAX_EVENT_BYTES: usize = 16 * 1024;
/// タグ数の上限(受信検証 3)。
pub const MAX_TAGS: usize = 64;
/// 各タグ要素のバイト長上限(受信検証 3)。
pub const MAX_TAG_ELEMENT_BYTES: usize = 1024;
/// bitrate(kbps)の上限(受信検証 5、data-model §AnnouncedChannel)。
pub const MAX_BITRATE_KBPS: u64 = 100_000;
/// `expiration` = created_at + 本値(NIP-40。data-model §Settings 単一出典)。
pub const EXPIRATION_OFFSET_SECS: u64 = 600;
/// 未来方向時刻ずれ許容の既定値(data-model §Settings `max_clock_skew_sec`)。
pub const DEFAULT_MAX_CLOCK_SKEW_SEC: u64 = 300;

/// `current_participants` / `relays` の「不明」を表す番兵値。
///
/// 発行時はタグを省略し、受信時はタグ省略を本値として復元する(往復規則 — contracts/http-yp.md)。
pub const UNKNOWN_COUNT: i64 = -1;

// ---------------------------------------------------------------------------
// チャンネル掲載情報(タグ写像)
// ---------------------------------------------------------------------------

/// 配信状態(`status` タグ)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelStatus {
    /// 配信中。
    Live,
    /// 配信終了。
    Ended,
}

impl ChannelStatus {
    fn as_str(self) -> &'static str {
        match self {
            ChannelStatus::Live => "live",
            ChannelStatus::Ended => "ended",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "live" => Some(ChannelStatus::Live),
            "ended" => Some(ChannelStatus::Ended),
            _ => None,
        }
    }
}

/// 曲情報(`["peca","track", title, artist, album, url]`)。
///
/// `url` は v1 では常に空文字列(対応する PCP atom を受信しない — contracts/pcp-announce.md)。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Track {
    /// 曲名(`titl`)。
    pub title: String,
    /// アーティスト(`crea`)。
    pub artist: String,
    /// アルバム(`albm`)。
    pub album: String,
    /// URL。v1 では常に空文字列。
    pub url: String,
}

/// kind 30311 のタグ写像を表す構造体(PeerCast チャンネル情報)。
///
/// `current_participants` / `relays` は [`UNKNOWN_COUNT`](-1)で「不明」を表す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelListing {
    /// `d` タグ = チャンネル GUID(hex 32 桁小文字)。
    pub channel_id: String,
    /// `title` タグ = チャンネル名。
    pub title: String,
    /// `summary` タグ = 説明(任意)。
    pub summary: Option<String>,
    /// `t` タグ = ジャンル(任意・小文字化想定)。
    pub genre: Option<String>,
    /// `status` タグ。
    pub status: ChannelStatus,
    /// `starts` タグ = 配信開始 unix 秒。
    pub starts: u64,
    /// `current_participants` タグ = 直接視聴者数。不明は [`UNKNOWN_COUNT`]。
    pub current_participants: i64,
    /// `streaming` タグ = `pcp://<tracker_ip>:<port>/<channel_id>`(任意)。
    pub streaming: Option<String>,
    /// `["peca","bitrate",..]` = ビットレート kbps(任意)。
    pub bitrate_kbps: Option<u64>,
    /// `["peca","type",..]` = コンテンツ種別(任意)。
    pub content_type: Option<String>,
    /// `["peca","tip",..]` = トラッカー ip:port(firewalled 時は省略)。
    pub tip: Option<String>,
    /// `["peca","contact",..]` = コンタクト URL(任意)。
    pub contact: Option<String>,
    /// `["peca","relays",..]` = リレー数。不明は [`UNKNOWN_COUNT`]。
    pub relays: i64,
    /// `["peca","track",..]` = 曲情報(任意)。
    pub track: Option<Track>,
}

impl ChannelListing {
    /// 発行用のタグ列を構築する(発行規則 — contracts/nostr-events.md)。
    ///
    /// `current_participants` / `relays` が負値(不明)ならタグを省略する。
    fn to_tags(&self, created_at: u64) -> Result<Vec<Tag>, EventBuildError> {
        let mut raw: Vec<Vec<String>> = Vec::new();
        raw.push(vec!["d".into(), self.channel_id.clone()]);
        raw.push(vec!["title".into(), self.title.clone()]);
        if let Some(summary) = &self.summary {
            raw.push(vec!["summary".into(), summary.clone()]);
        }
        if let Some(genre) = &self.genre {
            raw.push(vec!["t".into(), genre.clone()]);
        }
        raw.push(vec!["status".into(), self.status.as_str().into()]);
        raw.push(vec!["starts".into(), self.starts.to_string()]);
        if self.current_participants >= 0 {
            raw.push(vec![
                "current_participants".into(),
                self.current_participants.to_string(),
            ]);
        }
        if let Some(streaming) = &self.streaming {
            raw.push(vec!["streaming".into(), streaming.clone()]);
        }
        raw.push(vec![
            "expiration".into(),
            (created_at + EXPIRATION_OFFSET_SECS).to_string(),
        ]);
        // peca 拡張タグ
        if let Some(bitrate) = self.bitrate_kbps {
            raw.push(vec!["peca".into(), "bitrate".into(), bitrate.to_string()]);
        }
        if let Some(content_type) = &self.content_type {
            raw.push(vec!["peca".into(), "type".into(), content_type.clone()]);
        }
        if let Some(tip) = &self.tip {
            raw.push(vec!["peca".into(), "tip".into(), tip.clone()]);
        }
        if let Some(contact) = &self.contact {
            raw.push(vec!["peca".into(), "contact".into(), contact.clone()]);
        }
        if self.relays >= 0 {
            raw.push(vec!["peca".into(), "relays".into(), self.relays.to_string()]);
        }
        if let Some(track) = &self.track {
            raw.push(vec![
                "peca".into(),
                "track".into(),
                track.title.clone(),
                track.artist.clone(),
                track.album.clone(),
                track.url.clone(),
            ]);
        }

        raw.into_iter()
            .map(|elems| Tag::parse(elems).map_err(|e| EventBuildError::Tag(e.to_string())))
            .collect()
    }

    /// 掲載情報に署名して kind 30311 イベントを生成する(発行側)。
    ///
    /// `created_at` は掲載時刻(unix 秒)。`pow_bits > 0` のとき NIP-13 の PoW を付与する。
    /// content は空文字列(全情報はタグで表現)。
    pub fn sign(
        &self,
        keys: &Keys,
        created_at: u64,
        pow_bits: u8,
    ) -> Result<Event, EventBuildError> {
        let tags = self.to_tags(created_at)?;
        let mut builder = EventBuilder::new(Kind::Custom(CHANNEL_KIND), "")
            .tags(tags)
            .custom_created_at(Timestamp::from(created_at));
        if pow_bits > 0 {
            builder = builder.pow(pow_bits);
        }
        builder
            .sign_with_keys(keys)
            .map_err(|e| EventBuildError::Sign(e.to_string()))
    }

    /// 検証済みイベントからタグを復元する(受信検証 5 の内容チェックを兼ねる)。
    ///
    /// 形式チェック(受信検証 3)は [`validate_format`] が別途行う前提。ここでは
    /// 数値範囲・`tip` の ip:port 形式・制御文字を検査し、違反は [`VerifyReject::InvalidFormat`]。
    /// タグ省略の `current_participants` / `relays` は [`UNKNOWN_COUNT`] として復元する。
    pub fn from_event(event: &Event) -> Result<Self, VerifyReject> {
        let channel_id = tag_value(event, "d")
            .ok_or(VerifyReject::InvalidFormat("missing d tag"))?
            .to_string();
        let title_raw =
            tag_value(event, "title").ok_or(VerifyReject::InvalidFormat("missing title tag"))?;
        if security::contains_control_chars(title_raw) {
            return Err(VerifyReject::InvalidFormat("control char in title"));
        }
        let title = title_raw.to_string();
        let status = ChannelStatus::parse(
            tag_value(event, "status").ok_or(VerifyReject::InvalidFormat("missing status tag"))?,
        )
        .ok_or(VerifyReject::InvalidFormat("invalid status value"))?;
        let starts = parse_u64(
            tag_value(event, "starts").ok_or(VerifyReject::InvalidFormat("missing starts tag"))?,
        )
        .ok_or(VerifyReject::InvalidFormat("invalid starts value"))?;

        let summary = clean_optional(tag_value(event, "summary"))?;
        let genre = clean_optional(tag_value(event, "t"))?;
        let streaming = clean_optional(tag_value(event, "streaming"))?;

        let current_participants = parse_count(tag_value(event, "current_participants"))?;

        let bitrate_kbps = match peca_value(event, "bitrate") {
            Some(v) => {
                let n = parse_u64(v).ok_or(VerifyReject::InvalidFormat("invalid bitrate"))?;
                if n > MAX_BITRATE_KBPS {
                    return Err(VerifyReject::InvalidFormat("bitrate out of range"));
                }
                Some(n)
            }
            None => None,
        };
        let content_type = clean_optional(peca_value(event, "type"))?;
        let contact = clean_optional(peca_value(event, "contact"))?;
        let tip = match peca_value(event, "tip") {
            Some(v) => {
                if security::contains_control_chars(v) || SocketAddr::from_str(v).is_err() {
                    return Err(VerifyReject::InvalidFormat("invalid tip ip:port"));
                }
                Some(v.to_string())
            }
            None => None,
        };
        let relays = parse_count(peca_value(event, "relays"))?;
        let track = parse_track(event)?;

        Ok(Self {
            channel_id,
            title,
            summary,
            genre,
            status,
            starts,
            current_participants,
            streaming,
            bitrate_kbps,
            content_type,
            tip,
            contact,
            relays,
            track,
        })
    }
}

/// 曲情報タグ(`peca track`)を復元する。要素不足は空文字列で補う。
fn parse_track(event: &Event) -> Result<Option<Track>, VerifyReject> {
    let Some(slice) = peca_slice(event, "track") else {
        return Ok(None);
    };
    // slice = ["peca","track", title, artist, album, url]
    let get = |i: usize| slice.get(i).map(String::as_str).unwrap_or("");
    let title = get(2);
    let artist = get(3);
    let album = get(4);
    let url = get(5);
    for field in [title, artist, album, url] {
        if security::contains_control_chars(field) {
            return Err(VerifyReject::InvalidFormat("control char in track"));
        }
    }
    Ok(Some(Track {
        title: title.to_string(),
        artist: artist.to_string(),
        album: album.to_string(),
        url: url.to_string(),
    }))
}

// ---------------------------------------------------------------------------
// 受信検証パイプライン(1〜6)
// ---------------------------------------------------------------------------

/// 受信検証のパラメータ(data-model §Settings が単一出典)。
#[derive(Debug, Clone, Copy)]
pub struct VerifyConfig {
    /// 未来方向の時刻ずれ許容(秒)。
    pub max_clock_skew_sec: u64,
    /// 最小 NIP-13 難易度(0=無効)。
    pub min_pow_bits: u8,
}

impl Default for VerifyConfig {
    fn default() -> Self {
        Self {
            max_clock_skew_sec: DEFAULT_MAX_CLOCK_SKEW_SEC,
            min_pow_bits: 0,
        }
    }
}

/// 受信検証を通過したイベントとその復元済み掲載情報。
#[derive(Debug, Clone, PartialEq)]
pub struct VerifiedListing {
    /// 検証済みイベント(格納・再伝搬に使う)。
    pub event: Event,
    /// 復元した掲載情報(一覧ビュー構築に使う)。
    pub listing: ChannelListing,
}

/// 受信検証の拒否理由。[`SecurityCategory`] に対応付けできる。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyReject {
    /// 受信検証 1: サイズ超過。
    Oversize,
    /// 受信検証 2: id・署名検証失敗。
    InvalidSig,
    /// 受信検証 3・5: kind/タグ形式・内容範囲違反。
    InvalidFormat(&'static str),
    /// 受信検証 4: 未来方向の時刻ずれ許容超過。
    TimeSkew,
    /// 受信検証 6: PoW 難易度不足。
    PowInsufficient,
}

impl VerifyReject {
    /// 対応するセキュリティイベントのカテゴリ。
    pub fn category(&self) -> SecurityCategory {
        match self {
            VerifyReject::Oversize => SecurityCategory::EventOversize,
            VerifyReject::InvalidSig => SecurityCategory::EventInvalidSig,
            VerifyReject::InvalidFormat(_) => SecurityCategory::EventInvalidFormat,
            VerifyReject::TimeSkew => SecurityCategory::EventTimeSkew,
            VerifyReject::PowInsufficient => SecurityCategory::EventPowInsufficient,
        }
    }

    /// ログ用の短い説明(内部情報を含めてはならない — Principle II)。
    pub fn detail(&self) -> &'static str {
        match self {
            VerifyReject::Oversize => "event exceeds size limit",
            VerifyReject::InvalidSig => "signature or id verification failed",
            VerifyReject::InvalidFormat(d) => d,
            VerifyReject::TimeSkew => "created_at too far in the future",
            VerifyReject::PowInsufficient => "proof-of-work below minimum",
        }
    }
}

/// kind/タグ形式の検査(受信検証 3)。
///
/// kind=30311、`d` は hex 32 桁小文字、`status` ∈ {live, ended}、タグ数 ≤ 64、
/// 各タグ要素長 ≤ 1024 バイト。
pub fn validate_format(event: &Event) -> Result<(), VerifyReject> {
    if event.kind.as_u16() != CHANNEL_KIND {
        return Err(VerifyReject::InvalidFormat("unexpected kind"));
    }
    if event.tags.len() > MAX_TAGS {
        return Err(VerifyReject::InvalidFormat("too many tags"));
    }
    for tag in event.tags.iter() {
        for element in tag.as_slice() {
            if security::exceeds_bytes(element, MAX_TAG_ELEMENT_BYTES) {
                return Err(VerifyReject::InvalidFormat("tag element too long"));
            }
        }
    }
    let d = tag_value(event, "d").ok_or(VerifyReject::InvalidFormat("missing d tag"))?;
    if !security::is_lower_hex(d, 32) {
        return Err(VerifyReject::InvalidFormat("d tag not hex32"));
    }
    let status = tag_value(event, "status").ok_or(VerifyReject::InvalidFormat("missing status"))?;
    if ChannelStatus::parse(status).is_none() {
        return Err(VerifyReject::InvalidFormat("invalid status value"));
    }
    Ok(())
}

/// gossip 経由で受信した直列化イベントを検証する(受信検証 1〜6 を順に実行)。
///
/// `raw_json` は受信した直列化イベント全体、`now` は受信ノードのローカル時計(unix 秒)。
/// 検証失敗は [`VerifyReject`] を返す(格納も再伝搬もしない — 呼び出し側の責務)。
pub fn verify_incoming(
    raw_json: &str,
    config: &VerifyConfig,
    now: u64,
) -> Result<VerifiedListing, VerifyReject> {
    // 1. サイズ(直列化イベント全体 ≤ 16KB)
    if raw_json.len() > MAX_EVENT_BYTES {
        return Err(VerifyReject::Oversize);
    }
    // JSON デシリアライズ(署名検証の前提。失敗は形式違反として扱う)
    let event = Event::from_json(raw_json)
        .map_err(|_| VerifyReject::InvalidFormat("malformed event json"))?;
    // 2. 署名(id・sig 検証)
    if event.verify().is_err() {
        return Err(VerifyReject::InvalidSig);
    }
    // 3. kind/タグ形式
    validate_format(&event)?;
    // 4. 時刻(未来方向のみ拒否。過去方向は鮮度窓で自然減衰)
    if event.created_at.as_secs() > now.saturating_add(config.max_clock_skew_sec) {
        return Err(VerifyReject::TimeSkew);
    }
    // 5. 内容(数値範囲・tip 形式・制御文字)— 復元を兼ねる
    let listing = ChannelListing::from_event(&event)?;
    // 6. PoW(任意)
    if config.min_pow_bits > 0 && !event.check_pow(config.min_pow_bits) {
        return Err(VerifyReject::PowInsufficient);
    }
    Ok(VerifiedListing { event, listing })
}

// ---------------------------------------------------------------------------
// EventStore 用の要約(T016 が使用)
// ---------------------------------------------------------------------------

/// EventStore が置換・除去判定に使うイベント要約(タグから抽出)。
///
/// 検証済みイベントから作る前提。`d` タグを欠く場合は置換キーを作れないため `None`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventSummary {
    /// kind。
    pub kind: u16,
    /// 発行者 pubkey(hex 64)。
    pub pubkey: String,
    /// `d` タグ(チャンネル GUID)。
    pub channel_id: String,
    /// event id(hex 64)。
    pub event_id: String,
    /// created_at(unix 秒)。
    pub created_at: u64,
    /// `expiration`(NIP-40。欠落時は created_at + 600)。
    pub expiration: u64,
    /// `status=ended` か。
    pub ended: bool,
}

impl EventSummary {
    /// イベントから要約を抽出する。`d` タグを欠く場合は `None`。
    pub fn from_event(event: &Event) -> Option<Self> {
        let channel_id = tag_value(event, "d")?.to_string();
        let created_at = event.created_at.as_secs();
        let expiration = tag_value(event, "expiration")
            .and_then(parse_u64)
            .unwrap_or(created_at + EXPIRATION_OFFSET_SECS);
        let ended = tag_value(event, "status") == Some("ended");
        Some(Self {
            kind: event.kind.as_u16(),
            pubkey: event.pubkey.to_hex(),
            channel_id,
            event_id: event.id.to_hex(),
            created_at,
            expiration,
            ended,
        })
    }

    /// 置換キー `(kind, pubkey, d)`。
    pub fn key(&self) -> (u16, String, String) {
        (self.kind, self.pubkey.clone(), self.channel_id.clone())
    }
}

// ---------------------------------------------------------------------------
// エラー型
// ---------------------------------------------------------------------------

/// 発行(署名)時のエラー。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventBuildError {
    /// タグ構築エラー(nostr クレート)。
    Tag(String),
    /// 署名・直列化エラー(nostr クレート)。
    Sign(String),
}

impl std::fmt::Display for EventBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EventBuildError::Tag(e) => write!(f, "tag build error: {e}"),
            EventBuildError::Sign(e) => write!(f, "sign error: {e}"),
        }
    }
}

impl std::error::Error for EventBuildError {}

// ---------------------------------------------------------------------------
// タグ読み出しヘルパ
// ---------------------------------------------------------------------------

/// 単純タグ(`[name, value]`)の値を返す。
fn tag_value<'a>(event: &'a Event, name: &str) -> Option<&'a str> {
    event.tags.iter().find_map(|tag| {
        let slice = tag.as_slice();
        if slice.first().map(String::as_str) == Some(name) {
            slice.get(1).map(String::as_str)
        } else {
            None
        }
    })
}

/// peca 拡張タグ(`["peca", sub, value, ..]`)のスライス全体を返す。
fn peca_slice<'a>(event: &'a Event, sub: &str) -> Option<&'a [String]> {
    event.tags.iter().map(Tag::as_slice).find(|slice| {
        slice.first().map(String::as_str) == Some("peca")
            && slice.get(1).map(String::as_str) == Some(sub)
    })
}

/// peca 拡張タグの第 1 値(要素 index 2)を返す。
fn peca_value<'a>(event: &'a Event, sub: &str) -> Option<&'a str> {
    peca_slice(event, sub).and_then(|slice| slice.get(2).map(String::as_str))
}

/// カウント系タグ(current_participants / relays)を復元する。
///
/// タグ省略は [`UNKNOWN_COUNT`]。存在時は非負整数でなければならない
/// (負値は省略で表すため、明示負値は違反)。
fn parse_count(value: Option<&str>) -> Result<i64, VerifyReject> {
    match value {
        None => Ok(UNKNOWN_COUNT),
        Some(v) => parse_u64(v)
            .map(|n| n as i64)
            .ok_or(VerifyReject::InvalidFormat("invalid count value")),
    }
}

/// 非負十進整数としてパースする(符号・空白を許容しない)。
fn parse_u64(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<u64>().ok()
}

/// 任意文字列タグの制御文字検査。制御文字を含めば違反。
fn clean_optional(value: Option<&str>) -> Result<Option<String>, VerifyReject> {
    match value {
        None => Ok(None),
        Some(v) => {
            if security::contains_control_chars(v) {
                Err(VerifyReject::InvalidFormat("control char in tag"))
            } else {
                Ok(Some(v.to_string()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_listing() -> ChannelListing {
        ChannelListing {
            channel_id: "0123456789abcdef0123456789abcdef".into(),
            title: "テスト配信".into(),
            summary: Some("説明".into()),
            genre: Some("game".into()),
            status: ChannelStatus::Live,
            starts: 1_700_000_000,
            current_participants: 5,
            streaming: Some("pcp://198.51.100.1:7144/0123456789abcdef0123456789abcdef".into()),
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

    fn cfg() -> VerifyConfig {
        VerifyConfig::default()
    }

    #[test]
    fn sign_and_verify_roundtrip() {
        let keys = Keys::generate();
        let listing = sample_listing();
        let now = listing.starts;
        let event = listing.sign(&keys, now, 0).unwrap();
        let raw = event.as_json();

        let verified = verify_incoming(&raw, &cfg(), now).unwrap();
        assert_eq!(verified.listing, listing);
        assert_eq!(verified.event.pubkey, keys.public_key());
    }

    #[test]
    fn unknown_counts_roundtrip_via_tag_omission() {
        let keys = Keys::generate();
        let mut listing = sample_listing();
        listing.current_participants = UNKNOWN_COUNT;
        listing.relays = UNKNOWN_COUNT;
        let now = listing.starts;
        let event = listing.sign(&keys, now, 0).unwrap();

        // タグが省略されていること
        assert!(tag_value(&event, "current_participants").is_none());
        assert!(peca_value(&event, "relays").is_none());

        let restored = ChannelListing::from_event(&event).unwrap();
        assert_eq!(restored.current_participants, UNKNOWN_COUNT);
        assert_eq!(restored.relays, UNKNOWN_COUNT);
    }

    #[test]
    fn expiration_is_created_at_plus_600() {
        let keys = Keys::generate();
        let listing = sample_listing();
        let event = listing.sign(&keys, 1_700_000_000, 0).unwrap();
        let summary = EventSummary::from_event(&event).unwrap();
        assert_eq!(summary.expiration, 1_700_000_000 + 600);
        assert_eq!(summary.created_at, 1_700_000_000);
        assert!(!summary.ended);
        assert_eq!(summary.kind, CHANNEL_KIND);
    }

    #[test]
    fn ended_status_is_detected() {
        let keys = Keys::generate();
        let mut listing = sample_listing();
        listing.status = ChannelStatus::Ended;
        let event = listing.sign(&keys, 1_700_000_000, 0).unwrap();
        let summary = EventSummary::from_event(&event).unwrap();
        assert!(summary.ended);
    }

    #[test]
    fn reject_oversize_before_parse() {
        let raw = "x".repeat(MAX_EVENT_BYTES + 1);
        assert_eq!(verify_incoming(&raw, &cfg(), 0), Err(VerifyReject::Oversize));
    }

    #[test]
    fn reject_malformed_json() {
        let err = verify_incoming("not json", &cfg(), 0).unwrap_err();
        assert_eq!(err.category(), SecurityCategory::EventInvalidFormat);
    }

    #[test]
    fn reject_tampered_signature() {
        let keys = Keys::generate();
        let event = sample_listing().sign(&keys, 1_700_000_000, 0).unwrap();
        // content を改竄すると id 再計算が合わず署名検証に失敗する
        let raw = event.as_json().replace("\"content\":\"\"", "\"content\":\"x\"");
        assert_eq!(
            verify_incoming(&raw, &cfg(), 1_700_000_000),
            Err(VerifyReject::InvalidSig)
        );
    }

    #[test]
    fn reject_wrong_kind() {
        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::Custom(1), "")
            .tags([
                Tag::parse(["d", "0123456789abcdef0123456789abcdef"]).unwrap(),
                Tag::parse(["status", "live"]).unwrap(),
            ])
            .custom_created_at(Timestamp::from(1_700_000_000))
            .sign_with_keys(&keys)
            .unwrap();
        let raw = event.as_json();
        assert_eq!(
            verify_incoming(&raw, &cfg(), 1_700_000_000),
            Err(VerifyReject::InvalidFormat("unexpected kind"))
        );
    }

    #[test]
    fn reject_bad_d_tag() {
        let keys = Keys::generate();
        let mut listing = sample_listing();
        listing.channel_id = "NOTHEX".into();
        let event = listing.sign(&keys, 1_700_000_000, 0).unwrap();
        let err = verify_incoming(&event.as_json(), &cfg(), 1_700_000_000).unwrap_err();
        assert_eq!(err.category(), SecurityCategory::EventInvalidFormat);
    }

    #[test]
    fn reject_future_time_skew() {
        let keys = Keys::generate();
        let created = 1_700_000_000;
        let event = sample_listing().sign(&keys, created, 0).unwrap();
        // now が created より (skew + 1) 秒手前 = イベントは許容超過の未来
        let now = created - (DEFAULT_MAX_CLOCK_SKEW_SEC + 1);
        assert_eq!(
            verify_incoming(&event.as_json(), &cfg(), now),
            Err(VerifyReject::TimeSkew)
        );
    }

    #[test]
    fn accept_within_clock_skew() {
        let keys = Keys::generate();
        let created = 1_700_000_000;
        let event = sample_listing().sign(&keys, created, 0).unwrap();
        // ちょうど許容境界(未来方向 = skew)は受理
        let now = created - DEFAULT_MAX_CLOCK_SKEW_SEC;
        assert!(verify_incoming(&event.as_json(), &cfg(), now).is_ok());
    }

    #[test]
    fn reject_past_is_not_time_skew() {
        // 過去方向のずれは検証 4 では拒否しない(鮮度窓の責務)
        let keys = Keys::generate();
        let created = 1_700_000_000;
        let event = sample_listing().sign(&keys, created, 0).unwrap();
        let now = created + 10_000; // 大きく過去のイベント
        assert!(verify_incoming(&event.as_json(), &cfg(), now).is_ok());
    }

    #[test]
    fn reject_bitrate_out_of_range() {
        let keys = Keys::generate();
        let mut listing = sample_listing();
        listing.bitrate_kbps = Some(MAX_BITRATE_KBPS + 1);
        let event = listing.sign(&keys, 1_700_000_000, 0).unwrap();
        let err = verify_incoming(&event.as_json(), &cfg(), 1_700_000_000).unwrap_err();
        assert_eq!(err.category(), SecurityCategory::EventInvalidFormat);
    }

    #[test]
    fn reject_bad_tip_format() {
        let keys = Keys::generate();
        let mut listing = sample_listing();
        listing.tip = Some("not-an-addr".into());
        let event = listing.sign(&keys, 1_700_000_000, 0).unwrap();
        let err = verify_incoming(&event.as_json(), &cfg(), 1_700_000_000).unwrap_err();
        assert_eq!(err.category(), SecurityCategory::EventInvalidFormat);
    }

    #[test]
    fn pow_pass_and_insufficient() {
        let keys = Keys::generate();
        let listing = sample_listing();
        let created = 1_700_000_000;
        // 8 ビット PoW を付与 → min 8 は通過
        let event = listing.sign(&keys, created, 8).unwrap();
        let raw = event.as_json();
        let pass = VerifyConfig {
            max_clock_skew_sec: DEFAULT_MAX_CLOCK_SKEW_SEC,
            min_pow_bits: 8,
        };
        assert!(verify_incoming(&raw, &pass, created).is_ok());

        // 事実上到達不能な難易度は不足として拒否(決定的)
        let strict = VerifyConfig {
            max_clock_skew_sec: DEFAULT_MAX_CLOCK_SKEW_SEC,
            min_pow_bits: 240,
        };
        assert_eq!(
            verify_incoming(&raw, &strict, created),
            Err(VerifyReject::PowInsufficient)
        );
    }

    #[test]
    fn reject_too_many_tags() {
        // 65 個のタグ(> MAX_TAGS)を持つ live イベントを構築
        let keys = Keys::generate();
        let mut tags = vec![
            Tag::parse(["d", "0123456789abcdef0123456789abcdef"]).unwrap(),
            Tag::parse(["title", "t"]).unwrap(),
            Tag::parse(["status", "live"]).unwrap(),
            Tag::parse(["starts", "1"]).unwrap(),
        ];
        while tags.len() <= MAX_TAGS {
            tags.push(Tag::parse(["t", &format!("g{}", tags.len())]).unwrap());
        }
        let event = EventBuilder::new(Kind::Custom(CHANNEL_KIND), "")
            .tags(tags)
            .custom_created_at(Timestamp::from(1_700_000_000))
            .sign_with_keys(&keys)
            .unwrap();
        assert_eq!(
            verify_incoming(&event.as_json(), &cfg(), 1_700_000_000),
            Err(VerifyReject::InvalidFormat("too many tags"))
        );
    }

    #[test]
    fn reject_control_chars_in_title() {
        let keys = Keys::generate();
        let mut listing = sample_listing();
        listing.title = "bad\u{7}title".into();
        let event = listing.sign(&keys, 1_700_000_000, 0).unwrap();
        let err = verify_incoming(&event.as_json(), &cfg(), 1_700_000_000).unwrap_err();
        assert_eq!(err.category(), SecurityCategory::EventInvalidFormat);
    }

    #[test]
    fn reject_control_chars_in_summary() {
        let keys = Keys::generate();
        let mut listing = sample_listing();
        listing.summary = Some("bad\u{7}summary".into());
        let event = listing.sign(&keys, 1_700_000_000, 0).unwrap();
        let err = verify_incoming(&event.as_json(), &cfg(), 1_700_000_000).unwrap_err();
        assert_eq!(err.category(), SecurityCategory::EventInvalidFormat);
    }
}

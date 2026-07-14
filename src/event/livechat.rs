//! livechat イベントスキーマ(kind 1311 / 21311 / 31311)(T010)
//!
//! nostr 援用境界内(ADR-0002 §3)— 本モジュールが `nostr` クレートのイベント構造・
//! 署名・検証に直接依存する。伝送は行わない(スレ配送は
//! [`crate::livechat`]、announce は gossip が担う — contracts/thread-events.md)。
//!
//! 実装するのはイベント封筒の**スキーマ・直列化・フィールド検証**のみ:
//!
//! - **kind 31311**(スレ announce・addressable): `d="livechat"` 固定・`a`・`title`・
//!   `gen`・`key`・`res_count`・`tip`・`expiration`。置換キー `(31311, pubkey, "livechat")`。
//! - **kind 1311**(レス・regular): 本文(≤ 2048 文字・≤ 32 行・制御文字除去)+ peca タグ
//!   (thread / name / mail)。名前欄は `#` 以降を除去(FR-024)。
//! - **kind 21311**(順序確定情報・ephemeral): peca タグ thread / seq / order。
//!   イベント内の res_no は欠番なく連続(不変条件 T3)。
//!
//! **前方互換(MUST)**: 未知タグ・未知 peca サブタグは無視する(001 と同一規則)。
//!
//! セキュリティイベント種別(`livechat_*`)への写像・スレ主一致(FR-003/FR-011)・
//! レート・BAN・PoW しきい値の判定は上位(ingest / host / session — T020/T021/T030 ほか)の
//! 責務であり、本モジュールは形式検証に限る。

use std::net::SocketAddr;
use std::str::FromStr;

use nostr::{Event, EventBuilder, Keys, Kind, Tag, Timestamp};

use crate::security::{self, is_lower_hex};

/// kind 31311(スレ announce。peca 固有 addressable)。
pub const ANNOUNCE_KIND: u16 = 31311;
/// kind 1311(レス。NIP-53 Live Chat Message の封筒援用)。
pub const RES_KIND: u16 = 1311;
/// kind 21311(順序確定情報。peca 固有 ephemeral)。
pub const ORDER_KIND: u16 = 21311;

/// 参照チャンネル(30311)の kind 接頭辞。
const CHANNEL_KIND: u16 = 30311;
/// announce の `d` タグ固定値(置換キーの一部 — 板単位で 1 本)。
pub const ANNOUNCE_D: &str = "livechat";

/// `expiration` = created_at + 本値(30311 と同一規則 — FR-002 / thread-events.md)。
pub const EXPIRATION_OFFSET_SECS: u64 = 600;

/// スレタイトルの最大文字数(data-model §BoardSettings / thread-events.md)。
pub const TITLE_MAX_CHARS: usize = 128;
/// レス本文の最大文字数(SETTING.TXT の提示単位と一致 — contracts/compat-api.md)。
pub const BODY_MAX_CHARS: usize = 2048;
/// レス本文の最大行数。
pub const BODY_MAX_LINES: usize = 32;
/// 名前欄・メール欄の最大文字数(data-model §Res)。
pub const NAME_MAX_CHARS: usize = 64;

// ---------------------------------------------------------------------------
// 拒否理由(形式検証)
// ---------------------------------------------------------------------------

/// スキーマ検証の拒否理由。
///
/// `livechat_*` セキュリティカテゴリ・スレ主一致・レート等への写像は呼び出し側が行う
/// (本モジュールは形式のみ — thread-events.md §受信検証)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LivechatReject {
    /// 署名・id 検証失敗。
    InvalidSig,
    /// kind/タグ形式・本文制約・連続性の違反。
    InvalidFormat(&'static str),
}

impl LivechatReject {
    /// ログ用の短い説明(内部情報を含めてはならない — Principle II)。
    pub fn detail(&self) -> &'static str {
        match self {
            LivechatReject::InvalidSig => "signature or id verification failed",
            LivechatReject::InvalidFormat(d) => d,
        }
    }
}

/// 発行(署名・構築)時のエラー。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LivechatBuildError {
    /// フィールド制約違反(長さ・行数・形式)。
    Invalid(&'static str),
    /// タグ構築・署名エラー(nostr クレート)。
    Nostr(String),
}

impl std::fmt::Display for LivechatBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LivechatBuildError::Invalid(m) => write!(f, "invalid livechat field: {m}"),
            LivechatBuildError::Nostr(e) => write!(f, "nostr build error: {e}"),
        }
    }
}

impl std::error::Error for LivechatBuildError {}

// ---------------------------------------------------------------------------
// kind 31311 — スレ announce
// ---------------------------------------------------------------------------

/// スレ announce(kind 31311)の写像。`content` は空文字列。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadAnnounce {
    /// `a` タグ = 対象チャンネル `30311:<pubkey>:<guid>`(署名者一致は呼び出し側 — FR-003)。
    pub channel: String,
    /// `title` タグ = スレタイトル(≤ 128 文字)。
    pub title: String,
    /// `gen` タグ = スレ世代(u32)。
    pub generation: u32,
    /// `key` タグ = スレ作成 unix 秒(互換 API の dat キー)。
    pub key: u64,
    /// `res_count` タグ = 現在の確定レス数(一覧表示用・未検証の参考値。MAY)。
    pub res_count: Option<u64>,
    /// `tip` タグ = ホスト接続先 `ip:port`(受信のみでは接続しない — FR-004)。
    pub tip: String,
}

impl ThreadAnnounce {
    /// タグ列を構築する(`expiration` は created_at + 600)。
    fn to_tags(&self, created_at: u64) -> Result<Vec<Tag>, LivechatBuildError> {
        if self.title.chars().count() > TITLE_MAX_CHARS {
            return Err(LivechatBuildError::Invalid("title too long"));
        }
        if parse_channel_ref(&self.channel).is_none() {
            return Err(LivechatBuildError::Invalid("invalid channel ref"));
        }
        if SocketAddr::from_str(&self.tip).is_err() {
            return Err(LivechatBuildError::Invalid("invalid tip ip:port"));
        }
        let mut raw: Vec<Vec<String>> = vec![
            vec!["d".into(), ANNOUNCE_D.into()],
            vec!["a".into(), self.channel.clone()],
            vec!["title".into(), self.title.clone()],
            vec!["gen".into(), self.generation.to_string()],
            vec!["key".into(), self.key.to_string()],
        ];
        if let Some(count) = self.res_count {
            raw.push(vec!["res_count".into(), count.to_string()]);
        }
        raw.push(vec!["tip".into(), self.tip.clone()]);
        raw.push(vec![
            "expiration".into(),
            (created_at + EXPIRATION_OFFSET_SECS).to_string(),
        ]);
        build_tags(raw)
    }

    /// announce に署名して kind 31311 イベントを生成する(発行側 — スレ主ペルソナ鍵)。
    pub fn sign(
        &self,
        keys: &Keys,
        created_at: u64,
        pow_bits: u8,
    ) -> Result<Event, LivechatBuildError> {
        let tags = self.to_tags(created_at)?;
        sign_tags(keys, ANNOUNCE_KIND, "", tags, created_at, pow_bits)
    }

    /// 検証済みイベントから写像を復元する(形式検証を兼ねる)。
    ///
    /// kind・`d="livechat"`・`a` 形式・`title`(長さ・制御文字)・`gen`/`key`/`res_count` の
    /// 数値・`tip` の ip:port を検査する。署名検証は [`verify_res`] 等と分離(呼び出し側 or
    /// gossip パイプラインが実施)。
    pub fn from_event(event: &Event) -> Result<Self, LivechatReject> {
        if event.kind.as_u16() != ANNOUNCE_KIND {
            return Err(LivechatReject::InvalidFormat("unexpected kind"));
        }
        if tag_value(event, "d") != Some(ANNOUNCE_D) {
            return Err(LivechatReject::InvalidFormat("missing or invalid d tag"));
        }
        let channel = tag_value(event, "a")
            .ok_or(LivechatReject::InvalidFormat("missing a tag"))?
            .to_string();
        if parse_channel_ref(&channel).is_none() {
            return Err(LivechatReject::InvalidFormat("invalid channel ref"));
        }
        let title_raw =
            tag_value(event, "title").ok_or(LivechatReject::InvalidFormat("missing title tag"))?;
        if security::contains_control_chars(title_raw) {
            return Err(LivechatReject::InvalidFormat("control char in title"));
        }
        if title_raw.chars().count() > TITLE_MAX_CHARS {
            return Err(LivechatReject::InvalidFormat("title too long"));
        }
        let generation = parse_u32(tag_value(event, "gen"))
            .ok_or(LivechatReject::InvalidFormat("invalid gen"))?;
        let key = parse_u64_req(tag_value(event, "key"))
            .ok_or(LivechatReject::InvalidFormat("invalid key"))?;
        let res_count = match tag_value(event, "res_count") {
            None => None,
            Some(v) => {
                Some(parse_u64(v).ok_or(LivechatReject::InvalidFormat("invalid res_count"))?)
            }
        };
        let tip = tag_value(event, "tip")
            .ok_or(LivechatReject::InvalidFormat("missing tip tag"))?
            .to_string();
        if security::contains_control_chars(&tip) || SocketAddr::from_str(&tip).is_err() {
            return Err(LivechatReject::InvalidFormat("invalid tip ip:port"));
        }
        Ok(Self {
            channel,
            title: title_raw.to_string(),
            generation,
            key,
            res_count,
            tip,
        })
    }
}

// ---------------------------------------------------------------------------
// kind 1311 — レス
// ---------------------------------------------------------------------------

/// レス(kind 1311)の写像。`content` が本文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Res {
    /// `a` タグ = 対象チャンネル(31311 の `a` と同値)。
    pub channel: String,
    /// `["peca","thread","<board_id>","<gen>"]` のスレ主 pubkey(hex 64)。
    pub board_id: String,
    /// 同上の世代(u32)。
    pub generation: u32,
    /// `["peca","name",..]`。空・省略は名無し。`#` 以降は除去済み。
    pub name: Option<String>,
    /// `["peca","mail",..]`。表示互換のみ(FR-029)。
    pub mail: Option<String>,
    /// 本文(≤ 2048 文字・≤ 32 行・制御文字除去(改行を除く))。
    pub body: String,
}

impl Res {
    /// レスに署名して kind 1311 イベントを生成する(書き込みクライアント — 板鍵)。
    ///
    /// 送信前の正規化を適用する: 名前欄の `#` 以降除去(FR-024)、本文の制御文字除去
    /// (改行を除く)。正規化後に長さ・行数の上限を検査し、超過は
    /// [`LivechatBuildError::Invalid`]。
    pub fn sign(
        &self,
        keys: &Keys,
        created_at: u64,
        pow_bits: u8,
    ) -> Result<Event, LivechatBuildError> {
        if parse_channel_ref(&self.channel).is_none() {
            return Err(LivechatBuildError::Invalid("invalid channel ref"));
        }
        if !is_lower_hex(&self.board_id, 64) {
            return Err(LivechatBuildError::Invalid("invalid board_id"));
        }
        let body = strip_body_control(&self.body);
        if body.chars().count() > BODY_MAX_CHARS {
            return Err(LivechatBuildError::Invalid("body too long"));
        }
        if line_count(&body) > BODY_MAX_LINES {
            return Err(LivechatBuildError::Invalid("too many lines"));
        }
        let mut raw: Vec<Vec<String>> = vec![
            vec!["a".into(), self.channel.clone()],
            vec![
                "peca".into(),
                "thread".into(),
                self.board_id.clone(),
                self.generation.to_string(),
            ],
        ];
        if let Some(name) = &self.name {
            let name = strip_after_hash(name);
            if name.chars().count() > NAME_MAX_CHARS {
                return Err(LivechatBuildError::Invalid("name too long"));
            }
            if !name.is_empty() {
                raw.push(vec!["peca".into(), "name".into(), name]);
            }
        }
        if let Some(mail) = &self.mail {
            if mail.chars().count() > NAME_MAX_CHARS {
                return Err(LivechatBuildError::Invalid("mail too long"));
            }
            if !mail.is_empty() {
                raw.push(vec!["peca".into(), "mail".into(), mail.clone()]);
            }
        }
        let tags = build_tags(raw)?;
        sign_tags(keys, RES_KIND, &body, tags, created_at, pow_bits)
    }

    /// 検証済みイベントから写像を復元する(ホスト側受信検証の形式部 — 検証 3)。
    ///
    /// kind・`a` 形式・peca thread タグ・本文(長さ・行数)・name/mail(長さ・制御文字)を
    /// 検査する。復元した `name` は `#` 以降を除去(ホスト側二重防御 — FR-024)、`body` は
    /// 制御文字を除去(改行を除く)した表示用ビュー(署名済みイベント自体は不変)。
    pub fn from_event(event: &Event) -> Result<Self, LivechatReject> {
        if event.kind.as_u16() != RES_KIND {
            return Err(LivechatReject::InvalidFormat("unexpected kind"));
        }
        let channel = tag_value(event, "a")
            .ok_or(LivechatReject::InvalidFormat("missing a tag"))?
            .to_string();
        if parse_channel_ref(&channel).is_none() {
            return Err(LivechatReject::InvalidFormat("invalid channel ref"));
        }
        let (board_id, generation) = parse_thread_tag(event)?;

        let name = match peca_value(event, "name") {
            None => None,
            Some(v) => {
                if security::contains_control_chars(v) {
                    return Err(LivechatReject::InvalidFormat("control char in name"));
                }
                if v.chars().count() > NAME_MAX_CHARS {
                    return Err(LivechatReject::InvalidFormat("name too long"));
                }
                let stripped = strip_after_hash(v);
                if stripped.is_empty() {
                    None
                } else {
                    Some(stripped)
                }
            }
        };
        let mail = match peca_value(event, "mail") {
            None => None,
            Some(v) => {
                if security::contains_control_chars(v) {
                    return Err(LivechatReject::InvalidFormat("control char in mail"));
                }
                if v.chars().count() > NAME_MAX_CHARS {
                    return Err(LivechatReject::InvalidFormat("mail too long"));
                }
                Some(v.to_string())
            }
        };

        let body_raw = event.content.as_str();
        if body_raw.chars().count() > BODY_MAX_CHARS {
            return Err(LivechatReject::InvalidFormat("body too long"));
        }
        if line_count(body_raw) > BODY_MAX_LINES {
            return Err(LivechatReject::InvalidFormat("too many lines"));
        }
        let body = strip_body_control(body_raw);

        Ok(Self {
            channel,
            board_id,
            generation,
            name,
            mail,
            body,
        })
    }
}

// ---------------------------------------------------------------------------
// kind 21311 — 順序確定情報
// ---------------------------------------------------------------------------

/// 採番 1 件(res_no ↔ event_id の確定)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderEntry {
    pub res_no: u16,
    /// 確定対象レスの event id(hex 64)。
    pub event_id: String,
}

/// 順序確定情報(kind 21311)。`content` は空文字列。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderInfo {
    /// スレ主 pubkey(hex 64)。
    pub board_id: String,
    /// 世代(u32)。
    pub generation: u32,
    /// 確定情報の連番(欠落検出用 — 不変条件 O2)。
    pub seq: u32,
    /// 今回確定した採番。res_no は欠番なく連続(不変条件 T3)。
    pub entries: Vec<OrderEntry>,
}

impl OrderInfo {
    /// 順序確定情報に署名して kind 21311 イベントを生成する(発行側 — スレ主ペルソナ鍵)。
    ///
    /// `entries` は空でなく、res_no が欠番なく +1 で連続していなければならない(T3)。
    pub fn sign(&self, keys: &Keys, created_at: u64) -> Result<Event, LivechatBuildError> {
        if !is_lower_hex(&self.board_id, 64) {
            return Err(LivechatBuildError::Invalid("invalid board_id"));
        }
        check_entries_consecutive(&self.entries)
            .map_err(|_| LivechatBuildError::Invalid("entries not consecutive"))?;
        let mut raw: Vec<Vec<String>> = vec![
            vec![
                "peca".into(),
                "thread".into(),
                self.board_id.clone(),
                self.generation.to_string(),
            ],
            vec!["peca".into(), "seq".into(), self.seq.to_string()],
        ];
        for entry in &self.entries {
            if !is_lower_hex(&entry.event_id, 64) {
                return Err(LivechatBuildError::Invalid("invalid event_id"));
            }
            raw.push(vec![
                "peca".into(),
                "order".into(),
                entry.res_no.to_string(),
                entry.event_id.clone(),
            ]);
        }
        let tags = build_tags(raw)?;
        sign_tags(keys, ORDER_KIND, "", tags, created_at, 0)
    }

    /// 検証済みイベントから写像を復元する(参加者側検証の形式・連続性部)。
    ///
    /// kind・peca thread/seq・order エントリ(res_no 連続・event_id hex64)を検査する。
    /// スレ主一致(FR-011)は署名者 pubkey の照合で呼び出し側が行う。
    pub fn from_event(event: &Event) -> Result<Self, LivechatReject> {
        if event.kind.as_u16() != ORDER_KIND {
            return Err(LivechatReject::InvalidFormat("unexpected kind"));
        }
        let (board_id, generation) = parse_thread_tag(event)?;
        let seq = parse_u32(peca_value(event, "seq"))
            .ok_or(LivechatReject::InvalidFormat("invalid seq"))?;

        let mut entries = Vec::new();
        for slice in peca_slices(event, "order") {
            // ["peca","order","<res_no>","<event_id>"]
            let res_no = slice
                .get(2)
                .and_then(|s| parse_u16(s))
                .ok_or(LivechatReject::InvalidFormat("invalid order res_no"))?;
            let event_id = slice
                .get(3)
                .filter(|s| is_lower_hex(s, 64))
                .ok_or(LivechatReject::InvalidFormat("invalid order event_id"))?
                .to_string();
            entries.push(OrderEntry { res_no, event_id });
        }
        check_entries_consecutive(&entries)?;

        Ok(Self {
            board_id,
            generation,
            seq,
            entries,
        })
    }
}

// ---------------------------------------------------------------------------
// kind 21311 の特殊形 — 明示クローズ通知(T047 — thread-delivery.md THREAD_CLOSE)
// ---------------------------------------------------------------------------

/// 明示クローズ通知(kind 21311 の `["peca","close"]` 特殊形)。`content` は空文字列。
///
/// [`OrderInfo`] と kind は同じ(21311)だが `order` エントリを持たず、代わりに
/// `["peca","close"]` タグで「このスレを閉じる」ことを表す。スレ主ペルソナ鍵で署名し
/// (署名者一致は [`OrderInfo`] と同じく呼び出し側が照合)、受信側はスレデータを削除する
/// (揮発 — FR-014/FR-015)。`seq` を持たないのは、クローズが O2 の seq 連番系列とは別の
/// 終端シグナルであり、欠落検出・再送要求の対象にしないため(クローズは再送不要 — 受信すれば
/// 即座にスレデータを破棄するだけで、欠落しても次回接続の WELCOME で closed reject を返す
/// ため実害がない)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadClose {
    /// スレ主 pubkey(hex 64)。
    pub board_id: String,
    /// 世代(u32)。
    pub generation: u32,
}

impl ThreadClose {
    /// クローズ通知に署名して kind 21311 イベントを生成する(発行側 — スレ主ペルソナ鍵)。
    pub fn sign(&self, keys: &Keys, created_at: u64) -> Result<Event, LivechatBuildError> {
        if !is_lower_hex(&self.board_id, 64) {
            return Err(LivechatBuildError::Invalid("invalid board_id"));
        }
        let raw: Vec<Vec<String>> = vec![
            vec![
                "peca".into(),
                "thread".into(),
                self.board_id.clone(),
                self.generation.to_string(),
            ],
            vec!["peca".into(), "close".into()],
        ];
        let tags = build_tags(raw)?;
        sign_tags(keys, ORDER_KIND, "", tags, created_at, 0)
    }

    /// 検証済みイベントから写像を復元する(`["peca","close"]` タグの有無で判別)。
    ///
    /// kind 21311 かつ `["peca","close"]` タグを持たない場合は
    /// [`LivechatReject::InvalidFormat`]([`OrderInfo`] として解釈すべきイベント)。
    /// スレ主一致(FR-011 と同じ規範)は署名者 pubkey の照合で呼び出し側が行う。
    pub fn from_event(event: &Event) -> Result<Self, LivechatReject> {
        if event.kind.as_u16() != ORDER_KIND {
            return Err(LivechatReject::InvalidFormat("unexpected kind"));
        }
        if peca_slice(event, "close").is_none() {
            return Err(LivechatReject::InvalidFormat("missing close tag"));
        }
        let (board_id, generation) = parse_thread_tag(event)?;
        Ok(Self {
            board_id,
            generation,
        })
    }
}

/// イベントが明示クローズ通知(`["peca","close"]` タグ付き kind 21311)かどうかを判定する
/// (受信側が [`OrderInfo::from_event`] と [`ThreadClose::from_event`] のどちらを試すか
/// 分岐するための軽量な判別子。タグ検査のみで署名検証は行わない)。
pub fn is_close_notice(event: &Event) -> bool {
    event.kind.as_u16() == ORDER_KIND && peca_slice(event, "close").is_some()
}

// ---------------------------------------------------------------------------
// 共通ヘルパ
// ---------------------------------------------------------------------------

/// `["peca","thread","<board_id hex64>","<gen u32>"]` を読み出す。
fn parse_thread_tag(event: &Event) -> Result<(String, u32), LivechatReject> {
    let slice =
        peca_slice(event, "thread").ok_or(LivechatReject::InvalidFormat("missing thread tag"))?;
    let board_id = slice
        .get(2)
        .filter(|s| is_lower_hex(s, 64))
        .ok_or(LivechatReject::InvalidFormat("invalid thread board_id"))?
        .to_string();
    let generation = slice
        .get(3)
        .and_then(|s| parse_u32_str(s))
        .ok_or(LivechatReject::InvalidFormat("invalid thread gen"))?;
    Ok((board_id, generation))
}

/// order エントリが空でなく res_no が欠番なく +1 で連続しているか(不変条件 T3)。
fn check_entries_consecutive(entries: &[OrderEntry]) -> Result<(), LivechatReject> {
    if entries.is_empty() {
        return Err(LivechatReject::InvalidFormat("empty order entries"));
    }
    let first = entries[0].res_no;
    if first == 0 {
        return Err(LivechatReject::InvalidFormat("res_no must start at 1"));
    }
    for (i, entry) in entries.iter().enumerate() {
        let expected = first
            .checked_add(i as u16)
            .ok_or(LivechatReject::InvalidFormat("res_no overflow"))?;
        if entry.res_no != expected {
            return Err(LivechatReject::InvalidFormat("res_no not consecutive"));
        }
    }
    Ok(())
}

/// `30311:<pubkey hex64>:<guid hex32>` をパースする(署名者一致検査は呼び出し側)。
fn parse_channel_ref(a: &str) -> Option<(String, String)> {
    let rest = a.strip_prefix(&format!("{CHANNEL_KIND}:"))?;
    let (pubkey, guid) = rest.split_once(':')?;
    if is_lower_hex(pubkey, 64) && is_lower_hex(guid, 32) {
        Some((pubkey.to_string(), guid.to_string()))
    } else {
        None
    }
}

/// 名前欄の `#` 以降を除去する(トリップ入力の秘匿 — FR-024)。
fn strip_after_hash(name: &str) -> String {
    match name.split_once('#') {
        Some((head, _)) => head.to_string(),
        None => name.to_string(),
    }
}

/// 本文の制御文字を除去する(改行 `\n` は残す — data-model §Res)。
fn strip_body_control(s: &str) -> String {
    s.chars()
        .filter(|c| *c == '\n' || !c.is_control())
        .collect()
}

/// 行数(`\n` 区切りの要素数。空文字列は 1 行)。
fn line_count(s: &str) -> usize {
    s.split('\n').count()
}

/// 生タグ列を [`Tag`] へ変換する。
fn build_tags(raw: Vec<Vec<String>>) -> Result<Vec<Tag>, LivechatBuildError> {
    raw.into_iter()
        .map(|elems| Tag::parse(elems).map_err(|e| LivechatBuildError::Nostr(e.to_string())))
        .collect()
}

/// 指定 kind・content・タグでイベントを署名する(共通)。
fn sign_tags(
    keys: &Keys,
    kind: u16,
    content: &str,
    tags: Vec<Tag>,
    created_at: u64,
    pow_bits: u8,
) -> Result<Event, LivechatBuildError> {
    let mut builder = EventBuilder::new(Kind::Custom(kind), content)
        .tags(tags)
        .custom_created_at(Timestamp::from(created_at));
    if pow_bits > 0 {
        builder = builder.pow(pow_bits);
    }
    builder
        .sign_with_keys(keys)
        .map_err(|e| LivechatBuildError::Nostr(e.to_string()))
}

// --- タグ読み出し(schema.rs と同型。前方互換: 未知タグは自然に無視される)-------

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

/// peca 拡張タグ(`["peca", sub, ..]`)の最初のスライス全体を返す。
fn peca_slice<'a>(event: &'a Event, sub: &str) -> Option<&'a [String]> {
    peca_slices(event, sub).into_iter().next()
}

/// peca 拡張タグ(`["peca", sub, ..]`)のスライスを出現順に返す(複数可 — order 用)。
fn peca_slices<'a>(event: &'a Event, sub: &str) -> Vec<&'a [String]> {
    event
        .tags
        .iter()
        .map(Tag::as_slice)
        .filter(|slice| {
            slice.first().map(String::as_str) == Some("peca")
                && slice.get(1).map(String::as_str) == Some(sub)
        })
        .collect()
}

/// peca 拡張タグの第 1 値(要素 index 2)を返す。
fn peca_value<'a>(event: &'a Event, sub: &str) -> Option<&'a str> {
    peca_slice(event, sub).and_then(|slice| slice.get(2).map(String::as_str))
}

/// 非負十進整数(符号・空白を許容しない)。
fn parse_u64(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<u64>().ok()
}

fn parse_u64_req(s: Option<&str>) -> Option<u64> {
    s.and_then(parse_u64)
}

fn parse_u32_str(s: &str) -> Option<u32> {
    parse_u64(s).and_then(|n| u32::try_from(n).ok())
}

fn parse_u32(s: Option<&str>) -> Option<u32> {
    s.and_then(parse_u32_str)
}

fn parse_u16(s: &str) -> Option<u16> {
    parse_u64(s).and_then(|n| u16::try_from(n).ok())
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::JsonUtil;

    const GUID: &str = "0123456789abcdef0123456789abcdef";

    fn board_keys() -> Keys {
        Keys::generate()
    }

    fn channel_ref(pubkey: &str) -> String {
        format!("{CHANNEL_KIND}:{pubkey}:{GUID}")
    }

    // --- kind 31311 announce -------------------------------------------------

    fn sample_announce(pubkey: &str) -> ThreadAnnounce {
        ThreadAnnounce {
            channel: channel_ref(pubkey),
            title: "実況スレ".into(),
            generation: 3,
            key: 1_700_000_000,
            res_count: Some(42),
            tip: "198.51.100.1:7147".into(),
        }
    }

    #[test]
    fn announce_sign_and_roundtrip() {
        let keys = board_keys();
        let pubkey = keys.public_key().to_hex();
        let announce = sample_announce(&pubkey);
        let event = announce.sign(&keys, announce.key, 0).unwrap();

        assert_eq!(event.kind.as_u16(), ANNOUNCE_KIND);
        assert_eq!(event.content, "");
        assert_eq!(tag_value(&event, "d"), Some(ANNOUNCE_D));
        assert_eq!(
            tag_value(&event, "expiration"),
            Some((announce.key + EXPIRATION_OFFSET_SECS).to_string().as_str())
        );

        let restored = ThreadAnnounce::from_event(&event).unwrap();
        assert_eq!(restored, announce);
    }

    #[test]
    fn announce_res_count_omitted_when_none() {
        let keys = board_keys();
        let pubkey = keys.public_key().to_hex();
        let mut announce = sample_announce(&pubkey);
        announce.res_count = None;
        let event = announce.sign(&keys, announce.key, 0).unwrap();
        assert!(tag_value(&event, "res_count").is_none());
        assert_eq!(ThreadAnnounce::from_event(&event).unwrap(), announce);
    }

    #[test]
    fn announce_rejects_bad_tip_and_title() {
        let keys = board_keys();
        let pubkey = keys.public_key().to_hex();
        let mut a = sample_announce(&pubkey);
        a.tip = "not-an-addr".into();
        assert_eq!(
            a.sign(&keys, a.key, 0),
            Err(LivechatBuildError::Invalid("invalid tip ip:port"))
        );

        let mut a = sample_announce(&pubkey);
        a.title = "あ".repeat(TITLE_MAX_CHARS + 1);
        assert_eq!(
            a.sign(&keys, a.key, 0),
            Err(LivechatBuildError::Invalid("title too long"))
        );
    }

    #[test]
    fn announce_from_event_rejects_wrong_d_and_kind() {
        let keys = board_keys();
        // kind 不一致(1311 を渡す)
        let res = sample_res(&keys.public_key().to_hex());
        let ev = res.sign(&keys, 1, 0).unwrap();
        assert_eq!(
            ThreadAnnounce::from_event(&ev),
            Err(LivechatReject::InvalidFormat("unexpected kind"))
        );
    }

    // --- kind 1311 レス ------------------------------------------------------

    fn sample_res(board_id: &str) -> Res {
        Res {
            channel: channel_ref(board_id),
            board_id: board_id.to_string(),
            generation: 3,
            name: Some("名無し".into()),
            mail: Some("sage".into()),
            body: "本文\nテスト >>1".into(),
        }
    }

    #[test]
    fn res_sign_and_roundtrip() {
        let keys = board_keys();
        let board_id = "ab".repeat(32);
        let res = sample_res(&board_id);
        let event = res.sign(&keys, 1_700_000_000, 0).unwrap();

        assert_eq!(event.kind.as_u16(), RES_KIND);
        assert_eq!(event.content, "本文\nテスト >>1");
        let restored = Res::from_event(&event).unwrap();
        assert_eq!(restored, res);
    }

    #[test]
    fn res_name_hash_stripped_before_send() {
        let keys = board_keys();
        let board_id = "cd".repeat(32);
        let mut res = sample_res(&board_id);
        res.name = Some("コテハン#ひみつ".into());
        let event = res.sign(&keys, 1, 0).unwrap();
        // `#` 以降が除去されて署名される(FR-024)。
        assert_eq!(peca_value(&event, "name"), Some("コテハン"));
        assert_eq!(
            Res::from_event(&event).unwrap().name.as_deref(),
            Some("コテハン")
        );
    }

    #[test]
    fn res_empty_name_is_none() {
        let keys = board_keys();
        let board_id = "ef".repeat(32);
        let mut res = sample_res(&board_id);
        res.name = None;
        res.mail = None;
        let event = res.sign(&keys, 1, 0).unwrap();
        assert!(peca_value(&event, "name").is_none());
        let restored = Res::from_event(&event).unwrap();
        assert_eq!(restored.name, None);
        assert_eq!(restored.mail, None);
    }

    #[test]
    fn res_body_control_chars_stripped_newline_kept() {
        let keys = board_keys();
        let board_id = "12".repeat(32);
        let mut res = sample_res(&board_id);
        res.body = "行1\n\u{7}制御\t除去".into();
        let event = res.sign(&keys, 1, 0).unwrap();
        // 改行は残り、その他制御文字(ベル・タブ)は除去される。
        assert_eq!(event.content, "行1\n制御除去");
    }

    #[test]
    fn res_rejects_body_too_long_and_too_many_lines() {
        let keys = board_keys();
        let board_id = "34".repeat(32);
        let mut res = sample_res(&board_id);
        res.body = "あ".repeat(BODY_MAX_CHARS + 1);
        assert_eq!(
            res.sign(&keys, 1, 0),
            Err(LivechatBuildError::Invalid("body too long"))
        );

        let mut res = sample_res(&board_id);
        res.body = "x\n".repeat(BODY_MAX_LINES).trim_end().to_string() + "\ny";
        assert_eq!(
            res.sign(&keys, 1, 0),
            Err(LivechatBuildError::Invalid("too many lines"))
        );
    }

    #[test]
    fn res_from_event_rejects_oversized_body() {
        // 署名済みイベントの content を検証側で拒否できること(ホスト側二重防御)。
        let keys = board_keys();
        let board_id = "56".repeat(32);
        // sign は上限で弾くので、上限ちょうどのレスを作ってから content を検証で確認。
        let mut res = sample_res(&board_id);
        res.body = "y".repeat(BODY_MAX_CHARS);
        let event = res.sign(&keys, 1, 0).unwrap();
        assert!(Res::from_event(&event).is_ok());
    }

    // --- kind 21311 順序確定情報 ---------------------------------------------

    fn sample_order(board_id: &str) -> OrderInfo {
        OrderInfo {
            board_id: board_id.to_string(),
            generation: 3,
            seq: 7,
            entries: vec![
                OrderEntry {
                    res_no: 10,
                    event_id: "aa".repeat(32),
                },
                OrderEntry {
                    res_no: 11,
                    event_id: "bb".repeat(32),
                },
            ],
        }
    }

    #[test]
    fn order_sign_and_roundtrip() {
        let keys = board_keys();
        let board_id = "78".repeat(32);
        let order = sample_order(&board_id);
        let event = order.sign(&keys, 1_700_000_000).unwrap();

        assert_eq!(event.kind.as_u16(), ORDER_KIND);
        assert_eq!(event.content, "");
        let restored = OrderInfo::from_event(&event).unwrap();
        assert_eq!(restored, order);
    }

    #[test]
    fn order_rejects_non_consecutive_res_no() {
        let keys = board_keys();
        let board_id = "9a".repeat(32);
        let mut order = sample_order(&board_id);
        order.entries[1].res_no = 13; // 11 であるべきところ 13(欠番)
        assert_eq!(
            order.sign(&keys, 1),
            Err(LivechatBuildError::Invalid("entries not consecutive"))
        );
    }

    #[test]
    fn order_rejects_empty_entries() {
        let keys = board_keys();
        let board_id = "bc".repeat(32);
        let mut order = sample_order(&board_id);
        order.entries.clear();
        assert!(order.sign(&keys, 1).is_err());
    }

    // --- kind 21311 特殊形: 明示クローズ通知(T047)---------------------------

    #[test]
    fn thread_close_sign_and_roundtrip() {
        let keys = board_keys();
        let board_id = "de".repeat(32);
        let close = ThreadClose {
            board_id: board_id.clone(),
            generation: 5,
        };
        let event = close.sign(&keys, 1_700_000_000).unwrap();

        assert_eq!(event.kind.as_u16(), ORDER_KIND, "kind は 21311 を共有する");
        assert_eq!(event.content, "");
        assert!(is_close_notice(&event), "close タグで判別できる");
        let restored = ThreadClose::from_event(&event).unwrap();
        assert_eq!(restored, close);
    }

    #[test]
    fn thread_close_is_distinguishable_from_order() {
        // 通常の OrderInfo(close タグなし)は is_close_notice で偽と判定され、
        // ThreadClose::from_event でも拒否される(受信側の分岐規則)。
        let keys = board_keys();
        let board_id = "ef".repeat(32);
        let order_event = sample_order(&board_id).sign(&keys, 1).unwrap();
        assert!(!is_close_notice(&order_event));
        assert_eq!(
            ThreadClose::from_event(&order_event),
            Err(LivechatReject::InvalidFormat("missing close tag"))
        );

        // 逆に ThreadClose イベントは OrderInfo::from_event では seq タグを持たないため
        // 拒否される(invalid seq — close イベントに seq/order タグが一切ないことの確認)。
        let close_event = ThreadClose {
            board_id: board_id.clone(),
            generation: 1,
        }
        .sign(&keys, 1)
        .unwrap();
        assert!(is_close_notice(&close_event));
        assert_eq!(
            OrderInfo::from_event(&close_event),
            Err(LivechatReject::InvalidFormat("invalid seq"))
        );
    }

    #[test]
    fn thread_close_rejects_invalid_board_id() {
        let keys = board_keys();
        let close = ThreadClose {
            board_id: "not-hex".into(),
            generation: 1,
        };
        assert_eq!(
            close.sign(&keys, 1),
            Err(LivechatBuildError::Invalid("invalid board_id"))
        );
    }

    // --- 前方互換 ------------------------------------------------------------

    #[test]
    fn unknown_tags_and_peca_subtags_ignored() {
        // 未知タグ・未知 peca サブタグを足しても復元は成功する(前方互換 MUST)。
        let keys = board_keys();
        let board_id = keys.public_key().to_hex();
        let announce = sample_announce(&board_id);
        let base = announce.sign(&keys, announce.key, 0).unwrap();

        // 既存タグ + 未知タグ/未知 peca サブタグを付けて再署名。
        let mut tags: Vec<Tag> = base.tags.iter().cloned().collect();
        tags.push(Tag::parse(["futuretag", "value"]).unwrap());
        tags.push(Tag::parse(["peca", "unknownsub", "x"]).unwrap());
        let event = EventBuilder::new(Kind::Custom(ANNOUNCE_KIND), "")
            .tags(tags)
            .custom_created_at(Timestamp::from(announce.key))
            .sign_with_keys(&keys)
            .unwrap();

        assert_eq!(ThreadAnnounce::from_event(&event).unwrap(), announce);
    }

    #[test]
    fn signed_events_verify() {
        // 生成したイベントは nostr の署名検証を通る(id/sig 整合)。
        let keys = board_keys();
        let pubkey = keys.public_key().to_hex();
        let announce = sample_announce(&pubkey).sign(&keys, 1, 0).unwrap();
        assert!(announce.verify().is_ok());
        let res = sample_res(&pubkey).sign(&keys, 1, 0).unwrap();
        assert!(res.verify().is_ok());
        let order = sample_order(&pubkey).sign(&keys, 1).unwrap();
        assert!(order.verify().is_ok());
        // JSON 往復も可能(直列化の健全性)。
        let raw = announce.as_json();
        let parsed = Event::from_json(&raw).unwrap();
        assert_eq!(
            ThreadAnnounce::from_event(&parsed).unwrap(),
            sample_announce(&pubkey)
        );
    }
}

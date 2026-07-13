//! スレ状態の型と状態機械(T013 — data-model §エンティティ)
//!
//! 本モジュールのドメイン型は永続化・イベント封筒とは別物である:
//!
//! - [`Res`] は本モジュールの**ドメイン表現**(確定 res_no・`pending` を持つ)であり、
//!   イベント封筒 [`crate::event::livechat::Res`](kind 1311 の直列化・署名検証)とは別の型。
//!   両者の対応づけは呼び出し側([`host`](super::host) / [`session`](super::session))が担う。
//! - [`OrderInfo`] も同様に、イベント封筒 `crate::event::livechat::OrderInfo` とは別物。
//! - [`BoardSettings`] は [`crate::store::BoardSettingsRow`] との相互変換
//!   ([`BoardSettings::from_row`] / [`BoardSettings::to_row`])を提供する。値域強制は
//!   本モジュールの責務であり、ストア側は行の保管のみを担う(data-model §BoardSettings)。
//!
//! 不変条件(data-model §スレ):
//!
//! - **T1**: `state != Active` のスレへの書き込み(レス追加・採番)は受理しない。
//! - **T2**(板内で `Active` は高々 1 本)は板→スレのコンテナ責務であり、`Thread` 単体では
//!   強制しきれない。**所有側([`board`](super::board) 等のホスト)が強制する**(ここでは
//!   実装しない)。
//! - **T3**: 確定レスの `res_no` は 1 から欠番なく単調増加し、`res_limit` を超えない。

use crate::security::strip_control_chars;
use crate::store::BoardSettingsRow;

// ---------------------------------------------------------------------------
// 板設定(BoardSettings)— data-model §BoardSettings(FR-022〜FR-025)
// ---------------------------------------------------------------------------

/// タイトルの最大文字数。
pub const TITLE_MAX_CHARS: usize = 128;
/// res_limit の下限・上限・既定値。
pub const RES_LIMIT_MIN: u16 = 100;
pub const RES_LIMIT_MAX: u16 = 4000;
const RES_LIMIT_DEFAULT: u16 = 1000;
/// noname_name の最小・最大文字数。
pub const NONAME_NAME_MIN_CHARS: usize = 1;
pub const NONAME_NAME_MAX_CHARS: usize = 64;
const NONAME_NAME_DEFAULT: &str = "名無しさん";
/// local_rules の最大文字数。
pub const LOCAL_RULES_MAX_CHARS: usize = 2048;
/// first_post_pow_bits の上限(0〜32、既定 20 — research R6)。
pub const FIRST_POST_POW_BITS_MAX: u8 = 32;
const FIRST_POST_POW_BITS_DEFAULT: u8 = 20;

/// 板設定の検証エラー。`Display` は内部情報を漏らさない(Principle II)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardSettingsError {
    /// title が上限を超える。
    TitleTooLong,
    /// res_limit が範囲外(100〜4000)。
    ResLimitOutOfRange,
    /// noname_name が範囲外(1〜64 文字)。
    NonameNameOutOfRange,
    /// local_rules が上限を超える。
    LocalRulesTooLong,
    /// first_post_pow_bits が範囲外(0〜32)。
    PowBitsOutOfRange,
}

impl std::fmt::Display for BoardSettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BoardSettingsError::TitleTooLong => write!(f, "タイトルが長すぎます"),
            BoardSettingsError::ResLimitOutOfRange => {
                write!(f, "res_limit は 100〜4000 の範囲で指定してください")
            }
            BoardSettingsError::NonameNameOutOfRange => {
                write!(f, "名無し名は 1〜64 文字で指定してください")
            }
            BoardSettingsError::LocalRulesTooLong => write!(f, "ローカルルールが長すぎます"),
            BoardSettingsError::PowBitsOutOfRange => {
                write!(f, "first_post_pow_bits は 0〜32 の範囲で指定してください")
            }
        }
    }
}

impl std::error::Error for BoardSettingsError {}

/// 板設定(data-model §BoardSettings)。制御文字は保持前に除去する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardSettings {
    /// スレタイトル(≤ 128 文字。制御文字除去。即時反映)。
    pub title: String,
    /// 確定レス上限(100〜4000、既定 1000)。**次スレから**反映(FR-023)。
    pub res_limit: u16,
    /// 名無し名(1〜64 文字。制御文字除去。即時反映)。
    pub noname_name: String,
    /// ローカルルール(≤ 2048 文字。Markdown。即時反映)。
    pub local_rules: String,
    /// 初回書き込み PoW ビット数(0〜32、既定 20 — research R6)。即時反映。
    pub first_post_pow_bits: u8,
}

impl Default for BoardSettings {
    fn default() -> Self {
        BoardSettings {
            title: String::new(),
            res_limit: RES_LIMIT_DEFAULT,
            noname_name: NONAME_NAME_DEFAULT.to_string(),
            local_rules: String::new(),
            first_post_pow_bits: FIRST_POST_POW_BITS_DEFAULT,
        }
    }
}

impl BoardSettings {
    /// 値域を検証する(範囲外は `Err`)。制御文字は事前に除去済みである前提だが、
    /// 文字数は除去後の値で判定する(呼び出し側は [`strip_control_chars`] を通すこと)。
    pub fn validate(&self) -> Result<(), BoardSettingsError> {
        if self.title.chars().count() > TITLE_MAX_CHARS {
            return Err(BoardSettingsError::TitleTooLong);
        }
        if !(RES_LIMIT_MIN..=RES_LIMIT_MAX).contains(&self.res_limit) {
            return Err(BoardSettingsError::ResLimitOutOfRange);
        }
        let noname_len = self.noname_name.chars().count();
        if !(NONAME_NAME_MIN_CHARS..=NONAME_NAME_MAX_CHARS).contains(&noname_len) {
            return Err(BoardSettingsError::NonameNameOutOfRange);
        }
        if self.local_rules.chars().count() > LOCAL_RULES_MAX_CHARS {
            return Err(BoardSettingsError::LocalRulesTooLong);
        }
        if self.first_post_pow_bits > FIRST_POST_POW_BITS_MAX {
            return Err(BoardSettingsError::PowBitsOutOfRange);
        }
        Ok(())
    }

    /// 制御文字を除去した新しい値で構成する(受信・保存前の正規化)。
    pub fn sanitized(&self) -> Self {
        BoardSettings {
            title: strip_control_chars(&self.title),
            res_limit: self.res_limit,
            noname_name: strip_control_chars(&self.noname_name),
            local_rules: strip_control_chars(&self.local_rules),
            first_post_pow_bits: self.first_post_pow_bits,
        }
    }

    /// [`BoardSettingsRow`] から復元する(res_limit/first_post_pow_bits は i64→u16/u8)。
    /// 範囲外の値は既定値へフォールバックする(保管後の型変更・破損への耐性)。
    pub fn from_row(row: &BoardSettingsRow) -> Self {
        let res_limit = u16::try_from(row.res_limit)
            .ok()
            .filter(|v| (RES_LIMIT_MIN..=RES_LIMIT_MAX).contains(v))
            .unwrap_or(RES_LIMIT_DEFAULT);
        let first_post_pow_bits = u8::try_from(row.first_post_pow_bits)
            .ok()
            .filter(|v| *v <= FIRST_POST_POW_BITS_MAX)
            .unwrap_or(FIRST_POST_POW_BITS_DEFAULT);
        BoardSettings {
            title: row.title.clone(),
            res_limit,
            noname_name: row.noname_name.clone(),
            local_rules: row.local_rules.clone(),
            first_post_pow_bits,
        }
    }

    /// [`BoardSettingsRow`] へ変換する(res_limit/first_post_pow_bits は u16/u8→i64)。
    pub fn to_row(&self, board_id: &str) -> BoardSettingsRow {
        BoardSettingsRow {
            board_id: board_id.to_string(),
            title: self.title.clone(),
            res_limit: i64::from(self.res_limit),
            noname_name: self.noname_name.clone(),
            local_rules: self.local_rules.clone(),
            first_post_pow_bits: i64::from(self.first_post_pow_bits),
        }
    }
}

// ---------------------------------------------------------------------------
// スレ状態(ThreadState)— data-model §スレ 状態遷移
// ---------------------------------------------------------------------------

/// スレの状態(data-model §スレ)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadState {
    /// 書き込み・採番を受理する通常状態。
    Active,
    /// ホストとの接続喪失・announce 鮮度切れ。閲覧のみで書き込み不可。
    Frozen,
    /// スレ主署名付きクローズ通知の受信後。終端状態(以後の遷移なし)。
    Closed,
}

/// 状態遷移の拒否理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidTransition {
    pub from: ThreadState,
    pub to: ThreadState,
}

impl std::fmt::Display for InvalidTransition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "許可されない状態遷移です")
    }
}

impl std::error::Error for InvalidTransition {}

impl ThreadState {
    /// `Active` へ遷移できるか(data-model: `Frozen → Active` のみ。`Active` は自己遷移を
    /// 許容しない — 次スレ移行は新規 `Thread` を作るため、既存スレの自己遷移では表現しない)。
    pub fn freeze(self) -> Result<ThreadState, InvalidTransition> {
        match self {
            ThreadState::Active => Ok(ThreadState::Frozen),
            _ => Err(InvalidTransition {
                from: self,
                to: ThreadState::Frozen,
            }),
        }
    }

    /// 瞬断復帰(`Frozen → Active`)。
    pub fn resume(self) -> Result<ThreadState, InvalidTransition> {
        match self {
            ThreadState::Frozen => Ok(ThreadState::Active),
            _ => Err(InvalidTransition {
                from: self,
                to: ThreadState::Active,
            }),
        }
    }

    /// クローズ(`Active` / `Frozen` → `Closed`)。`Closed` は終端で再遷移不可。
    pub fn close(self) -> Result<ThreadState, InvalidTransition> {
        match self {
            ThreadState::Active | ThreadState::Frozen => Ok(ThreadState::Closed),
            ThreadState::Closed => Err(InvalidTransition {
                from: self,
                to: ThreadState::Closed,
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// レス(ドメイン表現)— data-model §レス
// ---------------------------------------------------------------------------

/// レス(スレ内のドメイン表現)。イベント封筒 [`crate::event::livechat::Res`] とは別物
/// (モジュール doc 参照)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Res {
    /// 一意性の根拠(nostr event id)。
    pub event_id: String,
    /// 署名者 = 板鍵(pubkey hex 64)。ID 表示・NG/BAN 完全鍵照合に使用。
    pub board_key: String,
    /// 名前欄。空・省略は当該レス確定時点の `noname_name` で表示(遡及しない — FR-023)。
    pub name: Option<String>,
    /// メール欄。表示互換のみ・機能的意味なし(FR-029)。
    pub mail: Option<String>,
    /// 本文。
    pub body: String,
    /// 参考情報(正となる順序は `res_no` のみ — spec Edge Case)。
    pub created_at: i64,
    /// 確定後のみ `Some`(順序確定情報が与える)。
    pub res_no: Option<u16>,
    /// 自分の未確定投稿のみ `true`(「送信中」表示 — FR-008)。
    pub pending: bool,
}

// ---------------------------------------------------------------------------
// 順序確定情報(ドメイン表現)— data-model §順序確定情報
// ---------------------------------------------------------------------------

/// 順序確定情報(スレ内のドメイン表現)。イベント封筒
/// `crate::event::livechat::OrderInfo` とは別物(モジュール doc 参照)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderInfo {
    /// 対象スレの板 id(スレ主 pubkey)。
    pub board_id: String,
    /// 対象スレの世代。
    pub generation: u32,
    /// 確定情報自体の連番(欠落検出 → 再送要求に使用)。
    pub seq: u32,
    /// 今回確定した採番(res_no, event_id)。res_no は既存確定の続きから欠番なし。
    pub entries: Vec<(u16, String)>,
}

// ---------------------------------------------------------------------------
// スレ(Thread)
// ---------------------------------------------------------------------------

/// 書き込み・確定操作の拒否理由。`Display` は内部情報を漏らさない(Principle II)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadError {
    /// 不変条件 T1: `state != Active` のスレへの書き込み・採番。
    NotActive,
    /// 不変条件 T3: 確定しようとした res_no が次に期待する値と一致しない(欠番・逆行・重複)。
    UnexpectedResNo { expected: u16, got: u16 },
    /// 不変条件 T3: res_limit を超える採番。
    ResLimitExceeded,
}

impl std::fmt::Display for ThreadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ThreadError::NotActive => write!(f, "このスレへは書き込めません"),
            ThreadError::UnexpectedResNo { .. } => write!(f, "順序確定情報が不正です"),
            ThreadError::ResLimitExceeded => write!(f, "レス数上限に達しています"),
        }
    }
}

impl std::error::Error for ThreadError {}

/// スレ(data-model §スレ)。
///
/// 不変条件 T2(板内で `Active` は高々 1 本)は本型では強制しない(モジュール doc 参照)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thread {
    /// スレ主(配信者)ペルソナの公開鍵。
    pub board_id: String,
    /// 対象チャンネル(`30311:<pubkey>:<guid>` — announce の `a` タグ)。
    pub channel: String,
    /// スレ世代(次スレの世代)。板内で単調増加。
    pub generation: u32,
    /// スレ作成 unix 秒(互換 API の dat キー)。
    pub key: u64,
    /// スレタイトル(≤ 128 文字)。
    pub title: String,
    /// 作成時の板設定のスナップショット(FR-023 — 進行中スレは作成時の値で固定)。
    pub res_limit: u16,
    /// 現在の状態。
    pub state: ThreadState,
    /// 確定レス列(res_no 1..=res_limit の順)。
    pub res: Vec<Res>,
}

impl Thread {
    /// 新規スレを開設する(`Active` から開始 — data-model §状態遷移)。
    pub fn new(
        board_id: impl Into<String>,
        channel: impl Into<String>,
        generation: u32,
        key: u64,
        title: impl Into<String>,
        res_limit: u16,
    ) -> Self {
        Thread {
            board_id: board_id.into(),
            channel: channel.into(),
            generation,
            key,
            title: title.into(),
            res_limit,
            state: ThreadState::Active,
            res: Vec::new(),
        }
    }

    /// 次に確定されるべき res_no(1 始まり)。
    pub fn next_res_no(&self) -> u16 {
        self.res.len() as u16 + 1
    }

    /// 未確定レスを保留リストへ加える前段のガード(不変条件 T1)。
    /// 書き込み自体(保留一覧への追加)は上位([`session`](super::session))が保持し、
    /// 本メソッドは「受理してよいか」のみを判定する。
    pub fn check_writable(&self) -> Result<(), ThreadError> {
        if self.state != ThreadState::Active {
            return Err(ThreadError::NotActive);
        }
        Ok(())
    }

    /// レスを確定する(不変条件 T1・T3 を強制)。
    ///
    /// - T1: `state != Active` なら [`ThreadError::NotActive`]。
    /// - T3: `res_no` は次に期待する値(`next_res_no`)と一致しなければならない。
    ///   不一致は [`ThreadError::UnexpectedResNo`]。`res_limit` 超過は
    ///   [`ThreadError::ResLimitExceeded`]。
    ///
    /// 確定成功時は `res.res_no = Some(res_no)` として列へ追加する。
    pub fn confirm(&mut self, mut res: Res, res_no: u16) -> Result<(), ThreadError> {
        self.check_writable()?;
        let expected = self.next_res_no();
        if res_no != expected {
            return Err(ThreadError::UnexpectedResNo {
                expected,
                got: res_no,
            });
        }
        if res_no > self.res_limit {
            return Err(ThreadError::ResLimitExceeded);
        }
        res.res_no = Some(res_no);
        res.pending = false;
        self.res.push(res);
        Ok(())
    }

    /// ホストとの接続喪失・announce 鮮度切れによる凍結(`Active → Frozen`)。
    pub fn freeze(&mut self) -> Result<(), InvalidTransition> {
        self.state = self.state.freeze()?;
        Ok(())
    }

    /// 瞬断復帰(`Frozen → Active`。同一世代の継続が前提 — 呼び出し側で確認する)。
    pub fn resume(&mut self) -> Result<(), InvalidTransition> {
        self.state = self.state.resume()?;
        Ok(())
    }

    /// スレ主署名付きクローズ通知の受信によるクローズ(終端)。
    pub fn close(&mut self) -> Result<(), InvalidTransition> {
        self.state = self.state.close()?;
        Ok(())
    }

    // --- アンカー解決(`>>n`)— FR-009(全端末一致)--------------------------

    /// アンカー `>>n` を確定レスへ解決する(T031 — FR-009)。
    ///
    /// `res_no` に対応する確定レスを返す(未確定・欠番・範囲外は `None`)。確定列は不変条件
    /// T3(1 から欠番なく単調増加)を満たすため `res[res_no - 1]` が res_no のレスになる。
    /// 全端末で確定列(res_no → event_id)が一致する(DisplayPrefix / 不変条件 O2)ため、
    /// **同一 `>>n` は全端末で同一イベントに解決される**(FR-009 のアンカー一致)。
    ///
    /// `res_no == 0`(`>>0` は無効)や `res_no > 確定数` は `None`。
    pub fn resolve_anchor(&self, res_no: u16) -> Option<&Res> {
        if res_no == 0 {
            return None;
        }
        let res = self.res.get(usize::from(res_no) - 1)?;
        // 防御的整合性チェック: 格納位置と res_no が一致すること(T3 の帰結)。
        if res.res_no == Some(res_no) {
            Some(res)
        } else {
            // T3 が守られていれば到達しないが、破損時は解決しない(誤リンクを避ける)。
            self.res.iter().find(|r| r.res_no == Some(res_no))
        }
    }

    /// 本文中の全アンカー `>>n` を解決した `(res_no, 対応レス)` 列を返す(表現層の補助)。
    ///
    /// 未確定・範囲外のアンカーは含めない(確定済みのみ解決 — FR-008/FR-009)。同一 `>>n` が
    /// 複数回現れても各出現ごとに 1 エントリ返す(表現層が本文の該当箇所へリンクを張るため)。
    pub fn resolve_anchors_in<'a>(&'a self, body: &str) -> Vec<(u16, &'a Res)> {
        parse_anchors(body)
            .into_iter()
            .filter_map(|n| self.resolve_anchor(n).map(|r| (n, r)))
            .collect()
    }

    /// NG 判定(board_key 完全一致)で確定列から非表示レスを除いた可視列を返す(T043 — FR-020)。
    ///
    /// **除外しても res_no は詰めない**(NG による欠番は表示上のみ・採番は不変 — 不変条件 T3 の
    /// 帰結。互換 API の dat には非適用 — contracts/compat-api.md)。`is_ng` は視聴者ローカルの
    /// 判定(例: [`crate::livechat::moderation::Moderation::is_ng`])を渡す。
    pub fn visible_res(&self, is_ng: impl Fn(&str) -> bool) -> Vec<&Res> {
        self.res.iter().filter(|r| !is_ng(&r.board_key)).collect()
    }
}

/// 本文中のアンカー `>>n` の参照先 res_no を出現順に抽出する(T031 — FR-009)。
///
/// `>>` に続く 1 桁以上の 10 進数を 1 アンカーとして読む。`>>>` のような 3 連以上は
/// 「先頭 2 個を `>>`、残りを本文」とはせず、**`>` が 2 個連続した直後の数字列**のみを
/// アンカーとする(伝統的掲示板の慣習に沿う。`>>12` は 12、`>>>12` も末尾の `>>12` を拾う)。
/// u16 を超える数値(> 65535)は採番され得ないためスキップする(res_limit ≤ 4000)。
pub fn parse_anchors(body: &str) -> Vec<u16> {
    let bytes = body.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        // `>>` を探す(直前が `>` でも、連続する `>` の最後の 2 個を境界に数字を読む)。
        if bytes[i] == b'>' && bytes[i + 1] == b'>' {
            let mut j = i + 2;
            let start = j;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > start {
                // 数字列を u32 でパースし u16 に収まるものだけ採る(桁溢れは無効アンカー)。
                if let Ok(n) = body[start..j].parse::<u32>()
                    && let Ok(n16) = u16::try_from(n)
                    && n16 != 0
                {
                    out.push(n16);
                }
                i = j;
                continue;
            }
        }
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn board_id() -> String {
        "ab".repeat(32)
    }

    fn sample_res(event_id: &str) -> Res {
        Res {
            event_id: event_id.to_string(),
            board_key: "cd".repeat(32),
            name: Some("名無し".to_string()),
            mail: None,
            body: "本文".to_string(),
            created_at: 1_700_000_000,
            res_no: None,
            pending: true,
        }
    }

    fn sample_thread(res_limit: u16) -> Thread {
        let board_id = board_id();
        Thread::new(
            &board_id,
            format!("30311:{board_id}:{}", "ef".repeat(16)),
            1,
            1_700_000_000,
            "実況スレ",
            res_limit,
        )
    }

    // --- ThreadState 状態遷移 ------------------------------------------------

    #[test]
    fn state_freeze_and_resume() {
        assert_eq!(ThreadState::Active.freeze(), Ok(ThreadState::Frozen));
        assert_eq!(ThreadState::Frozen.resume(), Ok(ThreadState::Active));
    }

    #[test]
    fn state_close_from_active_and_frozen() {
        assert_eq!(ThreadState::Active.close(), Ok(ThreadState::Closed));
        assert_eq!(ThreadState::Frozen.close(), Ok(ThreadState::Closed));
    }

    #[test]
    fn state_closed_is_terminal() {
        assert!(ThreadState::Closed.close().is_err());
        assert!(ThreadState::Closed.freeze().is_err());
        assert!(ThreadState::Closed.resume().is_err());
    }

    #[test]
    fn state_rejects_invalid_transitions() {
        // Active から resume はできない(Frozen 経由が必須)。
        assert!(ThreadState::Active.resume().is_err());
        // Frozen から freeze はできない(既に Frozen)。
        assert!(ThreadState::Frozen.freeze().is_err());
    }

    // --- Thread: T1(非 Active への書き込み拒否)------------------------------

    #[test]
    fn confirm_rejects_when_not_active() {
        let mut thread = sample_thread(10);
        thread.freeze().unwrap();
        let err = thread.confirm(sample_res("aa".repeat(32).as_str()), 1);
        assert_eq!(err, Err(ThreadError::NotActive));
    }

    #[test]
    fn confirm_rejects_when_closed() {
        let mut thread = sample_thread(10);
        thread.close().unwrap();
        let err = thread.confirm(sample_res("aa".repeat(32).as_str()), 1);
        assert_eq!(err, Err(ThreadError::NotActive));
    }

    #[test]
    fn check_writable_ok_only_when_active() {
        let mut thread = sample_thread(10);
        assert!(thread.check_writable().is_ok());
        thread.freeze().unwrap();
        assert!(thread.check_writable().is_err());
    }

    // --- Thread: T3(res_no 欠番なし単調増加・res_limit 超過拒否)--------------

    #[test]
    fn confirm_accepts_consecutive_res_no() {
        let mut thread = sample_thread(10);
        let id1 = "11".repeat(32);
        let id2 = "22".repeat(32);
        thread.confirm(sample_res(&id1), 1).unwrap();
        thread.confirm(sample_res(&id2), 2).unwrap();
        assert_eq!(thread.res.len(), 2);
        assert_eq!(thread.res[0].res_no, Some(1));
        assert_eq!(thread.res[1].res_no, Some(2));
        assert!(!thread.res[0].pending);
        assert_eq!(thread.next_res_no(), 3);
    }

    #[test]
    fn confirm_rejects_gap_in_res_no() {
        let mut thread = sample_thread(10);
        thread.confirm(sample_res(&"11".repeat(32)), 1).unwrap();
        let err = thread.confirm(sample_res(&"22".repeat(32)), 3);
        assert_eq!(
            err,
            Err(ThreadError::UnexpectedResNo {
                expected: 2,
                got: 3
            })
        );
    }

    #[test]
    fn confirm_rejects_duplicate_res_no() {
        let mut thread = sample_thread(10);
        thread.confirm(sample_res(&"11".repeat(32)), 1).unwrap();
        let err = thread.confirm(sample_res(&"22".repeat(32)), 1);
        assert_eq!(
            err,
            Err(ThreadError::UnexpectedResNo {
                expected: 2,
                got: 1
            })
        );
    }

    #[test]
    fn confirm_rejects_when_res_limit_exceeded() {
        let mut thread = sample_thread(2);
        thread.confirm(sample_res(&"11".repeat(32)), 1).unwrap();
        thread.confirm(sample_res(&"22".repeat(32)), 2).unwrap();
        let err = thread.confirm(sample_res(&"33".repeat(32)), 3);
        assert_eq!(err, Err(ThreadError::ResLimitExceeded));
    }

    // --- BoardSettings: Default / validate ------------------------------------

    #[test]
    fn board_settings_default_is_valid() {
        let s = BoardSettings::default();
        assert!(s.validate().is_ok());
        assert_eq!(s.res_limit, 1000);
        assert_eq!(s.noname_name, "名無しさん");
        assert_eq!(s.first_post_pow_bits, 20);
    }

    #[test]
    fn board_settings_rejects_title_too_long() {
        let s = BoardSettings {
            title: "あ".repeat(TITLE_MAX_CHARS + 1),
            ..Default::default()
        };
        assert_eq!(s.validate(), Err(BoardSettingsError::TitleTooLong));
    }

    #[test]
    fn board_settings_rejects_res_limit_out_of_range() {
        let too_low = BoardSettings {
            res_limit: RES_LIMIT_MIN - 1,
            ..Default::default()
        };
        assert_eq!(
            too_low.validate(),
            Err(BoardSettingsError::ResLimitOutOfRange)
        );
        let too_high = BoardSettings {
            res_limit: RES_LIMIT_MAX + 1,
            ..Default::default()
        };
        assert_eq!(
            too_high.validate(),
            Err(BoardSettingsError::ResLimitOutOfRange)
        );
        let boundary_low = BoardSettings {
            res_limit: RES_LIMIT_MIN,
            ..Default::default()
        };
        assert!(boundary_low.validate().is_ok());
        let boundary_high = BoardSettings {
            res_limit: RES_LIMIT_MAX,
            ..Default::default()
        };
        assert!(boundary_high.validate().is_ok());
    }

    #[test]
    fn board_settings_rejects_noname_name_out_of_range() {
        let empty = BoardSettings {
            noname_name: String::new(),
            ..Default::default()
        };
        assert_eq!(
            empty.validate(),
            Err(BoardSettingsError::NonameNameOutOfRange)
        );
        let too_long = BoardSettings {
            noname_name: "あ".repeat(NONAME_NAME_MAX_CHARS + 1),
            ..Default::default()
        };
        assert_eq!(
            too_long.validate(),
            Err(BoardSettingsError::NonameNameOutOfRange)
        );
    }

    #[test]
    fn board_settings_rejects_local_rules_too_long() {
        let s = BoardSettings {
            local_rules: "あ".repeat(LOCAL_RULES_MAX_CHARS + 1),
            ..Default::default()
        };
        assert_eq!(s.validate(), Err(BoardSettingsError::LocalRulesTooLong));
    }

    #[test]
    fn board_settings_rejects_pow_bits_out_of_range() {
        let s = BoardSettings {
            first_post_pow_bits: FIRST_POST_POW_BITS_MAX + 1,
            ..Default::default()
        };
        assert_eq!(s.validate(), Err(BoardSettingsError::PowBitsOutOfRange));
        let boundary = BoardSettings {
            first_post_pow_bits: FIRST_POST_POW_BITS_MAX,
            ..Default::default()
        };
        assert!(boundary.validate().is_ok());
    }

    #[test]
    fn board_settings_sanitized_strips_control_chars() {
        let s = BoardSettings {
            title: "タイトル\x07制御".to_string(),
            noname_name: "名無し\tさん".to_string(),
            local_rules: "ルール\x1f".to_string(),
            ..Default::default()
        };
        let sanitized = s.sanitized();
        assert_eq!(sanitized.title, "タイトル制御");
        assert_eq!(sanitized.noname_name, "名無しさん");
        assert_eq!(sanitized.local_rules, "ルール");
    }

    // --- BoardSettings <-> BoardSettingsRow 相互変換 ---------------------------

    #[test]
    fn board_settings_row_roundtrip() {
        let s = BoardSettings {
            title: "実況スレ板".to_string(),
            res_limit: 500,
            noname_name: "名無しさん".to_string(),
            local_rules: "荒らし禁止".to_string(),
            first_post_pow_bits: 16,
        };
        let board_id = board_id();
        let row = s.to_row(&board_id);
        assert_eq!(row.board_id, board_id);
        assert_eq!(row.res_limit, 500);
        assert_eq!(row.first_post_pow_bits, 16);
        let restored = BoardSettings::from_row(&row);
        assert_eq!(restored, s);
    }

    #[test]
    fn board_settings_from_row_falls_back_on_out_of_range() {
        let row = BoardSettingsRow {
            board_id: board_id(),
            title: "t".to_string(),
            res_limit: 99_999, // 範囲外
            noname_name: "n".to_string(),
            local_rules: String::new(),
            first_post_pow_bits: 200, // u8 範囲外
        };
        let restored = BoardSettings::from_row(&row);
        assert_eq!(restored.res_limit, RES_LIMIT_DEFAULT);
        assert_eq!(restored.first_post_pow_bits, FIRST_POST_POW_BITS_DEFAULT);
    }

    // --- アンカー `>>n` の解決(T031 — FR-009)------------------------------

    #[test]
    fn parse_anchors_extracts_res_numbers() {
        assert_eq!(parse_anchors(">>1 に返信"), vec![1]);
        assert_eq!(parse_anchors(">>152 を参照"), vec![152]);
        // 複数アンカー(出現順)。
        assert_eq!(parse_anchors(">>1 と >>23 と >>456"), vec![1, 23, 456]);
        // アンカーなし。
        assert!(parse_anchors("ただの本文").is_empty());
        // 単一 `>` は非アンカー。
        assert!(parse_anchors(">12 は引用").is_empty());
        // `>>0` は無効(採番は 1 始まり)。
        assert!(parse_anchors(">>0").is_empty());
        // 3 連 `>` の末尾 `>>` を拾う。
        assert_eq!(parse_anchors(">>>12"), vec![12]);
        // u16 溢れ(> 65535)は無効。
        assert!(parse_anchors(">>70000").is_empty());
    }

    #[test]
    fn resolve_anchor_returns_confirmed_res() {
        let mut thread = sample_thread(10);
        let id1 = "11".repeat(32);
        let id2 = "22".repeat(32);
        thread.confirm(sample_res(&id1), 1).unwrap();
        thread.confirm(sample_res(&id2), 2).unwrap();

        // res_no 1・2 は確定レスへ解決される(全端末で同じ event_id を指す — FR-009)。
        assert_eq!(thread.resolve_anchor(1).unwrap().event_id, id1);
        assert_eq!(thread.resolve_anchor(2).unwrap().event_id, id2);
        // 未確定・範囲外・0 は解決しない。
        assert!(thread.resolve_anchor(3).is_none());
        assert!(thread.resolve_anchor(0).is_none());
    }

    #[test]
    fn resolve_anchors_in_body() {
        let mut thread = sample_thread(10);
        let id1 = "11".repeat(32);
        let id2 = "22".repeat(32);
        thread.confirm(sample_res(&id1), 1).unwrap();
        thread.confirm(sample_res(&id2), 2).unwrap();

        // 本文中の >>1 >>2 >>9(未確定)を解決 → 確定分のみ返る。
        let resolved = thread.resolve_anchors_in(">>1 と >>2、それと >>9");
        assert_eq!(resolved.len(), 2, "確定済みの 2 件のみ解決される");
        assert_eq!(resolved[0].0, 1);
        assert_eq!(resolved[0].1.event_id, id1);
        assert_eq!(resolved[1].0, 2);
        assert_eq!(resolved[1].1.event_id, id2);
    }

    // --- T043: NG によるローカル非表示・欠番維持(FR-020)---------------------

    fn sample_res_with_key(event_id: &str, board_key: &str) -> Res {
        Res {
            event_id: event_id.to_string(),
            board_key: board_key.to_string(),
            name: None,
            mail: None,
            body: "本文".to_string(),
            created_at: 1_700_000_000,
            res_no: None,
            pending: false,
        }
    }

    #[test]
    fn visible_res_hides_ng_key_but_keeps_res_no_gap() {
        let mut thread = sample_thread(10);
        let key_a = "aa".repeat(32);
        let key_b = "bb".repeat(32);
        thread
            .confirm(sample_res_with_key(&"11".repeat(32), &key_a), 1)
            .unwrap();
        thread
            .confirm(sample_res_with_key(&"22".repeat(32), &key_b), 2)
            .unwrap();
        thread
            .confirm(sample_res_with_key(&"33".repeat(32), &key_a), 3)
            .unwrap();

        // key_b を NG にすると、可視列は res_no 1・3 のみ(2 は欠番のまま除外される)。
        let visible = thread.visible_res(|k| k == key_b);
        let visible_nos: Vec<u16> = visible.iter().filter_map(|r| r.res_no).collect();
        assert_eq!(
            visible_nos,
            vec![1, 3],
            "NG 対象は除外され res_no は詰めない"
        );

        // 全体の確定列自体は変わらない(NG は表示上のみ・採番は不変 — T3)。
        assert_eq!(thread.res.len(), 3);
    }

    #[test]
    fn visible_res_without_ng_returns_all() {
        let mut thread = sample_thread(10);
        thread
            .confirm(
                sample_res_with_key(&"11".repeat(32), "aa".repeat(32).as_str()),
                1,
            )
            .unwrap();
        let visible = thread.visible_res(|_| false);
        assert_eq!(visible.len(), 1);
    }
}

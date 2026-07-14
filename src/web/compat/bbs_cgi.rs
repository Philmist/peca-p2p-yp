//! bbs.cgi 書き込み処理(T056 — contracts/compat-api.md §POST /test/bbs.cgi)
//!
//! SJIS フォーム(`application/x-www-form-urlencoded`)を解析し、数値文字参照を展開して
//! **通常の書き込み経路と完全に同じ検証・採番(`LivechatRegistry::accept_write`)**へ渡す
//! (FR-028 — 互換 API だけの抜け道を作らない)。板鍵は自ノードの [`BoardKeyManager`] が
//! 自動管理し(なければ生成)、初回書き込みには [`crate::livechat::session::first_post_pow_bits`]
//! と同じ規則で PoW を計算する。
//!
//! ## 「ホストにとって初見か」の判定(レビュー対応)
//!
//! 当初は [`BoardKeyManager::existing_pubkey`] が `None`(ローカル未生成)であることを
//! もって「初回書き込み」とみなす近似を採っていたが、**板鍵ローテーション
//! (`BoardKeyManager::rotate` — T044)後に破綻する**ことが判明した: ローテーション後の
//! 新鍵はローカルには存在する(`existing_pubkey` が `Some`)ため PoW を計算しないが、
//! ホストの `known_board_keys` にとって新鍵は初見のため `accept_write` の PoW 検査
//! (検証 6)で `Rejected` になり、以後の書き込みが常に ERROR になっていた。
//!
//! 現在は [`LivechatRegistry::is_known_board_key`] で**ホスト側の実際の採番実績**を
//! 直接照会する(自ノードホスト前提のため直接照会が可能かつ正確)。近似を排除した
//! ため、ローテーション直後の書き込みも正しく PoW 付きで成功する。

use nostr::{Event, Keys};

use crate::livechat::board::BoardKeyManager;
use crate::livechat::registry::{AcceptOutcome, LivechatRegistry, RegistryError};
use crate::livechat::thread::BoardSettings;

use super::sjis;

/// bbs.cgi フォームの解析結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BbsForm {
    /// `bbs`(board_id)。
    pub bbs: String,
    /// `key`(スレ作成 unix 秒 = 対象スレの識別子)。
    pub key: Option<u64>,
    /// `FROM`(名前)。数値文字参照展開済み。
    pub from: Option<String>,
    /// `mail`。数値文字参照展開済み。
    pub mail: Option<String>,
    /// `MESSAGE`(本文)。数値文字参照展開済み。
    pub message: String,
    /// `subject`(スレ立て要求 — 存在すれば定型拒否 — FR-001)。
    pub subject: Option<String>,
}

/// bbs.cgi 処理のエラー(定型応答へ写像する — FR-030 内部情報非漏洩)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BbsCgiError {
    /// フォーム解析失敗(必須フィールド欠落・SJIS 解釈不能等)。
    MalformedForm,
    /// `subject` 付き(スレ立て)要求(FR-001 — スレ開設は配信者の明示操作のみ)。
    ThreadCreationNotAllowed,
    /// 未知の板(自ノードがホストしていない板を含む — リモート板は互換 API 非対応)。
    UnknownBoard,
    /// 対象スレが Active でない(Frozen/Closed — 書き込み不可)。
    ThreadNotActive,
    /// 板鍵の準備(生成・復号)に失敗した。
    BoardKeyUnavailable,
    /// イベント構築(署名)に失敗した(本文長超過等の形式違反を含む)。
    BuildFailed,
    /// ホスト側検証で採番拒否(BAN・PoW 不足・レート超過・満員等 — 理由は開示しない)。
    Rejected,
}

impl BbsCgiError {
    /// 従来クライアント向けの定型メッセージ(内部情報を含めない — FR-030 MUST NOT)。
    pub fn message(self) -> &'static str {
        match self {
            BbsCgiError::MalformedForm => "書き込みデータが不正です",
            BbsCgiError::ThreadCreationNotAllowed => "スレッドを立てることはできません",
            BbsCgiError::UnknownBoard => "書き込み先の板が見つかりません",
            BbsCgiError::ThreadNotActive => "このスレッドには書き込めません",
            BbsCgiError::BoardKeyUnavailable => "書き込み処理に失敗しました",
            BbsCgiError::BuildFailed => "書き込み内容を確認してください",
            BbsCgiError::Rejected => "書き込みに失敗しました",
        }
    }
}

/// SJIS URL エンコードされたフォームボディを解析する。
///
/// `application/x-www-form-urlencoded` の `key=value&key2=value2` 形式(SJIS バイト列)を
/// 想定する。値は percent-decode 後に SJIS → UTF-8 変換し、続けて数値文字参照
/// (`&#dddd;` / `&#xhhhh;`)を展開する(専ブラが SJIS 外の文字をこの形式で送る通例)。
///
/// `bbs`・`MESSAGE` は必須。両方揃わなければ [`BbsCgiError::MalformedForm`]。
pub fn parse_form(body: &[u8]) -> Result<BbsForm, BbsCgiError> {
    let mut bbs: Option<String> = None;
    let mut key: Option<u64> = None;
    let mut from: Option<String> = None;
    let mut mail: Option<String> = None;
    let mut message: Option<String> = None;
    let mut subject: Option<String> = None;

    for pair in body.split(|&b| b == b'&') {
        if pair.is_empty() {
            continue;
        }
        let Some(eq_pos) = pair.iter().position(|&b| b == b'=') else {
            continue; // 値なしフィールド(例: submit)は無視。
        };
        let (raw_key, raw_val) = pair.split_at(eq_pos);
        let raw_val = &raw_val[1..]; // '=' を除く。
        let Ok(key_str) = std::str::from_utf8(raw_key) else {
            continue;
        };
        let decoded_bytes = percent_decode(raw_val);
        let value_sjis = sjis::decode(&decoded_bytes);
        let value = sjis::decode_numeric_char_refs(&value_sjis);

        match key_str {
            "bbs" => bbs = Some(value),
            "key" => key = value.parse().ok(),
            "FROM" => from = Some(value),
            "mail" => mail = Some(value),
            "MESSAGE" => message = Some(value),
            "subject" => subject = Some(value),
            _ => {} // time・submit 等の未使用フィールドは無視(前方互換)。
        }
    }

    let bbs = bbs
        .filter(|s| !s.is_empty())
        .ok_or(BbsCgiError::MalformedForm)?;
    let message = message.ok_or(BbsCgiError::MalformedForm)?;

    Ok(BbsForm {
        bbs,
        key,
        from,
        mail,
        message,
        subject,
    })
}

/// percent-decode(`%XX` を 1 バイトへ、`+` を 0x20(半角スペース)へ)。
///
/// `application/x-www-form-urlencoded` の慣習に従う。不正な `%XX`(16 進でない・末尾で
/// 切れている)はバイトをそのまま素通りする(壊れた入力で全体を失敗させない)。
fn percent_decode(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        match input[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < input.len() => {
                let hex = std::str::from_utf8(&input[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    None => {
                        out.push(input[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    out
}

/// フォームを検証・署名し、通常経路(`accept_write`)へ渡す(T056 — FR-028)。
///
/// 手順:
/// 1. `subject` 付き(スレ立て)は定型拒否(FR-001)。
/// 2. 対象板(`registry`)が Active スレを持つことを確認する(自ノードホストの板のみ —
///    リモート板は互換 API の対象外。§設計判断参照)。
/// 3. 板鍵を [`BoardKeyManager::signing_keys`] で取得(未生成なら自動生成)。
///    [`LivechatRegistry::is_known_board_key`] でホスト側の採番実績を直接照会し、
///    未知(初回・ローテーション直後)なら `first_post_pow_bits` を満たす PoW を計算する。
/// 4. 名前欄の `#` 以降除去は [`crate::event::livechat::Res::sign`] が担う(FR-024 二重防御
///    ではなく単一防御 — 送信前除去がこの関数の責務)。
/// 5. **通常経路と同一の検証・採番**: `registry.accept_write` をそのまま呼ぶ(封筒署名検証
///    込み — `verify_incoming_res` 相当の検証は `accept_write` 内の対象スレ一致・状態確認が
///    代替する。署名は本関数が組み立てた直後のためなりすましの余地がない)。
pub fn submit(
    registry: &LivechatRegistry,
    board_keys: &BoardKeyManager,
    form: &BbsForm,
    created_at: u64,
) -> Result<AcceptOutcome, BbsCgiError> {
    if form.subject.is_some() {
        return Err(BbsCgiError::ThreadCreationNotAllowed);
    }

    let snapshot = registry
        .board_snapshot(&form.bbs)
        .ok_or(BbsCgiError::UnknownBoard)?;
    // 対象スレの key が指定されていればアクティブスレの key と一致することを要求する
    // (専ブラは通常アクティブスレの key で書き込む。凍結・旧世代の key 指定は「書き込み
    // 不可」として扱う — Frozen/Closed への書き込み拒否と同じ意味論)。
    if let Some(requested_key) = form.key
        && requested_key != snapshot.active.key
    {
        return Err(BbsCgiError::ThreadNotActive);
    }
    if snapshot.active.state != crate::livechat::thread::ThreadState::Active {
        return Err(BbsCgiError::ThreadNotActive);
    }

    let channel = snapshot.active.channel.clone();
    let generation = snapshot.active.generation;

    // 板鍵を取得(未生成なら自動生成 — FR-016)。
    let keys = board_keys
        .signing_keys(&form.bbs)
        .map_err(|_| BbsCgiError::BoardKeyUnavailable)?;
    // 「ホストにとって初見か」はホスト側の採番実績を直接照会する(レビュー対応 —
    // ローカル生成有無による近似は板鍵ローテーション後に破綻するため使わない)。
    let board_key_hex = keys.public_key().to_hex();
    let is_first_post = !registry.is_known_board_key(&form.bbs, &board_key_hex);
    let pow_bits = crate::livechat::session::first_post_pow_bits(
        &BoardSettings {
            first_post_pow_bits: pow_bits_for(&snapshot.settings, is_first_post),
            ..Default::default()
        },
        is_first_post,
    );

    let event = sign_res(
        &keys,
        &channel,
        &form.bbs,
        generation,
        form.from.as_deref(),
        form.mail.as_deref(),
        &form.message,
        created_at,
        pow_bits,
    )
    .map_err(|_| BbsCgiError::BuildFailed)?;

    registry
        .accept_write(&form.bbs, &event, created_at)
        .map_err(map_registry_error)
}

/// [`BoardSettings::first_post_pow_bits`] をそのまま返す薄いヘルパ(可読性のための命名)。
fn pow_bits_for(settings: &BoardSettings, _is_first_post: bool) -> u8 {
    settings.first_post_pow_bits
}

/// kind 1311 イベントを署名する薄いラッパ([`crate::event::livechat::Res::sign`] への委譲)。
#[allow(clippy::too_many_arguments)]
fn sign_res(
    keys: &Keys,
    channel: &str,
    board_id: &str,
    generation: u32,
    name: Option<&str>,
    mail: Option<&str>,
    body: &str,
    created_at: u64,
    pow_bits: u8,
) -> Result<Event, crate::event::livechat::LivechatBuildError> {
    crate::event::livechat::Res {
        channel: channel.to_string(),
        board_id: board_id.to_string(),
        generation,
        name: name.map(str::to_string),
        mail: mail.map(str::to_string),
        body: body.to_string(),
    }
    .sign(keys, created_at, pow_bits)
}

/// [`RegistryError`] を [`BbsCgiError`] へ写像する(内部情報を含めない)。
fn map_registry_error(err: RegistryError) -> BbsCgiError {
    match err {
        RegistryError::UnknownBoard => BbsCgiError::UnknownBoard,
        RegistryError::Confirm(_) => BbsCgiError::ThreadNotActive,
        RegistryError::Build(_) => BbsCgiError::BuildFailed,
        RegistryError::BoardIdMismatch | RegistryError::InvalidSettings(_) => BbsCgiError::Rejected,
    }
}

// ---------------------------------------------------------------------------
// 応答ページ(HTML)
// ---------------------------------------------------------------------------

/// 成功応答ページ(従来形式)。`<title>書きこみました。</title>` を含む。
pub fn success_page() -> String {
    "<html><head><title>書きこみました。</title></head><body>書きこみました。</body></html>"
        .to_string()
}

/// エラー応答ページ(従来形式)。`<title>ERROR!</title>` + `ERROR:<定型メッセージ>`
/// (FR-030 — 内部情報を含めない)。
pub fn error_page(err: BbsCgiError) -> String {
    format!(
        "<html><head><title>ERROR!</title></head><body>ERROR:{}</body></html>",
        err.message()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::livechat::thread::BoardSettings;
    use nostr::JsonUtil;

    const GUID: &str = "0123456789abcdef0123456789abcdef";

    fn channel_of(board_id: &str) -> String {
        format!("30311:{board_id}:{GUID}")
    }

    fn open_board(persona: &Keys) -> std::sync::Arc<LivechatRegistry> {
        let reg = LivechatRegistry::new(128);
        let board_id = persona.public_key().to_hex();
        reg.open_thread(
            persona.clone(),
            channel_of(&board_id),
            1,
            1_700_000_000,
            "実況スレ",
            BoardSettings {
                first_post_pow_bits: 0,
                ..Default::default()
            },
            "198.51.100.1:7147",
        )
        .unwrap();
        reg
    }

    fn key_manager() -> BoardKeyManager {
        BoardKeyManager::new(
            std::sync::Arc::new(crate::store::Store::open_in_memory().unwrap()),
            crate::identity::Keystore::ephemeral(),
        )
    }

    // --- フォーム解析 ------------------------------------------------------------

    #[test]
    fn parse_form_extracts_basic_fields() {
        let body = b"bbs=abc123&key=1700000000&FROM=%96%BC%91O&mail=sage&MESSAGE=%96%7B%95%B6";
        let form = parse_form(body).unwrap();
        assert_eq!(form.bbs, "abc123");
        assert_eq!(form.key, Some(1_700_000_000));
        assert_eq!(form.mail.as_deref(), Some("sage"));
    }

    #[test]
    fn parse_form_decodes_sjis_percent_encoded_values() {
        // "名前" の SJIS バイト列を percent エンコードした値を復号できる。
        let sjis_bytes = sjis::encode("名前");
        let percent_encoded: String = sjis_bytes.iter().map(|b| format!("%{b:02X}")).collect();
        let body = format!("bbs=x&MESSAGE=test&FROM={percent_encoded}");
        let form = parse_form(body.as_bytes()).unwrap();
        assert_eq!(form.from.as_deref(), Some("名前"));
    }

    #[test]
    fn parse_form_expands_numeric_char_refs_in_message() {
        let body = b"bbs=x&MESSAGE=a%26%23128640%3Bb"; // "a&#128640;b" percent-encoded
        let form = parse_form(body).unwrap();
        assert_eq!(form.message, "a🚀b");
    }

    #[test]
    fn parse_form_detects_subject_for_thread_creation() {
        let body = b"bbs=x&MESSAGE=test&subject=new+thread";
        let form = parse_form(body).unwrap();
        assert_eq!(form.subject.as_deref(), Some("new thread"));
    }

    #[test]
    fn parse_form_missing_bbs_is_malformed() {
        let body = b"MESSAGE=test";
        assert_eq!(parse_form(body), Err(BbsCgiError::MalformedForm));
    }

    #[test]
    fn parse_form_missing_message_is_malformed() {
        let body = b"bbs=x";
        assert_eq!(parse_form(body), Err(BbsCgiError::MalformedForm));
    }

    #[test]
    fn parse_form_plus_becomes_space() {
        let body = b"bbs=x&MESSAGE=hello+world";
        let form = parse_form(body).unwrap();
        assert_eq!(form.message, "hello world");
    }

    // --- submit(通常経路と同一検証)--------------------------------------------

    #[test]
    fn submit_signs_and_numbers_via_normal_path() {
        let persona = Keys::generate();
        let board_id = persona.public_key().to_hex();
        let reg = open_board(&persona);
        let km = key_manager();
        let form = BbsForm {
            bbs: board_id.clone(),
            key: Some(1_700_000_000),
            from: Some("名無し".to_string()),
            mail: None,
            message: "テスト投稿".to_string(),
            subject: None,
        };
        let outcome = submit(&reg, &km, &form, 1_700_000_010).unwrap();
        assert!(matches!(outcome, AcceptOutcome::Numbered { res_no: 1, .. }));
    }

    #[test]
    fn submit_rejects_subject_thread_creation() {
        let persona = Keys::generate();
        let board_id = persona.public_key().to_hex();
        let reg = open_board(&persona);
        let km = key_manager();
        let form = BbsForm {
            bbs: board_id,
            key: None,
            from: None,
            mail: None,
            message: "本文".to_string(),
            subject: Some("新スレ".to_string()),
        };
        assert_eq!(
            submit(&reg, &km, &form, 1_700_000_010),
            Err(BbsCgiError::ThreadCreationNotAllowed)
        );
    }

    #[test]
    fn submit_unknown_board_is_rejected() {
        let reg = LivechatRegistry::new(128);
        let km = key_manager();
        let form = BbsForm {
            bbs: "ab".repeat(32),
            key: None,
            from: None,
            mail: None,
            message: "本文".to_string(),
            subject: None,
        };
        assert_eq!(
            submit(&reg, &km, &form, 1_700_000_010),
            Err(BbsCgiError::UnknownBoard)
        );
    }

    #[test]
    fn submit_reuses_existing_board_key_across_calls() {
        let persona = Keys::generate();
        let board_id = persona.public_key().to_hex();
        let reg = open_board(&persona);
        let km = key_manager();
        let form = BbsForm {
            bbs: board_id.clone(),
            key: Some(1_700_000_000),
            from: None,
            mail: None,
            message: "1 件目".to_string(),
            subject: None,
        };
        submit(&reg, &km, &form, 1_700_000_010).unwrap();
        let pubkey_after_first = km.existing_pubkey(&board_id).unwrap().unwrap();

        let form2 = BbsForm {
            message: "2 件目".to_string(),
            ..form
        };
        submit(&reg, &km, &form2, 1_700_000_020).unwrap();
        let pubkey_after_second = km.existing_pubkey(&board_id).unwrap().unwrap();
        assert_eq!(
            pubkey_after_first, pubkey_after_second,
            "同一板への連続書き込みは同じ板鍵を再利用する"
        );
    }

    #[test]
    fn submit_strips_trip_from_name() {
        let persona = Keys::generate();
        let board_id = persona.public_key().to_hex();
        let reg = open_board(&persona);
        let km = key_manager();
        let form = BbsForm {
            bbs: board_id.clone(),
            key: Some(1_700_000_000),
            from: Some("コテハン#ひみつ".to_string()),
            mail: None,
            message: "本文".to_string(),
            subject: None,
        };
        submit(&reg, &km, &form, 1_700_000_010).unwrap();
        let snapshot = reg.board_snapshot(&board_id).unwrap();
        assert_eq!(
            snapshot.active.res[0].name.as_deref(),
            Some("コテハン"),
            "# 以降は送信前に除去される(FR-024)"
        );
    }

    #[test]
    fn submit_requires_pow_for_first_post_with_new_key() {
        let persona = Keys::generate();
        let board_id = persona.public_key().to_hex();
        let reg = LivechatRegistry::new_with_rate(128, 10_000);
        reg.open_thread(
            persona.clone(),
            channel_of(&board_id),
            1,
            1_700_000_000,
            "実況スレ",
            BoardSettings {
                first_post_pow_bits: 8,
                ..Default::default()
            },
            "198.51.100.1:7147",
        )
        .unwrap();
        let km = key_manager();
        let form = BbsForm {
            bbs: board_id.clone(),
            key: Some(1_700_000_000),
            from: None,
            mail: None,
            message: "初回投稿".to_string(),
            subject: None,
        };
        // submit は registry.is_known_board_key で初見と判定し PoW を自動計算するため、
        // first_post_pow_bits=8 でも Numbered になる(PoW 計算コストは払うが失敗しない)。
        let outcome = submit(&reg, &km, &form, 1_700_000_010).unwrap();
        assert!(matches!(outcome, AcceptOutcome::Numbered { .. }));
    }

    #[test]
    fn submit_succeeds_with_pow_after_board_key_rotation() {
        // T056 レビュー対応の回帰テスト: 「1 回書き込み → rotate → 再度書き込み」が
        // PoW 計算込みで成功することを確認する。当初の近似
        // (BoardKeyManager::existing_pubkey が Some かどうか)ではローテーション後の
        // 新鍵はローカルに存在するため PoW を計算せず、ホストの is_known_board_key では
        // 未知のため accept_write の PoW 検査で Rejected になり続ける不具合があった。
        let persona = Keys::generate();
        let board_id = persona.public_key().to_hex();
        let reg = LivechatRegistry::new_with_rate(128, 10_000);
        reg.open_thread(
            persona.clone(),
            channel_of(&board_id),
            1,
            1_700_000_000,
            "実況スレ",
            BoardSettings {
                first_post_pow_bits: 8,
                ..Default::default()
            },
            "198.51.100.1:7147",
        )
        .unwrap();
        let km = key_manager();

        // 1 回目の書き込み(初回 PoW 込みで成功)。
        let form1 = BbsForm {
            bbs: board_id.clone(),
            key: Some(1_700_000_000),
            from: None,
            mail: None,
            message: "1 回目".to_string(),
            subject: None,
        };
        let outcome1 = submit(&reg, &km, &form1, 1_700_000_010).unwrap();
        assert!(matches!(outcome1, AcceptOutcome::Numbered { .. }));
        let old_pubkey = km.existing_pubkey(&board_id).unwrap().unwrap();
        assert!(
            reg.is_known_board_key(&board_id, &old_pubkey),
            "1 回目の書き込みでホストにとって既知になる"
        );

        // 板鍵をローテーションする(明示操作 — FR-017)。
        let rotated = km.rotate(&board_id).unwrap();
        let new_pubkey = rotated.public_key().to_hex();
        assert_ne!(old_pubkey, new_pubkey, "ローテーションで鍵が変わる");
        assert!(
            !reg.is_known_board_key(&board_id, &new_pubkey),
            "ローテーション直後の新鍵はホストにとって未知(初見)"
        );

        // 2 回目の書き込み(ローテーション後 = ホストにとって新規初見 = PoW が必要)。
        // 近似が破綻していれば PoW を計算せず Rejected になっていたはずの箇所。
        let form2 = BbsForm {
            bbs: board_id.clone(),
            key: Some(1_700_000_000),
            from: None,
            mail: None,
            message: "ローテーション後の 2 回目".to_string(),
            subject: None,
        };
        let outcome2 = submit(&reg, &km, &form2, 1_700_000_020).unwrap();
        assert!(
            matches!(outcome2, AcceptOutcome::Numbered { .. }),
            "ローテーション後も PoW を計算して成功するべき: {outcome2:?}"
        );
    }

    #[test]
    fn submit_rejects_when_thread_key_mismatches_active() {
        let persona = Keys::generate();
        let board_id = persona.public_key().to_hex();
        let reg = open_board(&persona);
        let km = key_manager();
        let form = BbsForm {
            bbs: board_id,
            key: Some(1_699_999_999), // アクティブスレの key と不一致。
            from: None,
            mail: None,
            message: "本文".to_string(),
            subject: None,
        };
        assert_eq!(
            submit(&reg, &km, &form, 1_700_000_010),
            Err(BbsCgiError::ThreadNotActive)
        );
    }

    #[test]
    fn submit_rejects_when_thread_closed() {
        let persona = Keys::generate();
        let board_id = persona.public_key().to_hex();
        let reg = open_board(&persona);
        reg.close_thread(&board_id, 1_700_000_500).unwrap();
        let km = key_manager();
        let form = BbsForm {
            bbs: board_id,
            key: Some(1_700_000_000),
            from: None,
            mail: None,
            message: "本文".to_string(),
            subject: None,
        };
        assert_eq!(
            submit(&reg, &km, &form, 1_700_000_600),
            Err(BbsCgiError::ThreadNotActive)
        );
    }

    // --- 応答ページ --------------------------------------------------------------

    #[test]
    fn success_page_contains_expected_title() {
        assert!(success_page().contains("<title>書きこみました。</title>"));
    }

    #[test]
    fn error_page_contains_expected_title_and_message() {
        let page = error_page(BbsCgiError::ThreadNotActive);
        assert!(page.contains("<title>ERROR!</title>"));
        assert!(page.contains("ERROR:このスレッドには書き込めません"));
    }

    #[test]
    fn error_page_never_leaks_internal_details() {
        // すべてのエラーバリアントで内部情報(パス・スタックトレース等)を含まないことを
        // 網羅的に確認する(FR-030 MUST NOT)。
        let variants = [
            BbsCgiError::MalformedForm,
            BbsCgiError::ThreadCreationNotAllowed,
            BbsCgiError::UnknownBoard,
            BbsCgiError::ThreadNotActive,
            BbsCgiError::BoardKeyUnavailable,
            BbsCgiError::BuildFailed,
            BbsCgiError::Rejected,
        ];
        for v in variants {
            let page = error_page(v);
            assert!(!page.contains("panic"));
            assert!(!page.contains(".rs:"));
            assert!(!page.to_lowercase().contains("ban"));
        }
    }

    // --- percent デコード ---------------------------------------------------------

    #[test]
    fn percent_decode_handles_plus_and_hex() {
        assert_eq!(percent_decode(b"a+b%20c"), b"a b c");
    }

    #[test]
    fn percent_decode_tolerates_malformed_sequences() {
        assert_eq!(percent_decode(b"a%zzb"), b"a%zzb");
    }

    #[test]
    fn event_roundtrips_json() {
        // sign_res が生成するイベントが JSON 往復可能であることの疎通確認。
        let keys = Keys::generate();
        let board_id = keys.public_key().to_hex();
        let event = sign_res(
            &keys,
            &channel_of(&board_id),
            &board_id,
            1,
            None,
            None,
            "本文",
            1_700_000_000,
            0,
        )
        .unwrap();
        let raw = event.as_json();
        assert!(Event::from_json(&raw).is_ok());
    }
}

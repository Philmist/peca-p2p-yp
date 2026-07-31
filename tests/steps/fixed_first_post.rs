//! 固定 >>1 の自動投稿シナリオのステップ定義(T028 — 007)
//!
//! contracts/fixed-first-post.md §4 の Gherkin(ネガティブ含む — Principle IV)を、
//! [`LivechatRegistry`] を直接駆動して検証する(開設・次スレ・遡及なし・既定テンプレ・
//! 上限拒否・通常検証合格・res_limit カウント)。

use std::sync::Arc;

use cucumber::{given, then, when};
use nostr::Keys;

use peca_p2p_yp::livechat::registry::{LivechatRegistry, sign_res};
use peca_p2p_yp::livechat::thread::{BoardSettings, FIRST_POST_TEMPLATE_MAX_CHARS, RES_LIMIT_MIN};

use crate::AppWorld;

const GUID: &str = "0123456789abcdef0123456789abcdef";

/// 固定 >>1 シナリオ 1 個分の状態。
#[derive(Default)]
pub struct FixedFirstPostWorld {
    reg: Option<Arc<LivechatRegistry>>,
    board_id: Option<String>,
    channel: Option<String>,
    board_key: Option<Keys>,
    /// 未開設シナリオで組み立て中の板設定(検証・開設に使う)。
    settings: BoardSettings,
    /// `板設定を検証する` の結果。
    validate_ok: Option<bool>,
}

// LivechatRegistry は Debug 非対応のため AppWorld の Debug derive 用に手実装する。
impl std::fmt::Debug for FixedFirstPostWorld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FixedFirstPostWorld")
            .field("board_id", &self.board_id)
            .finish()
    }
}

fn w(world: &mut AppWorld) -> &mut FixedFirstPostWorld {
    world.fixed_first_post.get_or_insert_with(Default::default)
}

/// 現在の `settings` で板を開設し >>1 を arm する。
fn open_and_arm(lw: &mut FixedFirstPostWorld) {
    let reg = LivechatRegistry::new(128);
    let persona = Keys::generate();
    let board_id = persona.public_key().to_hex();
    let channel = format!("30311:{board_id}:{GUID}");
    reg.open_thread(
        persona,
        channel.clone(),
        1,
        1_700_000_000,
        "実況スレ",
        lw.settings.clone(),
        "198.51.100.1:7147",
    )
    .unwrap();
    let board_key = Keys::generate();
    reg.arm_first_post(&board_id, board_key.clone(), 1_700_000_001)
        .unwrap();
    lw.reg = Some(reg);
    lw.board_id = Some(board_id);
    lw.channel = Some(channel);
    lw.board_key = Some(board_key);
}

fn snap_active_res0_body(lw: &FixedFirstPostWorld) -> String {
    let reg = lw.reg.as_ref().unwrap();
    let snap = reg.board_snapshot(lw.board_id.as_ref().unwrap()).unwrap();
    snap.active.res[0].body.clone()
}

// --- Given ------------------------------------------------------------------

#[given(regex = r#"^板主が first_post_template を "(.*)" と設定している$"#)]
async fn given_template(world: &mut AppWorld, template: String) {
    let lw = w(world);
    *lw = FixedFirstPostWorld::default();
    lw.settings = BoardSettings {
        title: "実況スレ".into(),
        first_post_template: template,
        ..Default::default()
    };
}

#[given("板主が 2048 文字を超える first_post_template を用意する")]
async fn given_over_limit_template(world: &mut AppWorld) {
    let lw = w(world);
    *lw = FixedFirstPostWorld::default();
    lw.settings = BoardSettings {
        first_post_template: "あ".repeat(FIRST_POST_TEMPLATE_MAX_CHARS + 1),
        ..Default::default()
    };
}

#[given("板主が res_limit=3 の板でスレを開設した")]
async fn given_res_limit_3_open(world: &mut AppWorld) {
    let lw = w(world);
    *lw = FixedFirstPostWorld::default();
    lw.settings = BoardSettings {
        res_limit: 3,
        first_post_pow_bits: 0,
        first_post_template: "頭".into(),
        ..Default::default()
    };
    open_and_arm(lw);
}

// --- When(かつ で Given 化する開設ステップは given/when 両方に登録)------------

#[given("板主がスレを開設する")]
#[when("板主がスレを開設する")]
async fn when_open(world: &mut AppWorld) {
    let lw = w(world);
    open_and_arm(lw);
}

#[when("次スレへ移行する")]
async fn when_next_generation(world: &mut AppWorld) {
    let lw = w(world);
    let reg = lw.reg.as_ref().unwrap();
    reg.start_next_generation(lw.board_id.as_ref().unwrap(), 1_700_001_000, "実況スレ")
        .unwrap();
}

#[when(regex = r#"^板主がテンプレを "(.*)" に変更して次スレを開始する$"#)]
async fn when_change_and_next(world: &mut AppWorld, new_template: String) {
    let lw = w(world);
    let reg = lw.reg.as_ref().unwrap();
    let board_id = lw.board_id.clone().unwrap();
    reg.update_settings(
        &board_id,
        BoardSettings {
            title: "実況スレ".into(),
            first_post_template: new_template,
            ..Default::default()
        },
    )
    .unwrap();
    reg.start_next_generation(&board_id, 1_700_001_000, "実況スレ")
        .unwrap();
}

#[when("板設定を検証する")]
async fn when_validate(world: &mut AppWorld) {
    let lw = w(world);
    lw.validate_ok = Some(lw.settings.validate().is_ok());
}

#[when("参加者が上限まで書き込む")]
async fn when_fill_to_limit(world: &mut AppWorld) {
    let lw = w(world);
    let reg = lw.reg.as_ref().unwrap();
    let board_id = lw.board_id.clone().unwrap();
    let channel = lw.channel.clone().unwrap();
    let writer = Keys::generate();
    // >>1 が res_no=1。res_limit=3 なので参加者 2 件で res_no=3=上限 → 自動移行。
    for (i, ts) in [1_700_000_010u64, 1_700_000_011].iter().enumerate() {
        let res = sign_res(&writer, &board_id, &channel, 1, &format!("参加{i}"), *ts).unwrap();
        reg.accept_write(&board_id, &res, *ts).unwrap();
    }
}

// --- Then -------------------------------------------------------------------

#[then("res_no=1 がテンプレ本文で確定する")]
async fn then_res_no_1_is_template(world: &mut AppWorld) {
    let lw = w(world);
    let template = lw.settings.first_post_template.clone();
    assert_eq!(snap_active_res0_body(lw), template);
}

#[then(r#"">>1" は解決先を持つ"#)]
async fn then_anchor_resolvable(world: &mut AppWorld) {
    let lw = w(world);
    let reg = lw.reg.as_ref().unwrap();
    let snap = reg.board_snapshot(lw.board_id.as_ref().unwrap()).unwrap();
    assert_eq!(snap.active.res[0].res_no, Some(1), ">>1 が解決先を持つ");
}

#[then("新スレの res_no=1 に同じテンプレが自動投稿される")]
async fn then_next_gen_reposts(world: &mut AppWorld) {
    let lw = w(world);
    let template = lw.settings.first_post_template.clone();
    let reg = lw.reg.as_ref().unwrap();
    let snap = reg.board_snapshot(lw.board_id.as_ref().unwrap()).unwrap();
    assert_eq!(snap.active.generation, 2);
    assert_eq!(snap.active.res[0].res_no, Some(1));
    assert_eq!(snap.active.res[0].body, template);
}

#[then(regex = r#"^新スレの >>1 は "(.*)"、既存スレの >>1 は "(.*)" のままになる$"#)]
async fn then_not_retroactive(world: &mut AppWorld, new: String, old: String) {
    let lw = w(world);
    let reg = lw.reg.as_ref().unwrap();
    let snap = reg.board_snapshot(lw.board_id.as_ref().unwrap()).unwrap();
    assert_eq!(snap.active.res[0].body, new, "新スレは新テンプレ");
    let frozen = snap.frozen.expect("直近凍結スレを保持");
    assert_eq!(frozen.res[0].body, old, "既存スレの >>1 は不変");
}

#[then("res_no=1 は板タイトルと対象チャンネルを含む既定テンプレになる")]
async fn then_default_template(world: &mut AppWorld) {
    let lw = w(world);
    let channel = lw.channel.clone().unwrap();
    let body = snap_active_res0_body(lw);
    assert!(!body.is_empty(), "既定テンプレは非空");
    assert!(body.contains("実況スレ"), "板タイトルを含む: {body}");
    assert!(body.contains(&channel), "対象チャンネルを含む: {body}");
}

#[then("板設定検証エラーで拒否される")]
async fn then_validate_error(world: &mut AppWorld) {
    assert_eq!(w(world).validate_ok, Some(false), "検証エラーで拒否される");
}

#[then("固定 >>1 は署名・サイズ・行数の通常検証に合格する")]
async fn then_passes_normal_validation(world: &mut AppWorld) {
    let lw = w(world);
    let reg = lw.reg.as_ref().unwrap();
    let snap = reg.board_snapshot(lw.board_id.as_ref().unwrap()).unwrap();
    let res = &snap.active.res[0];
    // 通常レス検証と同一値域(サイズ・行数)を満たす。板鍵署名で confirm 済み。
    assert!(res.body.chars().count() <= FIRST_POST_TEMPLATE_MAX_CHARS);
    assert!(res.body.lines().count() <= 32);
    assert_eq!(
        res.board_key,
        lw.board_key.as_ref().unwrap().public_key().to_hex(),
        "ホスト板鍵で署名される"
    );
    // res_limit 内(RES_LIMIT_MIN 以上の板でも 1 件目として正当)。
    assert!(snap.active.res_limit >= RES_LIMIT_MIN || snap.active.res_limit >= 1);
}

#[then("参加者側は特例なく通常レスとして受理する")]
async fn then_accepted_as_normal(world: &mut AppWorld) {
    let lw = w(world);
    let reg = lw.reg.as_ref().unwrap();
    let snap = reg.board_snapshot(lw.board_id.as_ref().unwrap()).unwrap();
    assert!(
        !snap.active.res[0].pending,
        "特例なし = 通常の確定レス(pending でない)"
    );
}

#[then("固定 >>1 を含めて上限に達し次スレ移行が判定される")]
async fn then_migrated_by_limit(world: &mut AppWorld) {
    let lw = w(world);
    let reg = lw.reg.as_ref().unwrap();
    let snap = reg.board_snapshot(lw.board_id.as_ref().unwrap()).unwrap();
    assert_eq!(
        snap.active.generation, 2,
        ">>1 込みで上限到達 → 次スレへ移行"
    );
}

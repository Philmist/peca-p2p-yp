//! Web UI の安全描画シナリオのステップ定義(T039 — 007 / contracts/web-ui.md §4)
//!
//! サーバ側の応答経路([`BoardSettingsView`] / [`ResView`] / registry の固定 >>1)を直接
//! 駆動し、FR-005 を統合面から固定する:
//! - ローカルルール**のみ**サーバ側で安全 HTML 化(`local_rules_html`)される。
//! - レス本文・名前・固定 >>1 テンプレはサーバ側で HTML 化されず**プレーンテキストのまま
//!   往復**する(装飾・エスケープはクライアント側描画の責務 — 実機確認 T012)。

use nostr::Keys;

use peca_p2p_yp::livechat::registry::LivechatRegistry;
use peca_p2p_yp::livechat::thread::{BoardSettings, Res};
use peca_p2p_yp::web::livechat::{BoardSettingsView, ResView};

use cucumber::{given, then, when};

use crate::AppWorld;

const GUID: &str = "0123456789abcdef0123456789abcdef";

/// 安全描画シナリオ 1 個分の状態。
#[derive(Debug, Default)]
pub struct SafeRenderingWorld {
    settings: Option<BoardSettings>,
    settings_view: Option<BoardSettingsView>,
    res: Option<Res>,
    res_view: Option<ResView>,
    /// 固定 >>1 の本文(registry から取得した確定 res_no=1 の body)。
    first_post_body: Option<String>,
}

fn w(world: &mut AppWorld) -> &mut SafeRenderingWorld {
    world.safe_rendering.get_or_insert_with(Default::default)
}

fn sample_res(name: Option<&str>, body: &str) -> Res {
    Res {
        event_id: "11".repeat(32),
        board_key: "22".repeat(32),
        name: name.map(str::to_string),
        mail: None,
        body: body.to_string(),
        created_at: 1_700_000_000,
        res_no: Some(2),
        pending: false,
    }
}

// --- Given ------------------------------------------------------------------

#[given(regex = r#"^ローカルルールに "(.*)" を含む板設定がある$"#)]
async fn given_local_rules(world: &mut AppWorld, rules: String) {
    let lw = w(world);
    *lw = SafeRenderingWorld::default();
    lw.settings = Some(BoardSettings {
        local_rules: rules,
        ..Default::default()
    });
}

#[given(regex = r#"^本文に "(.*)" を含む確定レスがある$"#)]
async fn given_res_body(world: &mut AppWorld, body: String) {
    let lw = w(world);
    *lw = SafeRenderingWorld::default();
    lw.res = Some(sample_res(None, &body));
}

#[given(regex = r#"^名前に "(.*)" を含む確定レスがある$"#)]
async fn given_res_name(world: &mut AppWorld, name: String) {
    let lw = w(world);
    *lw = SafeRenderingWorld::default();
    lw.res = Some(sample_res(Some(&name), "本文"));
}

#[given(regex = r#"^固定 >>1 テンプレに "(.*)" を含む板でスレを開設した$"#)]
async fn given_first_post_template(world: &mut AppWorld, template: String) {
    let lw = w(world);
    *lw = SafeRenderingWorld::default();
    let reg = LivechatRegistry::new(128);
    let persona = Keys::generate();
    let board_id = persona.public_key().to_hex();
    let channel = format!("30311:{board_id}:{GUID}");
    reg.open_thread(
        persona,
        channel,
        1,
        1_700_000_000,
        "実況スレ",
        BoardSettings {
            first_post_template: template,
            ..Default::default()
        },
        "198.51.100.1:7147",
    )
    .unwrap();
    reg.arm_first_post(&board_id, Keys::generate(), None, 1_700_000_001)
        .unwrap();
    let snap = reg.board_snapshot(&board_id).unwrap();
    lw.first_post_body = Some(snap.active.res[0].body.clone());
}

// --- When -------------------------------------------------------------------

#[when("板設定ビューを生成する")]
async fn when_build_settings_view(world: &mut AppWorld) {
    let lw = w(world);
    let s = lw.settings.clone().unwrap();
    lw.settings_view = Some(BoardSettingsView::from_settings(&s));
}

#[when("レスビューを生成する")]
async fn when_build_res_view(world: &mut AppWorld) {
    let lw = w(world);
    let res = lw.res.clone().unwrap();
    lw.res_view = Some(ResView::from_res(&res, "名無しさん").unwrap());
}

#[when("固定 >>1 を取得する")]
async fn when_get_first_post(_world: &mut AppWorld) {
    // Given で取得済み(first_post_body)。ここでは追加操作なし。
}

// --- Then -------------------------------------------------------------------

#[then(regex = r#"^local_rules_html に "(.*)" が含まれない$"#)]
async fn then_html_excludes(world: &mut AppWorld, needle: String) {
    let lw = w(world);
    let view = lw.settings_view.as_ref().unwrap();
    assert!(
        !view.local_rules_html.contains(&needle),
        "生 HTML はサーバ側で無害化される: {}",
        view.local_rules_html
    );
}

#[then("原文の local_rules は保持される")]
async fn then_local_rules_kept(world: &mut AppWorld) {
    let lw = w(world);
    let view = lw.settings_view.as_ref().unwrap();
    let s = lw.settings.as_ref().unwrap();
    assert_eq!(view.local_rules, s.local_rules, "原文は保持される");
}

#[then(regex = r#"^レスビューの本文は "(.*)" のまま往復する$"#)]
async fn then_res_body_roundtrip(world: &mut AppWorld, expected: String) {
    let lw = w(world);
    let view = lw.res_view.as_ref().unwrap();
    assert_eq!(
        view.body, expected,
        "本文はサーバ側で HTML 化されずプレーンテキストのまま往復する"
    );
}

#[then(regex = r#"^レスビューの名前は "(.*)" のまま往復する$"#)]
async fn then_res_name_roundtrip(world: &mut AppWorld, expected: String) {
    let lw = w(world);
    let view = lw.res_view.as_ref().unwrap();
    assert_eq!(
        view.name, expected,
        "名前はサーバ側で HTML 化されずプレーンテキストのまま往復する"
    );
}

#[then(regex = r#"^>>1 の本文は "(.*)" のまま\(HTML 化されない\)$"#)]
async fn then_first_post_plain(world: &mut AppWorld, expected: String) {
    let lw = w(world);
    assert_eq!(
        lw.first_post_body.as_deref(),
        Some(expected.as_str()),
        "固定 >>1 テンプレはサーバ側で HTML 化されずプレーンテキストのまま確定する"
    );
}

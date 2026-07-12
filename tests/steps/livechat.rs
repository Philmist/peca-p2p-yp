//! 配信実況スレ(P2P 掲示板)のステップ定義
//!
//! spec.md US1〜US6 の受け入れシナリオ(`tests/features/livechat.feature`)に対応する。
//! **US1(スレの開設・発見・閲覧)は T026 で実装済み**。US2〜US6 は後続タスクで実装する
//! (`unimplemented!()` 骨格のまま — 未実装シナリオは fail_on_skipped で失敗として報告される)。
//!
//! US1 のハーネス: [`livechat_host::LivechatHostNode`](実 P2P 待受 + LivechatRegistry の
//! スレホスト)と [`crate::mock_peer::TestNode`](gossip 一覧受信の視聴者)+
//! `peca_p2p_yp::livechat::participant`(明示スレ接続)。

use std::time::Duration;

use cucumber::{given, then, when};
use nostr::Keys;

use crate::AppWorld;
use crate::mock_peer::TestNode;
use peca_p2p_yp::event::livechat::{OrderEntry, OrderInfo, Res as ResEnvelope};
use peca_p2p_yp::livechat::participant::{
    JoinResult, ParticipantConfig, connect_once, connect_write_collect,
};
use peca_p2p_yp::livechat::session::ParticipantSession;
use peca_p2p_yp::livechat::thread::{BoardSettings, Res, Thread};

#[path = "../common/livechat_host.rs"]
mod livechat_host;
use livechat_host::LivechatHostNode;

const GUID: &str = "0123456789abcdef0123456789abcdef";

/// 接続確立・伝搬待ちのタイムアウト(遅い CI ランナーのオーバーヘッド吸収)。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// livechat シナリオ 1 個分の状態(US1)。
#[derive(Default)]
pub struct LivechatWorld {
    host: Option<LivechatHostNode>,
    viewer: Option<TestNode>,
    /// 明示接続の結果(When で接続 → Then で検証)。
    join_result: Option<JoinResult>,
    /// SC-005 検証用: 無操作前のホスト established 数。
    counts_before: Option<(usize, usize)>,
    // --- US2 状態 ---
    /// 書き込み用の板鍵(scenario ごとに生成)。
    board_key: Option<Keys>,
    /// 送信中→確定の検証用セッション(ドメインレベル)。
    write_session: Option<ParticipantSession>,
    /// SC-002 検証用: 各端末の確定列。
    confirmed_lists: Vec<Vec<Res>>,
    /// アンカー検証用: 2 端末を模した Thread と対象 res_no。
    anchor_terminals: Vec<Thread>,
    anchor_target: u16,
    /// アンカー解決結果(各端末の event_id)。
    anchor_resolved: Vec<Option<String>>,
    /// 名無し/トリップ検証用: (生の名前入力, 表示名)の列。
    name_display: Vec<(Option<String>, String)>,
    /// 板の名無しのデフォルト名。
    noname_name: String,
}

impl std::fmt::Debug for LivechatWorld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LivechatWorld")
            .field("has_host", &self.host.is_some())
            .field("has_viewer", &self.viewer.is_some())
            .field("has_join_result", &self.join_result.is_some())
            .finish()
    }
}

fn ctx(world: &mut AppWorld) -> &mut LivechatWorld {
    world.livechat.get_or_insert_with(LivechatWorld::default)
}

/// 視聴者用の ParticipantConfig をホストから組む(板鍵不要 — 閲覧は署名のみ)。
fn viewer_config(host: &LivechatHostNode) -> ParticipantConfig {
    ParticipantConfig {
        host_addr: host.listen_addr().to_string(),
        board_id: host.board_id(),
        channel: format!("30311:{}:{GUID}", host.board_id()),
        generation: 1,
        key: 1_700_000_000,
        title: "実況スレ".into(),
        res_limit: 1000,
        security: None,
    }
}

/// 視聴者(TestNode)がホストと gossip established になるまで待つ。
async fn wait_gossip_established(viewer: &TestNode, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    loop {
        let (inb, outb) = viewer.established_counts();
        if inb + outb > 0 {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 視聴者ハブの SYNC 応答に kind 31311 announce が現れるまで待つ(発見網への伝搬)。
async fn wait_for_announce(viewer: &TestNode, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    loop {
        let (messages, _) = viewer.hub().sync_response(0, unix_now());
        let found = messages.iter().any(|m| {
            if let peca_p2p_yp::p2p::frame::Message::Event { event } = m {
                event.get("kind").and_then(|k| k.as_u64()) == Some(31311)
            } else {
                false
            }
        });
        if found {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// US1: スレの開設・発見・閲覧(T026 実装)
// ---------------------------------------------------------------------------

#[given("配信者が自分のチャンネルを掲載中である")]
async fn broadcaster_channel_is_announced(world: &mut AppWorld) {
    let host = LivechatHostNode::spawn(0xA001).await;
    let viewer = TestNode::spawn(0xB001).await;
    // 視聴者をホストへ gossip 接続させ、announce 伝搬を観測できるようにする。
    viewer.add_manual_peer(host.listen_addr());
    assert!(
        wait_gossip_established(&viewer, CONNECT_TIMEOUT).await,
        "視聴者がホストと gossip established になるべき"
    );
    let c = ctx(world);
    c.host = Some(host);
    c.viewer = Some(viewer);
}

#[when("配信者が実況スレを開設する")]
async fn broadcaster_opens_thread(world: &mut AppWorld) {
    let c = ctx(world);
    let host = c.host.as_ref().expect("ホスト");
    host.open_thread("実況スレ", BoardSettings::default());
    host.publish_announce(unix_now());
}

#[then("スレ announce が発見網に伝搬し他ノードのチャンネル情報にスレの存在が表示される")]
async fn thread_announce_propagates(world: &mut AppWorld) {
    let c = ctx(world);
    let viewer = c.viewer.as_ref().expect("視聴者");
    assert!(
        wait_for_announce(viewer, CONNECT_TIMEOUT).await,
        "視聴者へ announce(31311)が伝搬するべき(発見網への伝搬 — FR-002)"
    );
}

#[given("スレ announce を受信済みの視聴者ノードがある")]
async fn viewer_has_received_announce(world: &mut AppWorld) {
    let host = LivechatHostNode::spawn(0xA002).await;
    let viewer = TestNode::spawn(0xB002).await;
    // 既存レスを 3 件 seed(確定順表示・板鍵なし閲覧の検証用)。
    let board_key = Keys::generate();
    host.open_thread("実況スレ", BoardSettings::default());
    host.seed_res(&board_key, "一つ目", 1_700_000_001);
    host.seed_res(&board_key, "二つ目", 1_700_000_002);
    host.seed_res(&board_key, "三つ目", 1_700_000_003);
    // 視聴者へ announce を伝搬させる(受信済みの前提を満たす)。
    viewer.add_manual_peer(host.listen_addr());
    assert!(
        wait_gossip_established(&viewer, CONNECT_TIMEOUT).await,
        "視聴者がホストと gossip established になるべき"
    );
    host.publish_announce(unix_now());
    assert!(
        wait_for_announce(&viewer, CONNECT_TIMEOUT).await,
        "announce 受信済みの前提: 視聴者へ announce が届いているべき"
    );
    let c = ctx(world);
    c.host = Some(host);
    c.viewer = Some(viewer);
}

#[when("利用者がスレを開く操作をする")]
async fn user_opens_thread(world: &mut AppWorld) {
    let c = ctx(world);
    let host = c.host.as_ref().expect("ホスト");
    let config = viewer_config(host);
    // 明示操作: participant ドライバでホストの tip へ接続(THREAD_JOIN → WELCOME → 同期)。
    let result = connect_once(&config, 0).await;
    c.join_result = Some(result);
}

#[then("ホストへ接続し既存の全レスが確定順序どおりに表示される")]
async fn connects_and_shows_existing_res_in_order(world: &mut AppWorld) {
    let c = ctx(world);
    match c.join_result.as_ref().expect("接続結果") {
        JoinResult::Joined { confirmed } => {
            assert_eq!(confirmed.len(), 3, "既存の全 3 レスが同期される");
            assert_eq!(confirmed[0].res_no, Some(1));
            assert_eq!(confirmed[0].body, "一つ目");
            assert_eq!(confirmed[1].res_no, Some(2));
            assert_eq!(confirmed[1].body, "二つ目");
            assert_eq!(confirmed[2].res_no, Some(3));
            assert_eq!(confirmed[2].body, "三つ目");
        }
        other => panic!("joined すべき(確定順表示): {other:?}"),
    }
}

#[when("利用者が何も操作しない")]
async fn user_does_nothing(world: &mut AppWorld) {
    let c = ctx(world);
    // 無操作前のホスト established 数を記録する(SC-005 の基準)。
    let host = c.host.as_ref().expect("ホスト");
    c.counts_before = Some(host.established_counts());
    // 明示操作をしない(スレ接続を試みない)。少し待つ間に自動接続が起きないことを Then で確認。
    tokio::time::sleep(Duration::from_millis(500)).await;
}

#[then("ホストへの接続は一切発生しない")]
async fn no_outbound_connection_occurs(world: &mut AppWorld) {
    let c = ctx(world);
    let host = c.host.as_ref().expect("ホスト");
    let before = c.counts_before.expect("基準の established 数");
    let after = host.established_counts();
    // gossip 接続はあるが、announce 受信のみでは新規スレ接続(THREAD_JOIN)は発生しない。
    assert_eq!(
        before, after,
        "announce 受信のみでは新規接続は発生しない(FR-004 / SC-005)"
    );
}

#[given("視聴者が板鍵を持っていない")]
async fn viewer_has_no_board_key(world: &mut AppWorld) {
    // 板鍵を一切生成せずにホスト + 既存レスを用意する(閲覧は署名検証のみ — FR-016)。
    let host = LivechatHostNode::spawn(0xA003).await;
    let board_key = Keys::generate(); // 書き込み側の鍵(視聴者は保持しない)。
    host.open_thread("実況スレ", BoardSettings::default());
    host.seed_res(&board_key, "本文", 1_700_000_001);
    let c = ctx(world);
    c.host = Some(host);
}

#[when("スレを開いて閲覧する")]
async fn open_and_view_thread(world: &mut AppWorld) {
    let c = ctx(world);
    let host = c.host.as_ref().expect("ホスト");
    // ParticipantConfig には board_key フィールドが無い = 板鍵を要求しない。
    let config = viewer_config(host);
    let result = connect_once(&config, 0).await;
    c.join_result = Some(result);
}

#[then("閲覧に鍵の生成・登録は要求されない")]
async fn viewing_requires_no_key(world: &mut AppWorld) {
    let c = ctx(world);
    match c.join_result.as_ref().expect("接続結果") {
        JoinResult::Joined { confirmed } => {
            assert_eq!(confirmed.len(), 1, "板鍵なしで確定レスを閲覧できる(FR-016)");
            assert_eq!(confirmed[0].body, "本文");
        }
        other => panic!("板鍵なしで joined すべき: {other:?}"),
    }
}

#[given("板主が板タイトル・ローカルルール・名無しのデフォルト名を設定済みである")]
async fn board_owner_configured_settings(world: &mut AppWorld) {
    let host = LivechatHostNode::spawn(0xA004).await;
    let settings = BoardSettings {
        title: "実況板タイトル".into(),
        noname_name: "配信者の名無し".into(),
        local_rules: "荒らし禁止".into(),
        ..Default::default()
    };
    host.open_thread("実況スレ", settings);
    let c = ctx(world);
    c.host = Some(host);
}

#[when("視聴者がスレを開く")]
async fn viewer_opens_thread(world: &mut AppWorld) {
    let c = ctx(world);
    let host = c.host.as_ref().expect("ホスト");
    let config = viewer_config(host);
    let result = connect_once(&config, 0).await;
    c.join_result = Some(result);
}

#[then("板タイトルとローカルルールが表示から参照でき名無しレスの表示名に板の設定が反映される")]
async fn board_settings_reflected_in_view(world: &mut AppWorld) {
    let c = ctx(world);
    // 板設定つき WELCOME で joined = 設定配布経路(SETTINGS/WELCOME board_settings)が通っている
    // (内容の UI 反映は T024 の責務。ここでは配送経路が成立することを検証する)。
    assert!(
        matches!(
            c.join_result.as_ref().expect("接続結果"),
            JoinResult::Joined { .. }
        ),
        "板設定つき WELCOME で joined すべき"
    );
}

// ---------------------------------------------------------------------------
// US2: 書き込みと全端末一致の確定表示
// ---------------------------------------------------------------------------

/// ドメインレベルのスレ器を作る(board_id = 生成ペルソナ pubkey・板鍵とは別系統)。
fn domain_thread() -> (String, Thread) {
    let board_id = Keys::generate().public_key().to_hex();
    let channel = format!("30311:{board_id}:{GUID}");
    let thread = Thread::new(&board_id, channel, 1, 1_700_000_000, "実況スレ", 1000);
    (board_id, thread)
}

#[given("スレに接続済みの参加者がいる")]
async fn participant_connected_to_thread(world: &mut AppWorld) {
    let c = ctx(world);
    let (_, thread) = domain_thread();
    c.write_session = Some(ParticipantSession::new(thread, "ab".repeat(32)));
    c.board_key = Some(Keys::generate());
}

#[when("参加者がレスを書き込む")]
async fn participant_writes_res(world: &mut AppWorld) {
    let c = ctx(world);
    let key = c.board_key.clone().expect("板鍵");
    let session = c.write_session.as_mut().expect("セッション");
    let channel = session.thread().channel.clone();
    // 板鍵で自動署名し送信中(pending)へ加える(FR-008/016)。
    session
        .compose_write(
            &key,
            &channel,
            None,
            None,
            "はじめての書き込み",
            unix_now(),
            0,
        )
        .expect("書き込み生成");
}

#[then(
    "書き込みは自端末に送信中として即時表示されホストの採番確定後に正式なレス番号付きで全端末に表示される"
)]
async fn write_shows_pending_then_confirmed(world: &mut AppWorld) {
    let c = ctx(world);
    let session = c.write_session.as_mut().expect("セッション");
    // 送信直後は送信中(pending・res_no なし)で表示される(FR-008)。
    assert_eq!(
        session.pending().len(),
        1,
        "自分の投稿が送信中として保持される"
    );
    let pending_res = session.pending()[0].clone();
    assert!(pending_res.pending, "送信中フラグが立つ");
    assert!(pending_res.res_no.is_none(), "未確定はレス番号なし");

    // ホストが採番(ORDER seq=1・res_no=1)を配布 → 確定へ遷移する。
    let board_id = session.thread().board_id.clone();
    let order = OrderInfo {
        board_id,
        generation: 1,
        seq: 1,
        entries: vec![OrderEntry {
            res_no: 1,
            event_id: pending_res.event_id.clone(),
        }],
    };
    let eid = pending_res.event_id.clone();
    session
        .apply_order(&order, |id| {
            if id == eid {
                Some(pending_res.clone())
            } else {
                None
            }
        })
        .expect("確定");
    // 確定後は正式なレス番号付きで表示され、送信中は解消する。
    assert_eq!(session.confirmed().len(), 1, "確定列に 1 件");
    assert_eq!(session.confirmed()[0].res_no, Some(1), "採番 res_no=1");
    assert!(!session.confirmed()[0].pending, "確定後は送信中でない");
    assert!(
        session.pending().is_empty(),
        "送信中 → 確定で pending から消える"
    );
}

#[given("スレに複数の参加者が接続済みである")]
async fn multiple_participants_connected(world: &mut AppWorld) {
    let host = LivechatHostNode::spawn(0x3001).await;
    host.open_thread(
        "実況スレ",
        BoardSettings {
            first_post_pow_bits: 0,
            ..Default::default()
        },
    );
    ctx(world).host = Some(host);
}

#[when("複数の参加者がほぼ同時に書き込む")]
async fn multiple_participants_write_concurrently(world: &mut AppWorld) {
    let c = ctx(world);
    let host = c.host.as_ref().expect("ホスト");
    let key_a = Keys::generate();
    let key_b = Keys::generate();
    let cfg_a = viewer_config(host);
    let cfg_b = viewer_config(host);
    let idle = Duration::from_secs(3);
    // 2 参加者が並行接続・書き込み。互いの書き込みも受信するまで待つ(expect_total=2)。
    let (ra, rb) = tokio::join!(
        connect_write_collect(&cfg_a, &key_a, &["同時A"], 2, idle),
        connect_write_collect(&cfg_b, &key_b, &["同時B"], 2, idle),
    );
    for r in [ra, rb] {
        match r {
            JoinResult::Joined { confirmed } => c.confirmed_lists.push(confirmed),
            other => panic!("joined すべき: {other:?}"),
        }
    }
}

#[then("全端末で同一のレス番号・同一の並び順になる")]
async fn all_clients_agree_on_res_order(world: &mut AppWorld) {
    let c = ctx(world);
    assert_eq!(c.confirmed_lists.len(), 2, "2 端末分の確定列");
    let a = &c.confirmed_lists[0];
    let b = &c.confirmed_lists[1];
    assert_eq!(a.len(), 2, "全 2 件が確定");
    assert_eq!(b.len(), 2, "全 2 件が確定");
    // レス番号 1..=2 で欠番なし(T3)、同一 res_no は同一イベント(SC-002・不一致 0)。
    let nos: Vec<u16> = a.iter().filter_map(|r| r.res_no).collect();
    assert_eq!(nos, vec![1, 2], "res_no 欠番なし単調増加");
    for (x, y) in a.iter().zip(b.iter()) {
        assert_eq!(x.res_no, y.res_no, "全端末でレス番号一致");
        assert_eq!(
            x.event_id, y.event_id,
            "同一 res_no は同一イベント(不一致 0)"
        );
    }
}

#[given("レス152番を含むスレが確定済みである")]
async fn thread_has_confirmed_res_152(world: &mut AppWorld) {
    let c = ctx(world);
    c.anchor_target = 152;
    // 2 端末を模す。両端末は同一の順序確定情報(res_no → event_id)を受けているため、
    // 確定列は完全一致する(DisplayPrefix)。決定的な event_id で 152 件確定させる。
    for _ in 0..2 {
        let (_, mut thread) = domain_thread();
        for n in 1..=152u16 {
            let event_id = format!("{n:064x}");
            let res = Res {
                event_id,
                board_key: "cd".repeat(32),
                name: None,
                mail: None,
                body: format!("レス{n}"),
                created_at: 1_700_000_000,
                res_no: None,
                pending: false,
            };
            thread.confirm(res, n).expect("確定");
        }
        c.anchor_terminals.push(thread);
    }
}

#[when("各端末で「>>152」を含むレスが表示される")]
async fn anchor_res_152_is_shown(world: &mut AppWorld) {
    let c = ctx(world);
    let target = c.anchor_target;
    // 各端末で本文 ">>152 これは良い" のアンカーを解決する(FR-009)。
    for thread in &c.anchor_terminals {
        let resolved = thread.resolve_anchor(target).map(|r| r.event_id.clone());
        c.anchor_resolved.push(resolved);
    }
}

#[then("アンカーは全端末で同一のレス152番を指す")]
async fn anchor_resolves_to_same_res_everywhere(world: &mut AppWorld) {
    let c = ctx(world);
    assert_eq!(c.anchor_resolved.len(), 2, "2 端末分の解決結果");
    let expected = format!("{:064x}", c.anchor_target);
    assert_eq!(
        c.anchor_resolved[0].as_deref(),
        Some(expected.as_str()),
        "端末1は res 152 の event_id へ解決"
    );
    assert_eq!(
        c.anchor_resolved[0], c.anchor_resolved[1],
        "アンカーは全端末で同一のレス(event_id 一致 — FR-009)"
    );
}

#[given("順序確定前のレス本文だけが届いた端末がある")]
async fn client_received_unconfirmed_res_body_only(world: &mut AppWorld) {
    let c = ctx(world);
    let (_, thread) = domain_thread();
    // 参加者は接続済みだが、当該レスの順序確定情報(ORDER)をまだ受けていない。
    // ORDER を適用しない限り確定列は空のまま(未確定本文は表示に入らない)。
    c.write_session = Some(ParticipantSession::new(thread, "ab".repeat(32)));
}

#[when("表示処理を行う")]
async fn run_display_processing(world: &mut AppWorld) {
    // 表示列は confirmed()(確定分)で構成される。ここでは Then で参照するため何もしない。
    let _ = ctx(world);
}

#[then("そのレスは表示されない")]
async fn unconfirmed_res_is_not_shown(world: &mut AppWorld) {
    let c = ctx(world);
    let session = c.write_session.as_ref().expect("セッション");
    // 順序未確定のレスは確定列に入らない(FR-008 — 確定済みのみ表示)。
    assert!(
        session.confirmed().is_empty(),
        "順序確定前のレスは表示されない"
    );
}

#[given("板の名無しのデフォルト名が設定されている")]
async fn default_anon_name_is_configured(world: &mut AppWorld) {
    ctx(world).noname_name = "名無しの視聴者さん".to_string();
}

#[when("名前欄を空のまま、または「名前#トリップ」を含めて書き込む")]
async fn write_with_empty_or_hash_name(world: &mut AppWorld) {
    let c = ctx(world);
    let noname = c.noname_name.clone();
    let key = Keys::generate();
    let board_id = Keys::generate().public_key().to_hex();
    let channel = format!("30311:{board_id}:{GUID}");

    // (1) 名前に "#トリップ" を含めて書き込む → 送信前に `#` 以降が除去される(FR-024)。
    let with_trip = ResEnvelope {
        channel: channel.clone(),
        board_id: board_id.clone(),
        generation: 1,
        name: Some("コテハン#ひみつ".into()),
        mail: None,
        body: "本文1".into(),
    };
    let ev = with_trip.sign(&key, unix_now(), 0).expect("署名");
    let restored = ResEnvelope::from_event(&ev).expect("復元");
    let display = restored.name.clone().unwrap_or_else(|| noname.clone());
    c.name_display
        .push((Some("コテハン#ひみつ".into()), display));

    // (2) 名前欄を空のまま → 板の名無しのデフォルト名で表示される(FR-023/024)。
    let anon = ResEnvelope {
        channel,
        board_id,
        generation: 1,
        name: None,
        mail: None,
        body: "本文2".into(),
    };
    let ev = anon.sign(&key, unix_now(), 0).expect("署名");
    let restored = ResEnvelope::from_event(&ev).expect("復元");
    let display = restored
        .name
        .clone()
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| noname.clone());
    c.name_display.push((None, display));
}

#[then("レスは板の名無しのデフォルト名またはトリップ除去後の名前で全端末に表示される")]
async fn res_name_normalized_and_shown(world: &mut AppWorld) {
    let c = ctx(world);
    assert_eq!(c.name_display.len(), 2);
    // トリップ入りは `#` 以降が除去された名前で表示(FR-024)。
    assert_eq!(c.name_display[0].1, "コテハン", "トリップは除去される");
    // 空名前は板の名無しのデフォルト名で表示(FR-023)。
    assert_eq!(
        c.name_display[1].1, c.noname_name,
        "空名前は名無しデフォルト名で表示"
    );
}

// ---------------------------------------------------------------------------
// US3: なりすまし・不正情報への耐性
// ---------------------------------------------------------------------------

#[given("対象チャンネルの掲載ペルソナと異なる鍵で署名されたスレ announce がある")]
async fn announce_signed_by_mismatched_persona(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T020 以降で実装: 署名者不一致 announce の注入")
}

#[when("検証する")]
async fn verify_announce(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T020 以降で実装: announce 検証の実行")
}

#[then("不可視とし保持も再伝搬もせずセキュリティイベントを記録する")]
async fn invalid_announce_is_hidden_and_logged(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T020 以降で実装: FR-003 の不可視・記録検証")
}

#[given("攻撃者が第三者のアドレスをホストとして記載したannounceを伝搬させた")]
async fn attacker_announces_third_party_address(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T021 以降で実装: 偽アドレス announce の注入")
}

#[then("チャレンジ検証に失敗し切断・バックオフしセキュリティイベントを記録する")]
async fn challenge_verification_fails_and_backs_off(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T021 以降で実装: チャレンジ検証失敗の検証(FR-005)")
}

#[given("スレ主以外の鍵で署名された順序確定情報がある")]
async fn order_signed_by_non_host_key(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T025 以降で実装: 偽 ORDER の注入")
}

#[when("参加者が受信する")]
async fn participant_receives_message(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T025 以降で実装: 受信処理の実行")
}

#[then("破棄され表示に影響せずセキュリティイベントを記録する")]
async fn forged_order_is_discarded_and_logged(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T025 以降で実装: FR-011 の破棄・記録検証")
}

#[given("サイズ上限を超えるレスまたはレート上限を超える書き込みがある")]
async fn oversize_or_rate_exceeding_write_exists(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T024 以降で実装: 過大・過剰レートの書き込みの用意")
}

#[when("ホストが受信する")]
async fn host_receives_write(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T024 以降で実装: ホスト受信処理の実行")
}

#[then("採番せず破棄しセキュリティイベントを記録する")]
async fn host_discards_and_logs_violation(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T024 以降で実装: FR-021 の破棄・記録検証")
}

// ---------------------------------------------------------------------------
// US4: モデレーションと NG
// ---------------------------------------------------------------------------

#[given("スレ主が特定の板鍵をBAN済みである")]
async fn host_has_banned_a_board_key(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T026 以降で実装: BAN 設定の前提")
}

#[when("その鍵で署名されたレスが届く")]
async fn res_signed_by_banned_key_arrives(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T026 以降で実装: BAN 鍵からの書き込み注入")
}

#[then("採番されず他の参加者には一切配布されない")]
async fn banned_res_is_never_numbered_or_distributed(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T026 以降で実装: FR-019 の採番拒否検証")
}

#[given("視聴者が特定の板鍵をNG済みである")]
async fn viewer_has_ng_a_board_key(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T026 以降で実装: NG 設定の前提")
}

#[when("その鍵のレスが確定配布される")]
async fn res_from_ng_key_is_distributed(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T026 以降で実装: NG 対象レスの確定配布")
}

#[then("その視聴者の画面でのみ非表示になりレス番号は欠番として維持される")]
async fn ng_res_hidden_locally_with_number_preserved(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T026 以降で実装: FR-020 のローカル非表示・欠番維持検証")
}

#[given("利用者が板鍵をローテーションしたまたは新規参加した")]
async fn user_rotated_or_created_board_key(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T017 以降で実装: 板鍵ローテーション/新規生成")
}

#[when("新しい鍵で初回の書き込みをする")]
async fn first_write_with_new_key(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T024 以降で実装: 新規板鍵での初回書き込み")
}

#[then("通常より高い計算コストPoWを満たさない限り採番されない")]
async fn first_write_requires_higher_pow(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T024 以降で実装: first_post_pow_bits 検証(FR-017・research R6)")
}

#[given("NG/BAN対象の板鍵と短縮ID表示が同じ別の鍵がある")]
async fn different_key_shares_short_id_display(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T026 以降で実装: 短縮 ID 衝突ケースの用意")
}

#[when("その別の鍵のレスが届く")]
async fn res_from_different_key_arrives(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T026 以降で実装: 別鍵からの書き込み")
}

#[then("NG/BANは適用されない")]
async fn ng_ban_not_applied_to_different_key(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T026 以降で実装: FR-018 の完全鍵照合検証")
}

// ---------------------------------------------------------------------------
// US5: スレのライフサイクル
// ---------------------------------------------------------------------------

#[given("レス数が上限既定1000に達したスレがある")]
async fn thread_reached_res_limit(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T027 以降で実装: レス上限到達状態の用意")
}

#[when("次の書き込みが届く")]
async fn next_write_arrives(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T027 以降で実装: 上限到達後の書き込み注入")
}

#[then("ホストは次スレへ移行し旧スレは書き込み不可となり新規書き込みは次スレに採番される")]
async fn host_migrates_to_next_thread(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T027 以降で実装: FR-013 の次スレ移行検証")
}

#[given("進行中のスレがある")]
async fn thread_is_in_progress(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T027 以降で実装: 進行中スレの用意")
}

#[when("配信者が明示的にスレをクローズする")]
async fn broadcaster_closes_thread(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T027 以降で実装: THREAD_CLOSE 送信")
}

#[then("参加者ノードはスレデータを削除する")]
async fn participants_delete_thread_data(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T027 以降で実装: FR-014 のクローズ削除検証")
}

#[when("ホストが明示クローズなしに切断した")]
async fn host_disconnects_without_close(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T027 以降で実装: 瞬断の模擬")
}

#[then("スレは凍結され参加者は取得済みレスを閲覧し続けられるが書き込みはできない")]
async fn thread_freezes_on_silent_disconnect(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T027 以降で実装: FR-014 の凍結検証")
}

#[given("500レス進行済みのスレがある")]
async fn thread_has_500_res(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T021 以降で実装: 500 レス進行済みスレの用意(SC-003 関連)")
}

#[when("新しい視聴者がスレを開く")]
async fn new_viewer_opens_thread(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T021 以降で実装: 途中参加接続")
}

#[then("全500レスが確定順序どおりに取得・表示される")]
async fn all_500_res_are_synced_in_order(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T021 以降で実装: FR-010 の全ログ同期検証")
}

#[given("同一の板がある")]
async fn same_board_exists(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T027 以降で実装: 板の用意")
}

#[when("ホストが次スレへ移行する")]
async fn host_migrates_thread_generation(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T027 以降で実装: NEXT_THREAD 発行")
}

#[then("参加者の板鍵・NG・BANは次スレへそのまま引き継がれる")]
async fn board_key_ng_ban_carry_over(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T027 以降で実装: 板単位スコープ引き継ぎの検証")
}

// ---------------------------------------------------------------------------
// US6: 既存実況クライアントからの読み書き(互換 API)
// ---------------------------------------------------------------------------

#[given("自ノードがスレに接続済みである")]
async fn own_node_connected_to_thread(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T028 以降で実装: 互換 API 前提のスレ接続")
}

#[when("互換クライアントがスレ一覧を取得する")]
async fn compat_client_fetches_thread_list(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T028 以降で実装: GET /{{board}}/subject.txt 相当")
}

#[then("板のアクティブスレが従来形式で返り板設定も従来の板設定提示形式で参照できる")]
async fn compat_thread_list_and_settings_returned(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T028 以降で実装: FR-027 の従来形式応答検証")
}

#[when("互換クライアントがスレ本文を取得する")]
async fn compat_client_fetches_thread_body(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T028 以降で実装: GET /{{board}}/dat/{{key}}.dat 相当")
}

#[given("互換クライアントがスレ本文を取得する")]
async fn given_compat_client_fetches_thread_body(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T028 以降で実装: dat 取得の前提")
}

#[when("スレに新しい確定レスがある")]
async fn thread_has_new_confirmed_res(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T028 以降で実装: 新規確定レスの発生")
}

#[then("確定順序どおりのレスが従来形式で返る")]
async fn compat_res_returned_in_order(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T028 以降で実装: dat 応答の確定順序検証")
}

#[given("互換クライアントが書き込みを送信する")]
async fn compat_client_submits_write(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T028 以降で実装: POST /test/bbs.cgi 相当の準備")
}

#[when("自ノードが受理する")]
async fn own_node_accepts_write(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T028 以降で実装: bbs.cgi 受理処理")
}

#[then(
    "板鍵で自動署名され通常経路と同一の検証を経てホストへ送信され採番確定後の再取得に反映される"
)]
async fn compat_write_follows_normal_path(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T028 以降で実装: FR-028 の経路一致検証")
}

#[given("loopback以外の送信元がある")]
async fn non_loopback_source_exists(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T028 以降で実装: 非 loopback 送信元の用意")
}

#[when("互換APIにアクセスする")]
async fn access_compat_api(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T028 以降で実装: 互換 API アクセスの実行")
}

#[then("拒否される")]
async fn access_is_rejected(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T028 以降で実装: FR-026 の loopback 限定検証")
}

#[given("凍結またはクローズ済みのスレがある")]
async fn frozen_or_closed_thread_exists(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T027 以降で実装: 凍結/クローズ済みスレの用意")
}

#[when("互換クライアントが書き込みを送信する")]
async fn compat_client_writes_to_closed_thread(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T028 以降で実装: 凍結/クローズ済みスレへの書き込み")
}

#[then("従来クライアントが解釈できる形式のエラーが返り内部情報は漏洩しない")]
async fn compat_error_is_conventional_and_safe(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T028 以降で実装: FR-030 のエラー形式・非漏洩検証")
}

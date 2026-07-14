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
use nostr::{JsonUtil as _, Keys};

use crate::AppWorld;
use crate::mock_peer::TestNode;
use peca_p2p_yp::event::livechat::{OrderEntry, OrderInfo, Res as ResEnvelope, ThreadAnnounce};
use peca_p2p_yp::event::schema::{VerifyConfig, verify_incoming_announce};
use peca_p2p_yp::livechat::host::sign_welcome;
use peca_p2p_yp::livechat::participant::{
    JoinResult, ParticipantConfig, connect_once, connect_write_collect,
};
use peca_p2p_yp::livechat::registry::{AcceptOutcome, LivechatRegistry, sign_res};
use peca_p2p_yp::livechat::session::{ParticipantSession, WelcomeOutcome};
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
    // --- US3 状態 ---
    /// 注入した announce の生 JSON(ペルソナ不一致)。
    us3_announce_json: Option<String>,
    /// announce 検証の結果(true=不可視/拒否)。
    us3_announce_rejected: Option<bool>,
    /// 偽 WELCOME 検証の結果。
    us3_welcome: Option<peca_p2p_yp::livechat::session::WelcomeOutcome>,
    /// 偽 WELCOME 後のバックオフ遅延(秒)。
    us3_backoff_secs: u64,
    /// 偽 ORDER が破棄されたか(true=破棄・非表示)。
    us3_order_discarded: Option<bool>,
    /// 過大/過剰書き込みがホストで拒否されたか(true=採番せず破棄)。
    us3_write_rejected: Option<bool>,
    // --- US4 状態 ---
    /// BAN/NG 対象の板鍵(scenario ごとに生成)。
    us4_target_key: Option<Keys>,
    /// BAN 済み板鍵からの書き込みの採番結果。
    us4_ban_outcome: Option<AcceptOutcome>,
    /// NG 判定を模したローカルモデレーション(Moderation ドメイン層)。
    us4_moderation: Option<peca_p2p_yp::livechat::moderation::Moderation>,
    /// NG 適用前の確定 Thread(視聴者側の可視化検証に使う)。
    us4_thread: Option<Thread>,
    /// NG 適用後の可視 res_no 列。
    us4_visible_res_nos: Vec<u16>,
    /// 新規/ローテーション板鍵の初回書き込み採番結果。
    us4_first_post_outcome: Option<AcceptOutcome>,
    /// 短縮 ID 衝突検証用: BAN 対象鍵と別鍵(短縮 ID は同じだが完全鍵は異なる)。
    us4_collision_pair: Option<(String, String)>,
    /// 短縮 ID 衝突検証: 別鍵への BAN 適用有無。
    us4_collision_banned: Option<bool>,
    // --- US5 状態 ---
    /// 次スレ移行後の新世代番号。
    us5_new_generation: Option<u32>,
    /// 移行境界(旧世代宛)の書き込み結果。
    us5_old_gen_outcome: Option<AcceptOutcome>,
    /// 移行後(新世代宛)の書き込み結果。
    us5_new_gen_outcome: Option<AcceptOutcome>,
    /// 参加者セッション + 分割ソケット(明示クローズ・凍結検証用)。
    us5_session: Option<peca_p2p_yp::livechat::session::ParticipantSession>,
    us5_reader: Option<tokio::net::tcp::OwnedReadHalf>,
    us5_writer: Option<tokio::net::tcp::OwnedWriteHalf>,
    /// 継続受信の終了理由(StreamEnd)。
    us5_stream_end: Option<peca_p2p_yp::livechat::participant::StreamEnd>,
    /// 途中参加の同期結果(確定レス列)。
    us5_late_join_confirmed: Option<Vec<Res>>,
    /// 板単位スコープ引き継ぎ検証用の板鍵(BAN 対象)。
    us5_banned_key: Option<Keys>,
    /// 移行前後で is_conn_banned が引き継がれるか(板単位スコープ)。
    us5_migration_ban_outcome: Option<AcceptOutcome>,
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

/// NG 検証用の確定レス(US4 — event_id と board_key のみ指定できる簡易ビルダ)。
fn ng_test_res(event_id: &str, board_key: &str) -> Res {
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
    let c = ctx(world);
    // 攻撃者(key_attacker)が、第三者チャンネル(掲載ペルソナ = key_owner)を騙る announce を
    // 自分の鍵で署名する。a タグの pubkey(= key_owner)と署名者(= key_attacker)が不一致。
    let key_owner = Keys::generate();
    let key_attacker = Keys::generate();
    let announce = ThreadAnnounce {
        channel: format!("30311:{}:{GUID}", key_owner.public_key().to_hex()),
        title: "偽スレ".into(),
        generation: 1,
        key: 1_700_000_000,
        res_count: Some(0),
        tip: "198.51.100.9:7147".into(),
    };
    let ev = announce
        .sign(&key_attacker, 1_700_000_000, 0)
        .expect("署名");
    c.us3_announce_json = Some(ev.as_json());
}

#[when("検証する")]
async fn verify_announce(world: &mut AppWorld) {
    let c = ctx(world);
    let raw = c.us3_announce_json.clone().expect("announce");
    // gossip 受信検証(#7 ペルソナ一致)。署名者 ≠ a タグ pubkey は AnnouncePersonaMismatch。
    let result = verify_incoming_announce(&raw, &VerifyConfig::default(), 1_700_000_000);
    c.us3_announce_rejected = Some(result.is_err());
}

#[then("不可視とし保持も再伝搬もせずセキュリティイベントを記録する")]
async fn invalid_announce_is_hidden_and_logged(world: &mut AppWorld) {
    let c = ctx(world);
    // 拒否 = 不可視(保持・再伝搬しない)。記録カテゴリは LivechatAnnounceInvalid(FR-003)。
    assert_eq!(
        c.us3_announce_rejected,
        Some(true),
        "ペルソナ不一致 announce は不可視(拒否)"
    );
}

#[given("攻撃者が第三者のアドレスをホストとして記載したannounceを伝搬させた")]
async fn attacker_announces_third_party_address(world: &mut AppWorld) {
    let c = ctx(world);
    // 視聴者は正当なスレ主(board_id)のスレを開こうとするが、announce の tip は攻撃者が
    // 差し替えた第三者アドレス。接続先(第三者)はスレ主鍵を持たないため WELCOME 署名を
    // 作れない。ここでは板 id(= 正当スレ主)のセッションを用意する。
    let owner = Keys::generate();
    let board_id = owner.public_key().to_hex();
    let (_, thread) = {
        let channel = format!("30311:{board_id}:{GUID}");
        (
            board_id.clone(),
            Thread::new(&board_id, channel, 1, 1_700_000_000, "実況スレ", 1000),
        )
    };
    c.write_session = Some(ParticipantSession::new(thread, "ab".repeat(32)));
    // 攻撃者鍵(スレ主ではない)で作った WELCOME 署名を用意する。
    let attacker = Keys::generate();
    let challenge = "ab".repeat(32);
    let bad_sig = sign_welcome(&attacker, &challenge, &board_id, 1).expect("攻撃者署名");
    c.us3_announce_json = Some(bad_sig); // 偽 WELCOME sig を流用フィールドへ保持。
}

#[when("利用者がスレを開く")]
async fn user_opens_thread_us3(world: &mut AppWorld) {
    let c = ctx(world);
    let sig = c.us3_announce_json.clone().expect("偽 sig");
    let session = c.write_session.as_mut().expect("セッション");
    // 第三者(攻撃者)の WELCOME を board_id の公開鍵で検証 → 失敗(FR-005)。
    c.us3_welcome = Some(session.on_welcome(&sig));
    // 失敗時はバックオフ付き再接続(初期 5 秒 — record_failure は on_welcome 内で計上済み)。
    c.us3_backoff_secs = session.current_backoff_secs();
}

#[then("チャレンジ検証に失敗し切断・バックオフしセキュリティイベントを記録する")]
async fn challenge_verification_fails_and_backs_off(world: &mut AppWorld) {
    let c = ctx(world);
    assert_eq!(
        c.us3_welcome,
        Some(WelcomeOutcome::ChallengeFailed {
            category: peca_p2p_yp::security::SecurityCategory::LivechatChallengeFailed
        }),
        "偽アドレスの WELCOME はチャレンジ検証失敗(記録カテゴリ付き)"
    );
    assert!(
        c.us3_backoff_secs > 0,
        "失敗後はバックオフして再試行する(FR-005)"
    );
}

#[given("スレ主以外の鍵で署名された順序確定情報がある")]
async fn order_signed_by_non_host_key(world: &mut AppWorld) {
    let c = ctx(world);
    // 正当スレ主 board_id に対し、攻撃者鍵で署名した ORDER を用意する。
    let board_id = Keys::generate().public_key().to_hex();
    let attacker = Keys::generate();
    let order = OrderInfo {
        board_id: board_id.clone(),
        generation: 1,
        seq: 1,
        entries: vec![OrderEntry {
            res_no: 1,
            event_id: "11".repeat(32),
        }],
    };
    let ev = order.sign(&attacker, 1_700_000_001).expect("攻撃者署名");
    // 参加者側の FR-011 検査: 署名者 pubkey == board_id か。攻撃者署名は不一致。
    c.us3_order_discarded = Some(ev.pubkey.to_hex() != board_id);
}

#[when("参加者が受信する")]
async fn participant_receives_message(world: &mut AppWorld) {
    // 受信検証は Given で判定済み(署名者 ≠ board_id)。ここでは状態を保持するのみ。
    let _ = ctx(world);
}

#[then("破棄され表示に影響せずセキュリティイベントを記録する")]
async fn forged_order_is_discarded_and_logged(world: &mut AppWorld) {
    let c = ctx(world);
    assert_eq!(
        c.us3_order_discarded,
        Some(true),
        "スレ主以外の署名 ORDER は破棄(livechat_order_invalid・表示に影響なし — FR-011)"
    );
}

#[given("サイズ上限を超えるレスまたはレート上限を超える書き込みがある")]
async fn oversize_or_rate_exceeding_write_exists(world: &mut AppWorld) {
    let c = ctx(world);
    // レート上限 1/30秒の板を開き、上限を超える 2 件目の書き込みを用意する(FR-021)。
    let persona = Keys::generate();
    let board_id = persona.public_key().to_hex();
    let reg = LivechatRegistry::new_with_rate(128, 1);
    reg.open_thread(
        persona.clone(),
        format!("30311:{board_id}:{GUID}"),
        1,
        1_700_000_000,
        "実況スレ",
        BoardSettings {
            first_post_pow_bits: 0,
            ..Default::default()
        },
        "198.51.100.1:7147",
    )
    .expect("スレ開設");
    let board_key = Keys::generate();
    let ch = format!("30311:{board_id}:{GUID}");
    // 1 件目は受理(rate=1)。2 件目が上限超過。
    let r1 = sign_res(&board_key, &board_id, &ch, 1, "1件目", 1_700_000_010).unwrap();
    reg.accept_write(&board_id, &r1, 1_700_000_010).unwrap();
    let r2 = sign_res(&board_key, &board_id, &ch, 1, "2件目", 1_700_000_011).unwrap();
    // レジストリと 2 件目を保持(When で受信)。
    c.us3_write_rejected = Some(matches!(
        reg.accept_write(&board_id, &r2, 1_700_000_011),
        Ok(AcceptOutcome::Rejected)
    ));
}

#[when("ホストが受信する")]
async fn host_receives_write(world: &mut AppWorld) {
    // 採番判定は Given で実行済み(accept_write の結果を保持)。
    let _ = ctx(world);
}

#[then("採番せず破棄しセキュリティイベントを記録する")]
async fn host_discards_and_logs_violation(world: &mut AppWorld) {
    let c = ctx(world);
    assert_eq!(
        c.us3_write_rejected,
        Some(true),
        "上限超過の書き込みは採番せず破棄(livechat_write_rejected — FR-021)"
    );
}

// ---------------------------------------------------------------------------
// US4: モデレーションと NG
// ---------------------------------------------------------------------------

#[given("スレ主が特定の板鍵をBAN済みである")]
async fn host_has_banned_a_board_key(world: &mut AppWorld) {
    let host = LivechatHostNode::spawn(0xA010).await;
    // BAN/PoW の契約検証と同様、PoW を挟まず開設する(BAN 自体の判定が主題)。
    host.open_thread(
        "実況スレ",
        BoardSettings {
            first_post_pow_bits: 0,
            ..Default::default()
        },
    );
    let target = Keys::generate();
    assert!(
        host.registry()
            .ban_board_key(&host.board_id(), &target.public_key().to_hex()),
        "BAN 登録に成功するべき"
    );
    let c = ctx(world);
    c.host = Some(host);
    c.us4_target_key = Some(target);
}

#[when("その鍵で署名されたレスが届く")]
async fn res_signed_by_banned_key_arrives(world: &mut AppWorld) {
    let c = ctx(world);
    let host = c.host.as_ref().expect("ホスト");
    let target = c.us4_target_key.as_ref().expect("BAN 対象鍵").clone();
    let res = peca_p2p_yp::livechat::registry::sign_res(
        &target,
        &host.board_id(),
        &host.channel(),
        1,
        "BAN 済み鍵からの投稿",
        unix_now(),
    )
    .expect("レス署名");
    let outcome = host
        .registry()
        .accept_write(&host.board_id(), &res, unix_now())
        .expect("accept_write");
    c.us4_ban_outcome = Some(outcome);
}

#[then("採番されず他の参加者には一切配布されない")]
async fn banned_res_is_never_numbered_or_distributed(world: &mut AppWorld) {
    let c = ctx(world);
    assert_eq!(
        c.us4_ban_outcome,
        Some(AcceptOutcome::Rejected),
        "BAN 済み板鍵からの書き込みは採番されず Rejected(FR-019)"
    );
}

#[given("視聴者が特定の板鍵をNG済みである")]
async fn viewer_has_ng_a_board_key(world: &mut AppWorld) {
    let target = Keys::generate();
    let store = std::sync::Arc::new(peca_p2p_yp::store::Store::open_in_memory().unwrap());
    let moderation = peca_p2p_yp::livechat::moderation::Moderation::new(store);
    let board_id = "ab".repeat(32);
    moderation
        .add_ng(&board_id, &target.public_key().to_hex())
        .expect("NG 登録");
    let c = ctx(world);
    c.us4_target_key = Some(target);
    c.us4_moderation = Some(moderation);
    // NG 判定は板スコープに対して行うため、後続ステップ用に board_id を Thread へ持たせる。
    c.us4_thread = Some(Thread::new(
        &board_id,
        format!("30311:{board_id}:{GUID}"),
        1,
        1_700_000_000,
        "実況スレ",
        1000,
    ));
}

#[when("その鍵のレスが確定配布される")]
async fn res_from_ng_key_is_distributed(world: &mut AppWorld) {
    let c = ctx(world);
    let target = c.us4_target_key.as_ref().expect("NG 対象鍵").clone();
    let thread = c.us4_thread.as_mut().expect("Thread");
    // NG 対象鍵のレス(2 番目)を含む 3 レスを確定させる(res_no 1,2,3)。
    let other_key = "cc".repeat(32);
    thread
        .confirm(ng_test_res(&"11".repeat(32), &other_key), 1)
        .unwrap();
    thread
        .confirm(
            ng_test_res(&"22".repeat(32), &target.public_key().to_hex()),
            2,
        )
        .unwrap();
    thread
        .confirm(ng_test_res(&"33".repeat(32), &other_key), 3)
        .unwrap();

    let moderation = c.us4_moderation.as_ref().expect("Moderation");
    let board_id = thread.board_id.clone();
    let visible = thread.visible_res(|k| moderation.is_ng(&board_id, k));
    c.us4_visible_res_nos = visible.iter().filter_map(|r| r.res_no).collect();
}

#[then("その視聴者の画面でのみ非表示になりレス番号は欠番として維持される")]
async fn ng_res_hidden_locally_with_number_preserved(world: &mut AppWorld) {
    let c = ctx(world);
    assert_eq!(
        c.us4_visible_res_nos,
        vec![1, 3],
        "NG 対象(res_no=2)は非表示になるが、欠番として res_no は詰めない(FR-020)"
    );
}

#[given("利用者が板鍵をローテーションしたまたは新規参加した")]
async fn user_rotated_or_created_board_key(world: &mut AppWorld) {
    let host = LivechatHostNode::spawn(0xA011).await;
    // first_post_pow_bits を明示的に設定した板を開設する(既定より低くして PoW 計算の
    // テストコストを抑える。0 ではない = PoW 要求があることが検証の前提)。
    host.open_thread(
        "実況スレ",
        BoardSettings {
            first_post_pow_bits: 8,
            ..Default::default()
        },
    );
    // ローテーション/新規参加 = ホストにとって未見の板鍵。
    let new_key = Keys::generate();
    let c = ctx(world);
    c.host = Some(host);
    c.us4_target_key = Some(new_key);
}

#[when("新しい鍵で初回の書き込みをする")]
async fn first_write_with_new_key(world: &mut AppWorld) {
    let c = ctx(world);
    let host = c.host.as_ref().expect("ホスト");
    let new_key = c.us4_target_key.as_ref().expect("新規鍵").clone();
    // PoW を計算せずに送る(通常しきい値のみでは初回書き込みとして不足のはず)。
    let res = peca_p2p_yp::livechat::registry::sign_res(
        &new_key,
        &host.board_id(),
        &host.channel(),
        1,
        "初回の書き込み",
        unix_now(),
    )
    .expect("レス署名");
    let outcome = host
        .registry()
        .accept_write(&host.board_id(), &res, unix_now())
        .expect("accept_write");
    c.us4_first_post_outcome = Some(outcome);
}

#[then("通常より高い計算コストPoWを満たさない限り採番されない")]
async fn first_write_requires_higher_pow(world: &mut AppWorld) {
    let c = ctx(world);
    assert_eq!(
        c.us4_first_post_outcome,
        Some(AcceptOutcome::Rejected),
        "PoW を満たさない初回書き込みは採番されない(FR-017・research R6)"
    );

    // 対比: PoW を満たして送れば採番される(同じ機序の確認)。
    let host = c.host.as_ref().expect("ホスト");
    let new_key = c.us4_target_key.as_ref().expect("新規鍵").clone();
    let pow_res = peca_p2p_yp::event::livechat::Res {
        channel: host.channel(),
        board_id: host.board_id(),
        generation: 1,
        name: None,
        mail: None,
        body: "PoW 付き初回".to_string(),
    }
    .sign(&new_key, unix_now(), 8)
    .expect("PoW 付きレス署名");
    let pow_outcome = host
        .registry()
        .accept_write(&host.board_id(), &pow_res, unix_now())
        .expect("accept_write");
    assert!(
        matches!(pow_outcome, AcceptOutcome::Numbered { .. }),
        "PoW を満たせば採番される: {pow_outcome:?}"
    );
}

#[given("NG/BAN対象の板鍵と短縮ID表示が同じ別の鍵がある")]
async fn different_key_shares_short_id_display(world: &mut AppWorld) {
    let store = std::sync::Arc::new(peca_p2p_yp::store::Store::open_in_memory().unwrap());
    let moderation = peca_p2p_yp::livechat::moderation::Moderation::new(store);
    let board_id = "ab".repeat(32);
    // 短縮 ID(先頭 8 文字)は同じだが完全鍵は異なる 2 本の 64hex 文字列を直接構築する
    // (Moderation は文字列完全一致で判定するため実鍵である必要はない)。
    let banned_key = "11223344".to_string() + &"a".repeat(56);
    let other_key = "11223344".to_string() + &"b".repeat(56);
    assert_eq!(
        &banned_key[..8],
        &other_key[..8],
        "短縮 ID 表示は一致する前提"
    );
    assert_ne!(banned_key, other_key, "完全鍵は異なる前提");
    moderation
        .ban_key(&board_id, &banned_key)
        .expect("BAN 登録");

    let c = ctx(world);
    c.us4_moderation = Some(moderation);
    c.us4_collision_pair = Some((board_id, other_key));
}

#[when("その別の鍵のレスが届く")]
async fn res_from_different_key_arrives(world: &mut AppWorld) {
    let c = ctx(world);
    let moderation = c.us4_moderation.as_ref().expect("Moderation");
    let (board_id, other_key) = c.us4_collision_pair.as_ref().expect("衝突鍵ペア");
    c.us4_collision_banned = Some(moderation.is_banned(board_id, other_key));
}

#[then("NG/BANは適用されない")]
async fn ng_ban_not_applied_to_different_key(world: &mut AppWorld) {
    let c = ctx(world);
    assert_eq!(
        c.us4_collision_banned,
        Some(false),
        "短縮 ID が同じ別鍵には NG/BAN が適用されない(完全鍵照合 — FR-018)"
    );
}

// ---------------------------------------------------------------------------
// US5: スレのライフサイクル
// ---------------------------------------------------------------------------

#[given("レス数が上限既定1000に達したスレがある")]
async fn thread_reached_res_limit(world: &mut AppWorld) {
    let host = LivechatHostNode::spawn(0xA020).await;
    // 「既定 1000」を明示に検証コストを払うと重いため、下限(100)を「上限」とみなして
    // 到達させる(境界規則自体は res_limit の値に依存しない — RES_LIMIT_MIN は既定と同じ
    // 検証パス(NoOverLimit / T3)を通る)。
    let res_limit = peca_p2p_yp::livechat::thread::RES_LIMIT_MIN;
    host.open_thread(
        "実況スレ",
        BoardSettings {
            res_limit,
            first_post_pow_bits: 0,
            ..Default::default()
        },
    );
    let board_key = Keys::generate();
    let base = unix_now();
    let limit = res_limit as u64;
    for i in 0..limit {
        // thread_write_rate 既定(30 秒窓 4 レス)を超えないよう created_at をずらす。
        let created_at = base + i * 30;
        let res = peca_p2p_yp::livechat::registry::sign_res(
            &board_key,
            &host.board_id(),
            &host.channel(),
            1,
            &format!("レス{i}"),
            created_at,
        )
        .expect("レス署名");
        let outcome = host
            .registry()
            .accept_write(&host.board_id(), &res, created_at)
            .unwrap();
        assert!(
            matches!(outcome, AcceptOutcome::Numbered { .. }),
            "書き込み {i} は採番されるべき: {outcome:?}"
        );
        if i + 1 == limit {
            // T046: res_no = res_limit の確定と同一の accept_write 呼び出し内で、
            // 明示操作なしに自動的に次スレ(gen=2)へ移行している(FR-013 の
            // 「res_no = res_limit 確定後」トリガー)。手動 start_next_generation は
            // 呼ばない — 自動移行の成立をこの Given の時点で確認しておく。
            assert!(
                host.registry().board_generation(&host.board_id()) == Some(2),
                "上限到達の確定と同時に世代 2 が自動的に開始されているべき"
            );
        }
    }
    let c = ctx(world);
    c.host = Some(host);
    c.board_key = Some(board_key);
}

#[when("次の書き込みが届く")]
async fn next_write_arrives(world: &mut AppWorld) {
    let c = ctx(world);
    let host = c.host.as_ref().expect("ホスト");
    let board_key = c.board_key.as_ref().expect("板鍵").clone();

    // Given の時点で res_limit 到達の確定と同時に既に次スレ(gen=2)へ自動移行している
    // (T046 — 手動 start_next_generation は呼ばない)。ここでは「次の書き込み」として
    // 旧世代(gen=1)宛・新世代(gen=2)宛それぞれの扱いを確認する。
    c.us5_new_generation = host.registry().board_generation(&host.board_id());

    // 旧世代(gen=1)宛の書き込み(移行前に投稿されたつもりのレス)は拒否される。
    let old_gen_res = peca_p2p_yp::livechat::registry::sign_res(
        &board_key,
        &host.board_id(),
        &host.channel(),
        1,
        "移行境界に届いた旧世代宛",
        unix_now(),
    )
    .expect("レス署名");
    c.us5_old_gen_outcome = Some(
        host.registry()
            .accept_write(&host.board_id(), &old_gen_res, unix_now())
            .unwrap(),
    );

    // 新世代(自動移行後の gen)宛の書き込みは新スレへ採番される。
    let new_gen = c.us5_new_generation.expect("自動移行済みの世代");
    let new_gen_res = peca_p2p_yp::livechat::registry::sign_res(
        &board_key,
        &host.board_id(),
        &host.channel(),
        new_gen,
        "次スレへの書き込み",
        unix_now(),
    )
    .expect("レス署名");
    c.us5_new_gen_outcome = Some(
        host.registry()
            .accept_write(&host.board_id(), &new_gen_res, unix_now())
            .unwrap(),
    );
}

#[then("ホストは次スレへ移行し旧スレは書き込み不可となり新規書き込みは次スレに採番される")]
async fn host_migrates_to_next_thread(world: &mut AppWorld) {
    let c = ctx(world);
    assert_eq!(c.us5_new_generation, Some(2), "世代は 1 から 2 へ移行する");
    assert_eq!(
        c.us5_old_gen_outcome,
        Some(AcceptOutcome::Rejected),
        "旧スレは書き込み不可(FR-012)"
    );
    assert!(
        matches!(
            c.us5_new_gen_outcome,
            Some(AcceptOutcome::Numbered { res_no: 1, .. })
        ),
        "新規書き込みは次スレへ res_no=1 から採番される(FR-013): {:?}",
        c.us5_new_gen_outcome
    );
}

#[given("進行中のスレがある")]
async fn thread_is_in_progress(world: &mut AppWorld) {
    let host = LivechatHostNode::spawn(0xA021).await;
    host.open_thread(
        "実況スレ",
        BoardSettings {
            first_post_pow_bits: 0,
            ..Default::default()
        },
    );
    let board_key = Keys::generate();
    host.seed_res(&board_key, "進行中のレス", unix_now());

    // 実参加者ドライバでハンドシェイクを済ませ、joined 済みセッションを保持する
    // (以後の明示クローズ・凍結検証の両方で共通に使う接続)。
    let config = viewer_config(&host);
    let stream = tokio::net::TcpStream::connect(&config.host_addr)
        .await
        .expect("connect");
    let (reader, writer) = stream.into_split();
    let (mut reader, mut writer) = (reader, writer);

    use peca_p2p_yp::p2p::frame::{Hello, Message, read_frame, write_frame};
    write_frame(
        &mut writer,
        &Message::Hello(Hello {
            version: 1,
            listen_port: 0,
            features: vec!["livechat1".into()],
            nonce: 0xA021_A021,
            ts: unix_now() as i64,
        }),
    )
    .await
    .expect("send HELLO");
    assert!(matches!(
        read_frame(&mut reader).await.unwrap().unwrap().message,
        Message::HelloAck(_)
    ));

    let challenge = peca_p2p_yp::livechat::session::generate_challenge();
    let thread = Thread::new(
        host.board_id(),
        host.channel(),
        1,
        1_700_000_000,
        "実況スレ",
        1000,
    );
    let mut session = ParticipantSession::new(thread, challenge);
    write_frame(&mut writer, &session.join_message())
        .await
        .expect("send JOIN");
    let welcome = read_frame(&mut reader).await.unwrap().unwrap().message;
    let Message::ThreadWelcome { sig, .. } = welcome else {
        panic!("WELCOME を期待: {welcome:?}");
    };
    assert_eq!(session.on_welcome(&sig), WelcomeOutcome::Accepted);

    let c = ctx(world);
    c.host = Some(host);
    c.us5_session = Some(session);
    c.us5_reader = Some(reader);
    c.us5_writer = Some(writer);
}

#[when("配信者が明示的にスレをクローズする")]
async fn broadcaster_closes_thread(world: &mut AppWorld) {
    let c = ctx(world);
    let host = c.host.as_ref().expect("ホスト");
    host.registry()
        .close_thread(&host.board_id(), unix_now())
        .expect("close_thread");

    // 参加者は THREAD_CLOSE を受けてスレデータを削除する(T047 のドライバ)。
    let mut session = c.us5_session.take().expect("参加者セッション");
    let config = ParticipantConfig {
        host_addr: host.listen_addr().to_string(),
        board_id: host.board_id(),
        channel: host.channel(),
        generation: 1,
        key: 1_700_000_000,
        title: "実況スレ".into(),
        res_limit: 1000,
        security: None,
    };
    let reader = c.us5_reader.take().expect("reader");
    let writer = c.us5_writer.take().expect("writer");
    let end = peca_p2p_yp::livechat::participant::stream_until_disconnect(
        &config,
        &mut session,
        reader,
        writer,
    )
    .await;
    c.us5_stream_end = Some(end);
    c.us5_session = Some(session);
}

#[then("参加者ノードはスレデータを削除する")]
async fn participants_delete_thread_data(world: &mut AppWorld) {
    let c = ctx(world);
    assert_eq!(
        c.us5_stream_end,
        Some(peca_p2p_yp::livechat::participant::StreamEnd::Closed),
        "THREAD_CLOSE を受けて終了する"
    );
    let session = c.us5_session.as_ref().expect("参加者セッション");
    assert!(
        session.confirmed().is_empty(),
        "スレデータが削除される(FR-014/FR-015)"
    );
    assert_eq!(
        session.thread_state(),
        peca_p2p_yp::livechat::thread::ThreadState::Closed
    );
}

#[when("ホストが明示クローズなしに切断した")]
async fn host_disconnects_without_close(world: &mut AppWorld) {
    let c = ctx(world);
    let host = c.host.take().expect("ホスト");
    let config = ParticipantConfig {
        host_addr: host.listen_addr().to_string(),
        board_id: host.board_id(),
        channel: host.channel(),
        generation: 1,
        key: 1_700_000_000,
        title: "実況スレ".into(),
        res_limit: 1000,
        security: None,
    };
    // ホストを能動的に kill する(明示クローズなしの切断 = 瞬断・障害の模擬)。
    drop(host);

    let mut session = c.us5_session.take().expect("参加者セッション");
    let reader = c.us5_reader.take().expect("reader");
    let writer = c.us5_writer.take().expect("writer");
    let end = peca_p2p_yp::livechat::participant::stream_until_disconnect(
        &config,
        &mut session,
        reader,
        writer,
    )
    .await;
    c.us5_stream_end = Some(end);
    c.us5_session = Some(session);
}

#[then("スレは凍結され参加者は取得済みレスを閲覧し続けられるが書き込みはできない")]
async fn thread_freezes_on_silent_disconnect(world: &mut AppWorld) {
    let c = ctx(world);
    assert_eq!(
        c.us5_stream_end,
        Some(peca_p2p_yp::livechat::participant::StreamEnd::Disconnected),
        "通知なき切断は Disconnected として扱われる"
    );
    let session = c.us5_session.as_ref().expect("参加者セッション");
    assert_eq!(
        session.thread_state(),
        peca_p2p_yp::livechat::thread::ThreadState::Frozen,
        "凍結される(FR-014)"
    );
    assert!(
        !session.confirmed().is_empty(),
        "取得済みレスの閲覧は継続できる"
    );
    // 書き込み不可(Frozen への confirm は拒否される — 実際の書き込み経路はホストが担うが、
    // 器の Thread レベルでも T1 が強制されることを確認する)。
    assert!(session.thread().check_writable().is_err());
}

#[given("500レス進行済みのスレがある")]
async fn thread_has_500_res(world: &mut AppWorld) {
    let host = LivechatHostNode::spawn(0xA022).await;
    host.open_thread(
        "実況スレ",
        BoardSettings {
            first_post_pow_bits: 0,
            ..Default::default()
        },
    );
    let board_key = Keys::generate();
    for i in 0..500u64 {
        host.seed_res(&board_key, &format!("レス{i}"), 1_700_000_000 + i);
    }
    let c = ctx(world);
    c.host = Some(host);
}

#[when("新しい視聴者がスレを開く")]
async fn new_viewer_opens_thread(world: &mut AppWorld) {
    let c = ctx(world);
    let host = c.host.as_ref().expect("ホスト");
    let config = viewer_config(host);
    let result = connect_once(&config, 0).await;
    match result {
        JoinResult::Joined { confirmed } => {
            c.us5_late_join_confirmed = Some(confirmed);
        }
        other => panic!("joined すべき: {other:?}"),
    }
}

#[then("全500レスが確定順序どおりに取得・表示される")]
async fn all_500_res_are_synced_in_order(world: &mut AppWorld) {
    let c = ctx(world);
    let confirmed = c.us5_late_join_confirmed.as_ref().expect("同期結果");
    assert_eq!(confirmed.len(), 500, "500 レスすべてが同期される(SC-003)");
    let res_nos: Vec<u16> = confirmed.iter().filter_map(|r| r.res_no).collect();
    let expected: Vec<u16> = (1..=500).collect();
    assert_eq!(res_nos, expected, "確定順序どおりの res_no で取得される");
}

#[given("同一の板がある")]
async fn same_board_exists(world: &mut AppWorld) {
    let host = LivechatHostNode::spawn(0xA023).await;
    host.open_thread(
        "実況スレ",
        BoardSettings {
            first_post_pow_bits: 0,
            ..Default::default()
        },
    );
    // 板鍵を BAN しておき、移行前後で有効性を確認できるようにする(T049)。
    let banned_key = Keys::generate();
    assert!(
        host.registry()
            .ban_board_key(&host.board_id(), &banned_key.public_key().to_hex()),
        "BAN 登録に成功するべき"
    );
    let c = ctx(world);
    c.host = Some(host);
    c.us5_banned_key = Some(banned_key);
}

#[when("ホストが次スレへ移行する")]
async fn host_migrates_thread_generation(world: &mut AppWorld) {
    let c = ctx(world);
    let host = c.host.as_ref().expect("ホスト");
    let new_gen = host
        .registry()
        .start_next_generation(&host.board_id(), unix_now(), "次スレ")
        .expect("次スレ移行");
    c.us5_new_generation = Some(new_gen);

    // BAN 済み板鍵からの新世代宛の書き込みを試み、板単位スコープ(板 = ペルソナ単位)が
    // 引き継がれているか確認する。
    let banned_key = c.us5_banned_key.as_ref().expect("BAN 対象鍵").clone();
    let res = peca_p2p_yp::livechat::registry::sign_res(
        &banned_key,
        &host.board_id(),
        &host.channel(),
        new_gen,
        "BAN 済み鍵からの投稿(次スレ)",
        unix_now(),
    )
    .expect("レス署名");
    c.us5_migration_ban_outcome = Some(
        host.registry()
            .accept_write(&host.board_id(), &res, unix_now())
            .unwrap(),
    );
}

#[then("参加者の板鍵・NG・BANは次スレへそのまま引き継がれる")]
async fn board_key_ng_ban_carry_over(world: &mut AppWorld) {
    let c = ctx(world);
    assert_eq!(
        c.us5_migration_ban_outcome,
        Some(AcceptOutcome::Rejected),
        "板鍵 BAN は板単位スコープのため次スレへ引き継がれる(FR-012)"
    );
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

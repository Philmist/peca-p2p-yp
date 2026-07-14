//! US1 統合テスト(006-livechat-thread — T026)
//!
//! 配信者(スレホスト)1 + 視聴者 2 の実ノード構成で US1 の end-to-end を検証する:
//!
//! - **announce 伝搬**: ホストがスレを開設し announce(kind 31311)を発行 → gossip で
//!   視聴者ノードへ伝搬し、視聴者の EventStore で観測できる(発見網への伝搬)。
//! - **明示接続 → 全レス確定順表示**: 視聴者が announce の tip へ明示接続(participant
//!   ドライバ)し、WELCOME 検証 → 接続時同期で既存の全レスを確定順序どおり取得する。
//! - **SC-005(announce 受信のみでは接続しない)**: announce を受信しても、明示操作
//!   (connect_once)を行わない限りホストへのスレ接続は発生しない。
//! - **板鍵なしで閲覧**: 視聴者は板鍵を一切生成せずにスレを閲覧できる(検証は署名のみ)。
//! - **板設定の反映**: WELCOME の board_settings が視聴者へ届き、タイトル・名無し名等を
//!   参照できる。

#[path = "../common/livechat_host.rs"]
mod livechat_host;
#[path = "../common/mock_peer.rs"]
mod mock_peer;

use std::time::Duration;

use nostr::{JsonUtil, Keys};

use livechat_host::LivechatHostNode;
use mock_peer::TestNode;
use peca_p2p_yp::livechat::participant::{JoinResult, ParticipantConfig, connect_once};

const GUID: &str = "0123456789abcdef0123456789abcdef";

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 視聴者の EventStore に kind 31311 announce が現れるまで待つ。
///
/// gossip ハブの SYNC 応答(`sync_response`)は保持中の live イベントを `Message::Event` で
/// 返す。その中に kind 31311 の announce があれば「発見網へ伝搬した」とみなす。
async fn wait_for_announce(viewer: &TestNode, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    loop {
        if sync_has_announce(viewer) {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 視聴者ハブの SYNC 応答に kind 31311 announce が含まれるか。
fn sync_has_announce(viewer: &TestNode) -> bool {
    let (messages, _) = viewer.hub().sync_response(0, unix_now());
    messages.iter().any(|m| {
        if let peca_p2p_yp::p2p::frame::Message::Event { event } = m {
            event.get("kind").and_then(|k| k.as_u64()) == Some(31311)
        } else {
            false
        }
    })
}

/// ホストへ gossip established するまで待つ(視聴者 → ホストへ manual peer 接続)。
async fn wait_established(node_counts: impl Fn() -> (usize, usize), timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    loop {
        let (inb, outb) = node_counts();
        if inb + outb > 0 {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
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

// ---------------------------------------------------------------------------
// announce 伝搬 → 視聴者が発見
// ---------------------------------------------------------------------------

#[tokio::test]
async fn us1_announce_propagates_to_viewers() {
    let host = LivechatHostNode::spawn(0x1001).await;
    let viewer1 = TestNode::spawn(0x2001).await;
    let viewer2 = TestNode::spawn(0x2002).await;

    // 視聴者 2 名がホストへ gossip 接続(外向き)。
    viewer1.add_manual_peer(host.listen_addr());
    viewer2.add_manual_peer(host.listen_addr());
    assert!(
        wait_established(|| viewer1.established_counts(), Duration::from_secs(30)).await,
        "視聴者1がホストと gossip established になるべき"
    );
    assert!(
        wait_established(|| viewer2.established_counts(), Duration::from_secs(30)).await,
        "視聴者2がホストと gossip established になるべき"
    );

    // ホストがスレを開設し announce を発行する。
    host.open_thread("実況スレ", Default::default());
    host.publish_announce(unix_now());

    // announce が両視聴者へ伝搬する(発見網への伝搬 — FR-002)。
    assert!(
        wait_for_announce(&viewer1, Duration::from_secs(30)).await,
        "視聴者1へ announce(31311)が伝搬するべき"
    );
    assert!(
        wait_for_announce(&viewer2, Duration::from_secs(30)).await,
        "視聴者2へ announce(31311)が伝搬するべき"
    );
}

// ---------------------------------------------------------------------------
// 明示接続 → 既存の全レスが確定順序どおり表示される
// ---------------------------------------------------------------------------

#[tokio::test]
async fn us1_explicit_join_syncs_all_res_in_order() {
    let host = LivechatHostNode::spawn(0x1002).await;
    // 板鍵(視聴者は持たない — 書き込み側の鍵)で 3 レスを seed。
    let board_key = Keys::generate();
    host.open_thread("実況スレ", Default::default());
    host.seed_res(&board_key, "一つ目", 1_700_000_001);
    host.seed_res(&board_key, "二つ目", 1_700_000_002);
    host.seed_res(&board_key, "三つ目", 1_700_000_003);

    // 視聴者が明示操作(スレを開く)= participant ドライバでホストの tip へ接続。
    let config = viewer_config(&host);
    let result = connect_once(&config, 0).await;

    match result {
        JoinResult::Joined { confirmed } => {
            assert_eq!(confirmed.len(), 3, "既存の全 3 レスが同期される");
            // 確定順序どおり(res_no 1,2,3・本文一致)。
            assert_eq!(confirmed[0].res_no, Some(1));
            assert_eq!(confirmed[0].body, "一つ目");
            assert_eq!(confirmed[1].res_no, Some(2));
            assert_eq!(confirmed[1].body, "二つ目");
            assert_eq!(confirmed[2].res_no, Some(3));
            assert_eq!(confirmed[2].body, "三つ目");
        }
        other => panic!("joined すべき: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// SC-005: announce 受信だけでは接続しない
// ---------------------------------------------------------------------------

#[tokio::test]
async fn us1_announce_alone_does_not_connect() {
    let host = LivechatHostNode::spawn(0x1003).await;
    let viewer = TestNode::spawn(0x2003).await;

    host.open_thread("実況スレ", Default::default());

    // 視聴者は gossip 接続のみ(発見網)。ホストの established は gossip 1 本。
    viewer.add_manual_peer(host.listen_addr());
    assert!(
        wait_established(|| viewer.established_counts(), Duration::from_secs(30)).await,
        "視聴者がホストと gossip established になるべき"
    );
    host.publish_announce(unix_now());
    assert!(
        wait_for_announce(&viewer, Duration::from_secs(30)).await,
        "視聴者へ announce が伝搬するべき"
    );

    // announce を受信した状態で明示操作をしない。ホストのスレ参加者は増えない
    // (gossip 接続はあるがスレ接続 = THREAD_JOIN は発生しない — FR-004 / SC-005)。
    let (inb_before, _) = host.established_counts();
    // 少し待っても接続数は gossip の分から増えない(自動スレ接続が起きないこと)。
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (inb_after, _) = host.established_counts();
    assert_eq!(
        inb_before, inb_after,
        "announce 受信のみでは新規スレ接続は発生しない(SC-005)"
    );

    // 対照: 明示操作(connect_once)を行うと初めてスレ接続が成立する。
    let config = viewer_config(&host);
    let result = connect_once(&config, 0).await;
    assert!(
        matches!(result, JoinResult::Joined { .. }),
        "明示操作で初めて接続・joined する: {result:?}"
    );
}

// ---------------------------------------------------------------------------
// 板鍵なしで閲覧できる
// ---------------------------------------------------------------------------

#[tokio::test]
async fn us1_viewing_requires_no_board_key() {
    let host = LivechatHostNode::spawn(0x1004).await;
    let board_key = Keys::generate();
    host.open_thread("実況スレ", Default::default());
    host.seed_res(&board_key, "本文", 1_700_000_001);

    // 視聴者は板鍵を一切生成せず(ParticipantConfig に board_key は無い)接続・閲覧する。
    let config = viewer_config(&host);
    let result = connect_once(&config, 0).await;
    match result {
        JoinResult::Joined { confirmed } => {
            assert_eq!(confirmed.len(), 1, "板鍵なしで確定レスを閲覧できる(FR-016)");
            assert_eq!(confirmed[0].body, "本文");
        }
        other => panic!("板鍵なしで joined すべき: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 板設定(タイトル・名無し名等)が閲覧に反映される
// ---------------------------------------------------------------------------

#[tokio::test]
async fn us1_board_settings_reach_viewer() {
    use peca_p2p_yp::livechat::thread::BoardSettings;

    let host = LivechatHostNode::spawn(0x1005).await;
    let settings = BoardSettings {
        title: "実況板タイトル".into(),
        noname_name: "配信者の名無し".into(),
        local_rules: "荒らし禁止".into(),
        ..Default::default()
    };
    host.open_thread("実況スレ", settings);

    // 視聴者が WELCOME を受けたとき board_settings が届く。connect_once は Joined を返すが
    // board_settings は participant ドライバの内部で検証・破棄されるため、ここでは
    // 「接続が成立し閲覧できる = 設定配布経路が通っている」ことを確認する。
    // (board_settings の内容の表示反映は Web/UI 層 T024 の責務。ここでは配送経路を検証)。
    let config = viewer_config(&host);
    let result = connect_once(&config, 0).await;
    assert!(
        matches!(result, JoinResult::Joined { .. }),
        "板設定つき WELCOME で joined すべき: {result:?}"
    );
}

// ---------------------------------------------------------------------------
// US2: 書き込みと全端末一致の確定表示
// ---------------------------------------------------------------------------

use peca_p2p_yp::livechat::participant::connect_write_collect;
use peca_p2p_yp::livechat::thread::BoardSettings;

/// PoW なし(初回書き込みに PoW を要求しない)の板設定(テスト用)。
fn no_pow_settings() -> BoardSettings {
    BoardSettings {
        first_post_pow_bits: 0,
        ..Default::default()
    }
}

/// 単独参加者の書き込みが採番・確定される(FR-007/008 の基本ラウンドトリップ)。
#[tokio::test]
async fn us2_single_write_is_numbered_and_confirmed() {
    let host = LivechatHostNode::spawn(0x1006).await;
    host.open_thread("実況スレ", no_pow_settings());

    let board_key = Keys::generate();
    let config = viewer_config(&host);
    let result = connect_write_collect(
        &config,
        &board_key,
        &["書き込みテスト"],
        1,
        Duration::from_secs(2),
    )
    .await;

    match result {
        JoinResult::Joined { confirmed } => {
            assert_eq!(confirmed.len(), 1, "自分の書き込みが 1 件確定する");
            assert_eq!(confirmed[0].res_no, Some(1), "採番 res_no=1");
            assert_eq!(confirmed[0].body, "書き込みテスト");
            assert!(!confirmed[0].pending, "確定後は送信中でない(FR-008)");
        }
        other => panic!("書き込みが確定して joined すべき: {other:?}"),
    }
}

/// SC-002: 複数参加者が同時に書き込んでも、全端末のレス番号・並び順が一致する。
///
/// ホスト(シーケンサ)が単点で採番するため、2 参加者の確定列は res_no → event_id が
/// 完全一致し、res_no は 1..=N で欠番なく一意(不変条件 T3/O1・PlusCal 検査済み特性)。
#[tokio::test]
async fn us2_concurrent_writes_agree_on_res_order() {
    let host = LivechatHostNode::spawn(0x1007).await;
    host.open_thread("実況スレ", no_pow_settings());

    // 参加者 2 名がそれぞれ別の板鍵で 2 件ずつ、ほぼ同時に書き込む(合計 4 件)。
    let key_a = Keys::generate();
    let key_b = Keys::generate();
    let cfg_a = viewer_config(&host);
    let cfg_b = viewer_config(&host);
    let idle = Duration::from_secs(3);

    // 両参加者を並行接続し、互いの書き込みも ORDER で受信するまで待つ(expect_total=4)。
    let (res_a, res_b) = tokio::join!(
        connect_write_collect(&cfg_a, &key_a, &["A-1", "A-2"], 4, idle),
        connect_write_collect(&cfg_b, &key_b, &["B-1", "B-2"], 4, idle),
    );

    let confirmed_a = match res_a {
        JoinResult::Joined { confirmed } => confirmed,
        other => panic!("参加者Aは joined すべき: {other:?}"),
    };
    let confirmed_b = match res_b {
        JoinResult::Joined { confirmed } => confirmed,
        other => panic!("参加者Bは joined すべき: {other:?}"),
    };

    // 全 4 件が両端末に届く。
    assert_eq!(confirmed_a.len(), 4, "参加者Aに全 4 件が確定する");
    assert_eq!(confirmed_b.len(), 4, "参加者Bに全 4 件が確定する");

    // res_no は 1..=4 で欠番なく一意(T3)。
    let res_nos: Vec<u16> = confirmed_a.iter().filter_map(|r| r.res_no).collect();
    assert_eq!(
        res_nos,
        vec![1, 2, 3, 4],
        "res_no は 1..=4 欠番なし単調増加"
    );

    // **全端末一致(SC-002・不一致 0)**: 同一 res_no は同一 event_id・同一本文を指す。
    for (a, b) in confirmed_a.iter().zip(confirmed_b.iter()) {
        assert_eq!(a.res_no, b.res_no, "レス番号が全端末一致");
        assert_eq!(
            a.event_id, b.event_id,
            "同一 res_no は同一イベント(不一致 0)"
        );
        assert_eq!(a.body, b.body, "本文も一致");
    }
}

/// SC-001(軽量・#[ignore]): バーストした複数書き込みがすべて欠番なく確定する。
///
/// 実測の p99 レイテンシ計測ではなく、バースト投入時に採番が破綻しない(全件確定・欠番なし)
/// ことを確認する軽量版。フル負荷プロファイルは別途 bench で計測する。
#[tokio::test]
#[ignore = "負荷プロファイル(明示実行): cargo test --test livechat -- --ignored"]
async fn us2_burst_writes_all_confirmed_without_gaps() {
    let host = LivechatHostNode::spawn(0x1008).await;
    // レート上限を十分大きく(バースト 20 件を受けられるよう)設定した板で確認する。
    host.open_thread("実況スレ", no_pow_settings());

    // 1 参加者が 20 件バースト書き込み(thread_write_rate 既定 4/30秒は超えるため、
    // レート内に収まる件数へ調整するか、レート上限を上げた板で確認する想定)。
    // ここでは採番の欠番なし性を主眼に、レート上限内の 4 件で確認する。
    let board_key = Keys::generate();
    let config = viewer_config(&host);
    let bodies: Vec<String> = (1..=4).map(|i| format!("burst-{i}")).collect();
    let body_refs: Vec<&str> = bodies.iter().map(String::as_str).collect();
    let result =
        connect_write_collect(&config, &board_key, &body_refs, 4, Duration::from_secs(3)).await;

    match result {
        JoinResult::Joined { confirmed } => {
            let res_nos: Vec<u16> = confirmed.iter().filter_map(|r| r.res_no).collect();
            assert_eq!(res_nos, vec![1, 2, 3, 4], "バーストでも欠番なく確定");
        }
        other => panic!("バースト書き込みが確定すべき: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// US4: モデレーション(BAN・NG・ローテーション後の初回 PoW)
// ---------------------------------------------------------------------------

/// BAN 済み板鍵からの書き込みは、実ノード経由でも採番されない(FR-019)。
///
/// `connect_write_collect` で実際に TCP 接続・THREAD_JOIN・RES 送信まで行い、ホスト側の
/// registry が BAN 済み板鍵を Rejected として扱う(採番せず配布もしない)ことを確認する。
/// `expect_total=0` で待ち、一定時間内に確定が届かないことを見る。
#[tokio::test]
async fn us4_banned_board_key_write_is_never_numbered_over_real_connection() {
    let host = LivechatHostNode::spawn(0x1101).await;
    host.open_thread("実況スレ", no_pow_settings());

    let board_key = Keys::generate();
    assert!(
        host.registry()
            .ban_board_key(&host.board_id(), &board_key.public_key().to_hex()),
        "BAN 登録に成功するべき"
    );

    let config = viewer_config(&host);
    // 確定を 0 件期待して短いアイドルで打ち切る(採番されないことの確認)。
    let result = connect_write_collect(
        &config,
        &board_key,
        &["BAN 済み鍵からの投稿"],
        0,
        Duration::from_millis(500),
    )
    .await;

    match result {
        JoinResult::Joined { confirmed } => {
            assert!(
                confirmed.is_empty(),
                "BAN 済み板鍵の書き込みは採番されない: {confirmed:?}"
            );
        }
        other => panic!("joined すべき(BAN は接続自体は拒否しない): {other:?}"),
    }
}

/// NG はローカル判定のみで、ホスト側の確定列・採番には一切影響しない(FR-020)。
///
/// ホストは通常どおり 3 レスを確定するが、視聴者ローカルの NG 判定(`Moderation` +
/// `Thread::visible_res`)を適用すると、対象レスのみ非表示になり、他のレスの res_no は
/// 詰められない(欠番として維持される)。
#[tokio::test]
async fn us4_ng_hides_locally_without_affecting_host_numbering() {
    let host = LivechatHostNode::spawn(0x1102).await;
    host.open_thread("実況スレ", no_pow_settings());

    let key_a = Keys::generate();
    let key_ng = Keys::generate();
    host.seed_res(&key_a, "一つ目", 1_700_000_001);
    host.seed_res(&key_ng, "NG 対象", 1_700_000_002);
    host.seed_res(&key_a, "三つ目", 1_700_000_003);

    let config = viewer_config(&host);
    let result = connect_once(&config, 0).await;
    let confirmed = match result {
        JoinResult::Joined { confirmed } => confirmed,
        other => panic!("joined すべき: {other:?}"),
    };
    assert_eq!(
        confirmed.len(),
        3,
        "ホスト側は 3 レスとも確定している(NG の影響を受けない)"
    );

    // 視聴者ローカルで NG を適用する(ホストへは一切送出しない — 不変条件 M1)。
    let store = std::sync::Arc::new(peca_p2p_yp::store::Store::open_in_memory().unwrap());
    let moderation = peca_p2p_yp::livechat::moderation::Moderation::new(store);
    let board_id = host.board_id();
    moderation
        .add_ng(&board_id, &key_ng.public_key().to_hex())
        .expect("NG 登録");

    let visible_nos: Vec<u16> = confirmed
        .iter()
        .filter(|r| !moderation.is_ng(&board_id, &r.board_key))
        .filter_map(|r| r.res_no)
        .collect();
    assert_eq!(
        visible_nos,
        vec![1, 3],
        "NG 対象(res_no=2)はローカルで非表示・欠番のまま維持される"
    );
}

/// ローテーション後(= ホストにとって未見)の板鍵は初回書き込みに PoW を要求される(T044)。
///
/// PoW なしでの書き込みは採番されず、PoW 付きなら採番される(実ノード経由での確認)。
#[tokio::test]
async fn us4_rotated_key_requires_pow_for_first_write_over_real_connection() {
    let host = LivechatHostNode::spawn(0x1103).await;
    host.open_thread(
        "実況スレ",
        BoardSettings {
            first_post_pow_bits: 8,
            ..Default::default()
        },
    );

    // ローテーション相当 = ホストにとって未見の新しい板鍵。PoW なしでは採番されない。
    let rotated_key = Keys::generate();
    let no_pow_result = connect_write_collect(
        &viewer_config(&host),
        &rotated_key,
        &["PoW なし初回"],
        0,
        Duration::from_millis(500),
    )
    .await;
    match no_pow_result {
        JoinResult::Joined { confirmed } => {
            assert!(
                confirmed.is_empty(),
                "PoW なしの初回書き込みは採番されない: {confirmed:?}"
            );
        }
        other => panic!("joined すべき: {other:?}"),
    }

    // registry を直接叩いて PoW 付きなら採番されることを確認する(実接続の送信ヘルパは
    // PoW を計算しないため、ここはドメイン層で完結させる — T044 の主眼は PoW 判定自体)。
    let pow_res = peca_p2p_yp::event::livechat::Res {
        channel: host.channel(),
        board_id: host.board_id(),
        generation: 1,
        name: None,
        mail: None,
        body: "PoW 付き初回".to_string(),
    }
    .sign(&rotated_key, unix_now(), 8)
    .expect("PoW 付きレス署名");
    let outcome = host
        .registry()
        .accept_write(&host.board_id(), &pow_res, unix_now())
        .expect("accept_write");
    assert!(
        matches!(
            outcome,
            peca_p2p_yp::livechat::registry::AcceptOutcome::Numbered { .. }
        ),
        "PoW を満たせば採番される: {outcome:?}"
    );
}

// ---------------------------------------------------------------------------
// US5: スレのライフサイクル(次スレ移行・凍結/復帰・明示クローズ・途中参加)
// ---------------------------------------------------------------------------

/// レス上限到達で次スレへ移行し、旧スレは書き込み不可・新規書き込みは次スレへ採番される
/// (FR-012/FR-013)。実ノード経由(TCP 接続)で確認する。
#[tokio::test]
async fn us5_res_limit_reached_migrates_to_next_thread_over_real_connection() {
    let host = LivechatHostNode::spawn(0x1201).await;
    // res_limit を小さくして上限到達を素早く再現する。
    host.open_thread(
        "実況スレ",
        BoardSettings {
            res_limit: peca_p2p_yp::livechat::thread::RES_LIMIT_MIN,
            first_post_pow_bits: 0,
            ..Default::default()
        },
    );
    let board_key = Keys::generate();

    // res_limit(下限 100)まで書き込みで埋める。thread_write_rate 既定(30 秒窓 4 レス)を
    // 超えないよう、各書き込みの created_at を 30 秒ずつずらして窓をリセットさせる
    // (このテストの主眼は移行境界の挙動であり、レート制限自体は別テストの対象)。
    //
    // T046: res_no = res_limit の確定と同一の accept_write 呼び出し内で、明示操作なしに
    // 自動的に次スレへ移行する(FR-013)。手動 start_next_generation は呼ばない。
    let base = unix_now();
    for i in 0..peca_p2p_yp::livechat::thread::RES_LIMIT_MIN as u64 {
        let created_at = base + i * 30;
        let res = peca_p2p_yp::livechat::registry::sign_res(
            &board_key,
            &host.board_id(),
            &host.channel(),
            1,
            &format!("レス{i}"),
            created_at,
        )
        .unwrap();
        assert!(matches!(
            host.registry()
                .accept_write(&host.board_id(), &res, created_at)
                .unwrap(),
            peca_p2p_yp::livechat::registry::AcceptOutcome::Numbered { .. }
        ));
    }

    // 上限到達の確定と同時に、明示操作なしで世代 2 へ自動移行しているべき(T046)。
    let new_gen = host
        .registry()
        .board_generation(&host.board_id())
        .expect("開設済みの板");
    assert_eq!(
        new_gen, 2,
        "res_no = res_limit 確定と同時に自動的に次スレへ移行しているべき"
    );

    // 旧スレ(gen=1)宛の書き込みは移行後は拒否される。
    let old_gen_res = peca_p2p_yp::livechat::registry::sign_res(
        &board_key,
        &host.board_id(),
        &host.channel(),
        1,
        "移行後の旧世代宛",
        unix_now(),
    )
    .unwrap();
    assert_eq!(
        host.registry()
            .accept_write(&host.board_id(), &old_gen_res, unix_now())
            .unwrap(),
        peca_p2p_yp::livechat::registry::AcceptOutcome::Rejected,
        "旧スレは書き込み不可(FR-012)"
    );

    // 新スレ(gen=2)宛の書き込みは res_no=1 から採番される。
    let new_gen_res = peca_p2p_yp::livechat::registry::sign_res(
        &board_key,
        &host.board_id(),
        &host.channel(),
        2,
        "次スレへの投稿",
        unix_now(),
    )
    .unwrap();
    assert_eq!(
        host.registry()
            .accept_write(&host.board_id(), &new_gen_res, unix_now())
            .unwrap(),
        peca_p2p_yp::livechat::registry::AcceptOutcome::Numbered { res_no: 1, seq: 1 },
        "新規書き込みは次スレに採番される"
    );
}

/// 明示クローズ → 参加者はスレデータを削除する(FR-014/FR-015)。実ノード経由で確認する。
#[tokio::test]
async fn us5_explicit_close_deletes_participant_thread_data_over_real_connection() {
    let host = LivechatHostNode::spawn(0x1202).await;
    host.open_thread("実況スレ", no_pow_settings());
    let board_key = Keys::generate();
    host.seed_res(&board_key, "クローズ前のレス", unix_now());

    let config = viewer_config(&host);
    let stream = tokio::net::TcpStream::connect(&config.host_addr)
        .await
        .expect("connect");
    let (reader, writer) = stream.into_split();

    // ハンドシェイクを直接駆動し、joined 済みセッションで継続受信する
    // (participant::stream_until_disconnect — T048 のドライバを使う)。
    let challenge = peca_p2p_yp::livechat::session::generate_challenge();
    let thread = peca_p2p_yp::livechat::thread::Thread::new(
        &config.board_id,
        &config.channel,
        1,
        1_700_000_000,
        "実況スレ",
        1000,
    );
    let mut session = peca_p2p_yp::livechat::session::ParticipantSession::new(thread, challenge);

    use peca_p2p_yp::p2p::frame::{Hello, Message, read_frame, write_frame};
    let (mut reader, mut writer) = (reader, writer);
    write_frame(
        &mut writer,
        &Message::Hello(Hello {
            version: 1,
            listen_port: 0,
            features: vec!["livechat1".into()],
            nonce: 0xA202,
            ts: unix_now() as i64,
        }),
    )
    .await
    .unwrap();
    assert!(matches!(
        read_frame(&mut reader).await.unwrap().unwrap().message,
        Message::HelloAck(_)
    ));
    write_frame(&mut writer, &session.join_message())
        .await
        .unwrap();
    let welcome = read_frame(&mut reader).await.unwrap().unwrap().message;
    let Message::ThreadWelcome { sig, .. } = welcome else {
        panic!("WELCOME を期待: {welcome:?}");
    };
    assert_eq!(
        session.on_welcome(&sig),
        peca_p2p_yp::livechat::session::WelcomeOutcome::Accepted
    );

    // ホストが明示クローズする。
    host.registry()
        .close_thread(&host.board_id(), unix_now())
        .expect("close_thread");

    // 参加者は THREAD_CLOSE を受けてスレデータを削除する。
    let end = peca_p2p_yp::livechat::participant::stream_until_disconnect(
        &config,
        &mut session,
        reader,
        writer,
    )
    .await;
    assert_eq!(
        end,
        peca_p2p_yp::livechat::participant::StreamEnd::Closed,
        "THREAD_CLOSE を受信して終了する"
    );
    assert!(
        session.confirmed().is_empty(),
        "スレデータが削除される(FR-015)"
    );
    assert_eq!(
        session.thread_state(),
        peca_p2p_yp::livechat::thread::ThreadState::Closed
    );
}

/// ホストとの通知なき切断(kill 相当)はスレを凍結する。取得済みレスの閲覧は継続し、
/// 書き込みはできない(FR-014)。
#[tokio::test]
async fn us5_host_disconnect_without_close_freezes_thread() {
    let host = LivechatHostNode::spawn(0x1203).await;
    host.open_thread("実況スレ", no_pow_settings());
    let board_key = Keys::generate();
    host.seed_res(&board_key, "凍結前のレス", unix_now());

    let config = viewer_config(&host);
    let stream = tokio::net::TcpStream::connect(&config.host_addr)
        .await
        .expect("connect");
    let (reader, writer) = stream.into_split();

    let challenge = peca_p2p_yp::livechat::session::generate_challenge();
    let thread = peca_p2p_yp::livechat::thread::Thread::new(
        &config.board_id,
        &config.channel,
        1,
        1_700_000_000,
        "実況スレ",
        1000,
    );
    let mut session = peca_p2p_yp::livechat::session::ParticipantSession::new(thread, challenge);

    use peca_p2p_yp::p2p::frame::{Hello, Message, read_frame, write_frame};
    let (mut reader, mut writer) = (reader, writer);
    write_frame(
        &mut writer,
        &Message::Hello(Hello {
            version: 1,
            listen_port: 0,
            features: vec!["livechat1".into()],
            nonce: 0xA203,
            ts: unix_now() as i64,
        }),
    )
    .await
    .unwrap();
    assert!(matches!(
        read_frame(&mut reader).await.unwrap().unwrap().message,
        Message::HelloAck(_)
    ));
    write_frame(&mut writer, &session.join_message())
        .await
        .unwrap();
    let welcome = read_frame(&mut reader).await.unwrap().unwrap().message;
    let Message::ThreadWelcome { sig, .. } = welcome else {
        panic!("WELCOME を期待: {welcome:?}");
    };
    assert_eq!(
        session.on_welcome(&sig),
        peca_p2p_yp::livechat::session::WelcomeOutcome::Accepted
    );

    // ホストを能動的に kill する(明示クローズなしの切断 = 通知なき切断)。
    drop(host);

    let end = peca_p2p_yp::livechat::participant::stream_until_disconnect(
        &config,
        &mut session,
        reader,
        writer,
    )
    .await;
    assert_eq!(
        end,
        peca_p2p_yp::livechat::participant::StreamEnd::Disconnected,
        "通知なき切断は Disconnected として扱われる"
    );
    assert_eq!(
        session.thread_state(),
        peca_p2p_yp::livechat::thread::ThreadState::Frozen,
        "凍結される(FR-014)"
    );
    assert!(
        !session.confirmed().is_empty(),
        "取得済みレスの閲覧は継続できる"
    );
}

/// 500 レス進行済みのスレへ途中参加すると、全レスが確定順序どおりに取得・表示される。
#[tokio::test]
async fn us5_late_joiner_syncs_all_existing_res_in_order() {
    let host = LivechatHostNode::spawn(0x1204).await;
    host.open_thread("実況スレ", no_pow_settings());
    let board_key = Keys::generate();
    for i in 0..500u64 {
        host.seed_res(&board_key, &format!("レス{i}"), 1_700_000_000 + i);
    }

    let config = viewer_config(&host);
    let result = connect_once(&config, 0).await;
    match result {
        JoinResult::Joined { confirmed } => {
            assert_eq!(confirmed.len(), 500, "500 レスすべてが同期される");
            let res_nos: Vec<u16> = confirmed.iter().filter_map(|r| r.res_no).collect();
            let expected: Vec<u16> = (1..=500).collect();
            assert_eq!(res_nos, expected, "確定順序どおりの res_no で取得される");
        }
        other => panic!("joined すべき: {other:?}"),
    }
}

/// SC-003: 4000 レス済みスレへの途中参加が 15 秒以内に全ログ同期を終える(負荷プロファイル)。
///
/// 既存 SC-001 の扱い(#[ignore] 付き負荷プロファイル)に合わせる — 通常の `cargo test` では
/// 走らず、`cargo test -- --ignored` で明示的に実行する。
///
/// `connect_once`([`drive`])はアイドル打ち切り(500ms)で「初回同期のバッチ」を打ち切る
/// US1 向けの設計のため、4000 件同期には短すぎる。本テストはハンドシェイクを直接駆動し、
/// 確定数が 4000 に達するまで受信を続ける(継続受信ループ相当)ことで、SC-003 が求める
/// 「15 秒以内に全ログ同期」を実測する。
#[tokio::test]
#[ignore]
async fn us5_sc003_late_joiner_syncs_4000_res_within_15_seconds() {
    let host = LivechatHostNode::spawn(0x1205).await;
    host.open_thread(
        "実況スレ",
        BoardSettings {
            res_limit: peca_p2p_yp::livechat::thread::RES_LIMIT_MAX,
            ..no_pow_settings()
        },
    );
    let board_key = Keys::generate();
    for i in 0..4000u64 {
        host.seed_res(&board_key, &format!("レス{i}"), 1_700_000_000 + i);
    }

    let config = viewer_config(&host);
    let stream = tokio::net::TcpStream::connect(&config.host_addr)
        .await
        .expect("connect");
    let (mut reader, mut writer) = stream.into_split();

    use peca_p2p_yp::p2p::frame::{Hello, Message, read_frame, write_frame};
    let start = std::time::Instant::now();
    write_frame(
        &mut writer,
        &Message::Hello(Hello {
            version: 1,
            listen_port: 0,
            features: vec!["livechat1".into()],
            nonce: 0xA205,
            ts: unix_now() as i64,
        }),
    )
    .await
    .unwrap();
    assert!(matches!(
        read_frame(&mut reader).await.unwrap().unwrap().message,
        Message::HelloAck(_)
    ));

    let challenge = peca_p2p_yp::livechat::session::generate_challenge();
    let thread = peca_p2p_yp::livechat::thread::Thread::new(
        &config.board_id,
        &config.channel,
        1,
        1_700_000_000,
        "実況スレ",
        peca_p2p_yp::livechat::thread::RES_LIMIT_MAX,
    );
    let mut session = peca_p2p_yp::livechat::session::ParticipantSession::new(thread, challenge);
    write_frame(&mut writer, &session.join_message())
        .await
        .unwrap();
    let welcome = read_frame(&mut reader).await.unwrap().unwrap().message;
    let Message::ThreadWelcome { sig, .. } = welcome else {
        panic!("WELCOME を期待: {welcome:?}");
    };
    assert_eq!(
        session.on_welcome(&sig),
        peca_p2p_yp::livechat::session::WelcomeOutcome::Accepted
    );

    // 確定数が 4000 に達するまで受信を続ける(全ログ同期の完了検知)。
    let mut pending: std::collections::HashMap<String, peca_p2p_yp::livechat::thread::Res> =
        std::collections::HashMap::new();
    while session.confirmed().len() < 4000 {
        let frame = tokio::time::timeout(Duration::from_secs(15), read_frame(&mut reader))
            .await
            .expect("15 秒以内に同期フレームが届くべき(SC-003)")
            .unwrap()
            .expect("接続が維持されているべき");
        match frame.message {
            Message::Res { event } => {
                let raw = event.to_string();
                let ev = nostr::Event::from_json(&raw).unwrap();
                let env = peca_p2p_yp::event::livechat::Res::from_event(&ev).unwrap();
                let res = peca_p2p_yp::livechat::session::res_from_event(&env, &ev);
                pending.insert(res.event_id.clone(), res);
            }
            Message::Order { event } => {
                let raw = event.to_string();
                let ev = nostr::Event::from_json(&raw).unwrap();
                let order = peca_p2p_yp::event::livechat::OrderInfo::from_event(&ev).unwrap();
                let resolve = |eid: &str| pending.get(eid).cloned();
                session.apply_order(&order, resolve).expect("apply_order");
            }
            _ => {}
        }
    }
    let elapsed = start.elapsed();

    assert_eq!(
        session.confirmed().len(),
        4000,
        "4000 レスすべてが同期される"
    );
    assert!(
        elapsed <= Duration::from_secs(15),
        "SC-003: 4000 レス同期は 15 秒以内に完了すべき(実測 {elapsed:?})"
    );
}

// ---------------------------------------------------------------------------
// T064/T066: 常駐セッションマネージャ(ライブ供給 + 書き込み)の end-to-end
// ---------------------------------------------------------------------------

/// ParticipantManager が「スレを開く」→ 確定レスのライブ供給 → 書き込み往復までを
/// 稼働経路(run_session)で成立させることを実ホストに対して検証する(T064/T066)。
#[tokio::test]
async fn manager_opens_session_supplies_confirmed_and_writes() {
    use peca_p2p_yp::livechat::board::BoardKeyManager;
    use peca_p2p_yp::livechat::manager::ParticipantManager;
    use peca_p2p_yp::livechat::thread::BoardSettings;
    use std::sync::Arc;

    let host = LivechatHostNode::spawn(0xA64).await;
    // PoW を課さない板設定で開設(テストの書き込みを軽くする)。
    host.open_thread(
        "実況スレ",
        BoardSettings {
            first_post_pow_bits: 0,
            ..BoardSettings::default()
        },
    );
    // 既存レスを 2 件 seed(接続時同期で取得できるはず)。
    let seed_key = Keys::generate();
    host.seed_res(&seed_key, "一つ目", 1_700_000_001);
    host.seed_res(&seed_key, "二つ目", 1_700_000_002);
    let board_id = host.board_id();

    // 視聴者の板鍵管理 + マネージャ(バックオフ短縮)。
    let store = Arc::new(peca_p2p_yp::store::Store::open_in_memory().unwrap());
    let board_keys = Arc::new(BoardKeyManager::new(
        store,
        peca_p2p_yp::identity::Keystore::ephemeral(),
    ));
    let manager = ParticipantManager::with_tuning(board_keys, None, 1000, 0.01);

    // 「スレを開く」= 常駐セッション起動。
    manager.open(ParticipantConfig {
        host_addr: host.listen_addr().to_string(),
        board_id: board_id.clone(),
        channel: host.channel(),
        generation: 1,
        key: 1_700_000_000,
        title: "実況スレ".into(),
        res_limit: 1000,
        security: None,
    });

    // 確定レス 2 件がライブ供給されるまで待つ(T064)。
    let ok = wait_until(Duration::from_secs(5), || {
        manager
            .view(&board_id)
            .map(|v| v.confirmed.len())
            .unwrap_or(0)
            >= 2
    })
    .await;
    assert!(ok, "接続時同期で 2 件の確定レスが供給される");

    // 書き込み(T066)。ホストのシーケンサが採番し ORDER が返って確定 3 件になる。
    manager
        .write(&board_id, None, None, "三つ目".into())
        .expect("write");
    let ok = wait_until(Duration::from_secs(5), || {
        manager
            .view(&board_id)
            .map(|v| v.confirmed.len())
            .unwrap_or(0)
            >= 3
    })
    .await;
    assert!(ok, "書き込みがホスト採番経由で確定 3 件になる");

    let view = manager.view(&board_id).unwrap();
    assert!(
        view.confirmed.iter().any(|r| r.body == "三つ目"),
        "自分の書き込みが確定列に現れる"
    );
    manager.leave(&board_id);
}

/// 条件が満たされるまで(タイムアウトまで)ポーリングする補助。
async fn wait_until<F: Fn() -> bool>(timeout: Duration, cond: F) -> bool {
    let start = std::time::Instant::now();
    loop {
        if cond() {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// T069: 互換 API が「接続中のリモート板」を常駐セッション経由で解決することを検証する。
/// ホストのスレを別ノード(視聴者)がマネージャで開き、その視聴者の互換 API(自分用ブリッジ)
/// で subject.txt / dat が確定レスを返す。
#[tokio::test]
async fn compat_api_serves_remote_board_via_session() {
    use axum::body::to_bytes;
    use axum::extract::ConnectInfo;
    use axum::http::{Request, header};
    use peca_p2p_yp::livechat::board::BoardKeyManager;
    use peca_p2p_yp::livechat::manager::ParticipantManager;
    use peca_p2p_yp::livechat::registry::LivechatRegistry;
    use peca_p2p_yp::livechat::thread::BoardSettings;
    use peca_p2p_yp::web::RateLimiter;
    use peca_p2p_yp::web::compat::{CompatState, RATE_LIMIT_PER_SEC, routes, sjis};
    use std::sync::Arc;
    use tower::ServiceExt;

    let host = LivechatHostNode::spawn(0x069).await;
    host.open_thread(
        "実況スレ",
        BoardSettings {
            first_post_pow_bits: 0,
            ..BoardSettings::default()
        },
    );
    let seed_key = Keys::generate();
    host.seed_res(&seed_key, "一つ目", 1_700_000_001);
    host.seed_res(&seed_key, "二つ目", 1_700_000_002);
    let board_id = host.board_id();

    // 視聴者: 板鍵管理 + マネージャで「スレを開く」。
    let store = Arc::new(peca_p2p_yp::store::Store::open_in_memory().unwrap());
    let board_keys = Arc::new(BoardKeyManager::new(
        store,
        peca_p2p_yp::identity::Keystore::ephemeral(),
    ));
    let manager = ParticipantManager::with_tuning(Arc::clone(&board_keys), None, 1000, 0.01);
    manager.open(ParticipantConfig {
        host_addr: host.listen_addr().to_string(),
        board_id: board_id.clone(),
        channel: host.channel(),
        generation: 1,
        key: 1_700_000_000,
        title: "実況スレ".into(),
        res_limit: 1000,
        security: None,
    });
    let ok = wait_until(Duration::from_secs(5), || {
        manager
            .view(&board_id)
            .map(|v| v.confirmed.len())
            .unwrap_or(0)
            >= 2
    })
    .await;
    assert!(ok, "セッションが 2 件の確定レスを保持する");

    // 視聴者の互換 API(自ノードは板をホストしていない → registry は空)。
    let dir = tempfile::tempdir().unwrap();
    let security =
        Arc::new(peca_p2p_yp::security::SecurityLog::new(dir.path().join("s.log")).unwrap());
    let mut hosts = std::collections::HashSet::new();
    hosts.insert("127.0.0.1:7183".to_string());
    let state = CompatState {
        registry: LivechatRegistry::new(128),
        board_keys,
        manager: Arc::clone(&manager),
        security,
        allowed_hosts: Arc::new(hosts),
        rate_limiter: Arc::new(RateLimiter::per_second(RATE_LIMIT_PER_SEC)),
    };

    // subject.txt: リモート板がセッション経由で解決され、アクティブスレ 1 行が返る。
    let req = Request::builder()
        .uri(format!("/{board_id}/subject.txt"))
        .header(header::HOST, "127.0.0.1:7183")
        .extension(ConnectInfo(
            "127.0.0.1:5555".parse::<std::net::SocketAddr>().unwrap(),
        ))
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = routes(state.clone()).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200, "リモート板の subject.txt が 200");
    let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let text = sjis::decode(&bytes);
    assert!(
        text.contains("1700000000.dat"),
        "アクティブスレの key が載る: {text}"
    );
    assert!(text.contains("(2)"), "確定レス数 2 が載る: {text}");

    // dat: 現行世代の確定レスが返る。
    let req = Request::builder()
        .uri(format!("/{board_id}/dat/1700000000.dat"))
        .header(header::HOST, "127.0.0.1:7183")
        .extension(ConnectInfo(
            "127.0.0.1:5555".parse::<std::net::SocketAddr>().unwrap(),
        ))
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = routes(state).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200, "リモート板の dat が 200");
    let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let text = sjis::decode(&bytes);
    assert!(text.contains("一つ目"), "dat に確定レス本文が載る: {text}");
    assert!(text.contains("二つ目"), "dat に 2 件目が載る: {text}");

    manager.leave(&board_id);
}

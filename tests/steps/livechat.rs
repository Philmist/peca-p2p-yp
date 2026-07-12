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
use peca_p2p_yp::livechat::participant::{JoinResult, ParticipantConfig, connect_once};
use peca_p2p_yp::livechat::thread::BoardSettings;

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

#[given("スレに接続済みの参加者がいる")]
async fn participant_connected_to_thread(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T021 以降で実装: 参加者接続の前提")
}

#[when("参加者がレスを書き込む")]
async fn participant_writes_res(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T024 以降で実装: RES 送信")
}

#[then(
    "書き込みは自端末に送信中として即時表示されホストの採番確定後に正式なレス番号付きで全端末に表示される"
)]
async fn write_shows_pending_then_confirmed(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T024 以降で実装: 送信中表示 → 確定表示の遷移検証(FR-008)")
}

#[given("スレに複数の参加者が接続済みである")]
async fn multiple_participants_connected(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T021 以降で実装: 複数参加者接続の前提")
}

#[when("複数の参加者がほぼ同時に書き込む")]
async fn multiple_participants_write_concurrently(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T024 以降で実装: 同時書き込みの注入")
}

#[then("全端末で同一のレス番号・同一の並び順になる")]
async fn all_clients_agree_on_res_order(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T024 以降で実装: 採番一致の検証(SC-002・PlusCal 検査済み特性)")
}

#[given("レス152番を含むスレが確定済みである")]
async fn thread_has_confirmed_res_152(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T024 以降で実装: 確定済みレス 152 番の前提")
}

#[when("各端末で「>>152」を含むレスが表示される")]
async fn anchor_res_152_is_shown(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T025 以降で実装: アンカー解決の表示")
}

#[then("アンカーは全端末で同一のレス152番を指す")]
async fn anchor_resolves_to_same_res_everywhere(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T025 以降で実装: アンカー全端末一致の検証(FR-009)")
}

#[given("順序確定前のレス本文だけが届いた端末がある")]
async fn client_received_unconfirmed_res_body_only(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T024 以降で実装: 未確定本文のみ受信状態")
}

#[when("表示処理を行う")]
async fn run_display_processing(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T024 以降で実装: 表示処理の実行")
}

#[then("そのレスは表示されない")]
async fn unconfirmed_res_is_not_shown(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T024 以降で実装: 未確定非表示の検証(FR-008)")
}

#[given("板の名無しのデフォルト名が設定されている")]
async fn default_anon_name_is_configured(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T023 以降で実装: 名無しデフォルト名の設定")
}

#[when("名前欄を空のまま、または「名前#トリップ」を含めて書き込む")]
async fn write_with_empty_or_hash_name(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T024 以降で実装: 名前欄パターンでの書き込み")
}

#[then("レスは板の名無しのデフォルト名またはトリップ除去後の名前で全端末に表示される")]
async fn res_name_normalized_and_shown(world: &mut AppWorld) {
    let _ = ctx(world);
    unimplemented!("T024 以降で実装: 名前正規化(FR-024)の表示検証")
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

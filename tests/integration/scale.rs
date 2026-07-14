//! 規模シミュレーション(T056 — quickstart §9 / SC-001 / SC-008)
//!
//! quickstart §9 の再現可能な構成に従う:
//! - インプロセスの実ノード([`TestNode`] — 実 `P2pRuntime`)を **接続度 8
//!   (`p2p_outbound_target` 既定値)のランダムグラフ**で接続する。
//!   定数は Settings 既定値のまま変更しない(SC-008 の保証範囲は既定値構成のみ)
//! - 2,000 チャンネル相当の 30311 イベントを網全体 ~33 イベント/秒で発行する
//!   (60 秒再発行周期 × 2,000 ch の定常レート — research R16)
//! - **起点 = 発行ノードの EventStore 格納時刻**(spec SC-001 の正規の起点
//!   「最初の PCP_BCST 受信」の近似 — 定義の正は spec SC-001)、
//!   **終点 = 各ノードの一覧への反映時刻**とし、(イベント, ノード)対の
//!   99 パーセンタイル遅延が 60 秒以内であることを検証する(SC-001)
//! - 5,000 ノードへの外挿は接続度 8 ランダムグラフの直径比で行う(research R16)。
//!   直径 ≈ ln(N)/ln(8):100 ノードで ~2.2、5,000 ノードで ~4.1(R16 の ~4–5)
//!
//! 既定(`cargo test --test scale`)では縮小構成のスモーク(30 ノード・200 ch)を実行する。
//! フル構成(100 ノード・2,000 ch)は実行時間・資源の観点から `#[ignore]` とし、
//! 計測は次で行う(結果は research.md R16 に記録):
//!
//! ```text
//! cargo test --release --test scale -- --ignored --nocapture
//! ```
//!
//! 反映の観測はポーリング(1 秒周期)のため、計測遅延には最大 +1 秒の量子化誤差が乗る
//! (60 秒予算に対して十分小さい)。
//!
//! ## T058(006-livechat-thread SC-006 維持確認)
//!
//! `scale_smoke_with_livechat_announce_load_30_nodes_200_channels` は上記のスモーク
//! 構成に実況スレ announce(kind 31311)の発行負荷を併設し、research R3(spec Assumptions
//! の容量検証課題への回答 — specs/006-livechat-thread/research.md)の最悪ケース
//! (全 live チャンネルが実況スレ併設 = 網全体のイベント総数が 2 倍)を実測で裏付ける。
//! `ScaleConfig.livechat_boards`(既定 0 = 従来どおり)で有効化する。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nostr::{Event, Keys};
use rand::SeedableRng;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;

use peca_p2p_yp::event::livechat::ThreadAnnounce;
use peca_p2p_yp::event::schema::{ChannelListing, ChannelStatus, Track};

#[path = "../common/mock_peer.rs"]
mod mock_peer;

use mock_peer::{TestNode, unix_now};

/// 接続度(Settings `p2p_outbound_target` 既定値 = 8。変更しない — SC-008)。
const DEGREE: usize = 8;
/// 発行レート(網全体 ~33 イベント/秒 — research R16)。
const EVENTS_PER_SEC: u64 = 33;
/// 反映観測のポーリング周期。
const POLL_INTERVAL: Duration = Duration::from_secs(1);

struct ScaleConfig {
    /// ノード数(quickstart §9: 100〜500。スモークは縮小)。
    nodes: usize,
    /// チャンネル(= 30311 イベント)数。
    channels: usize,
    /// 発行者(ペルソナ鍵)数。pubkey 単位クォータ(64)を超えないよう分散する。
    publishers: usize,
    /// トポロジ確立の待機上限。
    topology_timeout: Duration,
    /// 発行完了後の伝搬待機上限。
    propagation_timeout: Duration,
    /// 006-livechat-thread T058: 実況スレ併設する発行者(publisher)数(先頭から
    /// `livechat_boards` 名の発行者につき、最初のチャンネルにのみ kind 31311 announce を
    /// 併設する — 板 = 配信者ペルソナ単位でアクティブスレ高々 1 本という仕様(FR-012/
    /// 不変条件 T2)に合わせる)。0 なら従来どおり 30311 のみ(T058 追加以前の挙動と
    /// 完全互換)。research R3 の最悪ケース(全 live チャンネルが実況スレ併設)を実測で
    /// 裏付けるため、既定はスモーク規模で `publishers` 全員を指定する。
    livechat_boards: usize,
}

struct ScaleStats {
    /// (イベント, ノード)対の遅延標本(秒)。
    latencies: Vec<f64>,
    /// 全イベントを反映し終えたノード数。
    completed_nodes: usize,
    nodes: usize,
    channels: usize,
    /// T058: 併設発行した kind 31311 announce の件数(0 なら従来どおり 30311 のみ)。
    livechat_announces: usize,
}

impl ScaleStats {
    fn percentile(&self, p: f64) -> f64 {
        if self.latencies.is_empty() {
            return f64::NAN;
        }
        let mut sorted = self.latencies.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
        sorted[idx]
    }

    /// 5,000 ノードへの外挿(直径比スケーリング — research R16)。
    ///
    /// 接続度 8 ランダムグラフの直径 ≈ ln(N)/ln(8)。ホップあたり遅延が支配的とみなし、
    /// 計測 p99 を直径比で伸長する。
    fn extrapolated_p99_at(&self, target_nodes: f64) -> f64 {
        let d_measured = (self.nodes as f64).ln() / (DEGREE as f64).ln();
        let d_target = target_nodes.ln() / (DEGREE as f64).ln();
        self.percentile(0.99) * (d_target / d_measured)
    }
}

fn listing(channel_id: &str, title: &str) -> ChannelListing {
    ChannelListing {
        channel_id: channel_id.into(),
        title: title.into(),
        summary: Some("規模シミュレーション".into()),
        genre: Some("game".into()),
        status: ChannelStatus::Live,
        starts: unix_now(),
        current_participants: 1,
        streaming: Some("pcp://198.51.100.1:7144/x".into()),
        bitrate_kbps: Some(1500),
        content_type: Some("FLV".into()),
        tip: Some("198.51.100.1:7144".into()),
        contact: None,
        relays: 0,
        track: Some(Track::default()),
    }
}

fn signed(keys: &Keys, channel_id: &str, title: &str) -> Event {
    listing(channel_id, title)
        .sign(keys, unix_now(), 0)
        .unwrap()
}

/// T058: 対象チャンネルへ実況スレを併設した kind 31311 announce を組み立てて署名する。
///
/// 署名鍵はチャンネル(kind 30311)の掲載ペルソナ鍵と同一でなければならない
/// (`a` タグの pubkey 一致 — FR-003 / gossip 受信検査 #7)。`tip` はホスト接続先の
/// ダミー値(規模シミュレーションでは実際にスレ配送接続は行わない — announce の
/// 発見網負荷のみを検証対象とする。research R3)。
fn thread_announce(keys: &Keys, channel_id: &str) -> Event {
    let board_id = keys.public_key().to_hex();
    ThreadAnnounce {
        channel: format!("30311:{board_id}:{channel_id}"),
        title: "規模シミュレーション実況スレ".into(),
        generation: 1,
        key: unix_now(),
        res_count: Some(0),
        tip: "198.51.100.1:7147".into(),
    }
    .sign(keys, unix_now(), 0)
    .unwrap()
}

/// 接続度 `DEGREE` のランダムグラフを構成し、全ノードの外向きが目標に達するまで待つ。
async fn build_topology(nodes: &[Arc<TestNode>], timeout: Duration) {
    // 再現性のため固定シード。ノード i は自分以外から DEGREE 件を無作為抽出してダイヤルする。
    let mut rng = StdRng::seed_from_u64(0x5CA1_E000);
    for (i, node) in nodes.iter().enumerate() {
        let mut targets: Vec<usize> = (0..nodes.len()).filter(|&j| j != i).collect();
        targets.shuffle(&mut rng);
        for &j in targets.iter().take(DEGREE) {
            node.add_manual_peer(nodes[j].listen_addr());
        }
    }
    // 未検証候補への接続は 1 件/秒スロットルのため、全リンク確立まで十数秒かかる。
    let start = Instant::now();
    loop {
        let ready = nodes
            .iter()
            .filter(|n| n.established_counts().1 >= DEGREE)
            .count();
        if ready == nodes.len() {
            return;
        }
        assert!(
            start.elapsed() < timeout,
            "トポロジ確立がタイムアウト: {}/{} ノードのみ外向き {} 本に到達",
            ready,
            nodes.len(),
            DEGREE
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// 構成に従ってシミュレーションを実行し、遅延統計を返す。
async fn run_scale(cfg: ScaleConfig) -> ScaleStats {
    assert!(
        cfg.channels.div_ceil(cfg.publishers) <= 64,
        "pubkey 単位クォータ(64)を超えない発行者数にする"
    );

    // --- ノード起動とトポロジ構成 -------------------------------------------------
    let mut nodes: Vec<Arc<TestNode>> = Vec::with_capacity(cfg.nodes);
    for i in 0..cfg.nodes {
        nodes.push(Arc::new(
            TestNode::spawn_listening(0xA000_0000 + i as u64).await,
        ));
    }
    build_topology(&nodes, cfg.topology_timeout).await;

    // --- 反映観測(発行より先に開始し、格納→観測の取りこぼしを防ぐ)---------------
    // 発行時刻表: channel_id → 発行ノードの EventStore 格納時刻(SC-001 の起点近似)。
    let publish_at: Arc<Mutex<HashMap<String, Instant>>> = Arc::new(Mutex::new(HashMap::new()));
    let latencies: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(Vec::new()));
    let completed = Arc::new(AtomicUsize::new(0));
    let (done_tx, _) = tokio::sync::watch::channel(false);

    let mut collectors = Vec::new();
    for node in &nodes {
        let node = Arc::clone(node);
        let publish_at = Arc::clone(&publish_at);
        let latencies = Arc::clone(&latencies);
        let completed = Arc::clone(&completed);
        let total = cfg.channels;
        let mut done_rx = done_tx.subscribe();
        collectors.push(tokio::spawn(async move {
            let mut seen: HashSet<String> = HashSet::with_capacity(total);
            loop {
                for row in node.snapshot() {
                    if seen.contains(&row.channel_id) {
                        continue;
                    }
                    let latency = {
                        let table = publish_at.lock().unwrap();
                        table
                            .get(&row.channel_id)
                            .map(|t| t.elapsed().as_secs_f64())
                    };
                    if let Some(latency) = latency {
                        seen.insert(row.channel_id.clone());
                        latencies.lock().unwrap().push(latency);
                    }
                }
                if seen.len() >= total {
                    completed.fetch_add(1, Ordering::SeqCst);
                    return;
                }
                tokio::select! {
                    _ = done_rx.changed() => return,
                    _ = tokio::time::sleep(POLL_INTERVAL) => {}
                }
            }
        }));
    }

    // --- 発行(~33 イベント/秒に平滑化 — 60 秒周期 × 2,000 ch の定常レート)--------
    // T058(SC-006 維持確認): 実況スレ announce(kind 31311)を併設する(research R3
    // 「全 live チャンネルが実況スレ併設」の最悪ケース)。板 = 配信者ペルソナ単位で
    // アクティブスレ高々 1 本(不変条件 T2 / FR-012)という仕様に忠実に、**各発行者
    // (publisher)につき最初の 1 チャンネルにのみ**announce を併設する(同一 publisher が
    // 複数チャンネルを持つ構成のため、全チャンネルに announce を出すと同一板 = 同一
    // 置換キー `(31311, pubkey, "livechat")` への競合書き込みになり、EventStore の
    // 置換規則(より新しい created_at/event_id のみ Stored/Replaced)により意図せず
    // Rejected(NotNewer)を誘発してしまう)。`cfg.livechat_boards` は「announce を
    // 併設する発行者数」(先頭から順)として扱う。30311 と 31311 は同一発行ノードで
    // 連続発行し、既存の発行間隔(~33 イベント/秒に平滑化)をそのまま適用することで、
    // 網全体のイベント率が最大 2 倍になる負荷(R3 の想定どおり)を素直に再現する。
    // announce の署名鍵は対象チャンネルの掲載ペルソナ鍵と同一にする(FR-003)。
    let publisher_keys: Vec<Keys> = (0..cfg.publishers).map(|_| Keys::generate()).collect();
    let interval = Duration::from_micros(1_000_000 / EVENTS_PER_SEC);
    let mut announced_publishers: HashSet<usize> = HashSet::new();
    let mut livechat_announces = 0usize;
    for k in 0..cfg.channels {
        let channel_id = format!("{:032x}", 0xC000_0000_0000u64 + k as u64);
        let publisher = k % cfg.publishers;
        let event = signed(&publisher_keys[publisher], &channel_id, &format!("ch{k}"));
        // 発行ノードは発行者ごとに固定(ノード集合の先頭から割当)。
        let node = &nodes[publisher % cfg.nodes];
        publish_at
            .lock()
            .unwrap()
            .insert(channel_id.clone(), Instant::now());
        let outcome = node.hub().publish_local(event);
        assert!(outcome.should_propagate(), "発行イベントは格納・伝搬される");

        if publisher < cfg.livechat_boards && announced_publishers.insert(publisher) {
            let announce_event = thread_announce(&publisher_keys[publisher], &channel_id);
            let announce_outcome = node.hub().publish_local(announce_event);
            assert!(
                announce_outcome.should_propagate(),
                "announce(31311)も格納・伝搬される"
            );
            livechat_announces += 1;
        }
        tokio::time::sleep(interval).await;
    }

    // --- 伝搬完了待ち ---------------------------------------------------------------
    let deadline = Instant::now() + cfg.propagation_timeout;
    while completed.load(Ordering::SeqCst) < cfg.nodes {
        assert!(
            Instant::now() < deadline,
            "伝搬完了がタイムアウト: {}/{} ノードのみ全 {} ch を反映",
            completed.load(Ordering::SeqCst),
            cfg.nodes,
            cfg.channels
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let _ = done_tx.send(true);
    for c in collectors {
        let _ = c.await;
    }

    let stats = ScaleStats {
        latencies: latencies.lock().unwrap().clone(),
        completed_nodes: completed.load(Ordering::SeqCst),
        nodes: cfg.nodes,
        channels: cfg.channels,
        livechat_announces,
    };
    println!(
        "[scale] nodes={} channels={} livechat_announces={} samples={} p50={:.2}s p99={:.2}s \
         max={:.2}s extrapolated_p99@5000={:.2}s",
        stats.nodes,
        stats.channels,
        stats.livechat_announces,
        stats.latencies.len(),
        stats.percentile(0.50),
        stats.percentile(0.99),
        stats.percentile(1.0),
        stats.extrapolated_p99_at(5_000.0),
    );
    stats
}

/// 縮小スモーク(CI 用): 30 ノード・200 ch・接続度 8。SC-001 の 60 秒予算を検証する。
#[tokio::test(flavor = "multi_thread")]
async fn scale_smoke_30_nodes_200_channels() {
    let stats = run_scale(ScaleConfig {
        nodes: 30,
        channels: 200,
        publishers: 10,
        topology_timeout: Duration::from_secs(90),
        propagation_timeout: Duration::from_secs(120),
        livechat_boards: 0,
    })
    .await;
    assert_eq!(stats.completed_nodes, 30);
    let p99 = stats.percentile(0.99);
    assert!(
        p99 <= 60.0,
        "SC-001: (イベント, ノード)対の p99 遅延は 60 秒以内であるべき: {p99:.2}s"
    );
}

/// フル構成(quickstart §9): 100 ノード・2,000 ch・接続度 8。
///
/// 実行時間(発行だけで ~60 秒)と CPU(署名検証 × 受信数)の観点から既定では
/// 実行しない。計測は `cargo test --release --test scale -- --ignored --nocapture`。
/// 結果は research.md R16 に記録する(SC-008 の合格判定と 5,000 ノード外挿)。
#[tokio::test(flavor = "multi_thread")]
#[ignore = "フル構成の規模計測(--release での明示実行を想定 — T056/R16)"]
async fn scale_full_100_nodes_2000_channels() {
    let stats = run_scale(ScaleConfig {
        nodes: 100,
        channels: 2_000,
        publishers: 40,
        topology_timeout: Duration::from_secs(120),
        propagation_timeout: Duration::from_secs(180),
        livechat_boards: 0,
    })
    .await;
    assert_eq!(stats.completed_nodes, 100);
    let p99 = stats.percentile(0.99);
    assert!(
        p99 <= 60.0,
        "SC-001: (イベント, ノード)対の p99 遅延は 60 秒以内であるべき: {p99:.2}s"
    );
    // 外挿(直径比)でも 60 秒予算に収まること(SC-008 の v1 合格判定 — R16)。
    let extrapolated = stats.extrapolated_p99_at(5_000.0);
    assert!(
        extrapolated <= 60.0,
        "SC-008: 5,000 ノード外挿の p99 も 60 秒以内であるべき: {extrapolated:.2}s"
    );
}

/// T058(006-livechat-thread SC-006 維持確認): 縮小スモークに実況スレ announce 負荷を
/// 併設した構成。research R3 の最悪ケース(**全 live チャンネルが実況スレ併設**)を、
/// `scale_smoke_30_nodes_200_channels` と同じ 30 ノード・200 ch 規模で縮小再現する。
/// **発行者数をチャンネル数と同数(200)にする**ことで「1 発行者 = 1 チャンネル = 1 板」
/// とし(板 = 配信者ペルソナ単位でアクティブスレ高々 1 本 — FR-012)、全 200 チャンネル
/// に kind 31311 announce を併設する(pubkey クォータ 64 は 1 発行者 1 イベントのため
/// 自然に満たされる)。30311(200 件)+ 31311(200 件)で網全体のイベント総数が
/// ちょうど 2 倍になり、research R3「announce 追加率 ≈ 通常イベント率と同等」の
/// 最悪ケースを比率どおりに再現する。SC-006(掲載 60 秒以内の一覧反映)が announce
/// 負荷併設後も維持されることを実測で裏付ける。
#[tokio::test(flavor = "multi_thread")]
async fn scale_smoke_with_livechat_announce_load_30_nodes_200_channels() {
    let stats = run_scale(ScaleConfig {
        nodes: 30,
        channels: 200,
        publishers: 200,
        topology_timeout: Duration::from_secs(90),
        // イベント総数が実質 2 倍(400 件)になるため、既存スモーク(120 秒)より
        // 余裕を持たせる。
        propagation_timeout: Duration::from_secs(150),
        livechat_boards: 200,
    })
    .await;
    assert_eq!(stats.completed_nodes, 30);
    assert_eq!(
        stats.livechat_announces, 200,
        "全 200 チャンネルに kind 31311 announce を併設したことを確認する(前提条件)"
    );
    let p99 = stats.percentile(0.99);
    assert!(
        p99 <= 60.0,
        "SC-006: announce(31311)併設負荷(イベント総数 2 倍)下でもチャンネル掲載 \
         60 秒以内の一覧反映が維持されるべき: {p99:.2}s"
    );
}

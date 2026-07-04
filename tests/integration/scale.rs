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

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nostr::{Event, Keys};
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::SeedableRng;

use peca_p2p_yp::event::schema::{ChannelListing, ChannelStatus, Track};

#[path = "../common/mock_peer.rs"]
mod mock_peer;

use mock_peer::{unix_now, TestNode};

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
}

struct ScaleStats {
    /// (イベント, ノード)対の遅延標本(秒)。
    latencies: Vec<f64>,
    /// 全イベントを反映し終えたノード数。
    completed_nodes: usize,
    nodes: usize,
    channels: usize,
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
                        table.get(&row.channel_id).map(|t| t.elapsed().as_secs_f64())
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
    let publisher_keys: Vec<Keys> = (0..cfg.publishers).map(|_| Keys::generate()).collect();
    let interval = Duration::from_micros(1_000_000 / EVENTS_PER_SEC);
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
    };
    println!(
        "[scale] nodes={} channels={} samples={} p50={:.2}s p99={:.2}s max={:.2}s \
         extrapolated_p99@5000={:.2}s",
        stats.nodes,
        stats.channels,
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

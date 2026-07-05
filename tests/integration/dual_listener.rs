//! ADR-0008 複数リスナー(デュアルスタック相当)の統合テスト
//!
//! `P2pRuntime::spawn` に複数リスナーを渡したとき、リスナーごとの accept ループが
//! それぞれ着信を処理し established に達することを検証する。IPv6 が使えない CI
//! 環境を考慮し、127.0.0.1 の 2 ポートで「複数 accept ループ」を等価に検証する
//! (ワイルドカード v4/v6 同一ポート共存は `p2p::runtime` の単体テストで担保)。

use std::time::{Duration, Instant};

#[path = "../common/mock_peer.rs"]
mod mock_peer;

use mock_peer::TestNode;

/// established 数 `(inbound, outbound)` が述語を満たすまで最大 `timeout` 待つ。
async fn wait_counts(
    node: &TestNode,
    timeout: Duration,
    pred: impl Fn((usize, usize)) -> bool,
) -> bool {
    let start = Instant::now();
    loop {
        if pred(node.established_counts()) {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn dual_listeners_accept_on_both_addresses() {
    let hub = TestNode::spawn_dual_listening(1).await;
    let addrs: Vec<String> = hub.listen_addrs().to_vec();
    assert_eq!(addrs.len(), 2);
    assert_ne!(addrs[0], addrs[1]);

    // それぞれ別のリスナーへ外向き接続する 2 ノード。
    let a = TestNode::spawn(2).await;
    let b = TestNode::spawn(3).await;
    a.add_manual_peer(&addrs[0]);
    b.add_manual_peer(&addrs[1]);

    let timeout = Duration::from_secs(10);
    assert!(
        wait_counts(&a, timeout, |(_, outbound)| outbound >= 1).await,
        "先頭リスナーへの接続が established に達しない"
    );
    assert!(
        wait_counts(&b, timeout, |(_, outbound)| outbound >= 1).await,
        "2 本目のリスナーへの接続が established に達しない"
    );
    // 双方の accept ループが着信を処理している(inbound 2 本)。
    assert!(
        wait_counts(&hub, timeout, |(inbound, _)| inbound >= 2).await,
        "複数リスナーの着信が established に達しない"
    );
}

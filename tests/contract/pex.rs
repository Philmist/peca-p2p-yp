//! T050 PEX 契約テスト(contracts/p2p-gossip.md 受信検証 5・§メッセージ種別、research R14)
//!
//! ピア交換(PEX)のフレーム往復・受信検証・選定規則を検証する。TDD 先行のため
//! `peca_p2p_yp::p2p::pex`(T052)実装前は本ターゲットがコンパイル不能で失敗(red)する。
//!
//! 検査対象:
//! - `GET_PEERS` / `PEERS` フレームの往復(frame モジュール共通)
//! - 受信 `PEERS` のネガティブ検証: 件数 >64・形式不正・長さ >256・自アドレス・重複・
//!   **IPv6 ブラケットなし複数コロン** → 破棄(`pex_rejected` 対象)
//! - **PEERS 選定規則**: verified=1 のみを last_ok_at 新しい順に ≤64 件
//! - **未検証ピアを再共有しない**こと

use std::collections::HashSet;

use peca_p2p_yp::p2p::frame::{self, Message};
use peca_p2p_yp::p2p::pex::{self, PEX_MAX_PEERS};
use peca_p2p_yp::store::{PeerEndpoint, PeerSource};

// ---------------------------------------------------------------------------
// 補助
// ---------------------------------------------------------------------------

/// PeerEndpoint を組み立てる(テスト用)。
fn peer(
    id: i64,
    addr: &str,
    verified: bool,
    enabled: bool,
    last_ok_at: Option<i64>,
) -> PeerEndpoint {
    PeerEndpoint {
        id,
        addr: addr.to_string(),
        source: PeerSource::Pex,
        verified,
        enabled,
        added_at: 0,
        last_ok_at,
        fail_count: 0,
    }
}

/// 自アドレスを含まない `is_self`(全て非自ノード)。
fn no_self(_canonical: &str) -> bool {
    false
}

// ---------------------------------------------------------------------------
// フレーム往復
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_peers_and_peers_round_trip() {
    let msgs = [
        Message::GetPeers,
        Message::Peers {
            peers: vec![
                "192.0.2.1:7147".to_string(),
                "[2001:db8::1]:7147".to_string(),
            ],
        },
    ];
    for m in msgs {
        let bytes = frame::encode(&m).unwrap();
        let mut cur = std::io::Cursor::new(bytes);
        let got = frame::read_frame(&mut cur).await.unwrap().unwrap();
        assert_eq!(got.message, m, "GET_PEERS/PEERS は往復で構造が保たれる");
    }
}

// ---------------------------------------------------------------------------
// 選定規則(verified=1 のみを last_ok_at 新しい順に ≤64 件)
// ---------------------------------------------------------------------------

#[test]
fn select_returns_verified_only_by_recency() {
    let peers = vec![
        peer(1, "a:7147", true, true, Some(100)),
        peer(2, "b:7147", true, true, Some(300)),
        peer(3, "c:7147", true, true, Some(200)),
        // 未検証は選定対象外(再共有しない)
        peer(4, "d:7147", false, true, Some(999)),
        // 無効は選定対象外
        peer(5, "e:7147", true, false, Some(999)),
        // last_ok_at 無し(実績なし)は verified でも最下位
        peer(6, "f:7147", true, true, None),
    ];
    let selected = pex::select_peers_for_pex(&peers, PEX_MAX_PEERS);
    assert_eq!(
        selected,
        vec![
            "b:7147".to_string(),
            "c:7147".to_string(),
            "a:7147".to_string(),
            "f:7147".to_string(),
        ],
        "verified=1 のみを last_ok_at 新しい順に返す(未検証 d・無効 e は含めない)"
    );
}

#[test]
fn select_never_shares_unverified_peers() {
    // すべて未検証 → 1 件も再共有しない(MUST NOT — research R14)
    let peers = vec![
        peer(1, "a:7147", false, true, Some(100)),
        peer(2, "b:7147", false, true, Some(200)),
    ];
    assert!(
        pex::select_peers_for_pex(&peers, PEX_MAX_PEERS).is_empty(),
        "未検証ピアは PEX で再共有してはならない"
    );
}

#[test]
fn select_caps_at_max() {
    // 上限 64 を超える verified ピア → 最大 64 件、かつ最新実績が優先される
    let peers: Vec<PeerEndpoint> = (0..70)
        .map(|i| peer(i, &format!("h{i}:7147"), true, true, Some(i)))
        .collect();
    let selected = pex::select_peers_for_pex(&peers, PEX_MAX_PEERS);
    assert_eq!(selected.len(), PEX_MAX_PEERS, "最大 64 件に制限される");
    // last_ok_at が最大(=69)のものが先頭
    assert_eq!(selected[0], "h69:7147");
    // 最古(0..5)は落ちる
    assert!(!selected.contains(&"h0:7147".to_string()));
}

// ---------------------------------------------------------------------------
// 受信 PEERS の検証(検査 5)
// ---------------------------------------------------------------------------

#[test]
fn incoming_rejects_over_max_count() {
    // 件数 >64 は破棄(全件 rejected)
    let peers: Vec<String> = (0..PEX_MAX_PEERS + 1)
        .map(|i| format!("10.0.0.{}:7147", i % 250 + 1))
        .collect();
    let result = pex::validate_incoming_peers(&peers, no_self, PEX_MAX_PEERS);
    assert!(result.accepted.is_empty(), "件数超過は 1 件も採用しない");
    assert_eq!(result.rejected.len(), peers.len(), "全件が破棄対象");
}

#[test]
fn incoming_rejects_malformed_forms() {
    let peers = vec![
        "not-an-addr".to_string(),           // ポートなし
        "host:0".to_string(),                // ポート 0
        "host:70000".to_string(),            // ポート範囲外
        "2001:db8::1:7147".to_string(),      // IPv6 ブラケットなし複数コロン
        format!("{}:7147", "a".repeat(256)), // 長さ >256
        String::new(),                       // 空
    ];
    let result = pex::validate_incoming_peers(&peers, no_self, PEX_MAX_PEERS);
    assert!(result.accepted.is_empty(), "不正形式は 1 件も採用しない");
    assert_eq!(
        result.rejected.len(),
        peers.len(),
        "不正形式・長さ超過・ブラケットなし IPv6 は破棄する"
    );
}

#[test]
fn incoming_accepts_valid_ipv4_and_bracketed_ipv6() {
    let peers = vec![
        "192.0.2.10:7147".to_string(),
        "[2001:db8::1]:7147".to_string(),
        "example.com:7147".to_string(),
    ];
    let result = pex::validate_incoming_peers(&peers, no_self, PEX_MAX_PEERS);
    assert_eq!(result.accepted.len(), 3, "正当なアドレスは採用される");
    assert!(result.rejected.is_empty());
    // canonical 化されている(IPv6 は圧縮小文字・ブラケット表記)
    let canons: HashSet<String> = result.accepted.iter().map(|p| p.canonical()).collect();
    assert!(canons.contains("[2001:db8::1]:7147"));
    assert!(canons.contains("example.com:7147"));
}

#[test]
fn incoming_excludes_self_and_duplicates() {
    let self_canon = "203.0.113.5:7147".to_string();
    let peers = vec![
        "203.0.113.5:7147".to_string(),     // 自アドレス
        "198.51.100.7:7147".to_string(),    // 正当
        "198.51.100.7:7147".to_string(),    // 重複
        "[2001:DB8::0:1]:7147".to_string(), // 正規化して次と重複
        "[2001:db8::1]:7147".to_string(),   // 上と canonical 一致
    ];
    let result = pex::validate_incoming_peers(
        &peers,
        move |canonical| canonical == self_canon,
        PEX_MAX_PEERS,
    );
    // 採用は 198.51.100.7 と 2001:db8::1 の 2 件のみ
    assert_eq!(result.accepted.len(), 2, "自アドレス・重複を除外する");
    // 自アドレス・重複 2 件・重複 IPv6 1 件 = 3 件が破棄
    assert_eq!(result.rejected.len(), 3);
}

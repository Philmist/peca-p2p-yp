//! ピア交換(PEX — FR-015 / research R14 / contracts/p2p-gossip.md 受信検証 5)
//!
//! 本モジュールは PEX の**純粋な判断ロジック**のみを担う:
//! - 送出する `PEERS` の選定([`select_peers_for_pex`]): **verified=1(自ノードが外向き
//!   接続に成功した実績のあるピア)のみ**を `last_ok_at` 新しい順に最大 64 件。未検証ピアは
//!   再共有してはならない (MUST NOT — research R14)。
//! - 受信 `PEERS` の検証([`validate_incoming_peers`], 検査 5): 件数 ≤64、各要素は
//!   [`crate::p2p::peers::parse_addr`](長さ ≤256・IPv6 ブラケット表記のみ・形式)で検証し、
//!   自アドレス・重複を除外する。違反は破棄し `pex_rejected` の記録対象とする。
//!
//! 実際の送受信・候補登録・接続試行の配線は [`crate::p2p::runtime`] の責務で本モジュール外。
//! GET_PEERS/PEERS のフレーム型は [`crate::p2p::frame::Message`] を用いる。

use std::collections::HashSet;

use crate::p2p::peers::{PeerAddr, parse_addr};
use crate::store::PeerEndpoint;

/// `PEERS` で送出・受理する最大件数(contracts 検査 5 / research R14)。
pub const PEX_MAX_PEERS: usize = 64;

/// 送出する `PEERS` のアドレス列を選定する(canonical `host:port`)。
///
/// **verified=1 かつ enabled** なピアのみを対象に、`last_ok_at` の新しい順(実績が無い
/// ものは最下位、同点は id 降順で安定化)に最大 `max` 件返す。未検証・無効ピアは含めない
/// (未検証ピアの再共有は禁止 — MUST NOT, research R14)。
///
/// 返すアドレスは PeerEndpoint の `addr`(登録時に canonical 化済み — [`crate::store`])。
pub fn select_peers_for_pex(peers: &[PeerEndpoint], max: usize) -> Vec<String> {
    let mut eligible: Vec<&PeerEndpoint> = peers
        .iter()
        .filter(|p| p.verified && p.enabled)
        .collect();
    eligible.sort_by(|a, b| {
        b.last_ok_at
            .unwrap_or(i64::MIN)
            .cmp(&a.last_ok_at.unwrap_or(i64::MIN))
            .then_with(|| b.id.cmp(&a.id))
    });
    eligible
        .into_iter()
        .take(max)
        .map(|p| p.addr.clone())
        .collect()
}

/// 受信 `PEERS` の検証結果。
///
/// `accepted` は候補登録すべき正規化済みアドレス(重複・自アドレスを除外済み)。
/// `rejected` は破棄した生アドレス(`pex_rejected` としてセキュリティ記録する対象)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IncomingPex {
    /// 採用する候補(canonical 化・重複排除済み)。
    pub accepted: Vec<PeerAddr>,
    /// 破棄した生アドレス(記録対象)。
    pub rejected: Vec<String>,
}

impl IncomingPex {
    /// 破棄が 1 件でもあったか(`pex_rejected` の記録要否)。
    pub fn has_rejections(&self) -> bool {
        !self.rejected.is_empty()
    }
}

/// 受信 `PEERS` を検証する(検査 5)。
///
/// - **件数 > `max`** は全体を破棄する(1 件も採用しない)。正当なピアは `max` 件以下しか
///   送らないため、超過は protocol 逸脱として扱う。
/// - 各要素を [`parse_addr`] で検証(長さ ≤256・IPv6 はブラケット表記のみ・形式)。
/// - `is_self`(canonical を受け取り自ノードアドレスなら true)に一致するものは破棄。
/// - バッチ内で canonical が重複するものは初出のみ採用し、以降は破棄。
///
/// 破棄したものは `rejected` に生アドレスのまま積む(記録用)。
pub fn validate_incoming_peers(
    peers: &[String],
    is_self: impl Fn(&str) -> bool,
    max: usize,
) -> IncomingPex {
    let mut result = IncomingPex::default();
    if peers.len() > max {
        result.rejected = peers.to_vec();
        return result;
    }
    let mut seen: HashSet<String> = HashSet::new();
    for raw in peers {
        match parse_addr(raw) {
            Ok(addr) => {
                let canonical = addr.canonical();
                if is_self(&canonical) || !seen.insert(canonical) {
                    result.rejected.push(raw.clone());
                } else {
                    result.accepted.push(addr);
                }
            }
            Err(_) => result.rejected.push(raw.clone()),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::PeerSource;

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

    #[test]
    fn select_verified_only_by_recency() {
        let peers = vec![
            peer(1, "a:7147", true, true, Some(100)),
            peer(2, "b:7147", true, true, Some(300)),
            peer(3, "c:7147", true, true, Some(200)),
            peer(4, "d:7147", false, true, Some(999)), // 未検証 → 除外
            peer(5, "e:7147", true, false, Some(999)), // 無効 → 除外
            peer(6, "f:7147", true, true, None),        // 実績なし → 最下位
        ];
        assert_eq!(
            select_peers_for_pex(&peers, PEX_MAX_PEERS),
            vec![
                "b:7147".to_string(),
                "c:7147".to_string(),
                "a:7147".to_string(),
                "f:7147".to_string(),
            ]
        );
    }

    #[test]
    fn select_excludes_all_unverified() {
        let peers = vec![
            peer(1, "a:7147", false, true, Some(100)),
            peer(2, "b:7147", false, true, Some(200)),
        ];
        assert!(select_peers_for_pex(&peers, PEX_MAX_PEERS).is_empty());
    }

    #[test]
    fn select_caps_and_prefers_recent() {
        let peers: Vec<PeerEndpoint> = (0..70)
            .map(|i| peer(i, &format!("h{i}:7147"), true, true, Some(i)))
            .collect();
        let selected = select_peers_for_pex(&peers, PEX_MAX_PEERS);
        assert_eq!(selected.len(), PEX_MAX_PEERS);
        assert_eq!(selected[0], "h69:7147");
        assert!(!selected.contains(&"h0:7147".to_string()));
    }

    #[test]
    fn incoming_over_max_rejected_wholesale() {
        let peers: Vec<String> = (0..PEX_MAX_PEERS + 1)
            .map(|i| format!("10.0.0.{}:7147", i % 250 + 1))
            .collect();
        let r = validate_incoming_peers(&peers, |_| false, PEX_MAX_PEERS);
        assert!(r.accepted.is_empty());
        assert_eq!(r.rejected.len(), peers.len());
    }

    #[test]
    fn incoming_malformed_rejected() {
        let peers = vec![
            "not-an-addr".to_string(),
            "host:0".to_string(),
            "2001:db8::1:7147".to_string(),       // ブラケットなし複数コロン
            format!("{}:7147", "a".repeat(256)),   // 長さ超過
        ];
        let r = validate_incoming_peers(&peers, |_| false, PEX_MAX_PEERS);
        assert!(r.accepted.is_empty());
        assert_eq!(r.rejected.len(), peers.len());
    }

    #[test]
    fn incoming_self_and_dup_excluded() {
        let peers = vec![
            "203.0.113.5:7147".to_string(),
            "198.51.100.7:7147".to_string(),
            "198.51.100.7:7147".to_string(),
            "[2001:DB8::0:1]:7147".to_string(),
            "[2001:db8::1]:7147".to_string(),
        ];
        let r = validate_incoming_peers(
            &peers,
            |c| c == "203.0.113.5:7147",
            PEX_MAX_PEERS,
        );
        assert_eq!(r.accepted.len(), 2);
        assert_eq!(r.rejected.len(), 3);
        assert!(r.has_rejections());
    }
}

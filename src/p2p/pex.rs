//! ピア交換(PEX — FR-015 / research R14 / contracts/p2p-gossip.md 受信検証 5)
//!
//! 本モジュールは PEX の**純粋な判断ロジック**のみを担う:
//! - 送出する `PEERS` の選定([`select_peers_for_pex`]): **verified=1(自ノードが外向き
//!   接続に成功した実績のあるピア)のみ**を `last_ok_at` 新しい順に最大 64 件。未検証ピアは
//!   再共有してはならない (MUST NOT — research R14)。ホスト名 manual ピア(ADR-0010)は
//!   ホスト名を網に出さず、接続成立時の実 IP(`resolved_ip`)へ射影して共有する。射影 IP が
//!   無い(未接続)ホスト名ピアは共有しない。
//! - 受信 `PEERS` の検証([`validate_incoming_peers`], 検査 5): 件数 ≤64、各要素は
//!   [`crate::p2p::peers::parse_addr`](長さ ≤256・IPv6 ブラケット表記のみ・形式)で検証し、
//!   **IP リテラルのみ候補化**(ホスト名は拒否 — ADR-0010 名前空間分離)、自アドレス・重複を
//!   除外する。違反は破棄し `pex_rejected` の記録対象とする。
//!
//! 実際の送受信・候補登録・接続試行の配線は [`crate::p2p::runtime`] の責務で本モジュール外。
//! GET_PEERS/PEERS のフレーム型は [`crate::p2p::frame::Message`] を用いる。

use std::collections::HashSet;

use crate::p2p::peers::{PeerAddr, parse_addr};
use crate::store::PeerEndpoint;

/// `PEERS` で送出・受理する最大件数(contracts 検査 5 / research R14)。
pub const PEX_MAX_PEERS: usize = 64;

/// 送出する `PEERS` のアドレス列を選定する(canonical `host:port` の IP リテラル)。
///
/// **verified=1 かつ enabled** なピアのみを対象に、`last_ok_at` の新しい順(実績が無い
/// ものは最下位、同点は id 降順で安定化)に最大 `max` 件返す。未検証・無効ピアは含めない
/// (未検証ピアの再共有は禁止 — MUST NOT, research R14)。
///
/// **名前空間分離(ADR-0010)**: ホスト名自体は網に出さない。
/// - IP リテラルピア → `addr` をそのまま共有。
/// - ホスト名ピア → 接続成立時の実 IP `resolved_ip`(検証済み)へ射影して共有。**送出時に
///   ホスト名を再解決した IP を載せてはならない**(未検証 IP の触れ回りは R14 違反)。射影 IP が
///   無い(まだ外向き成功していない)ホスト名ピアは共有しない。
///
/// 射影で複数のホスト名が同一 IP に解決した場合や IP リテラルと重複した場合は重複排除する。
pub fn select_peers_for_pex(peers: &[PeerEndpoint], max: usize) -> Vec<String> {
    let mut eligible: Vec<&PeerEndpoint> =
        peers.iter().filter(|p| p.verified && p.enabled).collect();
    eligible.sort_by(|a, b| {
        b.last_ok_at
            .unwrap_or(i64::MIN)
            .cmp(&a.last_ok_at.unwrap_or(i64::MIN))
            .then_with(|| b.id.cmp(&a.id))
    });
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for p in eligible {
        let Some(shared) = project_for_pex(p) else {
            continue;
        };
        if seen.insert(shared.clone()) {
            out.push(shared);
            if out.len() >= max {
                break;
            }
        }
    }
    out
}

/// PEX で共有すべきアドレスへ射影する(ADR-0010 名前空間分離)。
///
/// IP リテラルピアは `addr` を、ホスト名ピアは `resolved_ip`(あれば)を返す。射影不能
/// (未接続ホスト名・解析不能)は `None`。
fn project_for_pex(peer: &PeerEndpoint) -> Option<String> {
    // 登録時に canonical 化・検証済みだが、解析不能な行は防御的に共有しない。
    let parsed = parse_addr(&peer.addr).ok()?;
    if parsed.is_hostname {
        peer.resolved_ip.clone()
    } else {
        Some(peer.addr.clone())
    }
}

/// 受信 `PEERS` の検証結果。
///
/// `accepted` は候補登録すべき正規化済みアドレス(重複・自アドレスを除外済み)。
/// 破棄した生アドレスは**破棄理由で 2 分類**して保持する(feature 005 / ADR-0013):
/// - `benign_rejected`: 良性(自己アドレス・重複)。健全な網で日常的に発生する反射・dual-stack
///   重複であり、`pex_rejected` セキュリティイベントの**対象外**(debug ログのみ)。
/// - `suspicious_rejected`: 不審(件数超過・形式不正・長さ超過・ホスト名)。protocol 逸脱・
///   不正入力であり、従来どおり `pex_rejected` としてセキュリティ記録する対象。
///
/// 分類は**破棄理由のみ**で決まり、破棄する/しない(候補登録の可否)の判定は一切変えない。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IncomingPex {
    /// 採用する候補(canonical 化・重複排除済み)。
    pub accepted: Vec<PeerAddr>,
    /// 良性理由(自己アドレス・重複)で破棄した生アドレス(記録対象外・debug のみ)。
    pub benign_rejected: Vec<String>,
    /// 不審理由(件数超過・形式不正・長さ超過・ホスト名)で破棄した生アドレス(記録対象)。
    pub suspicious_rejected: Vec<String>,
}

impl IncomingPex {
    /// 破棄した生アドレス全体(良性 + 不審)を連結して返す後方互換アクセサ。
    ///
    /// 旧 `rejected` フィールドの代替。破棄の総数・全一覧を扱いたい呼び出し側向け。
    pub fn rejected(&self) -> Vec<String> {
        let mut all =
            Vec::with_capacity(self.benign_rejected.len() + self.suspicious_rejected.len());
        all.extend(self.benign_rejected.iter().cloned());
        all.extend(self.suspicious_rejected.iter().cloned());
        all
    }

    /// 不審な破棄が 1 件でもあるか(`pex_rejected` セキュリティイベントの記録要否)。
    pub fn has_suspicious(&self) -> bool {
        !self.suspicious_rejected.is_empty()
    }

    /// 良性な破棄が 1 件でもあるか(debug ログ出力の要否)。
    pub fn has_benign(&self) -> bool {
        !self.benign_rejected.is_empty()
    }

    /// 破棄が 1 件でもあったか(良性・不審いずれか)。
    pub fn has_rejections(&self) -> bool {
        self.has_benign() || self.has_suspicious()
    }
}

/// 受信 `PEERS` を検証する(検査 5)。
///
/// - **件数 > `max`** は全体を破棄する(1 件も採用しない)。正当なピアは `max` 件以下しか
///   送らないため、超過は protocol 逸脱として扱う。
/// - 各要素を [`parse_addr`] で検証(長さ ≤256・IPv6 はブラケット表記のみ・形式)。
/// - **ホスト名は拒否する**(ADR-0010 名前空間分離: PEX/gossip の名前空間は IP リテラルのみ。
///   ホスト名は利用者ローカルの manual リストにのみ存在する)。
/// - `is_self`(canonical を受け取り自ノードアドレスなら true)に一致するものは破棄。
/// - バッチ内で canonical が重複するものは初出のみ採用し、以降は破棄。
///
/// 破棄したものは理由で分類して積む(feature 005 / ADR-0013):
/// - **良性**(`benign_rejected`): 自己アドレス一致・バッチ内重複。健全な網で常時発生する反射・
///   dual-stack 重複であり、セキュリティイベントではない。
/// - **不審**(`suspicious_rejected`): 件数超過(バッチ全破棄)・`parse_addr` 失敗(形式不正・
///   長さ超過・ブラケットなし複数コロン)・ホスト名(ADR-0010 名前空間分離違反)。protocol
///   逸脱・不正入力であり `pex_rejected` の記録対象。
///
/// **分類は破棄理由のみで決まり、破棄する/しない(候補登録の可否)は一切変更しない**
/// (FR-004: 防御は不変)。
pub fn validate_incoming_peers(
    peers: &[String],
    is_self: impl Fn(&str) -> bool,
    max: usize,
) -> IncomingPex {
    let mut result = IncomingPex::default();
    // 件数超過はバッチ全体を破棄する。正当なピアは max 件以下しか送らないため protocol 逸脱=不審。
    if peers.len() > max {
        result.suspicious_rejected = peers.to_vec();
        return result;
    }
    let mut seen: HashSet<String> = HashSet::new();
    for raw in peers {
        match parse_addr(raw) {
            // ホスト名候補は名前空間分離違反(ADR-0010)。不正入力として不審に分類。
            Ok(addr) if addr.is_hostname => result.suspicious_rejected.push(raw.clone()),
            Ok(addr) => {
                let canonical = addr.canonical();
                // 自己アドレス反射・バッチ内重複は健全な網で常時起きる良性破棄。
                if is_self(&canonical) || !seen.insert(canonical) {
                    result.benign_rejected.push(raw.clone());
                } else {
                    result.accepted.push(addr);
                }
            }
            // 形式不正・長さ超過・ブラケットなし複数コロンは不正入力=不審。
            Err(_) => result.suspicious_rejected.push(raw.clone()),
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
            resolved_ip: None,
        }
    }

    /// ホスト名ピア(source=manual・resolved_ip 付き)を組み立てる。
    fn hostname_peer(
        id: i64,
        addr: &str,
        resolved_ip: Option<&str>,
        last_ok_at: Option<i64>,
    ) -> PeerEndpoint {
        PeerEndpoint {
            id,
            addr: addr.to_string(),
            source: PeerSource::Manual,
            verified: true,
            enabled: true,
            added_at: 0,
            last_ok_at,
            fail_count: 0,
            resolved_ip: resolved_ip.map(str::to_string),
        }
    }

    #[test]
    fn select_verified_only_by_recency() {
        // PEX は IP リテラルを共有する(ホスト名は網に出さない — ADR-0010)。
        let peers = vec![
            peer(1, "192.0.2.1:7147", true, true, Some(100)),
            peer(2, "192.0.2.2:7147", true, true, Some(300)),
            peer(3, "192.0.2.3:7147", true, true, Some(200)),
            peer(4, "192.0.2.4:7147", false, true, Some(999)), // 未検証 → 除外
            peer(5, "192.0.2.5:7147", true, false, Some(999)), // 無効 → 除外
            peer(6, "192.0.2.6:7147", true, true, None),       // 実績なし → 最下位
        ];
        assert_eq!(
            select_peers_for_pex(&peers, PEX_MAX_PEERS),
            vec![
                "192.0.2.2:7147".to_string(),
                "192.0.2.3:7147".to_string(),
                "192.0.2.1:7147".to_string(),
                "192.0.2.6:7147".to_string(),
            ]
        );
    }

    #[test]
    fn select_excludes_all_unverified() {
        let peers = vec![
            peer(1, "192.0.2.1:7147", false, true, Some(100)),
            peer(2, "192.0.2.2:7147", false, true, Some(200)),
        ];
        assert!(select_peers_for_pex(&peers, PEX_MAX_PEERS).is_empty());
    }

    #[test]
    fn select_caps_and_prefers_recent() {
        let peers: Vec<PeerEndpoint> = (0..70)
            .map(|i| peer(i, &format!("10.0.0.{i}:7147"), true, true, Some(i)))
            .collect();
        let selected = select_peers_for_pex(&peers, PEX_MAX_PEERS);
        assert_eq!(selected.len(), PEX_MAX_PEERS);
        assert_eq!(selected[0], "10.0.0.69:7147");
        assert!(!selected.contains(&"10.0.0.0:7147".to_string()));
    }

    #[test]
    fn select_projects_hostname_to_resolved_ip() {
        // ホスト名ピアは resolved_ip へ射影して共有し、ホスト名自体は網に出さない。
        let peers = vec![
            hostname_peer(
                1,
                "seed.example.org:7147",
                Some("192.0.2.10:7147"),
                Some(300),
            ),
            peer(2, "198.51.100.5:7147", true, true, Some(200)),
            // resolved_ip 未取得のホスト名ピアは共有しない。
            hostname_peer(3, "pending.example.org:7147", None, Some(400)),
        ];
        let selected = select_peers_for_pex(&peers, PEX_MAX_PEERS);
        assert_eq!(selected, vec!["192.0.2.10:7147", "198.51.100.5:7147"]);
        assert!(
            !selected.iter().any(|s| s.contains("example.org")),
            "ホスト名は共有されない"
        );
    }

    #[test]
    fn select_dedups_projected_ips() {
        // ホスト名の射影 IP が IP リテラルピアと重複したら初出のみ。
        let peers = vec![
            hostname_peer(
                1,
                "seed.example.org:7147",
                Some("192.0.2.10:7147"),
                Some(300),
            ),
            peer(2, "192.0.2.10:7147", true, true, Some(200)),
        ];
        let selected = select_peers_for_pex(&peers, PEX_MAX_PEERS);
        assert_eq!(selected, vec!["192.0.2.10:7147"]);
    }

    #[test]
    fn incoming_rejects_hostnames() {
        // 名前空間分離: PEX 受信はホスト名候補を拒否し IP リテラルのみ候補化する。
        let peers = vec![
            "192.0.2.10:7147".to_string(),
            "seed.example.org:7147".to_string(),
            "[2001:db8::1]:7147".to_string(),
        ];
        let r = validate_incoming_peers(&peers, |_| false, PEX_MAX_PEERS);
        let accepted: Vec<String> = r.accepted.iter().map(|p| p.canonical()).collect();
        assert_eq!(accepted, vec!["192.0.2.10:7147", "[2001:db8::1]:7147"]);
        assert_eq!(r.rejected(), vec!["seed.example.org:7147".to_string()]);
        // ホスト名は名前空間分離違反=不審に分類される。
        assert!(r.has_suspicious());
        assert!(!r.has_benign());
    }

    #[test]
    fn incoming_over_max_rejected_wholesale() {
        let peers: Vec<String> = (0..PEX_MAX_PEERS + 1)
            .map(|i| format!("10.0.0.{}:7147", i % 250 + 1))
            .collect();
        let r = validate_incoming_peers(&peers, |_| false, PEX_MAX_PEERS);
        assert!(r.accepted.is_empty());
        assert_eq!(r.rejected().len(), peers.len());
        // 件数超過は不審(全件)。
        assert_eq!(r.suspicious_rejected.len(), peers.len());
        assert!(!r.has_benign());
    }

    #[test]
    fn incoming_malformed_rejected() {
        let peers = vec![
            "not-an-addr".to_string(),
            "host:0".to_string(),
            "2001:db8::1:7147".to_string(), // ブラケットなし複数コロン
            format!("{}:7147", "a".repeat(256)), // 長さ超過
        ];
        let r = validate_incoming_peers(&peers, |_| false, PEX_MAX_PEERS);
        assert!(r.accepted.is_empty());
        assert_eq!(r.rejected().len(), peers.len());
        // 形式不正・長さ超過は不審。
        assert_eq!(r.suspicious_rejected.len(), peers.len());
        assert!(!r.has_benign());
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
        let r = validate_incoming_peers(&peers, |c| c == "203.0.113.5:7147", PEX_MAX_PEERS);
        assert_eq!(r.accepted.len(), 2);
        assert_eq!(r.rejected().len(), 3);
        assert!(r.has_rejections());
        // 自己アドレス・重複のみ = 良性。不審は 0 件(pex_rejected 非対象)。
        assert_eq!(r.benign_rejected.len(), 3);
        assert!(!r.has_suspicious());
        assert!(r.has_benign());
    }
}

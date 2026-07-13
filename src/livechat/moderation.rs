//! NG/BAN(T041)
//!
//! `livechat_moderation` テーブル CRUD(kind = Ng/Ban/ConnBan・完全鍵照合・
//! 板単位スコープ・ネットワーク非送出 = 不変条件 M1)を担う。
//!
//! - **FR-018 完全鍵照合**: [`Moderation::is_banned`]/[`Moderation::is_ng`]/
//!   [`Moderation::is_conn_banned`] は `target` と照合対象の**完全一致**(`==`)のみで
//!   判定する。表示用の短縮 ID(先頭 8 文字等)による照合は行わない — 短縮表示が
//!   同じ別の鍵に誤って NG/BAN が適用されると、無関係な利用者を巻き込む事故になる。
//! - **不変条件 M1**: 本型・[`crate::store::Store`] の NG/BAN 系 API はネットワークへ
//!   一切送出しない。NG/BAN はローカル(自ノード)専用の判定情報であり、他ノードへ
//!   同期・公開する経路を持たない(送出用の API 自体が存在しない)。

use std::sync::Arc;

use crate::store::{ModerationEntry, ModerationKind, Store, StoreError};

/// NG/BAN のドメイン層(store の CRUD をラップし、完全鍵照合の判定を提供する)。
pub struct Moderation {
    store: Arc<Store>,
}

impl Moderation {
    /// ストアを共有して作る。
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    /// 板鍵を BAN する(スレ主 — 採番拒否。thread-events.md 検証 5)。
    pub fn ban_key(&self, board_id: &str, board_key: &str) -> Result<ModerationEntry, StoreError> {
        self.store
            .insert_moderation(board_id, ModerationKind::Ban, board_key)
    }

    /// 接続元アドレスを BAN する(スレ主 — 接続拒否。FR-019)。
    pub fn ban_connection(
        &self,
        board_id: &str,
        addr: &str,
    ) -> Result<ModerationEntry, StoreError> {
        self.store
            .insert_moderation(board_id, ModerationKind::ConnBan, addr)
    }

    /// 板鍵を NG にする(視聴者 — ローカル非表示。FR-020)。
    pub fn add_ng(&self, board_id: &str, board_key: &str) -> Result<ModerationEntry, StoreError> {
        self.store
            .insert_moderation(board_id, ModerationKind::Ng, board_key)
    }

    /// NG/BAN を解除する(id 指定)。削除できれば `true`。
    pub fn remove(&self, id: i64) -> Result<bool, StoreError> {
        self.store.delete_moderation(id)
    }

    /// 指定板の NG/BAN 一覧(id 昇順)。
    pub fn list(&self, board_id: &str) -> Result<Vec<ModerationEntry>, StoreError> {
        self.store.list_moderation(board_id)
    }

    /// 指定板鍵が BAN 済みか(完全一致のみ — 短縮 ID 非適用。FR-018)。
    ///
    /// 一覧の照会に失敗した場合は `false`(適用しない)を返す。NG/BAN は
    /// **ローカル判定の付加情報**であり、照会失敗時に「安全側」として一律 BAN 扱いに
    /// 倒すと、ストア障害時に正当な書き込みまで無差別に拒否してしまう。ローカル情報の
    /// 欠落は「モデレーションなし」相当として扱うのが妥当(不変条件 M1 の帰結 — 本来
    /// ネットワークに存在しない情報の欠落で他ノードとの整合性が崩れるわけではない)。
    pub fn is_banned(&self, board_id: &str, board_key: &str) -> bool {
        self.list(board_id)
            .map(|entries| {
                entries
                    .iter()
                    .any(|e| e.kind == ModerationKind::Ban && e.target == board_key)
            })
            .unwrap_or(false)
    }

    /// 指定接続元アドレスが ConnBan 済みか(完全一致のみ)。
    pub fn is_conn_banned(&self, board_id: &str, addr: &str) -> bool {
        self.list(board_id)
            .map(|entries| {
                entries
                    .iter()
                    .any(|e| e.kind == ModerationKind::ConnBan && e.target == addr)
            })
            .unwrap_or(false)
    }

    /// 指定板鍵が NG 済みか(完全一致のみ — 短縮 ID 非適用。FR-018/FR-020)。
    pub fn is_ng(&self, board_id: &str, board_key: &str) -> bool {
        self.list(board_id)
            .map(|entries| {
                entries
                    .iter()
                    .any(|e| e.kind == ModerationKind::Ng && e.target == board_key)
            })
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOARD_A: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const BOARD_B: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
    const KEY_A: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const KEY_B: &str = "2222222222222222222222222222222222222222222222222222222222222222";

    fn moderation() -> Moderation {
        Moderation::new(Arc::new(Store::open_in_memory().unwrap()))
    }

    #[test]
    fn ban_key_applies_and_lifts() {
        let m = moderation();
        assert!(!m.is_banned(BOARD_A, KEY_A));
        m.ban_key(BOARD_A, KEY_A).unwrap();
        assert!(m.is_banned(BOARD_A, KEY_A));
        // 別鍵には適用されない。
        assert!(!m.is_banned(BOARD_A, KEY_B));

        // 解除(id 指定)で BAN が外れる。
        let entry = m.list(BOARD_A).unwrap().into_iter().next().unwrap();
        assert!(m.remove(entry.id).unwrap());
        assert!(!m.is_banned(BOARD_A, KEY_A));
    }

    #[test]
    fn ban_connection_applies_by_addr() {
        let m = moderation();
        assert!(!m.is_conn_banned(BOARD_A, "203.0.113.5:7147"));
        m.ban_connection(BOARD_A, "203.0.113.5:7147").unwrap();
        assert!(m.is_conn_banned(BOARD_A, "203.0.113.5:7147"));
        // 別アドレスには適用されない。
        assert!(!m.is_conn_banned(BOARD_A, "203.0.113.6:7147"));
    }

    #[test]
    fn add_ng_applies_and_lifts() {
        let m = moderation();
        assert!(!m.is_ng(BOARD_A, KEY_A));
        m.add_ng(BOARD_A, KEY_A).unwrap();
        assert!(m.is_ng(BOARD_A, KEY_A));

        let entry = m.list(BOARD_A).unwrap().into_iter().next().unwrap();
        assert!(m.remove(entry.id).unwrap());
        assert!(!m.is_ng(BOARD_A, KEY_A));
    }

    #[test]
    fn full_key_match_does_not_apply_to_short_id_collision() {
        // FR-018: 表示用の短縮 ID(先頭 8 文字)が同じでも、完全鍵が異なれば非適用。
        let m = moderation();
        let short_prefix = &KEY_A[..8];
        let colliding_key = format!("{short_prefix}{}", "9".repeat(56));
        assert_ne!(colliding_key, KEY_A, "テスト前提: 完全鍵は異なる");
        assert_eq!(
            &colliding_key[..8],
            short_prefix,
            "テスト前提: 短縮 ID 表示は一致"
        );

        m.ban_key(BOARD_A, KEY_A).unwrap();
        assert!(m.is_banned(BOARD_A, KEY_A));
        assert!(
            !m.is_banned(BOARD_A, &colliding_key),
            "短縮 ID が同じ別鍵には BAN を適用しない"
        );
    }

    #[test]
    fn moderation_is_scoped_per_board() {
        let m = moderation();
        m.ban_key(BOARD_A, KEY_A).unwrap();
        // 別板スコープには影響しない(板単位スコープ)。
        assert!(!m.is_banned(BOARD_B, KEY_A));
    }

    #[test]
    fn kinds_do_not_cross_apply() {
        // Ng で登録した鍵は is_banned では検出されない(kind ごとに独立)。
        let m = moderation();
        m.add_ng(BOARD_A, KEY_A).unwrap();
        assert!(m.is_ng(BOARD_A, KEY_A));
        assert!(!m.is_banned(BOARD_A, KEY_A));
    }
}

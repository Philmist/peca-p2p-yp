//! 板鍵の管理基盤(T012 — research R8 / FR-016)
//!
//! 板鍵 = 自分の書き込み鍵。板 = 配信者ペルソナ単位で 1 本(`board_id` = スレ主ペルソナの
//! 公開鍵をスコープとする)。本モジュールは:
//!
//! - **鍵ペア生成**(`nostr` の乱数生成に委譲。ペルソナ鍵と導出関係を持たない)
//! - **keystore 抽象**([`Keystore`] — Windows DPAPI / Linux エンベロープ、ADR-0003/0009)に
//!   よる秘密鍵の暗号化(平文保存 MUST NOT)
//! - **`board_keys` テーブル CRUD**(生成・取得・ローテーション・削除)
//! - **ペルソナとの構造分離**: `personas` テーブルとは識別子・外部キーを一切共有しない
//!   (誤結合・誤表示によるリンク事故の構造的防止 — FR-016)
//! - **エクスポート機能なし**: 板鍵は端末ローカルの使い捨て身元であり、nsec 表示等の
//!   持ち出し経路を設けない(漏洩・リンクリスク > 持ち出し需要 — research R8)
//!
//! ローテーション(FR-017 — T044)は行ごと置換で旧鍵を破棄する。初回書き込み PoW(T044)・
//! 設定配布(T032)は本基盤の上に構築する。

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use nostr::{Keys, SecretKey};

use crate::identity::Keystore;
use crate::store::{Store, StoreError};

/// 板鍵管理のエラー。`Display` は内部情報を漏らさない(Principle II)。
#[derive(Debug)]
pub enum BoardKeyError {
    /// 保管された板鍵を復号できない(エンベロープ破損・保護鍵消失・他プラットフォーム)。
    Unusable,
    /// 鍵の保護(暗号化)に失敗した(内部詳細は含めない)。
    Crypto,
    /// 永続層のエラー。
    Store(StoreError),
}

impl std::fmt::Display for BoardKeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BoardKeyError::Unusable => write!(f, "この板鍵は利用できません(復号失敗)"),
            BoardKeyError::Crypto => write!(f, "板鍵の保護処理に失敗しました"),
            BoardKeyError::Store(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for BoardKeyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            BoardKeyError::Store(e) => Some(e),
            _ => None,
        }
    }
}

impl From<StoreError> for BoardKeyError {
    fn from(e: StoreError) -> Self {
        BoardKeyError::Store(e)
    }
}

/// 板鍵マネージャ(`Arc<Store>` 共有・keystore 状態を保持)。
///
/// [`IdentityManager`](crate::identity::IdentityManager) とは**別系統**であり、ペルソナ鍵とは
/// テーブル・識別子を共有しない(FR-016)。
pub struct BoardKeyManager {
    store: Arc<Store>,
    /// 秘密鍵保護の入口(プラットフォーム状態を保持 — ADR-0009)。ペルソナと同じ保護経路を
    /// 再利用するが、保管先テーブル([`board_keys`])は分離している。
    keystore: Keystore,
}

impl BoardKeyManager {
    /// マネージャを作成する。
    ///
    /// `keystore` は本番では data-dir から初期化した [`Keystore`]、テストでは
    /// [`Keystore::ephemeral`] を明示的に渡す(ペルソナ管理と同一方針)。
    pub fn new(store: Arc<Store>, keystore: Keystore) -> Self {
        Self { store, keystore }
    }

    /// 指定板の署名鍵を返す(なければ生成して保存する — T029 の自動署名の基盤)。
    ///
    /// 既存行があれば復号して返す。復号に失敗する場合は黙って再生成せず
    /// [`BoardKeyError::Unusable`] を返す(保護鍵消失を鍵の使い捨てで隠さない)。
    pub fn signing_keys(&self, board_id: &str) -> Result<Keys, BoardKeyError> {
        if let Some(row) = self.store.get_board_key(board_id)? {
            let secret = self
                .keystore
                .unprotect(&row.secret_enc)
                .map_err(|_| BoardKeyError::Unusable)?;
            let secret_key = SecretKey::from_slice(&secret).map_err(|_| BoardKeyError::Unusable)?;
            return Ok(Keys::new(secret_key));
        }
        self.generate(board_id)
    }

    /// 指定板の板鍵公開鍵を返す(未生成は `None`)。ID 表示・NG/BAN 完全鍵照合の参照用。
    pub fn existing_pubkey(&self, board_id: &str) -> Result<Option<String>, BoardKeyError> {
        Ok(self.store.get_board_key(board_id)?.map(|row| row.pubkey))
    }

    /// 板鍵をローテーションする(明示操作 — FR-017)。
    ///
    /// 新しい鍵ペアを生成して行ごと置換する(旧鍵は破棄され、復元できない)。
    pub fn rotate(&self, board_id: &str) -> Result<Keys, BoardKeyError> {
        self.generate(board_id)
    }

    /// 板鍵を削除する。削除できれば `true`。
    pub fn delete(&self, board_id: &str) -> Result<bool, BoardKeyError> {
        Ok(self.store.delete_board_key(board_id)?)
    }

    /// 新規板鍵を生成し、暗号化して保存する(既存行は置換)。
    fn generate(&self, board_id: &str) -> Result<Keys, BoardKeyError> {
        let keys = Keys::generate();
        let pubkey = keys.public_key().to_hex();
        let secret_enc = self
            .keystore
            .protect(keys.secret_key().as_secret_bytes())
            .map_err(|_| BoardKeyError::Crypto)?;
        self.store
            .upsert_board_key(board_id, &pubkey, &secret_enc, unix_now())?;
        Ok(keys)
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOARD_A: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const BOARD_B: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

    fn manager() -> BoardKeyManager {
        BoardKeyManager::new(
            Arc::new(Store::open_in_memory().unwrap()),
            Keystore::ephemeral(),
        )
    }

    #[test]
    fn signing_keys_generates_and_persists() {
        let m = manager();
        assert!(m.existing_pubkey(BOARD_A).unwrap().is_none());
        let keys = m.signing_keys(BOARD_A).unwrap();
        // 生成された公開鍵が保存され、参照できる。
        assert_eq!(
            m.existing_pubkey(BOARD_A).unwrap().as_deref(),
            Some(keys.public_key().to_hex().as_str())
        );
    }

    #[test]
    fn signing_keys_is_stable_across_calls() {
        let m = manager();
        let first = m.signing_keys(BOARD_A).unwrap();
        let second = m.signing_keys(BOARD_A).unwrap();
        // 同一板では同じ鍵を返す(get_or_create の再取得)。
        assert_eq!(first.public_key(), second.public_key());
    }

    #[test]
    fn distinct_boards_get_distinct_keys() {
        let m = manager();
        let a = m.signing_keys(BOARD_A).unwrap();
        let b = m.signing_keys(BOARD_B).unwrap();
        assert_ne!(a.public_key(), b.public_key());
    }

    #[test]
    fn rotate_replaces_key_and_discards_old() {
        let m = manager();
        let old = m.signing_keys(BOARD_A).unwrap();
        let new = m.rotate(BOARD_A).unwrap();
        assert_ne!(old.public_key(), new.public_key(), "新鍵は旧鍵と異なる");
        // 行は 1 本のまま置換され、以後は新鍵を返す(旧鍵破棄 — FR-017)。
        assert_eq!(
            m.signing_keys(BOARD_A).unwrap().public_key(),
            new.public_key()
        );
    }

    #[test]
    fn undecryptable_key_is_unusable_not_silently_regenerated() {
        // 復号できない板鍵行を直接挿入すると Unusable(黙って再生成しない)。
        let store = Arc::new(Store::open_in_memory().unwrap());
        let m = BoardKeyManager::new(Arc::clone(&store), Keystore::ephemeral());
        store
            .upsert_board_key(BOARD_A, BOARD_B, b"garbage-not-an-envelope", 100)
            .unwrap();
        assert!(matches!(
            m.signing_keys(BOARD_A),
            Err(BoardKeyError::Unusable)
        ));
    }

    #[test]
    fn delete_removes_board_key() {
        let m = manager();
        m.signing_keys(BOARD_A).unwrap();
        assert!(m.delete(BOARD_A).unwrap());
        assert!(m.existing_pubkey(BOARD_A).unwrap().is_none());
        assert!(!m.delete(BOARD_A).unwrap());
    }

    #[test]
    fn board_keys_do_not_create_personas() {
        // 構造分離(FR-016): 板鍵の生成はペルソナテーブルへ一切書き込まない。
        let store = Arc::new(Store::open_in_memory().unwrap());
        let m = BoardKeyManager::new(Arc::clone(&store), Keystore::ephemeral());
        m.signing_keys(BOARD_A).unwrap();
        assert!(store.list_personas().unwrap().is_empty());
        // board_key の pubkey は persona として引けない(識別子非共有)。
        let bk = m.existing_pubkey(BOARD_A).unwrap().unwrap();
        assert!(store.get_persona_by_pubkey(&bk).unwrap().is_none());
    }
}

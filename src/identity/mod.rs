//! ペルソナ鍵管理(T028 — ADR-0003 準拠)
//!
//! - 鍵生成は `nostr` クレートの乱数生成に委ね、ペルソナ間で導出関係のある鍵を使わない
//!   (ADR-0003 §6 — リンク推定防止)
//! - 秘密鍵は keystore(プラットフォーム保護 — Windows は DPAPI、Linux は master.key +
//!   XChaCha20-Poly1305)で暗号化したエンベロープのみを SQLite に保存する(平文保存
//!   MUST NOT — data-model §Persona、ADR-0008)
//! - 復号失敗(エンベロープ破損・別アカウント・保護鍵消失・他プラットフォーム持込み)は
//!   当該ペルソナを「利用不可」として扱い、起動・他機能は継続する(ADR-0003 §4)
//! - 破棄 = 行削除。復元手段は提供しない(ADR-0003 §3)
//! - nsec エクスポートの本文は呼び出し側(API 層)が応答にのみ使い、
//!   ログ・セキュリティイベントへ記録してはならない (MUST NOT — ADR-0003 §2)
//!
//! チャンネルへの割当(channel_id → pubkey)はメモリ上の対応表で管理する
//! (AnnouncedChannel は揮発エンティティ — data-model)。「現在選択中」ペルソナは
//! settings テーブルのキー [`SELECTED_PERSONA_KEY`] で永続化する(UI 誤爆防止の明示表示用)。

pub mod keystore;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use nostr::nips::nip19::ToBech32;
use nostr::{Keys, SecretKey};

use crate::store::{PersonaState, Store, StoreError};

pub use keystore::{Keystore, KeystoreInit};

/// 「現在選択中」ペルソナを保存する settings キー。
pub const SELECTED_PERSONA_KEY: &str = "selected_persona";

/// 共有保管物(master.key・DB のパーミッション)起因の健全性(data-model §KeystoreHealth)。
///
/// 起動時パーミッション検査(`platform::enforce_permissions`)と keystore 初期化
/// (`KeystoreInit`)の結果から導く。`Unavailable` は個別ペルソナの復号可否と独立に
/// **全ペルソナを利用不可**へ倒す(パーミッション是正不能時は復号自体は成立し得るが、
/// at-rest 保護が崩れているため利用させない — contracts/cli-config.md §4)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeystoreHealth {
    /// 保管物は健全(是正済み含む)。個別ペルソナの復号可否で `usable` を判定する。
    Ok,
    /// 共有保管物が是正不能・master.key 破損等。全ペルソナ `usable:false`・鍵操作は
    /// 「利用不可」エラー。発見・伝搬(US1)は非影響(MUST — FR-013)。
    Unavailable(UnavailableCause),
}

/// [`KeystoreHealth::Unavailable`] の原因(利用者がログから区別できること — key-envelope §5)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnavailableCause {
    /// master.key が破損している(サイズ不一致)。
    MasterKeyCorrupt,
    /// 保管ファイルのパーミッションを是正できない。
    PermissionUnfixable,
}

impl KeystoreHealth {
    /// パーミッション検査結果(健全か)と keystore 初期化結果から健全性を導く。
    ///
    /// パーミッション是正不能を最優先で `Unavailable` にする(復号は成立し得るが at-rest
    /// 保護が崩れているため)。次いで master.key 破損。`CreatedMissingSuspected`(保護鍵
    /// 消失疑い)は新しい master.key で keystore 自体は機能するため `Ok` とし、既存
    /// scheme 0x02 ペルソナは個別に復号失敗して `usable:false` になる(key-envelope §5 の
    /// 影響範囲「既存ペルソナ」に一致)。消失疑いの定型警告は生成時点で記録済み。
    pub fn evaluate(permission_healthy: bool, init: KeystoreInit) -> Self {
        if !permission_healthy {
            KeystoreHealth::Unavailable(UnavailableCause::PermissionUnfixable)
        } else if init == KeystoreInit::Corrupt {
            KeystoreHealth::Unavailable(UnavailableCause::MasterKeyCorrupt)
        } else {
            KeystoreHealth::Ok
        }
    }

    /// 共有保管物起因で全ペルソナ利用不可か。
    fn is_unavailable(self) -> bool {
        matches!(self, KeystoreHealth::Unavailable(_))
    }
}

/// ペルソナ管理のエラー。
#[derive(Debug)]
pub enum IdentityError {
    /// 指定 pubkey のペルソナが存在しない。
    NotFound,
    /// keystore 復号に失敗した(ペルソナ利用不可 — ADR-0003 §4)。
    Unusable,
    /// keystore 暗号化・鍵構築の失敗(内部詳細は含めない — Principle II)。
    Crypto,
    /// 永続層のエラー。
    Store(StoreError),
}

impl std::fmt::Display for IdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IdentityError::NotFound => write!(f, "ペルソナが見つかりません"),
            IdentityError::Unusable => write!(f, "このペルソナは利用できません(復号失敗)"),
            IdentityError::Crypto => write!(f, "鍵の保護処理に失敗しました"),
            IdentityError::Store(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for IdentityError {}

impl From<StoreError> for IdentityError {
    fn from(e: StoreError) -> Self {
        IdentityError::Store(e)
    }
}

/// API・UI 向けのペルソナ表示情報(秘密鍵を含まない)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonaInfo {
    /// nostr 公開鍵(hex 64 小文字)。
    pub pubkey: String,
    /// ローカル表示名(ネットワークに出さない — FR-013)。
    pub label: String,
    /// active / archived。
    pub state: PersonaState,
    /// keystore 復号可能か(false = 利用不可表示 — ADR-0003 §4)。
    pub usable: bool,
    /// 作成時刻(unix 秒)。
    pub created_at: i64,
    /// 現在選択中(新規掲載の既定署名鍵)か。
    pub selected: bool,
}

/// ペルソナ管理(`Arc` 共有・Send+Sync)。
pub struct IdentityManager {
    store: Arc<Store>,
    /// 鍵保護の入口(プラットフォーム状態を保持 — ADR-0008)。
    keystore: Keystore,
    /// 共有保管物起因の健全性(起動時検査から確定 — T020)。
    health: KeystoreHealth,
    /// チャンネルへの割当(channel_id hex32 → pubkey hex64)。揮発。
    assignments: Mutex<HashMap<String, String>>,
}

/// ポイズン時も内部値を回収してロックを返す(パニックしない)。
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl IdentityManager {
    /// マネージャを作成する。
    ///
    /// `keystore` は本番では data-dir から初期化した [`Keystore`]([`Keystore::open`])、
    /// テストでは [`Keystore::ephemeral`] を明示的に渡す。ephemeral を暗黙に用いる
    /// 経路は設けない(鍵取扱いの安全性 — 本番で誤って揮発鍵を使わないため)。
    ///
    /// 健全性は [`KeystoreHealth::Ok`]。共有保管物起因の劣化を反映する場合は
    /// [`new_with_health`](Self::new_with_health) を使う。
    pub fn new(store: Arc<Store>, keystore: Keystore) -> Self {
        Self::new_with_health(store, keystore, KeystoreHealth::Ok)
    }

    /// 起動時検査で確定した [`KeystoreHealth`] を反映してマネージャを作成する(T021)。
    ///
    /// `Unavailable` のとき全ペルソナは `usable:false`、鍵操作(作成・署名・エクスポート・
    /// 破棄)は「利用不可」エラーになる。発見・伝搬は非影響(FR-013)。
    pub fn new_with_health(store: Arc<Store>, keystore: Keystore, health: KeystoreHealth) -> Self {
        Self {
            store,
            keystore,
            health,
            assignments: Mutex::new(HashMap::new()),
        }
    }

    /// ペルソナを新規作成する(鍵生成 → keystore 暗号化 → 保存)。
    pub fn create(&self, label: &str) -> Result<PersonaInfo, IdentityError> {
        // 共有保管物が利用不可なら鍵操作は「利用不可」エラー(FR-013)。
        if self.health.is_unavailable() {
            return Err(IdentityError::Unusable);
        }
        let keys = Keys::generate();
        let pubkey = keys.public_key().to_hex();
        let secret_enc = self.keystore.protect(keys.secret_key().as_secret_bytes())?;
        let persona = self.store.insert_persona(&pubkey, &secret_enc, label)?;
        // 最初のペルソナは自動的に選択中とする(UI が必ず 1 つ明示できるように)。
        if self.selected()?.is_none() {
            self.select(&pubkey)?;
        }
        Ok(PersonaInfo {
            pubkey: persona.pubkey,
            label: persona.label,
            state: persona.state,
            usable: true,
            created_at: persona.created_at,
            selected: self.selected()? == Some(pubkey),
        })
    }

    /// 全ペルソナを列挙する(利用可否は keystore 復号の試行で判定)。
    ///
    /// 共有保管物が利用不可([`KeystoreHealth::Unavailable`])のときは、個別の復号可否に
    /// かかわらず全ペルソナを `usable:false` にする(FR-013)。
    pub fn list(&self) -> Result<Vec<PersonaInfo>, IdentityError> {
        let selected = self.selected()?;
        let personas = self.store.list_personas()?;
        let available = !self.health.is_unavailable();
        Ok(personas
            .into_iter()
            .map(|p| {
                let usable = available && self.keystore.unprotect(&p.secret_enc).is_ok();
                PersonaInfo {
                    selected: selected.as_deref() == Some(p.pubkey.as_str()),
                    pubkey: p.pubkey,
                    label: p.label,
                    state: p.state,
                    usable,
                    created_at: p.created_at,
                }
            })
            .collect())
    }

    /// 表示名を変更する。
    pub fn set_label(&self, pubkey: &str, label: &str) -> Result<(), IdentityError> {
        if self.store.update_persona_label(pubkey, label)? {
            Ok(())
        } else {
            Err(IdentityError::NotFound)
        }
    }

    /// 状態(active ⇄ archived)を変更する。
    pub fn set_state(&self, pubkey: &str, state: PersonaState) -> Result<(), IdentityError> {
        if self.store.update_persona_state(pubkey, state)? {
            Ok(())
        } else {
            Err(IdentityError::NotFound)
        }
    }

    /// 「現在選択中」ペルソナを設定する(新規掲載の既定署名鍵 — UI 誤爆防止)。
    pub fn select(&self, pubkey: &str) -> Result<(), IdentityError> {
        if self.store.get_persona_by_pubkey(pubkey)?.is_none() {
            return Err(IdentityError::NotFound);
        }
        self.store.set_setting(SELECTED_PERSONA_KEY, pubkey)?;
        Ok(())
    }

    /// 「現在選択中」ペルソナの pubkey。未選択・破棄済みなら `None`。
    pub fn selected(&self) -> Result<Option<String>, IdentityError> {
        let Some(pubkey) = self.store.get_setting(SELECTED_PERSONA_KEY)? else {
            return Ok(None);
        };
        // 破棄済みペルソナが残っていたら選択解除扱いにする。
        if self.store.get_persona_by_pubkey(&pubkey)?.is_none() {
            return Ok(None);
        }
        Ok(Some(pubkey))
    }

    /// チャンネルへペルソナを割り当てる(掲載中の再割当は掲載エンジンが検出して
    /// 旧ペルソナの ended 発行を行う — T029)。
    pub fn assign_channel(&self, channel_id: &str, pubkey: &str) -> Result<(), IdentityError> {
        if self.store.get_persona_by_pubkey(pubkey)?.is_none() {
            return Err(IdentityError::NotFound);
        }
        lock(&self.assignments).insert(channel_id.to_ascii_lowercase(), pubkey.to_string());
        Ok(())
    }

    /// チャンネルに使う署名ペルソナ(割当 → 選択中の順で解決)。
    pub fn persona_for_channel(&self, channel_id: &str) -> Result<Option<String>, IdentityError> {
        if let Some(pk) = lock(&self.assignments)
            .get(&channel_id.to_ascii_lowercase())
            .cloned()
        {
            // 割当先が破棄済みなら選択中へフォールバックする。
            if self.store.get_persona_by_pubkey(&pk)?.is_some() {
                return Ok(Some(pk));
            }
        }
        self.selected()
    }

    /// ペルソナを破棄する(行削除 — 復元不可)。割当・選択からも取り除く。
    pub fn delete(&self, pubkey: &str) -> Result<(), IdentityError> {
        // 共有保管物が利用不可なら鍵操作は「利用不可」エラー(FR-013)。
        if self.health.is_unavailable() {
            return Err(IdentityError::Unusable);
        }
        if !self.store.delete_persona(pubkey)? {
            return Err(IdentityError::NotFound);
        }
        lock(&self.assignments).retain(|_, v| v != pubkey);
        Ok(())
    }

    /// 署名用の鍵ペアをロードする(掲載エンジン用)。復号失敗は利用不可。
    ///
    /// 共有保管物が利用不可なら復号可否によらず「利用不可」エラーになる(FR-013 —
    /// エクスポートも本関数を経由するため同様)。
    pub fn signing_keys(&self, pubkey: &str) -> Result<Keys, IdentityError> {
        if self.health.is_unavailable() {
            return Err(IdentityError::Unusable);
        }
        let persona = self
            .store
            .get_persona_by_pubkey(pubkey)?
            .ok_or(IdentityError::NotFound)?;
        let secret = self.keystore.unprotect(&persona.secret_enc)?;
        let secret_key = SecretKey::from_slice(&secret).map_err(|_| IdentityError::Unusable)?;
        Ok(Keys::new(secret_key))
    }

    /// nsec(bech32)をエクスポートする。
    ///
    /// 戻り値は API 応答本文にのみ使うこと。ログ・セキュリティイベントへの記録は
    /// MUST NOT(ADR-0003 §2 — 呼び出し側の責務)。
    pub fn export_nsec(&self, pubkey: &str) -> Result<String, IdentityError> {
        let keys = self.signing_keys(pubkey)?;
        keys.secret_key()
            .to_bech32()
            .map_err(|_| IdentityError::Crypto)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager() -> IdentityManager {
        IdentityManager::new(
            Arc::new(Store::open_in_memory().unwrap()),
            Keystore::ephemeral(),
        )
    }

    #[test]
    fn keystore_roundtrip() {
        let ks = Keystore::ephemeral();
        let plain = b"secret-bytes-0123456789abcdef";
        let enc = ks.protect(plain).unwrap();
        assert_ne!(
            enc.as_slice(),
            plain.as_slice(),
            "暗号化表現は平文と一致してはならない"
        );
        let dec = ks.unprotect(&enc).unwrap();
        assert_eq!(dec.as_slice(), plain);
    }

    #[test]
    fn corrupted_blob_is_unusable() {
        let ks = Keystore::ephemeral();
        let enc = ks.protect(b"secret").unwrap();
        let mut broken = enc.clone();
        let last = broken.len() - 1;
        broken[last] ^= 0xFF;
        assert!(matches!(
            ks.unprotect(&broken),
            Err(IdentityError::Unusable)
        ));
    }

    #[test]
    fn create_list_and_first_persona_is_selected() {
        let m = manager();
        let a = m.create("メイン").unwrap();
        assert!(a.selected, "最初のペルソナは自動選択される");
        let b = m.create("サブ").unwrap();
        assert!(!b.selected);

        let list = m.list().unwrap();
        assert_eq!(list.len(), 2);
        assert!(list.iter().all(|p| p.usable));
        assert_eq!(
            list.iter().filter(|p| p.selected).count(),
            1,
            "選択中は常に 1 つ"
        );
    }

    #[test]
    fn signing_keys_match_created_pubkey() {
        let m = manager();
        let info = m.create("配信用").unwrap();
        let keys = m.signing_keys(&info.pubkey).unwrap();
        assert_eq!(keys.public_key().to_hex(), info.pubkey);
    }

    #[test]
    fn export_nsec_is_bech32() {
        let m = manager();
        let info = m.create("エクスポート").unwrap();
        let nsec = m.export_nsec(&info.pubkey).unwrap();
        assert!(nsec.starts_with("nsec1"), "nsec bech32 形式: {nsec}");
    }

    #[test]
    fn assignment_falls_back_to_selected() {
        let m = manager();
        let a = m.create("A").unwrap(); // 自動選択
        let b = m.create("B").unwrap();
        let ch = "0123456789abcdef0123456789abcdef";

        // 未割当 → 選択中(A)
        assert_eq!(m.persona_for_channel(ch).unwrap(), Some(a.pubkey.clone()));
        // 割当(B)が優先
        m.assign_channel(ch, &b.pubkey).unwrap();
        assert_eq!(m.persona_for_channel(ch).unwrap(), Some(b.pubkey.clone()));
        // 割当先を破棄 → 選択中(A)へフォールバック
        m.delete(&b.pubkey).unwrap();
        assert_eq!(m.persona_for_channel(ch).unwrap(), Some(a.pubkey.clone()));
    }

    #[test]
    fn delete_is_irreversible_and_clears_selection() {
        let m = manager();
        let a = m.create("破棄対象").unwrap();
        m.delete(&a.pubkey).unwrap();
        assert!(matches!(m.delete(&a.pubkey), Err(IdentityError::NotFound)));
        assert_eq!(m.selected().unwrap(), None, "破棄済みは選択中から外れる");
        assert!(matches!(
            m.signing_keys(&a.pubkey),
            Err(IdentityError::NotFound)
        ));
    }

    #[test]
    fn archive_and_reactivate() {
        let m = manager();
        let a = m.create("往復").unwrap();
        m.set_state(&a.pubkey, PersonaState::Archived).unwrap();
        assert_eq!(m.list().unwrap()[0].state, PersonaState::Archived);
        m.set_state(&a.pubkey, PersonaState::Active).unwrap();
        assert_eq!(m.list().unwrap()[0].state, PersonaState::Active);
    }

    #[test]
    fn select_unknown_is_not_found() {
        let m = manager();
        assert!(matches!(
            m.select(&"0".repeat(64)),
            Err(IdentityError::NotFound)
        ));
    }
}

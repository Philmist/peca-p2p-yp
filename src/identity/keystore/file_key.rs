//! Linux(unix)向けマスター鍵ファイルと XChaCha20-Poly1305 保護
//! (scheme 0x02 — ADR-0008 §2/§4、key-envelope.md §1「scheme 0x02」・§5)。
//!
//! - payload 形式: `nonce(24) || ct_and_tag(48)`。nonce は暗号化ごとに OS CSPRNG で生成。
//! - AAD = `magic || scheme`(呼び出し側が渡す 5 bytes — エンベロープヘッダの改竄で
//!   復号失敗となること)。
//! - 鍵素材・中間バッファは使用後に `zeroize`(best-effort SHOULD — §4)。鍵保持型の
//!   `Debug` は内容を表示しない(redacted — FR-011)。
//! - 秘密鍵・鍵素材はログ・エラー文言へ出さない(FR-011 MUST NOT)。

use std::fs::{self, OpenOptions, Permissions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use zeroize::{Zeroize, Zeroizing};

use super::KeystoreInit;
use crate::identity::IdentityError;

/// マスター鍵長(bytes)。
const MASTER_KEY_LEN: usize = 32;
/// XChaCha20-Poly1305 の nonce 長(bytes)。
const NONCE_LEN: usize = 24;
/// data-dir 内のマスター鍵ファイル名。
const MASTER_KEY_FILE: &str = "master.key";

/// 32 bytes マスター鍵。`Debug` は内容を表示しない(redacted — FR-011)。
pub(super) struct MasterKey {
    key: Zeroizing<[u8; MASTER_KEY_LEN]>,
}

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MasterKey(redacted)")
    }
}

impl MasterKey {
    /// ランダムなマスター鍵(ファイルなし — テスト・インメモリ用)。
    pub(super) fn random() -> Self {
        // OS CSPRNG。chacha20poly1305 が再エクスポートする rand_core を用いる
        // (メイン依存の rand 0.9 は rand_core 世代が非互換のため直接使えない)。
        use chacha20poly1305::aead::rand_core::RngCore;
        let mut key = Zeroizing::new([0u8; MASTER_KEY_LEN]);
        OsRng.fill_bytes(key.as_mut_slice());
        Self { key }
    }

    /// 既存バイト列からマスター鍵を構築する(長さは呼び出し側で検証済み)。
    fn from_bytes(bytes: &[u8; MASTER_KEY_LEN]) -> Self {
        Self {
            key: Zeroizing::new(*bytes),
        }
    }

    /// 鍵は常に 32 bytes のため構築は失敗しない。
    fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new(Key::from_slice(self.key.as_slice()))
    }

    /// 平文を暗号化し `nonce(24) || ct_and_tag` を返す。AAD をバインドする。
    pub(super) fn protect(&self, plain: &[u8], aad: &[u8]) -> Result<Vec<u8>, IdentityError> {
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let ct = self
            .cipher()
            .encrypt(&nonce, Payload { msg: plain, aad })
            .map_err(|_| IdentityError::Crypto)?;
        let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
        out.extend_from_slice(nonce.as_slice());
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// `nonce(24) || ct_and_tag` を復号する。失敗(tag 不一致・短い入力)は Unusable(§2)。
    pub(super) fn unprotect(&self, payload: &[u8], aad: &[u8]) -> Result<Vec<u8>, IdentityError> {
        if payload.len() < NONCE_LEN {
            return Err(IdentityError::Unusable);
        }
        let (nonce_bytes, ct) = payload.split_at(NONCE_LEN);
        self.cipher()
            .decrypt(XNonce::from_slice(nonce_bytes), Payload { msg: ct, aad })
            .map_err(|_| IdentityError::Unusable)
    }
}

/// `<data-dir>/master.key` を読込/生成する(§5)。
///
/// - 存在しなければ `O_CREAT|O_EXCL` + mode 0600 で原子的に生成(TOCTOU 回避)。
///   `has_encrypted_personas` が真なら「保護鍵消失疑い」を通知する。
/// - `EEXIST`(生成競合)は既存読込へフォールバックし、同一鍵へ収束する。
/// - 既存ファイルはサイズ 32 bytes を検証。不一致 = 破損 → 鍵を保持せず
///   [`KeystoreInit::Corrupt`](全ペルソナ Unusable。ファイルは上書きしない)。
///
/// 戻り値の `Option<MasterKey>` が `None` の場合は破損(復号不能)を表す。
pub(super) fn load_or_create(
    data_dir: &Path,
    has_encrypted_personas: bool,
) -> Result<(Option<MasterKey>, KeystoreInit), IdentityError> {
    let path = data_dir.join(MASTER_KEY_FILE);
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(mut file) => {
            if has_encrypted_personas {
                // 暗号化済みペルソナ存在下での暗黙の新鍵生成(§5 の MUST)。
                // 生成・書込みの「前」に記録し、生成と記録の間にクラッシュしても
                // 警告が失われないようにする。鍵素材・絶対パスは含めない(FR-011/FR-014)。
                tracing::warn!(
                    "保護鍵が見つからないため新規生成します。既存ペルソナは復号できません"
                );
            }
            let key = MasterKey::random();
            file.write_all(key.key.as_slice())
                .map_err(|_| IdentityError::Crypto)?;
            // umask 非依存に 0600 を保証する(fd 経由で TOCTOU なし — §5)。
            file.set_permissions(Permissions::from_mode(0o600))
                .map_err(|_| IdentityError::Crypto)?;
            file.sync_all().map_err(|_| IdentityError::Crypto)?;
            let init = if has_encrypted_personas {
                KeystoreInit::CreatedMissingSuspected
            } else {
                KeystoreInit::Created
            };
            Ok((Some(key), init))
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => load_existing(&path),
        Err(_) => Err(IdentityError::Crypto),
    }
}

/// 既存 master.key を読み込む(サイズ検証つき)。
fn load_existing(path: &Path) -> Result<(Option<MasterKey>, KeystoreInit), IdentityError> {
    let mut bytes = match fs::read(path) {
        Ok(b) => b,
        Err(_) => {
            // 読取不能(所有者変更・パーミッション等)= 部分劣化。致命的エラーに
            // しない(FR-013)。ファイルは触らず、鍵を保持しない(§5)。
            // 全ペルソナ Unusable。鍵素材・絶対パスは含めない(FR-011/FR-014)。
            tracing::warn!("保護鍵ファイルを読み取れません。全ペルソナが利用できません");
            return Ok((None, KeystoreInit::Unreadable));
        }
    };
    if bytes.len() != MASTER_KEY_LEN {
        // サイズ不一致 = 破損。ファイルは上書きせず、鍵を保持しない(§5)。
        // 全ペルソナ Unusable。鍵素材・絶対パスは含めない(FR-011/FR-014)。
        tracing::warn!("保護鍵ファイルが破損しています。全ペルソナが利用できません");
        bytes.zeroize();
        return Ok((None, KeystoreInit::Corrupt));
    }
    let mut buf = [0u8; MASTER_KEY_LEN];
    buf.copy_from_slice(&bytes);
    bytes.zeroize();
    let key = MasterKey::from_bytes(&buf);
    buf.zeroize();
    Ok((Some(key), KeystoreInit::Loaded))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AAD の一致で roundtrip できる。
    #[test]
    fn roundtrip_with_aad() {
        let mk = MasterKey::random();
        let aad = b"PYK1\x02";
        let plain = [7u8; MASTER_KEY_LEN];
        let ct = mk.protect(&plain, aad).unwrap();
        assert_eq!(mk.unprotect(&ct, aad).unwrap().as_slice(), &plain);
    }

    /// 同一鍵・同一暗号文でも AAD が異なれば復号は失敗する
    /// (実装が AAD を実際にバインドしていることの確認 — key-envelope.md §1)。
    #[test]
    fn aad_binding_rejects_mismatched_header() {
        let mk = MasterKey::random();
        let plain = [7u8; MASTER_KEY_LEN];
        let ct = mk.protect(&plain, b"PYK1\x02").unwrap();
        assert!(matches!(
            mk.unprotect(&ct, b"PYK1\x03"),
            Err(IdentityError::Unusable)
        ));
        assert!(matches!(
            mk.unprotect(&ct, b""),
            Err(IdentityError::Unusable)
        ));
    }

    /// 短い payload(nonce 未満)はパニックせず Unusable。
    #[test]
    fn short_payload_is_unusable() {
        let mk = MasterKey::random();
        assert!(matches!(
            mk.unprotect(&[0u8; 4], b"PYK1\x02"),
            Err(IdentityError::Unusable)
        ));
    }

    /// `Debug` は鍵内容を表示しない。
    #[test]
    fn debug_is_redacted() {
        let mk = MasterKey::random();
        assert_eq!(format!("{mk:?}"), "MasterKey(redacted)");
    }
}

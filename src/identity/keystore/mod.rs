//! 鍵保護の共通入口(keystore 抽象 — ADR-0008 §1/§3)。
//!
//! 正: `specs/002-linux-support/contracts/key-envelope.md`。
//!
//! `personas.secret_enc` は自己記述型エンベロープ `magic "PYK1" || scheme(1byte) || payload`
//! で保管する(§1)。プラットフォーム差異はここ 1 モジュールへ集約し、
//! `#[cfg(windows)] dpapi`(scheme 0x01)/ `#[cfg(unix)] file_key`(scheme 0x02)へ委譲する。
//!
//! - 読込(§2): magic あり → scheme で分岐。現プラットフォームで復号可能な scheme のみ
//!   復号を試行し、他プラットフォーム由来・未知 scheme は **Unusable**(パニック・起動失敗に
//!   してはならない — FR-006)。magic なし → レガシー生 DPAPI BLOB とみなし、windows は
//!   後方互換で復号、unix は Unusable。
//! - 書込(§3): 常にエンベロープ形式・現プラットフォーム scheme で書く。
//! - いかなる入力(空・短い入力含む)でもパニックしない。

use std::path::Path;

use crate::identity::IdentityError;

#[cfg(windows)]
mod dpapi;
#[cfg(unix)]
mod file_key;

/// エンベロープの magic(`"PYK1"`)。
const MAGIC: &[u8; 4] = b"PYK1";
/// magic(4) + scheme(1) のヘッダ長。
const HEADER_LEN: usize = 5;

/// 現プラットフォームで書込み・復号可能な scheme。
#[cfg(windows)]
const CURRENT_SCHEME: u8 = 0x01; // dpapi-user
#[cfg(unix)]
const CURRENT_SCHEME: u8 = 0x02; // xchacha20-mk-v1

/// keystore 初期化の結果(呼び出し側が利用者向け警告を出し分けるための識別 —
/// key-envelope.md §5「障害原因の識別」)。完全な健全性判定(KeystoreHealth)は
/// 後続タスク(T020)で導入する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeystoreInit {
    /// 既存の保護鍵を読み込んだ(windows は状態を持たないため常にこれ)。
    Loaded,
    /// 保護鍵を新規生成した(暗号化済みペルソナは存在しなかった)。
    Created,
    /// 保護鍵が見つからないため新規生成したが、暗号化済みペルソナが存在した。
    /// 既存ペルソナは復号できない(保護鍵消失の可能性 — key-envelope.md §5)。
    CreatedMissingSuspected,
    /// 保護鍵ファイルが破損している(サイズ不一致 — 全ペルソナ Unusable)。
    /// ファイルは上書きしない(復元余地を残す)。
    Corrupt,
    /// 保護鍵ファイルが読み取れない(所有者変更・パーミッション等 — 全ペルソナ
    /// Unusable)。致命的エラーにせず部分劣化する(FR-013、quickstart 検証 3-5)。
    /// ファイルは上書きしない(復元余地を残す)。
    Unreadable,
}

/// 鍵保護のインスタンス(プラットフォーム状態を保持)。
///
/// windows は状態なし。unix は 32 bytes マスター鍵(破損時は保持しない)。
pub struct Keystore {
    #[cfg(unix)]
    master: Option<file_key::MasterKey>,
}

impl Keystore {
    /// data-dir を用いて keystore を初期化する。
    ///
    /// unix は `master.key` を読込/生成する(存在しなければ `O_CREAT|O_EXCL` + mode 0600 で
    /// 原子的に生成)。`has_encrypted_personas` が真かつ新規生成となった場合は
    /// [`KeystoreInit::CreatedMissingSuspected`] を返す(§5)。windows は自明に成功する。
    pub fn open(
        data_dir: &Path,
        has_encrypted_personas: bool,
    ) -> Result<(Self, KeystoreInit), IdentityError> {
        #[cfg(windows)]
        {
            let _ = (data_dir, has_encrypted_personas);
            Ok((Keystore {}, KeystoreInit::Loaded))
        }
        #[cfg(unix)]
        {
            let (master, init) = file_key::load_or_create(data_dir, has_encrypted_personas)?;
            Ok((Keystore { master }, init))
        }
    }

    /// ファイルを持たないインメモリ keystore(テスト・一時用)。
    ///
    /// unix はランダムなマスター鍵を保持し、`master.key` を作らない。windows は状態なし。
    pub fn ephemeral() -> Self {
        #[cfg(windows)]
        {
            Keystore {}
        }
        #[cfg(unix)]
        {
            Keystore {
                master: Some(file_key::MasterKey::random()),
            }
        }
    }

    /// 平文を保護し、常にエンベロープ形式(現プラットフォーム scheme)で返す(§3)。
    pub fn protect(&self, plain: &[u8]) -> Result<Vec<u8>, IdentityError> {
        #[cfg(windows)]
        {
            let payload = dpapi::protect(plain)?;
            Ok(encode(CURRENT_SCHEME, &payload))
        }
        #[cfg(unix)]
        {
            let master = self.master.as_ref().ok_or(IdentityError::Crypto)?;
            let payload = master.protect(plain, &aad(CURRENT_SCHEME))?;
            Ok(encode(CURRENT_SCHEME, &payload))
        }
    }

    /// 保管表現を復号する。復号不能・破損・他プラットフォーム・未知 scheme・短い入力は
    /// すべて [`IdentityError::Unusable`](パニックしない — §2 / FR-006)。
    pub fn unprotect(&self, blob: &[u8]) -> Result<Vec<u8>, IdentityError> {
        match decode(blob) {
            Decoded::Envelope { scheme, payload } => {
                if scheme != CURRENT_SCHEME {
                    // 他プラットフォーム由来・未知 scheme は復号不能。
                    return Err(IdentityError::Unusable);
                }
                #[cfg(windows)]
                {
                    dpapi::unprotect(payload)
                }
                #[cfg(unix)]
                {
                    let master = self.master.as_ref().ok_or(IdentityError::Unusable)?;
                    master.unprotect(payload, &aad(CURRENT_SCHEME))
                }
            }
            Decoded::Legacy => {
                // magic なし → レガシー生 DPAPI BLOB 扱い。
                #[cfg(windows)]
                {
                    dpapi::unprotect(blob)
                }
                #[cfg(unix)]
                {
                    Err(IdentityError::Unusable)
                }
            }
        }
    }
}

/// 保管表現が現プラットフォームの scheme のエンベロープか(呼び出し側が
/// 「保護鍵消失疑い」判定に使う — 既存ペルソナのうち自プラットフォームで暗号化された
/// ものの有無を数えるため)。
pub fn is_current_scheme(secret_enc: &[u8]) -> bool {
    matches!(
        decode(secret_enc),
        Decoded::Envelope { scheme, .. } if scheme == CURRENT_SCHEME
    )
}

/// エンベロープの解釈結果。
enum Decoded<'a> {
    /// magic あり。
    Envelope { scheme: u8, payload: &'a [u8] },
    /// magic なし(レガシー生 BLOB 相当)。
    Legacy,
}

/// 先頭 4 bytes が magic なら scheme/payload を切り出す。それ以外は Legacy。
fn decode(blob: &[u8]) -> Decoded<'_> {
    if blob.len() >= HEADER_LEN && blob.starts_with(MAGIC) {
        Decoded::Envelope {
            scheme: blob[4],
            payload: &blob[HEADER_LEN..],
        }
    } else {
        Decoded::Legacy
    }
}

/// `magic || scheme || payload` を組み立てる。
fn encode(scheme: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(MAGIC);
    out.push(scheme);
    out.extend_from_slice(payload);
    out
}

/// AEAD の AAD(= `magic || scheme` の 5 bytes — §1)。
#[cfg(unix)]
fn aad(scheme: u8) -> [u8; HEADER_LEN] {
    [MAGIC[0], MAGIC[1], MAGIC[2], MAGIC[3], scheme]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_rejects_short_and_non_magic() {
        assert!(matches!(decode(&[]), Decoded::Legacy));
        assert!(matches!(decode(b"PYK"), Decoded::Legacy));
        assert!(matches!(decode(b"PYK1"), Decoded::Legacy)); // scheme byte 欠落
        assert!(matches!(decode(&[0u8; 8]), Decoded::Legacy));
    }

    #[test]
    fn encode_decode_header_roundtrip() {
        let env = encode(CURRENT_SCHEME, &[1, 2, 3]);
        assert_eq!(&env[0..4], MAGIC);
        assert_eq!(env[4], CURRENT_SCHEME);
        match decode(&env) {
            Decoded::Envelope { scheme, payload } => {
                assert_eq!(scheme, CURRENT_SCHEME);
                assert_eq!(payload, &[1, 2, 3]);
            }
            Decoded::Legacy => panic!("magic 付きは Envelope"),
        }
        assert!(is_current_scheme(&env));
        assert!(!is_current_scheme(b"raw-legacy-blob"));
    }
}

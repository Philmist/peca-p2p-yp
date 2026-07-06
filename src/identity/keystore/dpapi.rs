//! Windows DPAPI ラッパ(ユーザースコープ・scheme 0x01 — ADR-0009 §1)。
//!
//! payload は生 DPAPI BLOB(`CryptProtectData` / `CRYPTPROTECT_UI_FORBIDDEN`)。
//! keystore がエンベロープ(magic || 0x01 || payload)を付与するため、本モジュールは
//! 挙動不変で BLOB の暗号化/復号のみを担う。復号失敗は
//! [`IdentityError::Unusable`](利用不可 — ADR-0003 §4)。

use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};
use windows::core::PCWSTR;

use crate::identity::IdentityError;

/// 出力 BLOB(`LocalAlloc` 済み)を Vec へ写して解放する。
///
/// # Safety
/// `blob` は DPAPI が成功時に返した出力 BLOB であること。
unsafe fn take_blob(blob: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    unsafe {
        let data = std::slice::from_raw_parts(blob.pbData, blob.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(blob.pbData.cast())));
        data
    }
}

/// 平文をユーザースコープで暗号化し、生 DPAPI BLOB を返す(UI プロンプト禁止)。
pub(super) fn protect(plain: &[u8]) -> Result<Vec<u8>, IdentityError> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: plain.len() as u32,
        pbData: plain.as_ptr().cast_mut(),
    };
    let mut out = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptProtectData(
            &input,
            PCWSTR::null(),
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
        .map_err(|_| IdentityError::Crypto)?;
        Ok(take_blob(out))
    }
}

/// 生 DPAPI BLOB を復号する。失敗は [`IdentityError::Unusable`](利用不可)。
pub(super) fn unprotect(blob: &[u8]) -> Result<Vec<u8>, IdentityError> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: blob.len() as u32,
        pbData: blob.as_ptr().cast_mut(),
    };
    let mut out = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptUnprotectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
        .map_err(|_| IdentityError::Unusable)?;
        Ok(take_blob(out))
    }
}

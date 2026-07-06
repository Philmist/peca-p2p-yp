//! 契約テスト: 鍵エンベロープと master.key(002-linux-support T016)
//!
//! 正: `specs/002-linux-support/contracts/key-envelope.md` §6(契約テスト #1〜#9)。
//! 設計決定は `docs/adr/0008-linux-key-protection.md`。
//!
//! - 秘密鍵・鍵素材はログ・テスト出力・エラー文言へ出さない(FR-011 MUST NOT)。
//!   本テストのダミー平文はコード内固定値のみで、`println!` 等で出力しない。
//! - パニック禁止(FR-006): 破損・他プラットフォーム・未知 scheme・短い入力でも
//!   `Result` で返り、プロセスを落とさないことを検証する。

use peca_p2p_yp::identity::IdentityError;
use peca_p2p_yp::identity::keystore::Keystore;

/// テスト用のダミー平文(ペルソナ秘密鍵に相当する 32 bytes)。固定値。
const PLAIN32: [u8; 32] = [
    0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00,
    0x0F, 0x1E, 0x2D, 0x3C, 0x4B, 0x5A, 0x69, 0x78, 0x87, 0x96, 0xA5, 0xB4, 0xC3, 0xD2, 0xE1, 0xF0,
];

/// `haystack` が `needle` を連続部分列として含むか。
fn contains_subsequence(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

// ---------------------------------------------------------------------------
// #1 protect 出力が `PYK1` + 現プラットフォーム scheme で始まり、平文を含まない
// ---------------------------------------------------------------------------
#[test]
fn protect_output_is_envelope_without_plaintext() {
    let ks = Keystore::ephemeral();
    let enc = ks.protect(&PLAIN32).expect("protect は成功する");

    assert!(enc.len() >= 5, "エンベロープは magic(4)+scheme(1) を持つ");
    assert_eq!(&enc[0..4], b"PYK1", "magic は PYK1");
    #[cfg(unix)]
    assert_eq!(enc[4], 0x02, "unix の scheme は 0x02(xchacha20-mk-v1)");
    #[cfg(windows)]
    assert_eq!(enc[4], 0x01, "windows の scheme は 0x01(dpapi-user)");

    assert!(
        !contains_subsequence(&enc, &PLAIN32),
        "平文 32 bytes が保管表現に部分列として現れてはならない(平文非保存)"
    );
}

// ---------------------------------------------------------------------------
// #2 roundtrip: protect → unprotect で平文一致
// ---------------------------------------------------------------------------
#[test]
fn roundtrip_recovers_plaintext() {
    let ks = Keystore::ephemeral();
    let enc = ks.protect(&PLAIN32).expect("protect");
    let dec = ks.unprotect(&enc).expect("unprotect");
    assert_eq!(dec.as_slice(), &PLAIN32, "roundtrip で平文が一致する");
}

// ---------------------------------------------------------------------------
// #3 payload/tag を 1 bit 破壊 → Unusable(パニックしない)
// ---------------------------------------------------------------------------
#[test]
fn corrupted_payload_is_unusable_not_panic() {
    let ks = Keystore::ephemeral();
    let enc = ks.protect(&PLAIN32).expect("protect");

    // 末尾(認証タグ内)の 1 bit を反転。
    let mut broken = enc.clone();
    let last = broken.len() - 1;
    broken[last] ^= 0x01;
    assert!(
        matches!(ks.unprotect(&broken), Err(IdentityError::Unusable)),
        "タグ破壊は Unusable(パニックしない)"
    );

    // payload 中間(暗号文)の 1 bit を反転。
    let mut broken2 = enc.clone();
    let mid = 5 + (enc.len() - 5) / 2;
    broken2[mid] ^= 0x01;
    assert!(
        matches!(ks.unprotect(&broken2), Err(IdentityError::Unusable)),
        "暗号文破壊は Unusable(パニックしない)"
    );
}

// ---------------------------------------------------------------------------
// #4 他プラットフォーム scheme・未知 scheme → Unusable(パニックしない)
// ---------------------------------------------------------------------------
#[test]
fn foreign_and_unknown_scheme_is_unusable() {
    let ks = Keystore::ephemeral();

    // 他プラットフォーム由来 scheme(unix では 0x01、windows では 0x02)。
    #[cfg(unix)]
    let foreign = 0x01u8;
    #[cfg(windows)]
    let foreign = 0x02u8;
    let mut blob = Vec::new();
    blob.extend_from_slice(b"PYK1");
    blob.push(foreign);
    blob.extend_from_slice(&[0xAB; 72]);
    assert!(
        matches!(ks.unprotect(&blob), Err(IdentityError::Unusable)),
        "他プラットフォーム scheme は Unusable"
    );

    // 未知 scheme(予約 0xFF)。
    let mut unknown = Vec::new();
    unknown.extend_from_slice(b"PYK1");
    unknown.push(0xFF);
    unknown.extend_from_slice(&[0xCD; 72]);
    assert!(
        matches!(ks.unprotect(&unknown), Err(IdentityError::Unusable)),
        "未知 scheme は Unusable"
    );

    // 短い入力(magic 未満・scheme 欠落)でもパニックしない。
    assert!(matches!(ks.unprotect(&[]), Err(IdentityError::Unusable)));
    assert!(matches!(ks.unprotect(b"PYK"), Err(IdentityError::Unusable)));
    assert!(matches!(
        ks.unprotect(b"PYK1"),
        Err(IdentityError::Unusable)
    ));
}

// ---------------------------------------------------------------------------
// #5 (windows) magic なしレガシー DPAPI BLOB が復号できる
// ---------------------------------------------------------------------------
// エンベロープ scheme 0x01 の payload は生 DPAPI BLOB そのものであるため、
// ヘッダ(magic 4 + scheme 1)を剥がせばレガシー形式の BLOB が得られる。
#[cfg(windows)]
#[test]
fn legacy_dpapi_blob_without_magic_decrypts() {
    let ks = Keystore::ephemeral();
    let enc = ks.protect(&PLAIN32).expect("protect");
    let legacy = &enc[5..]; // magic なしの生 DPAPI BLOB 相当
    let dec = ks
        .unprotect(legacy)
        .expect("レガシー BLOB は後方互換で復号できる");
    assert_eq!(dec.as_slice(), &PLAIN32);
}

// ---------------------------------------------------------------------------
// #6 (unix) magic なし BLOB → Unusable
// ---------------------------------------------------------------------------
#[cfg(unix)]
#[test]
fn legacy_blob_without_magic_is_unusable_on_unix() {
    let ks = Keystore::ephemeral();
    // 先頭が PYK1 でない任意のバイト列(レガシー DPAPI BLOB 持込みを想定)。
    let legacy = [0x01u8; 64];
    assert!(
        matches!(ks.unprotect(&legacy), Err(IdentityError::Unusable)),
        "unix では magic なし BLOB は Unusable(復号不能)"
    );
}

// ---------------------------------------------------------------------------
// #7 (unix) master.key 欠如時、keystore 初期化がファイルを 0600 で生成する。
//     既存の暗号化済みペルソナがある場合は「保護鍵消失疑い」シグナルが返る。
// ---------------------------------------------------------------------------
#[cfg(unix)]
#[test]
fn open_creates_master_key_with_mode_0600_and_missing_signal() {
    use peca_p2p_yp::identity::keystore::KeystoreInit;
    use std::os::unix::fs::PermissionsExt;

    // 暗号化済みペルソナが存在しない状況での新規生成 → Created。
    let dir = tempfile::tempdir().unwrap();
    let (_ks, init) = Keystore::open(dir.path(), false).expect("open は成功する");
    assert_eq!(init, KeystoreInit::Created);

    let key_path = dir.path().join("master.key");
    assert!(key_path.exists(), "master.key が生成される");
    let mode = std::fs::metadata(&key_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "master.key は mode 0600 で生成される");

    // 既存の暗号化済みペルソナがある状況での新規生成 → 保護鍵消失疑い。
    let dir2 = tempfile::tempdir().unwrap();
    let (_ks2, init2) = Keystore::open(dir2.path(), true).expect("open は成功する");
    assert_eq!(
        init2,
        KeystoreInit::CreatedMissingSuspected,
        "暗号化済みペルソナ存在下での新規生成は保護鍵消失疑いを通知する"
    );

    // 既存 master.key の再オープンは Loaded(暗号文が復号できる)。
    let (ks_reload, init3) = Keystore::open(dir.path(), false).expect("再オープン");
    assert_eq!(init3, KeystoreInit::Loaded);
    let enc = ks_reload.protect(&PLAIN32).expect("protect");
    let (ks_reload2, _) = Keystore::open(dir.path(), false).expect("再オープン");
    assert_eq!(
        ks_reload2
            .unprotect(&enc)
            .expect("同一 master.key で復号")
            .as_slice(),
        &PLAIN32,
        "同一 master.key を読み込めば復号できる"
    );
}

// ---------------------------------------------------------------------------
// #8 (unix) 別の master.key では復号できない
// ---------------------------------------------------------------------------
#[cfg(unix)]
#[test]
fn different_master_key_cannot_decrypt() {
    let a = Keystore::ephemeral();
    let b = Keystore::ephemeral();
    let enc = a.protect(&PLAIN32).expect("protect");
    assert!(
        matches!(b.unprotect(&enc), Err(IdentityError::Unusable)),
        "別 master.key(別アカウント相当)では復号できない"
    );
}

// ---------------------------------------------------------------------------
// #9 (unix) エンベロープヘッダ(AAD = magic||scheme)改竄で復号失敗
// ---------------------------------------------------------------------------
// scheme 0x02 は AAD にヘッダ(magic||scheme)をバインドする(key-envelope.md §1)。
// 観測可能な契約として「ヘッダ改竄 → 復号失敗(Unusable)」を検証する。
// AEAD が AAD を実際にバインドしていること自体は file_key の白箱ユニットテスト
// (`aad_binding_rejects_mismatched_header`)で確認する。
#[cfg(unix)]
#[test]
fn tampered_header_fails_decryption() {
    let ks = Keystore::ephemeral();
    let enc = ks.protect(&PLAIN32).expect("protect");
    // 対照: 無改竄なら復号できる。
    assert_eq!(ks.unprotect(&enc).expect("control").as_slice(), &PLAIN32);

    // scheme byte(AAD の一部)を改竄。
    let mut tampered_scheme = enc.clone();
    tampered_scheme[4] ^= 0x01;
    assert!(
        matches!(ks.unprotect(&tampered_scheme), Err(IdentityError::Unusable)),
        "scheme byte 改竄は復号失敗(Unusable)"
    );

    // magic(AAD の一部)を改竄。
    let mut tampered_magic = enc.clone();
    tampered_magic[0] ^= 0x01;
    assert!(
        matches!(ks.unprotect(&tampered_magic), Err(IdentityError::Unusable)),
        "magic 改竄は復号失敗(Unusable)"
    );
}

// ---------------------------------------------------------------------------
// #10 (unix) master.key が読み取れない(所有者変更・chmod 000 等)場合は
//      致命的エラーにせず部分劣化する(全ペルソナ利用不可・稼働継続 — FR-013、
//      quickstart 検証 3-5)。実機検証(2026-07-07)で発見した欠陥の再現テスト。
// ---------------------------------------------------------------------------
#[cfg(unix)]
#[test]
fn unreadable_master_key_degrades_instead_of_failing() {
    use peca_p2p_yp::identity::keystore::KeystoreInit;
    use std::os::unix::fs::PermissionsExt;

    // 正常な master.key で暗号化しておく。
    let dir = tempfile::tempdir().unwrap();
    let (ks, _) = Keystore::open(dir.path(), false).expect("初回生成");
    let enc = ks.protect(&PLAIN32).expect("protect");
    drop(ks);

    // 読取不能にする(chown root 相当の再現として mode 000)。
    let key_path = dir.path().join("master.key");
    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o000)).unwrap();

    // 読取不能でも open は成功し(致命的エラー禁止)、Unreadable が通知される。
    let (ks2, init2) = Keystore::open(dir.path(), true)
        .expect("master.key 読取不能は部分劣化であり致命的エラーにしない(FR-013)");
    assert_eq!(init2, KeystoreInit::Unreadable, "読取不能シグナルが返る");

    // 鍵なしでは復号は Unusable(パニック・鍵素材漏洩なし)。
    assert!(
        matches!(ks2.unprotect(&enc), Err(IdentityError::Unusable)),
        "読取不能時の復号は Unusable"
    );

    // 後片付け(tempdir の削除に必要)。
    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

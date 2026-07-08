//! US2 鍵保管セキュリティシナリオのステップ定義(T023 骨子 → T036 実装)
//!
//! spec.md セキュリティシナリオ(US2)4 件:
//! ① 平文非永続化(at-rest 保護 — FR-003) ② パーミッション自動是正(FR-013)
//! ③ 是正不能の部分劣化(全ペルソナ利用不可 + 発見継続 — FR-013)
//! ④ 復号不能データの隔離(当該ペルソナのみ利用不可 — FR-006)
//!
//! 各シナリオの事後アサーションとして、全ログ出力(tracing のキャプチャ + SecurityLog
//! ファイル)に秘密鍵・nsec(hex 64 桁・bech32・部分文字列)が現れないことを検査する
//! (FR-011)。
//!
//! パーミッション是正(②③)は unix 固有機能のため unix でのみ実体検査し、Windows では
//! DPAPI がアカウントスコープを担保する(contracts/cli-config.md §4)ため no-op として
//! シナリオを充足させる。at-rest 保護(①)と復号不能隔離(④)は両プラットフォームで検査する。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use cucumber::{given, then, when};
use nostr::Keys;

use peca_p2p_yp::event::schema::{ChannelListing, ChannelStatus, Track};
use peca_p2p_yp::identity::{IdentityManager, Keystore, PersonaInfo};
use peca_p2p_yp::security::SecurityLog;
use peca_p2p_yp::store::Store;

use crate::AppWorld;
use crate::mock_peer::{MockPeer, TestNode, unix_now};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// 発見継続の観測に使うチャンネル ID。
const CH_DISCOVER: &str = "0abcdef00000000000000000000000a2";

// ---------------------------------------------------------------------------
// tracing 出力のキャプチャ(全ログ出力の秘密鍵非漏洩検査 — FR-011)
//
// 実体は共有モジュール `crate::log_capture`(DEBUG レベルのグローバルサブスクライバを
// security の PEX 良性 debug 観測と共有する)。
// ---------------------------------------------------------------------------

use crate::log_capture::{captured_logs, init_capture};

/// バイト列を小文字 hex にする(秘密鍵の hex 表現を作る — 漏洩検査用)。
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

// ---------------------------------------------------------------------------
// World
// ---------------------------------------------------------------------------

/// US2 鍵保管シナリオ 1 個分の状態。
#[derive(Default)]
pub struct KeystoreWorld {
    dir: Option<tempfile::TempDir>,
    security: Option<Arc<SecurityLog>>,
    security_path: Option<PathBuf>,
    identity: Option<IdentityManager>,
    /// 是正不能シナリオ用に温存する keystore(健全性を後段で束ねて manager を作る)。
    keystore: Option<Keystore>,
    store: Option<Arc<Store>>,
    /// 漏洩検査用に捕捉した秘密鍵素材。
    secret_hex: Option<String>,
    secret_bytes: Option<Vec<u8>>,
    nsec: Option<String>,
    /// ① 検査対象。
    secret_enc: Option<Vec<u8>>,
    db_bytes: Option<Vec<u8>>,
    /// ②③ パーミッション検査結果。
    check: Option<peca_p2p_yp::platform::PermissionCheck>,
    /// ③④ 一覧結果。
    personas: Vec<PersonaInfo>,
    good_pubkey: Option<String>,
    bad_pubkey: Option<String>,
    /// ③ 発見継続の観測ノード。
    mock: Option<MockPeer>,
    node: Option<TestNode>,
}

impl std::fmt::Debug for KeystoreWorld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeystoreWorld")
            .field("has_identity", &self.identity.is_some())
            .field("check", &self.check)
            .field("personas", &self.personas.len())
            .finish()
    }
}

impl KeystoreWorld {
    fn dir_path(&self) -> &Path {
        self.dir.as_ref().expect("data-dir 未初期化").path()
    }
}

/// World から keystore 状態を取り出す(未初期化なら生成し、ログキャプチャを有効化する)。
fn ctx(world: &mut AppWorld) -> &mut KeystoreWorld {
    init_capture();
    world.keystore.get_or_insert_with(KeystoreWorld::default)
}

/// data-dir・Store・SecurityLog を用意する。
fn setup_dir(c: &mut KeystoreWorld) {
    let dir = tempfile::tempdir().unwrap();
    let sec_path = dir.path().join("security.log");
    let security = Arc::new(SecurityLog::new(&sec_path).unwrap());
    c.store = Some(Arc::new(Store::open_in_dir(dir.path()).unwrap()));
    c.security = Some(security);
    c.security_path = Some(sec_path);
    c.dir = Some(dir);
}

/// マネージャで作成したペルソナの秘密鍵素材を捕捉する(漏洩検査の基準)。
fn capture_secret(c: &mut KeystoreWorld, manager: &IdentityManager, pubkey: &str) {
    let keys = manager.signing_keys(pubkey).expect("署名鍵");
    let bytes = keys.secret_key().as_secret_bytes().to_vec();
    c.secret_hex = Some(hex(&bytes));
    c.secret_bytes = Some(bytes);
    c.nsec = Some(manager.export_nsec(pubkey).expect("nsec"));
}

/// 署名済みチャンネル掲載イベント(発見継続の観測用)。
fn signed_listing(keys: &Keys, channel_id: &str) -> nostr::Event {
    ChannelListing {
        channel_id: channel_id.into(),
        title: "発見継続".into(),
        summary: Some("説明".into()),
        genre: Some("game".into()),
        status: ChannelStatus::Live,
        starts: unix_now(),
        current_participants: 1,
        streaming: Some("pcp://198.51.100.1:7144/x".into()),
        bitrate_kbps: Some(1500),
        content_type: Some("FLV".into()),
        tip: Some("198.51.100.1:7144".into()),
        contact: None,
        relays: 0,
        track: Some(Track::default()),
    }
    .sign(keys, unix_now(), 0)
    .unwrap()
}

// ===========================================================================
// ① 平文非永続化(at-rest 保護)
// ===========================================================================

#[given("Linux ノードでペルソナを作成する")]
async fn create_persona_at_rest(world: &mut AppWorld) {
    let c = ctx(world);
    setup_dir(c);
    // 本番と同じ経路で keystore を初期化する(unix は master.key を 0600 生成)。
    let (keystore, _init) = Keystore::open(c.dir_path(), false).expect("keystore 初期化");
    let store = Arc::clone(c.store.as_ref().unwrap());
    let manager = IdentityManager::new(store, keystore);
    let info = manager.create("at-rest").expect("ペルソナ作成");
    capture_secret(c, &manager, &info.pubkey);
    c.good_pubkey = Some(info.pubkey);
    c.identity = Some(manager);
}

#[when("保管された秘密鍵表現を検査する")]
async fn inspect_stored_secret(world: &mut AppWorld) {
    let c = ctx(world);
    let store = Arc::clone(c.store.as_ref().unwrap());
    let personas = store.list_personas().expect("ペルソナ列挙");
    let p = personas.first().expect("ペルソナが 1 件ある");
    c.secret_enc = Some(p.secret_enc.clone());
    // DB ファイル本体も at-rest 検査対象にする(平文が SQLite に残らないこと)。
    c.db_bytes = Some(fs::read(c.dir_path().join("app.db")).unwrap_or_default());
}

#[then("保管データに平文の秘密鍵が含まれてはならない")]
async fn stored_secret_has_no_plaintext(world: &mut AppWorld) {
    let c = ctx(world);
    let enc = c.secret_enc.as_ref().expect("secret_enc");
    let plain = c.secret_bytes.as_ref().expect("平文秘密鍵");
    // 自己記述エンベロープ(PYK1 + 現プラットフォーム scheme)で保管される。
    assert!(enc.starts_with(b"PYK1"), "エンベロープ magic を持つべき");
    #[cfg(unix)]
    assert_eq!(enc.get(4), Some(&0x02u8), "unix は scheme 0x02");
    #[cfg(windows)]
    assert_eq!(enc.get(4), Some(&0x01u8), "windows は scheme 0x01");
    // 平文 32 bytes が保管表現にも DB ファイルにも現れない(FR-003)。
    assert!(
        !contains_subslice(enc, plain),
        "保管表現に平文秘密鍵が含まれてはならない"
    );
    let db = c.db_bytes.as_ref().expect("db bytes");
    assert!(
        !contains_subslice(db, plain),
        "DB ファイルに平文秘密鍵が含まれてはならない"
    );
}

/// `haystack` に `needle` が連続部分列として含まれるか。
fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

// ===========================================================================
// ② パーミッション自動是正(FR-013)
// ===========================================================================

#[given("保管ファイルのパーミッションが緩い data-dir が存在する")]
async fn loose_permission_dir(world: &mut AppWorld) {
    let c = ctx(world);
    setup_dir(c);
    // 本番同様に master.key と暗号化ペルソナ(→ app.db)を作る。
    let (keystore, _init) = Keystore::open(c.dir_path(), false).expect("keystore 初期化");
    let store = Arc::clone(c.store.as_ref().unwrap());
    let manager = IdentityManager::new(store, keystore);
    let info = manager.create("loose").expect("ペルソナ作成");
    capture_secret(c, &manager, &info.pubkey);
    c.identity = Some(manager);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // 他ユーザー可読へ緩める(0644 / 0755)。
        for name in ["master.key", "app.db"] {
            let p = c.dir_path().join(name);
            if p.exists() {
                fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
            }
        }
        fs::set_permissions(c.dir_path(), fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[when("起動時パーミッション検査を実行する")]
async fn run_permission_check(world: &mut AppWorld) {
    let c = ctx(world);
    #[cfg(unix)]
    {
        let security = Arc::clone(c.security.as_ref().unwrap());
        let check = peca_p2p_yp::platform::enforce_permissions(c.dir_path(), &security);
        c.check = Some(check);
    }
    #[cfg(not(unix))]
    {
        // Windows: パーミッション検査は no-op(DPAPI がアカウントスコープを担保)。
        c.check = Some(peca_p2p_yp::platform::PermissionCheck::default());
    }
}

#[then("保管ファイルのパーミッションが 0600 相当へ是正される")]
async fn permissions_are_fixed(world: &mut AppWorld) {
    let c = ctx(world);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let check = c.check.as_ref().expect("検査結果");
        assert!(check.is_healthy(), "是正できたので健全: {check:?}");
        let mode = |p: PathBuf| fs::symlink_metadata(p).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode(c.dir_path().join("master.key")), 0o600);
        assert_eq!(mode(c.dir_path().join("app.db")), 0o600);
        assert_eq!(mode(c.dir_path().to_path_buf()), 0o700);
    }
    #[cfg(not(unix))]
    {
        let _ = c;
    }
}

#[then("key_permission_fixed がセキュリティイベントに記録される")]
async fn key_permission_fixed_recorded(world: &mut AppWorld) {
    let c = ctx(world);
    #[cfg(unix)]
    {
        c.security.as_ref().unwrap().flush();
        let text = fs::read_to_string(c.security_path.as_ref().unwrap()).unwrap_or_default();
        assert!(
            text.contains("key_permission_fixed"),
            "key_permission_fixed が記録されるべき: {text}"
        );
    }
    #[cfg(not(unix))]
    {
        let _ = c;
    }
}

// ===========================================================================
// ③ 是正不能の部分劣化(全ペルソナ利用不可 + 発見継続)
// ===========================================================================

#[given("保管ファイルが是正不能な data-dir が存在する")]
async fn unfixable_permission_dir(world: &mut AppWorld) {
    let c = ctx(world);
    setup_dir(c);
    // 暗号化ペルソナを 1 件作る(全ペルソナ利用不可の対象)。
    let (keystore, _init) = Keystore::open(c.dir_path(), false).expect("keystore 初期化");
    let store = Arc::clone(c.store.as_ref().unwrap());
    let manager = IdentityManager::new(store, keystore);
    let info = manager.create("unfixable").expect("ペルソナ作成");
    capture_secret(c, &manager, &info.pubkey);
    drop(manager);

    // 発見・伝搬が keystore 非依存で継続することの観測ノードを立てる。
    // established 直後の SYNC で確実に届くよう serve(SYNC 応答)で保持させる。
    let mock = MockPeer::spawn().await;
    let node = TestNode::spawn(0x5EC0_00A2).await;
    node.add_manual_peer(mock.addr());
    let keys = Keys::generate();
    mock.serve_signed(&signed_listing(&keys, CH_DISCOVER));
    c.mock = Some(mock);
    c.node = Some(node);

    #[cfg(unix)]
    {
        // master.key を data-dir 内の実体への symlink に差し替える(追従せず是正不能扱い)。
        let real = c.dir_path().join("master.key.real");
        let link = c.dir_path().join("master.key");
        fs::rename(&link, &real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
    }
    // 是正不能後も master.key を読み込める keystore を温存する(検査結果で健全性を束ねる)。
    let (keystore2, _init2) = Keystore::open(c.dir_path(), true).expect("keystore 再初期化");
    c.keystore = Some(keystore2);
}

#[then("すべてのペルソナが利用不可になる")]
async fn all_personas_unusable(world: &mut AppWorld) {
    let c = ctx(world);
    // パーミッション検査結果から健全性を導く(是正不能 → Unavailable)。
    let healthy = c.check.as_ref().map(|k| k.is_healthy()).unwrap_or(true);
    #[cfg(unix)]
    assert!(!healthy, "是正不能なので健全でないはず");
    let health = peca_p2p_yp::identity::KeystoreHealth::evaluate(
        healthy,
        peca_p2p_yp::identity::KeystoreInit::Loaded,
    );
    let store = Arc::clone(c.store.as_ref().unwrap());
    let keystore = c.keystore.take().expect("温存 keystore");
    let manager = IdentityManager::new_with_health(store, keystore, health);
    let personas = manager.list().expect("一覧");
    assert!(!personas.is_empty(), "ペルソナは存在する");
    #[cfg(unix)]
    {
        assert!(
            personas.iter().all(|p| !p.usable),
            "是正不能時は全ペルソナが利用不可: {personas:?}"
        );
        // 鍵操作は既存「利用不可」エラーになる(作成・署名・エクスポート・破棄)。
        assert!(manager.create("new").is_err(), "作成は利用不可エラー");
        let pk = &personas[0].pubkey;
        assert!(manager.signing_keys(pk).is_err(), "署名は利用不可エラー");
        assert!(
            manager.export_nsec(pk).is_err(),
            "エクスポートは利用不可エラー"
        );
    }
    c.personas = personas;
    c.identity = Some(manager);
}

#[then("発見・伝搬機能は継続する")]
async fn discovery_continues(world: &mut AppWorld) {
    let c = ctx(world);
    let node = c.node.as_ref().expect("観測ノード");
    assert!(
        node.wait_for_channel(CH_DISCOVER, CONNECT_TIMEOUT).await,
        "keystore 利用不可でも発見・伝搬は継続するべき"
    );
}

#[then("key_permission_unfixable がセキュリティイベントに記録される")]
async fn key_permission_unfixable_recorded(world: &mut AppWorld) {
    let c = ctx(world);
    #[cfg(unix)]
    {
        c.security.as_ref().unwrap().flush();
        let text = fs::read_to_string(c.security_path.as_ref().unwrap()).unwrap_or_default();
        assert!(
            text.contains("key_permission_unfixable"),
            "key_permission_unfixable が記録されるべき: {text}"
        );
    }
    #[cfg(not(unix))]
    {
        let _ = c;
    }
}

// ===========================================================================
// ④ 復号不能データの隔離(当該ペルソナのみ利用不可)
// ===========================================================================

#[given("復号可能なペルソナと復号不能なペルソナが混在している")]
async fn mixed_decryptable_personas(world: &mut AppWorld) {
    let c = ctx(world);
    setup_dir(c);
    let (keystore, _init) = Keystore::open(c.dir_path(), false).expect("keystore 初期化");
    let store = Arc::clone(c.store.as_ref().unwrap());
    let manager = IdentityManager::new(Arc::clone(&store), keystore);
    // 復号可能なペルソナ。
    let good = manager.create("復号可").expect("正常ペルソナ");
    capture_secret(c, &manager, &good.pubkey);
    c.good_pubkey = Some(good.pubkey);
    // 復号不能なペルソナ(scheme 0x02 エンベロープだが payload が壊れている →
    // 現プラットフォームで復号失敗 = 当該ペルソナのみ Unusable)。
    let mut bad_enc = b"PYK1".to_vec();
    bad_enc.push(0x02);
    bad_enc.extend_from_slice(&[0u8; 72]); // nonce(24) + ct_and_tag(48) 相当だが不正
    let bad_pubkey = "ff".repeat(32);
    store
        .insert_persona(&bad_pubkey, &bad_enc, "復号不可")
        .expect("不正ペルソナ挿入");
    c.bad_pubkey = Some(bad_pubkey);
    c.identity = Some(manager);
}

#[when("ペルソナ一覧を取得する")]
async fn list_personas_step(world: &mut AppWorld) {
    let c = ctx(world);
    c.personas = c.identity.as_ref().unwrap().list().expect("一覧");
}

#[then("復号不能なペルソナのみが利用不可になる")]
async fn only_undecryptable_unusable(world: &mut AppWorld) {
    let c = ctx(world);
    let bad = c.bad_pubkey.as_ref().unwrap();
    let row = c
        .personas
        .iter()
        .find(|p| &p.pubkey == bad)
        .expect("復号不能ペルソナが一覧にある");
    assert!(!row.usable, "復号不能ペルソナは利用不可");
}

#[then("復号可能なペルソナは引き続き利用できる")]
async fn decryptable_still_usable(world: &mut AppWorld) {
    let c = ctx(world);
    let good = c.good_pubkey.as_ref().unwrap();
    let row = c
        .personas
        .iter()
        .find(|p| &p.pubkey == good)
        .expect("復号可能ペルソナが一覧にある");
    assert!(row.usable, "復号可能ペルソナは利用可能なまま");
}

// ===========================================================================
// 全シナリオ共通の事後アサーション: 秘密鍵・nsec のログ非出力(FR-011)
// ===========================================================================

#[then("いかなるログ出力にも秘密鍵や nsec が含まれてはならない")]
async fn no_secret_in_any_log(world: &mut AppWorld) {
    let c = ctx(world);
    let mut haystacks = vec![captured_logs()];
    if let Some(p) = &c.security_path {
        haystacks.push(fs::read_to_string(p).unwrap_or_default());
    }
    for text in &haystacks {
        if let Some(h) = &c.secret_hex {
            assert!(!text.contains(h.as_str()), "ログに秘密鍵 hex が漏れている");
            // 部分文字列(先頭 32 桁)も検査する。
            assert!(
                !text.contains(&h[..32]),
                "ログに秘密鍵 hex の部分が漏れている"
            );
        }
        if let Some(n) = &c.nsec {
            assert!(
                !text.contains(n.as_str()),
                "ログに nsec(bech32)が漏れている"
            );
        }
    }
}

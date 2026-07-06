//! プラットフォーム差異集約モジュール(002-linux-support T003)
//!
//! data-dir 解決・ディレクトリ作成のプラットフォーム横断抽象を提供する。
//! 差異を本モジュールへ集約し、他モジュールから `cfg` 分岐を排除する(ADR-0009 §1)。
//!
//! 解決優先順(全 OS 共通 — contracts/cli-config.md §1):
//! 1. `--data-dir <path>`
//! 2. (unix) `$STATE_DIRECTORY` — systemd `StateDirectory=` 注入。複数パス列挙時は先頭
//!    3a. (Windows) `%APPDATA%\peca-p2p-yp`
//!    3b. (unix) `$XDG_STATE_HOME/peca-p2p-yp`
//! 4. (unix) `~/.local/state/peca-p2p-yp`(`$HOME` から解決)
//!
//! 解決不能(Windows: APPDATA 未設定 / unix: HOME も未設定)は `Err(定型メッセージ)` を
//! 返す。呼び出し側が `eprintln!` + 終了コード 2 にできる(FR-014)。

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::security::SecurityLog;

// ---------------------------------------------------------------------------
// 解決ロジック(テスト容易性 — 環境変数ルックアップを注入できる純関数)
// ---------------------------------------------------------------------------

/// unix 版 data-dir 解決(contracts/cli-config.md §1)。
///
/// `lookup` に実環境では `std::env::var_os` を、テストではモック関数を渡す。
/// Rust edition 2024 では `std::env::set_var` が unsafe なため、テストスレッド間で
/// 環境変数を書き換えるリスクをこの注入パターンで回避する。
#[cfg(unix)]
pub fn resolve_with(
    lookup: impl Fn(&str) -> Option<OsString>,
    cli: Option<&Path>,
) -> Result<PathBuf, String> {
    // 優先 1: --data-dir CLI 引数
    if let Some(dir) = cli {
        return Ok(dir.to_path_buf());
    }

    // 優先 2: $STATE_DIRECTORY (systemd StateDirectory= が注入するパス)
    // コロン区切りで複数列挙された場合は先頭を使う
    if let Some(val) = lookup("STATE_DIRECTORY") {
        let s = val.to_string_lossy();
        let first = s.split(':').next().unwrap_or("").trim();
        if !first.is_empty() {
            return Ok(PathBuf::from(first));
        }
    }

    // 優先 3b: $XDG_STATE_HOME/peca-p2p-yp (XDG Base Directory)
    if let Some(val) = lookup("XDG_STATE_HOME") {
        let p = PathBuf::from(&val);
        if !p.as_os_str().is_empty() {
            return Ok(p.join("peca-p2p-yp"));
        }
    }

    // 優先 4: $HOME/.local/state/peca-p2p-yp (XDG 規定の既定パス)
    if let Some(val) = lookup("HOME") {
        let p = PathBuf::from(&val);
        if !p.as_os_str().is_empty() {
            return Ok(p.join(".local/state/peca-p2p-yp"));
        }
    }

    Err("HOME が未設定です。--data-dir を指定してください".to_string())
}

/// Windows 版 data-dir 解決(contracts/cli-config.md §1)。
#[cfg(windows)]
pub fn resolve_with(
    lookup: impl Fn(&str) -> Option<OsString>,
    cli: Option<&Path>,
) -> Result<PathBuf, String> {
    // 優先 1: --data-dir CLI 引数
    if let Some(dir) = cli {
        return Ok(dir.to_path_buf());
    }

    // 優先 3a: %APPDATA%\peca-p2p-yp (既存挙動)
    if let Some(val) = lookup("APPDATA") {
        return Ok(PathBuf::from(val).join("peca-p2p-yp"));
    }

    Err("APPDATA が未設定です。--data-dir を指定してください".to_string())
}

// ---------------------------------------------------------------------------
// 公開 API(実環境ラッパー)
// ---------------------------------------------------------------------------

/// data-dir を解決する(contracts/cli-config.md §1)。
///
/// `cli` は `--data-dir` オプションの値(`None` で CLI 省略扱い)。
/// 解決不能な場合は定型エラーメッセージを `Err` で返す(FR-014)。
pub fn resolve_data_dir(cli: Option<&Path>) -> Result<PathBuf, String> {
    resolve_with(|key| std::env::var_os(key), cli)
}

/// data-dir を解決し、存在しなければ作成する。
///
/// unix では mode `0700` で再帰作成する(FR-013 予防 — contracts/cli-config.md §1)。
/// umask によるさらなる制限は許容する。
/// 解決不能または作成失敗の場合は定型メッセージを `Err` で返す。
pub fn ensure_data_dir(cli: Option<&Path>) -> Result<PathBuf, String> {
    let dir = resolve_data_dir(cli)?;
    create_data_dir(&dir)?;
    Ok(dir)
}

// ---------------------------------------------------------------------------
// ディレクトリ作成
// ---------------------------------------------------------------------------

/// unix: mode 0700 で再帰作成(`std::os::unix::fs::DirBuilderExt` 使用)。
#[cfg(unix)]
fn create_data_dir(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|_| "データディレクトリを作成できませんでした".to_string())
}

/// Windows: 通常の再帰作成。
#[cfg(windows)]
fn create_data_dir(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|_| "データディレクトリを作成できませんでした".to_string())
}

// ---------------------------------------------------------------------------
// 起動時パーミッション検査・是正(unix のみ — contracts/cli-config.md §4)
// ---------------------------------------------------------------------------

/// パーミッション検査・是正の結果(contracts/cli-config.md §4)。
///
/// 記録する対象名は data-dir 相対名のみ(絶対パス非漏洩 — Principle II)。
/// `unfixable` が空でなければ共有保管物が守れていないため、呼び出し側は
/// [`KeystoreHealth::Unavailable`](crate::identity::KeystoreHealth) へ写像する。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PermissionCheck {
    /// 緩いパーミッションを是正した対象の data-dir 相対名。
    pub fixed: Vec<String>,
    /// 是正できなかった対象の data-dir 相対名(symlink・EPERM・EROFS 等)。
    pub unfixable: Vec<String>,
}

impl PermissionCheck {
    /// 是正不能な対象がなければ健全(全ペルソナ利用可)。
    pub fn is_healthy(&self) -> bool {
        self.unfixable.is_empty()
    }
}

/// 起動時に data-dir と保管物のパーミッションを検査・是正する(unix — FR-013)。
///
/// 対象と是正値: data-dir → `0700`、`master.key`・`app.db`・`app.db-wal`・`app.db-shm`
/// (存在するもの)→ `0600`。group/other ビット(`0o077`)が 1 つでも立っていれば是正
/// 対象とし、owner ビットのみ厳しい場合(例 `0400`)は他ユーザー開放がないため是正しない。
/// symlink は追従せず是正不能として扱う(第三者ファイルの mode 改変を避ける)。
///
/// 是正成功 → `key_permission_fixed` を記録して継続。是正失敗(`EPERM`・`EROFS`・IO
/// エラー・symlink)→ `key_permission_unfixable` を記録・警告し、[`PermissionCheck`] の
/// `unfixable` に載せる(呼び出し側が全ペルソナ利用不可へ写像する)。**起動と発見・伝搬
/// 機能は継続する**(MUST)。記録・警告に鍵素材・絶対パスは含めない(FR-011/FR-014)。
#[cfg(unix)]
pub fn enforce_permissions(data_dir: &Path, security: &SecurityLog) -> PermissionCheck {
    let mut check = PermissionCheck::default();
    // (相対名, 絶対パス, 是正 mode)。data-dir 自身は "." として記録する。
    let targets: [(&str, PathBuf, u32); 5] = [
        (".", data_dir.to_path_buf(), 0o700),
        ("master.key", data_dir.join("master.key"), 0o600),
        ("app.db", data_dir.join("app.db"), 0o600),
        ("app.db-wal", data_dir.join("app.db-wal"), 0o600),
        ("app.db-shm", data_dir.join("app.db-shm"), 0o600),
    ];
    for (name, path, mode) in targets {
        match enforce_one(&path, mode) {
            EnforceOutcome::Absent | EnforceOutcome::AlreadyStrict => {}
            EnforceOutcome::Fixed => {
                check.fixed.push(name.to_string());
                security.log(
                    crate::security::SecurityCategory::KeyPermissionFixed,
                    name,
                    "保管物のパーミッションを是正しました",
                );
            }
            EnforceOutcome::Unfixable => {
                check.unfixable.push(name.to_string());
                security.log(
                    crate::security::SecurityCategory::KeyPermissionUnfixable,
                    name,
                    "保管物のパーミッションを是正できませんでした",
                );
            }
        }
    }
    if !check.unfixable.is_empty() {
        // 定型警告(鍵素材・絶対パスなし — FR-011/FR-014)。原因の区別は
        // key_permission_unfixable の記録と本警告で成立する(key-envelope.md §5)。
        tracing::warn!(
            "保管ファイルのアクセス権を是正できないため、全ペルソナを利用できません(発見・伝搬は継続します)"
        );
    }
    check
}

/// 単一対象の是正結果。
#[cfg(unix)]
enum EnforceOutcome {
    /// 対象が存在しない。
    Absent,
    /// 既に他ユーザー開放がない(是正不要)。
    AlreadyStrict,
    /// 緩いパーミッションを是正した。
    Fixed,
    /// 是正できなかった(symlink・chmod 失敗)。
    Unfixable,
}

/// 1 対象を検査・是正する(symlink は追従しない — `lstat` 相当の `symlink_metadata`)。
#[cfg(unix)]
fn enforce_one(path: &Path, target_mode: u32) -> EnforceOutcome {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        // 存在しない・stat 不能はスキップ(WAL/SHM は無いこともある)。
        return EnforceOutcome::Absent;
    };
    let mode = meta.permissions().mode() & 0o7777;
    // 他ユーザー(group/other)への開放がなければ是正不要(owner のみ厳しい 0400 等も含む)。
    if mode & 0o077 == 0 {
        return EnforceOutcome::AlreadyStrict;
    }
    // symlink は追従して mode を変えない(第三者ファイル改変の回避 — §4)。
    if meta.file_type().is_symlink() {
        return EnforceOutcome::Unfixable;
    }
    match std::fs::set_permissions(path, std::fs::Permissions::from_mode(target_mode)) {
        Ok(()) => EnforceOutcome::Fixed,
        Err(_) => EnforceOutcome::Unfixable,
    }
}

/// Windows: パーミッション検査は no-op(DPAPI がアカウントスコープを担保 — §4)。
#[cfg(windows)]
pub fn enforce_permissions(_data_dir: &Path, _security: &SecurityLog) -> PermissionCheck {
    PermissionCheck::default()
}

// ---------------------------------------------------------------------------
// sd_notify プロトコル(unix のみ — contracts/systemd-service.md §1, research R5)
// ---------------------------------------------------------------------------

/// sd_notify プロトコルで通知を送る(unix のみ)。
///
/// `$NOTIFY_SOCKET` が未設定の場合は no-op(FR-009 MUST)。送信失敗は debug ログのみで
/// 稼働へ影響させない(MUST — contracts/systemd-service.md §1)。
/// 先頭 `@` は abstract socket として `\0` に読み替える(Linux 拡張 — research R5)。
/// 依存クレートは追加せず std のみで実装する(research R5)。
#[cfg(unix)]
pub fn sd_notify(message: &str) {
    use std::os::unix::net::UnixDatagram;

    let socket_path = match std::env::var_os("NOTIFY_SOCKET") {
        Some(p) => p,
        None => return, // 未設定は no-op(FR-009 MUST)
    };

    let sock = match UnixDatagram::unbound() {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!("sd_notify: ソケット作成失敗: {e}");
            return;
        }
    };

    if let Err(e) = notify_send(&sock, &socket_path, message) {
        tracing::debug!("sd_notify: 送信失敗: {e}");
    }
}

/// Linux: ファイルパスまたは abstract socket(`@` 前置)へデータグラムを送信する。
///
/// `@` 以降をアブストラクト名として `std::os::linux::net::SocketAddrExt` で解決する
/// (abstract socket は Linux 拡張 — research R5)。
#[cfg(target_os = "linux")]
fn notify_send(
    sock: &std::os::unix::net::UnixDatagram,
    socket_path: &std::ffi::OsStr,
    message: &str,
) -> std::io::Result<usize> {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::ffi::OsStrExt;

    let path_bytes = socket_path.as_bytes();
    if let Some(abstract_name) = path_bytes.strip_prefix(b"@") {
        // abstract socket: `@` 以降をアブストラクト名として送信(`\0` 読替 — research R5)
        let addr = std::os::unix::net::SocketAddr::from_abstract_name(abstract_name)?;
        sock.send_to_addr(message.as_bytes(), &addr)
    } else {
        sock.send_to(message.as_bytes(), std::path::Path::new(socket_path))
    }
}

/// 非 Linux unix(macOS 等): ファイルパスのみ対応(abstract socket は Linux 拡張)。
#[cfg(all(unix, not(target_os = "linux")))]
fn notify_send(
    sock: &std::os::unix::net::UnixDatagram,
    socket_path: &std::ffi::OsStr,
    message: &str,
) -> std::io::Result<usize> {
    sock.send_to(message.as_bytes(), std::path::Path::new(socket_path))
}

/// sd_notify: Windows は no-op(systemd は Windows で使用しない)。
#[cfg(not(unix))]
pub fn sd_notify(_message: &str) {}

// ---------------------------------------------------------------------------
// 停止シグナル抽象(contracts/cli-config.md §3, research R6)
// ---------------------------------------------------------------------------

/// 停止シグナルを非同期に待つ(プラットフォーム抽象 — research R6)。
///
/// unix: SIGTERM(systemd `systemctl stop`)または SIGINT(Ctrl+C)のいずれかを待つ
/// (`tokio::signal::unix` — contracts/cli-config.md §3)。
/// Windows: ctrl_c(現行挙動を維持)。
/// 呼び出し元は本関数が返ったら `STOPPING=1` 通知 → shutdown 伝播を行う
/// (contracts/systemd-service.md §1, contracts/cli-config.md §3)。
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut sigterm =
            signal(SignalKind::terminate()).expect("SIGTERM ハンドラの登録に失敗しました");
        let mut sigint =
            signal(SignalKind::interrupt()).expect("SIGINT ハンドラの登録に失敗しました");

        tokio::select! {
            _ = sigterm.recv() => {},
            _ = sigint.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// 環境変数モック。`std::env::set_var`(unsafe)を使わずに
    /// テストスレッド間干渉なしで解決順を検証する。
    fn mock(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, OsString> = vars
            .iter()
            .map(|&(k, v)| (k.to_owned(), OsString::from(v)))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    // ---- unix テスト ----------------------------------------------------------------

    /// --data-dir が全ソースに優先する(contracts/cli-config.md §7 #1)
    #[cfg(unix)]
    #[test]
    fn unix_cli_takes_highest_priority() {
        let cli = PathBuf::from("/custom/dir");
        let env = mock(&[
            ("STATE_DIRECTORY", "/systemd/dir"),
            ("XDG_STATE_HOME", "/xdg"),
            ("HOME", "/home/user"),
        ]);
        assert_eq!(resolve_with(env, Some(&cli)).unwrap(), cli);
    }

    /// STATE_DIRECTORY > XDG_STATE_HOME の優先順(contracts/cli-config.md §7 #2)
    #[cfg(unix)]
    #[test]
    fn unix_state_directory_beats_xdg() {
        let env = mock(&[
            ("STATE_DIRECTORY", "/systemd/dir"),
            ("XDG_STATE_HOME", "/xdg"),
            ("HOME", "/home/user"),
        ]);
        assert_eq!(
            resolve_with(env, None).unwrap(),
            PathBuf::from("/systemd/dir"),
        );
    }

    /// STATE_DIRECTORY がコロン区切りの場合は先頭を使う(contracts/cli-config.md §1)
    #[cfg(unix)]
    #[test]
    fn unix_state_directory_colon_uses_first() {
        let env = mock(&[
            ("STATE_DIRECTORY", "/first:/second:/third"),
            ("HOME", "/home/user"),
        ]);
        assert_eq!(resolve_with(env, None).unwrap(), PathBuf::from("/first"),);
    }

    /// XDG_STATE_HOME > $HOME フォールバックの優先順(contracts/cli-config.md §7 #2)
    #[cfg(unix)]
    #[test]
    fn unix_xdg_state_home_beats_home() {
        let env = mock(&[("XDG_STATE_HOME", "/xdg"), ("HOME", "/home/user")]);
        assert_eq!(
            resolve_with(env, None).unwrap(),
            PathBuf::from("/xdg/peca-p2p-yp"),
        );
    }

    /// HOME のみ設定時は ~/.local/state/peca-p2p-yp(contracts/cli-config.md §7 #2)
    #[cfg(unix)]
    #[test]
    fn unix_home_fallback() {
        let env = mock(&[("HOME", "/home/user")]);
        assert_eq!(
            resolve_with(env, None).unwrap(),
            PathBuf::from("/home/user/.local/state/peca-p2p-yp"),
        );
    }

    /// HOME 未設定かつ他のソースも未設定は Err(contracts/cli-config.md §1 解決不能)
    #[cfg(unix)]
    #[test]
    fn unix_unresolvable_returns_err() {
        let env = mock(&[]);
        assert!(resolve_with(env, None).is_err());
    }

    // ---- Windows テスト ----------------------------------------------------------------

    /// --data-dir が APPDATA に優先する(contracts/cli-config.md §7 #1)
    #[cfg(windows)]
    #[test]
    fn windows_cli_takes_highest_priority() {
        let cli = PathBuf::from("C:\\custom\\dir");
        let env = mock(&[("APPDATA", "C:\\Users\\user\\AppData\\Roaming")]);
        assert_eq!(resolve_with(env, Some(&cli)).unwrap(), cli);
    }

    /// APPDATA からの解決(contracts/cli-config.md §1 優先 3a)
    #[cfg(windows)]
    #[test]
    fn windows_appdata_fallback() {
        let env = mock(&[("APPDATA", "C:\\Users\\user\\AppData\\Roaming")]);
        assert_eq!(
            resolve_with(env, None).unwrap(),
            PathBuf::from("C:\\Users\\user\\AppData\\Roaming\\peca-p2p-yp"),
        );
    }

    /// APPDATA 未設定は Err(contracts/cli-config.md §1 解決不能)
    #[cfg(windows)]
    #[test]
    fn windows_unresolvable_returns_err() {
        let env = mock(&[]);
        assert!(resolve_with(env, None).is_err());
    }
}

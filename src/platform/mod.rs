//! プラットフォーム差異集約モジュール(002-linux-support T003)
//!
//! data-dir 解決・ディレクトリ作成のプラットフォーム横断抽象を提供する。
//! 差異を本モジュールへ集約し、他モジュールから `cfg` 分岐を排除する(ADR-0008 §1)。
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

//! 契約テスト: CLI・data-dir 解決優先順(contracts/cli-config.md §7 #1・#2)
//! T010(002-linux-support Phase 3)
//!
//! `platform::resolve_with` を黒箱として使い、解決優先順の回帰固定を行う。
//! Rust edition 2024 の `set_var` unsafe を避けるため環境変数は一切書き換えず、
//! ルックアップ関数をモックで注入する(同モジュール unit テストと同じ方式)。

// unix 専用機能のため全体を cfg で囲む。
// Windows ビルドでは本モジュール全体がコンパイル対象外になり dead_code 警告が出ない。
#[cfg(unix)]
mod unix {
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::path::PathBuf;

    use peca_p2p_yp::platform::resolve_with;

    /// 環境変数モック。テストスレッド間干渉なしで解決順を検証する。
    fn mock(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, OsString> = vars
            .iter()
            .map(|&(k, v)| (k.to_owned(), OsString::from(v)))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    // ---- §7 #1: --data-dir が全ソースに優先する(全 OS 共通) ---------------

    /// --data-dir が STATE_DIRECTORY・XDG_STATE_HOME・HOME のすべてに優先する
    #[test]
    fn cli_data_dir_beats_all_env_sources() {
        let cli = PathBuf::from("/explicit/path");
        let env = mock(&[
            ("STATE_DIRECTORY", "/systemd/dir"),
            ("XDG_STATE_HOME", "/xdg"),
            ("HOME", "/home/user"),
        ]);
        assert_eq!(resolve_with(env, Some(&cli)).unwrap(), cli);
    }

    // ---- §7 #2: (unix) STATE_DIRECTORY > XDG_STATE_HOME > ~/.local/state --

    /// STATE_DIRECTORY が XDG_STATE_HOME・HOME に優先する
    #[test]
    fn state_directory_beats_xdg_and_home() {
        let env = mock(&[
            ("STATE_DIRECTORY", "/var/lib/peca-p2p-yp"),
            ("XDG_STATE_HOME", "/xdg"),
            ("HOME", "/home/user"),
        ]);
        assert_eq!(
            resolve_with(env, None).unwrap(),
            PathBuf::from("/var/lib/peca-p2p-yp"),
        );
    }

    /// STATE_DIRECTORY がコロン区切りの場合は先頭エントリを使う
    #[test]
    fn state_directory_colon_separated_uses_first() {
        let env = mock(&[
            ("STATE_DIRECTORY", "/first:/second:/third"),
            ("HOME", "/home/user"),
        ]);
        assert_eq!(resolve_with(env, None).unwrap(), PathBuf::from("/first"),);
    }

    /// XDG_STATE_HOME が HOME に優先する
    #[test]
    fn xdg_state_home_beats_home() {
        let env = mock(&[("XDG_STATE_HOME", "/custom/state"), ("HOME", "/home/user")]);
        assert_eq!(
            resolve_with(env, None).unwrap(),
            PathBuf::from("/custom/state/peca-p2p-yp"),
        );
    }

    /// XDG_STATE_HOME 未設定時は $HOME/.local/state/peca-p2p-yp にフォールバックする
    #[test]
    fn home_dot_local_state_fallback() {
        let env = mock(&[("HOME", "/home/alice")]);
        assert_eq!(
            resolve_with(env, None).unwrap(),
            PathBuf::from("/home/alice/.local/state/peca-p2p-yp"),
        );
    }

    /// HOME も未設定かつ他のソースもなければ Err を返す
    #[test]
    fn unresolvable_without_any_source_returns_err() {
        let env = mock(&[]);
        assert!(resolve_with(env, None).is_err());
    }
}

// ---- §7 #3・#4: (unix) 起動時パーミッション検査・是正 ---------------------
//
// `platform::enforce_permissions` を黒箱として使い、緩いパーミッションの是正と
// 是正不能時の健全性判定(全ペルソナ利用不可へ写像される)を固定する。実際の
// `EPERM`(他ユーザー所有)は非 root の CI で再現できないため、是正不能ケースは
// symlink(追従せず是正不能として扱う — contracts/cli-config.md §4)で代表する。
#[cfg(unix)]
mod permissions {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use peca_p2p_yp::platform::enforce_permissions;
    use peca_p2p_yp::security::SecurityLog;

    /// data-dir 相当の一時ディレクトリと、その中のセキュリティログを用意する。
    fn setup() -> (tempfile::TempDir, SecurityLog) {
        let dir = tempfile::tempdir().unwrap();
        let log = SecurityLog::new(dir.path().join("security.log")).unwrap();
        (dir, log)
    }

    fn mode_of(path: &std::path::Path) -> u32 {
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
    }

    /// #3: 緩い master.key / DB は 0600 へ是正され key_permission_fixed が記録される
    #[test]
    fn loose_master_key_and_db_are_fixed_and_recorded() {
        let (dir, log) = setup();
        let root = dir.path();
        // 他ユーザー可読の状態を作る(0644 / 0664)。
        for (name, mode) in [
            ("master.key", 0o644),
            ("app.db", 0o664),
            ("app.db-wal", 0o644),
            ("app.db-shm", 0o644),
        ] {
            let p = root.join(name);
            fs::write(&p, b"x").unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(mode)).unwrap();
        }
        // data-dir 自体も緩める(0755)。
        fs::set_permissions(root, fs::Permissions::from_mode(0o755)).unwrap();

        let check = enforce_permissions(root, &log);

        assert!(check.is_healthy(), "是正できたので健全: {check:?}");
        assert!(check.unfixable.is_empty());
        // ファイルは 0600・ディレクトリは 0700 へ是正される。
        assert_eq!(mode_of(&root.join("master.key")), 0o600);
        assert_eq!(mode_of(&root.join("app.db")), 0o600);
        assert_eq!(mode_of(&root.join("app.db-wal")), 0o600);
        assert_eq!(mode_of(&root.join("app.db-shm")), 0o600);
        assert_eq!(mode_of(root), 0o700);
        // 是正対象は data-dir 相対名のみで記録される(絶対パス非漏洩)。
        assert!(check.fixed.iter().any(|n| n == "master.key"));
        assert!(check.fixed.iter().all(|n| !n.contains('/')));
        // key_permission_fixed がセキュリティイベントに記録される。
        log.flush();
        let text = fs::read_to_string(root.join("security.log")).unwrap();
        assert!(
            text.contains("key_permission_fixed"),
            "key_permission_fixed が記録されるべき: {text}"
        );
        // 記録に絶対パス片が漏れていないこと(相対名のみ)。
        assert!(!text.contains(root.to_string_lossy().as_ref()));
    }

    /// #3: owner ビットのみ厳しい状態(0400)は他ユーザー開放がないため是正しない
    #[test]
    fn owner_only_strict_is_not_touched() {
        let (dir, log) = setup();
        let root = dir.path();
        let p = root.join("master.key");
        fs::write(&p, b"x").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o400)).unwrap();
        fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();

        let check = enforce_permissions(root, &log);

        assert!(check.is_healthy());
        assert!(check.fixed.is_empty(), "0400 は是正対象ではない: {check:?}");
        assert_eq!(mode_of(&p), 0o400, "owner のみ厳しい mode は変更しない");
    }

    /// #4: 是正不能(symlink)なら健全でなくなり key_permission_unfixable が記録される
    /// (→ KeystoreHealth::Unavailable = 全ペルソナ利用不可へ写像される)。
    #[test]
    fn unfixable_symlink_is_reported_and_recorded() {
        let (dir, log) = setup();
        let root = dir.path();
        fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();
        // master.key を data-dir 外の実体への symlink にする(追従して mode を変えない)。
        let outside = dir.path().join("real_key");
        fs::write(&outside, b"x").unwrap();
        let link = root.join("master.key");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        let check = enforce_permissions(root, &log);

        assert!(!check.is_healthy(), "symlink は是正不能: {check:?}");
        assert!(check.unfixable.iter().any(|n| n == "master.key"));
        log.flush();
        let text = fs::read_to_string(root.join("security.log")).unwrap();
        assert!(
            text.contains("key_permission_unfixable"),
            "key_permission_unfixable が記録されるべき: {text}"
        );
    }
}

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

//! 参加者セッションの常駐管理(配線層 — T064: US1-AC2〜AC4 / FR-004 / FR-010)
//!
//! 「スレを開く」明示操作を起点に、板ごとに [`crate::livechat::participant::run_session`] を
//! tokio タスクとして常駐させ、確定レスのライブ供給(閲覧)・書き込み(T066)・凍結/復帰・
//! クローズ揮発・バックオフ再接続を稼働バイナリで成立させる。
//!
//! ## 役割の境界
//!
//! - **保持**: 板ごとの [`SessionEntry`](共有ビュー・書き込みコマンド送信口・タスクハンドル)。
//! - **供給**: [`Self::view`] で現在の確定列・送信中・板設定・状態を読み出す(UI/互換 API 用)。
//! - **書き込み**: [`Self::write`] は板鍵を解決し PoW ビットを決めてセッションタスクへ委譲する。
//! - **非責務**: トランスポート I/O・状態機械はセッションタスク(participant)側。本マネージャは
//!   タスクの生成・破棄と共有状態の橋渡しに徹する。
//!
//! announce 受信**のみ**では接続しない(SC-005)—— 接続は本マネージャの [`Self::open`]
//! (= UI の「スレを開く」操作)でしか始まらない。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use nostr::Keys;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::livechat::board::BoardKeyManager;
use crate::livechat::participant::{
    ParticipantConfig, SessionLiveState, SessionView, WriteCommand, run_session,
};
use crate::security::SecurityLog;

/// ポイズン時も内部値を回収してロックを返す(パニックしない)。
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 書き込み・操作の失敗理由(定型 — 内部情報を漏らさない)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagerError {
    /// 当該板のセッションが開かれていない(先に [`ParticipantManager::open`] が必要)。
    NotOpen,
    /// 板鍵の解決・生成に失敗(keystore 利用不可など)。
    KeyUnavailable,
}

/// 1 板分の常駐セッション。
struct SessionEntry {
    /// セッションタスクが更新するライブ状態(確定列・送信中・設定・状態)。
    shared: Arc<Mutex<SessionView>>,
    /// セッションタスクへの書き込みコマンド送信口(T066)。
    cmd_tx: mpsc::UnboundedSender<WriteCommand>,
    /// セッションタスク。破棄時に abort する。
    handle: JoinHandle<()>,
    /// 起動時の接続先(announce の `tip`)。再オープン要求の tip と異なれば張り替える。
    host_addr: String,
}

impl Drop for SessionEntry {
    fn drop(&mut self) {
        // タスクを畳む(共有 Arc は残るが、参照は本エントリのみのため回収される)。
        self.handle.abort();
    }
}

/// 参加者セッションの常駐マネージャ。`Arc<ParticipantManager>` として web 層・互換 API と共有する。
pub struct ParticipantManager {
    /// board_id(スレ主ペルソナ pubkey hex)→ 常駐セッション。
    sessions: Mutex<HashMap<String, SessionEntry>>,
    /// 視聴者の板向け書き込み鍵(板単位・ローテーション対象 — FR-016/FR-017)。
    board_keys: Arc<BoardKeyManager>,
    /// セキュリティイベントログ(チャレンジ失敗・偽 ORDER の記録用 — participant へ渡す)。
    security: Option<Arc<SecurityLog>>,
    /// 連続失敗の再接続上限(既定は実質無限 — 凍結中の再接続を続ける FR-014)。
    max_attempts: u32,
    /// バックオフ倍率(本番 1.0。テストで短縮する)。
    sleep_scale: f64,
}

impl ParticipantManager {
    /// 本番設定でマネージャを作る(再接続は事実上無限・バックオフ等倍)。
    pub fn new(board_keys: Arc<BoardKeyManager>, security: Option<Arc<SecurityLog>>) -> Arc<Self> {
        Self::with_tuning(board_keys, security, u32::MAX, 1.0)
    }

    /// 再接続上限・バックオフ倍率を指定して作る(テスト用)。
    pub fn with_tuning(
        board_keys: Arc<BoardKeyManager>,
        security: Option<Arc<SecurityLog>>,
        max_attempts: u32,
        sleep_scale: f64,
    ) -> Arc<Self> {
        Arc::new(Self {
            sessions: Mutex::new(HashMap::new()),
            board_keys,
            security,
            max_attempts,
            sleep_scale,
        })
    }

    /// 「スレを開く」明示操作。当該板の常駐セッションを起動する(冪等)。
    ///
    /// 既に生きた(未終端)セッションが**同じ tip** で動いていれば何もしない。終端済みエントリと、
    /// tip が変わった(板の開設し直し・ホストのアドレス変化)生存中エントリは張り替える —
    /// 生存中のセッションは起動時の tip へ再接続を続けるため、張り替えないと古い tip に
    /// 接続し続けて表示が空のままになる。
    /// `config.security` は本マネージャの `security` で上書きする(呼び出し側は未設定でよい)。
    pub fn open(&self, mut config: ParticipantConfig) {
        let board_id = config.board_id.clone();
        let mut sessions = lock(&self.sessions);
        // 生存中(未終端)のセッションがあれば二重起動しない(SC-005 — 接続は 1 本)。
        if let Some(existing) = sessions
            .get(&board_id)
            .filter(|e| !lock(&e.shared).terminated)
        {
            if existing.host_addr == config.host_addr {
                tracing::debug!(
                    target: "livechat",
                    board_id = %board_id,
                    host = %existing.host_addr,
                    "生存中の参加者セッションを継続(同一 tip の再オープン要求)"
                );
                return;
            }
            // tip 変更: 既存エントリは下の insert で置き換わり、Drop でタスクが畳まれる。
            tracing::debug!(
                target: "livechat",
                board_id = %board_id,
                session_host = %existing.host_addr,
                announced_host = %config.host_addr,
                "announce の tip が変わったため参加者セッションを張り替え"
            );
        }
        tracing::debug!(
            target: "livechat",
            board_id = %board_id,
            host = %config.host_addr,
            generation = config.generation,
            "参加者セッションを起動"
        );
        let host_addr = config.host_addr.clone();
        config.security = self.security.clone();
        let shared = Arc::new(Mutex::new(SessionView::initial(
            config.generation,
            config.key,
        )));
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let task_shared = Arc::clone(&shared);
        let (max_attempts, sleep_scale) = (self.max_attempts, self.sleep_scale);
        let handle = tokio::spawn(async move {
            run_session(config, task_shared, cmd_rx, max_attempts, sleep_scale).await;
        });
        sessions.insert(
            board_id,
            SessionEntry {
                shared,
                cmd_tx,
                handle,
                host_addr,
            },
        );
    }

    /// 当該板の常駐セッションが開かれている(生存中)か。
    pub fn is_open(&self, board_id: &str) -> bool {
        lock(&self.sessions)
            .get(board_id)
            .is_some_and(|e| !lock(&e.shared).terminated)
    }

    /// 生存中(未終端)の常駐セッションを持つ板 id 一覧(参加中=視聴中の板 — 007 FR-023c）。
    ///
    /// 互換 API の `boards.json`(ブラウザ SPA の板一覧)がホスト板に加えて列挙する。終端済み
    /// (クローズ揮発・切断確定)エントリは除く。
    pub fn board_ids(&self) -> Vec<String> {
        lock(&self.sessions)
            .iter()
            .filter(|(_, e)| !lock(&e.shared).terminated)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// 現在のライブ状態(確定列・送信中・板設定・状態)を読み出す。未オープンは `None`。
    pub fn view(&self, board_id: &str) -> Option<SessionView> {
        lock(&self.sessions)
            .get(board_id)
            .map(|e| lock(&e.shared).clone())
    }

    /// スレから抜ける(視聴者側の明示操作)。常駐タスクを畳んでエントリを除去する。
    pub fn leave(&self, board_id: &str) -> bool {
        lock(&self.sessions).remove(board_id).is_some()
    }

    /// 当該板へ書き込む(T066 — FR-008)。板鍵を解決してセッションへ委譲する。
    ///
    /// 初回 PoW のビット数はセッションタスクが送信時に決める(joined 後の板設定とホストの確定列で
    /// 判定する — `participant::write_pow_bits`)。接続前に受けた書き込みは joined まで保留される。
    /// 送信中(pending)への反映はセッションタスクが行うため、呼び出し側は [`Self::view`] を
    /// 再取得して送信中投稿を観測する(FR-008 の「送信中」区別表示)。未オープン板は
    /// [`ManagerError::NotOpen`]、板鍵解決失敗は [`ManagerError::KeyUnavailable`]。
    pub fn write(
        &self,
        board_id: &str,
        name: Option<String>,
        mail: Option<String>,
        body: String,
    ) -> Result<(), ManagerError> {
        // 板鍵を解決(未生成なら生成 — FR-016)。
        let keys: Keys = self
            .board_keys
            .signing_keys(board_id)
            .map_err(|_| ManagerError::KeyUnavailable)?;

        let sessions = lock(&self.sessions);
        let entry = sessions.get(board_id).ok_or(ManagerError::NotOpen)?;
        if lock(&entry.shared).terminated {
            return Err(ManagerError::NotOpen);
        }
        let cmd = WriteCommand {
            board_keys: keys,
            name,
            mail,
            body,
        };
        entry.cmd_tx.send(cmd).map_err(|_| ManagerError::NotOpen)?;
        Ok(())
    }
}

/// 常駐セッションが接続確立前・凍結中かどうかを問わず表示に足る状態か(UI 補助)。
pub fn is_viewable(state: SessionLiveState) -> bool {
    // Connecting でも過去の凍結ビュー(空)を表示してよい。Closed は削除済み。
    !matches!(state, SessionLiveState::Closed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Keystore;
    use crate::store::Store;

    fn manager() -> Arc<ParticipantManager> {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let board_keys = Arc::new(BoardKeyManager::new(store, Keystore::ephemeral()));
        // テスト: 連続失敗 1 回で終端、バックオフ 0。
        ParticipantManager::with_tuning(board_keys, None, 1, 0.0)
    }

    #[test]
    fn view_of_unknown_board_is_none() {
        let m = manager();
        assert!(m.view(&"ab".repeat(32)).is_none());
        assert!(!m.is_open(&"ab".repeat(32)));
    }

    #[test]
    fn write_to_unopened_board_is_not_open() {
        let m = manager();
        let err = m
            .write(&"ab".repeat(32), None, None, "本文".into())
            .unwrap_err();
        assert_eq!(err, ManagerError::NotOpen);
    }

    #[test]
    fn leave_unknown_board_is_noop() {
        let m = manager();
        assert!(!m.leave(&"ab".repeat(32)));
    }

    #[tokio::test]
    async fn open_registers_session_and_reports_terminated_after_task_ends() {
        // max_attempts=0: run_session は接続を試みず即終端する(登録 → 終端の簿記を決定的に検証。
        // 実接続・凍結/復帰は統合テスト(mock host)が担う)。
        let store = Arc::new(Store::open_in_memory().unwrap());
        let board_keys = Arc::new(BoardKeyManager::new(store, Keystore::ephemeral()));
        let m = ParticipantManager::with_tuning(board_keys, None, 0, 0.0);
        let board_id = "ab".repeat(32);
        let config = ParticipantConfig {
            host_addr: "203.0.113.1:7147".into(),
            board_id: board_id.clone(),
            channel: format!("30311:{board_id}:{}", "cd".repeat(16)),
            generation: 1,
            key: 1_700_000_000,
            title: "実況スレ".into(),
            res_limit: 1000,
            security: None,
        };
        m.open(config);
        // 直後は登録されている(view が取れる)。
        assert!(m.view(&board_id).is_some());
        // 即終端する(max_attempts=0 で接続を試みない)。
        for _ in 0..200 {
            if m.view(&board_id).map(|v| v.terminated).unwrap_or(false) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let view = m.view(&board_id).unwrap();
        assert!(view.terminated, "max_attempts=0 で即終端する");
        assert!(
            !m.is_open(&board_id),
            "終端後は is_open=false(再オープン可能)"
        );
    }

    #[tokio::test]
    async fn open_is_idempotent_while_alive() {
        // sleep_scale を大きくして最初のバックオフ中に留まらせ、二重 open が no-op であることを検証。
        let store = Arc::new(Store::open_in_memory().unwrap());
        let board_keys = Arc::new(BoardKeyManager::new(store, Keystore::ephemeral()));
        // 接続不能でも Connecting → 長いバックオフに入り、しばらく生存する。
        let m = ParticipantManager::with_tuning(board_keys, None, u32::MAX, 1.0);
        let board_id = "ab".repeat(32);
        let config = |bid: &str| ParticipantConfig {
            host_addr: "203.0.113.1:7147".into(),
            board_id: bid.to_string(),
            channel: format!("30311:{bid}:{}", "cd".repeat(16)),
            generation: 1,
            key: 1_700_000_000,
            title: "実況スレ".into(),
            res_limit: 1000,
            security: None,
        };
        m.open(config(&board_id));
        assert!(m.is_open(&board_id));
        // 書き込みは開いていれば受理される(コマンドはキューされる)。
        assert!(m.write(&board_id, None, None, "x".into()).is_ok());
        // 二重 open は no-op(生存中は張り替えない)。
        m.open(config(&board_id));
        assert!(m.is_open(&board_id));
        // 後始末(タスク abort)。
        assert!(m.leave(&board_id));
    }

    #[tokio::test]
    async fn reopen_with_changed_tip_replaces_alive_session() {
        // 生存中のセッションは起動時の tip で再接続を続けるため、announce の tip が変わった
        // (板の開設し直し・ホストのアドレス変化)ら張り替えないと古い tip に接続し続ける。
        let store = Arc::new(Store::open_in_memory().unwrap());
        let board_keys = Arc::new(BoardKeyManager::new(store, Keystore::ephemeral()));
        let m = ParticipantManager::with_tuning(board_keys, None, u32::MAX, 1.0);
        let board_id = "ab".repeat(32);
        let config = |tip: &str| ParticipantConfig {
            host_addr: tip.to_string(),
            board_id: board_id.clone(),
            channel: format!("30311:{board_id}:{}", "cd".repeat(16)),
            generation: 1,
            key: 1_700_000_000,
            title: "実況スレ".into(),
            res_limit: 1000,
            security: None,
        };
        let shared_of = |m: &ParticipantManager| {
            let sessions = lock(&m.sessions);
            let e = sessions.get(&board_id).unwrap();
            (Arc::clone(&e.shared), e.host_addr.clone())
        };

        m.open(config("127.0.0.1:7148"));
        let (first, _) = shared_of(&m);
        // 同じ tip の再オープンは no-op(同一セッションを維持)。
        m.open(config("127.0.0.1:7148"));
        let (same, _) = shared_of(&m);
        assert!(Arc::ptr_eq(&first, &same), "同一 tip では張り替えない");
        // tip が変わったら新しい tip でセッションを張り替える。
        m.open(config("[2001:db8::1]:7148"));
        let (replaced, host) = shared_of(&m);
        assert!(!Arc::ptr_eq(&first, &replaced), "tip 変更で張り替える");
        assert_eq!(host, "[2001:db8::1]:7148");
        assert!(m.is_open(&board_id));
        assert!(m.leave(&board_id));
    }
}

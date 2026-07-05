//! SQLite ストア(T012)
//!
//! 永続エンティティ(personas / peers / mutes / settings)の CRUD を提供する
//! (data-model.md rev 2 準拠 — relays テーブルは存在しない。FR-014)。
//!
//! - 本番配置は `%APPDATA%\peca-p2p-yp\app.db`([`Store::open_default`])。
//!   テスト・多ノード起動用に任意パス([`Store::open_at`] / [`Store::open_in_dir`])と
//!   インメモリ([`Store::open_in_memory`])のコンストラクタを持つ。
//! - スキーマは `schema.sql` を `include_str!` で埋め込み、起動時に冪等適用する。
//! - `Connection` を内部 `Mutex` で保持するため [`Store`] は `Sync` で、
//!   `Arc<Store>` として非同期タスク・axum 状態に共有できる(T019/T020)。
//! - エラー([`StoreError`])は内部情報(SQL 断片・パス)を `Display` に漏らさない
//!   (Principle II)。SQLite の原因は `source()` 経由でのみ内部ログに供給する。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, Row};

/// スキーマ定義(起動時に冪等適用)。
const SCHEMA: &str = include_str!("schema.sql");

/// ピア登録数の上限(data-model §PeerEndpoint 検証ルール)。
/// 超過時は `manual` を除く最も古い(LRU)ピアを降格・削除する。
pub const PEER_LIMIT: usize = 1_024;

/// ピアアドレスの最大バイト長(data-model §PeerEndpoint)。
pub const ADDR_MAX_LEN: usize = 256;

/// ペルソナ表示名(label)の最大文字数(data-model §Persona 検証ルール)。
pub const LABEL_MAX_CHARS: usize = 64;

/// nostr 公開鍵の hex 長(64 文字・小文字)。
const PUBKEY_HEX_LEN: usize = 64;

// ---------------------------------------------------------------------------
// エラー
// ---------------------------------------------------------------------------

/// ストア操作のエラー。
///
/// `Display` は利用者・ネットワークへ返せる定型文のみを出力し、内部情報
/// (SQL・パス・スタックトレース)を含めない (MUST NOT — Principle II)。
/// SQLite の詳細は内部ログ用に `source()` からのみ取得できる。
#[derive(Debug)]
pub enum StoreError {
    /// バックエンド(SQLite)操作の失敗。詳細は `source()` からのみ取得する。
    Backend(rusqlite::Error),
    /// ロック獲得の失敗(内部状態異常)。
    Locked,
    /// 一意制約違反(pubkey / addr の重複登録)。
    Duplicate,
    /// 検証ルール違反(静的な定型メッセージのみ — 入力値は含めない)。
    Validation(&'static str),
    /// データ保存領域の解決・初期化失敗(APPDATA 未設定・ディレクトリ作成不可)。
    Environment,
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Backend(_) | StoreError::Locked => {
                f.write_str("ストレージ操作に失敗しました")
            }
            StoreError::Duplicate => f.write_str("既に登録されています"),
            StoreError::Validation(msg) => f.write_str(msg),
            StoreError::Environment => f.write_str("データ保存領域を初期化できませんでした"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StoreError::Backend(e) => Some(e),
            _ => None,
        }
    }
}

/// SQLite エラーを [`StoreError`] へ写像する。UNIQUE 違反は [`StoreError::Duplicate`]。
fn map_sqlite(e: rusqlite::Error) -> StoreError {
    if let rusqlite::Error::SqliteFailure(err, _) = &e
        && err.code == rusqlite::ErrorCode::ConstraintViolation
    {
        return StoreError::Duplicate;
    }
    StoreError::Backend(e)
}

/// `Result` 別名。
pub type Result<T> = std::result::Result<T, StoreError>;

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// エンティティ型
// ---------------------------------------------------------------------------

/// ペルソナの状態(`created → active ⇄ archived → deleted`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersonaState {
    Active,
    Archived,
}

impl PersonaState {
    fn as_str(self) -> &'static str {
        match self {
            PersonaState::Active => "active",
            PersonaState::Archived => "archived",
        }
    }

    /// DB 値からの復元(未知値は `active` とみなす)。
    fn from_db(s: &str) -> PersonaState {
        match s {
            "archived" => PersonaState::Archived,
            _ => PersonaState::Active,
        }
    }
}

/// ペルソナ(data-model §Persona)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Persona {
    pub id: i64,
    pub pubkey: String,
    pub secret_enc: Vec<u8>,
    pub label: String,
    pub created_at: i64,
    pub state: PersonaState,
}

/// ピアの獲得経路。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerSource {
    /// 手動登録(利用者が明示削除するまで LRU 対象外)。
    Manual,
    /// ピア交換(PEX)で獲得(未検証。LRU 対象)。
    Pex,
}

impl PeerSource {
    fn as_str(self) -> &'static str {
        match self {
            PeerSource::Manual => "manual",
            PeerSource::Pex => "pex",
        }
    }

    /// DB 値からの復元(未知値は `pex` = LRU 対象・未検証として安全側に倒す)。
    fn from_db(s: &str) -> PeerSource {
        match s {
            "manual" => PeerSource::Manual,
            _ => PeerSource::Pex,
        }
    }
}

/// ピア(data-model §PeerEndpoint)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerEndpoint {
    pub id: i64,
    pub addr: String,
    pub source: PeerSource,
    /// 自ノードが外向き接続に成功した実績があるか。未検証ピアは PEX で再共有禁止 (MUST NOT)。
    pub verified: bool,
    pub enabled: bool,
    pub added_at: i64,
    pub last_ok_at: Option<i64>,
    pub fail_count: i64,
}

/// ミュート単位(data-model §MuteEntry)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MuteKind {
    /// ペルソナ(pubkey)単位。
    Pubkey,
    /// チャンネル(GUID)単位。
    Channel,
}

impl MuteKind {
    fn as_str(self) -> &'static str {
        match self {
            MuteKind::Pubkey => "pubkey",
            MuteKind::Channel => "channel",
        }
    }

    fn from_db(s: &str) -> MuteKind {
        match s {
            "channel" => MuteKind::Channel,
            _ => MuteKind::Pubkey,
        }
    }
}

/// ミュートエントリ(data-model §MuteEntry)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MuteEntry {
    pub id: i64,
    pub kind: MuteKind,
    pub value: String,
    pub created_at: i64,
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// SQLite ストア(スレッド安全)。
pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    /// 本番配置(`%APPDATA%\peca-p2p-yp\app.db`)で開く。
    pub fn open_default() -> Result<Self> {
        let base = std::env::var_os("APPDATA").ok_or(StoreError::Environment)?;
        let mut dir = PathBuf::from(base);
        dir.push("peca-p2p-yp");
        Self::open_in_dir(dir)
    }

    /// 指定ディレクトリ配下の `app.db` を開く(`--data-dir` による多ノード起動用)。
    /// ディレクトリが無ければ作成する。
    pub fn open_in_dir(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir).map_err(|_| StoreError::Environment)?;
        Self::open_at(dir.join("app.db"))
    }

    /// 任意のファイルパスで開く(テスト・明示指定用)。
    pub fn open_at(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path).map_err(map_sqlite)?;
        Self::from_connection(conn)
    }

    /// インメモリで開く(テスト用)。
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(map_sqlite)?;
        Self::from_connection(conn)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        conn.execute_batch(SCHEMA).map_err(map_sqlite)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>> {
        self.conn.lock().map_err(|_| StoreError::Locked)
    }

    // ---------------------------------------------------------------- personas

    /// ペルソナを新規登録する。
    ///
    /// - `label` は 64 文字以内(検証)。
    /// - `pubkey` は小文字 hex 64(`nostr` の `to_hex()` 出力形式)。重複は拒否。
    /// - `state` は `active`、`created_at` は現在時刻。
    pub fn insert_persona(&self, pubkey: &str, secret_enc: &[u8], label: &str) -> Result<Persona> {
        if label.chars().count() > LABEL_MAX_CHARS {
            return Err(StoreError::Validation(
                "ラベルは 64 文字以内で指定してください",
            ));
        }
        if !is_lower_hex(pubkey, PUBKEY_HEX_LEN) {
            return Err(StoreError::Validation("公開鍵の形式が不正です"));
        }
        let created_at = unix_now();
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO personas (pubkey, secret_enc, label, created_at, state)
             VALUES (?1, ?2, ?3, ?4, 'active')",
            rusqlite::params![pubkey, secret_enc, label, created_at],
        )
        .map_err(map_sqlite)?;
        Ok(Persona {
            id: conn.last_insert_rowid(),
            pubkey: pubkey.to_string(),
            secret_enc: secret_enc.to_vec(),
            label: label.to_string(),
            created_at,
            state: PersonaState::Active,
        })
    }

    /// pubkey でペルソナを取得する(署名時の秘密鍵ロード用 — T028)。
    pub fn get_persona_by_pubkey(&self, pubkey: &str) -> Result<Option<Persona>> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT id, pubkey, secret_enc, label, created_at, state
             FROM personas WHERE pubkey = ?1",
            [pubkey],
            row_to_persona,
        )
        .optional()
    }

    /// 全ペルソナを作成順(id 昇順)で列挙する。
    pub fn list_personas(&self) -> Result<Vec<Persona>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, pubkey, secret_enc, label, created_at, state
                 FROM personas ORDER BY id ASC",
            )
            .map_err(map_sqlite)?;
        let rows = stmt.query_map([], row_to_persona).map_err(map_sqlite)?;
        collect_rows(rows)
    }

    /// ペルソナの表示名を更新する。存在すれば `true`。
    pub fn update_persona_label(&self, pubkey: &str, label: &str) -> Result<bool> {
        if label.chars().count() > LABEL_MAX_CHARS {
            return Err(StoreError::Validation(
                "ラベルは 64 文字以内で指定してください",
            ));
        }
        let conn = self.lock()?;
        let n = conn
            .execute(
                "UPDATE personas SET label = ?1 WHERE pubkey = ?2",
                rusqlite::params![label, pubkey],
            )
            .map_err(map_sqlite)?;
        Ok(n > 0)
    }

    /// ペルソナの状態(active ⇄ archived)を更新する。存在すれば `true`。
    pub fn update_persona_state(&self, pubkey: &str, state: PersonaState) -> Result<bool> {
        let conn = self.lock()?;
        let n = conn
            .execute(
                "UPDATE personas SET state = ?1 WHERE pubkey = ?2",
                rusqlite::params![state.as_str(), pubkey],
            )
            .map_err(map_sqlite)?;
        Ok(n > 0)
    }

    /// ペルソナを破棄する(行削除 — 復元不可。data-model §Persona)。削除できれば `true`。
    pub fn delete_persona(&self, pubkey: &str) -> Result<bool> {
        let conn = self.lock()?;
        let n = conn
            .execute("DELETE FROM personas WHERE pubkey = ?1", [pubkey])
            .map_err(map_sqlite)?;
        Ok(n > 0)
    }

    // ------------------------------------------------------------------- peers

    /// ピアを登録または取得する(addr は一意)。
    ///
    /// - `addr` は空でなく 256 バイト以内(検証)。IPv6 ブラケット表記・自アドレス拒否は
    ///   接続層(T018)の責務であり本メソッドでは扱わない。
    /// - 既存 addr の場合は再登録せず既存行を返す。ただし `manual` での再登録は既存 `pex`
    ///   を `manual` へ昇格させる(手動登録は LRU 免除のため)。
    /// - 新規登録後、総数が [`PEER_LIMIT`] を超えると `manual` 以外を LRU 順に降格削除する。
    pub fn upsert_peer(&self, addr: &str, source: PeerSource) -> Result<PeerEndpoint> {
        if addr.is_empty() {
            return Err(StoreError::Validation("アドレスが空です"));
        }
        if addr.len() > ADDR_MAX_LEN {
            return Err(StoreError::Validation("アドレスが長すぎます"));
        }
        let conn = self.lock()?;
        if let Some(existing) = query_peer(&conn, addr)? {
            if source == PeerSource::Manual && existing.source != PeerSource::Manual {
                conn.execute("UPDATE peers SET source = 'manual' WHERE addr = ?1", [addr])
                    .map_err(map_sqlite)?;
                return query_peer(&conn, addr)?
                    .ok_or(StoreError::Backend(rusqlite::Error::QueryReturnedNoRows));
            }
            return Ok(existing);
        }
        let added_at = unix_now();
        conn.execute(
            "INSERT INTO peers (addr, source, verified, enabled, added_at, last_ok_at, fail_count)
             VALUES (?1, ?2, 0, 1, ?3, NULL, 0)",
            rusqlite::params![addr, source.as_str(), added_at],
        )
        .map_err(map_sqlite)?;
        enforce_peer_limit(&conn)?;
        query_peer(&conn, addr)?.ok_or(StoreError::Backend(rusqlite::Error::QueryReturnedNoRows))
    }

    /// 全ピアを id 昇順で列挙する(UI の健全性表示用)。
    pub fn list_peers(&self) -> Result<Vec<PeerEndpoint>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(&format!("{PEER_SELECT} ORDER BY id ASC"))
            .map_err(map_sqlite)?;
        let rows = stmt.query_map([], row_to_peer).map_err(map_sqlite)?;
        collect_rows(rows)
    }

    /// addr でピアを取得する。
    pub fn get_peer(&self, addr: &str) -> Result<Option<PeerEndpoint>> {
        let conn = self.lock()?;
        query_peer(&conn, addr)
    }

    /// PEX 共有・エクスポート用に verified かつ enabled なピアを last_ok_at の新しい順に返す。
    /// `limit` が `None` の場合は全件(エクスポート用)。
    pub fn verified_peers_by_recency(&self, limit: Option<usize>) -> Result<Vec<PeerEndpoint>> {
        let conn = self.lock()?;
        let base = format!(
            "{PEER_SELECT} WHERE verified = 1 AND enabled = 1 \
             ORDER BY COALESCE(last_ok_at, 0) DESC, id DESC"
        );
        let sql = match limit {
            Some(n) => format!("{base} LIMIT {n}"),
            None => base,
        };
        let mut stmt = conn.prepare(&sql).map_err(map_sqlite)?;
        let rows = stmt.query_map([], row_to_peer).map_err(map_sqlite)?;
        collect_rows(rows)
    }

    /// ピアの有効/無効を設定する(無効化=切り離し)。存在すれば `true`。
    pub fn set_peer_enabled(&self, addr: &str, enabled: bool) -> Result<bool> {
        let conn = self.lock()?;
        let n = conn
            .execute(
                "UPDATE peers SET enabled = ?1 WHERE addr = ?2",
                rusqlite::params![enabled as i64, addr],
            )
            .map_err(map_sqlite)?;
        Ok(n > 0)
    }

    /// ピアを削除する。削除できれば `true`。
    pub fn delete_peer(&self, addr: &str) -> Result<bool> {
        let conn = self.lock()?;
        let n = conn
            .execute("DELETE FROM peers WHERE addr = ?1", [addr])
            .map_err(map_sqlite)?;
        Ok(n > 0)
    }

    /// 外向き接続の成功を記録する。
    ///
    /// **外向き接続の成功時のみ**呼ぶこと(verified=1 は PEX 再共有の前提であり、
    /// 着信のみのピアに立ててはならない — data-model §PeerEndpoint / research R14)。
    /// verified=1・last_ok_at 更新・fail_count=0 リセットを行う。存在すれば `true`。
    pub fn record_peer_success(&self, addr: &str, at: i64) -> Result<bool> {
        let conn = self.lock()?;
        let n = conn
            .execute(
                "UPDATE peers SET verified = 1, last_ok_at = ?1, fail_count = 0 WHERE addr = ?2",
                rusqlite::params![at, addr],
            )
            .map_err(map_sqlite)?;
        Ok(n > 0)
    }

    /// 接続失敗を記録し(fail_count を +1)、更新後の fail_count を返す。
    /// ピアが存在しない場合は `None`。
    pub fn record_peer_failure(&self, addr: &str) -> Result<Option<i64>> {
        let conn = self.lock()?;
        let n = conn
            .execute(
                "UPDATE peers SET fail_count = fail_count + 1 WHERE addr = ?1",
                [addr],
            )
            .map_err(map_sqlite)?;
        if n == 0 {
            return Ok(None);
        }
        let count = conn
            .query_row(
                "SELECT fail_count FROM peers WHERE addr = ?1",
                [addr],
                |r| r.get::<_, i64>(0),
            )
            .map_err(map_sqlite)?;
        Ok(Some(count))
    }

    /// ピア総数。
    pub fn count_peers(&self) -> Result<usize> {
        let conn = self.lock()?;
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM peers", [], |r| r.get(0))
            .map_err(map_sqlite)?;
        Ok(n as usize)
    }

    // ------------------------------------------------------------------- mutes

    /// ミュートを登録する(同一 `(kind, value)` は再登録せず既存を返す)。
    pub fn insert_mute(&self, kind: MuteKind, value: &str) -> Result<MuteEntry> {
        if value.is_empty() {
            return Err(StoreError::Validation("ミュート対象が空です"));
        }
        let created_at = unix_now();
        let conn = self.lock()?;
        conn.execute(
            "INSERT OR IGNORE INTO mutes (kind, value, created_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![kind.as_str(), value, created_at],
        )
        .map_err(map_sqlite)?;
        conn.query_row(
            "SELECT id, kind, value, created_at FROM mutes WHERE kind = ?1 AND value = ?2",
            rusqlite::params![kind.as_str(), value],
            row_to_mute,
        )
        .map_err(map_sqlite)
    }

    /// 全ミュートを id 昇順で列挙する。
    pub fn list_mutes(&self) -> Result<Vec<MuteEntry>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare("SELECT id, kind, value, created_at FROM mutes ORDER BY id ASC")
            .map_err(map_sqlite)?;
        let rows = stmt.query_map([], row_to_mute).map_err(map_sqlite)?;
        collect_rows(rows)
    }

    /// ミュートを id で削除する。削除できれば `true`。
    pub fn delete_mute(&self, id: i64) -> Result<bool> {
        let conn = self.lock()?;
        let n = conn
            .execute("DELETE FROM mutes WHERE id = ?1", [id])
            .map_err(map_sqlite)?;
        Ok(n > 0)
    }

    // ---------------------------------------------------------------- settings

    /// 設定値を取得する(未保存キーは `None`)。
    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let conn = self.lock()?;
        conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| {
            r.get::<_, String>(0)
        })
        .optional()
    }

    /// 設定値を保存する(UPSERT)。
    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![key, value],
        )
        .map_err(map_sqlite)?;
        Ok(())
    }

    /// 保存済み設定を全件取得する。
    pub fn all_settings(&self) -> Result<HashMap<String, String>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare("SELECT key, value FROM settings")
            .map_err(map_sqlite)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(map_sqlite)?;
        let mut map = HashMap::new();
        for row in rows {
            let (k, v) = row.map_err(map_sqlite)?;
            map.insert(k, v);
        }
        Ok(map)
    }
}

// ---------------------------------------------------------------------------
// 内部ヘルパ
// ---------------------------------------------------------------------------

/// ピア行の共通 SELECT(列順は [`row_to_peer`] と一致させる)。
const PEER_SELECT: &str =
    "SELECT id, addr, source, verified, enabled, added_at, last_ok_at, fail_count FROM peers";

fn row_to_persona(row: &Row<'_>) -> rusqlite::Result<Persona> {
    Ok(Persona {
        id: row.get(0)?,
        pubkey: row.get(1)?,
        secret_enc: row.get(2)?,
        label: row.get(3)?,
        created_at: row.get(4)?,
        state: PersonaState::from_db(&row.get::<_, String>(5)?),
    })
}

fn row_to_peer(row: &Row<'_>) -> rusqlite::Result<PeerEndpoint> {
    Ok(PeerEndpoint {
        id: row.get(0)?,
        addr: row.get(1)?,
        source: PeerSource::from_db(&row.get::<_, String>(2)?),
        verified: row.get::<_, i64>(3)? != 0,
        enabled: row.get::<_, i64>(4)? != 0,
        added_at: row.get(5)?,
        last_ok_at: row.get(6)?,
        fail_count: row.get(7)?,
    })
}

fn row_to_mute(row: &Row<'_>) -> rusqlite::Result<MuteEntry> {
    Ok(MuteEntry {
        id: row.get(0)?,
        kind: MuteKind::from_db(&row.get::<_, String>(1)?),
        value: row.get(2)?,
        created_at: row.get(3)?,
    })
}

fn query_peer(conn: &Connection, addr: &str) -> Result<Option<PeerEndpoint>> {
    conn.query_row(
        &format!("{PEER_SELECT} WHERE addr = ?1"),
        [addr],
        row_to_peer,
    )
    .optional()
}

/// 登録上限超過時に `manual` 以外を LRU 順(last_ok_at 昇順→added_at 昇順→id 昇順)で降格削除する。
fn enforce_peer_limit(conn: &Connection) -> Result<()> {
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM peers", [], |r| r.get(0))
        .map_err(map_sqlite)?;
    let excess = count - PEER_LIMIT as i64;
    if excess <= 0 {
        return Ok(());
    }
    conn.execute(
        "DELETE FROM peers WHERE id IN (
             SELECT id FROM peers WHERE source != 'manual'
             ORDER BY COALESCE(last_ok_at, 0) ASC, added_at ASC, id ASC
             LIMIT ?1
         )",
        [excess],
    )
    .map_err(map_sqlite)?;
    Ok(())
}

fn collect_rows<T>(rows: impl Iterator<Item = rusqlite::Result<T>>) -> Result<Vec<T>> {
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(map_sqlite)?);
    }
    Ok(out)
}

/// 指定長の小文字 hex 文字列か(pubkey 形式検証用)。
fn is_lower_hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `QueryReturnedNoRows` を `Ok(None)` に畳み込むための拡張。
trait OptionalRow<T> {
    fn optional(self) -> Result<Option<T>>;
}

impl<T> OptionalRow<T> for rusqlite::Result<T> {
    fn optional(self) -> Result<Option<T>> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(map_sqlite(e)),
        }
    }
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const PK1: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const PK2: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }

    #[test]
    fn schema_applies_and_tables_exist() {
        let s = store();
        // 各テーブルへの単純クエリが成功する = スキーマ適用済み
        assert_eq!(s.list_personas().unwrap().len(), 0);
        assert_eq!(s.list_peers().unwrap().len(), 0);
        assert_eq!(s.list_mutes().unwrap().len(), 0);
        assert!(s.all_settings().unwrap().is_empty());
    }

    #[test]
    fn persona_crud_roundtrip() {
        let s = store();
        let p = s.insert_persona(PK1, b"enc-secret", "配信A").unwrap();
        assert_eq!(p.pubkey, PK1);
        assert_eq!(p.label, "配信A");
        assert_eq!(p.state, PersonaState::Active);

        let got = s.get_persona_by_pubkey(PK1).unwrap().unwrap();
        assert_eq!(got.secret_enc, b"enc-secret");

        assert!(s.update_persona_label(PK1, "配信B").unwrap());
        assert!(s.update_persona_state(PK1, PersonaState::Archived).unwrap());
        let got = s.get_persona_by_pubkey(PK1).unwrap().unwrap();
        assert_eq!(got.label, "配信B");
        assert_eq!(got.state, PersonaState::Archived);

        assert!(s.delete_persona(PK1).unwrap());
        assert!(s.get_persona_by_pubkey(PK1).unwrap().is_none());
        assert!(!s.delete_persona(PK1).unwrap());
    }

    #[test]
    fn persona_duplicate_pubkey_rejected() {
        let s = store();
        s.insert_persona(PK1, b"a", "x").unwrap();
        let err = s.insert_persona(PK1, b"b", "y").unwrap_err();
        assert!(matches!(err, StoreError::Duplicate));
    }

    #[test]
    fn persona_label_over_64_chars_rejected() {
        let s = store();
        let label = "あ".repeat(65);
        let err = s.insert_persona(PK1, b"a", &label).unwrap_err();
        assert!(matches!(err, StoreError::Validation(_)));
        // 64 文字ちょうどは許容
        let label64 = "あ".repeat(64);
        assert!(s.insert_persona(PK1, b"a", &label64).is_ok());
    }

    #[test]
    fn persona_invalid_pubkey_rejected() {
        let s = store();
        assert!(matches!(
            s.insert_persona("XYZ", b"a", "x").unwrap_err(),
            StoreError::Validation(_)
        ));
        assert!(matches!(
            s.insert_persona(&"A".repeat(64), b"a", "x").unwrap_err(),
            StoreError::Validation(_)
        ));
    }

    #[test]
    fn peer_upsert_and_get() {
        let s = store();
        let p = s.upsert_peer("127.0.0.1:7147", PeerSource::Manual).unwrap();
        assert_eq!(p.source, PeerSource::Manual);
        assert!(!p.verified);
        assert!(p.enabled);
        assert_eq!(p.fail_count, 0);
        assert!(p.last_ok_at.is_none());

        // 再 upsert は既存を返す(重複挿入しない)
        let again = s.upsert_peer("127.0.0.1:7147", PeerSource::Pex).unwrap();
        assert_eq!(again.id, p.id);
        assert_eq!(s.count_peers().unwrap(), 1);
    }

    #[test]
    fn peer_pex_promoted_to_manual() {
        let s = store();
        s.upsert_peer("10.0.0.1:7147", PeerSource::Pex).unwrap();
        let promoted = s.upsert_peer("10.0.0.1:7147", PeerSource::Manual).unwrap();
        assert_eq!(promoted.source, PeerSource::Manual);
    }

    #[test]
    fn peer_addr_validation() {
        let s = store();
        assert!(matches!(
            s.upsert_peer("", PeerSource::Manual).unwrap_err(),
            StoreError::Validation(_)
        ));
        let long = format!("{}:7147", "a".repeat(ADDR_MAX_LEN));
        assert!(matches!(
            s.upsert_peer(&long, PeerSource::Manual).unwrap_err(),
            StoreError::Validation(_)
        ));
    }

    #[test]
    fn peer_success_and_failure_tracking() {
        let s = store();
        s.upsert_peer("host:7147", PeerSource::Pex).unwrap();
        assert_eq!(s.record_peer_failure("host:7147").unwrap(), Some(1));
        assert_eq!(s.record_peer_failure("host:7147").unwrap(), Some(2));
        assert!(s.record_peer_success("host:7147", 12345).unwrap());
        let p = s.get_peer("host:7147").unwrap().unwrap();
        assert!(p.verified);
        assert_eq!(p.fail_count, 0);
        assert_eq!(p.last_ok_at, Some(12345));
        // 存在しないピア
        assert_eq!(s.record_peer_failure("nope:1").unwrap(), None);
        assert!(!s.record_peer_success("nope:1", 1).unwrap());
    }

    #[test]
    fn peer_enabled_and_delete() {
        let s = store();
        s.upsert_peer("host:7147", PeerSource::Manual).unwrap();
        assert!(s.set_peer_enabled("host:7147", false).unwrap());
        assert!(!s.get_peer("host:7147").unwrap().unwrap().enabled);
        assert!(s.delete_peer("host:7147").unwrap());
        assert!(s.get_peer("host:7147").unwrap().is_none());
    }

    #[test]
    fn verified_peers_ordered_by_recency() {
        let s = store();
        for (addr, ok) in [("a:1", 100), ("b:1", 300), ("c:1", 200)] {
            s.upsert_peer(addr, PeerSource::Pex).unwrap();
            s.record_peer_success(addr, ok).unwrap();
        }
        // 未検証は含めない
        s.upsert_peer("d:1", PeerSource::Pex).unwrap();
        // 無効は含めない
        s.upsert_peer("e:1", PeerSource::Manual).unwrap();
        s.record_peer_success("e:1", 999).unwrap();
        s.set_peer_enabled("e:1", false).unwrap();

        let list = s.verified_peers_by_recency(Some(64)).unwrap();
        let addrs: Vec<&str> = list.iter().map(|p| p.addr.as_str()).collect();
        assert_eq!(addrs, vec!["b:1", "c:1", "a:1"]);
    }

    #[test]
    fn peer_limit_evicts_lru_non_manual() {
        let s = store();
        // manual を 1 件(LRU 免除)
        s.upsert_peer("manual:1", PeerSource::Manual).unwrap();
        // pex を上限まで + 1 件登録し、最古 pex が降格削除されることを確認
        for i in 0..PEER_LIMIT {
            let addr = format!("pex-{i}:1");
            s.upsert_peer(&addr, PeerSource::Pex).unwrap();
            // last_ok_at を i にして LRU 順序を決定づける
            s.record_peer_success(&addr, i as i64).unwrap();
        }
        // ここで total = 1 (manual) + PEER_LIMIT (pex) = PEER_LIMIT + 1 → 1 件降格されるはず
        assert_eq!(s.count_peers().unwrap(), PEER_LIMIT);
        // 最古(last_ok_at=0)の pex-0 が消え、manual は残る
        assert!(s.get_peer("pex-0:1").unwrap().is_none());
        assert!(s.get_peer("manual:1").unwrap().is_some());
        assert!(
            s.get_peer(&format!("pex-{}:1", PEER_LIMIT - 1))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn manual_peers_exempt_from_lru() {
        let s = store();
        // manual だけで上限を超えて登録しても降格されない
        for i in 0..(PEER_LIMIT + 5) {
            s.upsert_peer(&format!("m-{i}:1"), PeerSource::Manual)
                .unwrap();
        }
        assert_eq!(s.count_peers().unwrap(), PEER_LIMIT + 5);
        assert!(s.get_peer("m-0:1").unwrap().is_some());
    }

    #[test]
    fn mute_crud_and_dedup() {
        let s = store();
        let m = s.insert_mute(MuteKind::Pubkey, PK1).unwrap();
        assert_eq!(m.kind, MuteKind::Pubkey);
        // 同一 (kind, value) は再登録せず同じ id
        let again = s.insert_mute(MuteKind::Pubkey, PK1).unwrap();
        assert_eq!(again.id, m.id);
        // kind が違えば別エントリ
        s.insert_mute(MuteKind::Channel, PK1).unwrap();
        assert_eq!(s.list_mutes().unwrap().len(), 2);
        assert!(s.delete_mute(m.id).unwrap());
        assert_eq!(s.list_mutes().unwrap().len(), 1);
    }

    #[test]
    fn settings_get_set_all() {
        let s = store();
        assert!(s.get_setting("pcp_bind").unwrap().is_none());
        s.set_setting("pcp_bind", "127.0.0.1:7146").unwrap();
        s.set_setting("pcp_bind", "127.0.0.1:9999").unwrap(); // UPSERT
        assert_eq!(
            s.get_setting("pcp_bind").unwrap().unwrap(),
            "127.0.0.1:9999"
        );
        s.set_setting("pex_enabled", "1").unwrap();
        let all = s.all_settings().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all["pex_enabled"], "1");
    }

    #[test]
    fn open_in_dir_creates_db_file() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("nested");
        let s = Store::open_in_dir(&sub).unwrap();
        s.insert_persona(PK2, b"x", "y").unwrap();
        assert!(sub.join("app.db").exists());
    }
}

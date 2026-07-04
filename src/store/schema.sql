-- SQLite スキーマ(T012 — data-model.md rev 2 準拠)
--
-- 永続エンティティは 4 テーブルのみ: personas / peers / mutes / settings。
-- リレー排除(spec Clarifications 2026-07-03・FR-014)により relays テーブルは存在しない。
-- 復活させてはならない (MUST NOT)。
--
-- 起動時に CREATE TABLE IF NOT EXISTS で冪等に適用する(include_str! で埋め込み)。

-- ペルソナ(匿名文化の単位。秘密鍵は DPAPI 暗号化 BLOB のみ保存)
CREATE TABLE IF NOT EXISTS personas (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    pubkey      TEXT NOT NULL UNIQUE,   -- nostr 公開鍵(hex 64)。ネットワーク上の識別子
    secret_enc  BLOB NOT NULL,          -- DPAPI 暗号化済み秘密鍵。平文保存禁止 (MUST NOT)
    label       TEXT NOT NULL,          -- ローカル表示名(ネットワークには出さない)
    created_at  INTEGER NOT NULL,       -- 作成時刻(unix 秒)
    state       TEXT NOT NULL           -- 'active' / 'archived'
);

-- ピア(gossip 接続の候補・実績。手動シードまたは PEX で獲得)
CREATE TABLE IF NOT EXISTS peers (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    addr        TEXT NOT NULL UNIQUE,   -- host:port(長さ ≤ 256)
    source      TEXT NOT NULL,          -- 'manual' / 'pex'
    verified    INTEGER NOT NULL DEFAULT 0,  -- 0/1。自ノードの外向き接続成功実績
    enabled     INTEGER NOT NULL DEFAULT 1,  -- 0/1。無効化=切り離し
    added_at    INTEGER NOT NULL,
    last_ok_at  INTEGER,                -- 最終接続成功時刻(NULL 可)
    fail_count  INTEGER NOT NULL DEFAULT 0   -- 連続接続失敗数
);

-- ミュート(ローカル保存のみ。ネットワークへは公開しない)
CREATE TABLE IF NOT EXISTS mutes (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    kind        TEXT NOT NULL,          -- 'pubkey' / 'channel'
    value       TEXT NOT NULL,          -- hex pubkey または hex32 GUID
    created_at  INTEGER NOT NULL,
    UNIQUE(kind, value)
);

-- 設定(key-value。既定値の単一出典は data-model §Settings / config.rs)
CREATE TABLE IF NOT EXISTS settings (
    key         TEXT PRIMARY KEY,
    value       TEXT NOT NULL
);

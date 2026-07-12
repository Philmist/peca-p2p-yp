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
    addr        TEXT NOT NULL UNIQUE,   -- host:port(長さ ≤ 256)。manual はホスト名も可(ADR-0010)
    source      TEXT NOT NULL,          -- 'manual' / 'pex'
    verified    INTEGER NOT NULL DEFAULT 0,  -- 0/1。自ノードの外向き接続成功実績
    enabled     INTEGER NOT NULL DEFAULT 1,  -- 0/1。無効化=切り離し
    added_at    INTEGER NOT NULL,
    last_ok_at  INTEGER,                -- 最終接続成功時刻(NULL 可)
    fail_count  INTEGER NOT NULL DEFAULT 0,  -- 連続接続失敗数
    resolved_ip TEXT                    -- 外向き成功時の実ソケット IP(canonical。PEX 射影専用・ダイヤル不使用 — ADR-0010)
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

-- ===========================================================================
-- 006-livechat-thread(T007 — data-model.md §永続化)
--
-- スレデータ(レス・順序確定情報)は揮発でメモリのみ(FR-015)。テーブルを作らない。
-- 復活させてはならない (MUST NOT)。以下 3 テーブルのみが livechat の永続情報。
-- ===========================================================================

-- 板鍵(自分の書き込み鍵。板 = 配信者ペルソナ単位で 1 本 — FR-016)
-- 秘密鍵は keystore 暗号化エンベロープのみ保存(平文保存 MUST NOT — research R8)。
-- personas とは識別子・外部キーを一切共有しない(構造分離 — FR-016)。
CREATE TABLE IF NOT EXISTS board_keys (
    board_id    TEXT PRIMARY KEY,       -- 板スコープ = スレ主(配信者)ペルソナの公開鍵(hex 64)
    pubkey      TEXT NOT NULL,          -- 板鍵の公開鍵(hex 64)。ID 表示・NG/BAN 完全鍵照合に使用
    secret_enc  BLOB NOT NULL,          -- keystore 暗号化済み板鍵秘密鍵。平文保存禁止 (MUST NOT)
    created_at  INTEGER NOT NULL        -- 生成/ローテーション時刻(unix 秒。ローテーションで行ごと置換)
);

-- NG / BAN(ローカル保存のみ。ネットワークへは送出しない = 不変条件 M1 — FR-019)
CREATE TABLE IF NOT EXISTS livechat_moderation (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    board_id    TEXT NOT NULL,          -- 板スコープ(pubkey hex 64)
    kind        TEXT NOT NULL,          -- 'ng' / 'ban' / 'conn_ban'
    target      TEXT NOT NULL,          -- 完全鍵(hex 64)または接続元アドレス。短縮 ID 照合禁止(FR-018)
    created_at  INTEGER NOT NULL,
    UNIQUE(board_id, kind, target)
);

-- 板設定(自分が板主の板。BoardSettings — FR-022〜FR-025)
CREATE TABLE IF NOT EXISTS board_settings (
    board_id             TEXT PRIMARY KEY,  -- 板スコープ(= 自ペルソナ pubkey hex 64)
    title                TEXT NOT NULL,     -- ≤ 128 文字
    res_limit            INTEGER NOT NULL,  -- 100〜4000(既定 1000)
    noname_name          TEXT NOT NULL,     -- 1〜64 文字
    local_rules          TEXT NOT NULL,     -- ≤ 2048 文字(Markdown)
    first_post_pow_bits  INTEGER NOT NULL   -- 0〜32(既定 20)。唯一の正式名(ノード Settings には置かない)
);

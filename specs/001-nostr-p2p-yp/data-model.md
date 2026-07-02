# Data Model: 分散型配信情報共有ネットワーク(YP代替)

**Date**: 2026-07-02 | **Plan**: [plan.md](./plan.md)

エンティティは 3 層に分かれる: **永続(SQLite)** / **メモリ(揮発)** / **ネットワーク(nostr イベント)**。
ネットワーク表現の詳細は [contracts/nostr-events.md](./contracts/nostr-events.md) を参照。

## 永続エンティティ(SQLite)

### Persona(ペルソナ)

匿名文化の単位。1 利用者が複数保持し、切替・破棄できる(FR-013)。

| フィールド | 型 | 制約 |
|-----------|-----|------|
| id | INTEGER PK | 自動採番(ローカル管理用。ネットワークには出さない) |
| pubkey | TEXT UNIQUE | nostr 公開鍵(hex 64)。ネットワーク上の識別子 |
| secret_enc | BLOB | DPAPI で暗号化された秘密鍵。平文で保存してはならない (MUST NOT) |
| label | TEXT | ローカル表示名(ネットワークには出さない。ペルソナ間リンク防止 — FR-013) |
| created_at | INTEGER | 作成時刻(unix) |
| state | TEXT | `active` / `archived` |

**状態遷移**: `created → active ⇄ archived → deleted(行削除)`
削除時は secret_enc を含む行を完全に削除する。破棄されたペルソナの復元は不可(匿名性優先)。

**検証ルール**: label は 64 文字以内。同一 pubkey の重複登録拒否。

### RelayEndpoint(共有先リレー)

| フィールド | 型 | 制約 |
|-----------|-----|------|
| id | INTEGER PK | |
| url | TEXT UNIQUE | `wss://`(推奨)または `ws://`(警告付きで許可)。それ以外のスキームは拒否 |
| enabled | INTEGER | 0/1。無効化 = 切り離し(FR-008 緩和策) |
| read | INTEGER | 購読に使う |
| write | INTEGER | 掲載に使う |
| added_at | INTEGER | |
| last_ok_at | INTEGER NULL | 最終接続成功時刻(UI の健全性表示用) |

**検証ルール**: URL 長 512 以内、パース可能な WebSocket URL であること。登録数上限 50。

### MuteEntry(ミュート)

| フィールド | 型 | 制約 |
|-----------|-----|------|
| id | INTEGER PK | |
| kind | TEXT | `pubkey`(ペルソナ単位)/ `channel`(チャンネル GUID 単位) |
| value | TEXT | hex pubkey または hex32 GUID |
| created_at | INTEGER | |

NIP-51(kind 10000)のローカル相当。**ネットワークへは公開しない**(閲覧傾向の漏洩防止)。

### Settings(設定)

key-value(TEXT/TEXT)。主なキー:

| キー | 既定値 | 意味 |
|------|--------|------|
| pcp_bind | `127.0.0.1:7146` | PCP アナウンス待受 |
| http_bind | `127.0.0.1:7180` | HTTP(UI・index.txt)待受 |
| index_txt_encoding | `shift_jis` | `shift_jis` / `utf-8` |
| freshness_window_sec | `600` | 鮮度判定窓(FR-006) |
| republish_interval_sec | `60` | 掲載中の再発行間隔 |
| min_pow_bits | `0` | 受信フィルタの最小 NIP-13 難易度(0=無効) |

## メモリ上エンティティ(揮発)

### AnnouncedChannel(自分が掲載中のチャンネル)

PeerCastStation から PCP で受信した配信中チャンネル。nostr への掲載元。

| フィールド | 由来(PCP) | 検証 |
|-----------|-----------|------|
| channel_id | BroadcastID/ChannelID (GUID) | 16 バイト固定 |
| name | `name` | 1..256 バイト(UTF-8)、制御文字除去 |
| genre | `gnre` | 0..256 バイト |
| description | `desc` | 0..1024 バイト |
| contact_url | `url` | 0..512 バイト。表示前に URL 警告判定(FR-012) |
| bitrate_kbps | `bitr` | 0..100_000 |
| content_type | `type` | 0..32 バイト英数 |
| track | `titl`/`crea`/`albm` + track url | 各 0..256 バイト |
| tracker | PCP_HOST(グローバル IP:port) | IP:port 形式。firewalled 時は空 |
| listeners / relays_cnt | `numl`/`numr` | -1(非表示)以上 |
| started_at | 受信時に記録 | |
| persona_id | UI で選択されたペルソナ | 掲載に使う鍵 |
| session_state | — | `announced → updating ⇄ … → ended` |

**状態遷移**:
`(PCP HELO/OLEH 完了) → announced → [BCST 更新で updating を繰返し] → (playing=false または切断) → ended`
`ended` 遷移時に `status=ended` の最終イベントを発行し、以後この channel_id の掲載を停止する。

### DiscoveredChannel(リレーから発見したチャンネル)

視聴者の一覧の 1 行。kind 30311 の検証済みイベントから構築。

| フィールド | 由来 | 検証 |
|-----------|------|------|
| author_pubkey | event.pubkey | 署名検証必須(nostr-sdk)。失敗は破棄+セキュリティログ(FR-005) |
| channel_id | `d` タグ | hex 32 桁。不一致は破棄 |
| (name, genre, description, contact_url, bitrate, content_type, track, tracker, listeners, relays_cnt, started_at) | contracts/nostr-events.md のタグ写像 | 同契約の上限・形式チェック |
| status | `status` タグ | `live` / `ended`。それ以外は破棄 |
| created_at | event.created_at | 未来方向 +300 秒超は破棄(時計ずれ対策) |
| source_relays | 受信リレー集合 | UI 表示・リレー品質判断用 |
| muted | MuteEntry 照合 | 一覧から除外(既定はオープン型 — 除外はミュート時のみ) |

**同一性と更新規則(集約)**: 一覧のキーは `(author_pubkey, channel_id)`。
同一キーは `created_at` が最大のイベントで置換(last-write-wins)。
`status=ended` または `now - created_at > freshness_window_sec` で一覧から除去(FR-006)。
**同名チャンネルでも author_pubkey が異なれば別行**として扱う(なりすましは併存表示され、
ペルソナの継続性で判別する — spec Clarifications のペルソナモデル)。

### SecurityEvent(セキュリティイベントログ)

構造化ログ(tracing)としてファイルへ追記。DB には保存しない。

| フィールド | 内容 |
|-----------|------|
| ts / category | `pcp_reject` / `nostr_invalid_sig` / `nostr_oversize` / `http_rate_limited` / `url_warning` 等 |
| source | 接続元(loopback アドレス / リレー URL) |
| detail | 内部情報(スタックトレース・パス)を含めてはならない (MUST NOT — Principle II) |

## エンティティ関係

```text
Persona 1 ──< AnnouncedChannel(掲載時に使用する鍵)
Persona(pubkey) ──< DiscoveredChannel.author_pubkey(ネットワーク上の対応)
RelayEndpoint >──< DiscoveredChannel.source_relays
MuteEntry ──> DiscoveredChannel.muted(表示制御のみ。データは破棄しない)
```

将来フェーズ(実況コメント)は `DiscoveredChannel` のアドレス
`30311:<author_pubkey>:<channel_id>` を kind 1311 の `a` タグで参照する(FR-011 の互換保証)。

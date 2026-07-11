# Data Model: 配信実況スレ(P2P 掲示板)

**Feature**: `006-livechat-thread` | **Date**: 2026-07-12 | **Input**: [spec.md](./spec.md), [research.md](./research.md)

エンティティの正本は本ファイル。イベントの直列化・検証順序は
[contracts/thread-events.md](./contracts/thread-events.md)、ワイヤ上の交換は
[contracts/thread-delivery.md](./contracts/thread-delivery.md) を参照。

## 永続化の方針(FR-015)

| データ | 保存先 | 生存期間 |
|--------|--------|----------|
| スレデータ(レス・順序確定情報) | メモリのみ | スレのクローズ通知・ノード再起動で消滅(揮発) |
| 板鍵(自分の書き込み鍵) | SQLite `board_keys`(秘密鍵は keystore 暗号化 — research R8) | 明示ローテーション・削除まで |
| NG / BAN エントリ | SQLite `livechat_moderation` | 明示解除まで(板単位スコープ) |
| 板設定(自分が板主の板) | SQLite `board_settings` | 板主が変更するまで |
| スレ announce | EventStore(kind 31311 独立枠 — research R3) | `expiration` 超過で除去(鮮度 600 秒) |

## エンティティ

### 板(Board)

配信者ペルソナ単位の掲示板。板鍵・NG・BAN・板設定のスコープ(FR-012)。

| フィールド | 型 | 制約 |
|-----------|-----|------|
| `board_id` | pubkey(hex 64) | = スレ主(配信者)ペルソナの公開鍵。板の同一性の唯一の根拠 |
| `active_thread` | Thread への参照 | 常に 0 or 1 本(FR-012) |
| `settings` | BoardSettings | 下記 |

### 板設定(BoardSettings)— FR-022〜FR-025

| フィールド | 型 | 制約 | 反映タイミング |
|-----------|-----|------|---------------|
| `title` | 文字列 | ≤ 128 文字。制御文字除去 | 即時 |
| `res_limit` | u16 | 100〜4000、既定 1000 | **次スレから**(進行中スレは作成時の値で固定) |
| `noname_name` | 文字列 | 1〜64 文字。制御文字除去 | 即時 |
| `local_rules` | 文字列 | ≤ 2048 文字(Markdown。描画は安全なサブセット — research R7) | 即時 |
| `first_post_pow_bits` | u8 | 0〜32、既定 20(research R6)。**唯一の正式名**(ノード Settings には置かない) | 即時 |

受信側検証: 上記制約への違反は破棄 + `livechat_settings_invalid`(FR-025)。

### スレ(Thread)

| フィールド | 型 | 制約 |
|-----------|-----|------|
| `board_id` | pubkey | スレ主ペルソナ |
| `channel` | `30311:<pubkey>:<guid>` | 対象チャンネル(announce の `a` タグ) |
| `gen` | u32 | スレ番号(次スレの世代)。板内で単調増加 |
| `key` | u64 | スレ作成 unix 秒。互換 API の dat キー(contracts/compat-api.md) |
| `title` | 文字列 | ≤ 128 文字 |
| `res_limit` | u16 | **作成時の板設定のスナップショット**(FR-023) |
| `state` | enum | `Active` / `Frozen` / `Closed`(下記状態遷移) |
| `res` | 順序付き列 | 確定レス(res_no 1..=res_limit) |

**状態遷移**:

```text
(開設) → Active
Active → Active(次スレ移行: 旧スレは Frozen 扱いで閲覧のみ、新 Thread gen+1 が Active に)
Active → Frozen   : ホストとの接続喪失・announce 鮮度切れ(通知なき切断)
Frozen → Active   : ホスト再接続に成功し同一 gen が継続していた場合(瞬断復帰)
Active/Frozen → Closed : スレ主署名付きクローズ通知の受信 → スレデータ削除(揮発)
```

- 不変条件 T1: `state != Active` のスレへの書き込みは受理されない(採番もされない)
- 不変条件 T2: 板内で `Active` は高々 1 本(FR-012)
- 不変条件 T3: `res_no` は 1 から欠番なく単調増加し、`res_limit` を超えない
  (欠番 = 未達であり採番の飛びではない。NG による欠番は表示上のみ — FR-020)

### レス(Res)— kind 1311(research R1)

| フィールド | 型 | 制約 |
|-----------|-----|------|
| `event_id` | nostr id | 一意性の根拠 |
| `board_key` | pubkey | 署名者 = 板鍵。ID 表示・NG/BAN 照合(完全鍵 — FR-018)に使用 |
| `name` | 文字列 | ≤ 64 文字。**送信前に `#` 以降を除去**(FR-024)。空は**当該レス確定時点の** `noname_name` で表示し、以後の板設定変更は遡及しない(FR-023 — dat 追記不変性の基盤) |
| `mail` | 文字列 | ≤ 64 文字。表示互換のみ・機能的意味なし(FR-029) |
| `body` | 文字列 | ≤ 2048 文字かつ ≤ 32 行(単位は文字数 — SETTING.TXT の提示単位と一致、contracts/compat-api.md)。直列化イベント全体 ≤ 16KB は別途適用。制御文字除去(改行除く) |
| `created_at` | unix 秒 | 参考情報。正となる順序は res_no のみ(spec Edge Case) |
| `res_no` | u16 | **確定後のみ存在**(順序確定情報が与える)。未確定レスに番号はない |
| `pending` | bool | 自分の未確定投稿のみ true(「送信中」表示 — FR-008) |

### 順序確定情報(OrderInfo)— kind 21311(research R2)

| フィールド | 型 | 制約 |
|-----------|-----|------|
| `thread` | (board_id, gen) | 対象スレ |
| `seq` | u32 | 確定情報自体の連番(欠落検出 → 再送要求に使用) |
| `entries` | `[(res_no, event_id)]` 配列 | 今回確定した採番。res_no は既存確定の続きから欠番なし |
| 署名 | スレ主ペルソナ | 不一致は破棄 + `livechat_order_invalid`(FR-011) |

- 不変条件 O1: 同一 res_no に異なる event_id を与える確定情報をスレ主が発行しない
  (発行側の不変条件 — PlusCal モデルの検査対象、research R9)
- 不変条件 O2: 受信側は seq の欠落を検出したら表示を進めず再送要求する(spec Edge Case)

### スレ announce — kind 31311(research R2)

| フィールド | 型 | 制約 |
|-----------|-----|------|
| 署名者 | pubkey | **対象チャンネルの掲載ペルソナと一致必須**(FR-003)。不一致は不可視 + `livechat_announce_invalid` |
| `channel` | `a` タグ | 対象チャンネル(30311 参照) |
| `gen` / `key` / `title` / `res_count` | — | スレの現況(一覧表示用) |
| `tip` | `ip:port` | ホスト接続先。**受信のみでは接続しない**(FR-004) |
| `expiration` | unix 秒 | created_at + 600。30311 と同一の鮮度規則(FR-002) |

### 板鍵(BoardKey)— research R8

| フィールド | 型 | 制約 |
|-----------|-----|------|
| `board_id` | pubkey | スコープ(板単位固定 — FR-016) |
| `keypair` | nostr 鍵ペア | 秘密鍵は keystore 暗号化。ペルソナと識別子・テーブルを共有しない |
| `created_at` | unix 秒 | ローテーション(FR-017)で行ごと置換(旧鍵は破棄) |

### NG / BAN エントリ

| フィールド | 型 | 制約 |
|-----------|-----|------|
| `board_id` | pubkey | スコープ |
| `kind` | enum | `Ng`(視聴者ローカル非表示)/ `Ban`(スレ主: 採番拒否)/ `ConnBan`(スレ主: 接続拒否) |
| `target` | pubkey(完全鍵)または接続元アドレス | 短縮 ID 照合禁止(FR-018) |

- 不変条件 M1: NG/BAN はネットワークへ送信されない(FR-019 — ローカル情報)

## Settings 追加(001 data-model §Settings への追記)

| キー | 既定値 | 制約 |
|------|--------|------|
| `livechat_enabled` | true | false でスレ機能全体を無効化(announce は検証のみ・不可視) |
| `thread_max_participants` | 128 | ホストの受入接続上限(spec Assumptions)。超過は定型拒否(FR-006) |
| `thread_write_rate` | 板鍵あたり 4 レス/30 秒 | ホスト側強制(FR-021) |
| `thread_msg_rate` | 接続あたり 16 msg/秒 | 制御メッセージ込み(FR-021) |
| `announce_store_quota` | 2048 | kind 31311 の EventStore 独立保持枠(research R3) |
| `compat_bbs_bind` | `127.0.0.1:7183` | loopback のみ受理・非 loopback は起動拒否。空文字で無効化(research R5) |

## SecurityEvent 追加カテゴリ(001 data-model §SecurityEvent への追記 — 15 → 21)

| カテゴリ | 契機 | 対応 FR |
|----------|------|---------|
| `livechat_announce_invalid` | announce の署名者がチャンネル掲載ペルソナと不一致・形式違反 | FR-003 |
| `livechat_challenge_failed` | 接続時チャレンジの検証失敗(切断 + バックオフ) | FR-005 |
| `livechat_order_invalid` | スレ主以外の鍵で署名された順序確定情報 | FR-011 |
| `livechat_write_rejected` | サイズ・形式・PoW・レート違反の書き込み(ホスト側)。**BAN による採番拒否は記録するが応答では理由を開示しない**(spec Edge Case) | FR-007/FR-021 |
| `livechat_settings_invalid` | 検証に失敗する板設定の受信 | FR-025 |
| `compat_bbs_denied` | 互換 API への loopback 外アクセス・Host 検証失敗・レート違反 | FR-026 |

既存カテゴリの再利用: gossip 経由の announce の署名・サイズ・形式・時刻違反は既存の
`event_*` カテゴリ(001 検査 #1〜#6)がそのまま適用される。上記は本機能固有の意味を
持つ違反のみを追加する。

## 原則参照

- 揮発性と鍵・モデレーションの永続の分離: Principle I(残骸を残さない)/ FR-015
- 完全鍵照合・非リンク保管: Principle II / FR-016〜FR-018
- SecurityEvent 追加: Security Requirements(セキュリティログ)
- 不変条件 T3/O1/O2: Principle V(PlusCal 検査対象 — research R9)

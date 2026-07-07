# Data Model: 分散型配信情報共有ネットワーク(YP代替)

**Date**: 2026-07-03 (rev 2) | **Plan**: [plan.md](./plan.md)

エンティティは 3 層に分かれる: **永続(SQLite)** / **メモリ(揮発)** / **ネットワーク(署名済みイベント)**。
ネットワーク表現の詳細は [contracts/nostr-events.md](./contracts/nostr-events.md)、
ノード間の伝送は [contracts/p2p-gossip.md](./contracts/p2p-gossip.md) を参照。

**rev 2**: リレー排除(spec Clarifications 2026-07-03)に伴い、RelayEndpoint を PeerEndpoint に
置換し、EventStore(ローカル置換ストア)と DedupCache を追加した。

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

**DPAPI 復号失敗時の挙動**: secret_enc の復号に失敗した場合(BLOB 破損・別 Windows
ユーザープロファイル・OS 再インストール後など)、当該ペルソナを「利用不可」として UI に
明示し、起動と他ペルソナ・閲覧機能は継続する(全体を停止させない)。復元手段は提供しない
(事前の nsec エクスポート — research R6 — が唯一のバックアップ手段であることを UI で案内する)。

### PeerEndpoint(ピア — 接続先の利用者ノード)

手動シード登録またはピア交換(PEX)で得た、gossip 接続の候補・実績(FR-010, FR-015)。

| フィールド | 型 | 制約 |
|-----------|-----|------|
| id | INTEGER PK | |
| addr | TEXT UNIQUE | `host:port`(IPv4/IPv6 リテラルまたはホスト名、長さ ≤ 256)。IPv6 リテラルは `[addr]:port` のブラケット表記のみ許容(例 `[2001:db8::1]:7147`)。ブラケットなしでコロンを複数含む文字列はポート境界が曖昧なためパース不能として拒否。**ホスト名は manual 登録経路のみ許容**(RFC 1123・ASCII・末尾ドット正規化 — ADR-0010) |
| source | TEXT | `manual`(手動登録)/ `pex`(ピア交換で獲得) |
| verified | INTEGER | 0/1。**自ノードが接続に成功した実績**があるか(research R14)。未検証ピアは PEX で再共有してはならない (MUST NOT) |
| enabled | INTEGER | 0/1。無効化 = 切り離し(FR-008 緩和策) |
| added_at | INTEGER | |
| last_ok_at | INTEGER NULL | 最終接続成功時刻(UI の健全性表示・PEX 共有判定用) |
| fail_count | INTEGER | 連続接続失敗数。**8 回**の連続失敗で平常時の接続候補から降格(research R14)。接続成功で 0 にリセット。降格からの復帰は、利用者の手動操作(削除→再登録・enabled 再設定)または全ピア到達不能時の再試行(contracts/p2p-gossip.md §接続管理)での接続成功による |
| resolved_ip | TEXT NULL | 外向き接続成立時に捕捉した実ソケット IP(canonical)。**ホスト名 manual ピアの PEX 射影専用**(ADR-0010)。ダイヤルには使わない(外向きは常にホスト名を再解決)。IP リテラルピアでは冗長 |

**検証ルール**: 登録数上限 1,024(LRU で降格・削除)。自分自身のアドレス(ループバック検出)は登録拒否。
`manual` は利用者が明示削除するまで LRU 対象外。ホスト名 manual ピアはダイヤル毎に再解決し、
**PEX には解決済み IP(`resolved_ip`)のみを射影**して共有する(ホスト名は網に出さない — ADR-0010)。

### MuteEntry(ミュート)

| フィールド | 型 | 制約 |
|-----------|-----|------|
| id | INTEGER PK | |
| kind | TEXT | `pubkey`(ペルソナ単位)/ `channel`(チャンネル GUID 単位) |
| value | TEXT | hex pubkey または hex32 GUID |
| created_at | INTEGER | |

ローカル保存のみ。**ネットワークへは公開しない**(閲覧傾向の漏洩防止)。

**適用規則**: `pubkey` 単位と `channel` 単位は独立に評価し、**いずれかに一致すれば非表示**(OR)。
両者の間に優先順位はない(解除はエントリごとに行う)。

### Settings(設定)

key-value(TEXT/TEXT)。主なキー:

| キー | 既定値 | 意味 |
|------|--------|------|
| pcp_bind | `127.0.0.1:7146` | PCP アナウンス待受 |
| http_bind | `127.0.0.1:7180` | HTTP(UI・index.txt)待受 |
| p2p_bind | `0.0.0.0:7147,[::]:7147` | P2P gossip 待受(唯一の外部露出。カンマ区切りで複数アドレス指定可=IPv4/IPv6 同時待受、一部バインド失敗は残りで続行 — ADR-0008。空文字で待受無効=外向きのみ — FR-016) |
| p2p_outbound_target | `8` | 維持する外向き接続数の目標(research R16) |
| p2p_inbound_max | `32` | 着信接続の上限 |
| pex_enabled | `1` | ピア交換の有効/無効(FR-015) |
| upnp_enabled | `1` | UPnP ポートマッピング試行(research R15) |
| index_txt_encoding | `utf-8` | `utf-8` / `shift_jis`(2026-07-04 実機検証で既定を UTF-8 へ改訂 — contracts/http-yp.md) |
| freshness_window_sec | `600` | 鮮度判定窓(FR-006) |
| republish_interval_sec | `60` | 掲載中の再発行間隔 |
| max_clock_skew_sec | `300` | 受信イベントの未来方向時刻ずれ許容(spec Edge Case「時計のずれ」) |
| min_pow_bits | `0` | 受信フィルタの最小 NIP-13 難易度(0=無効。閾値未満は保持も再伝搬もしない) |
| event_store_max | `4096` | イベントストア上限(research R16) |

**時刻関連定数の単一出典**: 鮮度窓(600)・`expiration`(created_at + 600)・再発行間隔(60)・
時計スキュー許容(±300)の正は本表とする。spec Edge Case・contracts/nostr-events.md の記載は
本表の値を参照する(スキュー許容 300 = freshness_window_sec / 2 で、±300 秒未満のずれでは
鮮度・期限判定の誤除去が生じない関係を維持して変更すること)。

## メモリ上エンティティ(揮発)

### AnnouncedChannel(自分が掲載中のチャンネル)

PeerCastStation から PCP で受信した配信中チャンネル。イベント発行(掲載)の情報源。

| フィールド | 由来(PCP) | 検証 |
|-----------|-----------|------|
| channel_id | BroadcastID/ChannelID (GUID) | 16 バイト固定 |
| name | `name` | 1..256 バイト(UTF-8)、制御文字除去 |
| genre | `gnre` | 0..256 バイト |
| description | `desc` | 0..1024 バイト |
| contact_url | `url` | 0..512 バイト。表示前に URL 警告判定(FR-012) |
| bitrate_kbps | `bitr` | 0..100_000 |
| content_type | `type` | 0..32 バイト英数 |
| track | `titl`/`crea`/`albm`(track url は v1 では常に空 — 対応する PCP atom を受信しない) | 各 0..256 バイト |
| tracker | PCP_HOST(グローバル IP:port) | IP:port 形式。firewalled 時は空 |
| listeners / relays_cnt | `numl`/`numr` | -1(非表示)以上 |
| started_at | 受信時に記録 | |
| persona_id | UI で選択されたペルソナ | 掲載(署名)に使う鍵 |
| session_state | — | `announced → updating ⇄ … → ended` |

**状態遷移**:
`(PCP HELO/OLEH 完了) → announced → [BCST 更新で updating を繰返し] → (playing=false または切断) → ended`
`ended` 遷移時に `status=ended` の最終イベントを発行し、以後この channel_id の掲載を停止する。

### EventStore(署名済みイベントのローカル置換ストア)

リレーが担っていた addressable 置換 `(kind, pubkey, d)` を各ノードで実装する(research R1)。
gossip 受信・自ノード発行の両方のイベントを保持し、SYNC_REQ 応答・再伝搬の供給源となる。

| 項目 | 規則 |
|------|------|
| キー | `(kind, pubkey, d タグ)` |
| 置換 | 同一キーは `created_at` が最大のイベントのみ保持(last-write-wins。同値なら event id 辞書順大)。**既に保持している同一 event id の再受信は格納・再伝搬とも行わない**(DedupCache 期限切れ後のループ再発を防ぐ第二の防壁 — contracts/p2p-gossip.md §伝搬規則 3) |
| 除去 | `expiration` 超過 / `status=ended` / `now - created_at > freshness_window_sec` で供給・表示から即時除外し、以後再伝搬しない(FR-006, research R2)。ただし `status=ended` は即時物理削除せず **tombstone として鮮度切れ・`expiration` 到達まで保持**する — 即時削除すると、ended のみ受信したノードで鮮度窓内の古い live イベントのリプレイが空スロットへ格納され、置換の単調性(ADR-0005 StoreMonotonic — `docs/formal/gossip_propagation.tla` で検証済み)に反するため。物理回収は鮮度切れ/`expiration` 超過時に行う |
| 容量 | `event_store_max`(既定 4,096)。超過時は created_at が古い順に破棄 |
| 供給 | 接続時同期(SYNC_REQ)には live かつ鮮度窓内のイベントのみを返す |

### DedupCache(重複抑制キャッシュ)

| 項目 | 規則 |
|------|------|
| キー | event id(hex 64) |
| 保持 | 直近 10 分(research R16。~20,000 件想定) |
| 用途 | 受信済みイベントの再処理・再伝搬ループの防止(gossip 終端保証の要 — Principle V 判定対象) |

### DiscoveredChannel(発見したチャンネル)

視聴者の一覧の 1 行。EventStore 上の検証済み kind 30311 イベントから構築するビュー。

| フィールド | 由来 | 検証 |
|-----------|------|------|
| author_pubkey | event.pubkey | 署名検証必須(`nostr` クレート)。失敗は破棄+セキュリティログ(FR-005) |
| channel_id | `d` タグ | hex 32 桁。不一致は破棄 |
| (name, genre, description, contact_url, bitrate, content_type, track, tracker, listeners, relays_cnt, started_at) | contracts/nostr-events.md のタグ写像 | 同契約の上限・形式チェック |
| status | `status` タグ | `live` / `ended`。それ以外は破棄 |
| created_at | event.created_at | 未来方向 +300 秒超は破棄(時計ずれ対策) |
| source_peers | 受信ピア集合 | UI 表示・ピア品質判断用 |
| muted | MuteEntry 照合 | 一覧から除外(既定はオープン型 — 除外はミュート時のみ) |

**同一性と更新規則(集約)**: 一覧のキーは `(author_pubkey, channel_id)`(EventStore の置換規則に従う)。
`status=ended` または `now - created_at > freshness_window_sec` で一覧から除去(FR-006)。
**同名チャンネルでも author_pubkey が異なれば別行**として扱う(なりすましは併存表示され、
ペルソナの継続性で判別する — spec Clarifications のペルソナモデル)。

### PeerSession(接続中ピアの状態)

| フィールド | 内容 |
|-----------|------|
| addr / direction | 接続先(または接続元)と `outbound` / `inbound` |
| state | `connecting → hello_sent/received → established → closed`(contracts/p2p-gossip.md の状態機械) |
| negotiated | HELLO で交換したバージョン・機能フラグ・相手の待受ポート・時刻申告 `ts`(いずれも申告値。未検証 — `ts` は時計ずれ自己診断のみに使用) |
| rx_budget | 受信レート・サイズの残余(超過で切断+セキュリティログ — Principle II) |
| last_pong_at | keepalive。タイムアウトで切断し fail_count へ反映 |

## エンティティ関係

```text
Persona 1 ──< AnnouncedChannel(掲載時に使用する鍵)
AnnouncedChannel ──> EventStore(発行イベントを格納し gossip へ)
PeerSession >──> EventStore(受信イベントを検証後に格納・再伝搬)
EventStore ──> DiscoveredChannel(live かつ鮮度窓内のイベントのビュー)
PeerEndpoint ──< PeerSession(接続実績が verified / last_ok_at / fail_count に反映)
MuteEntry ──> DiscoveredChannel.muted(表示制御のみ。データは破棄しない)
```

### SecurityEvent(セキュリティイベントログ)

構造化ログ(tracing)としてファイルへ追記。DB には保存しない。
**カテゴリの一覧は本表を正とし、各契約書のログ名はここに列挙されたものを使う**(FR-007)。

| フィールド | 内容 |
|-----------|------|
| ts / category | 下記カテゴリ一覧のいずれか |
| source | 接続元(loopback アドレス / ピアアドレス) |
| detail | 内部情報(スタックトレース・パス)を含めてはならない (MUST NOT — Principle II) |

**カテゴリ一覧(全量)**:

| カテゴリ | 発生箇所(契約) |
|----------|------------------|
| `pcp_reject` | PCP 入力検証違反(contracts/pcp-announce.md) |
| `p2p_invalid_frame` | フレーム/JSON/順序違反(contracts/p2p-gossip.md 検査 3、状態機械違反) |
| `p2p_oversize` | フレーム長超過(同 検査 1) |
| `p2p_rate_limited` | 受信レート・SYNC 応答量超過(同 検査 2・6) |
| `event_oversize` | イベントサイズ超過(contracts/nostr-events.md 検証 1) |
| `event_invalid_sig` | 署名・id 検証失敗(同 検証 2 — FR-005) |
| `event_invalid_format` | kind/タグ形式・内容範囲違反(同 検証 3・5) |
| `event_time_skew` | 時刻ずれ許容超過(同 検証 4) |
| `event_pow_insufficient` | PoW 難易度不足(同 検証 6、min_pow_bits > 0 時) |
| `pex_rejected` | PEERS 内容違反(contracts/p2p-gossip.md 検査 5) |
| `http_rate_limited` | HTTP レート制限超過(contracts/http-yp.md・local-api.md) |
| `url_warning` | URL 警告判定の発動(FR-012) |

**ログ自体の DoS 耐性**(Constitution §Security Requirements): ログファイルはサイズ上限と
ローテーション(1 ファイル 10MB × 5 世代、超過分は古い順に削除)を持つ。同一
`(category, source)` の高頻度イベントは一定間隔(10 秒)で件数集約して記録し、
攻撃によるログ洪水がディスク枯渇(DoS 経路)とならないようにする。

将来フェーズ(実況コメント)は `DiscoveredChannel` のアドレス
`30311:<author_pubkey>:<channel_id>` を kind 1311 の `a` タグで参照する(FR-011 の互換保証)。

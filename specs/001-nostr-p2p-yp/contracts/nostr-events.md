# Contract: イベントスキーマ(チャンネル掲載・実況 — nostr 援用範囲)

**Status**: v1 対象は kind 30311 のみ。kind 1311 は将来フェーズの予約定義。

**援用範囲(FR-014)**: 本契約が nostr(NIP)を援用する唯一の層である。援用するのは
イベントのデータ構造(直列化・タグ・id 計算・Schnorr 署名)のみで、リレーとの通信は行わない。
イベントの伝送は [p2p-gossip.md](./p2p-gossip.md) の独自プロトコルが担う。

## kind 30311 — チャンネル掲載イベント(NIP-53 Live Streaming Event 形式)

addressable イベント。置換規則 `(30311, pubkey, d)` は各ノードのローカル EventStore で
実装する(last-write-wins — data-model.md 参照。リレーには依存しない)。

### タグ写像(PeerCast チャンネル情報 → タグ)

| タグ | 必須 | 値 | PeerCast 由来 |
|------|------|-----|---------------|
| `d` | MUST | チャンネル GUID(hex 32 桁小文字) | ChannelID |
| `title` | MUST | チャンネル名 | `name` |
| `summary` | MAY | 説明 | `desc` |
| `t` | MAY | ジャンル(小文字化) | `gnre` |
| `status` | MUST | `live` または `ended` | 配信状態 |
| `starts` | MUST | 配信開始 unix 秒(文字列) | 開始時刻 |
| `current_participants` | MAY | 直接視聴者数 | `numl`(負値は省略) |
| `streaming` | MAY | `pcp://<tracker_ip>:<port>/<channel_id>` | PCP_HOST |
| `expiration` | MUST | created_at + 600(NIP-40 形式。判定は受信ノードのローカル時計) | 鮮度管理(research R2) |
| `nonce` | MAY | NIP-13 PoW | min_pow_bits 設定時 |

### PeerCast 固有拡張タグ(本ソフトウェア定義)

| タグ | 値 | 由来 |
|------|-----|------|
| `["peca","bitrate","<kbps>"]` | 数値文字列 | `bitr` |
| `["peca","type","<content type>"]` | 例 `FLV` | `type` |
| `["peca","tip","<ip:port>"]` | トラッカー。firewalled 時は省略 | PCP_HOST |
| `["peca","contact","<url>"]` | コンタクト URL | `url` |
| `["peca","relays","<count>"]` | リレー数 | `numr`(負値は省略) |
| `["peca","track","<title>","<artist>","<album>","<url>"]` | 曲情報(空要素可) | `titl`/`crea`/`albm` |

`content` フィールドは空文字列とする(全情報はタグで表現)。

### 発行規則(掲載側)

- 配信中は `republish_interval_sec`(既定 60 秒)ごと、および PCP で情報変更を受けた時点で再発行
  (発行 = 自ノードの EventStore へ格納し、接続中の全ピアへ `EVENT` で送信 — p2p-gossip.md)
- 配信終了(playing=false / PCP 切断)時に `status=ended` で最終発行
- 署名鍵は当該チャンネルに割り当てたペルソナのもの。**他ペルソナの情報(label 等)をイベントに含めてはならない(FR-013)**

### 受信検証(gossip 経由の受信側)— Principle II: trust nothing from the network

p2p-gossip.md の検査 #4 として、順序どおりに検査し、失敗したら破棄して
セキュリティイベントに記録する(検証失敗イベントは格納も再伝搬もしない):

1. **サイズ**: 直列化イベント全体 ≤ 16KB(超過は `event_oversize`)
2. **署名**: `nostr` クレートによる id・sig 検証(失敗は `event_invalid_sig` — FR-005)
3. **kind/タグ形式**: kind=30311、`d` は hex 32 桁、`status` ∈ {live, ended}、タグ数 ≤ 64、各タグ要素長 ≤ 1024 バイト
4. **時刻**: `created_at` が現在+300 秒を超える未来なら破棄。許容時計スキューは ±300 秒とする(spec Edge Case「時計のずれ」)。鮮度窓 600 秒・再発行周期 60 秒の設計により、±300 秒未満のずれでは鮮度・`expiration` 判定の誤除去は生じない。鮮度・期限の判定はすべて受信者ローカル時計を基準に行う
5. **内容**: 数値タグの範囲(bitrate ≤ 100000 等)、`tip` の ip:port 形式、文字列は制御文字除去
6. **PoW(任意)**: `min_pow_bits` > 0 のとき、NIP-13 のコミット難易度を満たさないイベントは破棄
7. **URL 安全性**: `contact` は表示時に scheme が http/https 以外なら警告フラグ(FR-012)。リンクの自動オープンはしない

## kind 1311 — 実況コメント(将来フェーズ・予約)

- `a` タグで `30311:<author_pubkey>:<channel_guid>` を参照(NIP-53 準拠)
- 投稿ペルソナはチャンネル掲載ペルソナと独立(同一人物が別ペルソナで実況可能 — spec Clarifications)
- v1 では送受信とも実装しない。識別子設計のみ本契約で固定する(FR-011)

## 原則参照

- 検証パイプライン: Principle II(入力検証・trust nothing)
- 署名必須・検証失敗不可視: Principle I / FR-005
- PoW・ミュートとの併用: FR-008(オープン型既定を崩さない)

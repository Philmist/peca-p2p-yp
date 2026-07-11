# Contract: 実況スレのイベントスキーマ(kind 1311 / 21311 / 31311)

**Feature**: `006-livechat-thread` | **参照**: [data-model.md](../data-model.md), research R1/R2

**援用範囲(001 FR-014 の踏襲)**: nostr を援用するのはイベントのデータ構造
(直列化・タグ・id 計算・Schnorr 署名)のみ。kind 1311 は NIP-53 の予約定義
(001 contracts/nostr-events.md)の履行、kind 21311/31311 は **peca 固有 kind**
(NIP 未割当領域)であり NIP 互換を主張しない。伝送は
[thread-delivery.md](./thread-delivery.md)(レス・順序確定)と 001 p2p-gossip.md
(announce)が担う。

## kind 31311 — スレ announce(addressable・gossip で伝搬)

置換規則 `(31311, pubkey, d)`。板 = ペルソナ単位でアクティブスレ 1 本のため、
ペルソナあたり常に最新 1 件に置換される。

| タグ | 必須 | 値 |
|------|------|-----|
| `d` | MUST | 固定文字列 `"livechat"` |
| `a` | MUST | `30311:<pubkey>:<channel_guid>`(対象チャンネル。`<pubkey>` は announce 署名者と一致 — FR-003) |
| `title` | MUST | スレタイトル(≤ 128 文字) |
| `gen` | MUST | スレ世代(u32 の 10 進文字列) |
| `key` | MUST | スレ作成 unix 秒(互換 API の dat キー) |
| `res_count` | MAY | 現在の確定レス数(一覧表示用・未検証の参考値) |
| `tip` | MUST | ホスト接続先 `ip:port`。**受信のみでは接続しない**(FR-004) |
| `expiration` | MUST | created_at + 600(30311 と同一規則 — FR-002) |
| `nonce` | MAY | NIP-13(`min_pow_bits` 設定時) |

`content` は空文字列。

### 受信検証(gossip 検査 #4 への追加分岐)

001 nostr-events.md の検査 1〜6(サイズ・署名・形式・時刻・内容・PoW)を共通で通した上で:

7. **ペルソナ一致**: `a` タグの `<pubkey>` が announce の署名者と一致しない場合、
   不可視(保持・再伝搬しない)+ `livechat_announce_invalid`(FR-003)
8. **対象実在(緩和)**: 参照先 30311 が EventStore に未着でも破棄しない(到着順は保証
   されない)。ただし一覧表示は対応する live チャンネルが見えている場合のみ行う

**gossip の許可 kind 集合**: gossip 検査 #3(001 nostr-events.md)の許可 kind は
`{30311, 31311}` に拡張する。**kind 1311 / 21311 が gossip 経由で到着した場合は
破棄し(格納・再伝搬しない)、`event_invalid_format` として記録する**(受信側規範 —
「流してはならない (MUST NOT)」の送信側規範と対)。逆にスレ配送セッション内で
`RES` に 1311 以外・`ORDER` に 21311 以外の kind が載っていた場合は不正フレームとして
切断する(thread-delivery.md)。

## kind 1311 — レス(スレ配送セッションのみ。gossip に流してはならない (MUST NOT))

| フィールド/タグ | 必須 | 値 |
|----------------|------|-----|
| `content` | MUST | 本文(≤ 2048 文字・≤ 32 行 — 単位は文字数で SETTING.TXT の提示と一致。アンカー `>>n` は本文の一部であり表現層が解釈) |
| 署名鍵 | MUST | 板鍵(FR-016)。ペルソナ鍵で署名されたレスも技術的には検証を通るが、クライアントは常に板鍵で署名する |
| `a` | MUST | 対象チャンネル(31311 の `a` と同値) |
| `["peca","thread","<board_id>","<gen>"]` | MUST | 対象スレ |
| `["peca","name","<名前>"]` | MAY | ≤ 64 文字。**`#` 以降は送信前に除去済みであること**(FR-024)。省略・空 = 名無し |
| `["peca","mail","<メール>"]` | MAY | ≤ 64 文字。表示互換のみ(FR-029) |
| `nonce` | 条件付き MUST | NIP-13。ホストにとって初見の板鍵は `first_post_pow_bits` を満たすこと(research R6) |

### ホスト側受信検証(採番前 — FR-007。順序どおり、失敗は破棄 + `livechat_write_rejected`)

1. **サイズ**: 直列化イベント全体 ≤ 16KB(既存共通上限)
2. **署名**: nostr クレートによる id・sig 検証
3. **形式**: kind=1311、必須タグ、name/mail/body の長さ・行数、制御文字、
   name に `#` が残っていれば**ホスト側でも除去**(二重防御)
4. **スレ状態**: 対象スレが自板の Active スレであること(不変条件 T1/T2)
5. **BAN**: 板鍵 BAN・接続 BAN に該当しないこと(該当時は記録するが応答で理由を開示しない)
6. **PoW**: 初見板鍵は `first_post_pow_bits`、既知は通常しきい値
7. **レート**: `thread_write_rate`(板鍵単位)・`thread_msg_rate`(接続単位)

## kind 21311 — 順序確定情報(スレ配送セッションのみ。gossip に流してはならない (MUST NOT))

| フィールド/タグ | 必須 | 値 |
|----------------|------|-----|
| 署名鍵 | MUST | スレ主ペルソナ(不一致は破棄 + `livechat_order_invalid` — FR-011) |
| `["peca","thread","<board_id>","<gen>"]` | MUST | 対象スレ |
| `["peca","seq","<u32>"]` | MUST | 確定情報の連番(欠落検出用 — 不変条件 O2) |
| `["peca","order","<res_no>","<event_id>"]`(複数可) | MUST | 今回確定した採番。res_no は確定済みの続きから欠番なし(不変条件 T3) |

`content` は空文字列。参加者側検証: サイズ → 署名 → スレ主一致 → seq 連続性 →
res_no 連続性。seq の欠落は表示を進めず再送要求(thread-delivery.md `RESEND_REQ`)。

## NIP 適合・逸脱・前方互換

### kind 1311「予約定義の履行」の採否列挙(NIP-53 との対応)

| NIP-53 の規範的要素 | 採否 |
|--------------------|------|
| `a` タグで対象 30311 を参照 | **採用**(001 予約定義と同一) |
| `content` = チャット本文(平文) | **採用** |
| 投稿鍵は掲載ペルソナと独立 | **採用**(板鍵 — FR-016) |
| リレーへの発行・購読 | **不採用**(伝送は thread-delivery.md — FR-014) |
| zap・リアクション・emoji 等の周辺タグ | **不採用**(YAGNI) |
| regular kind(1000–9999)の「リレーに保存される」意味論 | **意図的逸脱**: 本ソフトはリレー非接続かつスレデータは揮発(FR-015)。保存意味論はローカルに閉じる |

### 借用タグの NIP 出典

| タグ | 出典 | 適合メモ |
|------|------|----------|
| `d` | NIP-01(addressable の置換キー) | 31311 で使用。固定文字列 `"livechat"` |
| `a` | NIP-01(イベント参照)/ NIP-53(30311 参照の用法) | 準拠 |
| `title` | NIP-53 | 準拠(≤ 128 文字は peca 制約) |
| `expiration` | NIP-40 | 準拠(判定は受信ノードのローカル時計 — 001 と同一) |
| `nonce` | NIP-13 | 準拠(コミット難易度の検証も NIP-13) |
| `["peca",...]` | peca 固有(001 で確立した名前空間) | NIP 外 |

### kind レンジ意味論と衝突時の対応

- 31311 = addressable(30000–39999、置換規則)・21311 = ephemeral(20000–29999、
  非保存)のレンジ意味論は NIP-01 と一致する用法で使う。両 kind とも **peca 固有であり
  NIP 互換を主張しない**(レジストリ確認記録は research R2)
- **将来 NIP が 21311 / 31311 を割り当てた場合**: リレー通信をしないため相互運用上の
  実害はない(受容)。ただし割当が実況・チャット近接領域で誤解釈のリスクを持つ場合は、
  プロトコルの次バージョン(HELLO feature `livechat2`)で改番しなければならない (MUST)。
  判断は実装フェーズ開始時のレジストリ再確認(research R2)とあわせて行う
- **リレー流出の受容**: peca イベントは有効な nostr イベントであり、第三者が公開リレーへ
  再公開しうる。31311 の `tip`(ip:port)は 30311 の `["peca","tip"]` と同じく元々公開の
  掲載情報であり追加露出はない。未対応 kind は一般の NIP-53 クライアントでは表示されない
  (受容として記録)

### 前方互換(未知タグの無視)

未知のタグ、および `["peca",...]` の未知サブタグは無視しなければならない (MUST)
(001 の HELLO `features` / タグ規則と同一の前方互換規則。livechat 3 kind すべてに適用)。

## 原則参照

- 検証パイプライン共有・多段検証: Principle II / FR-007
- ペルソナ一致(偽スレ・反射攻撃対策): Principle I / FR-003
- 順序確定の署名検証: Principle I / FR-011
- PoW 再利用(自前暗号なし): Principle II / research R6

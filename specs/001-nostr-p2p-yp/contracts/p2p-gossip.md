# Contract: P2P gossip ワイヤプロトコル(ノード間)

**Role**: 利用者ノード同士が署名済みイベント(contracts/nostr-events.md)を直接交換する
自前プロトコル。リレーサーバーは存在しない(FR-002, FR-014)。
**本契約はインターネットに露出する最大の信頼境界**であり、すべての受信データは
多段検証を通す(Principle II — trust nothing from the network)。

**設計判断の記録**: research R13(方式選定)、R14(PEX)、R16(規模導出)。
伝搬・重複抑制・同期の状態機械は Principle V の形式的検証候補(判定 ADR は tasks 先頭フェーズ)。

## トランスポート

- TCP。既定待受 `0.0.0.0:7147`(設定変更・無効化可 — 無効時は外向き接続のみ、FR-016)
- フレーム = `長さ(4 バイト BE、ペイロードのバイト数)` + `ペイロード(JSON、UTF-8)`
- トランスポート暗号化なし(完全性・真正性はイベント署名で担保。掲載情報は公開データ。
  判断は ADR に記録 — plan.md Constitution Check)
- keepalive: 60 秒間隔で `PING`、120 秒無応答で切断

## メッセージ種別

ペイロードは `{"type":"<TYPE>", ...}` の JSON オブジェクト。

| type | 方向 | フィールド | 意味 |
|------|------|-----------|------|
| `HELLO` | 発→受 | `version`(u32、v1=1)、`listen_port`(u16、待受なしは 0)、`features`(文字列配列) | 接続開始。最初のフレームでなければ切断 |
| `HELLO_ACK` | 受→発 | 同上 | 受理。バージョン非互換は `CLOSE` して切断 |
| `EVENT` | 双方向 | `event`(nostr イベント JSON) | イベントの伝搬(発行・再伝搬とも同形) |
| `SYNC_REQ` | 双方向 | `since`(unix 秒) | 接続時同期の要求。相手の EventStore の live かつ鮮度窓内イベントを求める |
| `SYNC_DONE` | 双方向 | `count`(u32) | SYNC_REQ への応答完了(応答本体は `EVENT` の列) |
| `GET_PEERS` | 双方向 | — | ピア交換の要求(FR-015) |
| `PEERS` | 双方向 | `peers`(`"host:port"` 文字列配列、≤ 64 件) | **検証済み(自ノードが接続成功した)ピアのみ**を返す(research R14) |
| `PING` / `PONG` | 双方向 | `nonce`(u64) | keepalive |
| `CLOSE` | 双方向 | `reason`(定型コード) | 正常切断。内部情報を含めてはならない (MUST NOT) |

## セッション状態機械

```text
[outbound] connect → HELLO 送信 → HELLO_ACK 受信 → established
[inbound]  accept  → HELLO 受信 → HELLO_ACK 送信 → established
established → (SYNC_REQ/EVENT/GET_PEERS/PING の交換) → CLOSE または異常切断 → closed
```

- established 前に HELLO/HELLO_ACK 以外を受信したら即切断(`p2p_invalid_frame`)
- established 直後に双方が `SYNC_REQ`(since = now − freshness_window_sec)を送るのが標準フロー
- 切断はいつ発生してもよい(US3: ピア障害時の継続性)。再接続は指数バックオフ

## 伝搬規則(gossip)

1. **受信** `EVENT` → 検証パイプライン(下記)を通す
2. **重複判定**: event id が DedupCache にあれば黙って破棄(再伝搬しない)
3. **格納**: EventStore の置換規則 `(kind, pubkey, d)` + last-write-wins で格納。
   旧版しか置換できない(より古い)イベントは格納も再伝搬もしない
4. **再伝搬**: 格納に成功したイベントのみ、**受信元を除く** established な全ピアへ `EVENT` を送信
5. **終端保証**: ホップ制限は設けない。重複抑制(2)と鮮度期限(FR-006 / research R2)が
   伝搬の終端を保証する(Principle V 判定対象の中核性質: ループ不在・重複爆発不在・
   live イベントの到達性)

## 受信検証パイプライン(Principle II)

順序どおりに検査し、失敗したら破棄してセキュリティイベントに記録する。
閾値超過は接続の切断と fail_count 反映を伴う:

| # | 検査 | 上限/規則 | 違反時ログ |
|---|------|-----------|-----------|
| 1 | フレーム長 | ≤ 64KB(EVENT を含む全メッセージ) | `p2p_oversize` |
| 2 | 受信レート | ≤ 256KB/秒・≤ 200 メッセージ/秒(1 ピアあたり) | `p2p_rate_limited` |
| 3 | JSON 形式 | パース可能、`type` が既知、未知フィールドは無視 | `p2p_invalid_frame` |
| 4 | EVENT 内容 | contracts/nostr-events.md の受信検証(サイズ→署名→形式→時刻→内容→PoW) | `event_*` |
| 5 | PEERS 内容 | 件数 ≤ 64、各要素は `host:port` 形式・長さ ≤ 256、自アドレス・重複は破棄 | `pex_rejected` |
| 6 | SYNC 応答量 | SYNC_REQ 1 回への応答 EVENT は ≤ event_store_max 件。超過は切断 | `p2p_rate_limited` |

- 未検証ピア(接続実績なし)を `PEERS` で再共有してはならない (MUST NOT — research R14)
- エラー・CLOSE の reason に内部情報(パス・スタックトレース)を含めてはならない (MUST NOT)

## 接続管理

- 外向き: `p2p_outbound_target`(既定 8)本を維持。候補は PeerEndpoint から
  `manual 優先 → last_ok_at 新しい順 → fail_count 少ない順`
- 着信: `p2p_inbound_max`(既定 32)本まで。超過は HELLO_ACK 前に CLOSE
- 同一アドレスへの多重接続は 1 本に統合。自己接続(自分の待受への接続)は HELLO の
  nonce 照合で検出し切断
- UPnP(research R15)は待受可否にのみ影響し、本契約のメッセージには現れない

## 検証方法

- `tests/contract/`: フレーム境界(分割・結合・過大長)、HELLO 順序違反、不正 JSON、
  PEERS 上限超過、SYNC 応答上限のフィクスチャ検証
- 統合テスト: インプロセスのモックピアで 3〜10 ノードのトポロジを構成し、
  伝搬(SC-001)・重複抑制(ループ不在)・接続時同期・ピア停止時の継続(US3)・
  PEX 拡大(FR-015)を検証
- Principle V 判定 ADR で該当となった場合: `docs/formal/` の PlusCal モデルで
  ループ不在・到達性・置換の単調性を検査してから実装する

## 原則参照

- 多段検証・レート制限: Principle II / FR-007 / Security Requirements
- 未検証ピア再共有禁止・自己検証: FR-015(「真に信頼できるのは自分だけ」)
- リレー非依存・単一障害点排除: FR-002 / FR-014
- 形式的検証候補: Principle V(plan.md Constitution Check)

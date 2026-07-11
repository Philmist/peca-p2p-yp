# Contract: スレ配送ワイヤプロトコル(ホスト直結星型)

**Feature**: `006-livechat-thread` | **参照**: [thread-events.md](./thread-events.md), research R4/R9

## トランスポート

- 既存 P2P 待受(既定 7147)と同一ポート・同一フレーミング(4 バイト BE 長さ前置 +
  JSON ペイロード、フレーム長 ≤ 64KB)を共有する(research R4)
- HELLO の `features` に `"livechat1"` を含める(001 の「未知 feature は無視 (MUST)」
  規則により旧ノードと前方互換)
- **1 TCP 接続 = 1 用途**: established 後の最初のメッセージが `THREAD_JOIN` なら
  スレセッション、それ以外は gossip セッション。以後の種別混在は不正フレームとして切断
- トランスポート暗号化なし(完全性はイベント署名で担保 — ADR-0006 と同一判断)
- keepalive: gossip と同じ `PING`/`PONG`(60 秒間隔・120 秒無応答切断)

## メッセージ種別

ペイロードは `{"type":"<TYPE>", ...}` の JSON オブジェクト。

| type | 方向 | フィールド | 意味 |
|------|------|-----------|------|
| `THREAD_JOIN` | 参→ホ | `thread`(`<board_id>:<gen>`)、`challenge`(参加者生成の 32 バイト乱数 hex)、`since_seq`(u32、初回は 0) | 参加要求 + チャレンジ提示 |
| `THREAD_WELCOME` | ホ→参 | `thread`、`sig`(スレ主ペルソナ鍵による `challenge \|\| board_id \|\| gen` への Schnorr 署名)、`board_settings`(板設定 JSON)、`res_count` | 受理 + アドレス真正性の証明(FR-005) |
| `THREAD_REJECT` | ホ→参 | `reason`(定型コード: `full` / `frozen` / `closed` / `unknown_thread` / `rate`) | 定型拒否。内部情報を含めてはならない (MUST NOT)(FR-006) |
| `RES` | 双方向 | `event`(kind 1311 JSON) | 参→ホ: 書き込み。ホ→参: 確定レス本文の配布 |
| `ORDER` | ホ→参 | `event`(kind 21311 JSON) | 順序確定情報の配布 |
| `SETTINGS` | ホ→参 | `board_settings` | 板設定の即時配布(FR-023) |
| `RESEND_REQ` | 参→ホ | `from_seq`, `to_seq` | 欠落した確定情報・対応レスの再送要求 |
| `THREAD_CLOSE` | ホ→参 | `event`(スレ主署名付きクローズ通知 = kind 21311 の `["peca","close"]` タグ付き特殊形) | 明示クローズ → 受信側はスレデータ削除(FR-14) |
| `NEXT_THREAD` | ホ→参 | `gen`(新世代)、`key`(新スレ作成秒) | 次スレ移行通知。旧 gen は書き込み不可(FR-013) |
| `PING` / `PONG` | 双方向 | `nonce`(u64) | keepalive(gossip と共通) |
| `CLOSE` | 双方向 | `reason`(定型コード) | セッション終了 |

## セッション状態機械

```text
[参加者] connect → HELLO/HELLO_ACK → THREAD_JOIN 送信
          → THREAD_WELCOME 受信 + sig 検証 OK → joined(同期開始)
          → THREAD_REJECT または sig 検証 NG → 切断(NG は livechat_challenge_failed + バックオフ)
[joined]  ホストからの RES/ORDER/SETTINGS/NEXT_THREAD/THREAD_CLOSE を受信
          参加者は RES(書き込み)・RESEND_REQ を送信
[ホスト]  accept → HELLO 受信(features に livechat1)→ HELLO_ACK
          → THREAD_JOIN 受信 → 参加上限・スレ状態を確認 → WELCOME or REJECT
```

- **チャレンジ検証(FR-005)**: 参加者は `THREAD_WELCOME.sig` を announce に記載された
  スレ主ペルソナ公開鍵で検証する。失敗は切断 + `livechat_challenge_failed` +
  指数バックオフ(初期 5 秒、係数 2、上限 300 秒 — gossip 再接続と同一パラメータ)
- **接続時同期(FR-010)**: joined 直後、ホストは `since_seq` 以降の全確定レス(`RES`)と
  全順序確定情報(`ORDER`)を seq 順に送る。参加者は同期完了前でも受信順に表示してよい
  (seq 連続性が保たれる限り)
- **書き込み(FR-007)**: 参加者の `RES` はホストのみが受理する。ホストは
  thread-events.md の受信検証(1〜7)を通過したレスに採番し、`RES` + `ORDER` を
  全接続参加者(送信者含む)へ配布する
- **次スレ移行(FR-013)**: res_no = res_limit の確定後、ホストは `NEXT_THREAD` を配布し
  新世代を開始する。移行完了までに届いた書き込みは `THREAD_REJECT(rate)` ではなく
  新スレへ採番するか、定型拒否する(移行境界の競合 — PlusCal モデルの検査対象、research R9)
- **凍結(FR-014)**: 参加者はホスト切断(TCP 断・PING 無応答)で当該スレを Frozen とし、
  取得済みレスの閲覧は継続。再接続はバックオフ付きで試行し、同一 gen が継続していれば
  `since_seq` から差分同期して Active に復帰する

## 防御(FR-021・Principle II)

- 参加上限: `thread_max_participants`(既定 128)。超過は `THREAD_REJECT(full)`
- レート: `thread_msg_rate`(接続単位・制御込み)、`thread_write_rate`(板鍵単位)。
  違反は破棄 + `livechat_write_rejected`、継続する場合は切断
- サイズ: フレーム長 ≤ 64KB・イベント ≤ 16KB(既存共通上限)
- ホストは THREAD_JOIN 前のスレメッセージ・joined 前の RES を不正フレームとして切断
- `RES` に kind 1311 以外・`ORDER` に kind 21311 以外のイベントが載っていた場合は
  不正フレームとして切断(kind とメッセージ種別の対応は 1:1 — thread-events.md)
- 接続 BAN(`ConnBan`)対象からの接続は HELLO 後に `CLOSE` で切断(理由は開示しない)

## 検証方法

- 契約テスト: モックピア(`tests/common/mock_peer.rs` 拡張)で JOIN/WELCOME/REJECT・
  チャレンジ失敗・偽 ORDER・レート違反・移行境界のネガティブケースを固定フィクスチャで検証
- 統合テスト: 実 `P2pRuntime` 多ノード(ホスト + 参加者 2 以上)で SC-001/SC-002 を計測

## 原則参照

- チャレンジによるアドレス真正性・バックオフ: Principle I / II / FR-005
- 定型拒否・内部情報非開示: Security Requirements(エラーハンドリング)/ FR-006
- 星型集約と採番の単点性: Principle V 検査対象(research R9 / ADR-0014)

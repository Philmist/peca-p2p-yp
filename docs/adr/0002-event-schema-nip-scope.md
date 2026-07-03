# ADR-0002: イベントスキーマと NIP 援用範囲

**Status**: Accepted
**Date**: 2026-07-03
**Principles**: Principle II (Security by Design), Principle VI (Principle Traceability)
**Task**: T005(Phase 2 実装前ゲート)

## 背景

spec FR-014 は nostr の援用をイベント形式・署名等のデータスキーマ(NIP のデータ構造)に
限定することを MUST と定める(spec Clarifications 2026-07-03: リレーサーバーの存在自体が
単一障害点であるため排除する)。本 ADR は、チャンネル掲載イベントをどの NIP スキーマで
表現するか(research R1/R2)、および援用の境界をコード上どこで強制するかを、実装開始前の
確定事項として記録する。

## 決定

### 1. イベントスキーマ: NIP-53 kind 30311(Live Streaming Event、addressable)

- `d` タグ = チャンネル GUID(hex 32 桁**小文字**)。タグ写像・拡張タグ(`peca`)・発行規則・
  受信検証の正は [contracts/nostr-events.md](../../specs/001-nostr-p2p-yp/contracts/nostr-events.md) とする
- addressable 置換規則 `(kind, pubkey, d)` は各ノードのローカル EventStore で実装する
  (last-write-wins、同値なら event id 辞書順大 — リレーに依存しない)
- 実況コメント(将来フェーズ)は kind 1311 を予約定義とし、`a` タグ
  `30311:<pubkey>:<channel_id>` からの**無変更参照**で識別体系の互換を保証する(FR-011)

### 2. 鮮度管理(research R2)

- 掲載側: 配信中 60 秒(`republish_interval_sec`)ごとに再発行し、NIP-40 `expiration`
  (created_at + 600)を付与。配信終了時は `status=ended` で最終発行
- 受信側: `status=live` かつ `created_at` が鮮度窓(`freshness_window_sec` = 600 秒)内の
  イベントのみを「配信中」と扱い、期限切れは削除し**再伝搬しない**
- 鮮度・期限の判定はすべて受信ノードのローカル時計基準(「真に信頼できるのは自分だけ」)。
  時刻関連定数の単一出典は data-model §Settings

### 3. 援用境界(FR-014 の強制方法)

- 援用するのは**イベントのデータ構造のみ**: 直列化(JSON)・タグ・event id 計算・
  secp256k1 Schnorr 署名(NIP-01/13/19/40/53 のデータ定義)
- 依存クレートは rust-nostr の **`nostr`(プロトコル/データ構造層)のみ**とし、
  リレークライアント機能を持つ `nostr-sdk` を依存に含めてはならない (MUST NOT — research R3)
- モジュール境界で強制する: nostr の型・API を使用するのは `src/event/`(スキーマ・署名検証・
  置換ストア)のみ。`src/p2p/`(伝送)は署名済みイベント JSON をオペークなペイロードとして
  運び、nostr クレートに依存しない。旧構成の `nostr/`(リレー通信込み)モジュールは存在しない
- リレー関連機能(リレープール・NIP-65・購読)を実装してはならない (MUST NOT)。
  イベントは標準準拠のため、将来のリレーブリッジ(スコープ外)は原理的に可能なまま残る

## トラッカー解決の充足方式(検証可能な仮定 — plan §Summary)

FR-004 のトラッカー解決は、現行 YP と同方式で index.txt の TIP フィールド
(トラッカー ip:port)を介した視聴クライアントの直接続で充足する。
PCP によるホストルックアップ(tracker lookup)は v1 の対象外とする
(contracts/pcp-announce.md §明示的な非対応)。

本方式は次の**検証可能な仮定**に依存するため、判断の記録項目を固定する:

1. **仮定**: 無改造の PeerCast クライアント(必須検証対象は PeerCastStation 現行安定版 —
   SC-003)が、TIP(トラッカー直接続)のみで視聴開始できる
2. **実機検証結果**: **未実施**。quickstart 手順 4(TIP 経由視聴開始)を T058 で実機検証し、
   結果を本節に追記して本 ADR を更新する
3. **不成立時の代替**: contracts/pcp-announce.md の明示的非対応を解除し、tracker lookup
   (`GET /channel/<id>` + HTTP 503 + PCP_HOST 応答)を追加実装する。この場合も
   本 ADR の援用境界(§3)は影響を受けない(lookup は PCP 層で完結する)

なお firewalled(TIP 空)チャンネルは v1 では直接視聴不可であり、UI で
「直接視聴不可(トラッカー未公開)」を明示する(contracts/local-api.md UI 要件)。

## 否定した選択肢

- **独自イベント形式の新規定義** — 署名・直列化・検証の設計をゼロから行うことになり、
  「実績ある仕様の援用」という依頼趣旨に反する。スキーマ再発明はバグ=脆弱性の温床(Principle II)
- **kind 1(テキストノート)への埋め込み** — 置換規則がなく古い情報が堆積し、構造化も貧弱
- **`nostr-sdk` の採用** — リレープール等の不要な依存とネットワーク経路を持ち込み、
  FR-014 の境界を曖昧にする(モジュール境界での強制が効かなくなる)
- **v1 での tracker lookup 同時実装** — TIP 経由で充足する仮定が成立する限り不要(YAGNI)。
  仮定の検証(上記)を先に行う

## 原則参照

- Principle II: 署名・検証を実績あるライブラリ(`nostr` クレート)に委ね、自前暗号を排除
- Principle VI: FR-011/FR-014 の判定基準・境界をコード構造(モジュール分離・依存制限)に対応付け
- FR-004/FR-006/FR-011/FR-014、research R1/R2/R3、spec Clarifications 2026-07-03

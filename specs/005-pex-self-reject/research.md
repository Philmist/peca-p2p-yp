# Phase 0 Research: PEX 自己アドレス拒否の良性化

spec に未解決の [NEEDS CLARIFICATION] は無い(良性化対象=自己アドレス+重複で確定済み)。
本フェーズは実装方針の選択肢を整理する。

## R1. 破棄理由の分類をどこで表現するか

**Decision**: `validate_incoming_peers` の戻り値 `IncomingPex` に、破棄を「良性(自己アドレス・重複)」と
「不審(件数超過・parse 失敗・ホスト名・長さ超過)」へ分類して保持する。呼び出し側はこの分類を見て
セキュリティイベント記録の要否を判断する。

**Rationale**:
- 破棄理由を知っているのは検証関数だけ。呼び出し側で再判定するのは二重ロジックで乖離の温床。
- `IncomingPex` は既に「採用/破棄」を返す構造で、分類の追加は自然な拡張。
- 純粋関数のまま単体テストしやすい(Principle IV のネガティブテストを関数レベルで固定できる)。

**Alternatives considered**:
- (a) 呼び出し側で `is_self` を再チェックして分岐 → 検証関数と重複、count>64 の全破棄と混在時の判定が煩雑。却下。
- (b) `rejected: Vec<(String, RejectReason)>` に理由列挙を持たせる → 表現力は高いが、記録要否には
  「良性か不審か」の二値で十分。過剰設計。ただし将来の可観測性向上余地として research に記録。
- **採用**: `IncomingPex` に `benign_rejected: Vec<String>` と `suspicious_rejected: Vec<String>` の
  2 リスト(または `rejected` + `has_suspicious()` 判定)を持たせる最小案。

## R2. セキュリティイベント記録の発火条件

**Decision**: 受信 `PEERS` の破棄に**不審が 1 件以上**あるときのみ `pex_rejected` を記録する。
良性のみ(または破棄ゼロ)のときは記録しない。良性破棄があった場合は debug ログを 1 行出す。

**Rationale**: FR-001/FR-003 の要求そのもの。混在時に不審を見逃さない(Safety First)。

**Alternatives considered**:
- 良性・不審で別カテゴリのセキュリティイベントを作る → data-model に新カテゴリ追加が必要で過剰。
  良性は「イベントではない」が正しい切り分け。却下。

## R3. count > 64(件数超過)の分類

**Decision**: 件数超過は**不審**に分類する(バッチ全体破棄 → `pex_rejected` 記録は従来どおり)。

**Rationale**: 正当なピアは 64 件以下しか送らない(検査5)。超過は protocol 逸脱で攻撃兆候になりうる。
良性化の対象外。既存の「全破棄」挙動・記録を維持する。

## R4. debug ログの内容と情報漏洩

**Decision**: debug ログには接続元 `source`・良性破棄件数(必要なら理由内訳)を出す。自ノードアドレスを
出す場合も自分自身のものに限定する。既定 INFO では出力されず、`RUST_LOG` 有効化時のみ観測できる。

**Rationale**: Principle II(内部情報を漏洩しない)。自ノードアドレスは秘匿情報ではないが、
セキュリティイベントに載せない方針は維持し、可観測性は debug に隔離する。

## R5. Formal Verification(Principle V)の要否

**Decision**: 対象外。ADR-0013 に非クリティアルの理由を明記する。

**Rationale**: 新規の並行アルゴリズム/プロトコル状態機械ではなく、既存の同期的検証結果に対する
ログ分類の分岐追加にすぎない。競合状態・デッドロックの新規リスクを持ち込まない。Principle V の
クリティカル 3 基準(新規設計・非自明な並行性・安全/整合性への直接影響)をいずれも満たさない。

## R6. ドキュメント整合の範囲

**Decision**: (1) contracts/p2p-gossip.md 検査5 の「違反時ログ」欄を精緻化(良性=記録なし・不審=記録)、
(2) data-model.md の `pex_rejected` 行を記録条件付きへ更新、(3) ADR-0013 を新規作成、
(4) CONTEXT.md に SecurityEvent 記述の乖離があれば追随。

**Rationale**: Principle VI(追跡可能性)と単一コンテキスト方針(CLAUDE.md)。コードとドキュメントの
乖離を残さない(FR-005)。

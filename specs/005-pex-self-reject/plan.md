# Implementation Plan: PEX 自己アドレス拒否の良性化

**Branch**: `005-pex-self-reject` | **Date**: 2026-07-08 | **Spec**: [spec.md](./spec.md)

**Input**: Feature specification from `/specs/005-pex-self-reject/spec.md`

## Summary

受信 `PEERS` 検証(contracts/p2p-gossip.md 検査5)の破棄を、**良性(自己アドレス・重複)**と
**不審(件数超過・形式不正・長さ超過・ホスト名)**に分類する。破棄そのもの(候補に登録しない防御)は
維持したまま、`pex_rejected` セキュリティイベントは**不審な破棄が 1 件以上あるときのみ**記録し、
良性のみの破棄は debug ログへ格下げする。あわせて契約・data-model・ADR を整合させる。

技術方針: `validate_incoming_peers` の戻り値 `IncomingPex` に破棄理由の分類を持たせ、
呼び出し側(`runtime.rs` の `Message::Peers`)が分類に応じてセキュリティイベント記録 or debug ログを
選ぶ。既存の破棄判定ロジック・候補登録・PEX 選定規則は不変(ログ分類のみの変更)。

## Technical Context

**Language/Version**: Rust (edition 2024)

**Primary Dependencies**: tokio(非同期ランタイム), tracing(構造化ログ), rusqlite(ピアストア。本機能では不使用)

**Storage**: SQLite(`app.db` の `peers` テーブル)。本機能はストアを変更しない

**Testing**: `cargo test`(unit), cucumber(Gherkin BDD — `tests/features/*.feature` + `tests/steps/*.rs`), 契約テスト(`tests/contract/pex.rs`)

**Target Platform**: Linux(systemd)/ Windows。本変更はプラットフォーム非依存

**Project Type**: 単一プロジェクト(P2P ノードデーモン)

**Performance Goals**: 変更なし。ログ分類は接続ごとの受信 `PEERS` 処理内の O(拒否件数)追加判定のみ

**Constraints**: 内部情報を漏洩しない(Principle II)。debug ログに載せる自ノードアドレスは自ノード自身のもの限定

**Scale/Scope**: コード変更は `src/p2p/pex.rs`・`src/p2p/runtime.rs` の局所。ドキュメントは契約 1・data-model 1・ADR 1・Gherkin 1

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

| 原則 | 評価 | 根拠 |
|------|------|------|
| I. Safety First | PASS(条件付き) | 良性化は**不審な破棄の記録を一切減らさない**(US2/FR-003 が担保)。混在時は記録。防御(破棄)は不変(FR-004)。偽陽性除去でセキュリティ監視の実効性は**向上**する |
| II. Security by Design | PASS | 入力検証・破棄判定は不変。debug ログの自ノードアドレスは自分自身のみで情報漏洩なし(FR-007)。Security Requirements「不正リクエストの記録」は維持(良性反射は"不正"ではない) |
| III. Code Quality & Review | PASS | 局所変更・分類意図をコメント。`cargo fmt --check` / clippy / レビュー。分類ロジックに意図コメント(MUST) |
| IV. BDD with Gherkin | PASS | FR-006 で良性(自己のみ/重複のみ)・不審・混在のネガティブシナリオを Gherkin 化し先に失敗確認 |
| V. Formal Verification | 対象外(要 ADR 明記) | 新規の並行アルゴリズム・プロトコル状態機械ではなく、既存検証結果のログ分類のみ。非クリティカルの理由を ADR-0013 に明記(MUST) |
| VI. Principle Traceability | PASS | ADR-0013 に原則参照付きで記録。契約・data-model と同時更新(FR-005) |

**Gate 判定**: 違反なし。ADR-0013 に (a) 良性/不審の切り分け根拠、(b) Principle V 非クリティカル判断、
(c) Security Requirements との整合を記載することを条件に通過。

**Post-Design 再評価(Phase 1 後)**: research.md / data-model.md / contracts の設計を経ても新たな
違反は生じない。分類は純粋関数 `validate_incoming_peers` 内に閉じ、`runtime.rs` は記録要否の分岐のみ
追加(新規並行処理なし → Principle V 対象外の判断は不変)。防御(破棄)の不変性(FR-004)は
data-model の不変条件と contracts C3 で二重に固定。ゲート通過を維持。

## Project Structure

### Documentation (this feature)

```text
specs/005-pex-self-reject/
├── plan.md              # This file
├── research.md          # Phase 0 output
├── data-model.md        # Phase 1 output(SecurityEvent 意味変更・IncomingPex 分類)
├── quickstart.md        # Phase 1 output(検証手順)
├── contracts/
│   └── pex-rejection-classification.md  # 破棄分類とイベント記録規則の契約
└── checklists/
    └── requirements.md  # 作成済み(spec 品質)
```

### Source Code (repository root)

```text
src/p2p/
├── pex.rs        # validate_incoming_peers / IncomingPex に破棄分類を追加(主変更)
└── runtime.rs    # Message::Peers: 分類に応じて security.log or debug! を選択(主変更)

tests/
├── contract/pex.rs          # 分類のネガティブ契約テスト(良性/不審/混在)を追加
├── features/security.feature # Gherkin: 良性のみ→無記録 / 不審→記録 / 混在→記録
└── steps/security.rs         # 上記シナリオのステップ実装

# ドキュメント(既存資産の整合更新)
specs/001-nostr-p2p-yp/contracts/p2p-gossip.md  # 検査5 の違反時ログ条件を精緻化
specs/001-nostr-p2p-yp/data-model.md            # pex_rejected の記録条件を更新
docs/adr/0013-pex-benign-rejection.md           # 新規 ADR
CONTEXT.md                                       # SecurityEvent 記述に乖離があれば追随
```

**Structure Decision**: 単一プロジェクト構成。変更は P2P レイヤ 2 ファイルに集中し、
ドキュメントは既存 feature 001 の契約・data-model を「正」として更新する(単一コンテキスト方針 — CLAUDE.md)。

## Complexity Tracking

> Constitution Check に違反なし。追加の正当化は不要。

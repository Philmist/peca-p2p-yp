# Tasks: PEX 自己アドレス拒否の良性化

**Input**: Design documents from `/specs/005-pex-self-reject/`

**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/pex-rejection-classification.md, quickstart.md

**Tests**: 本機能はテスト必須(FR-006 / Principle IV)。Gherkin ネガティブシナリオと契約テストを**実装より先に失敗させる**。

**Organization**: ユーザーストーリー単位でフェーズ分割し、各ストーリーを独立して検証可能にする。

## Format: `[ID] [P?] [Story] Description`

- **[P]**: 並行実行可能(別ファイル・未完了タスクへの依存なし)
- **[Story]**: US1 / US2 / US3(spec.md のユーザーストーリーに対応)
- パスはリポジトリルート起点

## Path Conventions

- 単一プロジェクト構成(`src/`, `tests/` がルート直下)
- 主変更: `src/p2p/pex.rs`・`src/p2p/runtime.rs`
- テスト: `tests/contract/pex.rs`・`tests/features/security.feature`・`tests/steps/security.rs`

---

## Phase 1: Setup (Shared Infrastructure)

**Purpose**: 変更前のベースライン確認(回帰基準の固定)

- [X] T001 変更前のベースラインを確認: `cargo test --lib p2p::pex`(cucumber は harness=false でフィルタ引数を受けないため `--lib` 限定)と `cargo test --test pex`(Cargo.toml のテストターゲット名は `pex`。`path = tests/contract/pex.rs`)が現行仕様で通ることを記録し、`accepted` 集合の回帰基準(FR-004 / SC-004)を把握する

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: 破棄理由の分類を担う `IncomingPex` / `validate_incoming_peers`(全ユーザーストーリーの土台)

**⚠️ CRITICAL**: このフェーズが完了するまで US1 / US2 の挙動実装は開始できない

- [X] T002 [P] `src/p2p/pex.rs` の `IncomingPex` に破棄分類フィールドを追加する: `benign_rejected: Vec<String>`(自己アドレス・重複)と `suspicious_rejected: Vec<String>`(件数超過・parse 失敗・ホスト名。長さ超過は parse 失敗に包含 — contracts C1)。既存 `pub rejected: Vec<String>`(`src/p2p/pex.rs:86`)は**フィールドから両リスト連結を返す派生アクセサ(メソッド)へ変更する破壊的変更**。既存 `has_rejections()`(`src/p2p/pex.rs:91`)は `has_benign() || has_suspicious()` を返す形で**維持**する(runtime.rs:712 が使用中のため廃止しない)。既存利用箇所の移行を同タスク内で完了させる: `src/p2p/pex.rs` 内の単体テスト(L272/282/295/309 の `r.rejected` 参照)・`tests/contract/pex.rs`(L191/207/224/256 の `result.rejected` 参照)・`src/p2p/runtime.rs:712` を新アクセサ経由へ更新(data-model.md エンティティ1)
- [X] T003 `src/p2p/pex.rs` に判定メソッド `has_suspicious()`(= `suspicious_rejected` 非空)と `has_benign()`(= `benign_rejected` 非空)を追加する(T002 に依存)
- [X] T004 `src/p2p/pex.rs` の `validate_incoming_peers` を分類規則(contracts C1)に従って更新する: 件数 > 64 の全破棄・`parse_addr` 失敗・ホスト名 → `suspicious_rejected`、自己アドレス一致・バッチ内 canonical 重複 → `benign_rejected` へ振り分ける。**破棄する/しないの判定(候補登録の可否)は一切変更しない**(FR-004 / data-model 不変条件)(T003 に依存)
- [X] T005 `src/p2p/pex.rs` に分類の意図コメントを付す(良性=健全な網で常時発生する反射・dual-stack 重複 / 不審=protocol 逸脱・不正入力)(Principle III, T004 に依存)

**Checkpoint**: `IncomingPex` が良性/不審を区別でき、`accepted` は変更前と同一。US1 / US2 の挙動実装に着手可能

---

## Phase 3: User Story 1 - 良性な自己反射・重複でセキュリティログが汚染されない (Priority: P1) 🎯 MVP

**Goal**: 破棄理由が良性(自己アドレス・重複)のみのとき `pex_rejected` を記録せず、debug ログへ格下げする

**Independent Test**: 自己アドレスのみ / 重複のみの `PEERS` を検証し、破棄は起きるが `pex_rejected` が 0 件、かつ debug ログに source と破棄件数が出ることを確認する

### Tests for User Story 1 (先に書いて FAIL させる) ⚠️

- [X] T006 [P] [US1] `tests/contract/pex.rs` に良性のみの契約テストを追加する: (a) 自己アドレスのみ → `has_suspicious() == false` かつ `benign_rejected` 非空、(b) 同一 canonical 重複のみ(不審なし)→ `has_suspicious() == false`。各ケースで `accepted` 集合が変更前と同一であることをアサートする(contracts テスト観点1・2・6 / FR-004・SC-004 の回帰)
- [X] T007 [P] [US1] `tests/features/security.feature` に良性シナリオを追加する: 「自己アドレスのみの PEX 破棄はセキュリティイベントを生成しない」「重複のみの PEX 破棄はセキュリティイベントを生成しない」(FR-006, spec US1 受け入れシナリオ1・2)
- [X] T008 [US1] `tests/steps/security.rs` に T007 の良性シナリオのステップ実装を追加し、`pex_rejected` が記録されないこと・debug 観測できることを検証する(spec US1 シナリオ3)

### Implementation for User Story 1

- [X] T009 [US1] `src/p2p/runtime.rs` の `Message::Peers` ハンドラを更新する: `result.has_suspicious()` のときのみ `security.log(SecurityCategory::PexRejected, addr, ...)` を呼び、良性のみのときは呼ばない(FR-001)
- [X] T010 [US1] `src/p2p/runtime.rs` に良性破棄の debug ログを追加する: `result.has_benign()` のとき `tracing::debug!(target: "p2p", source = %addr, benign = result.benign_rejected.len(), ...)` を 1 行出力。自ノードアドレスを載せる場合は自ノード自身のものに限る(FR-002 / FR-007, T009 に依存)

**Checkpoint**: 良性のみの `PEERS` で `pex_rejected` が出ず、debug に観測される。T006–T008 が緑

---

## Phase 4: User Story 2 - 真に不審な PEX 内容は引き続き検知される (Priority: P1)

**Goal**: 件数超過・形式不正・ホスト名・長さ超過を含む(良性混在含む)`PEERS` で従来どおり `pex_rejected` を記録する

**Independent Test**: 65 件の `PEERS`・形式不正エントリ・良性+不審の混在を投入し、いずれも `pex_rejected` が記録されることを確認する

### Tests for User Story 2 (先に書いて FAIL させる) ⚠️

- [X] T011 [P] [US2] `tests/contract/pex.rs` に不審系の契約テストを追加する: (a) 65 件 → 全破棄 + `has_suspicious() == true`、(b) 形式不正 / ホスト名エントリ → `has_suspicious() == true`。`accepted` 集合が変更前と同一であることをアサートする(contracts テスト観点3・4・6 / FR-004・SC-004 の回帰)
- [X] T012 [P] [US2] `tests/contract/pex.rs` に混在の契約テストを追加する: 自己アドレス(良性)+ 形式不正(不審)→ `has_suspicious() == true` かつ `has_benign() == true`。`accepted` 集合が変更前と同一であることをアサートする(contracts テスト観点5・6, spec US2 シナリオ3 / FR-004・SC-004 の回帰)
- [X] T013 [P] [US2] `tests/features/security.feature` に不審・混在シナリオを追加する: 「不正な PEX 内容(件数超過/形式不正)はセキュリティイベントを生成する」「良性と不正の混在はセキュリティイベントを生成する」(FR-006, spec US2 受け入れシナリオ)
- [X] T014 [US2] `tests/steps/security.rs` に T013 の不審・混在シナリオのステップ実装を追加し、`pex_rejected` が記録されることを検証する

### Implementation for User Story 2

- [X] T015 [US2] `src/p2p/runtime.rs` の記録分岐(T009)が不審・混在で確実に `pex_rejected` を記録することを確認・固定する(件数超過の全破棄も `suspicious_rejected` に入るため記録される)。既存の `source` / `detail` は内部情報を漏洩しないまま不変(FR-003 / FR-007)

**Checkpoint**: 不審・混在で `pex_rejected` が 100% 記録(SC-002)。T011–T014 が緑。US1 と US2 が両立

---

## Phase 5: User Story 3 - 契約・ドキュメントとの整合 (Priority: P2)

**Goal**: 変更後の挙動を契約・data-model・ADR に反映し、コードとドキュメントの乖離を残さない

**Independent Test**: 契約・data-model の該当記述を読み、良性=無記録・不審=記録が明記され、ADR に根拠があることを確認する

### Implementation for User Story 3

- [X] T016 [P] [US3] `specs/001-nostr-p2p-yp/contracts/p2p-gossip.md` の検査5(line 85 付近)の「違反時ログ」条件を精緻化する: 良性(自己アドレス・重複)のみの破棄は `pex_rejected` 対象外・不審な破棄で記録、と明記(FR-005, spec US3 シナリオ1)
- [X] T017 [P] [US3] `specs/001-nostr-p2p-yp/data-model.md`(line 207 付近)の `pex_rejected` 行を更新する: 「PEERS 内容違反(不審な破棄)。自己アドレス・重複のみの破棄は良性として記録しない(feature 005 / ADR-0013)」(FR-005)
- [X] T018 [P] [US3] `docs/adr/0013-pex-benign-rejection.md` を新規作成する: (a) 良性/不審の切り分け根拠、(b) Principle V 非クリティカルの判断理由(research R5)、(c) Security Requirements との整合、(d) Principle I/II/IV/VI の参照を記載(FR-005, spec US3 シナリオ2)
- [X] T019 [US3] `CONTEXT.md` の SecurityEvent 記述に `pex_rejected` の記録条件変更との乖離があれば追随する(research R6, 単一コンテキスト方針)

**Checkpoint**: コードとドキュメントが一致(FR-005)。全ユーザーストーリーが独立して成立

---

## Phase 6: Polish & Cross-Cutting Concerns

**Purpose**: 静的検査・整合の最終確認

- [X] T020 `cargo fmt -- --check` と `cargo clippy --all-targets -- -D warnings` を通す(CLAUDE.md / quickstart §1)
- [X] T021 [P] `docs/adr/security-review-checklist.md` の観点チェックリスト適用結果を `specs/005-pex-self-reject/checklists/security.md` に記録する(本機能はセキュリティイベント `pex_rejected` の記録条件を変える「セキュリティに関わる変更」。Constitution Principle III の MUST / 実装中ゲート6。feature 004 の `checklists/security.md` に倣う)
- [X] T022 `specs/005-pex-self-reject/quickstart.md` の検証手順(§2 単体・契約 / §3 BDD / §4 実運用観点 / §5 ドキュメント整合)を実行し、SC-001〜SC-004 を確認する

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: 依存なし
- **Foundational (Phase 2)**: Setup 後。US1 / US2 の挙動実装をブロック
- **User Story 1 (Phase 3)**: Foundational 後
- **User Story 2 (Phase 4)**: Foundational 後。記録分岐を US1 の T009 と共有(`runtime.rs` 同一箇所)するため、US1 の T009 完了後に T015 を確認する
- **User Story 3 (Phase 5)**: 挙動実装(US1/US2)確定後が望ましいが、ドキュメント作業のため並行着手可
- **Polish (Phase 6)**: 全ストーリー完了後

### User Story Dependencies

- **US1 (P1)**: Foundational のみに依存。MVP
- **US2 (P1)**: Foundational に依存。`runtime.rs` の記録分岐を US1 と共有(同一ファイル・同一分岐)するため US1 と直列化
- **US3 (P2)**: コード変更に依存しない(ドキュメント)。独立

### Within Each User Story

- テスト(契約・Gherkin)を先に書いて FAIL 確認 → 実装
- `IncomingPex` 拡張(Foundational)→ `runtime.rs` 分岐(US1/US2)
- ストーリー完了後に次優先へ

### Parallel Opportunities

- Foundational: T002 は単独 [P] だが T003→T004→T005 は同一ファイル直列
- US1 テスト: T006(contract)と T007(feature)は別ファイルで [P]。T008 は T007 に続く
- US2 テスト: T011 / T012(contract, 同一ファイルのため実際は直列)/ T013(feature)は [P]
- US3: T016 / T017 / T018 は別ファイルで全て [P]。T019 は最後
- **注意**: `src/p2p/runtime.rs` を触る T009 / T010 / T015 は同一ファイルのため直列

---

## Parallel Example: User Story 3(ドキュメント整合)

```bash
# 別ファイルなので並行可能:
Task: "specs/001-nostr-p2p-yp/contracts/p2p-gossip.md 検査5 を更新 (T016)"
Task: "specs/001-nostr-p2p-yp/data-model.md の pex_rejected 行を更新 (T017)"
Task: "docs/adr/0013-pex-benign-rejection.md を新規作成 (T018)"
```

---

## Implementation Strategy

### MVP First (User Story 1 のみ)

1. Phase 1: Setup(ベースライン固定)
2. Phase 2: Foundational(`IncomingPex` 分類 — 全ストーリーをブロック)
3. Phase 3: US1(良性 → 無記録 + debug)
4. **STOP and VALIDATE**: 良性のみで `pex_rejected` が 0 件・debug 観測を独立確認(SC-001 / SC-003)
5. ここまでで主目的(信号対雑音比の回復)は達成

### Incremental Delivery

1. Setup + Foundational → 分類基盤が完成
2. US1 追加 → 独立検証 → MVP
3. US2 追加 → 不審・混在の記録を独立検証(SC-002)
4. US3 追加 → ドキュメント整合(FR-005)
5. Polish → fmt/clippy + quickstart 全体検証

### 注意事項

- テストは実装前に FAIL させる(Principle IV ゲート)
- `runtime.rs` を触る 3 タスク(T009/T010/T015)は同一ファイル直列
- 防御(破棄)の不変性(FR-004 / SC-004)を各段階で回帰確認
- cargo 系はサブエージェントに投げず前景実行(運用メモ)
- タスクまたは論理グループごとにコミット

---

## Phase 7: Convergence

- [X] T023 `tests/steps/security.rs` の良性シナリオ(自己アドレスのみ・重複のみ)に debug ログ観測の自動検証を追加する: tracing キャプチャ(`tests/steps/keystore.rs` の共有バッファ方式に倣い debug レベルを有効化)で、良性破棄時に source と良性破棄件数を含む debug 行が出力されることをアサートする(T008 の未実装分)per US1/AC3 (partial)
- [X] T024 `CONTEXT.md` の ADR 一覧(ADR-0012 の次)に `ADR-0013: PEX 破棄の良性/不審分類(pex_rejected は不審な破棄のみ記録・良性は debug 格下げ)` を追記する per FR-005 (partial)
- [X] T025 `tests/features/security.feature` に件数超過シナリオ「65 件の PEX 応答はセキュリティイベントを生成する」を追加し、`tests/steps/security.rs` にステップ実装(モックピアが 65 件を共有 → 全破棄 + `pex_rejected` 記録を検証)を追加する(T013 が挙げた件数超過の e2e 検証)per US2/AC1 (partial)

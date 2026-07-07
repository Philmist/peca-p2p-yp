# Tasks: 掲載前のペルソナ選択(選択中ペルソナの明示的な切り替え)

**Input**: Design documents from `/specs/003-persona-selection/`

**Prerequisites**: [plan.md](./plan.md), [spec.md](./spec.md), [research.md](./research.md), [data-model.md](./data-model.md), [contracts/local-api.md](./contracts/local-api.md)

**Tests**: 本機能はテストを**含む**。Principle IV(BDD with Gherkin, tests-first MUST)および ADR-0011 の代替担保(並行性統合テスト・cucumber ネガティブ)により、各ストーリーはテスト先行で実装する(テストが失敗する状態を確認してから実装)。

**Organization**: タスクはユーザーストーリー単位でグループ化する。US1(選択操作)と US2(配信中ロック)はいずれも P1 で対を成し、両方そろって初めて「安全な選択機能」が成立する(spec US2 Why)。

## Format: `[ID] [P?] [Story] Description`

- **[P]**: 並行実行可(異なるファイル・未完了タスクへの依存なし)
- **[Story]**: US1 / US2 / US3(spec のユーザーストーリー)
- 各タスクに正確なファイルパスを記載

## Path Conventions

単一 Rust プロジェクト構成。ソースは `src/`、テストは `tests/`(`contract/`・`integration/`・`steps/` + `features/*.feature` の cucumber)、UI は `ui/*.html`。モジュール登録は `src/lib.rs`。

---

## Phase 1: Setup(共有インフラ)

**Purpose**: BDD シナリオの受け皿を用意する(実シナリオは各ストーリーのテストタスクで追加)。

- [ ] T001 cucumber ランナー `tests/cucumber.rs` に新規ステップモジュール `persona_selection` を登録し、空のステップ定義ファイル `tests/steps/persona_selection.rs` と空の feature ファイル `tests/features/persona_selection.feature` を作成する(以降の Given/When/Then の受け皿)

---

## Phase 2: Foundational(ブロッキング前提)

**Purpose**: 全ストーリーが依存する共有状態 `BroadcastState` と配線・エラー型を整える。循環依存を避けるため中立モジュールに置き `Arc` 共有する(research R3)。

**⚠️ CRITICAL**: 本フェーズ完了までどのユーザーストーリーも着手不可。

- [ ] T002 `src/broadcast.rs` を新規作成し `BroadcastState { channels: Mutex<HashSet<String>> }` と公開 API シグネチャ(`is_broadcasting() -> bool`、`reserve_and_read_selected(...)`、`release(&channel_id)`、`guard_selected_mutation(...)`)を定義する。本体は後続 US2 で実装するが、既定で never-broadcasting(空集合)として `is_broadcasting()` は `false` を返す。ファイル冒頭に不変条件 INV-1(相互排他)/INV-2(予約先行)/INV-3(確実な解錠)の意図コメントを置く(Principle III MUST、data-model §BroadcastState)
- [ ] T003 `src/lib.rs` に `pub mod broadcast;` を追加してクレートへ登録する
- [ ] T004 [P] `src/identity/mod.rs` の `IdentityError` に新規バリアント `BroadcastingLocked` と `NotSelectable` を追加する(data-model §エラー写像)
- [ ] T005 `src/identity/mod.rs` の `IdentityManager` に `Arc<BroadcastState>` フィールドと `with_broadcast_state(Arc<BroadcastState>)` ビルダを追加する。既定フィールドは never-broadcasting の空 `Arc` とし、既存の `new(store, keystore)` 経路のテストがロックガード no-op で挙動不変になるようにする(research R3)
- [ ] T006 [P] `src/web/mod.rs` の `AppState` に `broadcast: Option<Arc<BroadcastState>>` フィールドと `with_broadcast(Arc<BroadcastState>)` ビルダを追加する(既存 `with_announced`/`with_node_status` に倣う)
- [ ] T007 [P] `src/event/publish.rs` の `PublishEngine` に `broadcast: Arc<BroadcastState>` フィールドを追加し、`PublishEngine::new(...)` の引数に受け取る(配線のみ。予約/解除ロジックは US2 の T023 で実装)
- [ ] T008 `src/main.rs` で単一の `Arc<BroadcastState>` を生成し、`IdentityManager`(`with_broadcast_state`)・`PublishEngine::new`・`AppState`(`with_broadcast`)の 3 者へ同一インスタンスを配布する(T002–T007 に依存)
- [ ] T009 [P] `src/web/personas.rs` の `identity_err()` に写像を追加する: `BroadcastingLocked` → 409 `{"error":"broadcasting_locked"}`、`NotSelectable` → 409 `{"error":"persona_not_selectable"}`(既存 `Unusable`→422 は流用。T004 に依存、contracts §5)

**Checkpoint**: 共有状態と配線が整い、各ストーリーの実装に着手可能。

---

## Phase 3: User Story 1 - 配信を始める前に名乗るペルソナを選ぶ (Priority: P1) 🎯 MVP

**Goal**: `active` かつ `usable` なペルソナを 1 クリックで選択中に切り替えられ、バナーが即時追随する。archived/unusable はバックエンドでも拒否する(FR-001/002/003/004/013、SC-001)。

**Independent Test**: 有効ペルソナ 2 つを用意し交互に「選択」→ 「現在選択中」バナーが即追随。archived を直接 API で選択 → 409。0 個のとき作成導線が出る。

### Tests for User Story 1 ⚠️（先に書いて失敗を確認)

- [ ] T010 [P] [US1] `src/identity/mod.rs` の `#[cfg(test)]` に `select()` の状態ガード単体テストを追加する: active+usable → `Ok`、archived → `NotSelectable`、unusable → `Unusable`、不在 → `NotFound`(R4/FR-002)
- [ ] T011 [P] [US1] `tests/contract/local_api.rs` に `PUT /api/v1/personas/{pubkey}` `{select:true}` の契約テストを追記する: 有効 → 204、archived → 409 `persona_not_selectable`、unusable → 422 `persona_unusable`(contracts §1)
- [ ] T012 [P] [US1] `tests/features/persona_selection.feature` に「非配信中に有効ペルソナを選択できる」「archived / unusable は選択できない」シナリオ(contracts §1 Gherkin)を追加し、`tests/steps/persona_selection.rs` に対応ステップを実装する

### Implementation for User Story 1

- [ ] T013 [US1] `src/identity/mod.rs` の `select()` に選択可能ガードを追加する: 対象が存在し `state == Active` かつ keystore 復号可能(`usable`)でなければ `NotSelectable`/`Unusable` を返す。UI だけでなくバックエンドで拒否(FR-002、R4)(T004 に依存)
- [ ] T014 [US1] `src/identity/mod.rs` の `create()` の最初ペルソナ自動選択が新ガードを通過すること(作成直後は active+usable)を確認し、2 個目以降で selected を自動変更しない既存挙動を維持する(FR-004)
- [ ] T015 [US1] `ui/personas.html` を変更する: 各 active 行に「選択」ボタン(現在 selected 行は「選択中」ラベル+強調、ラジオ的に常に 1 つ)、押下で **確認ダイアログを挟まず**即時に `PUT /api/v1/personas/{pubkey}` `{select:true}` を送信し「現在選択中」バナー表示(FR-012 — 常時明示で誤爆防止)。archived/unusable 行の選択ボタンは無効化(グレーアウト)。**既存の破棄・秘密鍵エクスポートの確認フローは変更しない**(FR-012 後段)(FR-001/003/010/012)
- [ ] T016 [US1] `ui/personas.html` にペルソナ 0 個時の作成導線を追加する(FR-013、spec US1 シナリオ 2)

**Checkpoint**: 非配信中の選択操作が UI・API 双方で完結し独立検証可能。

---

## Phase 4: User Story 2 - 配信中は名乗っているペルソナを凍結する (Priority: P1)

**Goal**: 1 つ以上のチャンネルを発行中の間、selected の切替/破棄/アーカイブを 409 `broadcasting_locked` で拒否し、TOCTOU レースなく「配信中は selected 不変」を構造的に保証する(FR-005/007/008/009、SC-002/003/005)。label 変更と他ペルソナ操作は許可。

**Independent Test**: 配信中に selected の切替/破棄/アーカイブを直接 API で試行 → いずれも 409。label 変更・他ペルソナ操作は成功。全チャンネル ended 後に再び選択が 204。

### Tests for User Story 2 ⚠️（先に書いて失敗を確認)

- [ ] T017 [P] [US2] `src/identity/mod.rs` の `#[cfg(test)]` にロックガード単体テストを追加する: 共有 `BroadcastState` を非空にした状態で `select`/`delete`/`set_state(→archived)` が `BroadcastingLocked`、`set_label` と非 selected ペルソナ操作は許可(FR-005/006/007)
- [ ] T018 [P] [US2] `tests/integration/persona_lock.rs`(新規)に**並行性統合テスト**を追加する: 「発行開始(予約)」と「`select(B)`」を交錯させ、どちらが先にロックを取っても不変条件「配信中の区間 selected は不変」が保たれることを確認する(SC-005 / R2、ADR-0011 代替担保)
- [ ] T019 [P] [US2] `tests/integration/persona_lock.rs` に解錠統合テストを追加する: 全チャンネルが `publish_ended` で集合から除去され `is_broadcasting()==false` になった後、`select` が成功する(SC-003 / FR-009)
- [ ] T020 [P] [US2] `tests/contract/local_api.rs` に配信中拒否の契約テストを追記する: 配信中の `PUT {select:true}` / `PUT {state:"archived"}` / `DELETE ?confirm=true` がいずれも 409 `broadcasting_locked`、および `GET /api/v1/status` が `broadcasting: bool` を含む(contracts §1/§2/§3)
- [ ] T021 [P] [US2] `tests/features/persona_selection.feature` に配信中ネガティブシナリオ(直接 API での切替/破棄/アーカイブ → 409、label と他ペルソナ操作は許可、停止後は解錠、**および古い画面状態から送信した制限操作 → 409 + `GET /status` が最新の `broadcasting` を返す**=競合 edge case)を追加し、`tests/steps/persona_selection.rs` にステップを実装する(Principle IV、spec edge case「UI の状態が古いまま制限操作を送信」、contracts §1 Gherkin)

### Implementation for User Story 2

- [ ] T022 [US2] `src/broadcast.rs` の本体を実装する: 単一 `Mutex` 下で `reserve_and_read_selected`(selected 読取 + チャンネル予約を原子的に)、`guard_selected_mutation`(配信中集合が非空なら拒否)、`release`(集合から除去)を相互排他に行う。ロック外で署名する前提を意図コメントで明記(INV-1/INV-2、R2)
- [ ] T023 [US2] `src/event/publish.rs` を変更する: あるチャンネルの**初回発行時に予約を署名の前**に実行(`reserve_and_read_selected`)、`publish_ended` および署名失敗時に `release` で巻き戻す。周期再発行・終了時最終発行の発行契機自体は変更しない(FR-015、INV-2/INV-3、R2)(T022 に依存)
- [ ] T024 [US2] `src/identity/mod.rs` の `select()`・`delete()`・`set_state(→archived)` に `guard_selected_mutation` を通し、対象が現在の selected かつ配信中なら `BroadcastingLocked` を返す。`set_label` は配信中でも許可(FR-005/006/007)(T022 に依存)
- [ ] T025 [US2] `src/web/announced.rs` の `StatusResponse` に `broadcasting: bool` を追加し、`get_status()` で `BroadcastState::is_broadcasting()` を反映する(供給元未配線時は `false`)(FR-008、R6、contracts §3)
- [ ] T026 [US2] `ui/personas.html` を変更する: 既存 5 秒ポーリングの `GET /status` の `broadcasting` を見て、selected 行の選択/破棄/アーカイブボタンを無効化し「配信中はペルソナを変更できません」と理由表示する(best-effort、真の強制は T024 のバックエンド拒否)(FR-005 UI 側)

**Checkpoint**: US1 + US2 で安全な選択機能が成立(P1 完了・リンク推定を構造的に防止)。

---

## Phase 5: User Story 3 - 誤ったペルソナで名乗っていないか常に自覚できる (Priority: P2)

**Goal**: チャンネル一覧画面に現在の selected を読み取り専用で常時明示し、selected が後から archived/unusable 化したら警告表示にして掲載を保留に落とす(FR-010/011、SC-004)。

**Independent Test**: channels.html に selected が label+短縮 pubkey で表示され切替に追随。selected を archived にすると全画面が警告状態になり `GET /personas` の `selected` が全 false、掲載が保留に落ちる。

### Tests for User Story 3 ⚠️（先に書いて失敗を確認)

- [ ] T027 [P] [US3] `src/identity/mod.rs` の `#[cfg(test)]` に `selected()` セマンティクス拡張の単体テストを追加する: 対象が破棄済み・archived・unusable のいずれでも `None` を返す(R5/FR-011)
- [ ] T028 [P] [US3] `tests/integration/persona_lock.rs`(または新規 `tests/integration/persona_display.rs`)に統合テストを追加する: selected を archived にすると `GET /personas` の全要素 `selected==false` になり、新規到着チャンネルの掲載が `Ok(false)`(保留)に落ちる(FR-011)

### Implementation for User Story 3

- [ ] T029 [US3] `src/identity/mod.rs` の `selected()` を拡張する: 対象が破棄済みに加え archived または unusable(復号失敗)の場合も `None` を返す。設定値 `selected_persona` は消去せず都度判定にする(FR-011、R5)
- [ ] T030 [US3] `ui/channels.html` を変更する: `GET /api/v1/personas` の `selected`・`label`・`pubkey` から現在の selected を label+短縮 pubkey で読み取り専用表示する。未選択(全 false / 0 個)は「未選択(ペルソナを作成してください)」、archived/unusable 相当は警告表示にして再選択を促す(FR-010/011、R7、新 API フィールド不要)

**Checkpoint**: 全ストーリーが独立に機能。誤爆防止の常時明示が完成。

---

## Phase 6: Polish & Cross-Cutting Concerns

**Purpose**: 品質ゲートとトレーサビリティの最終確認。

- [ ] T031 [P] ロック順序・TOCTOU 閉塞・予約先行の意図コメントを `src/broadcast.rs`・`src/event/publish.rs`・`src/identity/mod.rs` でレビューし、保安上複雑なロジックに意図が明記されていることを確認する(Principle III MUST)
- [ ] T032 [P] `docs/adr/0011-broadcasting-lock.md` と spec/contracts の参照原則番号・FR/SC 対応の齟齬がないか最終確認する(Principle VI、checklists/security.md・api.md 参照)
- [ ] T033 `cargo fmt -- --check` を通す(CLAUDE.md / CI)
- [ ] T034 `cargo clippy` を警告なしで通す(CI)
- [ ] T035 `quickstart.md` のシナリオ 1〜6 を手動実行し、SC-001〜SC-005 の受け入れを確認する

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: 依存なし・即着手可
- **Foundational (Phase 2)**: Setup 後。全ストーリーをブロックする(特に T008 main.rs 配線は T002–T007 に依存)
- **User Stories (Phase 3–5)**: Foundational 完了後
  - US1(P1)と US2(P1)は対だが独立テスト可能。両方とも `src/identity/mod.rs` の `select()` を変更するため、同一関数の編集は逐次化する(US1 の T013 → US2 の T024 の順を推奨)
  - US3(P2)は `selected()` 拡張が中心で US1/US2 と別関数のため比較的独立
- **Polish (Phase 6)**: 対象ストーリー完了後

### User Story Dependencies

- **US1 (P1)**: Foundational 後に着手可。他ストーリーへの依存なし(MVP の中核)
- **US2 (P1)**: Foundational 後に着手可。`select()` 編集で US1 の T013 と同一ファイル・同一関数を触るため T013 の後に T024 を行う
- **US3 (P2)**: Foundational 後に着手可。`selected()` 拡張は独立だが、UI 表示は US1 の selected 導出と整合させる

### Within Each User Story

- テストを先に書き、失敗を確認してから実装(tests-first MUST)
- broadcast 本体(T022)→ publish 予約(T023)/ identity ロックガード(T024)の順
- 実装完了後にチェックポイント検証

### Parallel Opportunities

- Foundational: T004・T006・T007 は異なるファイルで並行可(T005 は T004 と同ファイル `identity/mod.rs` のため逐次)。T009 は T004 完了後に並行可
- 各ストーリーのテストタスク([P] 付き)は相互に別ファイルのため並行可(T010–T012、T017–T021、T027–T028)
- Polish の T031・T032 は並行可

---

## Parallel Example: User Story 2 のテスト

```bash
# US2 のテストを並行起票(いずれも別ファイル):
Task: "ロックガード単体テスト in src/identity/mod.rs (#[cfg(test)])"          # T017
Task: "並行性統合テスト in tests/integration/persona_lock.rs"                  # T018
Task: "解錠統合テスト in tests/integration/persona_lock.rs"                    # T019
Task: "配信中拒否の契約テスト in tests/contract/local_api.rs"                  # T020
Task: "配信中ネガティブ cucumber in tests/features/persona_selection.feature"  # T021
```

---

## Implementation Strategy

### MVP スコープ

- テンプレート上の MVP は **US1(選択操作)単独**。ただし US1 単独では配信中の入替を防げず SC-005(プライバシー核心)を満たさない。**安全な出荷単位は US1 + US2(ともに P1)**。US2 の Why(spec)どおり両者を対で完成させることを推奨する。
- US3(P2)は誤爆防止の常時明示で、US1/US2 完成後に追加する補完。

### Incremental Delivery

1. Setup + Foundational → 共有状態と配線が完成
2. US1 → 非配信中の選択操作(内部検証)
3. US2 → 配信中ロック(**ここで P1 = 安全な選択機能が成立、SC-005 達成**)
4. US3 → 常時明示表示(P2)
5. Polish → fmt/clippy/quickstart 検証

### 検証の要点(ADR-0011 代替担保)

- Principle V は非該当(ADR-0011)。代替担保として **T018 の並行性統合テスト**(予約 vs select の相互排他)と **T021 の cucumber ネガティブ**(直接 API バイパス → 409)を必須とする。これらが緑になることが SC-005 の受け入れ根拠。

---

## Notes

- [P] = 異なるファイル・依存なしで並行可
- [Story] ラベルでタスクをストーリーへトレース
- テストは実装前に失敗を確認(tests-first)
- タスクまたは論理単位ごとにコミット(コミット手順は scratchpad + `git commit -F`、トレーラーは事後 amend)
- `select()` は US1(T013)と US2(T024)が同一関数を触るため編集順に注意(same-file conflict 回避)
- `cargo fmt -- --check` を各コミット前に通す(CLAUDE.md / CI)

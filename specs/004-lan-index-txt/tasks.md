# Tasks: 読み取り専用 index.txt の LAN 公開(オプトイン)

**Input**: Design documents from `/specs/004-lan-index-txt/`

**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/index-txt-lan.md, quickstart.md

**Tests**: constitution Principle IV(テストファースト MUST)および plan の fail-first 順序化に従い、
各ストーリーでテストを先に書き、**失敗を確認してから**実装する。

**Organization**: ユーザーストーリー単位でフェーズ化し、各ストーリーが独立に実装・検証できる。

## Format: `[ID] [P?] [Story] Description`

- **[P]**: 並列実行可(異なるファイル・未完了タスクへの依存なし)
- **[Story]**: 対応するユーザーストーリー(US1〜US4)
- 各タスクに正確なファイルパスを含む

## Path Conventions

単一クレート構成: `src/`・`tests/`・`ui/` はリポジトリルート直下(plan.md の構造どおり)。

---

## Phase 1: Setup(実装前ゲート)

**Purpose**: ADR ゲートの通過(Principle I「実装前レビュー」/ Principle II/VI)

- [X] T001 【GATE】ADR-0012 のユーザー承認を得て `docs/adr/0012-index-txt-lan-exposure.md` の Status を Proposed → Accepted に更新する。**承認まで T003 以降の実装タスクを開始してはならない**
- [X] T002 ADR-0012 承認と同時に `docs/adr/0006-unencrypted-transport.md` の冒頭へ「決定 4 は read-only index.txt に限り ADR-0012 が部分 supersede」の追記を行う(research R7)

**Checkpoint**: ADR-0012 が Accepted であること。これが Phase 2 以降の前提条件。

---

## Phase 2: Foundational(全ストーリーの前提)

**Purpose**: `index_bind` 設定キーと許可リスト検証 `require_lan_or_loopback` — 全ストーリーが依存する唯一の検証ゲート

**⚠️ CRITICAL**: このフェーズ完了までユーザーストーリーの実装を開始しない

- [X] T003 fail-first ユニットテスト: `require_lan_or_loopback` のゴールデン/ネガティブ判定テーブル(data-model.md §2 と 1:1 — loopback・RFC1918 境界・リンクローカル・ULA 境界・v4-mapped・ゾーン ID の受理、unspecified・グローバル・CGNAT(100.64/10)・境界外・書式不正の拒否)を `src/config.rs` のテストモジュールに追加し、**失敗することを確認**する
- [X] T004 `require_lan_or_loopback(key, value)` と `ConfigError::NonLanBind { key }` を `src/config.rs` に実装する(`SocketAddr` パース → `to_canonical()` → IPv4: `is_loopback/is_private/is_link_local`、IPv6: loopback + fc00::/7・fe80::/10 のビット判定 — research R1)。T003 のテストを green にする
- [X] T005 `Settings` に `index_bind: String`(既定 `""`)とキー `"index_bind"`(13→14 キー、lenient load / 全キー save の既存規約)を追加し、`validate()` に「空 = 検証スキップ、非空 = `require_lan_or_loopback`」を統合する(`src/config.rs`)
- [X] T006 `CliOverrides` に `index_bind: Option<String>` と `--index-bind`(`--key value` / `--key=value` 両形式)を追加する(`src/config.rs`)

**Checkpoint**: `cargo test`(config ユニットテスト)green。既存 `http_bind` / `pcp_bind` / `p2p_bind` の検証は不変(FR-001)。

---

## Phase 3: User Story 1 - 別 PC の YP ブラウザから掲載一覧を取得する (Priority: P1) 🎯 MVP

**Goal**: `index_bind` 非空時に index.txt 配信専用の第 2 リスナーを起動し、LAN 内の別 PC から loopback 側と同一内容の index.txt を取得できるようにする

**Independent Test**: `index_bind` に LAN アドレスを設定して起動し、別 PC(またはテスト内の別ソケット)から index.txt を取得して同一ホスト取得分と比較する(spec US1 Independent Test)

### Tests for User Story 1(fail-first)⚠️

- [ ] T007 [US1] `Cargo.toml` に `[[test]] name = "index_lan"` を追加し、統合テスト `tests/integration/index_lan.rs` を新規作成する。fail-first シナリオ: (1) LAN リスナーへの `GET /index.txt` が loopback 側と同一内容・同一 `Content-Type`(`index_txt_encoding` 共有)、(2) `HEAD /index.txt` が GET と整合、(3) `index_bind` 空なら第 2 リスナーが存在しない(接続拒否)、(4) 同一送信元 10 req/秒超過で 429 `{"error":"rate_limited"}`(spec US1 受入 1〜4)。**失敗することを確認**する

### Implementation for User Story 1

- [ ] T008 [US1] `build_index_router(state: AppState) -> Router` を `src/web/mod.rs` に追加する(`Router::new().merge(index_txt::routes()).with_state(state)` — research R2。fallback は US2 で追加)
- [ ] T009 [US1] `src/main.rs` §15 直後に `index_bind` 非空時の第 2 リスナー起動を追加する(`TcpListener::bind` → `axum::serve` を `into_make_service_with_connect_info::<SocketAddr>()` で起動、`with_graceful_shutdown` + `handles` へ push — 既存 §17 と同パターン)。起動サマリログに LAN 公開の記載を追加する。bind 失敗時は暫定で `tracing::warn!` + リスナー起動スキップとし、panic・即終了させない(縮退の完成形 = 状態反映と定型コード写像は T022)

**Checkpoint**: `cargo test --test index_lan` の US1 シナリオが green。MVP としてこの時点で spec SC-002 を手動確認可能。

---

## Phase 4: User Story 2 - index.txt 以外は LAN へ一切露出しない (Priority: P1)

**Goal**: LAN リスナーが index.txt の GET/HEAD 以外(API・UI・書き込み系)へ定型エラーのみを返し、内部情報を漏らさないことを保証する

**Independent Test**: LAN 公開を有効にした状態で、別 PC 相当のクライアントから管理 UI・API 各パス・書き込み系メソッドへアクセスし、すべて拒否されることを確認する(spec US2 Independent Test)

### Tests for User Story 2(fail-first)⚠️

- [ ] T010 [US2] fail-first ネガティブシナリオを `tests/integration/index_lan.rs` に追加する: (1) LAN リスナーへの `/api/v1/status`・`/api/v1/settings`(PUT 含む)→ 404 `{"error":"not_found"}`、(2) `/`・静的アセットパス → 404 定型 JSON、(3) `POST /index.txt` → 405(空ボディ + `Allow` ヘッダ)、(4) 管理 HTTP・PCP 受け口の loopback 強制が不変、(5) URL 長 >1KB / ヘッダ合計 >8KB → 400 `{"error":"request_too_large"}`(spec US2 受入 1〜4、contract §1.1〜1.2)。**失敗することを確認**する

### Implementation for User Story 2

- [ ] T011 [US2] `build_index_router` に定型 404 fallback(`{"error":"not_found"}` JSON — contract §1.1)を追加し、URL 長 ≤1KB・ヘッダ ≤8KB の既存上限レイヤーが第 2 リスナーにも適用される構成にする(`src/web/mod.rs`。検証は T010 のテストで行う)

**Checkpoint**: US1 + US2 で「安全な LAN 公開」が成立(SC-003)。`cargo test --test index_lan` green。

---

## Phase 5: User Story 3 - 危険性を理解した上で有効化し、露出状態を常に自覚できる (Priority: P2)

**Goal**: UI 警告ゲート(明示確認なしに保存不可)、SecurityEvent `index_txt_lan_exposed` の記録、`GET /api/v1/status` での露出状態表示を実装する

**Independent Test**: 設定 UI で非 loopback 値の保存を試みて警告と明示確認を検証し、有効起動後のイベント記録と状態表示を確認する(spec US3 Independent Test)

### Tests for User Story 3(fail-first)⚠️

- [ ] T012 [US3] fail-first 契約テストを `tests/contract/local_api.rs` に追加する: (1) `GET /api/v1/settings` 応答に `index_bind`(既定 `""`、14 キー)、(2) `PUT` で受理値(private/loopback/リンクローカル/ULA)→ 200 + `restart_required: true` / `restart_keys: ["index_bind"]`、(3) `0.0.0.0` / グローバル / CGNAT → 400 `{"error":"non_lan_bind"}`、(4) ポート欠落・カンマ区切り複数 → 400 `{"error":"invalid_bind"}`、(5) `GET /api/v1/status` の `index_txt_lan` が無効時 `{enabled:false, bind:null, listening:false, error:null}` / 露出中 `{enabled:true, listening:true}`(contract §2〜3)。**失敗することを確認**する
- [ ] T013 [P] [US3] fail-first 契約テストを `tests/contract/cli_config.rs` に追加する: `--index-bind 192.168.1.10:7180` / `--index-bind=...` の受理、危険値(`0.0.0.0:7180` 等)の設定エラーによる起動拒否(検証エラーは fail-fast — contract §2.3)。**失敗することを確認**する
- [ ] T014 [P] [US3] fail-first 統合テストを `tests/integration/index_lan.rs` に追加する: (1) 非 loopback `index_bind` + bind 成功時に SecurityEvent `index_txt_lan_exposed` が 1 件(source = バインドアドレス)、(2) loopback 値では 0 件、(3) 機能無効(`index_bind` 空)でも 0 件(spec US3 受入 3・5、SC-001、contract §4)。**失敗することを確認**する

### Implementation for User Story 3

- [ ] T015 [P] [US3] `SecurityCategory::IndexTxtLanExposed`(`"index_txt_lan_exposed"`)を `src/security/mod.rs` に追加する(`ALL` 14→15、`as_str()`、網羅テスト更新。「違反の拒否でなく利用者が選んだ露出状態の監査」を doc コメントに明記 — research R4)
- [ ] T016 [P] [US3] `IndexLanStatus { bind: String, listening: bool, error: Option<&'static str> }` と `AppState.index_lan: Option<Arc<IndexLanStatus>>` を `src/web/mod.rs` に追加する(data-model §3)
- [ ] T017 [P] [US3] `web/settings.rs` の `BIND_KEYS` を 3→4(`index_bind` 追加)し、検証エラー写像に `NonLanBind` → 400 `"non_lan_bind"` を追加する
- [ ] T018 [US3] `src/main.rs` で `IndexLanStatus` を構築して `AppState` に注入し、**非 loopback かつ bind 成功**時のみ SecurityEvent `IndexTxtLanExposed` を 1 件記録する(loopback 値・機能無効では記録しない — T015/T016 に依存)
- [ ] T019 [US3] `web/announced.rs` の `GET /api/v1/status` 応答に `index_txt_lan` オブジェクト(`enabled` / `bind` / `listening` / `error` — contract §3)を追加する(T016 に依存)
- [ ] T020 [P] [US3] `ui/settings.html` に `index_bind` 入力欄を追加し、JS 側 `BIND_KEYS` へ追加(再起動要求の注記対象)。保存時に非空かつ非 loopback(`127.` / `[::1]` 接頭辞判定 — バックエンド正規判定の**安全側近似**: 過剰警告は許容、過小警告は不可)なら警告 1 項目(「平文」「無認証」「取得・改ざんされうる」の 3 要素必須 — contract §5)とチェックボックスを表示し、チェックされるまで PUT 送信をブロックする(research R6)。UI ゲートの自動 DOM テストは課さず手動検証とする(ADR-0012「Principle IV の適用範囲」・quickstart §6 = T024)

**Checkpoint**: `cargo test --test local_api --test cli_config --test index_lan` green。SC-004・SC-006 が検証可能。

---

## Phase 6: User Story 4 - LAN 公開に失敗しても本体は止まらない (Priority: P3)

**Goal**: 第 2 リスナーの bind 失敗を致命エラーとせず、警告ログ + 状態表示への失敗理由反映のみで本体を継続稼働させる

**Independent Test**: `index_bind` に競合ポートや存在しないアドレスを与えて起動し、本体機能の継続稼働と状態表示への失敗理由反映を確認する(spec US4 Independent Test)

### Tests for User Story 4(fail-first)⚠️

- [ ] T021 [US4] fail-first 統合テストを `tests/integration/index_lan.rs` に追加する: (1) `index_bind` を管理ポートと同一(bind 競合)にして起動 → 本体(loopback UI/API)は稼働継続、`index_txt_lan` が `{enabled:true, listening:false, error:"addr_in_use"}`、警告ログ記録、SecurityEvent は 0 件、(2) 存在しないアドレス → `error:"addr_not_available"` で同様に継続(spec US4 受入 1〜3、Edge Cases)。**失敗することを確認**する

### Implementation for User Story 4

- [ ] T022 [US4] `src/main.rs` の第 2 リスナー bind 失敗時の縮退継続を実装する: `bind_error()`(即終了)を使わず `tracing::warn!` + `IndexLanStatus { listening: false, error: Some(code) }` を注入して起動続行。`ErrorKind` → 定型コード写像(`addr_in_use` / `permission_denied` / `addr_not_available` / `unknown` — research R3)。既存 3 受け口の fail-fast は不変(FR-007)

**Checkpoint**: 全ストーリー完了。SC-005 が検証可能。

---

## Phase 7: Polish & Cross-Cutting Concerns

**Purpose**: セキュリティレビュー・回帰確認・end-to-end 検証

- [ ] T023 [P] セキュリティレビューチェックリスト(`docs/adr/security-review-checklist.md`)を本変更へ適用し、結果を `specs/004-lan-index-txt/checklists/security.md` に記録する(Principle III MUST — plan Constitution Check)
- [ ] T024 [P] `specs/004-lan-index-txt/quickstart.md` の手動検証(SC-001〜SC-006、Windows ファイアウォールプロンプトの案内確認を含む)を実施する。§6 の UI 警告ゲート手順は FR-005 の正式な検証手段である(ADR-0012「Principle IV の適用範囲」)
- [ ] T025 [P] `CONTEXT.md` の信頼境界表に「index.txt(オプトイン時): LAN」を追記する(ADR-0012 帰結)
- [ ] T026 回帰・品質ゲート: `cargo fmt -- --check`、`cargo clippy`、`cargo test`(全テスト)を通過させる。`index_bind` 未設定時の外部観測挙動が現行と完全一致すること(SC-001 — 既存テストの green で担保)を確認する

---

## Dependencies & Execution Order

### Phase Dependencies

- **Phase 1(Setup/ゲート)**: 依存なし。**T001 の ADR-0012 承認が全実装の前提**
- **Phase 2(Foundational)**: Phase 1 完了後。全ユーザーストーリーをブロックする
- **Phase 3〜6(US1〜US4)**: Phase 2 完了後。優先度順(P1 → P1 → P2 → P3)の逐次実行を推奨
  - US2 は US1 の `build_index_router`(T008)に依存(fallback の追加先)
  - US3 は US1 の第 2 リスナー起動(T009)に依存(イベント記録・状態注入の挿入点)
  - US4 は US1 の T009 と US3 の T016(`IndexLanStatus`)に依存
- **Phase 7(Polish)**: 全ストーリー完了後

### Within Each User Story

- テストを先に書き、**失敗を確認してから**実装する(constitution Principle IV MUST)
- モデル/型(T015・T016)→ 統合(T018・T019)の順

### Parallel Opportunities

- **US3 のテスト**: T013・T014 は T012 と別ファイルのため並列可
- **US3 の実装**: T015(security/mod.rs)・T016(web/mod.rs)・T017(web/settings.rs)・T020(ui/settings.html)は互いに別ファイルで並列可。T018・T019 はそれらの完了後
- **Polish**: T023・T024・T025 は並列可(T026 はそれらの後)

---

## Parallel Example: User Story 3

```bash
# fail-first テストを並列で作成(T012 完了後):
Task: "T013 契約テスト --index-bind in tests/contract/cli_config.rs"
Task: "T014 統合テスト SecurityEvent 記録条件 in tests/integration/index_lan.rs"

# 実装タスクを並列で開始:
Task: "T015 SecurityCategory::IndexTxtLanExposed in src/security/mod.rs"
Task: "T016 IndexLanStatus + AppState.index_lan in src/web/mod.rs"
Task: "T017 BIND_KEYS + NonLanBind 写像 in src/web/settings.rs"
Task: "T020 警告ゲート UI in ui/settings.html"
```

---

## Implementation Strategy

### MVP First(User Story 1 のみ)

1. Phase 1: **ADR-0012 の承認を得る**(T001 — これなしに着手しない)
2. Phase 2: Foundational(検証ゲート)完了
3. Phase 3: US1 完了 → `cargo test --test index_lan` で独立検証
4. **停止して検証**: 別 PC からの index.txt 取得(SC-002)をデモ可能

### Incremental Delivery

1. Setup + Foundational → 検証基盤完成
2. US1 → LAN 配信が動く(MVP)
3. US2 → 攻撃面の限定が保証され「安全な LAN 公開」が成立(P1 完結)
4. US3 → 警告ゲート・監査・状態表示
5. US4 → 縮退継続(運用品質)
6. Polish → セキュリティレビュー記録 + quickstart 検証 + CONTEXT.md 追記

---

## Notes

- [P] = 別ファイル・依存なし。同一ファイル(`src/config.rs`・`src/main.rs`・`tests/integration/index_lan.rs`)を触るタスクは逐次実行
- 各タスク(または論理グループ)ごとにコミットし、コミット前に `cargo fmt -- --check` を実行する(プロジェクト CLAUDE.md)
- 既存 `yp/index_txt.rs` は**変更しない**(再マウントのみ — plan)
- エラー応答は内部情報を含めない定型のみ(constitution Security Requirements)

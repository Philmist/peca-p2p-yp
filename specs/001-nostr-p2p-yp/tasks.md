# Tasks: 分散型配信情報共有ネットワーク(YP代替)

**Input**: Design documents from `/specs/001-nostr-p2p-yp/`

**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/, quickstart.md(すべて生成済み)

**Tests**: constitution Principle IV(テストファースト MUST)により**テストタスクは必須**。
各ストーリーの Gherkin/契約テストは実装前に作成し、**失敗することを確認してから**実装に着手する。

**Organization**: ユーザーストーリー単位でフェーズ化し、各ストーリーが独立して実装・検証可能。
US4(実況コメント)は将来フェーズのため v1 タスクなし(識別子互換は contracts/nostr-events.md で確保済み)。

## Format: `[ID] [P?] [Story] Description`

- **[P]**: 並列実行可(異なるファイル・未完了タスクへの依存なし)
- **[Story]**: 対応するユーザーストーリー(US1/US2/US3)

---

## Phase 1: Setup (Shared Infrastructure)

**Purpose**: プロジェクト初期化と開発基盤

- [ ] T001 Cargo プロジェクト作成とモジュール骨格: `Cargo.toml`(tokio/axum/nostr-sdk/rusqlite/windows/encoding_rs/tracing/cucumber ほか plan.md 記載の依存)、`src/main.rs`、`src/{config.rs, pcp/mod.rs, yp/mod.rs, nostr/mod.rs, identity/mod.rs, store/mod.rs, web/mod.rs, security/mod.rs}`、`ui/`、`tests/{features/, contract/, integration/}` を作成しビルドが通る状態にする
- [ ] T002 [P] rustfmt / clippy 設定(`rustfmt.toml`、`Cargo.toml` の lints で warnings deny)と `.gitignore` 追記(`target/`、`*.db`)
- [ ] T003 [P] CI ワークフロー `.github/workflows/ci.yml`: `windows-latest` ランナーで build + test + clippy を実行(DPAPI/`windows` クレート依存のため Windows 必須 — FR-009)+ `cargo audit`(+ ADR-0001 準拠の Trivy スキャン)— Principle III
- [ ] T004 [P] cucumber テストハーネス: `tests/cucumber.rs` と `Cargo.toml` の `[[test]]` 定義。空の `tests/features/` でコンパイル・実行できること — Principle IV

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: 全ストーリーが依存する基盤+constitution 実装前ゲート(ADR)

**⚠️ CRITICAL**: このフェーズ完了までユーザーストーリー実装を開始しない

- [ ] T005 [P] ADR 作成 `docs/adr/0002-nostr-event-model.md`: NIP-53 kind 30311 採用・タグ写像・鮮度管理(research R1, R2)。FR-004 トラッカー解決の充足方式(index.txt TIP 経由。PCP tracker lookup は v1 対象外)の判断記録を含める(plan.md 参照)。原則参照必須(Principle II, VI)
- [ ] T006 [P] ADR 作成 `docs/adr/0003-persona-key-management.md`: DPAPI 鍵保管・エクスポート方針・ペルソナ破棄の非可逆性(research R6、FR-013、Principle II)。リレー運営者から見たペルソナ間リンク可能性(同一 IP・同一リレー接続メタデータ)の限界と非目標の明記を含める(constitution PRIVACY TODO の一部消化)。30311 `relays` タグ(自ノードの write リレーリスト公開)が複数ペルソナのリンク推定材料となるリスクの扱い(タグ省略・ペルソナ別リレーセット等の要否)も判断・記録する — checklists/security.md CHK014
- [ ] T007 [P] ADR 作成 `docs/adr/0004-threat-model-sybil-mitigation.md`: 多層緩和(署名検証/ミュート/リレー切離し/任意 PoW)と閾値方針(research R8、FR-008、constitution Security Requirements)。URL 以外の内容ベース警告の要否と v1 スコープの確定を含める(FR-012)
- [ ] T008 [P] ADR 作成 `docs/adr/0005-formal-verification-scope.md`: クリティカル該当なしの判断理由(PCP は既存プロトコル準拠で対象外、集約は LWW で自明 — Principle V の MUST)
- [ ] T051 [P] セキュリティレビュー観点チェックリスト作成 `docs/security-review-checklist.md`: Principle II 由来の観点(入力検証の有無・エラー応答の内部情報漏洩・最小権限・既存暗号ライブラリの利用・セキュリティイベントログ・レート制限)を列挙し、セキュリティに関わる PR で適用結果を記録する運用手順を明記 — constitution 実装中ゲート 6(Principle II, III)。※ ID は追記順のため Phase 2 に配置
- [ ] T009 SQLite ストア実装 `src/store/mod.rs` + `src/store/schema.sql`: personas / relays / mutes / settings テーブル(data-model.md 準拠)と CRUD、`%APPDATA%\peca-p2p-yp\app.db` 配置
- [ ] T010 設定管理 `src/config.rs`: Settings 既定値(pcp_bind=127.0.0.1:7146, http_bind=127.0.0.1:7180, freshness=600s, republish=60s, min_pow_bits=0, index_txt_encoding=shift_jis)の読込・保存(T009 依存)
- [ ] T011 セキュリティ共通部 `src/security/mod.rs`: 入力検証ヘルパ(サイズ/制御文字/URL)、SecurityEvent カテゴリ定義、tracing ファイル出力、URL 警告判定(http/https 以外)— Principle II、FR-012
- [ ] T012 Web 骨格 `src/web/mod.rs`: axum ルーター、Host ヘッダ検証、`X-Api-Token` ミドルウェア、レート制限(tower)、定型エラー応答(内部情報漏洩禁止)、`ui/` 静的アセット埋め込み、`ui/index.html` シェル(contracts/local-api.md 保護方針)
- [ ] T013 起動配線 `src/main.rs`: 設定読込→store→web→(後続で pcp/nostr)の起動監視と graceful shutdown

**Checkpoint**: 基盤完成 — ユーザーストーリー実装を開始できる

---

## Phase 3: User Story 1 - 配信者によるチャンネル掲載 (Priority: P1) 🎯 MVP

**Goal**: PeerCastStation から PCP で受けたチャンネル情報を、選択したペルソナで複数リレーへ kind 30311 として掲載する

**Independent Test**: PCP 疑似クライアント(またはモックリレー)で announce→掲載イベント発行→終了イベントまでを単独検証(quickstart 手順 3)

### Tests for User Story 1(実装前に作成し失敗を確認)⚠️

- [ ] T014 [P] [US1] Gherkin `tests/features/us1_announce.feature` + ステップ骨格: spec US1 受け入れシナリオ 1〜3(60 秒以内掲載・更新反映・ended)を記述し失敗状態にする
- [ ] T015 [P] [US1] PCP 契約テスト `tests/contract/pcp_handshake.rs`: HELO→OLEH→BCST→QUIT のフィクスチャバイト列往復+ネガティブ(atom ネスト深さ >8、64KB 超ペイロード、不正 GUID → 切断+`pcp_reject`)— contracts/pcp-announce.md、spec セキュリティシナリオ 1(PCP 側)
- [ ] T016 [P] [US1] nostr イベント契約テスト `tests/contract/nostr_event_30311.rs`: AnnouncedChannel→30311 タグ写像ゴールデン(必須タグ、peca 拡張タグ、expiration、ended、firewalled 時 tip 省略)— contracts/nostr-events.md

### Implementation for User Story 1

- [ ] T017 [P] [US1] PCP atom コーデック `src/pcp/atom.rs`: 符号化/復号、ネスト深さ・サイズ上限の強制(unit テスト同梱)
- [ ] T018 [US1] PCP announce セッション `src/pcp/session.rs`: 状態機械(announced→updating⇄…→ended)、HELO/OLEH、BCST 解析、loopback 外接続拒否、受信レート上限(T017 依存)
- [ ] T019 [US1] AnnouncedChannel レジストリ `src/pcp/channel.rs`: data-model.md の検証ルール適用(文字列長・制御文字・数値範囲)とメモリ管理(T018 依存)
- [ ] T020 [P] [US1] ペルソナコア `src/identity/mod.rs`: 鍵生成(nostr-sdk)、DPAPI 暗号化/復号(`windows` クレート)、store CRUD、チャンネルへの割当(T009 依存、ADR-0003 準拠)
- [ ] T021 [US1] 30311 ビルダー `src/nostr/event30311.rs`: contracts/nostr-events.md のタグ写像・content 空・NIP-40 expiration(T019, T020 依存)
- [ ] T022 [US1] リレープール `src/nostr/relays.rs`: nostr-sdk クライアント管理、store の relays(enabled/read/write)反映、URL 検証(wss 推奨/ws 警告/他拒否、上限 50)
- [ ] T023 [US1] 掲載エンジン `src/nostr/publish.rs`: 変更即時発行+60 秒周期再発行+終了時 `status=ended` 最終発行(+NIP-09 併用)、ペルソナ署名(T021, T022 依存)
- [ ] T024 [US1] 配線と状態 API: `src/main.rs` に PCP→publish 接続、`src/web/status.rs` に `GET /api/v1/announced`・`GET /api/v1/status`(基本形)(T013, T023 依存)
- [ ] T025 [P] [US1] ペルソナ API `src/web/personas.rs`: GET/POST/PUT/DELETE + export(nsec 表示は明示操作+警告)— contracts/local-api.md(T020 依存)
- [ ] T026 [P] [US1] リレー API `src/web/relays.rs`: GET/POST(貼り付け一括登録、不正 URL 個別エラー)/PUT/DELETE/export(1 行 1 URL)— research R10(T022 依存)
- [ ] T027 [US1] UI ページ `ui/`: ペルソナ管理(現在選択の常時明示=誤爆防止)・リレー管理(貼り付け登録/書き出し)・掲載中一覧(掲載成功リレー数表示)(T024–T026 依存)
- [ ] T028 [US1] US1 統合: `tests/integration/announce_flow.rs`(PCP 疑似クライアント→モックリレーで掲載〜ended まで)+ T014 の cucumber を green にする

**Checkpoint**: 掲載側が単独で機能。モックリレーで US1 の全受け入れシナリオがパス

---

## Phase 4: User Story 2 - 視聴者によるチャンネル発見と視聴開始 (Priority: P1)

**Goal**: リレーから 30311 を購読・検証してチャンネル一覧を構築し、UI と index.txt(YP ブラウザ互換)で供給する

**Independent Test**: モックリレーに既知イベントを投入し、一覧 5 秒以内表示・index.txt ゴールデン一致・不正イベント不可視を単独検証(quickstart 手順 4)

### Tests for User Story 2(実装前に作成し失敗を確認)⚠️

- [ ] T029 [P] [US2] Gherkin `tests/features/us2_discover.feature`: spec US2 シナリオ 1〜3+ネガティブ(16KB 超イベント破棄=`nostr_oversize`、署名不正不可視=`nostr_invalid_sig`、未来時刻破棄、時計スキュー境界=±300 秒での鮮度・expiration 判定が破綻しないこと)— spec セキュリティシナリオ 1・2(nostr 側)、SC-005/SC-007、spec Edge Case「時計のずれ」
- [ ] T030 [P] [US2] index.txt 契約テスト `tests/contract/index_txt.rs`: 17 フィールドゴールデン比較(Shift_JIS/UTF-8、空一覧、firewalled=TIP 空、名称内 `<>` 除去、BROADCAST_TIME 形式)— contracts/http-yp.md
- [ ] T031 [P] [US2] モックリレー `tests/integration/mock_relay.rs`: インプロセス WebSocket リレー(EVENT/REQ/EOSE 最小実装、任意イベント投入ヘルパ)

### Implementation for User Story 2

- [ ] T032 [US2] 受信検証パイプライン `src/nostr/validate.rs`: contracts/nostr-events.md の 7 段階(サイズ→署名→形式→時刻→内容→PoW(後続 T045)→URL 警告フラグ)+セキュリティログ(T011 依存)
- [ ] T033 [US2] 購読エンジン `src/nostr/subscribe.rs`: kind 30311 フィルタで複数リレー購読、受信を検証パイプラインへ(T022, T032 依存)
- [ ] T034 [US2] 一覧集約 `src/nostr/listing.rs`: `(author_pubkey, channel_id)` キーの last-write-wins、鮮度窓(600 秒)超過・ended の自動除去、source_relays 記録 — FR-006(T033 依存)
- [ ] T035 [US2] index.txt 生成 `src/yp/index_txt.rs`: 17 フィールド組立、Shift_JIS/UTF-8 出力(encoding_rs、変換不能は `?`)(T034 依存)
- [ ] T036 [US2] index.txt ルート `src/web/index_txt.rs`: `GET /index.txt`(GET/HEAD のみ、レート制限 10 req/秒、定型エラー)(T012, T035 依存)
- [ ] T037 [US2] ミュート機能: `src/store/mod.rs` の mutes CRUD 利用+`src/web/mutes.rs`(GET/POST/DELETE)+listing への適用(表示除外のみ)— FR-008、公開しない(T034 依存)
- [ ] T038 [US2] チャンネル一覧 API+UI: `src/web/channels.rs` `GET /api/v1/channels`(url_warning フラグ付与)、`ui/` 一覧ページ(警告表示・ミュート操作・未検証リンクを開く前の確認・firewalled チャンネルの「直接視聴不可」バッジ表示 — spec Edge Case「視聴可否が視聴者に分かる」)— FR-012(T034, T037 依存)
- [ ] T039 [US2] US2 統合: `tests/integration/discover_flow.rs`(US1 掲載→モックリレー→発見→index.txt までの E2E)+ T029 の cucumber を green にする

**Checkpoint**: US1+US2 で「掲載→発見→YP ブラウザ表示」の一連が成立(コア価値実証)

---

## Phase 5: User Story 3 - 共有先障害時の継続性 (Priority: P2)

**Goal**: リレーの一部/全部の障害時に掲載・発見が継続し、全断時は通知と自動再開を行う

**Independent Test**: モックリレー 2 台構成で 1 台停止→継続、全停止→バナー→復旧→自動再開を単独検証(quickstart 手順 5)

### Tests for User Story 3(実装前に作成し失敗を確認)⚠️

- [ ] T040 [P] [US3] Gherkin `tests/features/us3_resilience.feature`: spec US3 シナリオ 1〜3(1 リレー停止で継続、応答なしリレーの影響は遅延のみ、全断通知+自動再開)— SC-002

### Implementation for User Story 3

- [ ] T041 [US3] リレー健全性 `src/nostr/relays.rs` 拡張: last_ok_at 更新、指数バックオフ再接続、切断リレーへの発行スキップ(掲載は残存リレーで継続)(T023, T033 依存)
- [ ] T042 [US3] 全断検知と自動再開 `src/nostr/publish.rs`+`src/web/status.rs`: 全リレー到達不能フラグを `GET /api/v1/status` に反映、回復時に掲載を自動再開(T041 依存)
- [ ] T043 [US3] UI: 到達不能バナー(目立つ表示+回復時の自動再開表示)とリレー健全性表示(last_ok_at)を `ui/` に追加(T042 依存)
- [ ] T044 [US3] US3 統合: `tests/integration/resilience.rs`(モックリレー停止/再開シミュレーション)+ T040 の cucumber を green にする

**Checkpoint**: 全ストーリーが独立して機能。単一障害点排除(本機能の動機)を自動テストで実証

---

## Phase 6: Polish & Cross-Cutting Concerns

**Purpose**: 横断的なセキュリティ強化・文書・受け入れ検証

- [ ] T045 NIP-13 PoW 受信フィルタ: `src/nostr/validate.rs` に min_pow_bits 適用(コミット難易度検証)+設定 UI(既定 0=無効)— research R8、ADR-0004 準拠(T032 依存)
- [ ] T046 大量偽登録耐性シナリオ `tests/features/security_spam.feature`: spec セキュリティシナリオ 3(大量偽チャンネル投入下でミュート/リレー切離し/PoW により一覧の実用性維持)を green にする(T037, T045 依存)
- [ ] T047 [P] README.md: 配布 exe 入手→起動→リレー貼り付け→一覧閲覧の手順書(SC-006: 15 分以内セットアップの根拠文書)
- [ ] T048 quickstart.md 実機受け入れ検証(手動): 実機 PeerCastStation からの掲載、ユーザー所有 YP ブラウザでの index.txt 表示確認(Shift_JIS 文字化け有無 — research R5 リスク解消)、2 ノード間 60 秒以内反映(SC-001, SC-003)、README(T047)手順に従ったクリーン環境での配布物入手→リレー登録→一覧閲覧の所要時間実測 15 分以内(SC-006)
- [ ] T049 性能検証 `tests/integration/perf.rs`: チャンネル 2,000 件時の一覧 API・index.txt 応答と鮮度除去の劣化なし、メモリ < 150MB 確認(plan.md Scale/Scope)
- [ ] T050 リリース前セキュリティ最終確認: 全 Gherkin(ネガティブ含む)パス、`cargo audit` クリーン、エラー応答の内部情報漏洩なし、レート制限動作、constitution リリース前ゲート(8〜10)照合結果を記録

---

## Dependencies & Execution Order

### Phase Dependencies

- **Phase 1 (Setup)**: 依存なし
- **Phase 2 (Foundational)**: Phase 1 完了後。**全ユーザーストーリーをブロック**。ADR(T005–T008)は constitution 実装前ゲートのため、コード実装(T017 以降)より先に完了必須
- **Phase 3 (US1)**: Phase 2 完了後
- **Phase 4 (US2)**: Phase 2 完了後(独立検証はモックリレーで可。E2E テスト T039 のみ US1 の掲載機能を利用)
- **Phase 5 (US3)**: Phase 2 完了後(実質的には US1/US2 のリレー接続部 T023/T033 の存在が前提)
- **Phase 6 (Polish)**: 対象ストーリー完了後

### User Story Dependencies

- **US1 (P1)**: 基盤のみに依存 — MVP
- **US2 (P1)**: 基盤+モックリレーで独立検証可。統合 E2E(T039)は US1 と接続
- **US3 (P2)**: US1/US2 のリレー接続コンポーネントを拡張
- **US4**: v1 タスクなし(将来フェーズ。識別子互換は契約で固定済み)

### Within Each User Story

- テスト(Gherkin+契約)→ 失敗確認 → モデル → サービス → API/UI → 統合 green の順(Principle IV)

### Parallel Opportunities

- Phase 1: T002 / T003 / T004
- Phase 2: T005 / T006 / T007 / T008 / T051(ADR 4 本+チェックリスト並列)
- US1: T014 / T015 / T016(テスト 3 本)→ T017 / T020(異なるモジュール)→ T025 / T026(API 2 本)
- US2: T029 / T030 / T031(テスト+モックリレー 3 本)
- Phase 2 完了後、US1 と US2 は別担当者で並列進行可能(T039 の E2E のみ US1 完了待ち)

## Parallel Example: User Story 1

```text
# テストを同時に作成(すべて失敗状態を確認):
Task: T014 Gherkin tests/features/us1_announce.feature
Task: T015 PCP 契約テスト tests/contract/pcp_handshake.rs
Task: T016 nostr 契約テスト tests/contract/nostr_event_30311.rs

# 独立モジュールを同時に実装:
Task: T017 PCP atom コーデック src/pcp/atom.rs
Task: T020 ペルソナコア src/identity/mod.rs
```

## Implementation Strategy

### MVP First (US1 のみ)

1. Phase 1 → Phase 2(ADR 含む)完了
2. Phase 3(US1)完了 → モックリレーで掲載検証
3. **STOP & VALIDATE**: quickstart 手順 3 相当を実施

※ 利用者に見える価値(一覧閲覧)は US2 と対で成立するため、実運用デモは Phase 4 完了時点を推奨。

### Incremental Delivery

1. Setup + Foundational → 基盤完成(ADR 4 本で設計判断確定)
2. + US1 → 掲載成立(MVP)
3. + US2 → 掲載→発見→YP ブラウザ表示のコア価値実証
4. + US3 → 単一障害点排除の実証(本機能の動機)
5. + Polish → PoW・性能・実機受け入れ・リリース前ゲート

## Notes

- タスク完了ごと(または論理的まとまりごと)にコミットする。コミットメッセージは実行後に確認する(ユーザー CLAUDE.md)
- 著作権表記を追加する場合はコミット前にユーザーへ確認する(ユーザー CLAUDE.md)
- PeerCastStation(GPLv3)のコードは参照・複製しない(research R9 のクリーンルーム方針)
- すべてのセキュリティ関連実装は ADR-0004 の脅威モデルと contracts/ の検証規則に従う

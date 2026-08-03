# Tasks: Web ブラウザからの実況スレ書き込み(旧来 BBS 互換 UI・LAN 公開・固定 >>1)

**Input**: Design documents from `/specs/007-web-bbs-write/`

**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/, quickstart.md

**Tests**: 含む(必須)。憲法 Principle IV により、シナリオに対応するテストが**失敗する状態を
確認してから**実装を開始しなければならない (MUST)。セキュリティ要件はネガティブシナリオを
含む Gherkin + 自動テストで検証する。

**Organization**: ユーザーストーリー単位(US1 = 旧来 BBS 互換 Web UI / US2 = LAN 公開 /
US3 = 固定 >>1)。すべて P1 だが各ストーリーは独立に実装・検証可能。

## Format: `[ID] [P?] [Story] Description`

- **[P]**: 並列実行可(異なるファイル・未完了タスクへの依存なし)
- **[Story]**: US1 / US2 / US3(spec.md のユーザーストーリー対応)

## Path Conventions

単一 Rust プロジェクト(リポジトリルートの `src/`・`tests/`・`ui/`・`docs/`)。

---

## Phase 1: Setup

**Purpose**: ベースライン確認と実装前ゲート(憲法: 実装前ゲート 1〜4)

- [X] T001 ベースライン確認 — `cargo fmt -- --check` / `cargo clippy --all-targets` / `cargo test` が現行ブランチで全パスすることを確認し、失敗があれば本機能着手前に記録する
- [X] T002 [P] ADR-0015 草案を docs/adr/0015-web-bbs-lan-write.md に作成(Status: Draft)— 書き込みを含む LAN 公開の許可リスト方式 + 2 要素オプトイン + 監査、互換 API 無認証 LAN 書き込みの受容と補償、LAN 内平文トークン共有の受容リスク、Principle V 非該当判断。参照原則 I・II を明記(research.md R9)

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: 006 で定義済み・未配線の板設定永続化を production 配線する(US1 の設定表示・US3 のテンプレ永続化が依存)

**⚠️ CRITICAL**: US3 着手前に完了必須(US1/US2 は論理的には依存しないが、板設定の読み書き経路を先に固める)

- [X] T003 board_settings 永続化の production 配線 — src/main.rs(起動時に store から板設定を読み込み registry へ反映)と src/web/ の板設定 PUT 適用経路(適用成功時に `set_board_settings` で保存)を接続する。既存テスト(src/store/mod.rs:1389-1414)を壊さないこと

**Checkpoint**: 板設定が再起動をまたいで保持される — ユーザーストーリー実装開始可

---

## Phase 3: User Story 1 - 旧来 BBS 互換の Web UI で読み書きする (Priority: P1) 🎯 MVP

**Goal**: `ui/livechat.html` を参考画像 `_alt` 相当の伝統的 2ch 風掲示板(板ページ + スレページ)へ全面刷新する。書き込み経路・検証は 006 のまま(FR-001〜FR-006)

**Independent Test**: 稼働端末 1 台・既定設定(loopback)で quickstart V-1 が成立する(スレ一覧・スレ立て・伝統形式レス列・`>>n` アンカー・書き込み反映・安全描画)

### Tests for User Story 1(先に書き、失敗を確認)⚠️

- [X] T004 [P] [US1] contract テスト追加: 板詳細 API(`GET /api/v1/livechat/threads/{board}`)レスポンスに `compat_bbs_port`(互換 API 有効時はポート番号・無効時は null)が含まれることを tests/contract/local_api.rs に追加し、失敗を確認する(contracts/web-ui.md §5.1)
- [ ] T039 [P] [US1] FR-005(安全描画)の自動テスト追加(/speckit-analyze C1 対応): tests/features/safe_rendering.feature に Gherkin を追加し対応ステップを cucumber ハーネスに実装する — (a) ローカルルール Markdown への `<script>`/生 HTML 注入が板設定 API の `local_rules_html` で無害化される(既存の src/web/livechat.rs:759-873 単体テストを統合面から補完)、(b) レス本文・名前・固定 >>1 テンプレに HTML/スクリプトを含めても API 応答はプレーンテキストのまま往復し、サーバ側で HTML 化されない。クライアント側エスケープ(T008/T009 の描画)は実機確認 T012 で検証済みのため、その検証範囲の判断と根拠を ADR-0015(T035)に記録する(憲法 Principle IV: セキュリティ要件の Gherkin + 自動テスト)

### Implementation for User Story 1

- [X] T005 [US1] 板詳細 API に `compat_bbs_port` を追加する — src/web/livechat.rs(レスポンス構造体)と src/main.rs(`compat_bbs_bind` のポートを AppState/アダプタ経由で受け渡す)。T004 をパスさせる
- [X] T006 [US1] ui/livechat.html にハッシュルーティング骨格を実装する — `#/`(板選択)・`#/board/{board_id}`(板ページ)・`#/thread/{board_id}/{thread_key}`(スレページ)の 3 ビュー切替。既存のトークン取得(`GET /api/v1/token`)・`api()` ヘルパは温存(contracts/web-ui.md §1)
- [X] T007 [US1] 板ページを実装する — 板タイトル帯・ローカルルール掲示(サーバ生成 `local_rules_html` のみ挿入)・スレ一覧(`{連番}: {タイトル} ({レス数})` → クリックでスレページ)・新規スレ作成欄・専ブラ向け板 URL の動的生成(`location.hostname` + `compat_bbs_port`、null 時非表示)(contracts/web-ui.md §2)
- [X] T008 [US1] スレページのレス列を実装する — `{res_no} :{名前}:{日付} ID:{id}` + 本文の伝統形式、名前空欄は `noname_name` 表示、各レスに `id="res-{n}"`、欠番は詰めない、既定は最新 50 のクライアント側スライス + 「全部読む」/「最新50」切替(contracts/web-ui.md §3.2/3.4)
- [X] T009 [US1] `>>n` アンカーを実装する — 本文をテキストエスケープ後に `>>数字` をアンカー化。クリックでページ内ジャンプ(範囲外なら全件表示へ切替後ジャンプ)、ホバー/タップでインラインポップアップ、未確定・欠番・範囲外は「解決不能」表示(contracts/web-ui.md §3.3、FR-002/FR-005)
- [X] T010 [US1] 書き込み欄と追従表示を実装する — 名前/メール/本文欄 + 既存 `POST .../write`(202 受理)、8 秒ポーリング + 明示更新ボタン、既存の管理操作(モデレーション・板設定・join/leave/next/close)は `<details>` 折りたたみで温存(contracts/web-ui.md §3.5〜3.7)
- [X] T011 [US1] 旧来スキン CSS を適用する — 参考 assets/bbsjpnkn_board_alt.png / assets/bbsjpnkn_thread_last50_alt.png 相当(レンガ調背景は CSS/データ URI、緑帯、伝統的配色)。外部アセット参照なし(research.md R5)
- [X] T012 [US1] quickstart V-1 の実機確認を実施し結果を記録する — 板ページ構成・レス列形式・アンカー動作・書き込み反映・`<script>` 注入がテキスト表示されること(SC-001、FR-005 ネガティブ)

**Checkpoint**: US1 単独で「掲示板そのもの」の読み書き体験が成立(MVP)

---

## Phase 4: User Story 2 - 視聴端末(別端末)から書き込む (Priority: P1)

**Goal**: `http_bind` / `compat_bbs_bind` の LAN 公開(許可リスト + 2 要素オプトイン + 監査 + 保護非緩和)により、同一 LAN 上の別端末のブラウザ・専ブラから読み書きできるようにする(FR-007〜FR-012、FR-020〜FR-022)

**Independent Test**: 稼働端末 + 別端末の 2 台で quickstart V-3/V-4/V-5 が成立する(LAN からの読み書き・拒否マトリクス・SecurityEvent 記録・保護同一水準)

### Tests for User Story 2(先に書き、失敗を確認)⚠️

- [X] T013 [P] [US2] contract テスト追加: bind 検証 — グローバル/`0.0.0.0`/`::`/CGNAT(100.64.0.0/10)拒否、v4-mapped プライベートの正規化許可、非 loopback + 同意キー false の設定エラー、loopback は同意キー不要 — を tests/contract/cli_config.rs に追加し、失敗を確認する(contracts/lan-exposure.md §2)
- [X] T014 [P] [US2] contract テスト追加: Web UI 面の Host ホワイトリスト — 非 loopback bind 時に `{bind_ip}:{port}` 許可・ホワイトリスト外/欠落 Host は 403 — を tests/contract/local_api.rs に追加し、失敗を確認する(contracts/lan-exposure.md §3)
- [X] T015 [P] [US2] contract テスト追加: 互換 API 面の Host 検証・許可範囲外送信元 403・レート上限が loopback と同一(20 req/秒)であることを tests/contract/compat_bbs.rs に追加し、失敗を確認する(contracts/lan-exposure.md §4/§5)
- [X] T016 [P] [US2] integration テスト新規作成: tests/integration/lan_write.rs(Cargo.toml へ `[[test]] name = "lan_write"` 追加)— (a) 非 loopback 待受成功時の SecurityEvent `WebUiLanExposed` / `CompatBbsLanExposed` 各 1 件記録、(b) LAN アドレス経由の読み書き成立(書き込みに必要なトークンの LAN 経由取得 `GET /api/v1/token` を含む)、(c) 別端末相当(非 loopback 送信元)からの書き込みがホスト採番を経て**全ノードで同一 res_no・同一並び順・同一アンカー解決**に確定すること(FR-012 / SC-007 の自動化)、(d) 互換 API 経由の LAN 書き込みが 006 と同一検証(板鍵署名・名前欄 `#` 除去・サイズ上限・PoW/レート)を通過して確定すること(FR-022 の肯定シナリオ)。失敗を確認する
- [X] T017 [P] [US2] cucumber シナリオ追加: tests/features/lan_exposure.feature に contracts/lan-exposure.md §7 の Gherkin(拒否 4 種・正規化・監査・Host 拒否・送信元拒否・レート同一)を追加し、対応ステップを tests/ の cucumber ハーネスに実装、失敗を確認する

### Implementation for User Story 2

- [X] T018 [US2] src/config.rs を拡張する — `http_bind`/`compat_bbs_bind` の検証を `require_loopback` から `require_lan_or_loopback` へ変更、`http_lan_consent`/`compat_bbs_lan_consent`(bool・既定 false)のキー定数・load/save・`Settings::validate`(非 loopback かつ同意 false は設定エラー)・`CliOverrides` 対応を追加する。T013 をパスさせる(data-model.md「Settings(拡張)」)
- [X] T019 [P] [US2] src/security/mod.rs に `SecurityCategory::WebUiLanExposed` / `CompatBbsLanExposed` を追加する(21→23 カテゴリ、data-model.md)
- [X] T020 [US2] src/web/mod.rs を拡張する — allowed_hosts 生成を「loopback 3 形式 + 非 loopback bind 時 `{bind_ip}:{port}`(IPv6 は `[{ip}]:{port}`)」へ拡張し、非 loopback 待受時の送信元 IP 許可リスト検証ミドルウェア(`to_canonical()` 正規化 → loopback/RFC1918/リンクローカル/ULA 以外は 403 + ログ)を追加する。T014 をパスさせる
- [X] T021 [US2] src/web/compat/mod.rs を拡張する — `CompatState.allowed_hosts` の同様の拡張と送信元 IP 検証を追加する(レート制限・ボディ上限は既存水準のまま変更しない)。T015 をパスさせる
- [X] T022 [US2] src/main.rs を配線する — bind 値からのホワイトリスト構築と受け渡し、非 loopback 待受成功時の起動時 SecurityEvent 記録(`IndexTxtLanExposed` — src/main.rs:437-447 — と同パターン、面ごとに 1 件)。T016/T017 をパスさせる
- [X] T023 [US2] ui/settings.html に LAN 公開の設定 UI を追加する — 非 loopback bind 保存時に「LAN 内で平文・実質無認証のまま読み書きされうる」警告と明示確認を表示し、確認操作で同意キーを立てて保存する(FR-010、contracts/lan-exposure.md §6)
- [ ] T024 [US2] quickstart V-3/V-4/V-5 の実機確認(2 台)を実施し結果を記録する — 別端末ブラウザ/専ブラからの読み書き・全端末同一レス番号・拒否マトリクス・警告表示・SecurityEvent・レート/ボディ上限同一(SC-002/003/004/007/008/009)

**Checkpoint**: US1 + US2 で「視聴端末から掲示板に書き込む」動線が完成

---

## Phase 5: User Story 3 - スレ立てで固定 >>1 が自動投稿される (Priority: P1)

**Goal**: `BoardSettings.first_post_template` を追加し、スレ開設・次スレ移行時にホストが板鍵署名した kind 1311 を res_no=1 として自動採番・配布する(FR-013〜FR-019)

**Independent Test**: quickstart V-2 が成立する(開設 → >>1 自動確定 → 次スレでも同テンプレ → 変更は遡及しない → 未設定でも既定テンプレ → 再起動後も保持)

### Tests for User Story 3(先に書き、失敗を確認)⚠️

- [X] T025 [P] [US3] unit テスト追加: `first_post_template` の検証(2048 文字/32 行上限・超過拒否・制御文字除去・改行保持)を src/livechat/thread.rs のテストモジュールに追加し、失敗を確認する(contracts/fixed-first-post.md §2)
- [X] T026 [P] [US3] integration テスト追加: tests/integration/livechat.rs に — スレ開設で res_no=1 がテンプレ本文・板鍵署名で自動確定 / `>>1` が開設直後から解決 / 次スレ移行(自動・明示)で投稿時点のテンプレが自動投稿 / テンプレ変更が既存スレへ遡及しない / 未設定時は板タイトル+チャンネルを含む既定テンプレ / >>1 が res_limit にカウントされ移行判定に含まれる / 自動 >>1 に PoW が課されない / **board_settings に保存したテンプレが store 経由の再読込(再起動相当)後もスレ開設 >>1 に反映される**(SC-005 再起動保持の自動化)— を追加し、失敗を確認する
- [X] T027 [P] [US3] contract テスト追加: 板設定 API(PUT/GET `.../settings`)の `first_post_template` 往復と上限超過 400 を tests/contract/local_api.rs に追加し、失敗を確認する(contracts/web-ui.md §5.2)
- [X] T028 [P] [US3] cucumber シナリオ追加: tests/features/fixed_first_post.feature に contracts/fixed-first-post.md §4 の Gherkin(自動確定・次スレ・遡及なし・既定テンプレ・上限拒否・通常検証合格・res_limit カウント)を追加し、対応ステップを実装、失敗を確認する

### Implementation for User Story 3

- [X] T029 [US3] src/livechat/thread.rs の `BoardSettings` に `first_post_template: String`(既定 `""`)を追加する — `validate()`(≤2048 文字・≤32 行)、`sanitized()`(改行保持の制御文字除去)、`from_row`/`to_row` 対応。T025 をパスさせる
- [X] T030 [US3] src/store/mod.rs の `board_settings` テーブルに `first_post_template` 列を追加する — `BoardSettingsRow` 拡張 + 既存 DB への `ALTER TABLE ... ADD COLUMN ... DEFAULT ''` マイグレーション(既存行を壊さない)
- [X] T031 [US3] src/livechat/registry.rs の `open_thread` に >>1 自動投稿を実装する — スレ生成直後、板鍵で `sign_res`(kind 1311、名前・メール空)し通常採番経路で res_no=1 に確定・配布。PoW 免除(ホスト内部採番)。テンプレ空時はシステム既定テンプレ(板タイトル + 対象チャンネル案内)を生成(research.md R3、contracts/fixed-first-post.md §3)
- [X] T032 [US3] src/livechat/registry.rs の次スレ移行(`migrate_to_next_generation_locked` / `start_next_generation` 共通ヘルパ)に >>1 自動投稿を実装する — 投稿時点の板設定テンプレを使用(変更は次スレから反映・遡及なし)。T026/T028 をパスさせる
- [X] T033 [US3] 配線と API 公開 — src/main.rs(`LivechatAdapter::open_thread`/`next_thread` で板鍵アクセスを registry へ受け渡す)と src/web/livechat.rs(板設定 GET/PUT に `first_post_template` を公開)。T027 をパスさせる
- [ ] T034 [US3] quickstart V-2 の実機確認を実施し結果を記録する — 開設/次スレの自動投稿・遡及なし・既定テンプレ・再起動後の永続化(SC-005/006)

**Checkpoint**: 全ストーリーが独立に機能する

---

## Phase 6: Polish & Cross-Cutting Concerns

**Purpose**: ADR 最終化・ドキュメント同期・リリース前ゲート(憲法: リリース前ゲート 8〜10)

- [X] T035 [P] ADR-0015 を最終化する — docs/adr/0015-web-bbs-lan-write.md を実装と同期させ Status: Accepted へ。セキュリティレビューチェックリスト(docs/adr/security-review-checklist.md)の適用結果、および FR-005 クライアント側エスケープの検証範囲判断(自動テストはサーバ側 T039・クライアント側描画は実機確認 T012 — Principle IV)を記録する(Principle II/III)
- [X] T036 [P] ドキュメント同期 — CONTEXT.md と変更モジュールの doc コメント(src/web/mod.rs・src/web/livechat.rs・src/config.rs・src/livechat/registry.rs)を実装に同期する
- [ ] T037 最終ゲート実行 — `cargo fmt -- --check` / `cargo clippy --all-targets` / `cargo audit` / `cargo test`(cucumber 含む)全パスを確認する
- [ ] T038 quickstart 全検証(V-1〜V-5)の完了と SC-001〜SC-009 の充足を specs/007-web-bbs-write/ 配下に記録する
- [ ] T040 大量レス描画の実機確認(/speckit-analyze G1 対応)— quickstart V-1 手順 6 に従い、res_limit 上限相当(4000 レス)のスレで「全部読む」を選択し、初回描画 5 秒以内・スクロールと `>>n` ジャンプの応答を確認して記録する(plan Performance Goals)

---

## Dependencies & Execution Order

### Phase Dependencies

- **Phase 1 (Setup)**: 依存なし
- **Phase 2 (Foundational)**: Phase 1 完了後。US3 をブロック(US1/US2 は論理依存なしだが板設定経路の安定のため先行推奨)
- **Phase 3 (US1)**: Phase 2 完了後。他ストーリーへの依存なし
- **Phase 4 (US2)**: Phase 2 完了後。US1 と独立(専ブラ経路 V-4 は UI 不要。V-3 の Web UI 確認だけは US1 完了後が望ましい)
- **Phase 5 (US3)**: Phase 2(永続化配線)完了後。US1/US2 と独立(>>1 は互換 API・JSON API でも確認可能。Web UI 表示確認は US1 完了後)
- **Phase 6 (Polish)**: 全ストーリー完了後

### User Story Dependencies

- US1 → なし(既存 API + 新 `compat_bbs_port` のみ)
- US2 → なし(bind/検証/監査の層のみ。UI 実機確認の一部が US1 を参照)
- US3 → Phase 2(board_settings 永続化)。US1/US2 とは独立

### Within Each User Story

- テストを先に書き、**失敗を確認してから**実装(憲法 Principle IV — MUST)
- モデル(config/thread.rs)→ サービス(registry/web 層)→ 配線(main.rs)→ UI → 実機確認

### Parallel Opportunities

- T002(ADR 草案)は T001 と並列可
- 各ストーリーのテストタスク(T013〜T017、T025〜T028)はそれぞれ別ファイルで並列可
- T019(security カテゴリ追加)は T018 と並列可
- Phase 2 完了後、US1(UI 中心)・US2(config/web 層)・US3(livechat 層)は触るファイルがほぼ重ならず、並列進行可能(ただし src/main.rs は T005/T022/T033 で競合するため調整すること)

---

## Parallel Example: User Story 2

```text
# テストを一斉に作成(失敗確認まで):
Task: T013 tests/contract/cli_config.rs — bind 許可リスト・同意キー
Task: T014 tests/contract/local_api.rs — Host ホワイトリスト
Task: T015 tests/contract/compat_bbs.rs — 互換 API 境界
Task: T016 tests/integration/lan_write.rs — LAN 読み書き・SecurityEvent
Task: T017 tests/features/lan_exposure.feature — Gherkin

# 実装は config → web/compat → main の順(T019 のみ並列可)
```

---

## Implementation Strategy

### MVP First (User Story 1 Only)

1. Phase 1 → Phase 2 → Phase 3(US1)
2. **STOP & VALIDATE**: quickstart V-1(稼働端末 1 台・loopback のまま)で「掲示板そのもの」の体験を確認
3. この時点で専ブラなし利用者への価値(見た目・操作感)が成立する

### Incremental Delivery

1. US1 完了 → 実機確認 → MVP(loopback の旧来 BBS UI)
2. US3 追加 → スレ開設体験の完成(>>1 のある掲示板)— US2 より先に入れても単体で価値
3. US2 追加 → 視聴端末からの到達(LAN 公開)— 2 台での実機確認
4. Polish → ADR 最終化・ゲート通過

### 注意(このリポジトリの運用)

- 各タスク完了ごとに `cargo fmt -- --check` を通してからコミットする(プロジェクト CLAUDE.md)
- src/main.rs は 3 ストーリーで競合するため、並列進行時はタスク単位で直列化する
- 実機確認(T012/T024/T034)はユーザーの操作・確認が必要になりうる(特に 2 台構成の V-3/V-4)

---

## Phase 7: Convergence

**Purpose**: Clarifications 2026-08-03(quickstart 実機検証で判明した UI/公開経路の要件漏れ)で
追加された FR-002a / FR-006a / FR-006b / FR-014a / FR-023 のうち、現行コードで未達・部分実装の
残作業を追補する。すべて表現層(Web UI / 互換ポートのルーティング)+ 開設 API の任意フィールド
追加で、006 の P2P・順序確定・検証・鍵体系は不変。設計根拠は research.md R11/R12、契約は
contracts/web-ui.md §1/§2.4/§2.5/§3.7/§5.3/§7 と contracts/fixed-first-post.md §3.5/§4。

**⚠️ テスト先行(憲法 Principle IV — MUST)**: T041/T043 は対応実装(T042/T044/T045)より先に
書き、失敗を確認してから実装する。

- [ ] T041 [P] FR-023 の contract/integration テストを追加し失敗を確認する per FR-023 (missing) — 互換ポート(`src/web/compat/`)への `GET /{board}/`(末尾スラッシュのみ)が旧来 BBS UI の板ページ HTML(`text/html; charset=UTF-8`)を返し、`GET /{board}/subject.txt` は従来どおり Shift_JIS を返す(専ブラ向けパス非干渉)ことを tests/contract/compat_bbs.rs(または新規)に追加(contracts/web-ui.md §7)
- [ ] T042 FR-023 を実装する per FR-023 (missing) — `src/web/compat/mod.rs` の `routes()` に `/{board}/` ルートを追加し、既存 Web UI(`ui/livechat.html`)の `include_str!` 資産を UTF-8 の HTML として配信する(`{board}` を初期表示板として開く)。専ブラ向け `subject.txt`/`dat`/`bbs.cgi` の応答・SJIS・検証は変更しない。他面へのリダイレクトはしない。T041 をパスさせる(research.md R11、contracts/web-ui.md §7)
- [ ] T043 [P] FR-014a の contract/cucumber テストを追加し失敗を確認する per FR-014a (missing) — スレ開設 API の任意 `first_post_override` について (a) 指定時にそのスレの >>1 が override 本文で確定、(b) override は板設定 `first_post_template` を変更せず次スレ・別スレに波及しない、(c) 上限超過(>2048 文字/>32 行)は 400 — を tests/contract/local_api.rs と tests/features/fixed_first_post.feature(contracts/fixed-first-post.md §4 の追加 2 シナリオ)へ追加(fixed-first-post.md §3.5)
- [ ] T044 FR-014a の開設 API・registry 配線を実装する per FR-014a (missing) — `src/web/livechat.rs` のスレ開設ハンドラに任意 `first_post_override`(string、検証は固定テンプレと同一)を受け付け、`src/livechat/registry.rs` の `open_thread` の >>1 生成へ「override があればそれ、なければ板設定テンプレ、空なら既定テンプレ」の優先順で渡す。override は当該スレ限りで永続テンプレを変更しない(MUST NOT)。T043 をパスさせる(contracts/web-ui.md §5.3、fixed-first-post.md §3.5)
- [ ] T045 [US1] FR-014a の UI を実装する per FR-014a (missing) — `ui/livechat.html` の新規スレ作成欄(現状 `164-171`・タイトルのみ)に >>1 本文入力欄を追加し、板詳細 API の `settings.first_post_template`(未設定時は既定テンプレ相当)を既定値としてプリフィル、開設 POST に `first_post_override` を付与する。未編集時は空 >>1 を生じさせない(contracts/web-ui.md §2.4)
- [ ] T046 [US1] FR-006a の板設定導線を板ページへ移設する per FR-006a (partial) — `ui/livechat.html` の板設定編集(現状スレページ `222-231`)を板ページ(view-board)の板管理セクション(`<details>`)へ移し、スレ非依存で編集可能にする(スレ 0 件でも可)。あわせて設定フォームと PUT(現状 `724-730` は `first_post_template` 未送信)に**固定 >>1 テンプレ**編集欄を追加する。スレページからは板設定編集を外す(モデレーション実行はスレページに残す)(contracts/web-ui.md §2.5/§3.7)
- [ ] T047 FR-006b の BAN 一覧取得経路を追加する per FR-006b (missing) — `src/web/livechat.rs`(および `LivechatDirectory`/adapter)に現行 BAN(板鍵 BAN・接続 BAN)の**一覧取得**経路を追加する(現状は ban/unban 実行系のみ・`680-728`)。応答は Principle II の定型化を維持(内部情報を漏らさない)。契約に無い新規 API 形は最小限とし、対応する contract/unit テストを先に追加して失敗を確認する
- [ ] T048 [US1] FR-006b の BAN 一覧 UI を追加する per FR-006b (missing) — `ui/livechat.html` の板ページ板管理セクションに、T047 の一覧取得を用いた BAN 済みエントリの一覧参照と解除(既存 unban/unconnban)導線を追加する。個別レスへの BAN **実行**はスレページ側に残す(contracts/web-ui.md §2.5)
- [ ] T049 [US1] FR-002a のナビゲーションリンクを実機で確認・是正する per FR-002a (partial) — `ui/livechat.html` は板一覧→自板(`366-367`)・スレ一覧→スレ(`460`)のアンカーを既に持つが、実機検証で遷移リンク欠落が報告された。再ビルド後に板一覧→自板・スレ一覧→スレの遷移が実機で機能することを確認し、条件描画等で欠落する経路があれば是正する(contracts/web-ui.md §1、quickstart V-1 手順 7)

# Tasks: 分散型配信情報共有ネットワーク(YP代替)

**Input**: Design documents from `/specs/001-nostr-p2p-yp/`

**Prerequisites**: plan.md (rev 2), spec.md, research.md, data-model.md, contracts/, quickstart.md(すべて rev 2 = 純粋 P2P 版で生成済み)

**Rewrite note**: 本ファイルは 2026-07-03 に**1から再生成**した。旧版はリレーサーバー前提
(nostr-sdk リレープール・relays テーブル・モックリレー)であり、spec Clarifications 2026-07-03 /
plan rev 2(リレー排除・独自 gossip)と矛盾するため全面破棄した。

**Tests**: constitution Principle IV(テストファースト MUST)により**テストタスクは必須**。
各ストーリーの Gherkin/契約テストは実装前に作成し、**失敗することを確認してから**実装に着手する。

**Organization**: ユーザーストーリー単位でフェーズ化し、各ストーリーが独立して実装・検証可能。
US4(実況コメント)は将来フェーズのため v1 タスクなし(識別子互換は contracts/nostr-events.md の
kind 1311 予約定義で確保済み — FR-011)。FR-015(PEX)/FR-016(UPnP・着信不可)は特定ストーリーに
属さない横断機能のため独立フェーズ(Phase 6)とする。

## Format: `[ID] [P?] [Story] Description`

- **[P]**: 並列実行可(異なるファイル・未完了タスクへの依存なし)
- **[Story]**: 対応するユーザーストーリー(US1/US2/US3)。Setup/Foundational/Phase 6/Polish には付けない

---

## Phase 1: Setup (Shared Infrastructure)

**Purpose**: プロジェクト初期化と開発基盤

- [ ] T001 Cargo プロジェクト作成とモジュール骨格: `Cargo.toml`(tokio / nostr / axum+tower / igd-next / rusqlite(bundled) / windows / encoding_rs / tracing / cucumber — plan.md「Primary Dependencies」準拠。**nostr-sdk のリレークライアント機能は依存に含めない** — FR-014)、`src/main.rs`、`src/{config.rs, pcp/mod.rs, yp/mod.rs, event/mod.rs, p2p/mod.rs, identity/mod.rs, store/mod.rs, web/mod.rs, security/mod.rs}`、`ui/`、`tests/{features/, contract/, integration/}`、`docs/formal/` を作成しビルドが通る状態にする
- [ ] T002 [P] rustfmt / clippy 設定(`rustfmt.toml`、`Cargo.toml` の `[lints]` で warnings deny)と `.gitignore` 追記(`target/`、`*.db`)
- [ ] T003 [P] CI ワークフロー `.github/workflows/ci.yml`: `windows-latest` ランナーで build + test + clippy(DPAPI/`windows` クレート依存のため Windows 必須 — FR-009)+ `cargo audit` + Trivy スキャン(ADR-0001 準拠)。あわせて実装言語確定(Rust)に伴う静的解析ツール選定(clippy + cargo audit)を `docs/adr/0001-security-scanning-tools.md` の追補として記録し、constitution Follow-up TODO(SAST_TOOL)を解消する — Principle III, VI
- [ ] T004 [P] cucumber テストハーネス: `tests/cucumber.rs` と `Cargo.toml` の `[[test]]` 定義。空の `tests/features/` でコンパイル・実行できること — Principle IV

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: constitution 実装前ゲート(ADR・形式的検証判定・契約確定)+全ストーリー共通の基盤コード

**⚠️ CRITICAL**: このフェーズ完了までユーザーストーリー実装を開始しない

### 実装前ゲート: 設計判断の確定(Principle II, V, VI)

- [ ] T005 [P] ADR 作成 `docs/adr/0002-event-schema-nip-scope.md`: NIP-53 kind 30311 採用・タグ写像・鮮度管理(research R1/R2)・**nostr 援用をデータスキーマに限定する境界**(FR-014、`event/` と `p2p/` のモジュール分離)。トラッカー解決の充足方式(index.txt TIP 経由、PCP tracker lookup は v1 非対応 — plan §Summary)の判断記録を含める。原則参照必須(Principle II, VI)
- [ ] T006 [P] ADR 作成 `docs/adr/0003-persona-key-management.md`: DPAPI 鍵保管・nsec エクスポート方針・ペルソナ破棄の非可逆性(research R6、FR-013)。**ネットワークメタデータ(接続元 IP・発行タイミング・最初の伝搬元ピア)経由のペルソナ間リンク推定の限界と対策範囲**(FR-013 ※但し書き、checklists/security.md CHK014)、DPAPI 復号失敗時(BLOB 破損・別プロファイル・OS 再インストール)の挙動(CHK006)、鍵漏洩時の視聴者側識別の要否(CHK005)を判断・記録する
- [ ] T007 [P] ADR 作成 `docs/adr/0004-threat-model.md`: 多層緩和構成(署名検証/ミュート/ピア切離し/任意 PoW/容量・レート上限 — research R8、FR-008)と閾値方針。**P2P 特有の脅威**を明示的に扱う: PEX 毒入れ・反射攻撃(R14、CHK022)、Eclipse 攻撃(CHK019)、EventStore 追い出し攻撃(CHK020)、悪意ピアの選択的隠蔽・偽 SYNC(CHK018)、`min_pow_bits=0` 既定の帰結(CHK017)。URL 以外の内容ベース警告の要否と v1 スコープの確定(FR-012)、ログ洪水対策(CHK003)、**配信者トラッカー接続先(IP 等)を公開情報として扱う方針とその帰結**(視聴メタデータ含む — constitution Follow-up TODO: PRIVACY、spec Assumptions「プライバシー」の ADR 化義務)も含める — constitution Security Requirements
- [ ] T008 [P] ADR 作成 `docs/adr/0006-unencrypted-transport.md`: P2P トランスポート非暗号化(平文 TCP)の判断記録 — 完全性・真正性はイベント署名で担保、掲載情報は公開データで機密性要件なし(plan §Constitution Check)。**署名で保護されないメッセージ(HELLO/PEERS/SYNC_REQ/CLOSE)の経路上改ざんリスクの受容範囲**(checklists/security.md CHK021)を明示する(Principle II, VI)
- [ ] T009 契約書の整合性修正 `specs/001-nostr-p2p-yp/contracts/p2p-gossip.md` + `specs/001-nostr-p2p-yp/data-model.md`: checklists/p2p.md の [Conflict]/[Gap] を解消する — ①自己接続検出方式の再定義(HELLO に nonce フィールドを追加するか検出方式を変更 — CHK009)②SYNC 応答で受信した EVENT の再伝搬規則の明記(CHK003)③再接続バックオフのパラメータ数値化(CHK004)④fail_count 降格閾値・復帰条件の数値化(CHK012)⑤IPv6 リテラルのブラケット表記規則(CHK010)⑥PEERS 応答の選定規則(CHK011)⑦「外向きのみ」3 表現(p2p_bind 空 / UPnP 失敗 / listen_port 0)の対応関係(CHK014)
- [ ] T010 ADR 作成 `docs/adr/0005-gossip-formal-verification.md`: gossip 伝搬プロトコル(重複抑制・再伝搬・接続時同期)の Principle V クリティカル判定。基準①(新規設計)は該当済み(plan §Constitution Check)。基準②③の判定と、該当時の検証スコープ(ループ不在・重複爆発不在・live イベント到達性・置換の単調性 — checklists/p2p.md CHK001/CHK002)を記録する(T009 の契約確定後に実施)
- [ ] T011 (T010 で「該当」判定の場合のみ)PlusCal モデル作成 `docs/formal/gossip_propagation.tla` + 検証結果: 伝搬・重複抑制・接続時同期の状態機械をモデル化し、デッドロック・不変条件違反(ループ不在・重複爆発不在・到達性・置換単調性)を TLC で検査してから実装に進む(Principle V MUST、`docs/formal/README.md` のツールチェーン使用)
- [ ] T012 [P] セキュリティレビュー観点チェックリスト作成 `docs/security-review-checklist.md`: Principle II 由来の観点(入力検証・エラー応答の内部情報漏洩・最小権限・既存暗号ライブラリ・セキュリティイベントログ・レート制限)+ P2P 観点(未検証ピア再共有禁止・受信多段検証)を列挙し、セキュリティに関わる PR で適用結果を記録する運用を明記 — constitution 実装中ゲート 6(checklists/security.md CHK007)

### 基盤コード

- [ ] T013 SQLite ストア実装 `src/store/mod.rs` + `src/store/schema.sql`: personas / **peers** / mutes / settings テーブル(data-model.md rev 2 準拠 — relays テーブルは存在しない)と CRUD、`%APPDATA%\peca-p2p-yp\app.db` 配置、unit テスト同梱
- [ ] T014 設定管理 `src/config.rs`: Settings 既定値(pcp_bind=127.0.0.1:7146、http_bind=127.0.0.1:7180、**p2p_bind=0.0.0.0:7147**、p2p_outbound_target=8、p2p_inbound_max=32、pex_enabled=1、upnp_enabled=1、freshness_window_sec=600、republish_interval_sec=60、min_pow_bits=0、event_store_max=4096、index_txt_encoding=shift_jis)の読込・保存、コマンドライン上書き(`--p2p-bind` 等 — quickstart 手順 2 の多ノード起動)(T013 依存)
- [ ] T015 [P] セキュリティ共通部 `src/security/mod.rs`: 入力検証ヘルパ(サイズ/制御文字/URL)、**SecurityEvent カテゴリの一元定義**(`pcp_reject` / `p2p_invalid_frame` / `p2p_oversize` / `p2p_rate_limited` / `event_invalid_sig` / `event_oversize` / `pex_rejected` / `http_rate_limited` / `url_warning` — data-model §SecurityEvent、checklists/security.md CHK002)、tracing ファイル出力とサイズ上限・ローテーション(CHK003)、URL 警告判定(http/https 以外 — FR-012)— Principle II
- [ ] T016 [P] イベントスキーマ・署名検証 `src/event/schema.rs`: kind 30311 の型・タグ写像・`nostr` クレートによる署名生成/検証(secp256k1 Schnorr)・受信検証パイプライン(サイズ 16KB→署名→kind/タグ形式→時刻±300 秒→内容範囲→PoW — contracts/nostr-events.md 受信検証 1〜6)、unit テスト同梱(ADR-0002 準拠)
- [ ] T017 イベントストア `src/event/store.rs`: EventStore(置換キー `(kind, pubkey, d)`・last-write-wins・expiration/ended/鮮度切れ除去・容量 event_store_max で古い順破棄)+ DedupCache(event id、直近 10 分)— data-model.md、unit テスト同梱(T016 依存)
- [ ] T018 gossip フレーミングとセッション `src/p2p/frame.rs` + `src/p2p/session.rs`: 長さ前置(4 バイト BE)フレーム(≤ 64KB)、HELLO/HELLO_ACK/CLOSE/PING/PONG、セッション状態機械(established 前の他メッセージは即切断)、1 ピアあたり受信レート制限(256KB/秒・200 msg/秒)、`tests/contract/gossip_frames.rs` にフレーム境界(分割・結合・過大長)・HELLO 順序違反・不正 JSON のフィクスチャ契約テスト(**テスト先行で失敗確認** — contracts/p2p-gossip.md、T009 の確定内容準拠)
- [ ] T019 ピア管理 `src/p2p/peers.rs`: PeerEndpoint CRUD(手動登録・verified フラグ・fail_count・LRU 上限 1,024・自己アドレス登録拒否)、接続管理(外向き目標 8・着信上限 32・多重接続統合・候補選定 manual 優先→last_ok_at→fail_count)、再接続指数バックオフ(T009 で数値化したパラメータ)(T013, T018 依存)
- [ ] T020 Web 骨格 `src/web/mod.rs`: axum ルーター、Host ヘッダ検証、`X-Api-Token` ミドルウェア、レート制限(tower)、JSON ボディ ≤ 64KB、定型エラー応答(内部情報漏洩禁止)、`ui/` 静的アセット埋め込み — contracts/local-api.md 保護方針
- [ ] T021 起動配線 `src/main.rs`: 設定読込→store→security→p2p(待受+外向き接続ループ)→web の起動監視と graceful shutdown(T014, T019, T020 依存)
- [ ] T022 ピア API + UI 基本 `src/web/peers.rs` + `ui/peers.html`: GET(健全性表示)/ POST(**貼り付け一括登録** `{addrs:[...]}`、不正アドレス個別エラー、source=manual)/ PUT(enabled)/ DELETE / `GET /peers/export`(verified のみ 1 行 1 アドレス)— research R10、FR-010(T019, T020 依存)

**Checkpoint**: 同一 PC 上の 2 プロセスを手動ピア登録で established にできる(quickstart 手順 2)。イベントはまだ流れない

---

## Phase 3: User Story 1 - 配信者によるチャンネル掲載 (Priority: P1) 🎯 MVP

**Goal**: PeerCastStation から PCP で受けたチャンネル情報を、選択したペルソナで署名した kind 30311 イベントとして自ノードの EventStore へ格納し、接続中の全ピアへ gossip 伝搬する

**Independent Test**: PCP 疑似クライアントで announce → 接続済みモックピアが署名済み EVENT を受信 → 終了で `status=ended` 発行、を単独検証(quickstart 手順 3 相当)

### Tests for User Story 1(実装前に作成し失敗を確認)⚠️

- [ ] T023 [P] [US1] Gherkin `tests/features/us1_announce.feature` + ステップ骨格: spec US1 受け入れシナリオ 1〜3(60 秒以内に他参加者から取得可能・詳細変更 60 秒以内反映・終了で一覧から除去)を記述し失敗状態にする
- [ ] T024 [P] [US1] PCP 契約テスト `tests/contract/pcp_handshake.rs`: HELO→OLEH→BCST→QUIT のフィクスチャバイト列往復+ネガティブ(atom ネスト深さ >8・64KB 超ペイロード・不正 GUID・loopback 外接続 → 切断+`pcp_reject`)— contracts/pcp-announce.md、spec セキュリティシナリオ 1(PCP 側)
- [ ] T025 [P] [US1] 30311 発行契約テスト `tests/contract/event_30311.rs`: AnnouncedChannel→30311 タグ写像ゴールデン(必須タグ・peca 拡張タグ・expiration=created_at+600・ended・firewalled 時 tip 省略・content 空・**他ペルソナ情報の不混入**(FR-013))— contracts/nostr-events.md

### Implementation for User Story 1

- [ ] T026 [P] [US1] PCP atom コーデック `src/pcp/atom.rs`: 符号化/復号、ネスト深さ ≤8・1 atom ≤64KB の強制、unit テスト同梱
- [ ] T027 [US1] PCP announce セッション `src/pcp/session.rs`: HELO(BroadcastID)→OLEH 応答、BCST 解析(name/gnre/desc/url/bitr/type/titl/crea/albm + PCP_HOST)、状態機械 `announced→updating⇄…→ended`(playing=false / PCP_QUIT / TCP 切断)、loopback 外接続の即切断、セッションレート ≤64KB/秒・同時 ≤32(T026 依存)
- [ ] T028 [US1] AnnouncedChannel レジストリ `src/pcp/channel.rs`: data-model.md の検証ルール適用(文字列長・制御文字除去・数値範囲・GUID 16 バイト)とメモリ管理(T027 依存)
- [ ] T029 [P] [US1] ペルソナ管理 `src/identity/mod.rs`: 鍵生成(`nostr` クレート)、DPAPI 暗号化/復号(`windows` クレート)、active/archived 状態遷移、破棄=行削除(復元不可)、チャンネルへの割当(T013 依存、ADR-0003 準拠)
- [ ] T030 [US1] 掲載エンジン `src/event/publish.rs`: 30311 ビルド(T016 のスキーマ)→ペルソナ署名→**自ノード EventStore へ格納+established 全ピアへ `EVENT` 送信**、republish_interval_sec(60 秒)周期再発行+PCP 変更契機の即時再発行+終了時 `status=ended` 最終発行 — contracts/nostr-events.md 発行規則(T017, T018, T028, T029 依存)
- [ ] T031 [P] [US1] ペルソナ API `src/web/personas.rs`: GET(秘密鍵は返さない)/POST/PUT(label・archive・割当)/DELETE(確認フラグ必須)+ `POST /personas/{pubkey}/export`(nsec 表示は明示操作+警告)— contracts/local-api.md(T029, T020 依存)
- [ ] T032 [US1] 掲載状態 API `src/web/announced.rs`: `GET /api/v1/announced`(AnnouncedChannel + 伝搬先 established ピア数)+ `GET /api/v1/status` 基本形(PCP 待受・ピア数 in/out)(T030 依存)
- [ ] T033 [US1] UI ページ `ui/`: ペルソナ管理(**現在選択中ペルソナの常時明示** = 誤爆防止 — contracts/local-api.md UI 要件)・掲載中一覧(伝搬先ピア数表示)(T031, T032 依存)
- [ ] T034 [US1] US1 統合テスト `tests/integration/announce_flow.rs`: PCP 疑似クライアント+インプロセスモックピアで announce→EVENT 受信→詳細変更→ended までを通し、T023 の cucumber を green にする

**Checkpoint**: 掲載側が単独で機能 — モックピアが検証可能な署名済みイベントを受信できる(MVP)

---

## Phase 4: User Story 2 - 視聴者によるチャンネル発見と視聴開始 (Priority: P1)

**Goal**: gossip で受信したイベントを多段検証して一覧を構築し、UI・`/api/v1/channels`・index.txt で公開。既存 PeerCast クライアント/YP ブラウザがそのまま視聴・閲覧できる

**Independent Test**: モックピアから署名済み/不正イベントを投入し、一覧・index.txt への反映と不正分の不可視(SC-005)、接続直後の SYNC による取得(5 秒以内 — SC-004)を単独検証(quickstart 手順 4 相当)

### Tests for User Story 2(実装前に作成し失敗を確認)⚠️

- [ ] T035 [P] [US2] Gherkin `tests/features/us2_discover.feature` + ステップ骨格: spec US2 受け入れシナリオ 1〜3(5 秒以内一覧表示・無改造クライアントで視聴開始・鮮度切れ除去)を記述し失敗状態にする
- [ ] T036 [P] [US2] 受信検証契約テスト `tests/contract/event_validation.rs`: contracts/nostr-events.md 受信検証 1〜6 の正常系+ネガティブ(16KB 超 → `event_oversize`、署名不正 → `event_invalid_sig`、kind/タグ形式違反、created_at 未来 +300 秒超、数値範囲外、PoW 不足)— spec セキュリティシナリオ 1・2
- [ ] T037 [P] [US2] index.txt ゴールデンテスト `tests/contract/index_txt.rs`: 既知 DiscoveredChannel 集合 → 17 フィールド出力比較(Shift_JIS / UTF-8 両方・空一覧・firewalled(TIP 空)・`<>` 含む名称のサニタイズ・BROADCAST_TIME・不明 `-1`)— contracts/http-yp.md

### Implementation for User Story 2

- [ ] T038 [US2] gossip 受信パイプライン `src/p2p/ingest.rs`: 受信検証(フレーム長→レート→JSON→イベント検証(T016)→DedupCache 重複判定→EventStore 格納)→**格納成功イベントのみ受信元を除く established 全ピアへ再伝搬** — contracts/p2p-gossip.md 伝搬規則 1〜5・受信検証パイプライン(T017, T018 依存)
- [ ] T039 [US2] 接続時同期 `src/p2p/sync.rs`: established 直後の SYNC_REQ(since = now − freshness_window_sec)送信、応答は live かつ鮮度窓内イベントのみ・上限 event_store_max 件、SYNC_DONE、T009 で確定した SYNC 受信イベントの再伝搬規則に従う(T038 依存)
- [ ] T040 [US2] DiscoveredChannel ビュー `src/event/view.rs`: EventStore からの集約(キー `(author_pubkey, channel_id)`・同名別 pubkey は別行)、`status=ended`/鮮度切れの自動除去(FR-006)、ミュート適用(既定オープン型 — FR-008)、source_peers 記録(T038 依存)
- [ ] T041 [P] [US2] ミュート API `src/web/mutes.rs`: GET/POST/DELETE(pubkey / channel 単位 — data-model §MuteEntry。ローカル保存のみ・ネットワーク非公開)(T013, T020 依存)
- [ ] T042 [US2] チャンネル一覧 API `src/web/channels.rs`: `GET /api/v1/channels`(muted 除外・`url_warning` フラグ付与 — FR-012)(T040 依存)
- [ ] T043 [US2] index.txt 生成 `src/yp/index_txt.rs`: 17 フィールド `<>` 区切り・encoding_rs による Shift_JIS(変換不能は `?`)/UTF-8・live かつ鮮度窓内かつ非ミュートのみ・更新新しい順・GET/HEAD のみ・10 req/秒レート制限(`http_rate_limited`)— contracts/http-yp.md(T040 依存)
- [ ] T044 [US2] UI チャンネル一覧 `ui/`: index.txt 相当の列+掲載ペルソナ短縮表示+firewalled 状態の明示(視聴可否の目安 — spec Edge Case)、ミュート操作、コンタクト URL 警告表示と未検証リンクを開く前の確認(FR-012)(T041, T042 依存)
- [ ] T045 [US2] US2 統合テスト `tests/integration/discover_flow.rs`: モックピアから正常/不正イベント投入 → 一覧・index.txt 反映と不正分の不可視(SC-005)、接続直後 SYNC での初期一覧構築、鮮度切れ除去を通し、T035 の cucumber を green にする

**Checkpoint**: US1+US2 で掲載→伝搬→発見→視聴の一連(SC-003)がノード 2 つで成立

---

## Phase 5: User Story 3 - 接続ピア障害時の継続性 (Priority: P2)

**Goal**: 接続ピアの一部/全部が停止しても掲載・発見が継続し、全断時は通知+回復時に自動再開する。単一ノードの停止がネットワーク全体の停止にならない(FR-002)

**Independent Test**: 3 ノード以上のトポロジで掲載・発見を成立させ、ピアを 1 つ停止 → 継続、全停止 → 通知、復帰 → 自動再掲載を検証(quickstart 手順 5)

### Tests for User Story 3(実装前に作成し失敗を確認)⚠️

- [ ] T046 [P] [US3] Gherkin `tests/features/us3_resilience.feature` + ステップ骨格: spec US3 受け入れシナリオ 1〜3(ピア 1 停止で掲載継続・一覧取得継続・全断通知と自動再開)を記述し失敗状態にする

### Implementation for User Story 3

- [ ] T047 [US3] keepalive と切断検出 `src/p2p/session.rs` 拡張: PING 60 秒間隔・120 秒無応答切断、異常切断の安全なクリーンアップ、fail_count / last_ok_at への反映 — contracts/p2p-gossip.md(T018 依存)
- [ ] T048 [US3] 再接続とフェイルオーバー `src/p2p/peers.rs` 拡張: 指数バックオフ再接続(T009 確定値)、外向き目標本数の自動維持(候補補充)、**全ピア到達不能の検出と回復時の再発行トリガ**(掲載中チャンネルの即時再 EVENT 送信 — US3 シナリオ 3)(T019, T030 依存)
- [ ] T049 [US3] 全断通知 UI + status 完成 `src/web/status.rs` + `ui/`: `GET /api/v1/status` に全ピア到達不能フラグ・established 数 in/out を反映し、UI に目立つバナー(到達不能)と回復・自動再開の表示 — contracts/local-api.md UI 要件。あわせて**自ノードの時計ずれ検出と時刻同期を促す通知**(spec Edge Case: 時刻ずれ ±300 秒超で掲載が拒否されるため)を同じ通知系に実装する。検知方式(接続ピアの HELLO/イベント時刻との差分比較等)は実装時に確定し、受信イベント検証の ±300 秒規則(T016)と閾値を一致させる(T048 依存)
- [ ] T050 [US3] US3 統合テスト `tests/integration/resilience.rs`: インプロセスモックピアで 3〜10 ノードのメッシュ/チェーントポロジを構成し、ピア 1 停止での伝搬継続(SC-002)・全断→復帰での自動再掲載を通し、T046 の cucumber を green にする

**Checkpoint**: 全ストーリーが独立して機能 — 単一障害点排除(SC-002)を自動テストで実証

---

## Phase 6: ネットワーク自律性 — ピア交換と NAT 越え(FR-015 / FR-016)

**Purpose**: 手動シードから接続先を自動拡大(PEX)し、着信可否によらず全機能参加を保証する。全ストーリー横断の MUST 要件

- [ ] T051 [P] PEX 契約テスト `tests/contract/pex.rs`: GET_PEERS/PEERS のフィクスチャ+ネガティブ(件数 >64・形式不正・長さ >256・自アドレス・重複 → 破棄+`pex_rejected`)、**未検証ピアを再共有しないこと**の検証(実装前に作成し失敗を確認)— contracts/p2p-gossip.md 受信検証 5、research R14
- [ ] T052 PEX 実装 `src/p2p/pex.rs`: GET_PEERS 要求への PEERS 応答(**verified = 自ノードが接続成功したピアのみ**、≤64 件、T009 で確定した選定規則)、受信候補の source=pex 登録(未検証)と接続成功による verified 昇格、pex_enabled 設定(T019 依存、T051 を green にする)
- [ ] T053 [P] UPnP ポートマッピング `src/p2p/upnp.rs`: `igd-next` による起動時マッピング試行+lease 定期更新、失敗時は警告なしで外向きのみモードへフォールバック、upnp_enabled 設定、着信可否の status 反映 — research R15、FR-016(T014 依存)
- [ ] T054 外向きのみ参加の統合テスト `tests/integration/outbound_only.rs`: P2P 待受を無効化(p2p_bind 空)したノードが外向き接続のみで掲載(US1)・発見(US2)・PEX の全機能を成立させること(SC-009)+ UI 状態表示「外向き接続のみで参加中」— quickstart 手順 6(T052, T053 依存)

**Checkpoint**: 手動シード 1 件から PEX で網に参加でき、NAT 内ノードも全機能を利用できる

---

## Phase 7: Polish & Cross-Cutting Concerns

**Purpose**: セキュリティシナリオの総仕上げ・規模検証・配布準備(constitution リリース前ゲート)

- [ ] T055 [P] セキュリティシナリオ cucumber `tests/features/security.feature`: spec セキュリティシナリオ 3 件(過大ペイロード拒否・なりすまし検出・大量偽登録耐性)+ quickstart 手順 7 の各項(64KB 超フレーム / 16KB 超イベント / 署名不正 / PEX 不正アドレス / `javascript:` URL 警告)を記述し green にする(SC-005, SC-007)
- [ ] T056 [P] 規模・伝搬性能の統合テスト `tests/integration/scale.rs`: インプロセスモックピアで多ノード・多チャンネル(数百〜2,000 ch 相当)を構成し、伝搬遅延(SC-001 の 60 秒)と一覧構築(SC-004 の 5 秒)の目安を計測。5,000 ノード実網(SC-008)は beta 実測で補正する前提を結果と併せて `specs/001-nostr-p2p-yp/research.md` R16 に追記 — research R16
- [ ] T057 [P] README.md 作成: 配布 exe 入手→起動→ピアアドレス貼り付け→一覧閲覧の 15 分手順(SC-006)、着信不可時の説明、手動ポートフォワード案内(research R15)、ライセンス表記は constitution 暫定方針に従い公開前 ADR を参照
- [ ] T058 quickstart.md 全手順の実機検証と更新: 実機 PeerCastStation での掲載(手順 3)・実 YP ブラウザでの index.txt 表示と Shift_JIS 文字化け確認(手順 4 — research R5 のリスク解消ポイント)・2 台以上の PC での実網伝搬(手順 2〜6)
- [ ] T059 リリース前セキュリティ最終確認: `cargo audit`/Trivy で High/Critical 未緩和ゼロ(Principle I)、SecurityEvent カテゴリ一覧(T015)と実装ログ出力の一致確認、全エラー応答の内部情報漏洩なし、`docs/security-review-checklist.md`(T012)の全変更への適用記録 — constitution リリース前ゲート 8〜10
- [ ] T060 ドキュメント最終化: `CONTEXT.md` 更新(モジュール構成・信頼境界)、ADR-0002〜0006 の実装との突合、`docs/formal/` の検証結果最新化(該当時)、チェックリスト(`specs/001-nostr-p2p-yp/checklists/`)未解消項目の解消状況記録

---

## Dependencies & Execution Order

### Phase Dependencies

- **Phase 1 (Setup)**: 依存なし — 即時開始可
- **Phase 2 (Foundational)**: Phase 1 完了後。**全ユーザーストーリーをブロックする**
  - ADR(T005〜T008)は相互に独立 [P]。T009(契約修正)→ T010(形式検証判定)→ T011(該当時モデル)は直列
  - T011 が「該当」の場合、gossip 実装(T018/T019/T038/T039)は モデル検証完了後に着手する(Principle V)
- **Phase 3 (US1)**: Phase 2 完了後
- **Phase 4 (US2)**: Phase 2 完了後(US1 と並行可。ただし統合テスト T045 は T030 の発行エンジンがあると容易)
- **Phase 5 (US3)**: Phase 2 完了後(T048 のみ T030 に依存 — 再発行トリガ)
- **Phase 6 (PEX/UPnP)**: Phase 2 完了後(T052 は T019 依存。US1〜3 とは独立に進行可)
- **Phase 7 (Polish)**: 全ストーリー+Phase 6 完了後

### User Story Dependencies

- **US1 (P1)**: Foundational のみに依存。モックピアで独立検証可能 — MVP
- **US2 (P1)**: Foundational のみに依存。モックピアからのイベント投入で US1 なしでも独立検証可能
- **US3 (P2)**: Foundational に依存。T048 の再発行トリガのみ US1(T030)を要する
- **US4**: v1 タスクなし(将来フェーズ。識別子互換は contracts/nostr-events.md kind 1311 で確保済み)

### Within Each User Story

- テスト(Gherkin・契約テスト)を先に書き、**失敗を確認してから**実装(Principle IV — MUST)
- モデル/コーデック → セッション/エンジン → API → UI → 統合テスト の順

### Parallel Opportunities

- Phase 1: T002/T003/T004 は T001 後に並列
- Phase 2: ADR 4 件(T005〜T008)+ T012 は並列。基盤コードでは T015/T016 が並列、T013→T014、T016→T017
- Phase 2 完了後: **US1(Phase 3)・US2(Phase 4)・Phase 6 を並行着手可能**(担当を分ける場合)
- 各ストーリー内: テストタスク(例 T023/T024/T025)は相互に並列

---

## Parallel Example: Phase 2 実装前ゲート

```text
# ADR 4 件とレビューチェックリストを同時に起票:
Task: "ADR-0002 イベントスキーマと NIP 援用範囲 in docs/adr/0002-event-schema-nip-scope.md"
Task: "ADR-0003 ペルソナ鍵管理 in docs/adr/0003-persona-key-management.md"
Task: "ADR-0004 脅威モデル in docs/adr/0004-threat-model.md"
Task: "ADR-0006 トランスポート非暗号化 in docs/adr/0006-unencrypted-transport.md"
Task: "セキュリティレビュー観点チェックリスト in docs/security-review-checklist.md"
```

## Parallel Example: User Story 1

```text
# US1 のテストを同時に作成(実装前・失敗確認まで):
Task: "Gherkin us1_announce.feature in tests/features/"
Task: "PCP 契約テスト in tests/contract/pcp_handshake.rs"
Task: "30311 発行契約テスト in tests/contract/event_30311.rs"

# その後、独立モジュールを並列実装:
Task: "PCP atom コーデック in src/pcp/atom.rs"
Task: "ペルソナ管理 in src/identity/mod.rs"
```

---

## Implementation Strategy

### MVP First (User Story 1 Only)

1. Phase 1: Setup
2. Phase 2: Foundational(ADR ゲート+gossip 接続基盤まで — CRITICAL)
3. Phase 3: US1 完了 → **モックピアが署名済みイベントを受信できることを独立検証**
4. 停止して検証: quickstart 手順 2〜3 相当をローカル 2 プロセスで確認

### Incremental Delivery

1. Setup + Foundational → 2 ノードが established になる基盤
2. US1 → 掲載がネットワークに流れる(MVP)
3. US2 → 発見・視聴・index.txt が成立(SC-003 の一連が完成 — 実質的な最小リリース候補)
4. US3 → 障害耐性の実証(SC-002)
5. Phase 6 → PEX / NAT 対応で「実際に使える網」へ(FR-015/016)
6. Phase 7 → セキュリティ総仕上げ・実機検証・配布準備

### 注意事項

- T011(PlusCal)が該当判定の場合、gossip 中核(T018/T019/T038/T039)の実装はモデル検証完了が前提(Principle V)
- 旧 tasks.md のリレー関連タスク(リレープール・relays テーブル・NIP-65)は本ファイルに存在しない。復活させないこと(FR-014 MUST NOT)
- コミットはタスク単位または論理グループ単位。セキュリティに関わる変更は `docs/security-review-checklist.md`(T012)の適用結果を PR に記録(constitution 実装中ゲート 6)

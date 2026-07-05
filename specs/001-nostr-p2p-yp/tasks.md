# Tasks: 分散型配信情報共有ネットワーク(YP代替)

**Input**: Design documents from `/specs/001-nostr-p2p-yp/`

**Prerequisites**: plan.md (rev 2), spec.md, research.md, data-model.md, contracts/, quickstart.md(すべて rev 2 = 純粋 P2P 版で生成済み)

**Rewrite note**: 2026-07-03 の再生成(2 回目)。checklists/(requirements・p2p・security・interop、
全 69 項目)の消し込みで spec・contracts・data-model が更新されたことを反映した。
主な変更: 旧 T009(契約整合性修正)は checklist 消し込みで**解消済みのため削除**し、
「T009 で確定する」としていたパラメータ(HELLO nonce・バックオフ 5s/×2/300s・fail_count 閾値 8・
PEERS 選定規則・IPv6 ブラケット表記等)を確定値として各タスクに直書きした。
**Phase 1(T001〜T004)は完了済みのため変更していない**。

**Amendment(2026-07-03 /speckit-analyze 反映)**: T061(local API 契約テスト)・
T062(設定 API + UI 設定画面 — contracts/local-api.md の `GET/PUT /settings` は v1 必須)・
T063(ライセンス ADR — T057 の前提)を追加。T008 に LAN 公開オプトインの実装可否判断
(デッドライン: Phase 2 実装前ゲート完了まで)を組み込み。index.txt のフィールド数を
18(区切り `<>` 17 個)へ訂正(T036/T042 — contracts/http-yp.md と同時修正)。
T038 に SYNC_REQ `since` の応答側フィルタ規則を反映。T056 の SC-001 測定起点は
spec 正規定義(最初の PCP_BCST 受信)の近似であることを明記。
さらに: HELLO/HELLO_ACK に `ts`(時計ずれ自己診断用の未検証申告値)を追加し
T048 の検知方式を確定(contracts/p2p-gossip.md 改訂 — T017/T048)。SYNC 応答量の
受信側検査(検査 6)を T038 に明記。T011 の成果物を番号なしの
`docs/adr/security-review-checklist.md` へ改名(ADR 連番との衝突回避)。

**Amendment(2026-07-03 Phase 2 実装前ゲート完了 — ADR-0002〜0006 の決定を反映)**:
T005〜T011 完了。ゲート判断の実装への反映: (1) LAN 公開オプトインは **v1 では実装しない**
(ADR-0006 決定 4)— T013/T062 に `pcp_bind`/`http_bind` の loopback 強制を追記、
T062 の警告 2 項目は不要化。(2) EventStore に **pubkey 単位クォータ(≤ 64)** を追加
(ADR-0004 §2 — T016)。(3) **DedupCache 保持期間 = max(600 秒, freshness_window_sec)**
の連動制約(ADR-0005 設計制約 — T016)。(4) ペルソナ管理 UI にリンク推定の注意文言
(ADR-0004 §7 — T032)。(5) Principle V 判定は**該当** — PlusCal モデル
`docs/formal/gossip_propagation.tla` を作成し TLC 検査済み(T010。結果は
`docs/formal/gossip_propagation-result.md`)。

**Amendment(2026-07-04 Phase 3/4 実装)**: T026 実装時判断で contracts/pcp-announce.md
§セッション終了と ended を改訂 — `playing=false` の BCST は**当該 ChannelID のみ** ended
(PCP_QUIT / TCP 切断は従来どおりセッションの全チャンネル ended)。BCST はチャンネル単位の
信号であり、多チャンネルセッションで無関係な live チャンネルを巻き込まないため。
DELETE /personas の確認フラグはクエリパラメータ `?confirm=true` を採用(契約は形式を
規定しておらず、ボディ MUST は export のみ — T030)。

**Tests**: constitution Principle IV(テストファースト MUST)により**テストタスクは必須**。
各ストーリーの Gherkin/契約テストは実装前に作成し、**失敗することを確認してから**実装に着手する。

**Organization**: ユーザーストーリー単位でフェーズ化し、各ストーリーが独立して実装・検証可能。
US4(実況コメント)は将来フェーズのため v1 タスクなし(識別子互換は contracts/nostr-events.md の
kind 1311 予約定義で確保済み — FR-011。判定基準は `a` タグからの無変更参照 — spec FR-011)。
FR-015(PEX)/FR-016(UPnP・着信不可)は特定ストーリーに属さない横断機能のため
独立フェーズ(Phase 6)とする。

## Format: `[ID] [P?] [Story] Description`

- **[P]**: 並列実行可(異なるファイル・未完了タスクへの依存なし)
- **[Story]**: 対応するユーザーストーリー(US1/US2/US3)。Setup/Foundational/Phase 6/Polish には付けない

---

## Phase 1: Setup (Shared Infrastructure) — 完了済み

**Purpose**: プロジェクト初期化と開発基盤

- [X] T001 Cargo プロジェクト作成とモジュール骨格: `Cargo.toml`(tokio / nostr / axum+tower / igd-next / rusqlite(bundled) / windows / encoding_rs / tracing / cucumber — plan.md「Primary Dependencies」準拠。**nostr-sdk のリレークライアント機能は依存に含めない** — FR-014)、`src/main.rs`、`src/{config.rs, pcp/mod.rs, yp/mod.rs, event/mod.rs, p2p/mod.rs, identity/mod.rs, store/mod.rs, web/mod.rs, security/mod.rs}`、`ui/`、`tests/{features/, contract/, integration/}`、`docs/formal/` を作成しビルドが通る状態にする
- [X] T002 [P] rustfmt / clippy 設定(`rustfmt.toml`、`Cargo.toml` の `[lints]` で warnings deny)と `.gitignore` 追記(`target/`、`*.db`)
- [X] T003 [P] CI ワークフロー `.github/workflows/ci.yml`: `windows-latest` ランナーで build + test + clippy(DPAPI/`windows` クレート依存のため Windows 必須 — FR-009)+ `cargo audit` + Trivy スキャン(ADR-0001 準拠)。あわせて実装言語確定(Rust)に伴う静的解析ツール選定(clippy + cargo audit)を `docs/adr/0001-security-scanning-tools.md` の追補として記録し、constitution Follow-up TODO(SAST_TOOL)を解消する — Principle III, VI
- [X] T004 [P] cucumber テストハーネス: `tests/cucumber.rs` と `Cargo.toml` の `[[test]]` 定義。空の `tests/features/` でコンパイル・実行できること — Principle IV

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: constitution 実装前ゲート(ADR・形式的検証判定)+全ストーリー共通の基盤コード。
契約書の整合性修正(旧 T009)は checklists/ 消し込み(2026-07-03)で解消済み — 契約は確定している

**⚠️ CRITICAL**: このフェーズ完了までユーザーストーリー実装を開始しない

### 実装前ゲート: 設計判断の確定(Principle II, V, VI)

- [X] T005 [P] ADR 作成 `docs/adr/0002-event-schema-nip-scope.md`: NIP-53 kind 30311 採用・タグ写像・鮮度管理(research R1/R2)・**nostr 援用をデータスキーマに限定する境界**(FR-014、`event/` と `p2p/` のモジュール分離)。トラッカー解決の充足方式(index.txt TIP 経由、PCP tracker lookup は v1 非対応)は**検証可能な仮定**として記録し、記録項目に (1) 仮定「無改造クライアントが TIP のみで視聴開始できる」(2) 実機検証結果(quickstart 手順 4)(3) 不成立時の代替(tracker lookup 追加実装)を含める — plan §Summary。原則参照必須(Principle II, VI)
- [X] T006 [P] ADR 作成 `docs/adr/0003-persona-key-management.md`: DPAPI 鍵保管・nsec エクスポート方針・ペルソナ破棄の非可逆性(research R6、FR-013)。判断・記録事項: **ネットワークメタデータ(接続元 IP・発行タイミング・最初の伝搬元ピア)経由のペルソナ間リンク推定の限界と対策(発行タイミングのジッタ等)の要否**(spec FR-013 ※但し書き)、DPAPI 復号失敗時の挙動の確認(利用不可表示・起動継続・復元不可 — data-model §Persona で設計済み)、**鍵漏洩時の失効の表明と視聴者側が漏洩ペルソナを識別・回避する手段の要否**(v1 非目標 — spec Edge Cases で本 ADR へ委譲済み)
- [X] T007 [P] ADR 作成 `docs/adr/0004-threat-model.md`: 多層緩和構成(署名検証/ミュート/ピア切離し/任意 PoW/容量・レート上限 — research R8、FR-008)。**contracts/p2p-gossip.md §「脅威と対応範囲」表を入力とし**、同表が ADR スコープに委譲した事項を確定する: Eclipse 攻撃への接続先多様性確保(サブネット分散等)の要否、EventStore 追い出しへの pubkey/ピア単位クォータの要否、PEX 反射・負荷集中の残余リスク評価(research R14)、選択的隠蔽・偽 SYNC の残余リスク記録(検出は意図的な非目標)。さらに spec が委譲した事項: **FR-008 の追加緩和手法の要否・PoW の既定閾値・「一覧の実用性維持」の判定基準**(既定 `min_pow_bits=0` の帰結含む)、FR-012 の URL 以外の内容ベース警告の要否、FR-013 のペルソナ間リンク推定対策の要否(T006 と分担整理)、**配信者トラッカー接続先(IP 等)を公開情報として扱う方針とその帰結**(constitution Follow-up TODO: PRIVACY、spec Assumptions「プライバシー」の ADR 化義務)— constitution Security Requirements
- [X] T008 [P] ADR 作成 `docs/adr/0006-unencrypted-transport.md`: P2P トランスポート非暗号化(平文 TCP)の判断記録 — 完全性・真正性はイベント署名で担保、掲載情報は公開データで機密性要件なし(plan §Constitution Check)。**イベント署名の保護が EVENT のみに及び、制御メッセージ(HELLO/PEERS/SYNC_REQ/CLOSE)の経路上改ざんが「完全性はイベント署名で担保」の適用範囲外であること**とその受容範囲(PEERS 毒入れは実接続検証で緩和 — contracts/p2p-gossip.md §脅威と対応範囲)、および **index.txt を plain HTTP で供給する決定のリスク受容**(既定 loopback バインド前提・LAN 公開はオプトイン+警告での受容 — contracts/http-yp.md 冒頭)を明示する。あわせて **LAN 公開オプトイン(HTTP/PCP)を v1 で実装するか否かと方式**を本 ADR で確定する(**デッドライン: Phase 2 実装前ゲート完了 = ユーザーストーリー実装開始前**。実装する場合は T062 の設定画面に contracts/local-api.md §保護方針の警告 2 項目を MUST で含める)(Principle II, VI)
- [X] T009 ADR 作成 `docs/adr/0005-gossip-formal-verification.md`: gossip 伝搬プロトコル(重複抑制・再伝搬・接続時同期)の Principle V クリティカル判定。基準①(新規設計)は該当済み(plan §Constitution Check)。基準②③の判定と、該当時の検証スコープ(**ループ不在・重複爆発不在・live イベントの到達性の 3 性質 — contracts/p2p-gossip.md 伝搬規則 5 が検査すべき不変条件として明示** — と置換の単調性、DedupCache 期限切れ後の第二の防壁 = 同一 event id 再受信の不格納)を記録する
- [X] T010 (T009 で「該当」判定の場合のみ)PlusCal モデル作成 `docs/formal/gossip_propagation.tla` + 検証結果: 伝搬・重複抑制・接続時同期(SYNC 応答への伝搬規則適用 — 伝搬規則 6 含む)の状態機械をモデル化し、デッドロック・不変条件違反(ループ不在・重複爆発不在・到達性・置換単調性)を TLC で検査してから実装に進む(Principle V MUST、`docs/formal/README.md` のツールチェーン使用)
- [X] T011 セキュリティレビュー観点チェックリスト作成 `docs/adr/security-review-checklist.md`(番号なし — ADR 連番との衝突を避けるため): **脅威モデル ADR(T007)の成果物として作成し `docs/adr/` に併置する**(plan §Constitution Check の明記事項)。Principle II 由来の観点(入力検証・エラー応答の内部情報漏洩・最小権限・既存暗号ライブラリ・セキュリティイベントログ・レート制限)+ P2P 観点(未検証ピア再共有禁止・受信多段検証)を列挙し、セキュリティに関わる PR で適用結果を記録する運用を明記 — constitution 実装中ゲート 6(T007 依存)

### 基盤コード

**テストファーストの適用境界**: 基盤コード(T012〜T021, T062)は Gherkin シナリオに
直接対応しないため unit テスト同時作成とし、Principle IV のテストファースト(失敗確認)は
契約テスト(T017・T061)とユーザーストーリー層(Phase 3 以降)で適用する。

- [X] T012 SQLite ストア実装 `src/store/mod.rs` + `src/store/schema.sql`: personas / **peers** / mutes / settings テーブル(data-model.md rev 2 準拠 — relays テーブルは存在しない)と CRUD、`%APPDATA%\peca-p2p-yp\app.db` 配置、unit テスト同梱
- [X] T013 設定管理 `src/config.rs`: Settings 既定値(pcp_bind=127.0.0.1:7146、http_bind=127.0.0.1:7180、**p2p_bind=0.0.0.0:7147**、p2p_outbound_target=8、p2p_inbound_max=32、pex_enabled=1、upnp_enabled=1、freshness_window_sec=600、republish_interval_sec=60、**max_clock_skew_sec=300**、min_pow_bits=0、event_store_max=4096、index_txt_encoding=shift_jis — data-model §Settings が**時刻関連定数の単一出典**)の読込・保存、**`pcp_bind` / `http_bind` は loopback アドレスのみ受理(非 loopback 値は検証拒否 — ADR-0006 決定 4。LAN 公開オプトインは v1 非実装)**、コマンドライン上書き(`--p2p-bind` 等 — quickstart 手順 2 の多ノード起動)(T012 依存)
- [X] T014 [P] セキュリティ共通部 `src/security/mod.rs`: 入力検証ヘルパ(サイズ/制御文字/URL)、**SecurityEvent カテゴリの一元定義(全 12 カテゴリ: `pcp_reject` / `p2p_invalid_frame` / `p2p_oversize` / `p2p_rate_limited` / `event_oversize` / `event_invalid_sig` / `event_invalid_format` / `event_time_skew` / `event_pow_insufficient` / `pex_rejected` / `http_rate_limited` / `url_warning` — data-model §SecurityEvent カテゴリ一覧(全量)を正とする)**、tracing ファイル出力と**ログ自体の DoS 耐性(1 ファイル 10MB × 5 世代ローテーション+同一 `(category, source)` の高頻度イベントは 10 秒間隔で件数集約)**、URL 警告判定(http/https 以外 — FR-012)— Principle II
- [X] T015 [P] イベントスキーマ・署名検証 `src/event/schema.rs`: kind 30311 の型・タグ写像・`nostr` クレートによる署名生成/検証(secp256k1 Schnorr)・受信検証パイプライン(サイズ 16KB→署名→kind/タグ形式→時刻(未来方向 +`max_clock_skew_sec`=300 秒超は破棄)→内容範囲→PoW — contracts/nostr-events.md 受信検証 1〜6。違反ログは `event_oversize` / `event_invalid_sig` / `event_invalid_format` / `event_time_skew` / `event_pow_insufficient` に対応付け)、**current_participants / relays タグ省略 ⇔ 「不明 = -1」の往復規則**、unit テスト同梱(ADR-0002 準拠)
- [X] T016 イベントストア `src/event/store.rs`: EventStore(置換キー `(kind, pubkey, d)`・last-write-wins(同値なら event id 辞書順大)・expiration/ended/鮮度切れ除去・容量 event_store_max で古い順破棄・**同一 pubkey の保持イベント ≤ 64(超過は当該 pubkey の created_at 古い順破棄。セキュリティイベントとしない — ADR-0004 §2)**)+ **同一 event id 再受信の不格納・不再伝搬(DedupCache 期限切れ後の第二の防壁 — data-model §EventStore)** + DedupCache(event id、**保持期間 = max(600 秒, freshness_window_sec) — ADR-0005 設計制約(鮮度窓との連動 MUST)**)、unit テスト同梱(T015 依存)
- [X] T017 gossip フレーミングとセッション `src/p2p/frame.rs` + `src/p2p/session.rs`: 長さ前置(4 バイト BE)フレーム(≤ 64KB)、HELLO/HELLO_ACK(**`version`(完全一致のみ受理)・`listen_port`・`features`(v1 は空配列送信、未知値無視 MUST)・`nonce`(u64、起動時生成 — 自己接続検出用)・`ts`(unix 秒 — 時計ずれ自己診断用の未検証申告値。T048 で使用)**)/CLOSE/PING/PONG、セッション状態機械(established 前の他メッセージは即切断)、1 ピアあたり受信レート制限(256KB/秒・200 msg/秒)。`tests/contract/gossip_frames.rs` にフレーム境界(分割・結合・過大長)・HELLO 順序違反・不正 JSON のフィクスチャ契約テスト(**テスト先行で失敗確認**)。**フィクスチャはモックピア(T033 以降のテスト基盤)と共有できる形(テストベクタファイル)で置き、契約書とモック実装の乖離を検出する** — contracts/p2p-gossip.md §検証方法「契約フィクスチャの共有」
- [X] T018 ピア管理 `src/p2p/peers.rs`: PeerEndpoint CRUD(手動登録・verified フラグ・fail_count・LRU 上限 1,024・自己アドレス登録拒否・**IPv6 リテラルは `[addr]:port` ブラケット表記のみ許容、ブラケットなし複数コロンはパース不能として拒否**)、接続管理(外向き目標 8・着信上限 32・多重接続統合・候補選定 manual 優先→last_ok_at→fail_count・**自己接続は HELLO の nonce 一致で検出し切断+当該アドレスを登録拒否**・**新規(未検証)候補への接続試行 1 件/秒以下**)、再接続指数バックオフ(**初期 5 秒・係数 2・上限 300 秒・接続成功でリセット**)、**fail_count 8 回連続失敗で平常時候補から降格(成功で 0 リセット。復帰は手動操作または全ピア到達不能時の再試行成功)**(T012, T017 依存)
- [X] T019 Web 骨格 `src/web/mod.rs`: axum ルーター、Host ヘッダ検証、`X-Api-Token` ミドルウェア、**レート制限(`/api/v1` 全体で同一接続元 20 req/秒、超過 429 + `http_rate_limited`)**、JSON ボディ ≤ 64KB(超過 413)、定型エラー応答(内部情報漏洩禁止)、`ui/` 静的アセット埋め込み — contracts/local-api.md 保護方針
- [X] T020 起動配線 `src/main.rs`: 設定読込→store→security→p2p(待受+外向き接続ループ)→web の起動監視と graceful shutdown(T013, T018, T019 依存)
- [X] T021 ピア API + UI 基本 `src/web/peers.rs` + `ui/peers.html`: GET(健全性表示: source, verified, enabled, last_ok_at, fail_count, 接続中か)/ POST(**貼り付け一括登録** `{addrs:[...]}`、不正アドレス個別エラー、source=manual)/ PUT(enabled)/ DELETE / `GET /peers/export`(verified のみ 1 行 1 アドレス)— research R10、FR-010(T018, T019 依存)
- [X] T061 [P] local API 契約テスト `tests/contract/local_api.rs`: 各エンドポイントのスキーマ検証(正常系)+ ネガティブ(`X-Api-Token` 欠落の変更系 401・過大ボディ 64KB 超 413・`Host` ヘッダ検証失敗・`/api/v1` レート制限 20 req/秒超過 429 + `http_rate_limited`)— contracts/local-api.md §検証方法・§保護方針。**T019 の実装前に作成し失敗を確認する(Principle IV)**
- [X] T062 設定 API + UI 設定画面 `src/web/settings.rs` + `ui/settings.html`: `GET /api/v1/settings`(全キー返却)/ `PUT /api/v1/settings`(検証つき更新。バインド系キー `pcp_bind` / `http_bind` / `p2p_bind` の変更は保存のうえ再起動要求を応答し UI に表示)— contracts/local-api.md。UI は data-model §Settings のキーを閲覧・変更できる。**LAN 公開オプトインは v1 では実装しない(ADR-0006 決定 4)— §保護方針の警告 2 項目は不要。代わりに `pcp_bind` / `http_bind` の非 loopback 値を 400(定型エラー)で拒否する検証を含める**(T013, T019 依存)

**Checkpoint**: 同一 PC 上の 2 プロセスを手動ピア登録で established にできる(quickstart 手順 2)。イベントはまだ流れない

---

## Phase 3: User Story 1 - 配信者によるチャンネル掲載 (Priority: P1) 🎯 MVP

**Goal**: PeerCastStation から PCP で受けたチャンネル情報を、選択したペルソナで署名した kind 30311 イベントとして自ノードの EventStore へ格納し、接続中の全ピアへ gossip 伝搬する

**Independent Test**: PCP 疑似クライアントで announce → 接続済みモックピアが署名済み EVENT を受信 → 終了で `status=ended` 発行、を単独検証(quickstart 手順 3 相当)

### Tests for User Story 1(実装前に作成し失敗を確認)⚠️

- [X] T022 [P] [US1] Gherkin `tests/features/us1_announce.feature` + ステップ骨格: spec US1 受け入れシナリオ 1〜3(60 秒以内に他参加者から取得可能・詳細変更 60 秒以内反映・終了で一覧から除去)を記述し失敗状態にする
- [X] T023 [P] [US1] PCP 契約テスト `tests/contract/pcp_handshake.rs`: HELO→OLEH→BCST→QUIT のフィクスチャバイト列往復(**OLEH に agent 名 `peca-p2p-yp/<semver>` の固定書式を含むこと**)+ネガティブ(atom ネスト深さ >8・64KB 超ペイロード・不正 GUID・loopback 外接続 → 切断+`pcp_reject`。**未知 atom は無視して切断しないこと・1 セッション 17 チャンネル目は無視+`pcp_reject`**)+ **PCP_QUIT なしの TCP 異常切断 → 即 ended** — contracts/pcp-announce.md、spec セキュリティシナリオ 1(PCP 側)
- [X] T024 [P] [US1] 30311 発行契約テスト `tests/contract/event_30311.rs`: AnnouncedChannel→30311 タグ写像ゴールデン(必須タグ・peca 拡張タグ・expiration=created_at+600・ended・firewalled 時 tip 省略・**listeners/relays 負値はタグ省略**・content 空・**他ペルソナ情報の不混入**(FR-013))— contracts/nostr-events.md

### Implementation for User Story 1

- [X] T025 [P] [US1] PCP atom コーデック `src/pcp/atom.rs`: 符号化/復号、ネスト深さ ≤8・1 atom ≤64KB の強制、**未知・非対応 atom は無視(デバッグレベルでログ記録、セキュリティイベントとしない — 前方互換)**、unit テスト同梱
- [X] T026 [US1] PCP announce セッション `src/pcp/session.rs`: HELO(BroadcastID)→OLEH 応答(**gist 準拠の応答 atom + agent 名 `peca-p2p-yp/<semver>`**)、BCST 解析(name/gnre/desc/url/bitr/type/titl/crea/albm + PCP_HOST)、**1 セッション内の複数チャンネル(ChannelID 単位で AnnouncedChannel を構成、1 セッションあたり ≤16、超過分は無視+`pcp_reject`)**、状態機械 `announced→updating⇄…→ended`(**playing=false / PCP_QUIT / TCP 異常切断のいずれでも全チャンネルを即 ended とし最終イベント発行 — 鮮度切れを待たない**)、loopback 外接続の即切断(LAN 公開オプトイン無効の間)、セッションレート ≤64KB/秒・同時 ≤32(TCP 接続単位)、文字列長超過は切詰め許容(loopback の利用者自身のソフトウェアのため — contracts/pcp-announce.md「切詰め許容の根拠」)(T025 依存)
- [X] T027 [US1] AnnouncedChannel レジストリ `src/pcp/channel.rs`: data-model.md の検証ルール適用(文字列長・制御文字除去・数値範囲・GUID 16 バイト)とメモリ管理(T026 依存)
- [X] T028 [P] [US1] ペルソナ管理 `src/identity/mod.rs`: 鍵生成(`nostr` クレート)、DPAPI 暗号化/復号(`windows` クレート)、**復号失敗時(BLOB 破損・別プロファイル・OS 再インストール)は当該ペルソナを「利用不可」として明示し起動・他機能は継続、復元手段は提供しない(事前 nsec エクスポートが唯一のバックアップである旨を UI 案内 — data-model §Persona)**、active/archived 状態遷移、破棄=行削除(復元不可)、チャンネルへの割当(T012 依存、ADR-0003 準拠)
- [X] T029 [US1] 掲載エンジン `src/event/publish.rs`: 30311 ビルド(T015 のスキーマ)→ペルソナ署名→**自ノード EventStore へ格納+established 全ピアへ `EVENT` 送信**、republish_interval_sec(60 秒)周期再発行+PCP 変更契機の即時再発行+終了時 `status=ended` 最終発行 — contracts/nostr-events.md 発行規則(T016, T017, T027, T028 依存)
- [X] T030 [P] [US1] ペルソナ API `src/web/personas.rs`: GET(秘密鍵は返さない)/POST/PUT(label・archive・割当)/DELETE(確認フラグ必須)+ `POST /personas/{pubkey}/export` の**受け入れ基準 3 点: (1) ボディに `{"confirm":true}` 必須 — 欠落は 400、(2) 「秘密鍵を知る者はこのペルソナとして掲載できる」「破棄後は復元できず唯一のバックアップ手段である」旨の警告と明示確認、(3) nsec は応答本文のみ — ログ・セキュリティイベントに記録してはならない (MUST NOT)** — contracts/local-api.md(T028, T019 依存)
- [X] T031 [US1] 掲載状態 API `src/web/announced.rs`: `GET /api/v1/announced`(AnnouncedChannel + 伝搬先 established ピア数)+ `GET /api/v1/status` 基本形(PCP 待受・ピア数 in/out)(T029 依存)
- [X] T032 [US1] UI ページ `ui/`: ペルソナ管理(**現在選択中ペルソナの常時明示** = 誤爆防止 — contracts/local-api.md UI 要件。利用不可ペルソナの表示 — T028。**「同一ノード・同一トラッカーからの複数ペルソナ掲載はネットワーク観測によりリンク推定されうる」旨の注意文言を常設 — ADR-0004 §7**)・掲載中一覧(伝搬先ピア数表示)(T030, T031 依存)
- [X] T033 [US1] US1 統合テスト `tests/integration/announce_flow.rs`: PCP 疑似クライアント+インプロセスモックピア(**T017 の共有フィクスチャを適用した契約参照実装 — research R11**)で announce→EVENT 受信→詳細変更→ended までを通し、T022 の cucumber を green にする

**Checkpoint**: 掲載側が単独で機能 — モックピアが検証可能な署名済みイベントを受信できる(MVP)

---

## Phase 4: User Story 2 - 視聴者によるチャンネル発見と視聴開始 (Priority: P1)

**Goal**: gossip で受信したイベントを多段検証して一覧を構築し、UI・`/api/v1/channels`・index.txt で公開。既存 PeerCast クライアント/YP ブラウザがそのまま視聴・閲覧できる

**Independent Test**: モックピアから署名済み/不正イベントを投入し、一覧・index.txt への反映と不正分の不可視(SC-005)、接続直後の SYNC による取得(5 秒以内 — SC-004)を単独検証(quickstart 手順 4 相当)

### Tests for User Story 2(実装前に作成し失敗を確認)⚠️

- [X] T034 [P] [US2] Gherkin `tests/features/us2_discover.feature` + ステップ骨格: spec US2 受け入れシナリオ 1〜3(5 秒以内一覧表示・無改造クライアントで視聴開始・鮮度切れ除去)を記述し失敗状態にする
- [X] T035 [P] [US2] 受信検証契約テスト `tests/contract/event_validation.rs`: contracts/nostr-events.md 受信検証 1〜6 の正常系+ネガティブと**ログ名の対応**(16KB 超 → `event_oversize`、署名不正 → `event_invalid_sig`、kind/タグ形式・内容範囲違反 → `event_invalid_format`、created_at 未来 +300 秒超 → `event_time_skew`、PoW 不足 → `event_pow_insufficient`)+ **タグ省略 ⇔ -1 の往復復元(current_participants / relays)** — spec セキュリティシナリオ 1・2
- [X] T036 [P] [US2] index.txt ゴールデンテスト `tests/contract/index_txt.rs`: 既知 DiscoveredChannel 集合 → 18 フィールド(区切り `<>` 17 個)出力比較(Shift_JIS / UTF-8 両方・空一覧・firewalled(TIP 空)・**ID の大文字化(内部小文字 → 出力大文字)**・**サニタイズ順序の両ケース(`<>` 含む名称の除去 → Shift_JIS 変換不能文字の `?` 置換、区切り解析が破壊されないこと)**・**BROADCAST_TIME の 24 時間超(例 25 時間 30 分 → `25:30`、分 2 桁固定)**・**15・17 番目の予約フィールドは常に空**・不明 `-1`)— contracts/http-yp.md

### Implementation for User Story 2

- [X] T037 [US2] gossip 受信パイプライン `src/p2p/ingest.rs`: 受信検証(フレーム長→レート→JSON→イベント検証(T015)→DedupCache 重複判定→EventStore 格納(**同一 event id 再受信は格納・再伝搬とも拒否 — 第二の防壁**))→**格納成功イベントのみ受信元を除く established 全ピアへ再伝搬** — contracts/p2p-gossip.md 伝搬規則 1〜5・受信検証パイプライン(T016, T017 依存)
- [X] T038 [US2] 接続時同期 `src/p2p/sync.rs`: established 直後の SYNC_REQ(since = now − freshness_window_sec)送信、応答は live かつ鮮度窓内のイベントのうち **`created_at ≥ max(since, now − freshness_window_sec)` のもののみ**(since による絞り込み — 非標準の since 値でも応答範囲は鮮度窓を超えない — contracts/p2p-gossip.md §メッセージ種別)・上限 event_store_max 件・SYNC_DONE(count)。**送信側は応答を受信側レート上限(256KB/秒・200 msg/秒)以下に平滑化して送信する (MUST — 正当な同期がレート制限で切断されない両立条件)**。**SYNC 応答として受信した EVENT には通常の伝搬規則 1〜4 をそのまま適用する(既知イベントは再伝搬されないため全量再フラッディングは生じない — 伝搬規則 6)**。受信側は SYNC_REQ 1 回への応答として受信した EVENT が event_store_max 件を超えたら切断+`p2p_rate_limited`(contracts/p2p-gossip.md 検査 6)(T037 依存)
- [X] T039 [US2] DiscoveredChannel ビュー `src/event/view.rs`: EventStore からの集約(キー `(author_pubkey, channel_id)`・同名別 pubkey は別行)、`status=ended`/鮮度切れの自動除去(FR-006)、ミュート適用(既定オープン型 — FR-008)、source_peers 記録(T037 依存)
- [X] T040 [P] [US2] ミュート API `src/web/mutes.rs`: GET/POST/DELETE(pubkey / channel 単位。**両単位は独立評価・いずれか一致で非表示(OR)・優先順位なし — data-model §MuteEntry 適用規則**。ローカル保存のみ・ネットワーク非公開)(T012, T019 依存)
- [X] T041 [US2] チャンネル一覧 API `src/web/channels.rs`: `GET /api/v1/channels`(muted 除外・`url_warning` フラグ付与 — FR-012)(T039 依存)
- [X] T042 [US2] index.txt 生成 `src/yp/index_txt.rs`: 18 フィールド(`<>` 区切り 17 個 — contracts/http-yp.md)(**15・17 番目は位置互換の予約で常に空・文字列欠損は空文字列・BITRATE 不明 0**)・**ID は出力時のみ大文字化**・**サニタイズ順序 = `<>` 除去 → encoding_rs 変換の `?` 置換**・**BROADCAST_TIME は 24 時間超で時間部拡張・分 2 桁固定**・live かつ鮮度窓内かつ非ミュートのみ・更新新しい順・GET/HEAD のみ(他 405)・ヘッダ ≤8KB / URL ≤1KB・10 req/秒レート制限(`http_rate_limited`)— contracts/http-yp.md(T039 依存)
- [X] T043 [US2] UI チャンネル一覧 `ui/`: index.txt 相当の列+掲載ペルソナ短縮表示+**TIP 空(firewalled)チャンネルの「直接視聴不可(トラッカー未公開)」明示(v1 は Tracker Lookup 非対応 — contracts/local-api.md UI 要件)**、ミュート操作、コンタクト URL 警告表示と未検証リンクを開く前の確認(FR-012)(T040, T041 依存)
- [X] T044 [US2] US2 統合テスト `tests/integration/discover_flow.rs`: モックピアから正常/不正イベント投入 → 一覧・index.txt 反映と不正分の不可視(SC-005)、接続直後 SYNC での初期一覧構築(平滑化込みで典型時 1 秒未満 — SC-004)、鮮度切れ除去を通し、T034 の cucumber を green にする

**Checkpoint**: US1+US2 で掲載→伝搬→発見→視聴の一連(SC-003)がノード 2 つで成立

---

## Phase 5: User Story 3 - 接続ピア障害時の継続性 (Priority: P2)

**Goal**: 接続ピアの一部/全部が停止しても掲載・発見が継続し、全断時は通知+回復時に自動再開する。単一ノードの停止がネットワーク全体の停止にならない(FR-002)

**Independent Test**: 3 ノード以上のトポロジで掲載・発見を成立させ、ピアを 1 つ停止 → 継続、全停止 → 通知、復帰 → 自動再掲載を検証(quickstart 手順 5)

### Tests for User Story 3(実装前に作成し失敗を確認)⚠️

- [X] T045 [P] [US3] Gherkin `tests/features/us3_resilience.feature` + ステップ骨格: spec US3 受け入れシナリオ 1〜3(ピア 1 停止で掲載継続・一覧取得継続・全断通知と自動再開)を記述し失敗状態にする

### Implementation for User Story 3

- [X] T046 [US3] keepalive と切断検出 `src/p2p/session.rs` 拡張: PING 60 秒間隔・120 秒無応答切断、異常切断の安全なクリーンアップ、fail_count / last_ok_at への反映 — contracts/p2p-gossip.md(T017 依存)
- [X] T047 [US3] 再接続とフェイルオーバー `src/p2p/peers.rs` 拡張: 指数バックオフ再接続(初期 5 秒・係数 2・上限 300 秒)、外向き目標本数の自動維持(候補補充)、**全ピア到達不能の検出と自動回復(enabled な全既知ピア — fail_count 閾値 8 超過で降格済みのものを含む — を通常候補順で再試行。降格は平常時の優先度を下げるのみで全断時の再試行からは除外しない — contracts/p2p-gossip.md §接続管理)**、回復時の再発行トリガ(掲載中チャンネルの即時再 EVENT 送信 — US3 シナリオ 3)(T018, T029 依存)
- [X] T048 [US3] 全断通知 UI + status 完成 `src/web/status.rs` + `ui/`: `GET /api/v1/status` に全ピア到達不能フラグ・established 数 in/out を反映し、UI に目立つバナー(到達不能)と回復・自動再開の表示 — contracts/local-api.md UI 要件。あわせて**自ノードの時計ずれ検出と時刻同期を促す通知**(spec Edge Case: 時刻ずれ ±300 秒超で掲載が拒否されるため)を同じ通知系に実装する。検知方式: 複数の established ピアの HELLO/HELLO_ACK `ts` 申告値との差分の**中央値**が閾値を超えた場合に通知する(補助的に受信イベントの created_at 分布も利用可 — contracts/p2p-gossip.md §メッセージ種別。**`ts` は未検証の申告値であり通知のみに用い、イベント検証・接続判断には使わない — Principle II**)。閾値は Settings `max_clock_skew_sec`(時刻関連定数の単一出典 — data-model §Settings)を参照して受信検証(T015)と一致させる(T047 依存)
- [X] T049 [US3] US3 統合テスト `tests/integration/resilience.rs`: インプロセスモックピアで 3〜10 ノードのメッシュ/チェーントポロジを構成し、ピア 1 停止での伝搬継続(SC-002)・全断→復帰での自動再掲載・**churn(ノードの高頻度な参加・離脱の反復下での伝搬・ピアリスト鮮度 — spec Edge Case / contracts/p2p-gossip.md §検証方法)**を通し、T045 の cucumber を green にする

**Checkpoint**: 全ストーリーが独立して機能 — 単一障害点排除(SC-002)を自動テストで実証

---

## Phase 6: ネットワーク自律性 — ピア交換と NAT 越え(FR-015 / FR-016)

**Purpose**: 手動シードから接続先を自動拡大(PEX)し、着信可否によらず全機能参加を保証する。全ストーリー横断の MUST 要件

- [X] T050 [P] PEX 契約テスト `tests/contract/pex.rs`: GET_PEERS/PEERS のフィクスチャ+ネガティブ(件数 >64・形式不正・長さ >256・自アドレス・重複・**IPv6 ブラケットなし複数コロン** → 破棄+`pex_rejected`)、**未検証ピアを再共有しないこと**・**PEERS 選定規則(verified=1 のみを last_ok_at 新しい順に ≤64 件)**の検証(実装前に作成し失敗を確認)— contracts/p2p-gossip.md 受信検証 5、research R14
- [X] T051 [P] Gherkin `tests/features/outbound_only.feature` + ステップ骨格: spec「追加受け入れシナリオ」Feature 着信不可ノードの参加(FR-016 / SC-009 — UPnP 失敗下で外向き接続のみで全機能+状態表示「外向き接続のみで参加中」)を記述し失敗状態にする(実装前に作成し失敗を確認)
- [X] T052 PEX 実装 `src/p2p/pex.rs`: GET_PEERS 要求への PEERS 応答(**verified=1 = 自ノードが外向き接続に成功した実績のあるピアのみを last_ok_at の新しい順に最大 64 件**)、受信候補の source=pex 登録(未検証)と接続成功による verified 昇格、**inbound 相手の候補化(申告 `listen_port` > 0 を接続元 host と組み合わせ source=pex・verified=0 で登録。自ノードからの接続成功で verified=1。`listen_port: 0` は候補登録しない)**、pex_enabled 設定(T018 依存、T050 を green にする)
- [X] T053 [P] UPnP ポートマッピング `src/p2p/upnp.rs`: `igd-next` による起動時マッピング試行(**lease 3,600 秒・半分 = 1,800 秒間隔で定期更新**)、失敗時は警告なしで外向きのみモードへフォールバック、**定期更新の失敗は着信性の喪失として即時検出し `GET /api/v1/status` の着信可否表示を「外向き接続のみで参加中」へ更新(research R15。UPnP の成否は HELLO の listen_port 申告値には影響しない — contracts/p2p-gossip.md)**、upnp_enabled 設定(T013 依存)
- [X] T054 外向きのみ参加の統合テスト `tests/integration/outbound_only.rs`: P2P 待受を無効化(p2p_bind 空)したノードが外向き接続のみで掲載(US1)・発見(US2)・PEX の全機能を成立させること(SC-009)+ UI 状態表示「外向き接続のみで参加中」を検証し、T051 の cucumber を green にする — quickstart 手順 6(T052, T053 依存)

**Checkpoint**: 手動シード 1 件から PEX で網に参加でき、NAT 内ノードも全機能を利用できる

---

## Phase 7: Polish & Cross-Cutting Concerns

**Purpose**: セキュリティシナリオの総仕上げ・規模検証・配布準備(constitution リリース前ゲート)

- [X] T055 [P] セキュリティシナリオ cucumber `tests/features/security.feature`: spec セキュリティシナリオ **5 件**(過大ペイロード拒否・なりすまし検出・大量偽登録耐性・**危険なコンタクト URL の警告(http/https 以外は警告+明示操作なしに開かない — FR-012)**・**ペルソナの切替と破棄(相互紐づけ情報の不在+破棄後の秘密鍵復元不可 — FR-013)**)+ quickstart 手順 7 の各項(64KB 超フレーム / 16KB 超イベント / 署名不正 / PEX 不正アドレス)を記述し green にする(SC-005, SC-007)
- [X] T056 [P] 規模シミュレーション `tests/integration/scale.rs`: **quickstart §9 の再現可能な構成に従う** — インプロセスのモックピア 100〜500 ノードを接続度 8(p2p_outbound_target 既定値)のランダムグラフで接続(定数は Settings 既定値のまま — SC-008 の保証範囲は既定値構成のみ)、2,000 チャンネル相当の 30311 イベントを 60 秒周期で再発行(網全体 ~33 イベント/秒)、**起点 = 発行ノードの EventStore 格納時刻(spec SC-001 の正規の起点「最初の PCP_BCST 受信」の近似 — quickstart §9。定義の正は spec SC-001)・終点 = 全ノードの一覧反映時刻で p99 ≤ 60 秒(SC-001)**を計測し、**接続度 8 ランダムグラフの直径比(5,000 ノードで ~4–5)で外挿**。結果と beta 実測補正の前提を `specs/001-nostr-p2p-yp/research.md` R16 に追記
- [X] T057 [P] README.md 作成: 配布 exe 入手→起動→ピアアドレス貼り付け→一覧閲覧の 15 分手順(SC-006 — ピアアドレスは事前入手済み前提)、着信不可時の説明、手動ポートフォワード案内(research R15)、ライセンス表記は ADR-0007(T063)に従う(T063 依存)
- [X] T058 quickstart.md 手順 1〜8 の実機検証と更新(2026-07-04〜05 完了。手順 8 = SC-006 計測のみ README 依存のため T057 完了後に実施): 実機 PeerCastStation(現行安定版 — SC-003 の必須検証対象)での掲載(手順 3)・実 YP ブラウザでの index.txt 表示と Shift_JIS 文字化け確認(手順 4 — research R5 のリスク解消ポイント。**BROADCAST_TIME 24 時間超の解釈揺れ確認を含む — contracts/http-yp.md**)・2 台以上の PC での実網伝搬(手順 2〜6)・TIP 経由視聴開始の仮定検証(結果を ADR-0002 = T005 へ記録)
- [X] T059 リリース前セキュリティ最終確認: `cargo audit`/Trivy で High/Critical 未緩和ゼロ(Principle I)、**SecurityEvent 全 12 カテゴリ(T014)と実装ログ出力の一致確認**、全エラー応答の内部情報漏洩なし、セキュリティレビュー観点チェックリスト(T011)の全変更への適用記録 — constitution リリース前ゲート 8〜10
- [X] T060 ドキュメント最終化: `CONTEXT.md` 更新(モジュール構成・信頼境界)、ADR-0002〜0007 の実装との突合、`docs/formal/` の検証結果最新化(該当時)、チェックリスト(`specs/001-nostr-p2p-yp/checklists/` — 2026-07-03 全 69 項目消し込み済み)の解消記録と実装の乖離がないことの確認
- [X] T063 [P] ADR 作成 `docs/adr/0007-license.md`: 許容的ライセンス(MIT に確定 — `Copyright (c) 2026 Philmist`、opensource.org 準拠の `LICENSE` を配置)と、GPL 不伝播の根拠を独立 2 系統(A: プロセス間 TCP 連携のみ=結合著作物でない / B: プロトコルの事実のみのクリーンルーム実装=著作権的派生物でない、マージ理論含む — research R9)で記録。事実源の性質と逐語コピー不在の維持ルール・商標の分離も明記。constitution Governance「ライセンス方針」(→ 確定へ更新・v1.1.1)と Follow-up TODO(LICENSE → ✅)を解消済み — Principle VI(**T057 の前提。公開前に完了必須 — constitution Governance**)

---

## Dependencies & Execution Order

### Phase Dependencies

- **Phase 1 (Setup)**: 完了済み
- **Phase 2 (Foundational)**: Phase 1 完了後。**全ユーザーストーリーをブロックする**
  - ADR(T005〜T008)は相互に独立 [P]。T009(形式検証判定)→ T010(該当時モデル)は直列。T011 は T007 の成果物
  - 契約は checklist 消し込みで確定済みのため、T009 は即着手可能(旧版の「契約修正後」前提は解消)
  - T010 が「該当」の場合、gossip 中核(T017/T018/T037/T038)は モデル検証完了後に着手する(Principle V)
  - T061(local API 契約テスト)は **T019 の実装前に作成し失敗を確認**する(Principle IV)。T062 は T013/T019 依存
- **Phase 3 (US1)**: Phase 2 完了後
- **Phase 4 (US2)**: Phase 2 完了後(US1 と並行可。ただし統合テスト T044 は T029 の発行エンジンがあると容易)
- **Phase 5 (US3)**: Phase 2 完了後(T047 のみ T029 に依存 — 再発行トリガ)
- **Phase 6 (PEX/UPnP)**: Phase 2 完了後(T052 は T018 依存。US1〜3 とは独立に進行可)
- **Phase 7 (Polish)**: 全ストーリー+Phase 6 完了後。T057 は T063(ライセンス ADR)完了後

### User Story Dependencies

- **US1 (P1)**: Foundational のみに依存。モックピアで独立検証可能 — MVP
- **US2 (P1)**: Foundational のみに依存。モックピアからのイベント投入で US1 なしでも独立検証可能
- **US3 (P2)**: Foundational に依存。T047 の再発行トリガのみ US1(T029)を要する
- **US4**: v1 タスクなし(将来フェーズ。識別子互換は contracts/nostr-events.md kind 1311 で確保済み)

### Within Each User Story

- テスト(Gherkin・契約テスト)を先に書き、**失敗を確認してから**実装(Principle IV — MUST)
- モデル/コーデック → セッション/エンジン → API → UI → 統合テスト の順

### Parallel Opportunities

- Phase 2: ADR 4 件(T005〜T008)は並列。基盤コードでは T014/T015 が並列、T012→T013、T015→T016。T061 は他タスクと並列可(ただし T019 実装より先)、T062 は T013/T019 の後
- Phase 2 完了後: **US1(Phase 3)・US2(Phase 4)・Phase 6 を並行着手可能**(担当を分ける場合)
- 各ストーリー内: テストタスク(例 T022/T023/T024)は相互に並列
- Phase 6: T050/T051(テスト)と T053(UPnP)は並列
- Phase 7: T055/T056/T063 は並列(T057 は T063 依存)

---

## Parallel Example: Phase 2 実装前ゲート

```text
# ADR 4 件を同時に起票(T011 のレビューチェックリストは T007 完了後):
Task: "ADR-0002 イベントスキーマと NIP 援用範囲 in docs/adr/0002-event-schema-nip-scope.md"
Task: "ADR-0003 ペルソナ鍵管理 in docs/adr/0003-persona-key-management.md"
Task: "ADR-0004 脅威モデル in docs/adr/0004-threat-model.md"
Task: "ADR-0006 トランスポート非暗号化 in docs/adr/0006-unencrypted-transport.md"
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

1. Phase 1: Setup(完了済み)
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

- T010(PlusCal)が該当判定の場合、gossip 中核(T017/T018/T037/T038)の実装はモデル検証完了が前提(Principle V)
- 旧 tasks.md のリレー関連タスク(リレープール・relays テーブル・NIP-65)は本ファイルに存在しない。復活させないこと(FR-014 MUST NOT)
- LAN 公開オプトイン(HTTP/PCP)は **v1 では実装しない**と確定済み(T008 = ADR-0006 決定 4)。`pcp_bind` / `http_bind` は loopback のみ受理(T013/T062)。将来解禁する場合は ADR-0006 の改訂+contracts/local-api.md §保護方針の警告 2 項目(攻撃面拡大・平文経路上のトークン盗聴)の MUST 実装を条件とする
- コミットはタスク単位または論理グループ単位。セキュリティに関わる変更は T011 のレビューチェックリストの適用結果を PR に記録(constitution 実装中ゲート 6)

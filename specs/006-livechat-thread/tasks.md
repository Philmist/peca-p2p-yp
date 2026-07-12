# Tasks: 配信実況スレ(P2P 掲示板)

**Input**: Design documents from `/specs/006-livechat-thread/`

**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/(thread-events / thread-delivery / compat-api), quickstart.md

**Tests**: 含む(Constitution Principle IV — BDD with Gherkin は必須。quickstart §1 のとおり実装前に feature の失敗を確認する)

**Organization**: ユーザーストーリー単位でフェーズ分割。各ストーリーは独立に実装・検証可能。

## Format: `[ID] [P?] [Story] Description`

- **[P]**: 並列実行可(別ファイル・未完了タスクへの依存なし)
- **[Story]**: 対応ユーザーストーリー(US1〜US6)
- 各タスクは正確なファイルパスを含む

## Path Conventions

単一プロジェクト(plan.md): `src/`・`tests/`・`docs/` はリポジトリルート直下。

---

## Phase 1: Setup(実装前ゲート + 骨格)

**Purpose**: Constitution Gate 条件(ADR-0014・PlusCal モデル)の履行と、新規モジュールの骨格作成

- [X] T001 nostr-protocol/nips の kind レジストリで 21311 / 31311 の未割当を一次資料で再確認し(research R2 CHK017)、結果と改番判断(thread-events.md §NIP 適合の基準)を specs/006-livechat-thread/research.md の R2 確認記録へ追記
- [X] T002 ADR-0014 を作成: 脅威モデル追加(announce 反射攻撃・偽 ORDER・荒らし)・Principle V 該当/非該当の判定理由・kind 31311/21311 採番根拠を docs/adr/0014-livechat-thread.md に記録(T001 の結果を含む)
- [X] T003 [P] PlusCal モデルを作成: シーケンサ状態機械(採番・ORDER 配布・次スレ移行(レス上限到達・配信者の明示操作の両トリガー — FR-013)・凍結/クローズ)を docs/formal/livechat_sequencer.tla + docs/formal/livechat_sequencer.cfg に記述。検査対象: 採番一意性・欠番なし単調増加(T3)・上限超過採番なし・移行境界の二重採番なし(O1)・確定情報の全接続参加者への到達(ライブネス)
- [X] T004 TLC で docs/formal/livechat_sequencer.tla を検査しパスさせ、結果を docs/adr/0014-livechat-thread.md へ記録(**実装前ゲート**: シーケンサ実装 T030 の開始条件 — research R9)
- [X] T005 [P] Cargo.toml に pulldown-cmark を追加(research R7。他の新規依存はなし)
- [X] T006 [P] モジュール骨格を作成: src/livechat/mod.rs(host / session / thread / board / moderation の空サブモジュール)・src/event/livechat.rs・src/web/compat/mod.rs を作成し src/lib.rs / src/event/mod.rs / src/web/mod.rs へ配線。`cargo build` が通ること

---

## Phase 2: Foundational(全ストーリーの前提)

**Purpose**: イベントスキーマ・永続化・設定・P2P 受け口・テスト基盤。ここが完了するまでユーザーストーリーは着手不可

**⚠️ CRITICAL**: 本フェーズ完了までユーザーストーリー実装を開始しない

- [ ] T007 store に 3 テーブルを追加: `board_keys` / `livechat_moderation` / `board_settings` のスキーマとマイグレーションを src/store/mod.rs に実装(data-model §永続化。スレデータのテーブルは作らない — FR-015 揮発)
- [ ] T008 [P] SecurityEvent に 6 カテゴリを追加(15 → 21): `livechat_announce_invalid` / `livechat_challenge_failed` / `livechat_order_invalid` / `livechat_write_rejected` / `livechat_settings_invalid` / `compat_bbs_denied` を src/security/mod.rs に定義(data-model §SecurityEvent)
- [ ] T009 [P] Settings に 6 キーを追加: `livechat_enabled`(既定 true)/ `thread_max_participants`(128)/ `thread_write_rate`(4 レス/30 秒)/ `thread_msg_rate`(16 msg/秒)/ `announce_store_quota`(2048)/ `compat_bbs_bind`(`127.0.0.1:7183`・非 loopback 値は起動拒否・空文字で無効化)を src/config.rs に実装し src/web/settings.rs へ公開(data-model §Settings)
- [ ] T010 kind 1311 / 21311 / 31311 のイベントスキーマ・直列化・フィールド検証を src/event/livechat.rs に実装: 31311 のタグ集合(d="livechat"・a・title・gen・key・res_count・tip・expiration)と置換規則、1311 の peca タグ(thread/name/mail)と本文制約(≤ 2048 文字・≤ 32 行・制御文字除去)、21311 の seq/order タグと連続性検証、未知タグ・未知 peca サブタグの無視(前方互換 MUST — contracts/thread-events.md)
- [ ] T011 [P] EventStore に kind 31311 の独立保持枠(`announce_store_quota`・板ごと最新 1 件へ置換)を src/event/store.rs に実装(research R3 — 既存 4096 枠と分離)
- [ ] T012 板鍵管理の基盤を src/livechat/board.rs に実装: 鍵ペア生成・keystore 抽象(DPAPI / Linux エンベロープ)による秘密鍵暗号化・`board_keys` テーブル CRUD・ペルソナテーブルとの構造分離(識別子・外部キー共有なし)・エクスポート機能なし(research R8 / FR-016)
- [ ] T013 [P] スレ状態の型と状態機械を src/livechat/thread.rs に実装: Thread(board_id / channel / gen / key / title / res_limit スナップショット / state)・BoardSettings(title ≤ 128・res_limit 100〜4000・noname_name 1〜64・local_rules ≤ 2048・first_post_pow_bits 0〜32)・Res・OrderInfo と、状態遷移(Active/Frozen/Closed)・不変条件 T1/T2/T3 の強制(data-model §エンティティ)
- [ ] T014 [P] P2P 受け口の多重化を実装: HELLO `features` に `livechat1` を追加し、established 後の最初のメッセージが `THREAD_JOIN` ならスレセッションに分岐(1 TCP 接続 = 1 用途・以後の種別混在は不正フレーム切断)。THREAD_* / RES / ORDER / SETTINGS / RESEND_REQ / THREAD_CLOSE / NEXT_THREAD のフレーム種別を src/p2p/frame.rs に追加し src/p2p/session.rs で分岐(research R4 / contracts/thread-delivery.md §トランスポート)
- [ ] T015 モックピアを THREAD_* 対応に拡張: JOIN/WELCOME/REJECT・RES/ORDER 送受・不正フレーム注入を tests/common/mock_peer.rs に追加(T014 のフレーム種別に依存)
- [ ] T016 [P] Gherkin feature を作成: spec US1〜US6 の受け入れシナリオを tests/features/livechat.feature に記述し、ステップ骨格 tests/steps/livechat.rs を tests/cucumber.rs へ登録。**実装前に全シナリオが失敗することを確認**(quickstart §1 テストファースト)

**Checkpoint**: 基盤完了 — 以降のユーザーストーリーは並列着手可能

---

## Phase 3: User Story 1 - スレの開設・発見・閲覧 (Priority: P1) 🎯 MVP

**Goal**: 配信者がスレを開設し announce が発見網に伝搬、視聴者が明示操作でスレを開き既存レスを鍵なしで閲覧できる

**Independent Test**: 配信者ノード 1 + 視聴者ノード 2 で、開設 → announce 伝搬 → スレを開く → 既存レス閲覧を単体検証(announce 受信のみでは接続 0 件)

### Tests for User Story 1

- [ ] T017 [P] [US1] 契約テスト(31311): announce の正常系・置換規則・expiration 鮮度・タグ形式を tests/contract/thread_events.rs(新規)に作成し失敗を確認
- [ ] T018 [P] [US1] 契約テスト(配送ハンドシェイク): JOIN → WELCOME(チャレンジ署名検証)/ REJECT 定型(full/frozen/closed/unknown_thread/rate)をモックピアで tests/contract/thread_delivery.rs(新規)に作成し失敗を確認

### Implementation for User Story 1

- [ ] T019 [US1] スレ開設の配信者操作とスレ announce 発行を実装: 掲載中チャンネルに対するスレ開設(スレ主 = 掲載ペルソナ限定 — FR-001)・kind 31311 の発行・60 秒再発行・expiration 600 秒(FR-002)を src/livechat/host.rs + src/event/publish.rs 連携で実装
- [ ] T020 [US1] gossip 受信検証を拡張: 許可 kind を {30311, 31311} に拡張、検査 #7 ペルソナ一致(不一致は不可視 + `livechat_announce_invalid` — FR-003)、#8 対象実在の緩和(30311 未着でも破棄しない)、kind 1311/21311 の gossip 混入は破棄 + `event_invalid_format`、`livechat_enabled=false` 時は検証のみで不可視 — src/event/schema.rs / src/p2p/ingest.rs(contracts/thread-events.md §受信検証)
- [ ] T021 [US1] ホスト側の接続受理を実装: THREAD_JOIN 受信 → 参加上限(`thread_max_participants`)・スレ状態を確認 → THREAD_WELCOME(スレ主ペルソナ鍵による `challenge || board_id || gen` への Schnorr 署名 + board_settings + res_count)または定型 THREAD_REJECT(内部情報非開示 — FR-006)— src/livechat/host.rs
- [ ] T022 [US1] 参加者セッションを実装: スレを開く明示操作を起点に接続(announce 受信のみでは接続しない — FR-004/SC-005)、WELCOME の sig を announce 記載のスレ主公開鍵で検証、失敗時は切断 + `livechat_challenge_failed` + 指数バックオフ(初期 5 秒・係数 2・上限 300 秒 — FR-005)— src/livechat/session.rs
- [ ] T023 [US1] 接続時同期を実装: joined 直後に `since_seq` 以降の全確定レス(RES)と順序確定情報(ORDER)を seq 順に送出(ホスト側)/ 受信順に表示(参加者側・seq 連続性維持)。閲覧に板鍵を要求しない(FR-010/FR-016)— src/livechat/host.rs / src/livechat/session.rs
- [ ] T024 [US1] Web UI / ローカル API を実装: announce 由来のスレ一覧表示・スレを開く操作・確定レスの閲覧・板タイトル/名無しのデフォルト名/ローカルルールの参照を src/web/livechat.rs(新規)に実装し src/web/mod.rs へ配線
- [ ] T025 [US1] ローカルルール Markdown の安全サブセット描画を実装: pulldown-cmark で raw HTML(インライン・ブロック)を破棄、http/https 以外のリンク無効化(FR-025 / research R7)— src/web/livechat.rs
- [ ] T026 [US1] US1 の cucumber ステップを tests/steps/livechat.rs に実装し、多ノード統合テスト(配信者 1 + 視聴者 2: announce 伝搬 → 一覧表示 → 明示操作で接続 → 全レス閲覧、announce 受信のみで外向き接続 0 件 = SC-005)を tests/integration/livechat.rs(新規)に実装して全パス

**Checkpoint**: US1 単独で機能。閲覧専用の掲示板として成立

---

## Phase 4: User Story 2 - 書き込みと全端末一致の確定表示 (Priority: P1)

**Goal**: 参加者が書き込み、ホストの採番により全端末で同一レス番号・同一並び・同一アンカー解決になる

**Independent Test**: 3 ノード(ホスト + 参加者 2)で同時書き込みし、全端末のレス番号・並び・アンカー解決の一致を検証

**⚠️ ゲート**: T030(シーケンサ)の開始前に T004(TLC パス)が完了していること(research R9)

### Tests for User Story 2

- [ ] T027 [P] [US2] 契約テスト(1311 ホスト側検証): 検証順序 1〜7(サイズ → 署名 → 形式 → スレ状態 → BAN → PoW → レート)の正常系・name の `#` 残存時のホスト側除去を tests/contract/thread_events.rs に追加し失敗を確認
- [ ] T028 [P] [US2] 契約テスト(採番・配布): RES 受理 → ORDER 発行 → 全参加者配布、seq 欠落 → RESEND_REQ → 再送をモックピアで tests/contract/thread_delivery.rs に追加し失敗を確認

### Implementation for User Story 2

- [ ] T029 [US2] 書き込みクライアント経路を実装: 板鍵での自動署名(未生成なら T012 で生成)・名前欄の `#` 以降除去(送信前 — FR-024)・mail 属性の保持(機能的意味なし — FR-029)・自分の未確定投稿の「送信中」区別表示(FR-008)— src/livechat/session.rs
- [ ] T030 [US2] ホストシーケンサを実装: thread-events.md の受信検証 1〜7 を通過したレスに一意採番し、kind 21311 の ORDER(seq 連番・entries 欠番なし)を発行して RES + ORDER を全接続参加者(送信者含む)へ配布(FR-007・不変条件 T3/O1 — PlusCal モデルと対応するコードに意図コメント必須)— src/livechat/host.rs
- [ ] T031 [US2] 参加者側 ORDER 検証と表示を実装: サイズ → 署名 → スレ主一致 → seq 連続性 → res_no 連続性の順で検証、欠落検出時は表示を進めず RESEND_REQ(不変条件 O2)、確定済みレスのみ表示、アンカー `>>n` の全端末一致解決(FR-008/FR-009/FR-011)— src/livechat/thread.rs / src/livechat/session.rs
- [ ] T032 [US2] 板設定の変更と配布を実装: 板主の設定変更 API(4 項目 + first_post_pow_bits — FR-022)・SETTINGS メッセージでの即時配布(FR-023)・受信側検証(制約違反は破棄 + `livechat_settings_invalid` — FR-025)・名無しのデフォルト名はレス確定時点で固定し遡及しない(FR-023/FR-024 — dat 追記不変性の基盤)・res_limit は次スレから適用 — src/livechat/board.rs / src/livechat/host.rs / src/livechat/thread.rs
- [ ] T033 [US2] US2 の cucumber ステップを tests/steps/livechat.rs に実装し、統合テスト(3 ノード同時書き込みで不一致 0 = SC-002、「送信中」→ 確定遷移、`#` 除去、名無し名適用)+ 負荷プロファイル(バースト 30 レス/分・100 接続で p99 ≤ 5 秒 = SC-001、`--ignored`)を tests/integration/livechat.rs に実装して全パス

**Checkpoint**: US1 + US2 で読み書き可能な掲示板として成立

---

## Phase 5: User Story 3 - なりすまし・不正情報への耐性 (Priority: P1)

**Goal**: 偽 announce・偽 ORDER・過大/過頻度の書き込みがすべて不可視 + SecurityEvent 記録となる

**Independent Test**: 署名不一致 announce・偽 ORDER・過大レスを注入するモックピアで、100% 不可視 + 記録を検証

### Tests for User Story 3

- [ ] T034 [P] [US3] 契約ネガティブテスト(イベント): 署名不一致 announce(ペルソナ不一致)・スレ主以外の鍵の ORDER・過大サイズ(> 16KB / 本文 > 2048 文字 / > 32 行)・gossip への 1311/21311 混入を tests/contract/thread_events.rs に追加し失敗を確認
- [ ] T035 [P] [US3] 契約ネガティブテスト(配送): 第三者アドレス announce へのチャレンジ失敗 → 切断 + バックオフ、JOIN 前のスレメッセージ・joined 前の RES・RES/ORDER の kind 不一致 → 不正フレーム切断、レート違反 → 破棄をモックピアで tests/contract/thread_delivery.rs に追加し失敗を確認

### Implementation for User Story 3

- [ ] T036 [US3] レート制限を実装: `thread_write_rate`(板鍵単位)・`thread_msg_rate`(接続単位・制御メッセージ込み)のホスト側強制、違反は破棄 + `livechat_write_rejected`、継続違反は切断(FR-021)— src/livechat/host.rs
- [ ] T037 [US3] 不正フレーム防御を実装: THREAD_JOIN 前のスレメッセージ・joined 前の RES の切断、RES に 1311 以外・ORDER に 21311 以外が載った場合の切断、フレーム長 ≤ 64KB・イベント ≤ 16KB の強制(contracts/thread-delivery.md §防御)— src/livechat/host.rs / src/livechat/session.rs
- [ ] T038 [US3] SecurityEvent 記録と定型エラーの全経路確認: 6 カテゴリの記録配線を検証し、全エラー応答(THREAD_REJECT・切断コード)が内部情報を漏洩しないことを確認。tests/features/security.feature にネガティブシナリオを追記し tests/steps/security.rs で検証(FR-003/FR-005/FR-011/FR-021/SC-004)
- [ ] T039 [US3] US3 の cucumber ステップを tests/steps/livechat.rs に実装し、統合テスト(注入攻撃がすべて不可視 + 記録 = SC-004 100%)を tests/integration/livechat.rs に追加して全パス

**Checkpoint**: P1 ストーリー(US1〜US3)完了 — リリース可能な安全性水準

---

## Phase 6: User Story 4 - モデレーションと NG (Priority: P2)

**Goal**: スレ主が BAN(採番拒否・理由非開示)、視聴者が NG(ローカル非表示・欠番維持)でき、鍵ローテーションには初回 PoW が課される

**Independent Test**: ホスト + 参加者 2 + 荒らし役 1 で、BAN 後の採番拒否・NG 後のローカル非表示・ローテーション時の初回 PoW を検証

### Tests for User Story 4

- [ ] T040 [P] [US4] 契約テスト(モデレーション): BAN 鍵の静黙拒否(理由非開示)・PoW 不足の拒否・完全鍵照合(短縮 ID が同じ別鍵には非適用 — FR-018)を tests/contract/thread_delivery.rs に追加し失敗を確認

### Implementation for User Story 4

- [ ] T041 [US4] NG/BAN の永続化を実装: `livechat_moderation` テーブル CRUD(kind = Ng/Ban/ConnBan・target は完全鍵または接続元・板単位スコープ・ネットワーク非送出 = 不変条件 M1)— src/livechat/moderation.rs + src/store/mod.rs
- [ ] T042 [US4] ホスト側 BAN を実装: 板鍵 BAN は採番拒否(`livechat_write_rejected` を記録するが応答で理由を開示しない — spec Edge Case)、ConnBan は HELLO 後に CLOSE で切断(理由非開示)、接続単位の切断操作(FR-019)— src/livechat/host.rs
- [ ] T043 [US4] 視聴者側 NG を実装: 板鍵単位のローカル非表示・レス番号の欠番維持(FR-020。互換 API の dat には非適用 — contracts/compat-api.md)— src/livechat/thread.rs
- [ ] T044 [US4] 板鍵ローテーションと初回 PoW を実装: 明示操作での鍵再生成(行ごと置換・旧鍵破棄 — FR-017)、ホスト側は初見板鍵に `first_post_pow_bits`(NIP-13)を要求し既知鍵は通常しきい値、クライアント側は送信前に nonce をローカル計算(research R6)— src/livechat/board.rs / src/livechat/host.rs / src/livechat/session.rs
- [ ] T045 [US4] NG/BAN・ローテーションのローカル API/UI を src/web/livechat.rs に追加し、US4 の cucumber ステップ + 統合テスト(BAN 採番拒否・NG 欠番・ローテーション PoW)を tests/steps/livechat.rs / tests/integration/livechat.rs に実装して全パス

**Checkpoint**: US4 完了 — 荒らし耐性のある運用が可能

---

## Phase 7: User Story 5 - スレのライフサイクル (Priority: P2)

**Goal**: レス上限で次スレ移行、明示クローズで揮発、通知なき切断で凍結(閲覧継続)、途中参加で全ログ同期

**Independent Test**: レス上限を小さく設定したスレで、次スレ移行・ホスト切断凍結・明示クローズ削除・途中参加同期を検証

### Implementation for User Story 5

- [ ] T046 [US5] 次スレ移行を実装: res_no = res_limit 確定後**または配信者の明示操作(次スレ操作 API — FR-013)**を契機に NEXT_THREAD(新 gen・新 key)を配布し新世代を開始、旧スレは Frozen(書き込み不可・閲覧可)、移行境界に届いた書き込みは新スレへ採番または定型拒否(PlusCal モデルの検査済み規則に一致させ意図コメントを付す)、res_limit 変更は次スレから適用(FR-012/FR-013)— src/livechat/host.rs / src/livechat/thread.rs / src/web/livechat.rs
- [ ] T047 [US5] 明示クローズを実装: 配信者のクローズ操作 API → スレ主署名付き THREAD_CLOSE(21311 の `["peca","close"]` 特殊形)配布 → 受信参加者はスレデータ削除(揮発 — FR-014/FR-015)、announce の発行停止 — src/livechat/host.rs / src/livechat/session.rs / src/web/livechat.rs
- [ ] T048 [US5] 凍結と復帰を実装: TCP 断・PING 無応答(60 秒間隔・120 秒無応答)で Frozen(取得済みレスの閲覧継続・書き込み不可)、バックオフ付き再接続、同一 gen 継続なら `since_seq` 差分同期で Active 復帰、announce 鮮度切れはスレ一覧から除去し接続済みは凍結扱い(FR-014 / spec Edge Case)— src/livechat/session.rs / src/livechat/thread.rs
- [ ] T049 [US5] 板単位スコープの引き継ぎを検証・配線: 次スレ移行後も板鍵・NG・BAN がそのまま有効であること(板 = ペルソナ単位・スレ非依存)を確認するテストを tests/contract/thread_delivery.rs に追加し、必要な修正を src/livechat/board.rs / src/livechat/moderation.rs に実施
- [ ] T050 [US5] US5 の cucumber ステップ + 統合テストを実装: 次スレ移行(上限到達・同時書き込み競合)・ホスト kill → 凍結 → 復帰・明示クローズ → データ削除・4000 レス済みスレへの途中参加 15 秒以内全ログ(SC-003)を tests/steps/livechat.rs / tests/integration/livechat.rs に実装して全パス

**Checkpoint**: US5 完了 — スレッドフロートと揮発性が成立

---

## Phase 8: User Story 6 - 既存実況クライアントからの読み書き(互換 API) (Priority: P2)

**Goal**: 専ブラ・コメントビューワを loopback に向けるだけで、P2P 実況スレを従来掲示板として読み書きできる

**Independent Test**: 自ノード + 互換 HTTP リクエストで、スレ一覧取得 → レス取得 → 書き込み → 反映確認を単体検証

### Tests for User Story 6

- [ ] T051 [P] [US6] 契約テスト(互換 API): 各エンドポイントの形式・SJIS 変換・数値文字参照保全・実体参照エスケープ・dat 追記不変性・loopback 外/Host 不正の定型拒否・エラー定型(`<title>ERROR!</title>`)を tests/contract/compat_bbs.rs(新規)に作成し失敗を確認

### Implementation for User Story 6

- [ ] T052 [US6] 専用 loopback リスナーを実装: `compat_bbs_bind`(既定 `127.0.0.1:7183`・非 loopback 起動拒否・空文字で無効化)、Host 検証(`127.0.0.1[:port]` / `localhost[:port]` 以外は定型 403)、レート制限・ボディ上限 ≤ 64KB、違反は `compat_bbs_denied` 記録(FR-026 / research R5)— src/web/compat/mod.rs
- [ ] T053 [US6] Shift_JIS 変換層を実装: encoding_rs(CP932)での全応答エンコード、変換不能文字の数値文字参照(`&#dddd;`)保全、受理時の数値文字参照(`&#dddd;` / `&#xhhhh;`)展開(contracts/compat-api.md §受け口)— src/web/compat/sjis.rs(新規)
- [ ] T054 [US6] 読み出し系エンドポイントを実装: `GET /{board}/subject.txt`(`<key>.dat<>タイトル (レス数)`)・`GET /{board}/SETTING.TXT`(BBS_TITLE 等 + 拡張キー BBS_MAX_RES、単位は文字数)・`GET /{board}/head.txt`(ローカルルール Markdown を平文のまま)・未知の板/保持しない dat は定型 404(FR-027)— src/web/compat/mod.rs
- [ ] T055 [US6] dat 出力を実装: `GET /{board}/dat/{key}.dat` — 確定済みレスのみ・1 レス 1 行(`名前<>メール<>日付 ID:xxxxxxxx<>本文<>スレタイトル`)・エスケープ一意規則(`&` `<` `>` `"` の順で実体参照・改行 `<br>`)・板鍵由来の短縮 ID 8 文字(表示専用)・名無し名はレス確定時点で固定・**追記不変性(MUST)**・`Last-Modified`/304・`Range`/206/416・gzip なし(contracts/compat-api.md §HTTP メタデータ)— src/web/compat/dat.rs(新規)
- [ ] T056 [US6] bbs.cgi を実装: `POST /test/bbs.cgi` — SJIS フォーム解析(bbs/key/time/FROM/mail/MESSAGE)・数値文字参照展開・名前欄 `#` 除去・板鍵自動署名(なければ生成 + 初回 PoW 計算)・**通常経路と同一の検証・送信(FR-028 抜け道禁止)**・確認画面なし/Cookie なし/Referer 不使用・`subject` 付き(スレ立て)は定型拒否・成功 `<title>書きこみました。</title>` / エラー `<title>ERROR!</title>` + `ERROR:<定型>`(内部情報非漏洩 — FR-030)— src/web/compat/bbs_cgi.rs(新規)
- [ ] T057 [US6] 板 URL(`http://127.0.0.1:7183/<board_id>/`)のコピー可能な UI 表示を src/web/livechat.rs に追加し、US6 の cucumber ステップ + quickstart §5 の curl 手動確認(一覧 → 書き込み → dat 反映・loopback 外拒否)を tests/steps/livechat.rs で実装して全パス

**Checkpoint**: 全ユーザーストーリー完了

---

## Phase 9: Polish & Cross-Cutting Concerns

**Purpose**: 横断的な成功基準の確認・文書化・最終検証

- [ ] T058 SC-006 の維持確認: 既存 scale テストに announce 負荷(live チャンネル併設スレ)を追加した構成で、チャンネル掲載 60 秒以内の一覧反映が維持されることを tests/integration/scale.rs で検証(research R3 の容量判定の実測裏付け)
- [ ] T059 [P] ドメイン文書の更新: CONTEXT.md に livechat モジュール・互換 API・kind 3 種の追加を反映(docs/agents/domain.md の単一コンテキスト方針に従う)
- [ ] T060 ADR-0014 の最終化: TLC 検査結果・実装中に生じた設計逸脱・SJIS 仮説の状態を docs/adr/0014-livechat-thread.md へ反映
- [ ] T061 quickstart §1〜§5 の全検証を実行: `cargo fmt -- --check` / `cargo clippy --all-targets` / `cargo test` / `cargo test --test cucumber` / `cargo audit` / gitleaks をすべてパスさせる(specs/006-livechat-thread/quickstart.md §完了判定)
- [ ] T062 SC-007 実機確認(利用者の協力が必要): 利用者所有の実況ツール一式を `http://127.0.0.1:7183/<board_id>/` に向け、quickstart §5 の 8 観点(hex 板名の登録可否・SJIS/数値文字参照描画・Range/304 の噛み合い・dat 落ち解釈・SETTING.TXT キー突合・head.txt 表示・確認画面なし書き込み・固定 ID の NG 機能)を確認し、結果を specs/006-livechat-thread/research.md の R5 へ追記。不成立項目は追加タスク化

---

## Dependencies & Execution Order

### Phase Dependencies

- **Phase 1(Setup)**: 依存なし。T002 は T001 の後。T004 は T002 + T003 の後
- **Phase 2(Foundational)**: T006(骨格)完了後。T012 は T007 の後、T015 は T014 の後。**全ユーザーストーリーをブロック**
- **Phase 3〜8(US1〜US6)**: Phase 2 完了後。優先順は US1 → US2 → US3(P1)→ US4 → US5 → US6(P2)
- **Phase 9(Polish)**: 対象ストーリー完了後(T058 は US1、T060 は全体、T062 は US6 に依存)

### User Story Dependencies

- **US1(P1)**: Foundational のみに依存。単独で MVP
- **US2(P1)**: Foundational + US1 のセッション基盤(T021〜T023)を利用。**T030(シーケンサ)は T004(TLC パス)がゲート**
- **US3(P1)**: US1/US2 の検証経路にネガティブ強制を追加。独立テスト可(モックピア注入)
- **US4(P2)**: US2 の書き込み経路に依存(BAN/PoW はホスト検証 5・6 の実装)
- **US5(P2)**: US2 の採番・配布に依存(移行境界・クローズは ORDER 経路の拡張)
- **US6(P2)**: US2 の書き込み経路に依存(bbs.cgi は同一経路へのブリッジ)。読み出し系(T052〜T055)は US1 完了時点で着手可能

### Within Each User Story

- 契約テスト([P])→ 失敗確認 → 実装 → cucumber ステップ + 統合テストで全パス
- 同一ファイル(tests/steps/livechat.rs・tests/integration/livechat.rs・src/livechat/host.rs 等)を触るタスクは [P] なし = 順次実行

### Parallel Opportunities

```text
Phase 1: T003(TLA+)‖ T005(Cargo.toml)‖ T006(骨格)   ※ T001→T002 は直列
Phase 2: T008(security)‖ T009(config)‖ T011(event store)‖ T013(thread 型)‖ T014(p2p)‖ T016(feature)
US1:     T017(thread_events.rs)‖ T018(thread_delivery.rs)
US2:     T027 ‖ T028
US3:     T034 ‖ T035
Phase 9: T059(CONTEXT.md)は他と並列可
Foundational 完了後は US1 と US6 読み出し系の並行など、別ファイル群のストーリーを並列着手可能
```

---

## Implementation Strategy

### MVP First(User Story 1)

1. Phase 1 完了(ADR-0014・TLC パスは以降のゲート)
2. Phase 2 完了(基盤 — 全ストーリーをブロック)
3. Phase 3(US1)完了 → 独立検証(閲覧専用掲示板として動作)

### リリース可能ライン(P1 完了)

US1 + US2 + US3 がそろって初めて「安全に読み書きできる実況スレ」になる(US3 は
Constitution Principle I の要請でありリリース前必須)。P2(US4〜US6)は各々独立に
追加デリバリー可能。

### Incremental Delivery

1. Setup + Foundational → 基盤完成
2. US1 → 発見・閲覧の検証(MVP)
3. US2 → 読み書き成立(TLC ゲート通過済みのシーケンサ)
4. US3 → 安全性確認 → **リリース可能**
5. US4 / US5 / US6 → それぞれ独立に追加・検証
6. Polish → SC-006/SC-007 の横断確認で受け入れ完了

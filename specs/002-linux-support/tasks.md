# Tasks: Linux 対応(常時稼働ノード・systemd 親和)

**Input**: Design documents from `/specs/002-linux-support/`

**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/(key-envelope / cli-config / systemd-service), quickstart.md

**Tests**: spec にセキュリティシナリオ(Gherkin)と契約テスト表(contracts §6・§7)が明記されているため、テストタスクを含める(Principle IV)。quickstart のトレーサビリティ表の全セルを本書のタスクへ割り当てる。

**Organization**: ユーザーストーリー単位でフェーズ分割し、各ストーリーを独立に実装・検証できるようにする。

## Format: `[ID] [P?] [Story] Description`

- **[P]**: 並列実行可能(異なるファイル・未完了タスクへの依存なし)
- **[Story]**: 対応ユーザーストーリー(US1 / US2 / US3)
- 各タスクに正確なファイルパスを記載

## Path Conventions

単一 Rust クレート構成(リポジトリルートの `src/`・`tests/`)。plan.md の Project Structure に従う。
新規テストファイルは `Cargo.toml` の `[[test]]` 登録が必要(既存の contract/integration テストと同方式)。

---

## Phase 1: Setup(ADR ゲート・依存関係)

**Purpose**: 実装の前提となる設計決定の確定(ADR-0008)と依存グラフの再配線

**⚠️ GATE**: T001(ADR-0008)は plan.md「Constitution Check」の条件付き PASS を成立させる先頭ゲート。T001 完了までいかなる実装タスクにも着手しない(spec Assumptions / checklists/security.md CHK031)。

- [X] T001 ADR-0008 を `docs/adr/0008-linux-key-protection.md` に作成する。research.md R1〜R4 と contracts/ を正として、plan.md「ADR-0008 転記項目」の 7 項目(①cfg 集約方式 ②AEAD 選定と依存評価 ③エンベロープ形式・後方互換・レガシー残存/ロールバック非対応の受容 ④master.key 配置・生成順序・生成競合 ⑤脅威モデルの限界の受容 ⑥パーミッション検査の範囲と意図的除外 ⑦Principle V 非適用の判定理由)をすべて記載する
- [X] T002 `Cargo.toml` を更新する: `windows` クレートを `[target.'cfg(windows)'.dependencies]` へ移動し、`chacha20poly1305`・`zeroize` を `[target.'cfg(unix)'.dependencies]` へ追加する(research R1/R2)。Windows でビルドが従来どおり通ることを確認する

**Checkpoint**: ADR-0008 確定・依存再配線済み(この時点で Linux ビルドはまだ失敗してよい)

---

## Phase 2: Foundational(全ストーリーのブロッキング前提)

**Purpose**: プラットフォーム抽象(platform / keystore)の導入。`identity::dpapi` 直接依存が残る限り Linux ではビルド自体が失敗する(FR-001)ため、全ストーリーがこのフェーズにブロックされる。

**⚠️ CRITICAL**: このフェーズ完了までユーザーストーリーの作業は開始できない

### Tests for Foundational(テストファースト — Principle IV)

**⚠️ 順序**: T016 は keystore 実装(T004〜T007)そのものを検証する契約テストであるため、US2 から本フェーズへ前倒しする。**T016 を先に作成し、失敗する状態(未実装によるコンパイル失敗を含む)を確認してから T004 以降の実装に着手する**(constitution Principle IV / 実装中ゲート 5)。

- [X] T016 [P] [US2] `tests/contract/key_envelope.rs` を新規作成し `Cargo.toml` に `[[test]]` 登録する: contracts/key-envelope.md §6 の契約テスト #1〜#9 — ①protect 出力が `PYK1` + 現プラットフォーム scheme で始まり平文 32 bytes 非含有 ②roundtrip 一致 ③payload/tag 1bit 破壊 → Unusable(パニックしない) ④他プラットフォーム/未知 scheme → Unusable ⑤(windows) レガシー BLOB 復号可 ⑥(unix) magic なし BLOB → Unusable ⑦(unix) master.key 欠如時に 0600 で生成・既存暗号化ペルソナありなら警告 ⑧(unix) 別 master.key では復号不能 ⑨(unix) AAD 改竄で復号失敗。**作成時点で全テストが失敗することを確認する**

### Implementation for Foundational

- [X] T003 [P] `src/platform/mod.rs` を新規作成し、data-dir 解決を実装する: 優先順 `--data-dir` > `$STATE_DIRECTORY`(unix・複数列挙時は先頭)> `%APPDATA%\peca-p2p-yp`(Windows)/ `$XDG_STATE_HOME/peca-p2p-yp`(unix)> `~/.local/state/peca-p2p-yp`。unix では data-dir を mode `0700` で再帰作成。解決不能時は定型メッセージ + 終了コード 2(contracts/cli-config.md §1)。`src/lib.rs` に `mod platform` を登録する
- [X] T004 [P] `src/identity/keystore/mod.rs` を新規作成し、鍵エンベロープを実装する: `magic "PYK1" || scheme(1byte) || payload` の encode/decode、`protect(&[u8]) -> Vec<u8>` / `unprotect(&[u8]) -> Result<...>` 共通入口、読込規則(magic なし → レガシー DPAPI BLOB 扱い、他プラットフォーム/未知 scheme → Unusable でパニック禁止)、書込みは常にエンベロープ形式(contracts/key-envelope.md §1〜§3)
- [X] T005 [P] `src/identity/mod.rs` 内の既存 `mod dpapi` を `src/identity/keystore/dpapi.rs` へ `#[cfg(windows)]` で移設する(挙動不変・scheme 0x01 として keystore から呼ばれる)(research R1)
- [X] T006 [P] `src/identity/keystore/file_key.rs` を `#[cfg(unix)]` で新規作成する: master.key 読込/生成(`O_CREAT|O_EXCL` + mode `0600` で原子的生成、`EEXIST` 時は既存読込へフォールバック、32 bytes サイズ検証 → 不一致は破損扱い、暗号化済みペルソナ存在下での新規生成時は「保護鍵消失疑い」警告)+ XChaCha20-Poly1305 protect/unprotect(nonce 24 bytes 前置、AAD = `magic || scheme`)+ 鍵素材・中間バッファの `zeroize`、鍵保持型の `Debug` redaction(contracts/key-envelope.md §4・§5)
- [X] T007 `src/identity/mod.rs` の `dpapi::protect` / `dpapi::unprotect` 直接呼出しを `keystore::protect` / `keystore::unprotect` へ置換する。復号失敗 → 当該ペルソナのみ `usable: false`(既存 ADR-0003 §4 挙動維持)。既存 unit テスト(`dpapi_roundtrip` 等)を keystore 経由に更新する(T004〜T006 完了後)
- [X] T008 [P] `src/store/mod.rs` の `open_default` を `platform::data_dir` 使用へ変更する(T003 完了後)
- [X] T009 [P] `src/main.rs` の `resolve_data_dir` を `platform` モジュールの解決へ差し替える(既存 CLI `--data-dir` の意味論不変 — contracts/cli-config.md)(T003 完了後)

**Checkpoint**: `cargo build` が Windows・Linux の両方で成功し(FR-001 の型レベル成立)、T016 の契約テストが両プラットフォームで通過する

---

## Phase 3: User Story 1 - Linux 上で利用者ノードとして稼働できる (Priority: P1) 🎯 MVP

**Goal**: Linux でビルド・起動し、gossip 参加・チャンネル発見・index.txt 提供が Windows 版と同等に機能する(ペルソナ掲載なしの発見・伝搬ノード)

**Independent Test**: Linux 環境でビルド・起動し、既知ピアへ接続して他ノードのチャンネルが一覧に現れること、ローカル HTTP から index.txt を取得できること(quickstart 検証 1・2)

### Tests for User Story 1

- [X] T010 [P] [US1] `tests/contract/cli_config.rs` を新規作成し `Cargo.toml` に `[[test]]` 登録する: data-dir 解決順テスト — `--data-dir` が全ソースに優先(cli-config §7 #1)、(unix)`STATE_DIRECTORY` > `XDG_STATE_HOME` > `~/.local/state` の順(§7 #2)
- [X] T011 [P] [US1] `tests/integration/platform_startup.rs` を新規作成し `Cargo.toml` に `[[test]]` 登録する: 使用中ポートで起動 → 定型メッセージ + 非 0 終了・スタックトレース/内部パスなし(cli-config §5・§7 #5)、異なる data-dir + ポート指定で 2 インスタンス同時稼働(§7 #6、FR-010)

### Implementation for User Story 1

- [X] T012 [P] [US1] `src/config.rs` の `--help` 文言を更新する: `--data-dir` 既定値をプラットフォーム別に正しく表示(Windows: `%APPDATA%\peca-p2p-yp` / Linux: `$XDG_STATE_HOME/peca-p2p-yp` ほか)、「DPAPI」等の Windows 固有名をプラットフォーム中立表現へ(挙動不変 — cli-config §6)
- [X] T013 [US1] `src/main.rs` の起動失敗経路を FR-014 合否基準に沿って整備する: バインド失敗・権限不足時に (a) 失敗した操作(どのリスナーか)と (b) 原因種別(使用中・権限不足等)が判別できる定型メッセージ + 終了コード規約(0/1/2)。スタックトレース・内部絶対パス・OS エラー生文字列のみの出力を排除(cli-config §5)
- [X] T014 [US1] `.github/workflows/ci.yml` に `ubuntu-latest` の build + test + clippy + fmt ジョブを追加し、既存 `windows-latest` ジョブと並走させる(research R10、SC-002 ゲート)
- [X] T015 [US1] Linux 実機で quickstart 検証 1・2 を実施する: `cargo fmt/clippy/build/test` 全成功、`cargo tree` に Win32 クレートが現れない、手動起動でピア接続 → チャンネル発見 → index.txt 取得、loopback 強制(`--http-bind 0.0.0.0:7180` 拒否 — ADR-0006)を確認
  - 2026-07-06 部分実施済み(Linux/WSL2): fmt・clippy・build・test 全成功、`cargo tree` に Win32 クレート出現なし、手動起動 → `/index.txt` 200・data-dir 0700・master.key 0600、`--http-bind 0.0.0.0:7180` は定型メッセージ + 終了コード 2 で拒否
  - 2026-07-06 完了: WSL(Linux)と Win11 ノード間で実ネットワーク疎通を確認(ピア接続 → 他ノードのチャンネル発見)— 利用者確認済み

**Checkpoint**: Linux 上で発見・伝搬ノードとして完全動作(MVP 成立、SC-001)

---

## Phase 4: User Story 2 - Linux 上でペルソナ秘密鍵を安全に保管して掲載できる (Priority: P2)

**Goal**: DPAPI のない Linux でも秘密鍵が平文非保存・他アカウント復号不能で at-rest 保護され、パーミッション検査・自動是正と復号不能時の部分劣化が機能する

**Independent Test**: Linux でペルソナを作成し、保管表現が平文でないこと・他ユーザーから読めないこと・掲載チャンネルが他ノードで発見されること・nsec エクスポート/破棄が Windows 版と同一意味論であること(quickstart 検証 3)

### Tests for User Story 2

> **注**: T016(key_envelope 契約テスト)は keystore 実装(Phase 2)の前提となるため Phase 2 へ前倒し済み(テストファースト — Principle IV)。

- [X] T017 [P] [US2] `tests/contract/cli_config.rs` に追加する: (unix) 緩いパーミッションの master.key/DB が `0600` へ是正され `key_permission_fixed` が記録される(cli-config §7 #3)、(unix) 是正不能時に全ペルソナ利用不可 + index.txt/一覧提供は継続(§7 #4)。**T018〜T021 の実装前に失敗する状態を確認する(Principle IV)**
- [X] T023 [P] [US2] `tests/features/security.feature` に cucumber シナリオを追加し、`tests/steps/` にステップ定義の骨子を用意する: ①平文非永続化(at-rest 保護) ②パーミッション自動是正 ③是正不能の部分劣化(全ペルソナ利用不可 + 発見機能継続) ④復号不能データの隔離(当該ペルソナのみ利用不可)。各シナリオの事後アサーションとして全ログ出力に秘密鍵・nsec(hex 64 桁・bech32・部分文字列・Debug 表現)が含まれないことを検査対象として定義する(spec セキュリティシナリオ、quickstart トレーサビリティ表)。**US2 実装(T018〜T021)前にシナリオが失敗する状態を確認する(Principle IV)**

### Implementation for User Story 2

- [X] T018 [US2] `src/security/mod.rs` に `SecurityCategory` を 2 件追加する: `KeyPermissionFixed`(`key_permission_fixed`)・`KeyPermissionUnfixable`(`key_permission_unfixable`)。`SecurityCategory::ALL` を 14 件に更新(data-model §SecurityCategory)(T017・T023 の失敗確認後)
- [X] T019 [US2] `src/platform/mod.rs` に起動時パーミッション検査・是正(unix のみ)を実装する: 対象 data-dir → `0700`、`master.key`・`app.db`・`app.db-wal`・`app.db-shm` → `0600`。group/other ビット(`0o077`)判定、symlink は追従せず是正不能扱い、是正成功 → `key_permission_fixed` 記録・継続、是正失敗(`EPERM`/`EROFS`/IO エラー)→ `key_permission_unfixable` 記録・警告。記録パスは data-dir 相対名のみ。Windows では no-op(cli-config §4)(T018 完了後)
- [X] T020 [US2] `src/identity/` に `KeystoreHealth`(`Ok` / `Unavailable` + 原因)を導入する: `Unavailable` 時は全ペルソナ `usable: false`・鍵操作(作成・署名・エクスポート・破棄)は既存「利用不可」エラー応答・発見/伝搬は非影響。原因別(master.key 破損 / パーミッション是正不能 / 保護鍵消失疑い / 個別ペルソナ復号失敗)に異なる定型警告 — 鍵素材・絶対パス非含有(key-envelope §5「障害原因の識別」、data-model §KeystoreHealth)
- [X] T021 [US2] `src/main.rs` の起動順序を配線する: data-dir 作成(`0700`)→ Store オープン → keystore 初期化(master.key 読込/生成)→ パーミッション検査 → リスナーバインド。検査結果(KeystoreHealth)を IdentityManager へ渡す(cli-config §4 起動順序)(T019・T020 完了後)
- [X] T022 [P] [US2] `src/web/personas.rs` の DPAPI 依存文言をプラットフォーム中立な「保護された保管」表現へ変更する(挙動変更なし — cli-config §6)
- [X] T036 [US2] `tests/steps/` のステップ定義を実装し、T023 の cucumber シナリオ①〜④を事後アサーション(秘密鍵・nsec 非出力検査)を含めて全通過させる(T018〜T021 完了後)
- [ ] T024 [US2] Linux 実機で quickstart 検証 3 を実施する: 平文非保存(strings 検査 + `PYK1` scheme 0x02)、鍵分離(DB 単体持出しで復号不能)、他アカウント遮断(Permission denied)、`chmod 644` → 自動是正 + `key_permission_fixed`、`chown root` → 部分劣化 + 発見機能継続、Windows ノードとの相互発見(SC-005)、nsec エクスポート/破棄の同一意味論(FR-007)
  - 2026-07-06 部分実施済み(Linux/WSL2・実バイナリ): API でペルソナ作成 → `secret_enc` が `PYK1` + scheme 0x02、strings 検査で nsec1 出現 0・64 桁 hex は公開鍵のみ、master.key 32 bytes `0600`・data-dir `0700`、`chmod 644 master.key` → 再起動で `0600` へ自動是正 + `key_permission_fixed` 記録・ペルソナ `usable:true` 維持、標準出力・security.log に秘密鍵/nsec 非出力。**残(要 root・複数アカウント・Windows ノード環境): 他アカウント遮断、`chown root` 部分劣化(cucumber ③ が symlink 代表で自動検証済み)、DB 単体持出し(contract #8 で自動検証済み)、Windows 相互発見(SC-005)、nsec エクスポート/破棄の手動比較**
  - 2026-07-07 追実施(Debian 12 実機・root 検証): **`chown root master.key` → 部分劣化で欠陥を発見**(致命的エラー終了しノードが起動しない — FR-013 違反)。契約テスト #10(`unreadable_master_key_degrades_instead_of_failing`)を追加(失敗確認)→ `KeystoreInit::Unreadable` / `UnavailableCause::MasterKeyUnreadable` を導入して修正。修正後の実機再検証: 定型警告「保護鍵ファイルを読み取れません」+ 全ペルソナ `usable:false` + index.txt 200 継続 + 鍵操作は `persona_unusable` 拒否 + ログに秘密鍵/nsec 非出力を確認。他アカウント遮断も確認済み(`sudo -u nobody cat master.key app.db` → 両ファイルとも「許可がありません」— 利用者実施・2026-07-07)。**残(要 Windows ノード環境): Windows 相互発見(SC-005)、nsec エクスポート/破棄の手動比較**

**Checkpoint**: US1 と US2 が独立に動作(Linux で掲載ノードとして完全機能、SC-003/SC-005/SC-006)

---

## Phase 5: User Story 3 - systemd サービスとして常時稼働に適した振る舞い (Priority: P3)

**Goal**: unit 定義例で登録でき、READY/STOPPING 通知・SIGTERM での安全終了・ジャーナルログ・異常時自動再起動が機能する

**Independent Test**: 提供 unit 例でサービス登録し、`systemctl start/stop/status` と `journalctl` で起動・安全停止・ログ確認・`kill -9` 後の自動再起動ができること(quickstart 検証 4)

### Tests for User Story 3

- [X] T025 [P] [US3] `tests/integration/graceful_shutdown.rs` を新規作成し `Cargo.toml` に `[[test]]` 登録する(unix): SIGTERM 受信で graceful shutdown 経路により終了コード 0 で終了すること、`NOTIFY_SOCKET` に一時 UnixDatagram を指定した場合に全リスナーバインド後 `READY=1`・停止開始時 `STOPPING=1` が届くこと、`NOTIFY_SOCKET` 未設定でも正常稼働すること(FR-008/FR-009、systemd-service §1)

### Implementation for User Story 3

- [X] T026 [US3] `src/platform/mod.rs` に sd_notify を std のみで実装する(unix): `$NOTIFY_SOCKET` のパス(先頭 `@` は abstract socket として `\0` 読替)へ `UnixDatagram` で `READY=1` / `STOPPING=1` を送信。未設定なら no-op、送信失敗は debug ログのみで稼働へ影響させない(research R5、systemd-service §1)
- [X] T027 [US3] `src/platform/mod.rs` に `shutdown_signal()` 抽象を実装する: unix は `SIGTERM` + `SIGINT`(`tokio::signal::unix`)、Windows は `ctrl_c`(現行)(research R6)(T026 と同一ファイルのため順次)
- [X] T028 [US3] `src/main.rs` を配線する: `ctrl_c` 直書きを `platform::shutdown_signal()` へ差替え、全リスナーバインド成功後に `READY=1`、shutdown 開始時に `STOPPING=1` を送信してから既存 watch チャネル経路で全サブシステム停止(FR-008/FR-009、SC-004)(T026・T027 完了後)
- [X] T029 [US3] `src/main.rs` の tracing 初期化で出力先が端末でないとき ANSI エスケープを無効化する(`with_ansi(IsTerminal)` — research R8、FR-011)(T028 と同一ファイルのため順次)
- [X] T030 [P] [US3] `contrib/systemd/peca-p2p-yp.service` を新規作成する: contracts/systemd-service.md §2 の unit 全文(`Type=notify`・`StateDirectory=peca-p2p-yp`・`StateDirectoryMode=0700`・`UMask=0077`・`Restart=on-failure`・ハードニング群・`RestrictAddressFamilies` に `AF_UNIX` 含む・`TimeoutStopSec` 非指定)
- [X] T031 [US3] systemd 実機で quickstart 検証 4 を実施する: `systemctl start` → active(READY 後に返る)、`journalctl` に起動サマリ(ANSI・秘密鍵なし)、`systemctl stop` が 90 秒以内・`ExecMainStatus=0`(SC-004)、`kill -9` → 自動再起動、data-dir = `/var/lib/peca-p2p-yp`(systemd-service §3)、および上記すべてがデスクトップセッション・対話操作なしのヘッドレス環境(nologin のサービスアカウント)で完了すること(ヘッドレス無人稼働 — FR-005/SC-003)
  - 2026-07-06 注記: 実装(T025〜T030)は完了し、SIGTERM → exit 0・NOTIFY_SOCKET 経由 READY/STOPPING は統合テストで自動検証済み。本タスクの systemd 実機検証は開発環境(WSL2・systemd offline)では実施不能のため、systemd 稼働環境での実施が必要
  - 2026-07-07 完了(Debian 12 実機・systemd 稼働環境): quickstart 検証 4 の 1〜6 を利用者が実施・確認済み。systemd 側の残存証跡とも整合 — `Type=notify` の unit が `Result=success`・`ExecMainStatus=0`(graceful stop、SC-004)・`NRestarts=1`(`kill -9` → `Restart=on-failure` による自動再起動)・`User=peca-p2p-yp`(nologin サービスアカウントによるヘッドレス無人稼働)・`StateDirectory=peca-p2p-yp`(data-dir = `/var/lib/peca-p2p-yp`)

**Checkpoint**: 全ユーザーストーリーが独立に機能

---

## Phase 6: Polish & Cross-Cutting Concerns

**Purpose**: ドキュメント・最終検証・リリースゲート

- [X] T032 [P] `README.md` に Linux 導入・稼働手順と systemd サービス登録手順(quickstart 検証 4 の手順・複数インスタンス用テンプレート unit `peca-p2p-yp@.service` への言及を含む)を追記する(FR-012)
- [ ] T033 [P] Linux/Windows 実機で quickstart 検証 5〜7 を実施する: 複数インスタンス同時稼働(FR-010)、起動失敗の定型エラー(FR-014)、Windows 後方互換(レガシー DPAPI BLOB のペルソナが利用可・新規は `PYK1` scheme 0x01)
  - 2026-07-07 部分実施済み(Debian 12 実機・実バイナリ): **検証 5** — 2 インスタンスが独立 data-dir(各 `0700`・個別 `master.key 0600`/`app.db 0600`)で同時稼働、ピア登録 → 双方向 established(片側は PEX 学習 `source:"pex"`)を確認。**検証 6** — 特権ポート(`--p2p-bind 0.0.0.0:80`)は「P2P 待受アドレスにバインドできませんでした(権限が不足しています)」、使用中ポートは「HTTP 待受アドレスにバインドできませんでした(ポートが使用中です)」で exit 1、スタックトレース・内部パスなし。**残(要 Windows 実機): 検証 7 — 002 実装前 DB のレガシー DPAPI BLOB 後方互換の手動確認(読込後方互換自体は windows CI の契約テスト `legacy_dpapi_blob_without_magic_decrypts` で自動検証済み)**
- [X] T034 `specs/002-linux-support/quickstart.md` のトレーサビリティ表の各セルを実装済みテスト ID(ファイル名・テスト関数名)へ更新し、割り当てのないシナリオがないことを確認する
- [X] T035 最終ゲートを確認する: `cargo fmt -- --check`・`cargo clippy --all-targets`・`cargo audit` が緑、windows-latest / ubuntu-latest 両 CI ジョブで全テスト(cucumber 含む)が同一に通過(SC-002)、`SecurityCategory::ALL` 14 件の一致確認(data-model — リリース前ゲート)
  - 2026-07-07 完了: ローカル(Linux)で `cargo fmt -- --check`・`cargo clippy --all-targets`・`cargo audit`(脆弱性 0・許容済み警告 1 件のみ)・`cargo test` 全緑(unit 218・contract/integration 全通過・cucumber 21/21)。`SecurityCategory::ALL` は 14 件で data-model と一致(`security/mod.rs` の網羅 unit テストも通過)。PR #2 の CI run 28804098219 で windows-latest / ubuntu-latest 両ジョブ + Trivy が全て success(SC-002)。なお trivy ジョブの `pull-requests: read` 権限不足(PR イベントで 403)を発見し修正(01144ef)

---

## Dependencies & Execution Order

### Phase Dependencies

- **Phase 1(Setup)**: T001(ADR-0008)が全実装タスクの先頭ゲート(plan.md Constitution Check の条件)。T002 は T001 完了後
- **Phase 2(Foundational)**: Phase 1 完了後。**全ユーザーストーリーをブロックする**(keystore 抽象なしには Linux ビルド自体が不能 — FR-001)
- **Phase 3〜5(US1〜US3)**: Phase 2 完了後。US1 → US2 → US3 の優先順を推奨するが、相互依存はなく並行可能
- **Phase 6(Polish)**: 対象ストーリーの完了後

### User Story Dependencies

- **US1 (P1)**: Phase 2 完了のみに依存。他ストーリーへの依存なし(MVP)
- **US2 (P2)**: Phase 2 完了のみに依存(keystore 本体は Phase 2 で実装済み。US2 はパーミッション検査・部分劣化・テストで完成させる)。US1 と独立にテスト可能
- **US3 (P3)**: Phase 2 完了のみに依存。US1/US2 と独立にテスト可能(T031 の実機検証は US1 完了後が現実的)

### Within Each Phase

- Phase 2: T016(契約テスト作成・失敗確認)と T003 が起点(相互に並列)→ T004(T016 の失敗確認後)→ T005・T006(T004 後)→ T007(T004〜T006 後)→ T008・T009(T003 後、相互に並列)
- US2: T017・T023(テスト作成・失敗確認)→ T018 → T019 → T021、T020 → T021 → T036(T018〜T021 後)。T022 は他と並列
- US3: T026 → T027 → T028 → T029(mod.rs / main.rs の同一ファイル編集は順次)。T025・T030 は並列

### Parallel Opportunities

- Phase 2: T003 ∥ T016(失敗確認まで)、その後 T004 → T005 ∥ T006、最後に T008 ∥ T009
- US1: T010 ∥ T011 ∥ T012(テストと config.rs は独立ファイル)
- US2: T017 ∥ T023 ∥ T022 を同時に開始可能(実装 T018〜T021 はテストの失敗確認後)
- US3: T025 ∥ T030 は実装と独立に着手可能
- Phase 6: T032 ∥ T033
- Phase 2 完了後は US1・US2・US3 をチームで並行実施可能

---

## Parallel Example: User Story 2

```bash
# US2 のテスト(失敗確認まで)と独立した文言変更を同時に着手:
Task: "tests/contract/cli_config.rs にパーミッション是正テストを追加"      # T017
Task: "tests/features/security.feature にシナリオ骨子を追加(失敗確認)"    # T023
Task: "src/web/personas.rs の DPAPI 文言を中立化"                        # T022
# 実装(T018〜T021)は T017・T023 の失敗確認後に開始
```

---

## Implementation Strategy

### MVP First (User Story 1 Only)

1. Phase 1(T001 ADR ゲート → T002)を完了
2. Phase 2(Foundational)を完了 — 両 OS でビルド成功が確認点
3. Phase 3(US1)を完了し、Linux 実機 + CI(T014/T015)で独立検証
4. **ここで停止して評価可能**: Linux 発見・伝搬ノードとして提供できる(SC-001)

### Incremental Delivery

1. Setup + Foundational → 両 OS ビルド成立
2. US1 追加 → Linux 稼働ノード(MVP、SC-001/SC-002)
3. US2 追加 → Linux 掲載ノード(SC-003/SC-005/SC-006)
4. US3 追加 → systemd 常時稼働(SC-004)
5. Polish → README・トレーサビリティ・リリースゲート

### セキュリティシナリオ → テスト割り当て(quickstart トレーサビリティ表の具体化)

| Gherkin シナリオ(spec) | contract テスト | cucumber(定義: T023 / 通過: T036) | 手動 |
|---|---|---|---|
| 平文非永続化 | T016(key-envelope #1) | T023 ①/T036 | T024 |
| 別アカウント復号不能 | T016(key-envelope #8) | —(CI 不能) | T024 |
| パーミッション自動是正 | T017(cli-config §7 #3) | T023 ②/T036 | T024 |
| 是正不能の部分劣化 | T017(cli-config §7 #4) | T023 ③/T036 | T024 |
| 復号不能データの隔離 | T016(key-envelope #3, #4, #6) | T023 ④/T036 | T024 |
| 秘密鍵のログ非出力 | —(全テスト事後アサーション) | T023 各シナリオ/T036 | T024・T031 |
| バインド失敗時の非漏洩 | T011(cli-config §7 #5) | — | T033 |

FR-007(nsec エクスポート・破棄)は新規テストを追加せず、既存 001 の contract/cucumber テストが Linux CI(T014)で同一通過することで検証する(research R10)。

---

## Notes

- [P] タスク = 異なるファイル・未完了依存なし。同一ファイル(platform/mod.rs、main.rs)を編集するタスクは順次実行
- テストファースト(Principle IV): テストタスク(T010・T011・T016・T017・T023・T025)は、対応する実装タスクの開始前に「失敗する状態」を確認してから実装へ進む(constitution 実装中ゲート 5)
- 新規テストファイルは `Cargo.toml` への `[[test]]` 登録を忘れない(既存方式)
- 各タスク(または論理グループ)完了ごとにコミット。コミット前に `cargo fmt -- --check`(CLAUDE.md)
- T001(ADR-0008)完了前に実装へ着手しない(Constitution Check の条件付き PASS の成立条件)
- 秘密鍵・nsec・鍵素材をログ・テスト出力・エラー文言へ出さない(FR-011 — 全タスク共通の MUST NOT)

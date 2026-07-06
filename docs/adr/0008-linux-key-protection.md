# ADR-0008: Linux 鍵保護(keystore 抽象・エンベロープ・マスター鍵)

**Status**: Accepted
**Date**: 2026-07-06
**Principles**: Principle I (Safety First), Principle II (Security by Design), Principle V (Formal Verification), Principle VI (Principle Traceability)
**Task**: T001(002-linux-support Phase 1 — 全実装タスクの先頭ゲート)

## 背景

本ノードは秘密鍵保護を `identity::dpapi`(Windows DPAPI)へ直接依存しており Windows 専用と
なっている(ADR-0003 §1)。002-linux-support はこれを「keystore」抽象へ置き換え、単一
コードベースのまま Linux でもビルド・稼働可能にする(FR-001、FR-003〜FR-006)。

spec Assumptions は「マスター鍵の配置・AEAD 選定・`secret_enc` 識別方式は plan 段階で
ADR 化して確定する」と本 ADR へ委譲した(checklists/security.md CHK031)。本 ADR は
`specs/002-linux-support/research.md` R1〜R4 と contracts(key-envelope / cli-config /
systemd-service)を正として、以下 7 点を正式決定として転記・確定する。

## 決定

### 1. プラットフォーム分離方式: `cfg` 集約(trait object 不採用)— research R1

- `windows` クレートは `[target.'cfg(windows)'.dependencies]` へ移動し、Linux ビルドから
  Win32 依存を型レベルで排除する(FR-001 MUST NOT)
- プラットフォーム差異は 2 モジュールへ**集約**し、それ以外のモジュールから `cfg` 分岐を
  排除する:
  - `src/identity/keystore/` — 鍵保護。`#[cfg(windows)] mod dpapi;` /
    `#[cfg(unix)] mod file_key;`。共通入口は `keystore::protect(&[u8]) -> Vec<u8>` /
    `keystore::unprotect(&[u8]) -> Result<...>`(エンベロープ処理を含む — §3)
  - `src/platform/` — data-dir 解決・パーミッション検査/是正・sd_notify・shutdown シグナル
- 実行時ポリモーフィズム(`Keystore` trait + 注入)は**不採用**: 1 バイナリは
  1 プラットフォームでしか動かず、本番で選択肢が 1 つしかない過剰抽象である
  (Principle III — 最小複雑度)
- `cfg(target_os = "linux")` ではなく `cfg(unix)` で実装する(macOS 等の将来拡張余地。
  サポート宣言は Linux のみ)
- `keyring` クレートは**不採用**: Linux バックエンドが Secret Service(D-Bus・デスクトップ
  前提)であり、ヘッドレス要件(FR-005 MUST NOT)と自己完結制約に反する

### 2. AEAD 選定と依存評価: XChaCha20-Poly1305(RustCrypto)— research R2

- AEAD は **XChaCha20-Poly1305**(RustCrypto `chacha20poly1305` クレート)、鍵は
  32 バイト乱数マスター鍵(OS CSPRNG)、nonce は暗号化ごとに 24 バイト乱数を生成して
  暗号文へ前置する
- 鍵素材・復号後の秘密鍵・中間バッファは `zeroize` で使用後消去する(best-effort SHOULD —
  contracts/key-envelope.md §4)
- **選定理由**(Principle II「既存の暗号論的に安全なライブラリ / 自前暗号禁止」):
  - RustCrypto AEAD 群は 2020 年に NCC Group の公開監査を受けている
  - pure Rust でビルド依存を増やさない(bundled SQLite と同じ自己完結方針)
  - XChaCha20 の 192bit nonce は乱数生成での衝突確率が実用上無視でき、nonce カウンタの
    永続化という新たな整合性リスクを作らない(Principle I)
  - ペルソナ秘密鍵(32 バイト)の暗号化は低頻度・小データであり AES-NI の性能優位は無意味
- **依存評価(供給網)**: 新規依存は `chacha20poly1305` と `zeroize` の 2 クレートのみ。
  ともに RustCrypto プロジェクト傘下で活発に保守され、エコシステムで広く採用されている。
  既存 CI の `cargo audit`(RustSec)ゲートを継続適用して既知脆弱性を監視する(Principle I)
- **否定した代替**: AES-256-GCM(96bit nonce の衝突リスク管理が必要・非 HW 環境の
  タイミング特性で劣後)/ `ring`(XChaCha20 非対応・C/asm ビルド依存)/ age・libsodium
  バインディング(外部 C 依存または過剰機能)
- パスフレーズ派生(argon2)は FR-003 のとおり将来オプション: マスター鍵を argon2 派生鍵で
  ラップする 2 層構造(エンベロープの scheme 番号で表現 — §3 の予約値)に拡張可能とする

### 3. エンベロープ形式・scheme 識別・後方互換 — research R3 / contracts/key-envelope.md

`personas.secret_enc` を自己記述型エンベロープにする:

```text
secret_enc := magic "PYK1" (4 bytes) || scheme (1 byte) || payload
  scheme 0x01 = dpapi-user      (Windows: payload = DPAPI BLOB)
  scheme 0x02 = xchacha20-mk-v1 (unix:    payload = nonce(24) || ct_and_tag(48))
  (将来予約: 0x03 = argon2 ラップ付きマスター鍵 等 — 未知 scheme は常に Unusable)
```

- scheme 0x02 の AAD は `magic || scheme`(エンベロープヘッダの改竄で復号失敗となること)
- **読込**: magic あり → scheme で分岐し、現プラットフォームで復号不能な scheme
  (他プラットフォーム由来・未知値)は **Unusable**(パニック・起動失敗にしてはならない
  MUST NOT — FR-006)。magic なし → レガシー生 DPAPI BLOB とみなし、Windows では従来どおり
  復号(後方互換 MUST)、unix では Unusable。Unusable は当該ペルソナのみに影響し、起動・
  他ペルソナ・発見伝搬機能は継続する(ADR-0003 §4 と同一挙動)
- **書込**: 新規作成・再暗号化は常にエンベロープ形式(Windows も 0x01 で包む)。
  復号側のみ後方互換で書込みは常に新形式のため、形式ダウングレードの攻撃面は存在しない
- **レガシー残存の受容**: 既存レガシー BLOB の一括マイグレーションは行わず、使用時の
  書き換え(再暗号化)も行わない(読込専用)。レガシー形式が当該ペルソナの破棄まで無期限に
  残存することを**受容する** — DPAPI 保護自体は scheme 0x01 と同水準であり残存が保護水準を
  下げることはなく、書換え契機を作らないことで障害面を増やさない
- **ロールバック非対応の受容**: 新形式で保管した後の旧バージョンへのロールバックは
  スコープ外。旧実装はエンベロープを DPAPI BLOB として復号を試みて失敗し、既存挙動
  (ADR-0003 §4)どおり当該ペルソナのみ利用不可となる(起動失敗・データ破壊は生じない)
- DB への scheme 列追加は**不採用**(列とペイロードの不整合という新たな不変条件が必要。
  データ自身が自己記述する方が壊れにくい)

### 4. マスター鍵: 名称・配置・生成順序・生成競合 — research R4 / contracts/key-envelope.md §5

**用語の統一**: 本機能の 32 バイト鍵の正式名称は**「マスター鍵」**、ファイル名・パス表記は
**`master.key`** とする。利用者向けの警告・ログ文言では**「保護鍵」**と表記する
(例: 「保護鍵ファイルが破損している」— contracts/key-envelope.md §5 の定型文言)。
三者は同一物を指し、これ以外の呼称(「秘密鍵ファイル」等 — ペルソナ秘密鍵との混同を招く)は
用いない。

- **配置**: `<data-dir>/master.key`(32 バイト生バイナリ)。DB(`app.db`)と同居だが
  **別ファイル**であり、DB 単体の持出しでは復号不能(FR-003 MUST)。data-dir 内配置により
  複数インスタンス(FR-010)がインスタンスごとに独立した鍵を持てる
- **生成順序**: keystore 初期化時(起動時・リスナーバインド前)に存在しなければ
  `O_CREAT|O_EXCL` + mode `0600` で原子的に作成する(TOCTOU 回避 MUST)。対話操作を
  要求しない(MUST NOT — FR-005)。起動順序は data-dir 作成(`0700`)→ Store オープン →
  keystore 初期化 → パーミッション検査 → リスナーバインド(contracts/cli-config.md §4)
- **生成競合**: `O_EXCL` が `EEXIST` で失敗した場合(並行生成の競合)は既存ファイルの
  読込へフォールバックし、両プロセスが同一鍵に収束する(MUST)。これは競合を安全に収束
  させるための規定であり、同一 data-dir での複数プロセス同時稼働の保証ではない(サポート外)
- **消失疑いの警告**: 生成時点で DB に scheme 0x02 のペルソナが既に存在する場合は
  「保護鍵消失の可能性」を示す警告を記録してから生成する(MUST — 暗黙の新鍵生成により
  既存ペルソナが Unusable になる事象を利用者が識別できること)
- **ライフサイクル**: 読込時に 32 バイト一致を検証し、不一致 = 破損 → 全ペルソナ
  Unusable + 警告、発見・伝搬は継続。削除時は全ペルソナ復号不能となり復元手段は提供しない
  (ADR-0003 §3 と同思想。nsec エクスポートが唯一のバックアップ)
- data-dir 解決順は `--data-dir` > `$STATE_DIRECTORY`(unix)> `%APPDATA%\peca-p2p-yp`
  (Windows)/ `$XDG_STATE_HOME/peca-p2p-yp`(unix)> `~/.local/state/peca-p2p-yp`
  (contracts/cli-config.md §1)。`$CREDENTIALS_DIRECTORY`(systemd LoadCredential)は
  systemd 専用となり手動起動と両立しないため不採用(unit 例の将来拡張として言及に留める)

### 5. 脅威モデルの限界の受容 — contracts/key-envelope.md §4 / systemd-service.md §2

以下を**受容する**(spec Assumptions と同一。ADR-0004 脅威モデルの範囲内):

- root など data-dir 全体(master.key 込み)を読める主体、および実行アカウント自身の
  プロセスメモリを読める主体には保護されない
- scheme 0x02 の他アカウント遮断(FR-004)は**ファイルパーミッションのみ**に依存し、
  DPAPI(scheme 0x01)のユーザー鍵への暗号学的拘束より保証水準は低い — 追加常駐サービス・
  デスクトップ環境・Secret Service を要求しない自己完結制約(FR-003/FR-005)との
  トレードオフとして受容する
- メモリ上の鍵素材消去(zeroize)は best-effort(SHOULD)に留まる — Rust ではムーブ・
  コピー・レジスタ/スワップ残留まで消去を保証できず、プロセスメモリを読める主体は上記の
  とおり脅威モデル外である
- **停止タイムアウト超過時の SIGKILL 受容**(spec Edge Cases / contracts/systemd-service.md
  §2): graceful shutdown が systemd 既定タイムアウト 90 秒を超えた場合は systemd による
  `SIGKILL` 強制終了に委ね、アプリ側の追加フェイルセーフは実装しない。SQLite は WAL +
  トランザクション境界により強制終了でもデータ破壊に至らず次回起動時に通常復旧し、
  `master.key` は生成後に書き換えないため影響を受けない(Principle I — データ完全性)

### 6. パーミッション検査の範囲と意図的除外 — contracts/cli-config.md §4

起動時(keystore 初期化直後・リスナーバインド前)に unix でのみ実施する:

- **対象と是正値**: data-dir → `0700`、`master.key`・`app.db`・`app.db-wal`・`app.db-shm`
  (存在するもの)→ `0600`
- **判定基準**: group/other ビット(`0o077`)が 1 つでも立っていれば是正対象。owner ビット
  のみが既定より厳しい場合(例: `0400`)は是正しない(是正対象は「他ユーザーへの開放」のみ)
- symlink は追従せず**是正不能**として扱う(symlink 経由で第三者ファイルの mode を変更する
  事故・攻撃を避ける)
- 是正成功 → `key_permission_fixed` 記録・継続 / 是正失敗(`EPERM`・`EROFS`・IO エラー)→
  `key_permission_unfixable` 記録・警告 + 全ペルソナ利用不可。**起動と発見・伝搬機能は
  継続する**(MUST — FR-013)。記録パスは data-dir 相対名のみ(絶対パス非漏洩)
- Windows では no-op(DPAPI がアカウントスコープを担保)

**意図的除外**(検査対象外とする決定):

- **`security.log`**: 鍵素材・秘匿情報を含まず、data-dir `0700` が実質的な防壁となるため
  是正対象に含めない
- **POSIX ACL・共有グループ等**: パーミッションビット以外のアクセス経路は検査しない。
  ACL 付与は管理者の明示操作であり利用者の意図とみなす(既定インストールでは発生しない)
- **稼働中の再検査**: 行わない(起動時検査のみ)。稼働中にアクセス権を緩められるのは実行
  アカウント自身か root に限られ、常時監視(inotify 等)は複雑度に見合わない。次回起動時に
  検知・是正される

予防側(FR-013)は master.key の mode `0600` 明示作成(umask 非依存)+ data-dir `0700`
(主防壁)+ systemd 実行時の `UMask=0077` / `StateDirectoryMode=0700`(多層防御)で成立する。

### 7. Principle V(形式的検証)非適用の判定

本機能に PlusCal/TLA+ による形式的検証は**適用しない**と判定する(Principle V の MUST に
基づく明示判定 — plan.md Constitution Check):

- 新規の並行アルゴリズム・プロトコル状態機械は存在しない: 鍵保護(protect/unprotect)は
  逐次処理であり、shutdown は既存 watch チャネル経路(ADR-0005 で検証済みの gossip とは
  独立)の再利用である
- 「クリティカル」3 基準のうち第 1(並行性・分散合意)・第 2(独自プロトコル状態機械)を
  満たさない
- master.key 生成競合(§4)は `O_CREAT|O_EXCL` という OS が原子性を保証するプリミティブ
  1 点に還元されており、モデル化すべき状態空間がない

## 否定した選択肢

- **`Keystore` trait + 実行時注入** — 本番で選択肢が 1 つしかない過剰抽象(§1)
- **`keyring` クレート** — Secret Service 依存がヘッドレス要件に反する(§1)
- **AES-256-GCM / `ring` / age / libsodium** — nonce 運用・ビルド依存・過剰機能(§2)
- **DB への scheme 列追加** — 自己記述エンベロープの方が壊れにくい(§3)
- **既存 BLOB の一括再暗号化マイグレーション** — 復号不能ペルソナで失敗し部分劣化
  (FR-006)と両立しにくい。lazy(読めた形式のまま)を採用(§3)
- **`$CREDENTIALS_DIRECTORY`(systemd LoadCredential)** — 手動起動と両立しない(§4)
- **緩いパーミッションでの起動拒否** — 可用性(Principle I・常時稼働)を損なう。
  自動是正 + 部分劣化に確定済み(§6)

## 原則参照

- Principle I: 復号不能・是正不能時の部分劣化で可用性維持(FR-006/FR-013)、SIGKILL 受容の
  データ完全性評価(§5)、`cargo audit` による供給網監視(§2)
- Principle II: 監査済み AEAD・OS CSPRNG・最小権限(`0600`/`0700`)・自前暗号なし・
  秘密情報のログ非出力(FR-011)
- Principle V: 非適用判定と理由の明記(§7)
- Principle VI: 本 ADR は research R1〜R4・contracts(key-envelope / cli-config /
  systemd-service)・spec(FR-001, FR-003〜FR-006, FR-010, FR-013)へトレース可能
- 関連 ADR: ADR-0003(ペルソナ鍵管理 — §4 復号失敗挙動を維持)、ADR-0004(脅威モデル)、
  ADR-0006(loopback 強制 — 不変)

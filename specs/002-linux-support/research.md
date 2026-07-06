# Research: Linux 対応(002-linux-support)

**Date**: 2026-07-06 | **Plan**: [plan.md](./plan.md)

spec の Technical Context に NEEDS CLARIFICATION はないが、spec Assumptions が
「plan 段階で ADR 化して確定する」と委譲した事項(マスター鍵配置・AEAD 選定・
`secret_enc` 識別方式)と、実装方式の未確定点を本書で確定する。
R1〜R4 は **ADR-0008(docs/adr/0008-linux-key-protection.md)** へ転記して正式決定とする。

---

## R1: Windows 専用依存の分離方式(FR-001)

**Decision**: `windows` クレートを `[target.'cfg(windows)'.dependencies]` へ移動し、
プラットフォーム差異を 2 モジュールに集約する。

- `src/identity/keystore/` — 鍵保護。`#[cfg(windows)] mod dpapi;` / `#[cfg(unix)] mod file_key;`
  共通入口は `keystore::protect(&[u8]) -> Vec<u8>` / `keystore::unprotect(&[u8]) -> Result<...>`
  (エンベロープ処理を含む — R3)
- `src/platform/` — data-dir 解決・パーミッション是正・sd_notify・shutdown シグナル

**Rationale**:
- 1 バイナリは 1 プラットフォームでしか動かないため、実行時ポリモーフィズム(trait object)
  は不要。`cfg` によるコンパイル時選択が最小複雑度(Principle III — 読みやすさ)
- ターゲット依存化により Linux ビルドから Win32 クレートが完全に消え、
  「Windows 専用機能への直接依存でビルド失敗」(FR-001 MUST NOT)を型レベルで排除
- 分岐を 2 モジュールへ集約することで、`cfg` の散在(保守困難・テスト漏れの温床)を防ぐ

**Alternatives considered**:
- **`Keystore` trait + 実行時注入**: テスト差替えは容易だが、本番で選択肢が 1 つしかなく
  過剰抽象。却下
- **`keyring` クレート(クロスプラットフォーム鍵保管)**: Linux バックエンドが Secret
  Service(D-Bus・デスクトップ前提)で、ヘッドレス要件(FR-005 MUST NOT)と自己完結制約
  (spec Assumptions / ADR-0003 research R6)に反する。却下
- **`cfg(target_os = "linux")`**: macOS 等の将来拡張余地(spec Assumptions)を考慮し、
  keystore/platform は `cfg(unix)` で実装する(サポート宣言は Linux のみ)

## R2: Linux at-rest 保護の AEAD・鍵素材(FR-003, FR-004)

**Decision**: **XChaCha20-Poly1305**(RustCrypto `chacha20poly1305` クレート)+
32 バイト乱数マスター鍵 + 24 バイト乱数 nonce(暗号文へ前置)。鍵素材は `zeroize` で
使用後消去する。マスター鍵は `OsRng`(OS CSPRNG)で生成する。

- 暗号化: `ciphertext = XChaCha20-Poly1305(key=master, nonce=random24, aad=envelope_header)`
- `secret_enc` ペイロード = `nonce(24) || ciphertext(平文32 + tag16)`(R3 のエンベロープに格納)

**Rationale**:
- Principle II「既存の暗号論的に安全なライブラリ」: RustCrypto AEAD 群は 2020 年に
  NCC Group の公開監査を受けており、pure Rust でビルド依存を増やさない(bundled SQLite と
  同じ自己完結方針)
- XChaCha20 の 192bit nonce は乱数生成での衝突確率が実用上無視でき、nonce 管理状態を
  持たずに済む(カウンタ永続化という新たな整合性リスクを作らない — Principle I)
- ペルソナ秘密鍵(32 バイト)の暗号化は低頻度・小データであり、AES-NI の性能優位は無意味
- パスフレーズ派生(argon2)は FR-003 のとおり将来オプション: マスター鍵を argon2 派生鍵で
  ラップする 2 層構造に拡張可能な形式とする(エンベロープのスキーム番号で表現 — R3)
- 供給網評価: `chacha20poly1305`・`zeroize` はともに RustCrypto プロジェクト傘下で活発に
  保守され、エコシステムで広く採用されている(依存追加はこの 2 クレートのみ)。既存 CI の
  `cargo audit`(RustSec)ゲートを継続適用して既知脆弱性を監視する(Principle I)

**Alternatives considered**:
- **AES-256-GCM**(`aes-gcm`): 96bit nonce は乱数運用での衝突リスク管理が必要。ハードウェア
  支援のない環境ではタイミング特性も XChaCha20 に劣後。却下
- **`ring`**: 監査実績はあるが XChaCha20 非対応・ビルドに C/asm を含む。却下
- **age / libsodium バインディング**: 外部 C 依存またはファイル形式の過剰機能。却下

## R3: `secret_enc` エンベロープと Windows 後方互換(FR-001, FR-006)

**Decision**: `secret_enc` を自己記述型エンベロープにする。

```text
secret_enc = magic "PYK1" (4 bytes) || scheme (1 byte) || payload
  scheme 0x01 = dpapi-user      (Windows: payload = DPAPI BLOB)
  scheme 0x02 = xchacha20-mk-v1 (Linux:   payload = nonce(24) || ct+tag)
  (将来予約: 0x03 = argon2 ラップ付きマスター鍵 等)
```

- **読込**: magic あり → scheme を判定し、現プラットフォームで復号可能なら復号、
  不可能な scheme(他プラットフォーム由来)は `Unusable`(FR-006 の「持込 → 当該ペルソナ
  のみ利用不可」)
- **後方互換**: magic なし → レガシー生 DPAPI BLOB とみなす。Windows では従来どおり復号、
  Linux では `Unusable`。既存 Windows インストールの DB はマイグレーション不要
- **書込**: 新規作成は常にエンベロープ形式(Windows も 0x01 で包む)

**Rationale**:
- FR-006 は「復号できない」を検知して部分劣化する必要があり、スキーム識別子があれば
  「別プラットフォーム由来」を試行錯誤なしに判定できる
- DPAPI BLOB に固定マジックはあるが(provider GUID)、それへの依存はドキュメント外の
  実装詳細依存になる。自前エンベロープが確実
- 復号側のみ後方互換とし書込みは常に新形式のため、形式のダウングレードは発生しない

**Alternatives considered**:
- **DB に scheme 列を追加**: スキーマ変更 + 列とペイロードの不整合という新たな不変条件が
  必要。データ自身が自己記述する方が壊れにくい。却下
- **既存 BLOB の一括再暗号化マイグレーション**: 起動時の全ペルソナ復号は復号不能ペルソナ
  で失敗し、部分劣化(FR-006)と両立しにくい。lazy(読めた形式のまま)を採用。却下

## R4: マスター鍵ファイルとディレクトリ規約(FR-003, FR-010)

**Decision**:

- マスター鍵ファイル: `<data-dir>/master.key`(32 バイト生バイナリ、`0600`、初回起動時に
  生成。生成は `O_CREAT|O_EXCL` + mode 0600 で TOCTOU を避ける)。DB(`app.db`)と同居
  だが**別ファイル**であり、DB 単体の持出しでは復号不能(FR-003 MUST)
- data-dir 解決順(両 OS 共通の優先順位):
  1. `--data-dir`(CLI — 既存、複数インスタンスの分離手段: FR-010)
  2. `$STATE_DIRECTORY`(systemd `StateDirectory=` が注入。unix のみ)
  3. Windows: `%APPDATA%\peca-p2p-yp` / Linux: `$XDG_STATE_HOME/peca-p2p-yp`
     (未設定時 `~/.local/state/peca-p2p-yp`)
- data-dir は `0700` で作成する(unix)

**Rationale**:
- XDG Base Directory 仕様で DB・ログ等の「状態」は `XDG_STATE_HOME` が正
  (`XDG_DATA_HOME` はユーザーデータ、`XDG_CONFIG_HOME` は設定。本アプリは設定も DB 内
  のため state に一本化 — FR-010 SHOULD)
- `StateDirectory=peca-p2p-yp` で systemd が `/var/lib/peca-p2p-yp` を所有権付きで用意し
  `$STATE_DIRECTORY` を注入する — サービスアカウント運用でホーム不要、DynamicUser とも
  両立(contracts/systemd-service.md)
- マスター鍵を data-dir 内に置くことで「実行アカウントのディレクトリに保管」(spec
  Clarifications)を満たしつつ、複数インスタンス(FR-010)がインスタンスごとに独立した
  鍵を持てる

**Alternatives considered**:
- **`$CREDENTIALS_DIRECTORY`(systemd LoadCredential)**: 強力だが systemd 専用となり
  手動起動(spec Assumptions)と両立しない。unit 例の将来拡張としてコメント言及に留める
- **設定/状態のディレクトリ分離(XDG_CONFIG + XDG_STATE)**: 設定は SQLite 内にあり
  分離対象のファイルが存在しない。過剰。却下

## R5: systemd 準備完了通知(FR-009)

**Decision**: `sd_notify` プロトコルを std のみで自前実装する(`src/platform/` 内、
unix のみ、約 30 行)。`NOTIFY_SOCKET` 環境変数があれば `UnixDatagram` で
`READY=1`(全リスナーのバインド完了後)と `STOPPING=1`(shutdown 開始時)を送る。
未設定なら no-op(FR-009: 通知できなくても正常稼働 MUST)。unit は `Type=notify` を
既定とし、`Type=simple` でも動作する。

**Rationale**:
- プロトコルは「環境変数のパスへ datagram を 1 発送る」だけで、暗号でも入力検証境界でも
  ない。依存追加(`sd-notify`/`libsystemd`)より std 実装が自己完結制約に合う
- READY をバインド完了後に送ることで、`systemctl start` の成功 = 待受開始済みが保証され、
  依存サービスの起動順制御が正しくなる(SC-001 の「手順どおりで起動完了」に寄与)
- abstract socket(`@` 前置)にも対応する(コンテナ環境で使われる)

**Alternatives considered**:
- **`sd-notify` クレート**: 小さく健全だが、30 行のために依存を増やす価値がない。却下
- **`Type=simple` のみ(通知なし)**: FR-009 SHOULD を満たさない。READY 不明のため
  再起動ループ検知や依存順制御が甘くなる。却下

## R6: 停止シグナルと graceful shutdown(FR-008, SC-004)

**Decision**: 既存の `watch::channel` shutdown 伝播はそのまま使い、トリガーを
プラットフォーム抽象 `platform::shutdown_signal()` に差し替える。
unix: `SIGTERM` + `SIGINT`(`tokio::signal::unix`)/ Windows: `ctrl_c`(現行)。
`STOPPING=1` 送信(R5)後に既存経路で全サブシステムを停止する。

**Rationale**:
- systemd の停止は SIGTERM(既定 90 秒後に SIGKILL)。現行実装は Ctrl+C しか拾わないため、
  SIGTERM 対応が SC-004 の唯一の欠落点
- 現行 shutdown 経路は全タスクが `watch` を select しており、実測で数秒以内に完了する
  構造(sweep 周期等の待ちはすべて select 側)。90 秒制約に対し設計変更不要
- unit に `TimeoutStopSec` は書かず systemd 既定(90 秒)に委ねる(spec Clarifications)

**Alternatives considered**:
- **SIGHUP での設定再読込**: スコープ外(spec に要求なし)。unit 例にも含めない

## R7: パーミッション検査・自動是正(FR-013)

**Decision**: 起動時(Store/keystore オープン直後)に unix でのみ実施:

- 対象: `master.key`・`app.db`(および存在すれば `app.db-wal` / `app.db-shm`)→ `0600`、
  data-dir → `0700`
- group/other ビット(`0o077`)が立っていれば `chmod` で是正し、SecurityLog へ
  `key_permission_fixed` を記録して継続
- 是正失敗(他ユーザー所有等)は `key_permission_unfixable` を記録して警告し、
  keystore を「全ペルソナ利用不可」モードで初期化(`list` は `usable:false`、
  `signing_keys` は `Unusable`)。発見・伝搬機能は通常起動(FR-013 MUST / spec
  Clarifications「共有保管物 → 全ペルソナ影響・US1 継続」)
- SQLite の新規ファイル作成モードは接続オープン前に data-dir を `0700` にすることと
  プロセス `umask` に依存するため、unit 例で `UMask=0077` を指定し、手動起動向けには
  起動直後の検査(上記)が是正する

**Rationale**:
- 「検知 → 自動是正 → セキュリティイベント記録 → 是正不能なら部分劣化」が spec
  Clarifications の決定そのもの。SecurityCategory を 2 件追加(`ALL` は 14 件になる —
  data-model.md)
- WAL/SHM は DB 内容の断片を含むため DB 本体と同等に扱う

**Alternatives considered**:
- **緩いパーミッションで起動拒否**: 可用性(Principle I・常時稼働)を損なう。spec で
  自動是正に確定済み。却下

## R8: ログとジャーナル(FR-011)

**Decision**: 現行の tracing → stdout をそのまま journald に捕捉させる
(`StandardOutput=journal` は systemd 既定)。変更は 1 点のみ: 出力先が端末でないとき
ANSI エスケープを無効化する(`with_ansi(IsTerminal)`)。journald ネイティブ接続や
JSON 形式は導入しない。秘密鍵・nsec の非出力は既存規約(ADR-0003 §2)を維持し、
新規コード(keystore/platform)にも同じ MUST NOT を適用する。

**Rationale**: stdout 捕捉で FR-011 SHOULD は充足。`tracing-journald` は依存追加に
見合う便益(構造化フィールド)が現状ない。ANSI 無効化はジャーナル可読性の実害への
最小修正。

**Alternatives considered**: `tracing-journald`(却下 — 上記)、syslog(却下 — 時代遅れ)

## R9: unit 定義例とハードニング(FR-012)

**Decision**: `contrib/systemd/peca-p2p-yp.service` を提供(system サービス、専用
サービスアカウント + `StateDirectory=`)。骨子:

```ini
[Service]
Type=notify
ExecStart=/usr/local/bin/peca-p2p-yp
User=peca-p2p-yp
StateDirectory=peca-p2p-yp
UMask=0077
Restart=on-failure
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
```

詳細(全文・導入手順・複数インスタンス用テンプレート unit `peca-p2p-yp@.service` の
言及)は contracts/systemd-service.md と quickstart.md に置く。

**Rationale**:
- `Restart=on-failure` で異常終了時の自動復帰(US3 シナリオ 4)
- ハードニング群は最小権限(Principle II)。`RestrictAddressFamilies` に AF_UNIX を
  含めるのは `NOTIFY_SOCKET`(R5)のため
- user サービス(`systemctl --user`)は lingering 設定が必要でヘッドレス導入の落とし穴に
  なるため、system サービスを第一の提供物とし、user サービスは quickstart で言及に留める

**Alternatives considered**:
- **DynamicUser=yes**: StateDirectory と両立はするが、トラブルシュート(ファイル所有者が
  動的 UID)が導入障壁。既定例は静的ユーザーとし、コメントで言及

## R10: CI での SC-002 検証

**Decision**: `.github/workflows/ci.yml` に `ubuntu-latest` の build + test(fmt/clippy 含む)
ジョブを追加し、既存 `windows-latest` ジョブと並走させる。両ジョブで契約・統合・cucumber
テストを同一に通すことを SC-002 のゲートとする。

**Rationale**: SC-002「両プラットフォームで既存テストが同一に通過」の自動化。
Principle III(静的解析 CI 必須)を Linux ターゲットにも適用。

**Alternatives considered**: クロスコンパイルのみ(テスト未実行 — SC-002 を検証できず却下)

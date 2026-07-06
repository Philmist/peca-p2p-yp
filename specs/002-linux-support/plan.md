# Implementation Plan: Linux 対応(常時稼働ノード・systemd 親和)

**Branch**: `002-linux-support` | **Date**: 2026-07-06 | **Spec**: [spec.md](./spec.md)

**Input**: Feature specification from `/specs/002-linux-support/spec.md`

## Summary

DPAPI 直接依存で Windows 専用となっている本ノードを、単一コードベースのまま Linux でも
ビルド・稼働可能にする。技術的核心は 3 点:

1. **鍵保護の抽象化**: `identity::dpapi` を「keystore」抽象へ置き換え、`secret_enc` に
   自己記述型エンベロープ(マジック + スキーム識別子)を導入する。Windows は DPAPI
   (既存 BLOB は後方互換で読める)、Linux は「初回起動時に乱数生成したマスター鍵ファイル
   (`0600`)+ XChaCha20-Poly1305 AEAD」で保護する(FR-003〜FR-006、ADR-0003 と整合)。
2. **プラットフォーム配線**: `windows` クレートを `cfg(windows)` ターゲット依存へ移し、
   data-dir 解決(`%APPDATA%` → XDG/`STATE_DIRECTORY`)、SIGTERM ハンドリング、
   パーミッション検査・自動是正(FR-013)を追加する(FR-001、FR-008、FR-010)。
3. **systemd 親和**: `NOTIFY_SOCKET` への READY/STOPPING 通知(FR-009)、ジャーナル向け
   ログ出力(FR-011)、ハードニング済み unit 定義例と導入手順(FR-012)を提供する。

ネットワークプロトコル・イベントスキーマ・信頼境界・loopback 強制(ADR-0006)は不変。

## Technical Context

**Language/Version**: Rust(edition 2024、stable toolchain — 既存 CI と同一)

**Primary Dependencies**: tokio / axum / nostr / rusqlite(bundled)/ rand / tracing(既存)。
`windows` クレートは `[target.'cfg(windows)'.dependencies]` へ移動。
新規: `chacha20poly1305`(RustCrypto、XChaCha20-Poly1305 — research R2)、
`zeroize`(鍵素材の消去 — research R2)。sd_notify は std のみで自前実装(research R5)

**Storage**: SQLite(`app.db` — 変更なし)+ **マスター鍵ファイル `master.key`(Linux 新規、
data-dir 直下・`0600`)**。`personas.secret_enc` はエンベロープ形式へ(後方互換あり —
contracts/key-envelope.md)

**Testing**: `cargo test`(unit + contract + integration)、`cucumber`(Gherkin/BDD)。
CI に Linux(ubuntu-latest)ビルド・テストジョブを追加し SC-002 を検証

**Target Platform**: Windows 10+(既存)/ Linux x86_64(systemd 採用ディストリビューション
第一。systemd なしでも手動起動可 — spec Assumptions)

**Project Type**: 単一 Rust バイナリ(常駐ノード + loopback HTTP UI)

**Performance Goals**: 既存(spec 001)と同一。新規は SC-004 のみ: 停止要求から安全終了まで
systemd 既定タイムアウト 90 秒以内(現行 graceful shutdown 経路を SIGTERM に接続)

**Constraints**: 自己完結(追加常駐サービス・デスクトップ環境・Secret Service を要求しない —
FR-003/FR-005)、ヘッドレス無人起動、loopback 強制不変(ADR-0006)、秘密鍵の平文非永続化・
ログ非出力(FR-003/FR-011)

**Scale/Scope**: 既存規模と同一(スケール要件の変更なし)。同一ホスト複数インスタンスは
data-dir + ポート個別指定で両立(FR-010 SHOULD — 既存 `--data-dir`/バインド上書きで成立)

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

| Gate | 原則 | 判定 | 根拠 |
|------|------|------|------|
| リスク評価の実施 | I | PASS | 本表・research R2/R3 に記載。最大リスクは「Linux 保管データの at-rest 保護水準」で、DPAPI 同等(実行アカウントスコープ)をマスター鍵ファイル + パーミッションで達成。復号不能・是正不能時は部分劣化で可用性を維持(FR-006/FR-013) |
| 既知脆弱性の不在 | I | PASS | 新規依存は RustCrypto `chacha20poly1305`(NCC Group 監査済み)と `zeroize` のみ。`cargo audit` は既存 CI ゲートで継続 |
| 入力検証・trust nothing | II | PASS | ネットワーク入力経路に変更なし。新規入力はローカルファイル(master.key)のみで、長さ・パーミッション検証を行う(contracts/key-envelope.md) |
| 既存の暗号論的に安全なライブラリ / 自前暗号禁止 | II | PASS | AEAD は RustCrypto `chacha20poly1305`(監査済み・pure Rust)。鍵生成は OS CSPRNG。自前アルゴリズムなし(research R2) |
| 最小権限 | II | PASS | 鍵ファイル/DB `0600`・data-dir `0700`・自動是正(FR-013)。unit 例は NoNewPrivileges / ProtectSystem=strict / UMask=0077 等でハードニング(contracts/systemd-service.md) |
| セキュリティ設計決定の ADR 化 | II, VI | PASS(条件付き) | 鍵エンベロープ・AEAD 選定・マスター鍵配置は **ADR-0008** として実装フェーズ冒頭に確定する(research.md の決定を正として転記)。spec Assumptions が plan 段階での ADR 化を要求 → 本 plan の research.md が草案、tasks で ADR-0008 作成をゲート化 |
| エラーの内部情報非漏洩 | II | PASS | FR-014。既存の定型メッセージ方式(main.rs / config.rs)を踏襲 |
| Gherkin 振舞い定義 | IV | PASS | spec にセキュリティシナリオ 7 本 + 受け入れシナリオを記載済み。cucumber テストへの対応付けを quickstart.md / tasks で行う。ネガティブシナリオ(緩いパーミッション・復号不能データ・バインド失敗)を含む |
| 形式的検証の要否判定 | V | PASS(対象外と判定) | 新規の並行アルゴリズム・プロトコル状態機械はない(鍵保護は逐次処理、shutdown は既存 watch チャネル経路の再利用)。「クリティカル」3 基準の第 1・2 を満たさないため PlusCal 対象外。判定理由は ADR-0008 に明記する(Principle V の MUST) |
| 原則トレーサビリティ | VI | PASS | 本表・research.md・contracts が原則番号を明記 |

**違反なし** → Complexity Tracking は空。

**Post-Phase 1 再評価**(2026-07-06): data-model / contracts 生成後も上記判定に変更なし。
新設 SecurityCategory(`key_permission_fixed` / `key_permission_unfixable`)はセキュリティ
ログ要件(Security Requirements)に適合。エンベロープ後方互換(Windows 既存 BLOB)は
復号経路のみで書込みは常に新形式 — ダウングレード攻撃面なし(contracts/key-envelope.md §4)。

### ADR-0008 転記項目(実装フェーズ冒頭のゲート)

research R1〜R4 と contracts を正として、以下を `docs/adr/0008-linux-key-protection.md` に
確定・転記する。**tasks.md はこの ADR 作成を実装タスクの先頭ゲートとすること**(spec
Assumptions の要求。checklists/security.md CHK031 対応):

1. プラットフォーム分離方式(`cfg` 集約・trait object 不採用 — research R1)
2. AEAD 選定(XChaCha20-Poly1305)と依存評価(`chacha20poly1305` / `zeroize` の監査・
   保守・供給網 — research R2)
3. エンベロープ形式・scheme 識別方式・レガシー後方互換と「レガシー残存の受容」
   「ロールバック非対応」(research R3、contracts/key-envelope.md §3)
4. master.key 配置パス(`<data-dir>/master.key`)・生成順序・ライフサイクル・生成競合
   (research R4、contracts/key-envelope.md §5)。あわせて用語(「保護鍵」「マスター鍵」
   「master.key」)の正式名称と表記を統一する
5. 脅威モデルの限界の受容(root・data-dir 全体持出し・パーミッション依存の保証水準差 —
   contracts/key-envelope.md §4、spec Assumptions)、および停止タイムアウト超過時の
   `SIGKILL` 受容(contracts/systemd-service.md §2「停止タイムアウト超過時の受容」)
6. パーミッション検査の範囲と意図的除外(ACL・security.log・稼働中再検査 —
   contracts/cli-config.md §4)
7. Principle V(形式的検証)非適用の判定理由(本表「形式的検証の要否判定」)

## Project Structure

### Documentation (this feature)

```text
specs/002-linux-support/
├── plan.md              # This file (/speckit-plan command output)
├── research.md          # Phase 0 output (/speckit-plan command)
├── data-model.md        # Phase 1 output (/speckit-plan command)
├── quickstart.md        # Phase 1 output (/speckit-plan command)
├── contracts/           # Phase 1 output (/speckit-plan command)
│   ├── key-envelope.md      # secret_enc エンベロープ・master.key 契約
│   ├── systemd-service.md   # unit 定義例・サービス振舞い契約
│   └── cli-config.md        # data-dir 解決順・CLI/環境変数の契約
└── tasks.md             # Phase 2 output (/speckit-tasks command - NOT created by /speckit-plan)
```

### Source Code (repository root)

```text
src/
├── identity/
│   ├── mod.rs           # IdentityManager(dpapi 直接呼出しを keystore 経由へ置換)
│   └── keystore/        # 新規: 鍵保護抽象
│       ├── mod.rs       #   エンベロープ encode/decode + protect/unprotect 入口
│       ├── dpapi.rs     #   cfg(windows): 既存 dpapi モジュールを移設
│       └── file_key.rs  #   cfg(unix): master.key 読込/生成 + XChaCha20-Poly1305
├── platform/            # 新規: プラットフォーム差異の単一集約点
│   └── mod.rs           #   data-dir 解決(APPDATA/XDG/STATE_DIRECTORY)、
│                        #   パーミッション検査・是正(unix)、sd_notify(unix)、
│                        #   shutdown シグナル(ctrl_c + SIGTERM)
├── security/mod.rs      # SecurityCategory 2 件追加(key_permission_fixed / _unfixable)
├── store/mod.rs         # open_default の Linux 対応(platform::data_dir 使用)
├── config.rs            # 変更最小(--data-dir は既存。ヘルプ文言の脱 Windows 化)
├── main.rs              # resolve_data_dir 差替え・シグナル・sd_notify・keystore 健全性起動処理
└── web/personas.rs      # 文言の DPAPI 依存表現を中立化(挙動変更なし)

contrib/
└── systemd/
    └── peca-p2p-yp.service   # unit 定義例(FR-012 の提供物)

tests/
├── contract/key_envelope.rs          # 新規: エンベロープ形式・後方互換の契約テスト
├── contract/cli_config.rs            # 新規: data-dir 解決順・パーミッション是正の契約テスト
├── integration/platform_startup.rs   # 新規: 起動失敗定型エラー・複数インスタンスの統合テスト
├── integration/graceful_shutdown.rs  # 新規(unix): SIGTERM / sd_notify の統合テスト
├── integration/                      # 既存フローは両 OS で同一に通す(SC-002)
└── cucumber(features/)               # 新規シナリオ: at-rest 保護・パーミッション是正・
                                      # 復号不能隔離・SIGTERM 安全終了(spec Gherkin 対応)

.github/workflows/ci.yml      # ubuntu-latest の build+test ジョブ追加(SC-002)
docs/adr/0008-linux-key-protection.md  # 実装フェーズ冒頭で確定(research.md を正とする)
README.md                      # Linux 導入・systemd 手順の追記(FR-012)
```

**Structure Decision**: 既存の単一クレート構成を維持する。プラットフォーム差異は
`src/platform/`(実行環境)と `src/identity/keystore/`(鍵保護)の 2 箇所へ集約し、
それ以外のモジュールから `cfg` 分岐を排除する。runtime トレイトオブジェクトではなく
`cfg` によるコンパイル時選択とする(1 バイナリ 1 プラットフォームであり、実行時切替は
不要 — research R1)。

## Complexity Tracking

> **Fill ONLY if Constitution Check has violations that must be justified**

違反なし(記載事項なし)。

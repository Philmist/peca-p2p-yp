# ADR-0001: セキュリティスキャンツールの選定と配置方針

**Status**: Accepted
**Date**: 2026-07-02
**Principles**: Principle I (Safety First), Principle II (Security by Design)

## 背景

コンスティテューション Principle II は「依存ライブラリはセキュリティ監査済み版を使用し、
定期的に更新しなければならない」と定める。また Principle I のユーザー安全を守るため、
シークレット（APIキー・パスワード等）がリポジトリに混入することを防がなければならない。

CI/CDパイプラインへのセキュリティスキャン組み込みとpre-commitフックの配置方針を決定する。

## 決定

以下のツールを採用する:

| ツール | バージョン | 役割 | 実行タイミング |
|--------|-----------|------|--------------|
| **Gitleaks** | 8.30.1 | シークレット検出 | pre-commit + CI |
| **Trivy** | 0.72.0 | 依存CVEスキャン | CI（依存ファイル変更時）|
| **Dependabot** | GitHub組み込み | 依存自動更新PR | プッシュ毎（GitHub側） |

静的コード解析（SAST）ツールは実装言語確定後に別ADRで決定する。

## Gitleaks をpre-commitに配置する理由

シークレットがgitヒストリーに一度でも入ると、その後の削除・rebaseを行っても
ヒストリーに残り続け、被害が不可逆になる。コミット前に阻止するのが最も合理的な場所。

`gitleaks protect --staged` はステージ済みファイルのみをスキャンするため高速（200ms程度）であり、
開発フローへの影響が最小限である。

## Trivy をpre-commitに配置しない理由

依存ファイル（lock file等）が変わらないコミットではTrivyを走らせても無意味である。
また脆弱性DBのダウンロードを伴うため、毎コミット実行は遅延が大きく開発リズムを壊す。
CIで依存ファイル変更時にのみ実行するほうが費用対効果が高い。

## 実装

- フックスクリプト: `.githooks/pre-commit`
- git設定: `core.hooksPath = .githooks`（`.git/config` に記録済み）
- Gitleaks設定: `.gitleaks.toml`（デフォルトルールセットを継承）

新規クローン後のセットアップ:

```powershell
git config core.hooksPath .githooks
```

## 否定した選択肢

- **`.git/hooks/` への直接配置**: バージョン管理できないため却下。
  `.githooks/` に置いて `core.hooksPath` で参照する方式を採用。
- **pre-commitフレームワーク（Python製）**: 追加の依存が増えるため却下。
  ツールが直接PATHで呼べる状況では不要な抽象層となる。
- **Trivy をpre-commitに追加**: 上記の理由で却下。

## 結果として期待する状態

- シークレットを含むコミットはローカルで即座にブロックされる
- 依存ライブラリのCVEはCI実行時に検出される
- 脆弱な依存ライブラリのアップデートPRがDependabotにより自動作成される

---

## 追補: Rust 静的コード解析（SAST）ツール選定

**Date**: 2026-07-03
**Principles**: Principle II (Security by Design), Principle III (Code Quality and Review)

### 背景

本ADR原版では「静的コード解析（SAST）ツールは実装言語確定後に別ADRで決定する」と保留していた。
Phase 1（T001）で実装言語を **Rust（stable, edition 2024）** と確定したため、SAST ツールを選定する。
constitution Follow-up TODO `SAST_TOOL` を本追補で解消する。

### 決定

Rust プロジェクトに対し以下のツールを採用する:

| ツール | 役割 | 実行タイミング |
|--------|------|--------------|
| **Clippy** | 静的解析・慣用句チェック・セキュリティ観点の lint | CI（`cargo clippy -- -D warnings`）|
| **cargo audit** | `Cargo.lock` 依存クレートの既知 CVE スキャン | CI（`cargo audit`）|

`Cargo.toml` の `[lints.rust]` に `warnings = "deny"` を設定し、
コンパイラ警告をエラーとして扱うことでコード品質の継続的な維持を保証する。

### 採用理由

- **Clippy**: Rust の公式リンター。コンパイラと同梱されるため外部ツール導入不要。
  `unsafe` ブロックや整数オーバーフロー・未処理エラー等のセキュリティ観点の lint を含む。
  `-D warnings` で警告をエラー扱いにすることで CI の強制ゲートとして機能する。
- **cargo audit**: `Cargo.lock` の依存クレートを RustSec Advisory Database と照合する。
  Trivy の Rust エコシステムカバレッジを補完し、より精度の高い Rust 固有の CVE 検出が可能。

### 否定した選択肢

- **semgrep / CodeQL（Rust ルールセット）**: 現時点での Rust ルールセットの成熟度が Clippy に
  劣るため却下。Clippy の lint カバレッジで本プロジェクトの要件は充足する。
- **cargo-geiger（unsafe 検出）**: 現フェーズでは不使用だが、unsafe ブロック導入時に追加を検討する。

### CI 統合

`.github/workflows/ci.yml` の `ci` ジョブに以下を組み込み済み:
- `cargo clippy --locked -- -D warnings`
- `cargo audit`（`cargo-audit` クレートを CI 上でインストール）

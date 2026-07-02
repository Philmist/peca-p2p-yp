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

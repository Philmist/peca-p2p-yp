# Implementation Plan: 読み取り専用 index.txt の LAN 公開(オプトイン)

**Branch**: `feature/lan-index-txt` | **Date**: 2026-07-08 | **Spec**: [spec.md](spec.md)

**Input**: Feature specification from `/specs/004-lan-index-txt/spec.md`

## Summary

新設定キー `index_bind`(既定空 = 無効)が非空のとき、index.txt 配信専用の第 2 HTTP
リスナーを追加起動する。専用リスナーには `index_txt::routes()` 相当のルートのみを
マウントし(API・UI は物理的に持たせない)、検証は private/LAN + loopback 限定の
許可リスト方式(`require_lan_or_loopback`)。bind 失敗は縮退継続、起動成功時は
SecurityEvent `index_txt_lan_exposed` を 1 件記録し `GET /api/v1/status` に露出状態を
反映する。ADR-0012(ADR-0006 決定 4 の read-only index.txt 限定の部分 supersede)の
承認を実装前ゲートとする。

## Technical Context

**Language/Version**: Rust(edition 2024、既存 CI ツールチェーン)

**Primary Dependencies**: axum 0.8.9 / tokio 1.52(既存のみ。**新規依存の追加なし**)

**Storage**: rusqlite(settings テーブルに `index_bind` キーを 1 つ追加)

**Testing**: cargo test(unit / `tests/contract/` / `tests/integration/`、cucumber は既存機能のみ)

**Target Platform**: Windows / Linux 単一バイナリ(既存と同一)

**Project Type**: 常駐ネットワークサービス(単一クレート)

**Performance Goals**: 既存 index.txt 配信と同一(レート制限 10 req/秒/IP を LAN 側にも適用)

**Constraints**: `http_bind` / `pcp_bind` / `p2p_bind` の意味・検証・fail-fast は不変(FR-001/FR-007)。エラー応答は内部情報を含めない定型のみ

**Scale/Scope**: 変更対象 6 ファイル + UI 1 ファイル + テスト 3 系統 + ドキュメント 4 件(見積り)

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

| Gate | 原則 | 判定 | 根拠 |
|------|------|------|------|
| リスク評価 | Principle I | ✅ PASS | LAN 露出リスクは ADR-0012(草案済み)で評価。既定無効・明示オプトイン・警告 1 項目・監査イベントで受容範囲を限定。付加機能の bind 失敗が本体可用性を奪わない縮退設計(US4) |
| 実装前レビュー | Principle I | ⏳ GATE | **ADR-0012 の承認(ユーザーレビュー)を実装開始の前提条件とする**(spec Assumptions・tasks で先頭ゲート化) |
| 最小権限・攻撃面 | Principle II | ✅ PASS | 専用リスナーに index.txt ルートのみ物理マウント(経路フィルタ方式を却下 — research R2)。検証は拒否リストでなく**許可リスト**(loopback/RFC1918/リンクローカル/ULA のみ — research R1)。unspecified・グローバル・CGNAT は構造的に拒否 |
| trust nothing / 入力検証 | Principle II | ✅ PASS | 既存の URL/ヘッダサイズ上限・per-IP レート制限を LAN 側にも同一適用(FR-002)。設定値は `Settings::validate()` の唯一ゲートで検証 |
| ADR 記録 | Principle II/VI | ✅ PASS | ADR-0012 草案を本 plan の成果物として作成済み(`docs/adr/0012-index-txt-lan-exposure.md`)。ADR-0006 への追記は ADR-0012 承認時に実施 |
| 自前暗号の不在 | Principle II | ✅ PASS | 暗号要素なし(plain HTTP の継続は ADR-0006 決定 3 の受容範囲) |
| コード品質・CI | Principle III | ✅ PASS | `cargo fmt --check` / clippy / 既存 CI に変更なし。セキュリティ変更につきレビューチェックリスト(`docs/adr/security-review-checklist.md`)適用を tasks に組込む |
| Gherkin 振舞い定義 | Principle IV | ✅ PASS | spec.md に Given/When/Then 受け入れシナリオ + ネガティブシナリオ(グローバル拒否・404・レート制限)定義済み。テストはシナリオ失敗を確認してから実装(tasks で fail-first 順序化) |
| 形式的検証 | Principle V | ✅ PASS(対象外) | 新規並行アルゴリズムなし — 既存 `axum::serve` + graceful shutdown パターンの再利用のみで「クリティカル」3 基準を満たさない。**判断理由は ADR-0012 に明記**(MUST 履行) |
| 原則追跡 | Principle VI | ✅ PASS | 本表・ADR-0012・spec「検証する原則」で追跡 |

**Post-Phase 1 再評価**: 設計成果物(data-model / contracts / ADR-0012)反映後も違反なし。
Complexity Tracking は空(逸脱なし)。

## Project Structure

### Documentation (this feature)

```text
specs/004-lan-index-txt/
├── plan.md              # This file
├── research.md          # Phase 0 output
├── data-model.md        # Phase 1 output
├── quickstart.md        # Phase 1 output
├── contracts/
│   └── index-txt-lan.md # Phase 1 output(専用リスナー・設定・status の契約)
└── tasks.md             # Phase 2 output(/speckit-tasks — 本コマンドでは作らない)

docs/adr/
└── 0012-index-txt-lan-exposure.md  # Phase 1 output(草案 — 承認が実装前ゲート)
```

### Source Code (repository root)

```text
src/
├── config.rs            # index_bind キー・Settings フィールド・require_lan_or_loopback・
│                        #   ConfigError::NonLanBind・--index-bind(CliOverrides)
├── main.rs              # §15 付近: 第 2 リスナーの bind + tokio::spawn(縮退継続)+
│                        #   IndexTxtLanExposed 記録 + 起動サマリ
├── security/mod.rs      # SecurityCategory::IndexTxtLanExposed(ALL 14→15)
├── web/
│   ├── mod.rs           # build_index_router()(index.txt ルート + 定型 404 fallback)、
│   │                    #   AppState.index_lan(露出状態の注入)
│   ├── settings.rs      # BIND_KEYS へ index_bind 追加・検証エラー写像
│   └── announced.rs     # GET /api/v1/status へ index_txt_lan オブジェクト追加
└── yp/index_txt.rs      # 変更なし(再マウントのみ。定数参照)

ui/
└── settings.html        # index_bind 入力 + 非 loopback 時の警告 1 項目 + 明示確認

tests/
├── contract/
│   ├── cli_config.rs    # --index-bind の受理・検証
│   ├── local_api.rs     # settings GET/PUT(index_bind・restart_keys)・status 新フィールド
│   └── index_txt.rs     # (必要なら)LAN ルーターの 404 契約
└── integration/
    └── index_lan.rs     # 新規: 二重リスナー同一内容・API 不達・縮退継続・監査イベント
                         #   (Cargo.toml へ [[test]] 追加)
```

**Structure Decision**: 既存単一クレート構成に新ファイルは `tests/integration/index_lan.rs`
のみ。プロダクションコードは既存 6 ファイルへの追記で完結する(handoff のコード調査で
`index_txt::routes()` が `AppState` のみに依存する独立サブルーターであることを確認済み)。

## Complexity Tracking

違反なし(記入不要)。

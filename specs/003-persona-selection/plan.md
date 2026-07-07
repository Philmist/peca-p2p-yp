# Implementation Plan: 掲載前のペルソナ選択(選択中ペルソナの明示的な切り替え)

**Branch**: `003-persona-selection` | **Date**: 2026-07-07 | **Spec**: [spec.md](./spec.md)

**Input**: Feature specification from `/specs/003-persona-selection/spec.md`

## Summary

ユーザーがチャンネルを掲載(30311 発行)する**前に**、名乗るグローバル「選択中ペルソナ(selected)」を能動的に切り替えられる UI と、それを安全にするバックエンド制約を追加する。中核は 2 点:

1. **選択操作**: `active` かつ `usable`(鍵復号可能)なペルソナのみ選択可能。ペルソナ管理画面に単一選択(ラジオ的)の「選択」操作を置き、チャンネル一覧画面に読み取り専用で現在の selected を人間可読表示する(誤爆防止)。
2. **配信中ロック(プライバシー核心)**: 1 つ以上のチャンネルを実際に発行中(broadcasting)である間、selected ペルソナに対する「切替 / 破棄 / アーカイブ」を拒否する(409 `broadcasting_locked`)。これにより同一 ChannelID 上で「旧ペルソナ ended → 新ペルソナ live」というリンク推定シグナル(ADR-0004 §7)を**構造的に**防ぐ。

技術的アプローチ: 「配信中か」と「selected の変更」を単一の共有ロックで相互排他にし、**配信開始時に selected を読み取って予約(reserve)してから署名する**順序にすることで、`select()` と発行開始の間の TOCTOU レースを構造的に閉じる。掲載パイプラインの発行契機(PCP 到着での自動発行・周期再発行・終了時最終発行)は変更しない(FR-015)。

## Technical Context

**Language/Version**: Rust(edition 2024、`let`-chains 使用中。既存 `rust-toolchain` に従う)

**Primary Dependencies**: axum 0.8(ローカル HTTP API)、nostr(鍵・イベント署名)、rusqlite(SQLite 永続層)、tokio(非同期ランタイム)

**Storage**: SQLite(`settings` テーブルの `selected_persona` キーで selected を永続化。`personas` テーブルで状態管理)。配信中状態(broadcasting)は揮発(メモリ上の共有状態)

**Testing**: `cargo test`(ユニット + 統合)、cucumber(セキュリティ/BDD シナリオ — Principle IV)。`cargo fmt -- --check` / `cargo clippy` を CI で強制

**Target Platform**: デスクトップ常駐(Windows/Linux)。ローカルループバック上の Web UI(`ui/*.html` バイナリ埋め込み配信)

**Project Type**: 単一 Rust バイナリ + 埋め込み静的 UI(単一プロジェクト構成)

**Performance Goals**: 選択切替は体感即時(SC-001)。ロック取得はミリ秒未満(SQLite 1 行の読み書き + メモリ集合の判定のみ)

**Constraints**: selected の変更と発行開始の相互排他は**レースなし**でなければならない(SC-005 = Principle I)。ロック保持中に署名(暗号処理)を行わない(予約後に署名)。エラー応答は `{"error":"<code>"}` のみ(内部情報を漏らさない — Principle II)

**Scale/Scope**: 単一ノード・単一ユーザーのローカル操作。ペルソナ数は数個〜数十個規模。同時配信チャンネルは 1〜数個(EventStore pubkey クォータ 64 — ADR-0004 §2)

## Constitution Check

*GATE: Phase 0 前に通過必須。Phase 1 設計後に再確認する。*

| 原則 | 適用 | 本機能での充足 |
|------|------|----------------|
| **I. Safety First** | ★中核 | 配信中ロックが「旧→新ペルソナ入替」= リンク推定シグナル(ADR-0004 §7)を構造的に防ぐ。リスク評価は spec(US2・SC-005)+ [ADR-0011](../../docs/adr/0011-broadcasting-lock.md)に記録。selected の変更は非破壊操作で確認ダイアログ不要(FR-012)だが、破壊的操作(delete/archive/export)の既存確認は維持 |
| **II. Security by Design** | ★ | 拒否は UI 無効化だけでなく**バックエンド(`IdentityManager` 中核)**で強制(FR-002/FR-005、UI のみ防御の禁止)。select の状態ガード(active+usable)は入力検証。エラーは定型コードのみ。新規暗号なし(既存 keystore/nostr を利用)。**セキュリティログ非該当**: 本機能の 409(`broadcasting_locked`/`persona_not_selectable`)は既存 001 保護(Host 検証・トークン)を通過したローカル正当利用者による業務ルール拒否であり、§Security Requirements の「不正なリクエスト」(ネットワーク越しの攻撃)には当たらない。新規ネットワーク受信経路も無いためログ要件は不発火(判定を ADR-0011 に付記) |
| **III. Code Quality & Review** | ○ | ロック順序と TOCTOU 閉塞ロジックには意図コメント必須(MUST — 保安上複雑なロジック)。`cargo fmt --check` / clippy を CI で強制。レビュー観点は [security-review-checklist](../../docs/adr/security-review-checklist.md)適用 |
| **IV. BDD with Gherkin** | ★ | spec の受け入れシナリオは Given/When/Then。ネガティブシナリオ(配信中の直接 API 切替 → 409、archived/unusable の選択 → 拒否)を cucumber 化。テストが失敗する状態を確認してから実装(tests-first) |
| **V. Formal Verification** | 判定要 | [ADR-0011](../../docs/adr/0011-broadcasting-lock.md)で判定 = **非該当**(単一ミューテックスによる相互排他は標準プリミティブの単純利用であり基準①不成立)。代替担保は予約→署名順序を検査する並行性統合テスト。判定理由を ADR に明示(Principle VI MUST) |
| **VI. Traceability** | ○ | 本 plan の Constitution Check と ADR-0011 が参照原則を明示。ADR-0011 は ADR-0003(鍵管理)・ADR-0004 §7(リンク推定)を参照 |

**新規暗号アルゴリズムなし / ネットワークから受け取るデータの新規経路なし**(操作はローカル UI → ローカル API のみ)。**ゲート通過**。

## Project Structure

### Documentation (this feature)

```text
specs/003-persona-selection/
├── plan.md              # 本ファイル(/speckit-plan 出力)
├── spec.md              # 機能仕様(/speckit-specify 出力)
├── research.md          # Phase 0 出力 — 設計判断の確定
├── data-model.md        # Phase 1 出力 — BroadcastState / selectability
├── quickstart.md        # Phase 1 出力 — 検証シナリオ
├── contracts/
│   └── local-api.md     # Phase 1 出力 — API 差分(既存 001 契約への追補)
├── checklists/
│   └── requirements.md  # /speckit-specify 出力(合格済み)
└── tasks.md             # Phase 2 出力(/speckit-tasks — 本コマンドでは生成しない)
```

関連する既存設計文書(本機能で追補・参照):

```text
docs/adr/0011-broadcasting-lock.md   # 新規 — 配信中ロック設計 + Principle V 判定
docs/adr/0004-threat-model.md §7     # 参照 — ペルソナ間リンク推定
docs/adr/0003-persona-key-management.md  # 参照 — 状態・破棄・利用不可の既存方針
```

### Source Code (repository root)

```text
src/
├── identity/
│   └── mod.rs           # 変更 — select() に active+usable ガード / 配信中ロック
│                        #        selected() を archived・unusable でも None 扱いへ拡張
│                        #        delete()・set_state(→archived) に配信中ロック
├── event/
│   └── publish.rs       # 変更 — 発行開始時に BroadcastState へ予約してから署名(順序)
│                        #        publish_ended で予約解除
├── broadcast.rs         # 新規 — BroadcastState(配信中チャンネル集合 + 相互排他ロック)
├── web/
│   ├── mod.rs           # 変更 — AppState に broadcast 供給元を配線(with_broadcast)
│   ├── personas.rs      # 変更 — IdentityError の新規バリアントを 409/422 へ写像
│   └── announced.rs     # 変更 — GET /status に broadcasting: bool を追加
└── main.rs              # 変更 — BroadcastState を生成し identity と engine で共有
                         #        AppState へ配線

ui/
├── personas.html        # 変更 — 各 active 行に「選択」ボタン / 選択中の強調
│                        #        配信中は選択・破棄・アーカイブを無効化 + 理由表示
└── channels.html        # 変更 — 現在の selected を人間可読で読み取り専用表示

tests/                   # 追加 — 配信中ロックのユニット/統合、select ガード、
                         #        cucumber セキュリティシナリオ(ネガティブ)
```

**Structure Decision**: 既存の単一 Rust プロジェクト構成に従う。配信中状態は identity(ロックガード)・publish(ライフサイクル)・web(status 表示)の 3 者が共有するため、循環依存を避けて中立な新規モジュール `src/broadcast.rs` に `BroadcastState` を置き、各者が `Arc<BroadcastState>` を保持する(いずれも相手を所有しない)。

## Complexity Tracking

> Constitution Check に違反はない。本表は該当なし。

| Violation | Why Needed | Simpler Alternative Rejected Because |
|-----------|------------|-------------------------------------|
| （なし） | — | — |

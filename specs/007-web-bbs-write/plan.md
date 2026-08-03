# Implementation Plan: Web ブラウザからの実況スレ書き込み(旧来 BBS 互換 UI・LAN 公開・固定 >>1)

**Branch**: `007-web-bbs-write` | **Date**: 2026-07-31 | **Spec**: [spec.md](spec.md)

**Input**: Feature specification from `/specs/007-web-bbs-write/spec.md`

## Summary

006-livechat-thread が確立した P2P 実況スレの土台(順序確定・鍵体系・検証・2ch 互換 API)を
一切変更せず、その上の 3 点を補う。

1. **Web UI の旧来 BBS 化**: `ui/livechat.html` を管理表 UI から、参考画像 `_alt`
   (bbs.jpnkn.com 旧来スキン)相当の伝統的 2ch 風掲示板(板ページ = スレ一覧 + スレ立て +
   ローカルルール掲示 / スレページ = `1 :名前 :日付 ID:xxx` 形式のレス列 + `>>n` アンカー +
   名前/メール/本文の書き込み欄)へ全面刷新する。書き込みは既存 JSON API
   (`/api/v1/livechat/...`)をそのまま使い、検証経路は変えない。
2. **LAN 公開**: `http_bind`(Web UI + JSON API)と `compat_bbs_bind`(2ch 互換 API)を、
   ADR-0012 の許可リスト方式(`require_lan_or_loopback` — loopback / RFC 1918 /
   リンクローカル / ULA(`fc00::/7`)のみ)に従って LAN 内プライベートアドレスへ
   bind 可能にする。
   非 loopback bind には公開面ごとの明示同意キー(`http_lan_consent` /
   `compat_bbs_lan_consent`)を必須とする 2 要素オプトインとし、起動時に SecurityEvent を
   記録する。Host ホワイトリストは bind アドレスを含むよう拡張し、保護(per-IP レート制限・
   サイズ上限)は loopback 側と同一水準を維持する。書き込みを含む LAN 公開と互換 API の
   無認証 LAN 書き込みの受容は新 ADR-0015 に記録する。
3. **固定 >>1(スレ頭テンプレ)**: `BoardSettings` にテンプレ本文フィールドを追加し、
   スレ開設・次スレ移行時にホスト(板主)が板鍵で署名した kind 1311 レスを res_no=1 として
   自動採番・配布する(初回 PoW 免除)。テンプレ未設定時はシステム既定の最小テンプレを使う。
   板設定は SQLite `board_settings` テーブルへ永続化を配線する(006 で用意済み・未配線)。

### Clarifications 2026-08-03 反映(差分)

quickstart 実機検証(V-1〜V-5)で判明した UI/公開経路の要件漏れを spec Clarifications
(Session 2026-08-03)で確定し、以下 4 点を本 plan へ折り込む。いずれも**表現層(Web UI /
互換ポートのルーティング)の差分**で、006 の P2P・順序確定・検証・鍵体系は不変。設計根拠は
research.md R11・R12、契約は contracts/web-ui.md §1/§2.4/§2.5/§3.7/§5.3/§7 と
contracts/fixed-first-post.md §3.5。

1. **FR-002a(一覧の遷移リンク)**: 板選択→板ページ、スレ一覧→スレページのクリック遷移を
   必須化(遷移手段のない静的表示にしない)。
2. **FR-006a/FR-006b(管理導線の再編)**: 板設定編集(タイトル・名無し名・ローカルルール・
   固定 >>1 テンプレ)と BAN 一覧管理を**板ページ**へ移設しスレ非依存にする。個別レスへの
   NG/BAN 実行はスレページ起点を維持する。
3. **FR-014a(開設時 >>1 のプリフィル・上書き)**: 新規スレ作成欄に >>1 本文欄を設け固定
   テンプレ(未設定時はシステム既定)をプリフィル、任意で上書き可(そのスレ限り・永続
   テンプレは不変)。スレ開設 API に任意 `first_post_override` を追加。
4. **FR-023(互換ポートのブラウザ向け板ページ)**: `compat_bbs_bind` 面が `.../{board}/`
   (ファイル名なし)への GET に旧来 UI 板ページ(HTML・UTF-8)を同一ポート・同一パスで
   配信する(専ブラ向け `subject.txt`/`dat`/`bbs.cgi` は不変・SJIS のまま)。他面への
   リダイレクトはしない(公開面独立性・一貫性)。

**別扱い(実装バグ・仕様漏れではない)**: 板一覧のコピーボタンがブラウザで動作しない件は
クリップボード操作の実装不具合として GitHub Issue 起票(spec/plan には含めない)。

## Technical Context

**Language/Version**: Rust(edition 2024、stable toolchain)

**Primary Dependencies**: axum 0.8(HTTP サーバ・既存)、tower 0.5、tokio 1.x、nostr 0.44
(イベント署名・検証)、rusqlite 0.40(bundled SQLite)、pulldown-cmark 0.13(ローカルルール
Markdown 安全描画・既存)、encoding_rs 0.8(Shift_JIS・既存)。**新規依存なし**(Web UI は
単一 HTML + インライン CSS/JS を `include_str!` 埋め込みする既存方式を踏襲)

**Storage**: SQLite(`board_settings` テーブル — 006 で定義済み、007 で固定 >>1 テンプレ
列を追加し production 配線)。スレ本文(>>1 含む確定レス)は 006 どおり揮発(永続化しない)

**Testing**: cargo test(unit + `tests/contract/` + `tests/integration/`)、cucumber 0.23
(Gherkin シナリオ)。実機確認(SC-001/002/009)は quickstart.md の手順による

**Target Platform**: Windows / Linux デスクトップ(既存デーモン + 同梱 Web UI)。
視聴端末側は同一 LAN 上の一般的な Web ブラウザおよび 2ch 互換専ブラ

**Project Type**: 単一 Rust バイナリ(P2P デーモン + 埋め込み Web UI + 互換 API)

**Performance Goals**: 006 の水準を維持(書き込み確定・配布のレイテンシに新たな要求なし)。
Web UI のスレ表示は 4000 レス(res_limit 上限)を「全部読む」で表示しても、一般的な
デスクトップ/モバイルブラウザで初回描画 5 秒以内・スクロールと `>>n` ジャンプの応答を
維持できること(既定は最新 50 表示でクライアント側スライス。確認手順は quickstart V-1
手順 6)

**Constraints**: 006 の P2P プロトコル・順序確定・鍵体系・NG/BAN・PoW・レート制限を変更
しない。LAN 公開はグローバル/未指定/CGNAT を 100% 拒否(判定は `to_canonical()` 正規化後)。
保護水準は loopback と同一(ADR-0012 決定 5)。Web UI は外部アセット(CDN 等)を参照しない

**Scale/Scope**: 板 = 1 ホスト 1 板(006 どおり)。同時視聴端末は LAN 内の数台〜数十台想定。
UI は 2 ページ(板ページ / スレページ)+ 既存管理操作の温存

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

| 原則 | 判定 | 根拠 |
|------|------|------|
| I. Safety First | PASS | LAN 公開のリスク評価は spec(Assumptions・Edge Cases)で実施済み — 追加露出は「LAN 内の他端末による平文の読み書き・トークン取得」に限定され、グローバル公開を許可リストで 100% 拒否する(FR-008/SC-003)。受容リスクは明示同意キー + 起動時 SecurityEvent(FR-010/SC-004)で利用者に可視化する。新 ADR-0015 でリスク受容を記録する |
| II. Security by Design | PASS | 待受アドレスの許可リスト検証(正規化後判定)・Host ヘッダ検証拡張・per-IP レート制限とサイズ上限の非緩和(FR-011)を設計段階から規定。固定 >>1 は既存の kind 1311 検証(サイズ・行数・署名)をそのまま満たす経路で自動投稿し、検証の抜け道を作らない(FR-003/FR-016)。署名は既存の nostr ライブラリ(自前暗号なし)。設計決定は ADR-0015 に記録する |
| III. Code Quality and Review | PASS | rustfmt/`cargo fmt -- --check`・clippy・cargo audit は既存 CI のまま適用。セキュリティ関連変更(bind 検証・Host 検証・同意キー)はレビュー観点チェックリスト(`docs/adr/security-review-checklist.md`)の適用結果を記録する |
| IV. Behavior-Driven Testing | PASS | spec の Acceptance Scenarios は Given/When/Then 形式で、ネガティブシナリオ(グローバル/CGNAT bind 拒否・ホワイトリスト外 Host 拒否・レート超過・未確定アンカー)を含む。tasks フェーズで cucumber / contract テストへ対応付け、失敗確認後に実装する |
| V. Formal Verification | PASS(対象外) | 本機能は新規の並行アルゴリズム・プロトコル状態機械を導入しない。固定 >>1 は既存の採番経路(006 で検証済みの順序確定)への挿入であり、LAN 公開は bind/検証の追加、UI は表現層のみ。クリティカル基準(新規設計・非自明な並行性)を満たさない判断を ADR-0015 に明記する |
| VI. Principle Traceability | PASS | 本 plan の各決定に原則参照を付し、ADR-0015 は参照原則(I・II)を記載する |

**Gate 判定**: PASS(違反なし — Complexity Tracking 不要)

**Post-Design 再評価**(Phase 1 完了後): PASS を維持。data-model.md の同意キー 2 要素
オプトイン・SecurityEvent 追加カテゴリ、contracts/lan-exposure.md の拒否マトリクス、
contracts/fixed-first-post.md の検証同一性(通常書き込みと同じ kind 1311 検証)を確認した。
新たな違反・複雑性の追加なし。

**Post-Design 再評価**(Clarifications 2026-08-03 差分反映後): PASS を維持。

- **I / II**: FR-023 は互換ポートに**読み取り専用の HTML 板ページ**(既存 Web UI 資産の再利用)を
  パス分岐で追加するのみで、新規の書き込み経路・認証面を作らない。当該面の bind 検証・Host
  検証・送信元 LAN 限定・レート制限(lan-exposure.md §2〜§5)はブラウザ向けパスにも同一に
  適用され、保護は非緩和。FR-014a の >>1 上書きは固定テンプレと**同一の検証**(≤2048 文字・
  ≤32 行・kind 1311 署名・PoW 免除)を通り、検証の抜け道を作らない(FR-003/FR-016 と一貫)。
  FR-006a/b の管理導線再編は表現層の配置換えで、モデレーション/板設定の権限(トークン)
  検証は 006 のまま。
- **III**: 追加はいずれも既存モジュールへの分岐追加・UI 再配置で、新規モジュール・新規並行
  処理なし。
- **IV**: 追加要件の検証は FR-023(互換ブラウザ板ページ)・FR-014a(>>1 上書き・上限超過
  ネガティブ)を Gherkin/契約テストへ対応付ける(contracts/fixed-first-post.md §4 に上書き
  シナリオ追加済み。互換ブラウザ板ページは quickstart V-4 で実機確認)。
- **V(対象外)**: 新規の並行アルゴリズム・プロトコル状態機械なし(ルーティング分岐・UI 配置・
  レス本文検証の再利用のみ)。判断は不変。

新たな違反・複雑性の追加なし(Complexity Tracking 追記不要)。

## Project Structure

### Documentation (this feature)

```text
specs/007-web-bbs-write/
├── plan.md              # 本ファイル
├── research.md          # Phase 0 出力(設計決定と根拠)
├── data-model.md        # Phase 1 出力(エンティティ・設定・イベント)
├── quickstart.md        # Phase 1 出力(検証手順)
├── contracts/           # Phase 1 出力
│   ├── web-ui.md            # 板ページ/スレページの UI 契約と JSON API 差分
│   ├── lan-exposure.md      # LAN 公開の設定キー・検証規則・監査
│   └── fixed-first-post.md  # 固定 >>1 の板設定・自動投稿の意味論
├── checklists/
│   └── requirements.md  # 既存(speckit-checklist 出力)
├── assets/              # 既存(参考画像 4 枚)
└── tasks.md             # Phase 2 出力(/speckit-tasks — 本コマンドでは作らない)
```

### Source Code (repository root)

```text
src/
├── config.rs            # [変更] http_bind/compat_bbs_bind の LAN 許可分岐、
│                        #   http_lan_consent / compat_bbs_lan_consent キー追加
├── main.rs              # [変更] bind 時の Host ホワイトリスト拡張・SecurityEvent 記録、
│                        #   LivechatAdapter::open_thread / next_thread の >>1 配線
├── security/mod.rs      # [変更] SecurityCategory 2 件追加(WebUi/CompatBbs LanExposed)
├── livechat/
│   ├── thread.rs        # [変更] BoardSettings に first_post_template 追加 + 検証
│   ├── registry.rs      # [変更] open_thread / 次スレ移行での >>1 自動採番・署名
│   └── session.rs       # [確認] 参加者側は変更なし(通常レスとして受理)
├── store/mod.rs         # [変更] board_settings テーブルに列追加・production 配線
└── web/
    ├── mod.rs           # [変更] allowed_hosts 生成の LAN 対応、板詳細 API への
    │                    #   compat_bbs_port 追加
    ├── livechat.rs      # [変更] スレ開設 API に任意 first_post_override(FR-014a)
    └── compat/mod.rs    # [変更] CompatState.allowed_hosts の LAN 対応・送信元検証、
                         #   板ルート URL `.../{board}/` のブラウザ向け HTML 配信(FR-023・R11)

ui/
└── livechat.html        # [全面刷新] 旧来スキン(板ページ + スレページ、ハッシュ
                         #   ルーティング、>>n アンカー、最新 50 既定表示)。
                         #   [2026-08-03 差分] 板選択/スレ一覧の遷移リンク(FR-002a)、
                         #   板設定・BAN 一覧を板ページへ移設(FR-006a/b)、新規スレ作成欄の
                         #   >>1 プリフィル・上書き欄(FR-014a)

tests/
├── contract/
│   ├── cli_config.rs    # [追加] LAN bind 許可リスト・同意キーの検証シナリオ
│   ├── local_api.rs     # [追加] Host ホワイトリスト拡張・板詳細 API 差分
│   └── compat_bbs.rs    # [追加] 互換 API の LAN Host 検証・無認証書き込み境界
├── integration/
│   ├── livechat.rs      # [追加] 固定 >>1 の自動投稿・次スレ・遡及なし
│   └── lan_write.rs     # [新規] LAN 公開時の別端末相当(非 loopback)読み書き
└── cucumber(features/) # [追加] FR 対応 Gherkin シナリオ

docs/adr/
└── 0015-web-bbs-lan-write.md  # [新規] 書き込みを含む LAN 公開・互換 API 無認証受容
```

**Structure Decision**: 単一プロジェクト構成(既存)を維持する。フロントエンドは
`ui/livechat.html` 単一ファイル(`include_str!` 埋め込み・外部アセットなし)の既存方式を
踏襲し、ビルドパイプラインを追加しない。新規モジュールは作らず、既存モジュールへの変更 +
新 ADR + 統合テスト 1 本の追加に留める。

## Complexity Tracking

Constitution Check に違反なし — 記載事項なし。

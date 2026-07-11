# Implementation Plan: 配信実況スレ(P2P 掲示板)

**Branch**: `006-livechat-thread` | **Date**: 2026-07-12 | **Spec**: [spec.md](./spec.md)

**Input**: Feature specification from `/specs/006-livechat-thread/spec.md`

## Summary

PeerCast 配信のリアルタイム実況を、中央サーバーなしのスレッドフロート型掲示板として
提供する。発見 = 既存 gossip に **kind 31311 スレ announce**(チャンネル掲載ペルソナと
同一署名必須)を相乗りさせ、配送 = **ホスト(= 配信者)直結の星型**でホストが
シーケンサとして採番・署名付き順序確定情報(kind 21311)を配布し、表現 = 各端末が
確定順序から掲示板(レス番号・アンカー)を再構成する。レスは 001 で予約済みの
**kind 1311** を履行(research R1)。書き込み身元は既存ペルソナと別系統の**板鍵**
(板 = 配信者ペルソナ単位、ローテーション可・初回高 PoW・完全鍵照合の NG/BAN)。
既存の専ブラ・コメントビューワ向けに、各利用者ノードが loopback 専用の
**伝統的 2ch 形式互換 API**(subject.txt / dat / SETTING.TXT / bbs.cgi、Shift_JIS)を
提供する。スレデータは揮発(クローズで削除)、板鍵・NG/BAN・板設定のみ永続。

技術方針: 配送は既存 P2P 待受(7147)・フレーミング・検証パイプラインを最大限再利用し
(research R4)、nostr 援用はイベント封筒のみという 001 FR-014 の境界を維持する。
シーケンサ状態機械は Principle V「該当」と判定し、PlusCal モデルを実装前ゲートとする
(research R9 / ADR-0014 予定)。

## Technical Context

**Language/Version**: Rust (edition 2024)

**Primary Dependencies**: tokio(非同期ランタイム)、axum + tower(互換 API — 既存
`web/` 保護層を再利用)、nostr クレート(イベント封筒・Schnorr 署名・NIP-13 PoW)、
encoding_rs(Shift_JIS — 既存 index.txt と共用)、pulldown-cmark(ローカルルール
Markdown の安全なサブセット描画 — research R7。**新規依存**)、rusqlite

**Storage**: SQLite(`board_keys` / `livechat_moderation` / `board_settings` の 3 テーブル
追加 — data-model.md)。スレデータ(レス・順序確定情報)はメモリのみ(FR-015 揮発)。
板鍵秘密鍵は keystore 抽象(DPAPI / Linux エンベロープ)で暗号化(research R8)

**Testing**: `cargo test`(unit)、cucumber(Gherkin — spec US1〜US6)、契約テスト
(`tests/contract/` — thread-events / thread-delivery / compat_bbs)、モックピア拡張
(`tests/common/mock_peer.rs`)、多ノード統合(`tests/integration/livechat.rs`)、
TLC(`docs/formal/livechat_sequencer.tla` — research R9)

**Target Platform**: Windows / Linux(既存と同一。プラットフォーム依存は keystore 経由のみ)

**Project Type**: 単一プロジェクト(P2P ノードデーモン + 埋め込み Web UI)

**Performance Goals**: 書き込み→全参加者確定表示 p99 ≤ 5 秒(SC-001)、レス番号・
アンカー不一致 0(SC-002)、4000 レス同期 ≤ 15 秒(SC-003)、既存発見網の掲載 60 秒以内を
維持(SC-006 — announce 追加率は R16 余裕内、research R3)

**Constraints**: バースト 30 レス/分・参加者 100 弱・参加上限 128 接続(星型で成立)。
announce 受信のみでの自動接続 0(SC-005)。互換 API は loopback 限定・トークンなし
(Host 検証 + レートで代替)。内部情報を漏洩しない定型エラー(全経路)

**Scale/Scope**: 新規モジュール `src/livechat/`(ホスト・参加者セッション・スレ状態・
モデレーション)+ `event/` への 3 kind 追加 + `web/` 互換 API + store 3 テーブル +
SecurityEvent 6 カテゴリ追加(15 → 21)。契約 3 本・PlusCal モデル 1 本・ADR 1 本

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

| 原則 | 評価 | 根拠 |
|------|------|------|
| I. Safety First | PASS(条件付き) | 最大リスク = announce を反射攻撃の母体にされること。緩和: 掲載ペルソナ同一署名必須(FR-003)・受信のみでは接続しない(FR-004/SC-005)・チャレンジ認証 + バックオフ(FR-005)。荒らし対策(BAN 採番拒否・初回 PoW・レート)と揮発性(残骸を残さない — FR-015)。条件: 脅威追加を ADR-0014 に記録 |
| II. Security by Design | PASS | 全受信データは既存多段検証パイプライン共有(サイズ→署名→形式→時刻→内容→PoW)+ 本機能固有検査(ペルソナ一致・スレ主一致・seq 連続)。暗号は nostr クレート(Schnorr)・NIP-13 再利用で自前実装なし。互換 API は loopback 限定 + Host 検証 + 定型エラーで、書き込みは通常経路と同一検証(FR-028 抜け道禁止)。設計判断は ADR-0014 に記録(MUST) |
| III. Code Quality & Review | PASS | `cargo fmt --check` / clippy / CI。契約 3 本 + 共有フィクスチャで実装とモックピアの乖離を防止。シーケンサ・チャレンジ・PoW 判定のセキュリティロジックには意図コメント(MUST) |
| IV. BDD with Gherkin | PASS | spec US1〜US6 が Given/When/Then で記述済み。ネガティブ(偽 announce・偽 ORDER・過大・レート・BAN・PoW 不足)は US3/US4 + Edge Cases に明示。実装前に feature の失敗を確認(quickstart §1) |
| V. Formal Verification | 該当(条件付き PASS) | シーケンサ状態機械(採番・確定配布・次スレ移行・凍結/クローズ)は 3 基準すべて充足 → PlusCal モデル `docs/formal/livechat_sequencer.tla` を実装前ゲートとして作成(research R9)。デッドロック・不変条件(T3/O1/O2 — data-model)を TLC で検査(MUST)。該当/非該当の判定理由は ADR-0014 に明記(MUST) |
| VI. Principle Traceability | PASS | 本表・各契約・data-model が原則参照を含む。ADR-0014(脅威・Principle V 判定・kind 採番の根拠)を実装フェーズ先頭で作成 |

**Gate 判定**: 違反なし。ADR-0014 の作成(脅威追加・Principle V 判定・31311/21311 の
kind 採番根拠)と PlusCal モデルの実装前完了を条件に通過。

**Post-Design 再評価(Phase 1 後)**: research R1〜R9・data-model・契約 3 本を経ても
新たな違反は生じない。むしろ設計で強化された点: (a) 板鍵はストレージ層でもペルソナと
構造分離(R8 — FR-016 の二重担保)、(b) 互換 API は専用リスナー分離により既存 7180 の
トークン保護に例外を作らない(R5)、(c) レス・順序確定情報は gossip に流さない
(MUST NOT — thread-events.md)ため発見網への攻撃面追加は announce のみで、容量検証済み
(R3)。SJIS 既定(R5)は SC-007 実機確認までは仮説としてリスク明示。ゲート通過を維持。

**Interop チェックリスト反映(2026-07-12)**: `checklists/interop.md`(24 項目)の指摘を
spec・research・契約へ反映済み。主な修正: dat の追記不変性を契約の不変条件化(名無し名は
レス確定時点で固定 — FR-023/FR-024 改訂)、本文上限の単位を 2048 **文字**に統一
(SETTING.TXT と同一単位)、SJIS 変換不能文字は数値文字参照で保全(index.txt の `?` 置換
とは別方針 — R5 改訂)、HTTP メタデータ(charset / 304 / Range 206 / gzip なし)と
bbs.cgi 前段(確認画面なし・Cookie/Referer 不使用・スレ立て拒否)を明文化、NIP 適合の
採否列挙・kind レジストリ確認記録・gossip への 1311/21311 混入時の受信側挙動を契約に追加、
PoW 設定名を `first_post_pow_bits`(板設定)に一本化。仮説領域は quickstart §5 の
SC-007 確認観点リストへ転記。

## Project Structure

### Documentation (this feature)

```text
specs/006-livechat-thread/
├── plan.md              # This file
├── research.md          # Phase 0 output(R1〜R9)
├── data-model.md        # Phase 1 output(エンティティ・Settings・SecurityEvent・不変条件)
├── quickstart.md        # Phase 1 output(検証手順)
├── contracts/
│   ├── thread-events.md    # kind 1311 / 21311 / 31311 スキーマと検証順序
│   ├── thread-delivery.md  # スレ配送ワイヤ(JOIN/WELCOME/RES/ORDER/…・状態機械・防御)
│   └── compat-api.md       # 2ch 互換 API(subject.txt / dat / SETTING.TXT / bbs.cgi)
├── checklists/
│   └── requirements.md  # 作成済み(spec 品質 16/16)
└── tasks.md             # Phase 2 output(/speckit-tasks — 本コマンドでは作らない)
```

### Source Code (repository root)

```text
src/
├── event/
│   └── livechat.rs      # kind 1311/21311/31311 のスキーマ・検証(nostr 援用境界内 — FR-014)
├── livechat/            # 新規モジュール(配送・状態 — 援用境界の外)
│   ├── mod.rs
│   ├── host.rs          # シーケンサ(採番・ORDER 発行・次スレ移行・BAN 強制・PoW/レート判定)
│   ├── session.rs       # 参加者セッション(JOIN/チャレンジ検証/同期/凍結・復帰)
│   ├── thread.rs        # スレ状態(Active/Frozen/Closed・レス列・seq 検証 — data-model)
│   ├── board.rs         # 板・板設定・板鍵(ローテーション)
│   └── moderation.rs    # NG/BAN(完全鍵照合)
├── p2p/                 # 既存: HELLO features "livechat1"・THREAD_* フレームの受付分岐(research R4)
├── web/
│   └── compat/          # 互換 API(専用 loopback リスナー・SJIS・dat/subject/SETTING/bbs.cgi)
├── store/               # board_keys / livechat_moderation / board_settings テーブル追加
└── security/            # SecurityEvent 6 カテゴリ追加

tests/
├── features/livechat.feature       # US1〜US6 の Gherkin(+ security.feature へ追加)
├── steps/livechat.rs
├── contract/
│   ├── thread_events.rs            # イベント検証のネガティブ含む契約テスト
│   ├── thread_delivery.rs          # モックピアによるワイヤ契約(fixtures 共有)
│   └── compat_bbs.rs               # 互換 API の形式・SJIS・拒否系
├── integration/livechat.rs         # 多ノード E2E(SC-001〜SC-005)
└── common/mock_peer.rs             # THREAD_* 対応の拡張

docs/
├── formal/livechat_sequencer.tla   # PlusCal モデル(research R9)+ .cfg
└── adr/0014-livechat-thread.md     # 脅威・Principle V 判定・kind 採番(実装フェーズ先頭)
```

**Structure Decision**: 単一プロジェクト構成を維持。イベントスキーマは `event/` に置き、
配送・状態機械は新規 `livechat/` に隔離することで、nostr 援用境界(FR-014)を
モジュール境界で強制する既存方針(ADR-0002 §3)を踏襲する。互換 API は `web/` 配下で
既存保護層(Host 検証・レート・ボディ上限・定型エラー)を再利用しつつ、専用リスナーで
トークン保護と分離する(research R5)。

## Complexity Tracking

> Constitution Check に違反なし。追加の正当化は不要。

(補足: 新規依存 pulldown-cmark は「HTML を生成しない描画」のための最小追加であり、
代替(生 HTML + サニタイザ)より攻撃面が小さいことを research R7 に記録済み)

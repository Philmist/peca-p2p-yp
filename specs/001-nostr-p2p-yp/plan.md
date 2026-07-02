# Implementation Plan: 分散型配信情報共有ネットワーク(YP代替)

**Branch**: `001-nostr-p2p-yp` | **Date**: 2026-07-02 | **Spec**: [spec.md](./spec.md)

**Input**: Feature specification from `/specs/001-nostr-p2p-yp/spec.md`

## Summary

単一障害点である中央 YP サーバーを、nostr プロトコル(NIP)を用いた分散型の配信情報共有で置き換える。
Windows 上で動作する自己完結型の単一 Rust バイナリを提供し、以下を 1 プロセスで担う:

1. **PCP アナウンス受信**: PeerCastStation が従来 YP と同様に接続できる PCP(TCP)リスナー
2. **nostr 掲載/購読エンジン**: チャンネル情報を NIP-53 (kind 30311) の addressable イベントとして
   複数リレーへ掲載・購読し、署名検証済みのチャンネル一覧を構築
3. **HTTP 供給層**: 既存 YP ブラウザ向け `index.txt`(plain HTTP)+ ブラウザベースの管理 UI(簡易 Web サーバー)

**トラッカー解決(FR-004)の充足方式**: 現行 YP と同方式で、`index.txt` の TIP フィールド
(トラッカー ip:port)を介して視聴クライアントがトラッカーへ直接接続することで充足する。
PCP によるホストルックアップ(tracker lookup)は v1 の対象外とし、この判断は ADR-0002 に記録する。

UI 層はユーザーの Web ブラウザに置き、ネイティブ GUI は持たない。
初期リレー(共有先)は同梱せず、ユーザーが掲示板/SNS 等で発見してソフトウェアに貼り付けて登録する。
匿名文化の維持は「ペルソナ = nostr 鍵ペア」の複数保持・切替・破棄で実現する(spec Clarifications 参照)。

## Technical Context

**Language/Version**: Rust(stable、edition 2021 以降。MSRV は依存クレートに追随し CI で固定)

**Primary Dependencies**:
- `tokio`(非同期ランタイム)
- `nostr-sdk`(rust-nostr。イベント署名・検証・リレー通信。自前暗号実装の回避 — Principle II)
- `axum` + `tower`(Web UI・`/index.txt`・ローカル JSON API、レート制限ミドルウェア)
- `rusqlite`(bundled SQLite。設定・ペルソナ・リレー・ミュートの永続化)
- `windows`(DPAPI `CryptProtectData` によるペルソナ秘密鍵の暗号化保存)
- `encoding_rs`(index.txt の Shift_JIS 出力)
- `tracing`(構造化ログ+セキュリティイベントログ)
- `cucumber`(Gherkin シナリオの自動テスト — Principle IV)
- PCP プロトコルは自前モジュール(仕様は参考資料 gist に基づくクリーンルーム実装。コード流用なし)

**Storage**: SQLite 単一ファイル(`%APPDATA%\peca-p2p-yp\`)。秘密鍵は DPAPI で暗号化した BLOB。チャンネル一覧は原則メモリ上(揮発)

**Testing**: `cargo test`(unit/integration)+ `cucumber` で spec の Gherkin シナリオを実行。契約テスト: index.txt ゴールデンファイル、PCP ハンドシェイクフィクスチャ、インプロセス WebSocket モックリレー

**Target Platform**: Windows 10/11 x64。単一 exe(自己完結、インストーラ不要)。リッスンは既定で 127.0.0.1 のみ

**Project Type**: 常駐型ローカルサービス + ブラウザ UI(single project)

**Performance Goals**: 配信開始→他参加者の一覧反映 60 秒以内(SC-001)/一覧表示 5 秒以内(SC-004)/チャンネル数 ~2,000 まで劣化なし

**Constraints**: メモリ < 150MB、単一バイナリ、外部ランタイム不要、index.txt は plain HTTP(ユーザー要求)、リレー全断時もローカル機能(UI・index.txt)は継続動作

**Scale/Scope**: 想定同時配信チャンネル数百〜2,000、リレー数 1〜10、ペルソナ数 ~100/ユーザー

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

| Gate | 原則 | 判定 | 根拠 |
|------|------|------|------|
| ユーザー安全リスク評価 | I | PASS | リスク: 悪意リレー/偽エントリ/悪性URL。緩和: 署名検証(FR-005)、URLスキーム許可リスト+警告UI(FR-012)、既定 loopback バインドで攻撃面最小化 |
| 既知脆弱性なしでリリース | I | PASS(継続ゲート) | `cargo audit`+Trivy を CI に組込み(ADR-0001 準拠)。High/Critical 未緩和ならリリース不可 |
| 入力検証・trust nothing | II | PASS | 3 つの信頼境界(PCP/TCP、nostr リレー、ローカル HTTP)すべてにサイズ・形式・内容の検証を設計(contracts/ 参照)。エラーは内部情報を漏らさず、セキュリティイベントとして記録 |
| 既存暗号ライブラリ使用・自前暗号禁止 | II | PASS | 署名は nostr-sdk(secp256k1/Schnorr)、鍵保護は OS 機能(DPAPI)。自前実装なし |
| セキュリティ設計決定の ADR 化 | II, VI | PASS(要フォロー) | 実装開始前に ADR を作成: ①イベントモデルと NIP 選定 ②ペルソナ鍵管理(DPAPI) ③脅威モデル(Sybil/汚染緩和: 検証+ミュート+リレー選別+任意 PoW) ④形式的検証スコープ判断。tasks の先頭フェーズに配置 |
| Gherkin 振舞い定義 | IV | PASS | spec.md に受け入れ+ネガティブシナリオあり。cucumber で対応付け、失敗確認後に実装(テストファースト) |
| 形式的検証のクリティカル判定 | V | PASS(要 ADR) | PCP 状態機械: 既存 PeerCast YP プロトコル準拠のため対象外(constitution 明記)。nostr 通信: 実績ある仕様+ライブラリの利用でクリティカル基準①を満たさない。複数リレーからの一覧集約は created_at による last-write-wins + 有効期限で自明。→「クリティカル該当なし」の判断理由を ADR に記録(MUST) |
| 原則トレーサビリティ | VI | PASS | 本表および各 contracts/ に原則参照を記載。ADR は原則番号を必須記載 |

**Post-Phase 1 再評価**: PASS — 設計成果物(data-model.md, contracts/)に新たな違反なし。
信頼境界ごとの検証ルールは各 contract に明記済み。

**SHOULD からの逸脱記録**(constitution 規範キーワード節に基づく):
- spec FR-010 の「既定の共有先を持つべき (SHOULD)」→ **同梱既定リレーなし** とする。
  理由: ユーザー指示(初期共有先は掲示板/SNS 等で発見して提供する)。同梱リストは事実上の
  中央集権点・単一障害点の再導入となり本機能の動機と矛盾するため。代替として
  貼り付けによる一括登録とリレーリストのテキスト書き出し(共有用)を UI に用意する。

## Project Structure

### Documentation (this feature)

```text
specs/001-nostr-p2p-yp/
├── plan.md              # This file
├── research.md          # Phase 0 output
├── data-model.md        # Phase 1 output
├── quickstart.md        # Phase 1 output
├── contracts/           # Phase 1 output
│   ├── pcp-announce.md  # PeerCastStation との PCP 契約
│   ├── http-yp.md       # index.txt(YP ブラウザ)契約
│   ├── local-api.md     # ブラウザ UI 用ローカル JSON API 契約
│   └── nostr-events.md  # nostr イベント(kind 30311/1311)契約
└── tasks.md             # Phase 2 output (/speckit-tasks — 本コマンドでは生成しない)
```

### Source Code (repository root)

```text
Cargo.toml
src/
├── main.rs              # 起動・設定読込・各サブシステムの起動と監視
├── config.rs            # 設定(ポート・バインド先・鮮度閾値など)
├── pcp/                 # PCP プロトコル: atom 符号化/復号、HELO/OLEH、BCST 解析、announce セッション
├── yp/                  # index.txt 生成(17 フィールド、Shift_JIS)、(将来: tracker lookup)
├── nostr/               # kind 30311 マッピング、掲載/購読、検証、鮮度・有効期限管理
├── identity/            # ペルソナ管理(鍵生成・切替・破棄)、DPAPI 鍵保管
├── store/               # SQLite 永続化(ペルソナ・リレー・ミュート・設定)
├── web/                 # axum ルーター: UI 静的配信、/api/v1、/index.txt、レート制限
└── security/            # 入力検証共通部、セキュリティイベントログ、URL 警告判定

ui/                      # Web UI 静的アセット(ビルド時にバイナリへ埋め込み)

tests/
├── features/            # Gherkin .feature(spec のシナリオと 1:1 対応)
├── contract/            # index.txt ゴールデン、PCP フィクスチャ、30311 スキーマ検証
└── integration/         # モックリレー+PCP 疑似クライアントでの E2E

docs/
├── adr/                 # 0002〜: イベントモデル、鍵管理、脅威モデル、形式的検証判断
└── formal/              # (Principle V 対象が生じた場合のみ)
```

**Structure Decision**: 単一 Rust クレート(single project)。サブシステムをモジュールで分離し、
信頼境界(pcp / nostr / web)ごとに検証層を持つ。UI はビルド時埋め込みの静的アセットで
「単一 exe・自己完結」制約を満たす。

## Complexity Tracking

Constitution 違反はなし(本表は該当なし)。

## Phase 0 / Phase 1 成果物

- [research.md](./research.md) — NIP 選定、ライブラリ選定、エンコーディング、鍵保管などの決定記録
- [data-model.md](./data-model.md) — エンティティ・検証ルール・状態遷移
- [contracts/](./contracts/) — 4 契約(PCP / index.txt / ローカル API / nostr イベント)
- [quickstart.md](./quickstart.md) — ビルド・接続確認・シナリオ検証手順

**Agent context update**: `.specify/scripts/powershell/` に agent context 更新スクリプトが
存在しないためスキップ(リポジトリの CLAUDE.md / docs/agents/ が同役割を担う)。

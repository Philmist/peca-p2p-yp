# Implementation Plan: 分散型配信情報共有ネットワーク(YP代替)

**Branch**: `001-nostr-p2p-yp` | **Date**: 2026-07-03 (rev 2) | **Spec**: [spec.md](./spec.md)

**Input**: Feature specification from `/specs/001-nostr-p2p-yp/spec.md`

**Revision note**: rev 2(2026-07-03)で、リレーサーバー前提の nostr 通信を廃し、
spec Clarifications 2026-07-03 に基づく**利用者ノード間の純粋 P2P(nostr はデータスキーマのみ援用)**へ
全面改訂した。旧 rev のリレー関連決定(リレープール・NIP-65 等)は無効。

## Summary

単一障害点である中央 YP サーバーを、利用者ノードのみで構成される純粋 P2P ネットワークで置き換える。
nostr の援用は**イベント形式・署名(NIP のデータ構造)に限定**し(FR-014)、伝送はリレーサーバーを
前提としない独自の gossip プロトコルで行う。Windows 上で動作する自己完結型の単一 Rust バイナリを提供し、
以下を 1 プロセスで担う:

1. **PCP アナウンス受信**: PeerCastStation が従来 YP と同様に接続できる PCP(TCP)リスナー
2. **P2P gossip エンジン**: チャンネル情報を NIP-53 (kind 30311) 形式の署名済みイベントとして
   接続ピアへ伝搬・受信し、署名検証済みのチャンネル一覧をローカルに構築。
   ピアは手動シード登録 + ピア交換(PEX)で獲得し(FR-015)、UPnP による着信ポート開放を試みつつ
   外向き接続のみでも全機能が成立する(FR-016)
3. **HTTP 供給層**: 既存 YP ブラウザ向け `index.txt`(plain HTTP)+ ブラウザベースの管理 UI(簡易 Web サーバー)

**トラッカー解決(FR-004)の充足方式**: 現行 YP と同方式で、`index.txt` の TIP フィールド
(トラッカー ip:port)を介して視聴クライアントがトラッカーへ直接接続することで充足する。
PCP によるホストルックアップ(tracker lookup)は v1 の対象外とし、この判断は ADR-0002 に記録する。
本方式は「無改造クライアントが TIP(トラッカー直接続)のみで視聴開始できる」という
**検証可能な仮定**に依存する。ADR-0002 の記録項目には、この仮定・実機での検証結果
(quickstart 手順 4)・不成立時の代替(tracker lookup の追加実装)を含める。

UI 層はユーザーの Web ブラウザに置き、ネイティブ GUI は持たない。
初期シードピアは同梱せず、ユーザーが掲示板/SNS 等で発見してソフトウェアに貼り付けて登録する。
匿名文化の維持は「ペルソナ = nostr 鍵ペア」の複数保持・切替・破棄で実現する(spec Clarifications 参照)。
「真に信頼できるのは自分だけ」— 他ノード由来の情報(イベント・ピアアドレス)はすべて自ノードで
検証してから使用する(FR-015 / Principle II)。

## Technical Context

**Language/Version**: Rust(stable、edition 2021 以降。MSRV は依存クレートに追随し CI で固定)

**Primary Dependencies**:
- `tokio`(非同期ランタイム。P2P/PCP/HTTP の全 I/O)
- `nostr`(rust-nostr のプロトコルクレート。イベント構造・署名・検証(secp256k1 Schnorr)のみ使用。
  リレークライアント機能(`nostr-sdk` のリレープール)は**使用しない** — スキーマ限定援用(FR-014))
- `axum` + `tower`(Web UI・`/index.txt`・ローカル JSON API、レート制限ミドルウェア)
- P2P gossip プロトコルは自前モジュール(TCP + 長さ前置 JSON フレーム。research R13 参照。
  暗号処理は含まない — 完全性はイベント署名で担保)
- `igd-next`(UPnP IGD によるポートマッピング試行 — FR-016)
- `rusqlite`(bundled SQLite。設定・ペルソナ・ピア・ミュートの永続化)
- `windows`(DPAPI `CryptProtectData` によるペルソナ秘密鍵の暗号化保存)
- `encoding_rs`(index.txt の Shift_JIS 出力)
- `tracing`(構造化ログ+セキュリティイベントログ)
- `cucumber`(Gherkin シナリオの自動テスト — Principle IV)
- PCP プロトコルは自前モジュール(仕様は参考資料 gist に基づくクリーンルーム実装。コード流用なし)

**Storage**: SQLite 単一ファイル(`%APPDATA%\peca-p2p-yp\`)。秘密鍵は DPAPI で暗号化した BLOB。
チャンネル一覧・イベントストアは原則メモリ上(揮発)。ピアリストは永続化(再起動後の再接続用)

**Testing**: `cargo test`(unit/integration)+ `cucumber` で spec の Gherkin シナリオを実行。
契約テスト: index.txt ゴールデンファイル、PCP ハンドシェイクフィクスチャ、
インプロセスのモックピア(gossip プロトコルのテスト実装)による多ノード伝搬検証

**Target Platform**: Windows 10/11 x64。単一 exe(自己完結、インストーラ不要)。
PCP/HTTP は既定で 127.0.0.1 のみ。P2P 待受のみ既定で外部着信を受ける(UPnP 試行、無効化可)

**Project Type**: 常駐型ローカルサービス + ブラウザ UI(single project)

**Performance Goals**: 配信開始→他参加者の一覧反映 60 秒以内(SC-001)/一覧表示 5 秒以内(SC-004)/
同時 5,000 ノード・2,000 チャンネル規模で SC-001/004 を維持(SC-008)

**Constraints**: メモリ < 150MB、単一バイナリ、外部ランタイム不要、index.txt は plain HTTP(ユーザー要求)、
全ピア到達不能時もローカル機能(UI・index.txt)は継続動作、着信不可(NAT 内)でも全機能利用可(FR-016)

**Scale/Scope**: 想定同時ノード数 ≤ 5,000(SC-008)、同時配信チャンネル数百〜2,000、
接続ピア数: 外向き目標 8 / 着信上限 32(設定変更可)、ペルソナ数 ~100/ユーザー

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

| Gate | 原則 | 判定 | 根拠 |
|------|------|------|------|
| ユーザー安全リスク評価 | I | PASS | リスク: 悪意ピア/偽エントリ/悪性URL/PEX 経由の毒入れ。緩和: 署名検証(FR-005)、ピア由来情報の自己検証(FR-015)、URLスキーム許可リスト+警告UI(FR-012)、PCP/HTTP は loopback 既定で攻撃面最小化、P2P 受信は多段検証(contracts/p2p-gossip.md) |
| 既知脆弱性なしでリリース | I | PASS(継続ゲート) | `cargo audit`+Trivy を CI に組込み(ADR-0001 準拠)。High/Critical 未緩和ならリリース不可 |
| 入力検証・trust nothing | II | PASS | 3 つの信頼境界(PCP/TCP、P2P gossip、ローカル HTTP)すべてにサイズ・形式・内容の検証を設計(contracts/ 参照)。P2P はインターネットに露出する最大の攻撃面のため、フレームサイズ・レート・メッセージ種別・イベント署名の多段検証を契約に明記。エラーは内部情報を漏らさず、セキュリティイベントとして記録 |
| 既存暗号ライブラリ使用・自前暗号禁止 | II | PASS | 署名は `nostr` クレート(secp256k1/Schnorr)、鍵保護は OS 機能(DPAPI)。P2P トランスポートは暗号化なし(平文 TCP)であり自前暗号を含まない。完全性・真正性はイベント署名で担保し、盗聴耐性は要件外(掲載情報は公開データ)— この判断は ADR に記録 |
| セキュリティ設計決定の ADR 化 | II, VI | PASS(要フォロー) | 実装開始前に ADR を作成: ①イベントスキーマと NIP 援用範囲 ②ペルソナ鍵管理(DPAPI) ③脅威モデル(Sybil/汚染/PEX 毒入れ緩和: 検証+ミュート+ピア選別+任意 PoW。contracts/p2p-gossip.md「脅威と対応範囲」を入力とする) ④gossip プロトコルの形式的検証スコープ判断 ⑤P2P トランスポート非暗号化の判断。tasks の先頭フェーズに配置。**実装中ゲート 6(constitution)が参照する「セキュリティレビュー観点チェックリスト」は ③脅威モデル ADR の成果物として作成し、`docs/adr/` に併置する** |
| Gherkin 振舞い定義 | IV | PASS | spec.md に受け入れ+ネガティブシナリオあり。cucumber で対応付け、失敗確認後に実装(テストファースト) |
| 形式的検証のクリティカル判定 | V | **要 ADR(対象候補あり)** | **gossip 伝搬プロトコル(重複抑制・再伝搬・接続時同期)は新規設計**であり、クリティカル基準①(新規設計)を満たす。基準②(競合状態の非自明さ: 伝搬ループ・重複爆発・同期と伝搬の競合)・基準③(失敗の影響: 一覧の完全性)の該当を設計 ADR で判定し、該当なら PlusCal モデルを `docs/formal/` に作成してから実装する(SHOULD)。PCP 状態機械: 既存プロトコル準拠のため対象外(constitution 明記)。イベント署名・検証: 実績あるライブラリ利用のため対象外 |
| 原則トレーサビリティ | VI | PASS | 本表および各 contracts/ に原則参照を記載。ADR は原則番号を必須記載 |

**Post-Phase 1 再評価**: PASS — 設計成果物(data-model.md, contracts/)に新たな違反なし。
信頼境界ごとの検証ルールは各 contract に明記済み。Principle V の gossip 判定 ADR は
tasks 先頭フェーズの成果物として残る(実装開始前ゲート)。

**SHOULD からの逸脱記録**(constitution 規範キーワード節に基づく):
- spec FR-010 の「既定のシードピアを持つべき (SHOULD)」→ **同梱シードピアなし** とする。
  理由: ユーザー指示(初期ピアは掲示板/SNS 等で発見して提供する)。同梱リストは事実上の
  中央集権点・単一障害点の再導入となり本機能の動機と矛盾するため。代替として
  貼り付けによる一括登録とピアリストのテキスト書き出し(共有用)を UI に用意する。

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
│   ├── nostr-events.md  # イベントスキーマ(kind 30311/1311)契約 — nostr 援用はここに限定
│   └── p2p-gossip.md    # ノード間 gossip ワイヤプロトコル契約
└── tasks.md             # Phase 2 output (/speckit-tasks — 本コマンドでは生成しない)
```

### Source Code (repository root)

```text
Cargo.toml
src/
├── main.rs              # 起動・設定読込・各サブシステムの起動と監視
├── config.rs            # 設定(ポート・バインド先・鮮度閾値・ピア接続数など)
├── pcp/                 # PCP プロトコル: atom 符号化/復号、HELO/OLEH、BCST 解析、announce セッション
├── yp/                  # index.txt 生成(18 フィールド、Shift_JIS)、(将来: tracker lookup)
├── event/               # nostr イベントスキーマ: kind 30311 写像、署名・検証、鮮度・有効期限管理、
│                        #   addressable 置換ストア(ローカル実装 — リレー非依存)
├── p2p/                 # gossip エンジン: フレーミング、HELLO/EVENT/SYNC/PEERS、重複抑制、
│                        #   ピア管理(手動+PEX)、UPnP ポートマッピング、レート制限
├── identity/            # ペルソナ管理(鍵生成・切替・破棄)、DPAPI 鍵保管
├── store/               # SQLite 永続化(ペルソナ・ピア・ミュート・設定)
├── web/                 # axum ルーター: UI 静的配信、/api/v1、/index.txt、レート制限
└── security/            # 入力検証共通部、セキュリティイベントログ、URL 警告判定

ui/                      # Web UI 静的アセット(ビルド時にバイナリへ埋め込み)

tests/
├── features/            # Gherkin .feature(spec のシナリオと 1:1 対応)
├── contract/            # index.txt ゴールデン、PCP フィクスチャ、30311 スキーマ検証、gossip フレーム検証
└── integration/         # モックピア+PCP 疑似クライアントでの多ノード E2E(伝搬・PEX・切断耐性)

docs/
├── adr/                 # 0002〜: イベントスキーマ、鍵管理、脅威モデル、gossip 形式的検証判断、非暗号化判断
└── formal/              # gossip 伝搬の PlusCal モデル(Principle V 判定で該当した場合)
```

**Structure Decision**: 単一 Rust クレート(single project)。サブシステムをモジュールで分離し、
信頼境界(pcp / p2p / web)ごとに検証層を持つ。旧構成の `nostr/`(リレー通信込み)は
`event/`(スキーマ・検証・ローカル置換ストア)と `p2p/`(伝送)に分割した — スキーマ限定援用
(FR-014)をモジュール境界で強制する。UI はビルド時埋め込みの静的アセットで
「単一 exe・自己完結」制約を満たす。

## Complexity Tracking

Constitution 違反はなし(本表は該当なし)。
独自 gossip プロトコルの新規設計は複雑性の追加だが、spec Clarifications 2026-07-03 の
ユーザー決定(リレー排除)に直接由来する必須要素であり、Principle V の判定 ADR で統制する。

## Phase 0 / Phase 1 成果物

- [research.md](./research.md) — NIP スキーマ選定、gossip 設計、PEX、UPnP、ライブラリ選定などの決定記録
- [data-model.md](./data-model.md) — エンティティ・検証ルール・状態遷移
- [contracts/](./contracts/) — 5 契約(PCP / index.txt / ローカル API / イベントスキーマ / P2P gossip)
- [quickstart.md](./quickstart.md) — ビルド・多ノード接続確認・シナリオ検証手順

**Agent context update**: `.specify/scripts/powershell/` に agent context 更新スクリプトが
存在しないためスキップ(リポジトリの CLAUDE.md / docs/agents/ が同役割を担う)。

# Research: 分散型配信情報共有ネットワーク(YP代替)

**Date**: 2026-07-02 | **Plan**: [plan.md](./plan.md)

Technical Context の未確定事項をすべて解決した。各決定は「Decision / Rationale / Alternatives considered」で記録する。

## R1. チャンネル掲載に用いる nostr イベント種別

- **Decision**: NIP-53 Live Activities の **kind 30311(Live Streaming Event、addressable)** を採用し、
  PeerCast 固有情報は追加タグで表現する。`d` タグ = チャンネル GUID(hex 32 桁小文字)。
  実況コメント(将来フェーズ)は同じ NIP-53 の **kind 1311(Live Chat Message)** を
  `a` タグ `30311:<pubkey>:<d>` で紐づける。
- **Rationale**:
  - addressable イベントはリレー側で同一 `(kind, pubkey, d)` の旧版が自動置換されるため、
    30 秒周期の情報更新(リスナー数・曲情報)と親和性が高い。
  - `status`(live/ended)、`starts`、`current_participants`、`title`、`summary`、`t`(ジャンル)など
    必要フィールドの大半が標準タグで揃う(2026-07-02 に NIP-53 原文で確認済み)。
  - 既存 nostr クライアント(zap.stream 等)からも配信の存在が見えるという副次的な相互運用性。
  - YP 代替と BBS 代替が同一 NIP で完結し、FR-011(識別体系の互換)を自然に満たす。
- **Alternatives considered**:
  - 独自 kind(3xxxx 帯)の新規定義 — 相互運用性ゼロ、「実績ある仕様の援用」という依頼趣旨に反するため却下。
  - kind 1(テキストノート)への埋め込み — 置換不可で古い情報が堆積、構造化も貧弱なため却下。

## R2. 鮮度管理・自動除去(FR-006)

- **Decision**: 掲載側は配信中 60 秒ごとに 30311 を再発行し、**NIP-40 `expiration` タグ**
  (created_at + 600 秒)を付与。購読側は `status=live` かつ `created_at` が 600 秒以内の
  イベントのみを「配信中」と扱う。配信終了時は `status=ended` で最終発行。
- **Rationale**: リレー側の自動削除(NIP-40 対応リレー)とクライアント側の鮮度判定の二重防御。
  クライアント側判定が主であるため、NIP-40 非対応リレーでも FR-006 は成立する。
  時計ずれ(Edge Case)は 600 秒の余裕幅で吸収する。
- **Alternatives considered**: NIP-09 削除要求のみ — リレーの削除実施は任意でありクラッシュ終了時に
  機能しないため、鮮度判定の代替にならない(補助として終了時に併用は可)。

## R3. Rust nostr ライブラリ

- **Decision**: **rust-nostr(`nostr-sdk` クレート)** を採用。
- **Rationale**: nostr の Rust 実装として最も成熟・活発。署名/検証(secp256k1 Schnorr)、
  リレープール、購読管理、NIP-40/13/19/51 等を実装済み。自前暗号実装の禁止(Principle II)を
  ライブラリ選定で担保できる。
- **Alternatives considered**: `tungstenite` + `secp256k1` の手組み — イベント直列化や
  署名検証の自作はバグ=脆弱性に直結するため却下(Principle II)。

## R4. Web フレームワーク(UI・index.txt・ローカル API)

- **Decision**: **axum**(+ tower ミドルウェア)。UI 静的アセットは `include_dir` 等でバイナリに埋め込み。
- **Rationale**: tokio と同一エコシステム、tower によるレート制限・タイムアウト・ボディサイズ上限の
  ミドルウェア適用が容易(Security Requirements のレート制限に対応)。単一バイナリ制約と両立。
- **Alternatives considered**: actix-web(別ランタイム系で依存が重い)、tiny_http(ミドルウェア資産なし、
  自前実装が増える)— いずれも却下。

## R5. index.txt のエンコーディング

- **Decision**: 既定 **Shift_JIS(CP932、`encoding_rs`)** で出力し、設定で UTF-8 に切替可能とする。
  変換不能文字は HTML 数値参照ではなく `?` 置換とし、フィールド区切り `<>` を壊さないことを最優先。
- **Rationale**: 従来の日本語 YP エコシステム(YP ブラウザ)は Shift_JIS 前提のものが多い。
  互換性が最重要要件(FR-004)のため保守的な既定を選ぶ。
- **Alternatives considered**: UTF-8 固定 — 一部の既存 YP ブラウザで文字化けの恐れ。
- **Risk / follow-up**: 実際の YP ブラウザ(ユーザー所有)での表示確認を受け入れテストに含める。
  ここは実機確認までは仮説であることを明記しておく。

## R6. ペルソナ秘密鍵の保管

- **Decision**: **Windows DPAPI(`CryptProtectData`、`windows` クレート)** で暗号化した BLOB を
  SQLite に保存。ユーザー単位スコープ(`CRYPTPROTECT_LOCAL_MACHINE` は使わない)。
  UI からのエクスポート(nsec 表示)は明示操作+警告付きでのみ許可。
- **Rationale**: 追加ソフトウェア・追加パスフレーズなしで OS 標準の保護が得られ、
  「自己完結・インストール物を増やさない」というユーザー制約に合致。
- **Alternatives considered**:
  - `keyring` クレート(資格情報マネージャー)— 保存サイズ制限と列挙性の扱いが煩雑なため却下。
  - パスフレーズ派生鍵(argon2)— 起動毎の入力が UX を損なう。将来のオプションとしては排除しない。

## R7. 永続化ストア

- **Decision**: **rusqlite(bundled)** の単一 DB ファイル(`%APPDATA%\peca-p2p-yp\app.db`)。
  対象: ペルソナ、リレーリスト、ミュート、設定。発見チャンネル一覧はメモリ上のみ(揮発)。
- **Rationale**: 単一ファイル・自己完結・トランザクション。チャンネル一覧は本質的に
  エフェメラルであり永続化しない方が鮮度規則(R2)と整合する。
- **Alternatives considered**: JSON/TOML ファイル群(排他制御と部分更新が脆弱)、sled(安定性・
  メンテ状況に不安)— 却下。

## R8. スパム・Sybil 緩和の初期構成(FR-008、脅威モデル ADR の前提)

- **Decision**: v1 は多層のクライアント側緩和で構成する:
  1. 署名検証必須(検証失敗は不可視 — FR-005)
  2. ペルソナ単位・リレー単位のミュート/非表示(ミュートは NIP-51 kind 10000 形式でローカル保存、公開はしない)
  3. リレーの追加・削除・無効化(汚染リレーの切り離し)
  4. **任意の NIP-13 PoW フィルタ**(既定 0 = 無効。閾値を上げると低コスト大量掲載を減衰)
  5. 受信レート・イベントサイズの上限(1 接続あたり、全体)
- **Rationale**: オープン型既定(spec Clarifications)を守りつつ、利用者側の自衛手段を積層する。
  PoW は身元登録を要求しないため匿名文化(ペルソナモデル)と両立する唯一のコスト型対策。
- **Alternatives considered**: Web of Trust 必須化・招待制 — 既定非表示の禁止(FR-008 MUST NOT)に
  抵触するため却下。具体的な閾値・追加手法は脅威モデル ADR(tasks 先頭フェーズ)で確定。

## R9. PCP プロトコル実装の方針とライセンス

- **Decision**: 参考資料 gist(PeerCastStation の YP プロトコル解説)に基づき、
  **プロトコルの事実(atom 構造・ハンドシェイク手順)のみを用いたクリーンルーム実装**を行う。
  PeerCastStation(GPLv3)のソースコードは参照・複製しない。連携はプロセス間 TCP のみ。
- **Rationale**: constitution のライセンス暫定方針(許容的ライセンス想定、GPL 結合回避)に従う。
  プロトコル仕様という事実の利用は結合著作物を構成しない。
- **Alternatives considered**: PeerCastStation へのプラグイン追加 — GPL 結合となり方針に反するため却下。

## R10. 初期リレー(共有先)の入手経路

- **Decision**: 既定リレーは**同梱しない**。UI に「リレー URL の貼り付け一括登録」
  (`wss://` を 1 行 1 件)と「現在のリレーリストのテキスト書き出し」を用意し、
  掲示板/SNS でのリスト流通を前提とする。
- **Rationale**: ユーザー指示。同梱リストは事実上の中央集権点となり本機能の動機と矛盾する。
  spec FR-010 の SHOULD からの逸脱として plan.md に理由を記録済み(constitution 準拠)。
- **Alternatives considered**: 既知公開リレーの同梱 — 却下(上記)。NIP-65 による他者のリレーリスト
  参照は将来の発見手段として検討可(v1 スコープ外)。

## R11. BDD テスト基盤

- **Decision**: **`cucumber` クレート**で `tests/features/*.feature` を実行。spec.md の
  受け入れ・セキュリティシナリオと 1:1 対応させ、実装前に失敗を確認する(Principle IV)。
  統合層はインプロセスのモックリレー(WebSocket サーバー)と PCP 疑似クライアントで構成。
- **Rationale**: Rust ネイティブで Gherkin を実行できる唯一の成熟ライブラリ。外部ランタイム不要。
- **Alternatives considered**: Gherkin を doc として扱い通常テストで代替 — シナリオとテストの
  乖離が生じやすく Principle IV の MUST(対応付け)を満たしにくいため却下。

## R12. 待受ポートとバインド

- **Decision**: 既定値 — PCP アナウンス: `127.0.0.1:7146`、HTTP(UI + index.txt): `127.0.0.1:7180`。
  いずれも設定変更可。LAN 公開(0.0.0.0 バインド)は明示的なオプトインとし、UI で警告を表示する。
- **Rationale**: PeerCastStation の既定 7144(および多重起動時の 7145)との衝突を回避。
  loopback 既定は最小権限の原則(Principle II)に基づく攻撃面の最小化。
  YP ブラウザ・PeerCastStation・本ソフトは同一 PC で動く構成が典型のため loopback で成立する。
- **Alternatives considered**: 7144 共用 — 衝突リスクがあり却下。

## 解決済み確認

Technical Context に NEEDS CLARIFICATION は残っていない。
R5(Shift_JIS)のみ実機確認を受け入れテストに残す(リスクとして明示)。

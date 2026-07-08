# ADR-0012: 読み取り専用 index.txt に限る LAN 公開オプトイン

**Status**: Accepted(2026-07-08 ユーザー承認)
**Date**: 2026-07-08
**Principles**: Principle I (Safety First), Principle II (Security by Design),
Principle IV (Behavior-Driven Testing), Principle V (Formal Verification),
Principle VI (Principle Traceability)
**Supersedes**: ADR-0006 決定 4 を**部分的に** supersede(read-only index.txt に限る。
HTTP API / UI / PCP の loopback 強制は不変)

## 背景

ADR-0006 決定 4 は「LAN 公開オプトイン(HTTP/PCP)は v1 では実装しない」と定め、
`http_bind` / `pcp_bind` を loopback 強制とした。その却下理由 2(需要が未確認)は
「別 PC の YP ブラウザから自宅サーバー上の index.txt を取得したい」という具体的需要の
確認により前提が変わった。一方、理由 1(`X-Api-Token` 盗聴・PCP への LAN 内接続という
追加リスク)は **index.txt 配信には該当しない**(トークン不要・読み取り専用・公開情報)。

そこで、露出対象を read-only index.txt のみに限定した第 3 の設計点を採る。
要件は specs/004-lan-index-txt/spec.md、設計詳細は同 plan.md / research.md /
contracts/index-txt-lan.md を正とする。

## 決定

1. **専用の第 2 リスナー**: 新設定キー `index_bind`(既定空 = 無効)が非空のときのみ、
   index.txt 配信ルートだけをマウントした専用 HTTP リスナーを追加起動する。
   API・UI ルートは物理的に存在しない(経路フィルタ方式は「フィルタのバグ = 全 API の
   LAN 露出」という故障モードを持つため却下)。`http_bind` の意味・loopback 強制は不変。
2. **許可リスト検証**: `index_bind` は loopback / RFC 1918 / リンクローカル
   (169.254/16・fe80::/10)/ IPv6 ULA(fc00::/7)のみ受理する
   (新 helper `require_lan_or_loopback` — 既存 `require_loopback` は不変)。
   unspecified(0.0.0.0 / ::)・グローバルユニキャスト・CGNAT 共有アドレス空間
   (100.64.0.0/10、Tailscale 等)は拒否する(拒否リストでなく許可リストで構造的に弾く)。
3. **明示オプトイン + 監査**: 設定 UI での非 loopback 設定は警告 1 項目
   (「掲載一覧が LAN 内で平文・無認証のまま取得・改ざんされうる」)への明示確認を必須と
   する。非 loopback で bind に成功した起動時に SecurityEvent
   `index_txt_lan_exposed` を 1 件記録し、`GET /api/v1/status` に露出状態を表示する。
4. **縮退継続**: 専用リスナーの bind 失敗は致命とせず、警告ログ + status 反映のうえ
   本体を継続稼働する(既存 3 バインドの fail-fast は不変。付加機能の失敗が本体可用性を
   奪わない — Principle I)。
5. **保護の非緩和**: URL/ヘッダサイズ上限・per-IP レート制限(10 req/秒)は loopback 側と
   同一に適用する。plain HTTP のリスク受容は ADR-0006 決定 3 の枠内
   (公開目的の一覧・機密性要件なし・署名検証は上流の gossip 層で実施済み)。

## ADR-0006 決定 4「将来の解禁条件」との対応

| 決定 4 の解禁条件 | 本 ADR での充足 |
|--------------------|-----------------|
| 条件 1: contracts/local-api.md §保護方針の警告 2 項目を設定画面に MUST で実装 | **警告 (1)(攻撃面が LAN 全体へ拡大)**: index.txt 向けに具体化した 1 項目警告として実装(決定 3)。**警告 (2)(`X-Api-Token` の LAN 内盗聴)**: 本経路はトークンを一切運ばないため**対象外**。API/UI の LAN 公開を将来解禁する場合は警告 2 項目が改めて必要(本 ADR は解禁しない) |
| 条件 2: 明示的な確認操作を伴うオプトイン、既定は loopback のまま | `index_bind` 既定空(無効)+ UI の明示確認(決定 3)。`http_bind` の既定・強制は不変 |

決定 4 の却下理由との整合:

- 理由 1(トークン盗聴・PCP 接続リスク)— index.txt 限定により該当リスクを持ち込まない
- 理由 2(需要未確認)— 別マシン YP ブラウザ構成の需要が確認された
- 理由 3(OS 転送での回避)— 回避策は Windows で常用に不向き(管理者権限の portproxy /
  SSH 常駐)であり、検証・警告・監査つきの公式経路の方が安全側(Principle I:
  利用者が無検証の自己構成に流れることを防ぐ)

## Principle IV(自動テスト検証)の適用範囲

FR-005(UI 警告ゲート)の受け入れシナリオ(spec US3 シナリオ 1)は**手動検証**
(quickstart §6 = SC-006 手順)とする。理由: CI は Rust ツールチェーン(cargo test)
のみで JS/DOM テスト基盤を持たず、インライン script 構成の HTML に自動テストを課すには
Node ツールチェーン一式と CI ワークフローの変更が必要となり、本機能の規模に見合わない。

安全性の**強制点**は多層防御のバックエンド側 — `require_lan_or_loopback` による危険値の
構造的拒否(FR-003)と設定 API の検証エラー写像(400 `non_lan_bind`)— にあり、これらは
ユニットテスト・契約テストで自動検証される(Principle IV 充足)。UI ゲートは「無自覚な
有効化の防止」を担う追加層であり、その検証は quickstart の手動手順に固定する。
UI テスト基盤が将来導入された場合は自動化へ引き上げる。

## Principle V(形式的検証)の判断

本機能は**クリティカル非該当**とする。3 基準のうち「新規性」を満たさない —
既存の `axum::serve` + graceful shutdown + `Arc` 共有状態パターンの再利用のみで、
新規の並行アルゴリズム・プロトコル状態機械を導入しない。よって PlusCal モデルは
作成しない。

## 否定した選択肢

- **単一リスナー + 経路/接続元フィルタ** — フィルタの実装ミスが全 API の LAN 露出に直結
  する故障モードを持つ。物理分離はこの故障モード自体を持たない(Principle II)
- **CGNAT 100.64.0.0/10 の受理(Tailscale 対応)** — 露出先が物理 LAN を超えて VPN
  メッシュ全体(構成次第で第三者共有ノード)に広がりうる。ユーザー確認のうえ v1 では
  含めない(2026-07-08)。必要になれば本 ADR の改訂で扱う
- **bind 失敗の fail-fast** — 付加機能の失敗で YP 本体(掲載・発見・PCP)を落とすのは
  可用性の損失が利益に見合わない。既存 3 バインドとの扱いの差は「本体機能か付加機能か」
  で線を引く
- **カンマ区切り複数アドレス** — 需要未確認(YAGNI)。単一アドレスのみ受理し、
  複数化は将来拡張の余地として残す
- **HTTPS(自己署名)での LAN 配信** — ADR-0006 が却下済み(TOFU の弱い保証・証明書管理
  の複雑化)。YP ブラウザ互換の観点でも plain HTTP が要求仕様

## 帰結

- ADR-0006 冒頭へ「決定 4 は ADR-0012 により read-only index.txt に限り部分 supersede」
  の追記を行う(本 ADR の承認と同時)
- `CONTEXT.md` の信頼境界表に「index.txt(オプトイン時): LAN」を追記する
- contracts/local-api.md §保護方針の警告 2 項目要件は API/UI の LAN 公開(未解禁)向けに
  そのまま維持される

## 原則参照

- Principle I: 既定無効・明示確認・監査による無自覚な露出の防止。縮退継続による本体可用性の保護
- Principle II: 最小権限(index.txt のみの物理マウント)・許可リスト検証・保護の非緩和・自前暗号の不在
- Principle IV: 安全性の強制点(バックエンド検証)の自動テスト検証と、UI 警告ゲートの手動検証の線引き(本 ADR)
- Principle V: クリティカル非該当の判断と理由の記録(本 ADR)
- Principle VI: specs/004-lan-index-txt(spec / plan / contracts)と本 ADR の相互参照

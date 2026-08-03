# ADR-0015: 書き込みを含む Web UI / JSON API / 2ch 互換 API の LAN 公開オプトイン

**Status**: Accepted(2026-07-31 起草 — 007 Phase 1 実装前ゲート / 2026-07-31 実装同期・
最終化 T035 — 決定 1〜6 は実装と一致し逸脱なし)
**Date**: 2026-07-31
**Principles**: Principle I (Safety First), Principle II (Security by Design),
Principle V (Formal Verification), Principle VI (Principle Traceability)
**Supersedes**: ADR-0006 決定 4 を**さらに部分 supersede**(ADR-0012 が read-only
index.txt に限って解禁した LAN 公開を、書き込みを含む Web UI + JSON API・2ch 互換 API へ
拡張する。`http_bind` / `compat_bbs_bind` の loopback 強制を解く。`pcp_bind` の loopback
強制・PCP の LAN 公開禁止は不変)

## 背景

ADR-0006 決定 4 は「LAN 公開オプトイン(HTTP/PCP)は v1 では実装しない」と定め、
`http_bind`(Web UI + JSON API)・`compat_bbs_bind`(2ch 互換 API)・`pcp_bind` を
loopback 強制とした。ADR-0012 はそのうち **read-only の index.txt に限り**、専用リスナー +
許可リスト検証 + 明示オプトイン + 監査で LAN 公開を解禁したが、書き込み系(API/UI)と
互換 API は「トークン盗聴・検証バイパスという追加リスクを持ち込む」として loopback のまま
残した(ADR-0012 §ADR-0006 決定 4 との対応)。

007-web-bbs-write は「配信の視聴端末(稼働端末とは別の LAN 内端末)のブラウザ・専ブラから
実況スレに**書き込む**」ことを要件(spec US2 / FR-007〜FR-012・FR-020〜FR-022)とする。
これは ADR-0012 が明示的に未解禁とした「API/UI の LAN 公開」であり、かつ 2ch 互換 API は
設計上トークン認証を持たない(006 / ADR-0014 A4 — loopback 前提で無認証)。よって
「書き込みを含む LAN 公開」と「互換 API の無認証 LAN 書き込み」を新たなセキュリティ上の
設計決定として本 ADR に記録する(Principle II: セキュリティ上の設計決定は ADR に記録 MUST)。

要件は specs/007-web-bbs-write/spec.md、設計詳細は同 plan.md / research.md(R1/R7/R8/R9)/
contracts/lan-exposure.md を正とする。

## 決定

### 1. 許可リスト方式による bind の LAN 解禁

`http_bind` / `compat_bbs_bind` の検証を `require_loopback` から `require_lan_or_loopback`
(ADR-0012 で新設 — loopback / RFC 1918 / リンクローカル 169.254.0.0-16・fe80::/10 /
IPv6 ULA fc00::/7 のみ受理)へ変更する。unspecified(0.0.0.0 / ::)・グローバル
ユニキャスト・CGNAT 共有アドレス空間(100.64.0.0/10、Tailscale 等)は**拒否リストでなく
許可リストで構造的に弾く**(判定は `addr.ip().to_canonical()` 正規化後 — v4-mapped の
誤判定防止)。既定はいずれも loopback のまま(現状維持 — FR-007)。

### 2. 公開面ごとの 2 要素オプトイン(明示同意キー)

非 loopback bind には、公開面ごとの明示同意キー `http_lan_consent` /
`compat_bbs_lan_consent`(bool・既定 false)を必須とする。同意キーなしの非 loopback bind は
`Settings::validate` で**設定エラー**として起動を拒否する(config ファイル直接編集の経路でも
確認を強制できる。CLI 上書きにも同キーを追加する)。ADR-0012 は UI 警告での確認のみだったが、
007 は**書き込みを含む**公開であり、SC-004(100% 記録・確認)の検証可能性で同意キー方式が
勝る。設定 UI(`ui/settings.html`)は保存前の警告への明示確認で同意キーを同時に立てる
(FR-010)。面ごとに独立させることで「Web UI だけ LAN・専ブラは loopback のみ」の運用を許す。

### 3. Host ホワイトリスト拡張 + 送信元 IP 許可リスト検証(多層防御)

- **Host 検証**: Host ヘッダのホワイトリストを「loopback 3 形式 + 非 loopback bind 時は
  `{bind_ip}:{port}`(IPv6 は `[{ip}]:{port}`)」へ拡張する(FR-009)。DNS rebinding /
  CSRF 対策(006 既存)を LAN 公開後も維持するための必須変更。
- **送信元検証**: 非 loopback 待受時は送信元 IP の許可リスト検証をミドルウェアで行い、
  `to_canonical()` 正規化後に loopback / RFC 1918 / リンクローカル / ULA 以外の送信元は
  403 + ログで拒否する(FR-021「LAN 内アドレス限定」)。bind 先が LAN でも、ルータの
  ポート転送等の誤設定に対する安価で効果のある多層防御となる。

### 4. 保護水準の非緩和(loopback と同一)

per-IP レート制限(互換 API 20 req/秒)・URL/ヘッダ/ボディサイズ上限は loopback 側と
**同一**に適用する(FR-011)。LAN 公開だからといって緩めない。既存の接続元ごと
`RateLimiter` はそのまま LAN でも機能する。

### 5. 互換 API の無認証 LAN 書き込みの受容(補償つき)

2ch 互換 API はトークン認証を持たない(専ブラ互換のための設計 — 006 / ADR-0014 A4)。
その面を LAN 公開すると「LAN 内の任意端末が無認証で書き込める」状態になる。これを**受容**し、
以下の補償で釣り合わせる:

- 書き込みは通常経路と**同一の検証**を通す(板鍵署名・名前欄 `#` 除去・サイズ上限・
  PoW / レート — 抜け道を作らない。ADR-0014 A4「互換 API の抜け道禁止」を LAN でも維持)。
- Host 検証・per-IP レート制限・送信元 LAN 限定(決定 3/4)。
- 面ごとの明示オプトイン + 起動時 SecurityEvent(決定 2/6)。
- 信頼境界を「LAN 内」と置く(Clarifications 2026-07-31: PIN/QR 等の追加認証は課さない。
  LAN 内を信頼境界とする)。

### 6. 監査(SecurityEvent)

`SecurityCategory` に 2 カテゴリを追加する(006 時点 21 → 23):

- `WebUiLanExposed`: `http_bind` が非 loopback で待受に成功した起動時に 1 件。
- `CompatBbsLanExposed`: `compat_bbs_bind` が非 loopback で待受に成功した起動時に 1 件。

記録は `IndexTxtLanExposed`(src/main.rs)と同パターン(`to_canonical()` で loopback 判定)。
面ごとにカテゴリを分けることで「どの面を公開したか」が監査ログから判別できる(SC-004)。
拒否系(ホワイトリスト外 Host・許可範囲外送信元・レート超過)は既存カテゴリを流用する。

### 7. 互換ポートのブラウザ向け板ページと互換名前空間 JSON(追補 — FR-023/023a/b/c)

互換 API 待受面(`compat_bbs_bind`)は、専ブラ向けパスに加えて板ルート URL `.../{board}/` を
旧来 BBS UI(HTML・UTF-8)として同一ポート・同一パスで配信する(FR-023。他面へのリダイレクト
なし)。その板ページ SPA の**自己完結**を、**トークン保護 API 面(`/api/v1`)を互換リスナーへ
一切生やさず**、互換名前空間の JSON で成立させる(FR-023a):

- `GET /{board}/board.json`(視聴者向け最小 `CompatBoardView` — 板主設定 `first_post_template`
  / `first_post_pow_bits` と送信中投稿 `pending` を**含めない**)、`GET /boards.json`(板一覧)、
  `POST /{board}/write.json`(トークンレス・board スコープ・実体は既存 `bbs_cgi::submit` と同一)。
- **不変条件の維持**: 互換リスナーの「`/api/v1`(トークン保護 API)を物理的に持たない」設計
  (`src/web/compat/` — 経路フィルタ由来の保護 API 露出の故障モードを構造的に排除)を字義でも
  精神でも維持する。新 JSON 読取は「公開済み SJIS 読取(subject.txt/dat/SETTING.TXT/head.txt)の
  JSON 再エンコード」に留め、新規のデータ種別・板鍵・秘密・トークン面を増やさない。
- 3 面とも既存の Host 検証 + 送信元 LAN 限定 + per-IP レート制限を共有(保護は非緩和 — 決定 3/4)。
- **視聴者スコープ(FR-023b)**: 互換オリジンでは閲覧・レス書込・板横断ブラウズのみ。ホスト管理
  (開設・板設定・モデレーション・BAN 一覧・チャンネル選択)は非表示(`http_bind` UI 専用)。
- 検討した代替: 互換面へ閲覧専用 `/api/v1` を限定公開する案は「`/api/v1` 非露出」不変条件を字義
  的に破壊するため退けた(research R13)。

## 受容リスク

| リスク | 受容理由 / 補償 |
|--------|-----------------|
| **`boards.json` による視聴活動の露出(FR-023c)** | 互換面の `GET /boards.json` は自ノードのホスト板に加えて**参加中(視聴)板**も列挙するため、自ノードがどの他ノード板を視聴中かが LAN のトークンレス面へ露出する。実況の UX(視聴者が別端末ブラウザで板を辿って書き込む)を成立させるために**受容**する。信頼境界を LAN 内に置く前提(決定 5)・`compat_bbs_lan_consent` の「公開して視聴・書込を募る」意図の範囲内。列挙は最小フィールド(board_id・title・res_count・is_local)に留め、tip/channel/内部状態は出さない。グローバル公開は許可リストで 100% 拒否(決定 1) |
| **LAN 内平文トークン共有** | Web UI の JSON API トークン(`GET /api/v1/token`)は plain HTTP で LAN 内を流れ、LAN 内の他端末に盗聴・共有されうる。ADR-0006 決定 3(plain HTTP のリスク受容)の枠内。信頼境界を LAN 内に置く前提(決定 5)で受容。グローバル公開は許可リストで 100% 拒否(決定 1)し、LAN を越えた露出は生じない |
| **互換 API の無認証 LAN 書き込み** | 決定 5 の補償(同一検証・Host/送信元/レート・明示オプトイン・監査)で釣り合わせて受容 |
| **LAN 内の他端末による改ざん・なりすまし書き込み** | 書き込みは板鍵署名検証を通り、順序確定はホスト採番(006 不変)。LAN 内の悪意ある端末は板鍵を持たない限りホスト由来の採番を偽造できない。per-IP レート・PoW で洪水を抑制 |
| **ルータのポート転送誤設定によるグローバル到達** | 送信元 IP 許可リスト検証(決定 3)で LAN 外送信元を 403 拒否する多層防御 |

## 実装同期(最終化 T035)

実装は決定 1〜6 および contracts/lan-exposure.md に一致し、設計からの逸脱はない。
最終化時点で確認した主な実装位置:

- **決定 1/2(許可リスト + 2 要素オプトイン)**: `src/config.rs` の `require_lan_or_loopback`
  / `require_lan_consent`、`http_lan_consent` / `compat_bbs_lan_consent`(bool・既定 false)。
  `Settings::validate` が config ロード・CLI 上書き(`--http-lan-consent` 等)・設定 UI 適用の
  全経路で強制する。判定は `to_canonical()` 正規化後。
- **決定 3(多層防御)**: `src/web/mod.rs` の `host_allowlist` / `lan_source_guard`
  (非 loopback 待受時のみ `enforce_lan_source` を立て最外周で送信元 IP を LAN 限定検証)、
  `src/web/compat/mod.rs` の同等実装。拒否は `forbidden_source` / `forbidden_host` ログ。
- **決定 6(監査)**: `src/security/mod.rs` の `SecurityCategory::WebUiLanExposed` /
  `CompatBbsLanExposed`(21→23)、`src/main.rs` の面ごと起動時記録(`IndexTxtLanExposed`
  と同パターン)。
- **設定 UI**: `ui/settings.html` は非 loopback 保存時に面ごとの警告 + 明示確認で同意キーを
  立てる。同意が既に永続化済みなら再確認を求めず(保存後にフォームを再読込して整合)。

### FR-005 クライアント側エスケープの検証範囲(Principle IV)

実況スレの安全描画(FR-005)は責務を 2 層に分けて検証する:

- **サーバ側の無害化 — 自動テストで検証**(T039 = `tests/features/safe_rendering.feature`):
  ローカルルール Markdown の `<script>`/生 HTML 無害化(`src/web/livechat.rs` の
  `render_local_rules_html`)、およびレス本文・名前・固定 >>1 テンプレが API 応答で
  プレーンテキストのまま往復しサーバ側で HTML 化されないこと。
- **クライアント側の描画エスケープ — 実機確認で検証**(T012 = quickstart V-1):
  `<script>` 注入がブラウザでテキスト表示されること。DOM 依存(`textContent` /
  `>>n` アンカー化)のため、ヘッドレス DOM を持ち込まず目視確認とする判断:描画は
  既存 UI 資産のみで外部依存を増やさず(research R5)、注入テキストの可視化は目視が
  最も確実かつ低コストなため。

### セキュリティレビューチェックリスト適用結果(実装中ゲート 6)

007 は信頼境界(`src/web/`)・入力検証・`src/security/` を変更するセキュリティ関連 PR であり、
docs/adr/security-review-checklist.md を適用する:

- [x] **1 入力検証(サイズ)**: ボディ 64KB・per-IP レート 20req/秒を loopback と同一に維持
  (決定 4)。LAN 公開で緩和しない
- [x] **2 入力検証(形式・内容)**: 書き込みは 006 と同一の多段検証(署名→形式→状態→BAN
  →PoW→レート)。LAN 経路(Web UI / 互換 API)でも抜け道を作らない(決定 5)
- [x] **3 信頼境界ごとの強度**: 非 loopback 露出に Host 検証 + 送信元 LAN 限定 + 面別
  オプトイン + 起動時監査を上乗せ。強度を弱める変更はない
- [x] **4 エラー応答の情報漏洩**: 追加拒否は定型コード(`forbidden_source` /
  `forbidden_host` / `lan_consent_required` / `non_lan_bind` / `non_loopback_bind`)のみ。
  内部情報を含めない
- [x] **5 最小権限・バインド**: 既定 loopback を維持(FR-007)。解禁は許可リスト
  (`require_lan_or_loopback`)+ 明示同意キーの 2 要素で、グローバル/未指定/CGNAT を
  構造的に拒否。`pcp_bind` の loopback 強制は不変
- [x] **6 暗号**: 板鍵署名は既存 nostr クレート。自前暗号・独自乱数の追加はない
- [x] **7 秘密情報の取り扱い**: JSON API トークンの LAN 平文共有を受容リスクとして明示
  (決定 5・受容リスク表)。ログ・SecurityEvent・エラー応答へ秘密鍵/トークンを出力しない
- [x] **8 セキュリティイベントログ**: 2 カテゴリ追加を data-model / 本 ADR(決定 6)へ
  先行記録。面別に記録し公開面が監査ログから判別可能(SC-004)。ローテーションは不変
- [x] **9 レート制限・資源上限**: per-IP レートは LAN でも同一。送信元多様化に対する
  秒内有界化を維持(`src/web/mod.rs`)。無制限成長する構造の追加はない
- [x] **14 コメント**: 検証順序・正規化・境界条件(`to_canonical()` 判定、送信元ガードの
  評価位置)に意図コメントを付与済み
- [ ] **13 テスト対応**: LAN 公開のネガティブシナリオ(拒否 4 種・Host 拒否・送信元拒否・
  レート同一)は T013〜T017 で自動化済み。**FR-005 サーバ側の安全描画テスト T039 は
  追補予定**(上記「FR-005 検証範囲」)— Principle IV の残タスク
- 10,11,12: 非該当(10/11 = P2P 伝搬の不変条件は不変 [固定 >>1 も既存採番経路に流すだけで
  ADR-0014 の安全性を変えない]、12 = 依存クレートの追加なし)

## Principle V(形式的検証)の判断

本機能は **クリティカル非該当** とする。3 基準のうち「新規性」を満たさない — 007 は
(a) bind 検証の許可リスト分岐(ADR-0012 で検証済みの `require_lan_or_loopback` の適用面
拡大)、(b) Host / 送信元検証ミドルウェアの追加(逐次検査)、(c) 表現層(Web UI)の刷新、
(d) 既存採番経路への固定 >>1 の**挿入**(006 で TLC 検証済みの順序確定 — ADR-0014 — を
変更しない)からなり、新規の並行アルゴリズム・プロトコル状態機械を導入しない。固定 >>1 は
ホスト自身の書き込みを既存 `accept_write` 経路に流すだけで、採番・移行・凍結/クローズの
規則を変えない(モデル ADR-0014 の安全性を損なわない)。よって PlusCal モデルは作成しない。

## 否定した選択肢

- **ADR-0012 と同一の UI 確認のみ(同意キーなし)** — config 直接編集で確認を迂回でき、
  FR-010 の MUST を検証可能な形で満たせない。書き込み面は index.txt より露出の影響が
  大きいため、config 経路でも強制できる同意キー方式を採る。
- **単一の `lan_expose` フラグで両面一括** — 「Web UI だけ LAN・専ブラは loopback」の
  ような面別運用(spec Edge Case)ができない。面ごとの同意キーとする。
- **PIN / QR 等の追加認証** — Clarifications で「追加認証は課さない(LAN 内を信頼境界と
  する)」と確定済み。専ブラ互換(無認証前提)とも整合しない。
- **CGNAT 100.64.0.0/10 の受理** — ADR-0012 と同じく、露出先が物理 LAN を超えて VPN
  メッシュへ広がりうるため v1 では含めない。
- **同意のイベント記録(DB テーブル)** — `*_lan_consent` を bool でなく「いつ・どの経路
  (UI 確認 / config 手編集 / CLI)・どの版のリスク文言に同意したか」の出来事として DB に
  記録する案。v1 では採らない:(a) 単一運用者のローカルアプリで同意記録の参照者 = 同意者
  本人であり、settings を書ける者は同意行も書き換えられるため記録の完全性は上がらない
  (自己文書化以上の価値がない)。(b) セキュリティ上意味があるのは「過去に同意したか」より
  「今公開されているか」であり、後者は毎起動の `Settings::validate` 強制と起動時
  SecurityEvent(決定 6)が実効的に担保する。恒久的な同意記録より毎起動の再検証・再記録の
  方が監査として実効的。(c) FR-010 / SC-004 は現方式(同意キー + 起動時イベント)で検証
  可能に満たされる。将来リスク範囲が変わる場合(例: リモート公開の導入で警告文言の範囲が
  広がる)は、bool では「どの版のリスクに同意したか」を表現できず再同意を強制できないが、
  その解はテーブル追加でなく**同意キーの値をリスク改訂識別子にする**(例:
  `http_lan_consent = "adr-0015-r2"` とし validate が現行改訂との一致を要求)ことで足りる
  — settings テーブルの耐久性はそのまま、いつ・何への同意かを表現でき、config 手編集では
  特定文字列の記入がより明示的な同意になる。必要になるまで導入しない。
- **互換 API の LAN 公開のみ禁止(Web UI だけ解禁)** — 視聴端末の専ブラからの書き込み
  (spec US2 / FR-022)が実現できない。補償つきで受容する(決定 5)。

## 帰結

- ADR-0006 冒頭 / ADR-0012 の「決定 4 の supersede 範囲」に、本 ADR が書き込みを含む
  Web UI + JSON API・2ch 互換 API へ拡張する旨を追記済み(T035 で実施 — 両 ADR の
  ヘッダに ADR-0015 への相互参照を追加)。
- `CONTEXT.md` の信頼境界表・モジュール表・ADR 一覧への反映は T036 で行う。
- セキュリティレビューチェックリスト(docs/adr/security-review-checklist.md)の適用結果は
  本 ADR「実装同期(最終化 T035)」節に記録済み(残: FR-005 サーバ側テスト T039)。
- 実装で確定した設計判断は本 ADR「実装同期(最終化 T035)」節に記録済み(逸脱なし
  — ADR-0014 の運用と同じ)。

## 原則参照

- Principle I: 既定 loopback・面別の明示同意・起動時監査による無自覚な露出の防止。
  グローバル公開の構造的拒否(許可リスト)。plain HTTP / 無認証書き込みのリスクを LAN 内
  信頼境界の明示のもとで受容。
- Principle II: 許可リスト検証(正規化後判定)・Host / 送信元の多層検証・保護の非緩和・
  互換 API の抜け道禁止(同一検証)・自前暗号の不在(署名は既存 nostr ライブラリ)。
- Principle V: クリティカル非該当の判断と理由の記録(本 ADR)。
- Principle VI: specs/007-web-bbs-write(spec / plan / research / contracts)と本 ADR、
  ADR-0006 / 0012 / 0014 の相互参照。

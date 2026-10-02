# Data Model: 007-web-bbs-write

**Date**: 2026-07-31 | **Plan**: [plan.md](plan.md) | **Research**: [research.md](research.md)

006 のデータモデル(`specs/006-livechat-thread/data-model.md`)への**差分**のみを記す。
006 のエンティティ(スレ・レス・OrderInfo・板鍵・NG/BAN)は変更しない。

## 永続化の方針(差分)

| データ | 置き場所 | 変更 |
|--------|----------|------|
| 板設定(固定 >>1 テンプレ含む) | SQLite `board_settings` テーブル | 列追加 + production 配線(006 で定義済み・未配線 — research R4) |
| LAN 公開設定(bind・同意キー) | 設定ファイル(`Settings`) | キー追加 |
| 固定 >>1 の確定レス | インメモリ(確定レス列) | 006 どおり揮発。>>1 も通常レスとして扱う |

## エンティティ

### BoardSettings(拡張)— `src/livechat/thread.rs`

006 既存フィールド(`title` / `res_limit` / `noname_name` / `local_rules` /
`first_post_pow_bits`)に以下を追加する。

| フィールド | 型 | 検証 | 意味 |
|-----------|-----|------|------|
| `first_post_template` | `String`(既定 `""`) | ≤ 2048 文字・≤ 32 行・制御文字除去(改行は保持)— kind 1311 本文検証と同一(research R2) | 固定 >>1(スレ頭テンプレ)本文。空 = 未設定(システム既定テンプレを使用) |

- **検証**: `BoardSettings::validate` に値域チェックを追加。`sanitized()` は
  `local_rules` と同様に改行保持の制御文字除去を適用する。
- **配布**: 006 の板設定配布(SETTING.TXT / JSON API の settings)と同じ経路。
  互換 API の SETTING.TXT へ新項目は出さない(専ブラ互換性に影響しないため)。
- **反映規則**: テンプレ変更は**次に開始するスレから**適用(FR-017)。確定済みスレの
  >>1 は不変(006 FR-023)。`res_limit` の「次スレから反映」と同じ規則。

### 固定 >>1(自動投稿レス)— 新規エンティティではない

自動投稿された >>1 は **006 の「レス(kind 1311)」そのもの**であり、専用の型を持たない。

| 属性 | 値 |
|------|-----|
| res_no | 1(スレ開設・次スレ移行の直後にホストが採番) |
| 本文 | 投稿時点の `first_post_template`。空なら**システム既定テンプレ**: 板タイトルと対象チャンネル名の案内(1〜数行の固定文言 — contracts/fixed-first-post.md) |
| 名前・メール | 空(表示は `noname_name`) |
| 署名 | 板鍵(ホスト管理)による kind 1311 署名 — 通常書き込みと同一(research R3) |
| PoW | 課さない(ホスト自身の採番 — FR-016) |
| res_limit カウント | 含む(res_no=1 を占有 — FR-019) |

**状態遷移**: スレ開設(`open_thread`)/ 次スレ移行(自動・明示)→ >>1 採番・確定 →
通常配布。参加者側は通常レスとして受理(変更なし)。

### Settings(拡張)— `src/config.rs`

| キー | 型 / 既定 | 検証 | 意味 |
|------|-----------|------|------|
| `http_bind` | 既存(既定 `127.0.0.1:7180`) | `require_loopback` → **`require_lan_or_loopback` へ変更**(loopback / RFC 1918 / リンクローカル / ULA。グローバル・未指定・CGNAT 拒否。`to_canonical()` 正規化後判定) | Web UI + JSON API の待受 |
| `compat_bbs_bind` | 既存(既定 `127.0.0.1:7183`) | 同上 | 2ch 互換 API の待受 |
| `http_lan_consent` | `bool` / `false` | `http_bind` が非 loopback のとき `true` 必須(でなければ設定エラー) | Web UI 面の LAN 公開への明示同意(FR-010) |
| `compat_bbs_lan_consent` | `bool` / `false` | `compat_bbs_bind` が非 loopback のとき `true` 必須 | 互換 API 面の LAN 公開への明示同意 |

- 既定はいずれも loopback のみ(現状維持 — FR-007)。
- CLI 上書き(`CliOverrides`)にも同意キーを追加する(config と同じ検証)。

### board_settings テーブル(拡張)— `src/store/mod.rs`

`BoardSettingsRow` に `first_post_template` 列を追加。既存テーブルにはマイグレーション
(`ALTER TABLE ... ADD COLUMN ... DEFAULT ''`)で追加する。`get/set_board_settings` を
production 経路(起動時読込・settings PUT 適用時保存)へ配線する(research R4)。

### Web UI ページ(表現層 — コード上の新規型なし)

| ページ | ルート | 構成要素 |
|--------|--------|----------|
| 板選択 | `#/` | 板一覧(各項目は `#/board/{board_id}` へのリンク — FR-002a) |
| 板ページ | `#/board/{board_id}` | スレ一覧(番号・タイトル・レス数、各項目はスレページへのリンク — FR-002a)/ 新規スレ作成欄(スレタイトル + >>1 本文プリフィル・上書き欄 — FR-014a)/ ローカルルール掲示(安全描画)/ 板タイトル・名無し名の提示 / **板管理セクション**(板設定編集 + BAN 一覧管理。スレ非依存 — FR-006a/b) |
| スレページ | `#/thread/{board_id}/{thread_key}` | レス列(`{res_no} :{名前}:{日付} ID:{id}` + 本文、既定最新 50)/ `>>n` アンカー(ジャンプ + ポップアップ、解決不能表示)/ 書き込み欄(名前・メール・本文)/ 全部読む・更新 / 管理操作(個別レスのモデレーション実行・join/leave/next/close。板設定編集は板ページへ移設 — FR-006a/b) |

表現層のみの変更で、書き込み・検証経路(006)は不変(research R12)。詳細は
contracts/web-ui.md(§2 板ページ・§3.7 スレページ管理・§7 互換ブラウザ板ページ)。

## SecurityEvent 追加カテゴリ — `src/security/mod.rs`

006 時点 21 カテゴリ → **23 カテゴリ**。

| カテゴリ | 記録タイミング |
|----------|----------------|
| `WebUiLanExposed` | `http_bind` が非 loopback で待受に成功した起動時に 1 件(`IndexTxtLanExposed` と同パターン) |
| `CompatBbsLanExposed` | `compat_bbs_bind` が非 loopback で待受に成功した起動時に 1 件 |

拒否系(ホワイトリスト外 Host・許可範囲外送信元・レート超過)は既存カテゴリ
(`forbidden_host` / `compat_bbs_denied` / `http_rate_limited` 等)を流用する。

## JSON API 差分(概要 — 詳細は contracts/web-ui.md)

- 板詳細レスポンスに `compat_bbs_port`(number | null)を追加(UI が互換 API の板 URL を
  閲覧元 hostname から動的生成するため — research R5)。
- 板設定 PUT/GET に `first_post_template` を追加。
- スレ開設 API(`POST .../open` 系)に任意 `first_post_override`(string、省略可)を追加
  (そのスレ限りの >>1 上書き。検証は固定テンプレと同一・永続テンプレは不変 — FR-014a /
  research R12 / contracts/web-ui.md §5.3)。
- それ以外のエンドポイント・認証(トークン)・レート制限は変更なし。

## 互換 API 面のブラウザ向け板ページ(FR-023 — research R13)

- `compat_bbs_bind` の待受面は、専ブラ向けパス(`.../{board}/subject.txt` / `dat` /
  `bbs.cgi`)に加えて、板ルート URL `.../{board}/`(ファイル名なし)への GET へ旧来 BBS UI の
  板ページ(HTML・UTF-8)を同一ポート・同一パスで配信する(パスで振り分け)。専ブラ向け
  応答(Shift_JIS)・検証は変更しない。他面へのリダイレクトはしない。
- 実装は `src/web/compat/mod.rs` のルータ分岐追加(既存 Web UI の `include_str!` 資産を再利用)。

### 互換名前空間 JSON エンドポイント(FR-023a/b/c — research R13)

板ページ SPA の自己完結を、**トークン保護 `/api/v1` を互換面へ生やさず**互換名前空間の JSON で
成立させる(不変条件「互換リスナーは `/api/v1` を物理的に持たない」の維持)。3 面とも既存の
Host 検証 + 送信元 LAN 限定 + レート制限を共有する。

| エンドポイント | 種別 | データ源 | 備考 |
|---|---|---|---|
| `GET /boards.json` | 板一覧 | registry(ホスト板)+ manager(参加中板) | 最小フィールド `{board_id,title,res_count,is_local}`。tip/channel/内部状態は出さない(FR-023c) |
| `GET /{board}/board.json` | 板詳細 `CompatBoardView` | `resolve_snapshot`(registry 優先・無ければ常駐セッション) | 視聴者向け最小(下記)。未知は 404 |
| `POST /{board}/write.json` | 書込 | 既存 `bbs_cgi::submit`(自板採番 / 未知板は `manager.write`) | トークンレス・board スコープ `{name,mail,body}`。202/400。FR-022 と同一検証 |

**`CompatBoardView`(board.json のペイロード — 視聴者向け最小)**

| フィールド | 由来 | 備考 |
|---|---|---|
| `title` / `noname_name` / `res_limit` | BoardSettings | SETTING.TXT 相当の公開情報 |
| `local_rules_html` | `render_local_rules_html` | 安全 HTML 化済み。原文 `local_rules` は**出さない** |
| `res[]` | 確定レス列(ResView 同形の最小: `res_no,name,mail,body,created_at,id` — `id` は dat と同一導出の短縮 ID) | dat 相当の公開情報 |
| `thread` | `{generation, res_count}` | 現行スレ記述子(SPA のスレルーティング用) |
| `compat_bbs_port` | 待受ポート | 板 URL の自己生成用 |

- **含めない(MUST NOT)**: `first_post_template` / `first_post_pow_bits`(板主設定)、`pending`
  (送信中の自分の投稿=セッション概念。互換面はセッションレス)。= トークンレス・別端末面への
  最小露出(FR-023a)。
- 位置づけ: 既存の公開 SJIS 読取(subject.txt/dat/SETTING.TXT/head.txt)の JSON 再エンコード。
  新規のデータ種別・板鍵・秘密・トークン面を増やさない。

**受容する露出(ADR-0015 追補)**: `boards.json` は参加中(視聴)板も列挙するため、自ノードの
視聴活動が LAN のトークンレス面へ露出する。実況公開(`compat_bbs_lan_consent`)意図の範囲として
受容する(FR-023c)。

**SPA の配信オリジン分岐(FR-023b)**: `ui/livechat.html` は起動時に配信オリジン(`/{board}/`
パス配信=互換)を検出し、互換オリジンでは読取=board.json・一覧=boards.json・書込=write.json に
振り替え、ホスト管理(開設・板設定・モデレーション・BAN 一覧・チャンネル選択・token 取得)を
非表示化する。同一 `include_str!` 資産のまま実行時分岐する。

## 原則参照

- 許可リスト検証・同意キー・SecurityEvent: Principle I(ユーザー安全)・II(Security by
  Design)
- 固定 >>1 が通常検証を満たす経路で確定すること(検証の抜け道なし): Principle II
- テーブルマイグレーションと配線の明示: Principle III(保守可能性)

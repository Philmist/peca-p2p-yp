# Research: 007-web-bbs-write

**Date**: 2026-07-31 | **Spec**: [spec.md](spec.md)

spec が plan へ持ち越した未確定点(公開面ごとの設定単位・固定 >>1 テンプレの書式と上限・
Web UI の構造/アンカー UI/読み出しレンジ)と、実装調査から浮かんだ論点を確定する。
コード参照は 2026-07-31 時点の `feature/livechat-thread` ブランチによる。

## R1. LAN 公開の設定単位とオプトイン方式

**Decision**: bind アドレス自体で公開範囲を表現し(`http_bind` / `compat_bbs_bind` に
LAN 内プライベートアドレスを指定可能にする)、非 loopback 指定時は公開面ごとの明示同意キー
`http_lan_consent` / `compat_bbs_lan_consent`(bool、既定 false)を必須とする
**2 要素オプトイン**とする。同意キーなしの非 loopback bind は `Settings::validate` で
設定エラーとして拒否する。許可リスト検証は既存 `require_lan_or_loopback`
(`src/config.rs:504-521`)を `http_bind` / `compat_bbs_bind` に適用拡張する
(現状は `require_loopback` — `src/config.rs:330,341`)。

**Rationale**:
- 公開面ごとの独立設定(spec Edge Case「公開面ごとのオプトイン」)は、既に bind が
  面ごとに分かれている(`http_bind` = Web UI + JSON API、`compat_bbs_bind` = 互換 API)
  ため、bind 値 + 面ごとの同意キーで自然に表現できる。新しい設定面の概念を導入しない。
- FR-010 は「明示確認を必須とし (MUST)」。ADR-0012(index.txt)は UI 警告での確認のみ
  だったが、007 は**書き込みを含む**公開であり、config ファイル直接編集の経路でも確認を
  強制できる同意キー方式が SC-004(100% 記録・確認)の検証可能性で勝る。設定 UI
  (`ui/settings.html`)は保存前警告で同意キーを同時に立てる。
- 判定は `addr.ip().to_canonical()` 正規化後に行う(v4-mapped 誤判定防止 — 既存方針)。

**Alternatives considered**:
- *ADR-0012 と同一の UI 確認のみ*: config 直接編集で確認を迂回でき、FR-010 の MUST を
  検証可能な形で満たせないため退けた。
- *単一の `lan_expose` フラグで両面一括*: 「Web UI だけ LAN、専ブラは loopback のみ」の
  ような運用(spec Edge Case が独立設定を要求)ができないため退けた。
- *PIN/QR 等の追加認証*: Clarifications で「追加の別途認証は課さない(LAN 内を信頼境界と
  する)」と確定済み。

## R2. 固定 >>1 テンプレの書式・上限・検証

**Decision**: プレーンテキスト(Markdown 描画しない)、上限は **2048 文字・32 行**
(kind 1311 レス本文の既存検証 — `src/event/livechat.rs:11` — と同一)。制御文字は
改行を除き除去。`BoardSettings::validate` / `sanitized()`(`src/livechat/thread.rs:109-142`)
にフィールドを追加して板設定の一部として検証・配布する。表示は通常レス本文と同じ安全描画
(エスケープ + `>>n` アンカー化 + URL の自動リンクは既存挙動に合わせる)。

**Rationale**: 自動投稿された >>1 は「通常の参加者書き込みと同じ検証を満たす」(FR-016)
必要があるため、テンプレ上限をレス本文検証と同一にすれば、開設時の自動投稿が検証で落ちる
事態が構造的に起きない。ローカルルール(Markdown、`local_rules`)と異なりレス本文として
確定するので、書式もレス本文と同じプレーンテキストが一貫する。

**Alternatives considered**:
- *Markdown 安全サブセット*: >>1 はレスであり、他のレスと描画規則が異なると FR-005
  (安全描画の一貫性)の実装が二重化するため退けた。
- *独自の上限(例 4096 文字)*: レス検証を >>1 専用に緩和する抜け道になる(FR-003
  違反)ため退けた。

## R3. 固定 >>1 の自動投稿経路(採番・署名・PoW)

**Decision**: `LivechatRegistry::open_thread`(`src/livechat/registry.rs:201-239`)と
次スレ移行共通ヘルパ(`migrate_to_next_generation_locked` / `start_next_generation` —
`src/livechat/registry.rs:769-838`)の中で、スレ生成直後にホスト自身が板鍵
(`BoardKeyManager::signing_keys` — 互換 API 書き込み `src/web/compat/bbs_cgi.rs:216`
と同じ鍵取得)で `sign_res`(kind 1311)し、通常の採番経路で res_no=1 に確定・配布する。
名前欄・メール欄は空(表示は名無しデフォルト名)。初回 PoW(006 FR-017)は課さない
(ホスト自身の採番であり、`accept_write` のホスト内部呼び出しとして PoW 検査対象外とする)。
テンプレは投稿時点の板設定から読む(変更は次スレから反映 — FR-017、`res_limit` の
「次スレから反映」と同じ規則)。

**Rationale**: 「板主(スレ主)由来として整合的に採番・署名・配布」(FR-016)は、既存の
書き込み確定経路をそのまま使うのが最短で、参加者側(`src/livechat/session.rs`)は通常レス
として受理するため**変更不要**になる。順序確定・配布プロトコルに手を入れない(006 不変の
前提)。res_no=1 の占有と res_limit カウント(FR-019)も既存採番に乗るだけで一貫する。

**Alternatives considered**:
- *announce(kind 31311)にテンプレを載せ受信側が合成*: 確定レス列に >>1 が存在しない
  ままになり `>>1` アンカーの解決先(FR-014)を持てない。全端末一致(FR-012)も
  受信側合成では保証しづらい。退けた。
- *スレ主ペルソナ鍵で署名*: 通常書き込みは板鍵署名(006 FR-028)であり、>>1 だけ鍵種別が
  異なると参加者側検証に特例が要る。板鍵署名で「板主由来」は達成できる(板鍵はホストが
  管理)ため退けた。

## R4. 板設定の永続化配線

**Decision**: `board_settings` テーブル(`src/store/mod.rs:307-316,823-865` — 006 で
定義済みだが production 未配線)に `first_post_template` 列を追加し、板設定の
読み込み(起動時/板初期化時)と保存(`PUT /api/v1/livechat/threads/{board}/settings`
適用時)を production 経路に配線する。

**Rationale**: 固定 >>1 テンプレは「板設定として永続化する」(spec Assumptions)。
006 の板設定と同じ永続化方針に乗せるには、006 が用意して未配線のテーブルを使うのが最小。
再起動をまたいだ次スレ開設でもテンプレが生きる(SC-005 の 100% 自動投稿)。

**Alternatives considered**: 設定ファイル(config)への保存 — 板設定は板(ペルソナ)に
属し、Settings(プロセス設定)と寿命・スコープが異なるため退けた。

## R5. Web UI の構造(ページ構成・ルーティング・スキン)

**Decision**: `ui/livechat.html` 単一ファイル(`include_str!` 埋め込み — 
`src/web/mod.rs:402-422`)を維持し、ハッシュルーティングで**板ページ**
(`#/board/{board_id}`: スレ一覧・新規スレ作成欄・ローカルルール掲示)と**スレページ**
(`#/thread/{board_id}/{thread_key}`: レス列 + 書き込み欄)を切り替える SPA とする。
スキンは参考画像 `_alt` 相当の旧来 2ch 風: レンガ調背景は CSS(データ URI の小型パターン
または繰り返しグラデーション)で再現し、外部アセットを参照しない。レス行は
`{res_no} :{名前}:{日付} ID:{id}` ヘッダ + 本文の伝統形式。既存の管理操作(モデレーション・
板設定変更・join/leave/next/close)は `<details>` 折りたたみ等で温存する。
互換 API の板 URL 表示(現状 `http://127.0.0.1:7183/` ハードコード —
`ui/livechat.html:299-301`)は、板詳細 API に追加する `compat_bbs_port` と
`window.location.hostname` から動的生成する。

**Rationale**: 既存配信方式(ビルドパイプラインなし・単一ファイル・トークンフロー)を
変えずに表現層だけ刷新でき、006 不変の前提と整合する。ハードコード URL は LAN 公開時に
別端末で壊れるため動的化が必須(視聴端末から見たホスト名はサーバの bind 値と一致するとは
限らないので、閲覧元の hostname を使う)。

**Alternatives considered**:
- *板/スレを別 HTML ファイルに分割*: 静的ハンドラのパス追加とトークン初期化の重複を招く
  だけで利点が薄い。退けた。
- *サーバサイドレンダリング(SJIS の read.cgi 風)*: 互換 API と役割が重複し、JSON API +
  クライアント描画という 006 の構図を壊す。退けた。
- *レンガ画像アセットの同梱*: 単一 HTML 埋め込み方式を維持するため CSS/データ URI 再現を
  選択(装飾は機能でない — spec Edge Case)。

## R6. スレ読み出しレンジとアンカー UI

**Decision**: スレページは**既定で最新 50 レスを表示**し(参考画像 `_alt` の last50 に
一致)、「全部読む」操作で全件表示に切り替える。レンジ制御はクライアント側スライスで行い、
JSON API にレンジパラメータは追加しない(既存の板詳細 API が確定レス全件を返す現行契約を
維持)。`>>n` アンカーは (1) クリックでページ内ジャンプ(`id="res-{n}"`)、(2) ホバー/
タップで参照先レスのインラインポップアップ表示、を提供する。参照先が未確定・欠番
(NG 非表示含む)・範囲外の場合はジャンプ・ポップアップとも「解決不能」を明示し、誤った
レスへ誘導しない(006 の欠番維持と整合)。追従表示は既存の 8 秒ポーリング
(`ui/livechat.html:405` 相当)を維持する(FR-006 は「明示更新または定期更新」)。

**Rationale**: res_limit 上限 4000 レスでも JSON 全件(本文 ≤2048 文字 × 4000)は
LAN 内転送・メモリとも実用範囲で、API 変更なしが最小。実況の実勢(流れる会話)では
last50 が既定として自然で、参考画像とも一致する。

**Alternatives considered**: API に `?range=l50` 等を追加 — 互換 API 側に既に last50
相当の取得手段(dat)があり、JSON API の契約変更はテスト・参加者側への波及に見合わない。
将来のレス数増大時に再検討。

## R7. Host ホワイトリストと送信元検証の拡張

**Decision**: Host ヘッダのホワイトリスト(`loopback_hosts` — `src/web/mod.rs:209-215`、
compat 側 `CompatState.allowed_hosts`)を「loopback 3 形式 + 非 loopback bind 時は
`{bind_ip}:{port}` 形式」を含むよう拡張する(FR-009)。加えて防御の多層化として、
非 loopback 待受時は**送信元 IP の許可リスト検証**(loopback / RFC 1918 / リンクローカル、
`to_canonical()` 正規化後判定)をミドルウェアで行い、範囲外の送信元は 403 で拒否 +
SecurityEvent 記録する(FR-021「LAN 内アドレス限定」の実装)。per-IP レート制限は既存の
接続元ごと `RateLimiter`(`src/web/mod.rs:448-463`、compat 20 req/秒)がそのまま LAN でも
機能するため水準変更なし(FR-011)。

**Rationale**: bind 先が LAN アドレスなら到達は物理的に LAN 内へ限られるが、ルータの
ポート転送等の誤設定に対する多層防御として送信元検証は安価で効果がある。Host 検証拡張は
DNS rebinding / CSRF 対策(006 既存)を LAN 公開後も維持するための必須変更。

**Alternatives considered**: hostname(mDNS 名等)のホワイトリスト追加 — 名前解決に
依存し rebinding 面が広がる。IP リテラルのみ許可とする。

## R8. SecurityEvent と監査

**Decision**: `SecurityCategory`(`src/security/mod.rs`)に 2 カテゴリを追加する:
`WebUiLanExposed`(http_bind 非 loopback 待受成功)/ `CompatBbsLanExposed`
(compat_bbs_bind 非 loopback 待受成功)。記録は起動時 1 件、
`IndexTxtLanExposed`(`src/main.rs:437-447`)と同じパターン(`to_canonical()` で
loopback 判定)。拒否系(ホワイトリスト外 Host・範囲外送信元)は既存の拒否ログカテゴリを
流用または最小追加とする。

**Rationale**: FR-010 / SC-004(100% 記録)。面ごとにカテゴリを分けることで「どの面を
公開したか」が監査ログから判別できる。

## R9. 新 ADR(ADR-0015)

**Decision**: `docs/adr/0015-web-bbs-lan-write.md` を Phase 1 設計に基づき起草する。
内容: (a) ADR-0012 が loopback 強制のまま残した書き込み系(Web UI + JSON API・互換 API)
の LAN 公開を、許可リスト方式 + 2 要素オプトイン + 監査で許す決定(ADR-0006 決定 4 の
部分 supersede を拡張)、(b) 互換 API の無認証 LAN 書き込みの受容(補償: Host 検証・
per-IP レート制限・送信元 LAN 限定・明示オプトイン・SecurityEvent)、(c) LAN 内平文
トークン共有の受容リスク、(d) Principle V 非該当判断(新規並行アルゴリズムなし)。
参照原則: I・II。最終化は実装フェーズ(006 の ADR-0014 と同じ運用)。

**Rationale**: spec Assumptions が新 ADR の必要を明記。憲法 Principle II
「セキュリティ上の設計決定は ADR に記録 (MUST)」。

## R10. 変更しないもの(確認)

- P2P プロトコル・順序確定(`21311`)・鍵体系・NG/BAN・PoW パラメータ・レート制限水準:
  006 のまま(spec Assumptions)。参加者側 `src/livechat/session.rs` は変更不要(R3)。
- 互換 API の**専ブラ向け**エンドポイント・SJIS 応答・bbs.cgi の検証(`src/web/compat/`):
  変更なし。LAN 対応は bind・Host 検証・送信元検証の層のみ。ただしブラウザ向けの板ルート
  URL(`.../{board}/`)への HTML 応答は R11 で新規追加する(専ブラ向けパスとは非干渉)。
- index.txt の LAN 公開(ADR-0012): 現行のまま。007 は同方式を書き込み面へ拡張するが
  `index_bind` の挙動には触れない。

## R11. 互換 API 面での板ルート URL のブラウザ向け配信(FR-023)

**Decision**: 互換 API 待受面(`compat_bbs_bind`、`src/web/compat/`)が、専ブラ向けパス
(`.../{board}/subject.txt` / `dat/{key}.dat` / `bbs.cgi` 相当)に加えて、**ファイル名を
伴わない板ルート URL `.../{board}/`**(末尾スラッシュのみ)への GET に対し、旧来 BBS UI の
板ページ(HTML)を**同一ポート・同一パス上で自ら**配信する。振り分けは**パス**で行い、
専ブラ向けレスポンスは従来どおり Shift_JIS、ブラウザ向け HTML は Web UI(`http_bind`)と
同じ表現層・UTF-8 とする。他面(`http_bind` 等)への HTTP リダイレクトでは代替しない。
配信する HTML は既存 Web UI と同一の `include_str!` 資産を用い、`{board}` を初期表示板と
して開くよう板ルーティングを解決する(互換ポート上でも板ページ・スレページが自己完結する)。

**Rationale**:
- 2ch の実勢(専ブラは `subject.txt` を読み、ブラウザは `.../{board}/` で板を開く)に一致
  させる(Clarifications 2026-08-03)。専ブラに板 URL を登録する導線と、ブラウザで開いて
  読める導線が**同一 URL 上で両立**する。
- リダイレクト案は「公開面ごとに独立オプトイン」(R1)と衝突する。互換 API だけを LAN
  公開し `http_bind` を loopback のままにした構成では、`http_bind` への 302 は別端末から
  到達できず壊れる。同一ポートで自己完結させれば公開面の独立性を保てる(FR-023 の
  リダイレクト禁止 — MUST NOT の根拠)。
- パス振り分けは既存ルータの分岐追加で足り、専ブラ向け応答(SJIS)には一切触れないため
  互換性リスクがない。

**Alternatives considered**:
- *`.../{board}/` → `http_bind` の Web UI へ 302 リダイレクト*: 上記のとおり公開面独立性を
  壊し、一貫性(同一 URL で完結)も損なうため退けた(FR-023 で明示的に禁止)。
- *板ルート URL のブラウザアクセスをエラー/専ブラ登録専用の説明ページに留める*: 実機で
  「ブラウザで開けない」と判明した実勢不一致そのものであり、Clarifications の決定に反する。
- *Accept ヘッダによる content negotiation(同一 `.../{board}/subject.txt` を HTML/SJIS
  切替)*: 専ブラの UA/Accept は多様で誤判定リスクがある。パス(ファイル名の有無)での
  分岐が確実で、既存の専ブラパスを一切変えない。

## R12. Web UI の管理導線再編・ナビゲーション・新規スレ >>1 上書き(FR-002a/006a/006b/014a)

**Decision**: `ui/livechat.html`(R5 の SPA)を次のとおり調整する。いずれも表現層のみの
変更で、書き込み・検証経路(006)は不変。

1. **ナビゲーションリンク(FR-002a)**: 板選択ビュー(`#/`)の各板項目は `#/board/{board_id}`
   へ、板ページのスレ一覧の各項目は `#/thread/{board_id}/{thread_key}` へ、クリックで遷移
   できるリンクとする(遷移手段のない静的表示にしない)。
2. **板設定の板ページ移設(FR-006a)**: 板設定編集(タイトル・名無し名・ローカルルール・
   固定 >>1 テンプレ)を**板ページ**の管理セクションへ移し、スレの有無・スレ開設操作から
   独立させる(スレ 0 件の板でも編集可能)。従来スレページの `<details>` に同居していた
   板設定編集はスレページから外す。
3. **モデレーション面の分離(FR-006b)**: 個別レスへの NG/BAN 実行は対象を見ながら操作
   できるよう**スレページ**起点を維持する。一方 **BAN 済み一覧の参照・変更(解除等)** は
   板ページの板設定セクションから到達・実行できるようにする(BAN 台帳はスレ非依存)。
4. **新規スレ >>1 のプリフィル・上書き(FR-014a)**: 新規スレ作成欄に >>1 本文入力欄を設け、
   板設定の固定 >>1 テンプレ(未設定時はシステム既定テンプレ)を既定値としてプリフィル
   表示する。板主は開設時に任意で上書きでき、未編集なら既定値がそのまま >>1 となる。
   上書き値は**そのスレの >>1 のみ**に反映し、板設定の永続テンプレ値は変更しない。

**Rationale**:
- 板と板設定は 006 でチャンネル単位に存在し、スレはその配下の揮発物(data-model.md)。
  管理面をスレのライフサイクルから切り離すのが実勢・体裁の双方に合致する(実機検証で
  「スレを立てないと設定変更できない」不具合が判明 — Clarifications 2026-08-03)。
- 一覧が遷移リンクを持たないと「掲示板そのもの」(SC-001)として成立しない。
- >>1 のプリフィル + 任意上書きは、固定テンプレの一貫性(空 >>1 禁止・自動確定 — FR-014)を
  壊さず、開設時の柔軟性(そのスレ限りの案内)を与える。永続値を変えないことで FR-017
  (テンプレ変更の遡及なし)とも干渉しない。

**Alternatives considered**:
- *板設定をスレページに残置*: 実機で判明した不具合そのもの。退けた。
- *新規スレ >>1 をその場入力主体にしフォールバックで固定テンプレ*: Clarifications で
  「固定テンプレをプリフィル・任意上書き」(Option B)に確定。既定値の可視性が高い B を採る。
- *BAN 一覧もスレページのみ*: スレ非依存で参照したい需要(Clarifications 補足)に反する。

## R13. 互換ポート上での板ページ SPA の自己完結(FR-023a/b/c — T050)

**Decision**: FR-023 でブラウザ向け板ページ HTML は互換ポートで配信できるが、SPA のデータ経路
(`/api/v1/token`・板詳細・書込)は**互換リスナーが構造的に持たない**(`src/web/compat/` は
「`/api/v1` のルートを物理的に持たない」= トークン保護 API がこちら側へ露出する故障モードを
排除する不変条件)。この自己完結を、**互換名前空間の JSON エンドポイント**で成立させる。
`/api/v1` は互換面へ一切生やさない。

1. **読取 `GET /{board}/board.json`(FR-023a)**: 視聴者向け最小ビュー `CompatBoardView`
   (板タイトル・名無し名・レス上限・安全 HTML 化済みローカルルール・確定レス列・現行スレ
   記述子 `{generation, res_count}`・互換ポート番号)。板主設定(固定 >>1 テンプレ・PoW
   ビット)と送信中投稿 `pending` は**含めない**。データ源は `resolve_snapshot`(registry
   優先・無ければ常駐セッション)で subject.txt/dat と同一。
2. **板一覧 `GET /boards.json`(FR-023c)**: ホスト板 + 参加中板を最小フィールド
   (board_id・title・res_count・is_local)で列挙。tip/channel/内部状態は出さない。
3. **書込 `POST /{board}/write.json`(FR-023a)**: トークンレス・board スコープ
   (`{name,mail,body}`・key 省略でホストが現行スレへ採番)。実体は既存 `bbs_cgi::submit`
   (自板採番 → 未知板は `manager.write` フォールバック)と**同一**で、bbs.cgi の検証・PoW・
   レート・非開示(FR-022)を 1 バイトも変えない。応答は 202(受理)/ 400(形式違反)。
4. **視聴者スコープ(FR-023b)**: SPA は配信オリジン(`/{board}/` パス配信=互換)を起動時に
   検出し、互換オリジンでは読取/一覧/書込を上記 JSON に振り替え、ホスト管理(開設・板設定・
   モデレーション・BAN 一覧・チャンネル選択・token 取得・disabled-banner probe)を非表示化する。

**Rationale**:
- 候補 (a)「互換面へ閲覧専用 `/api/v1` を限定公開」は「`/api/v1` を物理的に持たない」不変条件を
  **字義的に破壊**し、将来の一行の経路ミスで保護系が生える土台を作る。互換名前空間 JSON なら
  不変条件を字義でも精神でも保てる(保護 API 面を生やさない)。(b) は (a) を厳密に支配する。
- 書込を bbs.cgi の submit に一本化することで、専ブラ経路と互換ブラウザ経路が**同一の検証・
  採番・非開示**を共有し、検証の二重実装によるドリフトを避ける(key 省略の board スコープは
  既存 `route_write` 自板分岐と同一。未知板の常駐セッション書込も bbs.cgi が既に持つ挙動)。
- board.json は「公開済み SJIS 読取(subject/dat/SETTING/head)の JSON 再エンコード」に留め、
  新規のデータ種別・板鍵・秘密・トークン面を増やさない。pending(セッション概念)と板主設定を
  除外することで、トークンレス・別端末面へ余計な情報を晒さない(最小露出)。
- 実況の UX(視聴者が別端末ブラウザで板を辿って書く)を成立させるため boards.json は参加中板も
  列挙する。これは自ノードの視聴活動を LAN のトークンレス面へ露出させるが、`compat_bbs_lan_consent`
  (公開して視聴・書込を募る)の意図の範囲として**明示的に受容**する(ADR-0015 追補に記録)。

**Alternatives considered**:
- *(a) 互換面へ閲覧専用 `/api/v1`*: 不変条件を破壊。退けた。
- *dat/subject.txt を JS で SJIS パースして読取に流用*: 新規エンドポイント 0 だが SPA が SJIS
  デコード + dat 意味論(名無し解決・ID 生成・アンカー)をクライアントで二重実装することになり
  ドリフト源。JSON 再エンコードの方が SPA 改修最小(URL ビルダ分岐のみ)。退けた。
- *SPA が生 bbs.cgi を叩く(SJIS フォーム送信・HTML 応答)*: 成功/失敗判定が SJIS HTML の文字列
  マッチ頼みになる。薄い JSON 書込口(内部は同一 submit)の方が uniform で堅い。退けた。
- *boards.json をホスト板のみに限定*: 参加中板へ辿れず実況 UX(視聴者ブラウズ)が片肺。露出を
  受容して両方列挙する判断(Clarifications 2026-08-03 追補)。
- *互換面を読み取り専用(書込なし)*: 007 の主目的「視聴端末から書き込む」を互換ブラウザ面で
  満たせない。書込込みとする。

# Contract: index.txt 供給(既存 YP ブラウザ互換)

**Role**: ユーザー所有の YP ブラウザが従来 YP と同様に一覧を取得できるようにする。
**plain HTTP(https ではない)** で供給する(ユーザー要求)。既定バインドは loopback のみ。

**plain HTTP のリスク受容**: 既定バインドが loopback のみであるため、改ざん・盗聴は
同一ホスト内に限られる — この前提込みでリスクを受容する。LAN 公開時は利用者の
オプトイン+警告(contracts/local-api.md §保護方針)による受容とする。
この判断は非暗号化判断 ADR(plan Constitution Check の ADR ⑤)に含めて記録する。

## エンドポイント

```text
GET http://127.0.0.1:7180/index.txt
```

- **応答**: `200 OK`、`Content-Type: text/plain`(charset は設定に応じ `Shift_JIS` / `UTF-8`)
- **本文**(2026-07-04 実運用 YP の index.txt 実物と突合して改訂): 1 チャンネル 1 行、
  **19 フィールド**(区切り `<>` は 18 個)を出力(改行 LF)

```text
CHANNEL_NAME<>ID<>TIP<>CONTACT_URL<>GENRE<>DETAIL<>LISTENER_NUM<>RELAY_NUM
<>BITRATE<>TYPE<>TRACK_ARTIST<>TRACK_ALBUM<>TRACK_TITLE<>TRACK_CONTACT_URL
<>NAME_ENCODED<>BROADCAST_TIME<>click<>COMMENT<>DIRECT
```
(実際は 1 行。上記は紙面上の折返し)

改訂前は 15・17 番目を「予約(常に空)」、全 18 フィールドとしていたが、実物では
15 番目 = **NAME_ENCODED**(チャンネル名の percent エンコード)、17 番目 = 固定文字列
**`click`**、末尾に 19 番目 **DIRECT**(`0`/`1`)が存在する。18 フィールド出力は
既存 YP ブラウザに解釈されない(実機確認)。

**仕様出典**: [kumaryu/peercaststation wiki「index.txtの仕様」](https://github.com/kumaryu/peercaststation/wiki/index.txt%E3%81%AE%E4%BB%95%E6%A7%98)
(GPL 対象文書ではない参考仕様として利用 — 利用者確認済み 2026-07-04)。
実運用 YP の実物 index.txt との突合結果とも一致。wiki 記載の補足仕様のうち v1 の扱い:
制限チャンネル(ID 全 0・TIP `127.0.0.1`)は掲載制限機能がないため出力しない。
リスナー数の負値は YP4G 解釈(-1 = 非表示、それ未満 = サーバーメッセージ)があるため
`-1`(不明)以外の負値は出力しない。

| フィールド | 由来(DiscoveredChannel) | 規則 |
|-----------|--------------------------|------|
| CHANNEL_NAME | name | テキストサニタイズ(下記) |
| ID | channel_id(hex 32 桁**大文字**) | 内部・イベント(30311 `d` タグ)は常に**小文字**で保持し、index.txt 出力時のみ大文字化する(変換規則) |
| TIP | tracker `host:port` | firewalled は空文字列 |
| CONTACT_URL | contact_url | URL 警告対象(FR-012)は UI 側で警告。index.txt はそのまま(下記「FR-012 の適用範囲」参照) |
| GENRE / DETAIL | genre / description | 欠損は空文字列 |
| LISTENER_NUM / RELAY_NUM | listeners / relays_cnt | 不明(30311 のタグ省略)は `-1` |
| BITRATE / TYPE | bitrate_kbps / content_type | BITRATE 不明は `0`、TYPE 欠損は空文字列 |
| TRACK_* | track | 欠損は空文字列 |
| NAME_ENCODED | name(生値) | percent エンコード(出力エンコーディングのバイト列基準・大文字 hex。UTF-8 なら `%E3%83…`、Shift_JIS なら古典形 `%83e%83X…`) |
| BROADCAST_TIME | now - started_at を `HH:MM` | H は 2 桁以上可 — 24 時間超は時間部をそのまま拡張する(例 25 時間 30 分 → `25:30`)。分は 2 桁固定(`0` 埋め)。既存 YP ブラウザの解釈揺れは実機確認(research R5 と同枠)で検証する |
| (17 番目) | — | 固定文字列 `click`(実物準拠) |
| COMMENT | 空(v1) | |
| DIRECT | — | 固定 `0`(直接再生の提供なし) |

- **空値・欠損の共通規則**: 文字列フィールドは空文字列を出力する(行のフィールド数は常に固定)
- **サニタイズ**(2026-07-04 wiki 仕様に合わせて改訂): 全文字列フィールド共通で、
  値中の `<`/`>` を `&lt;`/`&gt;` へエスケープする(wiki「項目内で `<` または `>` が
  使われる場合、`&lt;` とか `&gt;` と記される」)。仕様上エスケープ対象は `<`/`>` のみで
  **`&` はエスケープしない**。CONTACT_URL も同一規則(wiki: 「URLとは限らない。任意の
  文字列を指定できる」)。エスケープ後は値中に `<>` 区切り列が現れないため区切り解析は
  破壊されない。エンコーディング変換の変換不能文字は `?` に置換(`?` は区切り文字と
  衝突しない — ゴールデンテストに両ケースを含める)
- **FR-012 の適用範囲**: CONTACT_URL は index.txt では無検査の生値で出力する。URL 警告
  (FR-012)の適用範囲は本ソフトウェアの UI に限られ、外部 YP ブラウザの利用者には及ばない。
  これは既存 YP ブラウザ互換(FR-004 — 従来 YP と同一の生値供給)を優先した意図的な設計判断
  であり、外部 YP ブラウザ利用者の保護は当該ブラウザの責務とする
- 出力対象: `status=live` かつ鮮度窓内、かつミュートされていないチャンネルのみ(FR-006, FR-008)
- **表示鮮度の許容範囲**: P2P 伝搬遅延(掲載直後は最大 60 秒 — SC-001)と YP ブラウザ側の
  取得周期により、掲載直後の未表示・配信終了直後の残存(`status=ended` 伝搬まで最大 60 秒
  + 取得周期分)が生じうる。`ended` イベントが欠落した場合でも鮮度窓(600 秒)で除去される。
  これらの遅延は従来 YP の更新周期と同等であり許容範囲とする
- 並び順: 掲載更新の新しい順
- エンコーディング: **既定 UTF-8**(2026-07-04 実機検証で改訂 — research R5 は Shift_JIS
  既定としていたが、現行 YP(SP/TP)は UTF-8 であり実 YP ブラウザも UTF-8 を既定解釈する
  ことを実機確認した。`index_txt_encoding` 設定で shift_jis へ変更可)。変換不能文字は `?` 置換

## 入力検証・保護(Principle II)

- 受け付けるのは `GET` / `HEAD` のみ。他メソッドは 405
- リクエストヘッダ合計 ≤ 8KB、URL 長 ≤ 1KB。超過は 400
- レート制限: 同一接続元あたり 10 req/秒(超過は 429 + `http_rate_limited` ログ)
- エラー本文は定型文のみ(内部情報の漏洩禁止)

## 検証方法

- `tests/contract/`: 既知の DiscoveredChannel 集合 → index.txt ゴールデンファイル比較
  (Shift_JIS / UTF-8 両方、空一覧、firewalled、`<>` 含む名称のサニタイズ)
- 受け入れ: ユーザー所有の実 YP ブラウザで表示確認(research R5 のリスク解消)

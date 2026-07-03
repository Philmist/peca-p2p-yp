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
- **本文**: 1 チャンネル 1 行、**18 フィールド**(区切り `<>` は 17 個)を出力(改行 LF)。
  内訳: 名前付き 16(CHANNEL_NAME〜TRACK_CONTACT_URL の 14 + BROADCAST_TIME + COMMENT)+ 予約 2(15・17 番目)

```text
CHANNEL_NAME<>ID<>TIP<>CONTACT_URL<>GENRE<>DETAIL<>LISTENER_NUM<>RELAY_NUM
<>BITRATE<>TYPE<>TRACK_ARTIST<>TRACK_ALBUM<>TRACK_TITLE<>TRACK_CONTACT_URL
<><>BROADCAST_TIME<><>COMMENT
```
(実際は 1 行。上記は紙面上の折返し)

| フィールド | 由来(DiscoveredChannel) | 規則 |
|-----------|--------------------------|------|
| CHANNEL_NAME | name | フィールド内の `<>` は除去 |
| ID | channel_id(hex 32 桁**大文字**) | 内部・イベント(30311 `d` タグ)は常に**小文字**で保持し、index.txt 出力時のみ大文字化する(変換規則) |
| TIP | tracker `host:port` | firewalled は空文字列 |
| CONTACT_URL | contact_url | URL 警告対象(FR-012)は UI 側で警告。index.txt はそのまま(下記「FR-012 の適用範囲」参照) |
| GENRE / DETAIL | genre / description | 欠損は空文字列 |
| LISTENER_NUM / RELAY_NUM | listeners / relays_cnt | 不明(30311 のタグ省略)は `-1` |
| BITRATE / TYPE | bitrate_kbps / content_type | BITRATE 不明は `0`、TYPE 欠損は空文字列 |
| TRACK_* | track | 欠損は空文字列 |
| BROADCAST_TIME | now - started_at を `HH:MM` | H は 2 桁以上可 — 24 時間超は時間部をそのまま拡張する(例 25 時間 30 分 → `25:30`)。分は 2 桁固定(`0` 埋め)。既存 YP ブラウザの解釈揺れは実機確認(research R5 と同枠)で検証する |
| COMMENT | 空(v1) | |

- **空値・欠損の共通規則**: 文字列フィールドは空文字列を出力する(行のフィールド数は常に固定)。
  フォーマット中の 15・17 番目の空フィールドは既存 index.txt フォーマットとの位置互換のための
  予約フィールドであり、常に空を出力する
- **サニタイズ順序**: (1) フィールド値から区切り列 `<>` を除去 → (2) エンコーディング変換で
  変換不能文字を `?` に置換。`?` は区切り文字と衝突しないため、この順序で `<>` 区切りの解析は
  破壊されない(ゴールデンテストに両ケースを含める — 検証方法)
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
- エンコーディング: 既定 Shift_JIS(research R5)。変換不能文字は `?` 置換

## 入力検証・保護(Principle II)

- 受け付けるのは `GET` / `HEAD` のみ。他メソッドは 405
- リクエストヘッダ合計 ≤ 8KB、URL 長 ≤ 1KB。超過は 400
- レート制限: 同一接続元あたり 10 req/秒(超過は 429 + `http_rate_limited` ログ)
- エラー本文は定型文のみ(内部情報の漏洩禁止)

## 検証方法

- `tests/contract/`: 既知の DiscoveredChannel 集合 → index.txt ゴールデンファイル比較
  (Shift_JIS / UTF-8 両方、空一覧、firewalled、`<>` 含む名称のサニタイズ)
- 受け入れ: ユーザー所有の実 YP ブラウザで表示確認(research R5 のリスク解消)

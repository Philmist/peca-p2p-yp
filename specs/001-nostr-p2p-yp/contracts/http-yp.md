# Contract: index.txt 供給(既存 YP ブラウザ互換)

**Role**: ユーザー所有の YP ブラウザが従来 YP と同様に一覧を取得できるようにする。
**plain HTTP(https ではない)** で供給する(ユーザー要求)。既定バインドは loopback のみ。

## エンドポイント

```text
GET http://127.0.0.1:7180/index.txt
```

- **応答**: `200 OK`、`Content-Type: text/plain`(charset は設定に応じ `Shift_JIS` / `UTF-8`)
- **本文**: 1 チャンネル 1 行、17 フィールドを `<>` 区切りで出力(改行 LF)

```text
CHANNEL_NAME<>ID<>TIP<>CONTACT_URL<>GENRE<>DETAIL<>LISTENER_NUM<>RELAY_NUM
<>BITRATE<>TYPE<>TRACK_ARTIST<>TRACK_ALBUM<>TRACK_TITLE<>TRACK_CONTACT_URL
<><>BROADCAST_TIME<><>COMMENT
```
(実際は 1 行。上記は紙面上の折返し)

| フィールド | 由来(DiscoveredChannel) | 規則 |
|-----------|--------------------------|------|
| CHANNEL_NAME | name | フィールド内の `<>` は除去 |
| ID | channel_id(hex 32 桁大文字) | |
| TIP | tracker `host:port` | firewalled は空文字列 |
| CONTACT_URL | contact_url | URL 警告対象(FR-012)は UI 側で警告。index.txt はそのまま |
| GENRE / DETAIL | genre / description | |
| LISTENER_NUM / RELAY_NUM | listeners / relays_cnt | 不明は `-1` |
| BITRATE / TYPE | bitrate_kbps / content_type | |
| TRACK_* | track | |
| BROADCAST_TIME | now - started_at を `HH:MM`(H は 2 桁以上可) | |
| COMMENT | 空(v1) | |

- 出力対象: `status=live` かつ鮮度窓内、かつミュートされていないチャンネルのみ(FR-006, FR-008)
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

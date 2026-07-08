# Contract: index.txt LAN 公開リスナー・設定・状態表示

**Date**: 2026-07-08 | **Spec**: [../spec.md](../spec.md) | **Data model**: [../data-model.md](../data-model.md)

本契約は既存契約の**差分**である。index.txt の内容仕様(19 フィールド・サニタイズ・
エンコーディング)は `specs/001-nostr-p2p-yp/contracts/http-yp.md` を正とし、一切変更しない。
`/api/v1` の保護方針は `specs/001-nostr-p2p-yp/contracts/local-api.md` を正とする。

## 1. 専用リスナー(`index_bind` 非空時のみ存在)

### 1.1 提供ルート

| メソッド + パス | 応答 |
|-----------------|------|
| `GET /index.txt` | 200。loopback 側と同一の生成ロジック・同一内容・同一 `Content-Type`(`text/plain; charset=UTF-8` または `Shift_JIS` — `index_txt_encoding` 共有) |
| `HEAD /index.txt` | 200。ボディなし、`Content-Type` は GET と同一 |
| `/index.txt` への GET/HEAD 以外 | 405(axum 定型。空ボディ + `Allow` ヘッダ。内部情報なし) |
| **上記以外のすべてのパス** | 404 `{"error":"not_found"}`(定型 JSON。`/api/v1/...`・`/`・静的アセットパスを含む — API/UI ルートは物理的に存在しない) |

### 1.2 保護(loopback 側 index.txt と同一 — 緩和しない)

| 保護 | 値 | 超過時 |
|------|----|--------|
| URL 長 | ≤ 1KB | 400 `{"error":"request_too_large"}` |
| ヘッダ合計 | ≤ 8KB | 400 `{"error":"request_too_large"}` |
| レート制限 | 同一接続元 10 req/秒(`index_txt_rate_limiter` を loopback 側と共有) | 429 `{"error":"rate_limited"}` + SecurityEvent `http_rate_limited` |
| Host 検証 | **適用しない**(認証・状態変更なしの公開一覧。DNS rebinding の標的でない — research R2) | — |
| トークン | 不要(読み取り専用) | — |

### 1.3 ライフサイクル

- 起動時に 1 回だけ bind。**失敗しても本体は継続稼働**(警告ログ + status 反映のみ。
  終了コードに影響しない)
- graceful shutdown は既存の shutdown 伝播(watch チャネル)に従う
- 実行中の `index_bind` 変更は反映されない(再起動要求)

## 2. 設定契約の差分(local-api.md §settings への追記)

### 2.1 `GET /api/v1/settings`

応答に `index_bind`(string、既定 `""`)が加わる(13→14 キー)。

### 2.2 `PUT /api/v1/settings`

| 入力 `index_bind` | 結果 |
|-------------------|------|
| `""` | 200(機能無効化。バインド系変更なら `restart_keys` に含む) |
| loopback / RFC1918 / リンクローカル / ULA の `addr:port`(単一) | 200 + `restart_required: true`, `restart_keys: ["index_bind"]` |
| unspecified / グローバル / CGNAT(100.64/10) | 400 `{"error":"non_lan_bind"}` |
| 書式不正(ポート欠落・カンマ区切り複数・非アドレス) | 400 `{"error":"invalid_bind"}`(既存写像に従う) |

判定の全数表は [data-model.md §2](../data-model.md) を正とする。

### 2.3 CLI

`--index-bind <addr:port>` / `--index-bind=<addr:port>` を受理(検証は §2.2 と同一
規則で `Settings::validate()` にて実施)。不正値は既存どおり設定エラーで起動拒否
(バインド失敗の縮退とは区別 — 検証エラーは fail-fast)。

## 3. `GET /api/v1/status` の差分

応答オブジェクトに追加:

```json
{
  "index_txt_lan": {
    "enabled": true,
    "bind": "192.168.1.10:7180",
    "listening": true,
    "error": null
  }
}
```

| 状態 | `enabled` | `bind` | `listening` | `error` |
|------|-----------|--------|-------------|---------|
| 無効(`index_bind` 空) | `false` | `null` | `false` | `null` |
| 露出中 | `true` | 設定値 | `true` | `null` |
| 設定有効だが bind 失敗 | `true` | 設定値 | `false` | `"addr_in_use"` \| `"permission_denied"` \| `"addr_not_available"` \| `"unknown"` |

## 4. セキュリティイベント契約

| category | 契機 | source | detail |
|----------|------|--------|--------|
| `index_txt_lan_exposed` | 起動時、`index_bind` 非 loopback かつ bind 成功で 1 件 | バインドアドレス | 定型文言(例: `index.txt is exposed to LAN`)。内部情報なし |

loopback 値・bind 失敗・機能無効では記録しない。

## 5. UI 契約(ui/settings.html)

- `index_bind` を設定フォームに表示・編集可(バインド系 = 再起動要求の注記対象)
- 保存時、`index_bind` が非空かつ非 loopback の場合:
  「掲載一覧(index.txt)が LAN 内で平文・無認証のまま取得・改ざんされうる」旨の
  警告 1 項目を表示し、明示確認(チェックボックス)なしには PUT を送信しない
- 警告文言は**要旨拘束**(spec FR-005 の「〜旨」に従う)であり、上記は参考文。
  表示文・チェックボックスラベルで語尾等を変えてよいが、「平文」「無認証」
  「取得・改ざんされうる」の 3 要素を欠いてはならない (MUST)
- トークン盗聴警告(local-api.md §保護方針の警告 (2))は**表示しない**
  (index.txt 経路はトークン不要 — ADR-0012 の対応表参照)

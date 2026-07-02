# Contract: ローカル管理 UI / JSON API

**Role**: 中心の UI 層。ユーザーの Web ブラウザから `http://127.0.0.1:7180/` を開いて操作する。
静的アセット(HTML/CSS/JS)はバイナリ埋め込みで配信する(自己完結制約)。

## 保護方針(Principle II / 最小権限)

- 既定バインドは loopback のみ。LAN 公開はオプトイン+UI 警告
- 変更系(POST/PUT/DELETE)は起動時生成のセッショントークンを `X-Api-Token` ヘッダで要求
  (トークンは UI 初回ロード時に同一オリジンで受け渡し。DNS rebinding / CSRF 対策として
  `Host` ヘッダ検証も行う)
- JSON ボディ ≤ 64KB。超過は 413
- エラー応答は `{"error":"<定型コード>"}` のみ(内部情報禁止)

## エンドポイント(`/api/v1`)

| Method & Path | 説明 | 主な入出力 |
|---------------|------|-----------|
| `GET /channels` | 発見済みチャンネル一覧(視聴者向け) | DiscoveredChannel の配列(muted 除外、`url_warning` フラグ付き — FR-012) |
| `GET /announced` | 自分が掲載中のチャンネルと掲載状態 | AnnouncedChannel + 掲載成功リレー数 |
| `GET /personas` | ペルソナ一覧(pubkey, label, state) | 秘密鍵は返さない |
| `POST /personas` | ペルソナ新規作成 | `{label}` → `{pubkey}` |
| `PUT /personas/{pubkey}` | label 変更 / archive / チャンネルへの割当 | |
| `DELETE /personas/{pubkey}` | ペルソナ破棄(確認フラグ必須、復元不可) | |
| `POST /personas/{pubkey}/export` | nsec 表示(明示操作+警告 — research R6) | |
| `GET /relays` | リレー一覧+健全性(last_ok_at) | |
| `POST /relays` | 追加。**貼り付け一括登録**(`{urls:["wss://…",…]}`)対応(research R10) | 不正 URL は個別にエラー返却 |
| `PUT /relays/{id}` | enabled/read/write 変更 | |
| `DELETE /relays/{id}` | 削除 | |
| `GET /relays/export` | 共有用テキスト(1 行 1 URL)書き出し(research R10) | |
| `GET /mutes` / `POST /mutes` / `DELETE /mutes/{id}` | ミュート管理(pubkey / channel 単位 — FR-008) | |
| `GET /settings` / `PUT /settings` | data-model.md の Settings キー | バインド変更は再起動要求を返す |
| `GET /status` | 全体状態(PCP 待受、リレー接続数、全リレー到達不能フラグ) | US3 シナリオ 3 の通知に使用 |

## UI 要件(契約レベル)

- チャンネル一覧: 表示列は index.txt 相当+掲載ペルソナ(pubkey 短縮表示)。
  ミュート操作・コンタクト URL の警告表示(FR-012)・「未検証リンクを開く前の確認」を含む
- ペルソナ切替: チャンネル掲載に使うペルソナを配信ごとに選択できる(FR-013)。
  現在選択中のペルソナを常時明示し、意図しないペルソナでの掲載(誤爆)を防ぐ
- 全リレー到達不能時: 目立つバナーで通知し、回復後自動で掲載再開を表示(US3-3)

## 検証方法

- `tests/contract/`: 各エンドポイントのスキーマ検証(正常系+トークン欠落 401+過大ボディ 413)
- cucumber: UI 操作相当のシナリオは API 経由で検証(ブラウザ E2E は v1 では手動)

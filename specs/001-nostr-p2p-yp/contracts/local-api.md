# Contract: ローカル管理 UI / JSON API

**Role**: 中心の UI 層。ユーザーの Web ブラウザから `http://127.0.0.1:7180/` を開いて操作する。
静的アセット(HTML/CSS/JS)はバイナリ埋め込みで配信する(自己完結制約)。

## 保護方針(Principle II / 最小権限)

- 既定バインドは loopback のみ。LAN 公開はオプトイン+UI 警告
- **LAN 公開オプトインの要件**: 有効化時、UI は (1) 攻撃面が LAN 全体へ拡大すること、
  (2) 通信が平文 HTTP であるため `X-Api-Token` を含む全トラフィックが LAN 内で盗聴可能に
  なること、を明示した警告を表示し、明示的な確認操作を経なければならない (MUST)。
  公開後も `X-Api-Token`・`Host` ヘッダ検証・レート制限は引き続き適用される
  (ただし平文経路上のトークン保護にはならないことを警告に含める)。
  PCP の LAN 公開オプトインも同一の警告方式に従う(contracts/pcp-announce.md)
- 変更系(POST/PUT/DELETE)は起動時生成のセッショントークンを `X-Api-Token` ヘッダで要求
  (トークンは UI 初回ロード時に同一オリジンで受け渡し。DNS rebinding / CSRF 対策として
  `Host` ヘッダ検証も行う)
- JSON ボディ ≤ 64KB。超過は 413
- レート制限: 同一接続元あたり `/api/v1` 全体で 20 req/秒(超過は 429 + `http_rate_limited` ログ)
- エラー応答は `{"error":"<定型コード>"}` のみ(内部情報禁止)

## エンドポイント(`/api/v1`)

| Method & Path | 説明 | 主な入出力 |
|---------------|------|-----------|
| `GET /channels` | 発見済みチャンネル一覧(視聴者向け) | DiscoveredChannel の配列(muted 除外、`url_warning` フラグ付き — FR-012) |
| `GET /announced` | 自分が掲載中のチャンネルと掲載状態 | AnnouncedChannel + 伝搬先(established ピア)数 |
| `GET /personas` | ペルソナ一覧(pubkey, label, state) | 秘密鍵は返さない |
| `POST /personas` | ペルソナ新規作成 | `{label}` → `{pubkey}` |
| `PUT /personas/{pubkey}` | label 変更 / archive / チャンネルへの割当 | |
| `DELETE /personas/{pubkey}` | ペルソナ破棄(確認フラグ必須、復元不可) | |
| `POST /personas/{pubkey}/export` | nsec 表示(明示操作+警告 — research R6)。受け入れ基準: (1) 要求ボディに確認フラグ(`{"confirm":true}`)必須 — 欠落は 400、(2) 応答前に UI は「秘密鍵を知る者はこのペルソナとして掲載できる」「破棄後は復元できず、これが唯一のバックアップ手段である」旨の警告と明示確認を経る、(3) nsec は応答本文でのみ返し、ログ・セキュリティイベントに記録してはならない (MUST NOT) | |
| `GET /peers` | ピア一覧+健全性(source, verified, enabled, last_ok_at, fail_count, 接続中か) | |
| `POST /peers` | 追加。**貼り付け一括登録**(`{addrs:["host:port",…]}`)対応(research R10) | 不正アドレスは個別にエラー返却。source=manual で登録 |
| `PUT /peers/{id}` | enabled 変更 | |
| `DELETE /peers/{id}` | 削除 | |
| `GET /peers/export` | 共有用テキスト(1 行 1 アドレス。verified のみ)書き出し(research R10) | |
| `GET /mutes` / `POST /mutes` / `DELETE /mutes/{id}` | ミュート管理(pubkey / channel 単位 — FR-008) | |
| `GET /settings` / `PUT /settings` | data-model.md の Settings キー | バインド変更は再起動要求を返す |
| `GET /status` | 全体状態(PCP 待受、established ピア数 in/out、着信可否(UPnP 結果 — FR-016)、全ピア到達不能フラグ) | US3 シナリオ 3 の通知に使用 |

## UI 要件(契約レベル)

- チャンネル一覧: 表示列は index.txt 相当+掲載ペルソナ(pubkey 短縮表示)。
  ミュート操作・コンタクト URL の警告表示(FR-012)・「未検証リンクを開く前の確認」を含む。
  TIP が空(firewalled)のチャンネルは「直接視聴不可(トラッカー未公開)」であることを
  一覧上で明示する(spec Edge Case。v1 は Tracker Lookup 非対応 — contracts/pcp-announce.md)
- ペルソナ切替: チャンネル掲載に使うペルソナを配信ごとに選択できる(FR-013)。
  現在選択中のペルソナを常時明示し、意図しないペルソナでの掲載(誤爆)を防ぐ
- 全ピア到達不能時: 目立つバナーで通知し、回復後自動で掲載再開を表示(US3-3)
- 着信不可(UPnP 失敗・待受無効)時: 状態表示で「外向き接続のみで参加中」であることを示す
  (エラーではない — FR-016)
- 設定画面: data-model §Settings のキーを閲覧・変更できる(`GET/PUT /settings`)。
  バインド系キー(`pcp_bind` / `http_bind` / `p2p_bind`)の変更時は再起動が必要である旨を表示する。
  LAN 公開オプトインを v1 で実装する場合(非暗号化判断 ADR ⑤で確定)は
  §保護方針の警告 2 項目を本画面に含める (MUST)

## 検証方法

- `tests/contract/`: 各エンドポイントのスキーマ検証(正常系+トークン欠落 401+過大ボディ 413)
- cucumber: UI 操作相当のシナリオは API 経由で検証(ブラウザ E2E は v1 では手動)

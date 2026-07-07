# Contract Delta: ローカル API(掲載前のペルソナ選択)

**Feature**: [../spec.md](../spec.md) | **Base**: [specs/001-nostr-p2p-yp/contracts/local-api.md](../../001-nostr-p2p-yp/contracts/local-api.md)

本書は既存の 001 ローカル API 契約への**差分**のみを定める。保護方針(Host 検証・レート制限・トークン・ボディ上限)、エラー応答形式 `{"error":"<code>"}`、既存エンドポイントの基本挙動は 001 を正とし変更しない。

---

## 1. `PUT /api/v1/personas/{pubkey}` — 選択とロックの追加

既存のボディ(部分更新)は不変。`select`・`state`・`channel_id`・`label` の各フィールドは従来どおり。本機能で**拒否条件**を追加する。

### 追加する拒否条件

| 条件 | ステータス | body |
|------|-----------|------|
| `select: true` かつ対象が **archived** | 409 | `{"error":"persona_not_selectable"}` |
| `select: true` かつ対象が **unusable**(復号失敗) | 422 | `{"error":"persona_unusable"}` |
| `select: true` かつ**配信中**(1 つ以上発行中) | 409 | `{"error":"broadcasting_locked"}` |
| `state: "archived"` かつ対象が**現在の selected** かつ**配信中** | 409 | `{"error":"broadcasting_locked"}` |

- `label` の変更は配信中でも常に許可(FR-006、署名アイデンティティに影響しない)。
- 対象が selected でない、または非配信中であれば `state` 変更・破棄は従来どおり許可(FR-007)。
- 複数フィールド同時指定時は既存実装の適用順(label → state → select → channel_id)を維持。最初に失敗したガードのステータスを返す。
- 対象 `{pubkey}` が存在しない場合は既存どおり 404 `{"error":"not_found"}`(001 契約・data-model §エラー写像を正とする)。

### 受け入れ(Given/When/Then)

```gherkin
Scenario: 非配信中に有効ペルソナを選択できる
  Given active かつ usable なペルソナ B が存在し、何も発行していない
  When PUT /personas/{B} に {"select": true} を送る
  Then 204 が返る
  And GET /personas の B は "selected": true になる

Scenario: 配信中は selected の切替が拒否される(直接 API)
  Given selected=A で 1 つ以上のチャンネルを発行中
  When PUT /personas/{B} に {"select": true} を送る
  Then 409 {"error":"broadcasting_locked"} が返る
  And selected は A のまま変わらない

Scenario: 配信中は selected の破棄・アーカイブが拒否される
  Given selected=A で発行中
  When PUT /personas/{A} に {"state":"archived"} を送る
  Then 409 {"error":"broadcasting_locked"} が返る
  And DELETE /personas/{A}?confirm=true も 409 {"error":"broadcasting_locked"} を返す

Scenario: 配信中でも label 変更と他ペルソナ操作は許可される
  Given selected=A で発行中、別ペルソナ C が存在する
  When PUT /personas/{A} に {"label":"新名"} を送る
  Then 204 が返る
  When DELETE /personas/{C}?confirm=true を送る
  Then 204 が返る

Scenario: archived / unusable は選択できない
  Given archived なペルソナ D と unusable なペルソナ E が存在
  When PUT /personas/{D} に {"select": true} を送る
  Then 409 {"error":"persona_not_selectable"} が返る
  When PUT /personas/{E} に {"select": true} を送る
  Then 422 {"error":"persona_unusable"} が返る

Scenario: 停止後はロックが解ける
  Given 発行中で select が 409 になる状態
  When 全チャンネルが ended になる
  Then PUT /personas/{B} に {"select": true} は 204 を返す

Scenario: 古い画面状態から送信された制限操作は拒否され状態が最新化される(競合 edge case)
  Given selected=A で、UI 表示上はまだ配信中を認識していないが送信時点では発行中である
  When PUT /personas/{B} に {"select": true} を送る
  Then 409 {"error":"broadcasting_locked"} が返る
  And selected は A のまま変わらない
  And 後続の GET /status は "broadcasting": true を返し、UI は配信中表示へ更新できる
```

---

## 2. `DELETE /api/v1/personas/{pubkey}` — 配信中ロックの追加

既存の確認フラグ(`?confirm=true` 必須)は不変。追加する拒否条件:

| 条件 | ステータス | body |
|------|-----------|------|
| 対象が**現在の selected** かつ**配信中** | 409 | `{"error":"broadcasting_locked"}` |

配信に無関係な(selected でない)ペルソナは配信中でも破棄可能(FR-007)。

---

## 3. `GET /api/v1/status` — `broadcasting` フィールドの追加

既存の応答に真偽フィールドを 1 つ追加する(他フィールドは不変)。

```jsonc
{
  "pcp_listening": true,
  "established": { "in": 2, "out": 5 },
  "all_peers_unreachable": false,
  "clock_skew": { "median_sec": 3, "warning": false },
  "inbound_reachable": true,
  "broadcasting": true          // 追加: 1 つ以上のチャンネルを発行中なら true(FR-008 の定義)
}
```

- `broadcasting` は「実際にネットワークへ発行中(BroadcastState 集合が非空)」を表す。**保留中(未発行)チャンネルは含めない**(FR-008)。
- 供給元(BroadcastState)未配線時は `false`。
- UI(personas.html / channels.html)はこのフラグでボタンの無効化を判定する(既存 5 秒ポーリングに相乗り)。これは利便のための無効化であり、真の強制は §1/§2 のバックエンド拒否。

### 受け入れ

```gherkin
Scenario: 発行中は broadcasting=true を返す
  Given 1 つ以上のチャンネルを発行中
  When GET /status を送る
  Then "broadcasting": true が返る

Scenario: 未発行(保留のみ / 何もなし)は broadcasting=false
  Given 発行中チャンネルが 0(保留のみ、または無し)
  When GET /status を送る
  Then "broadcasting": false が返る
```

---

## 4. `GET /api/v1/personas` — 変更なし(表示は既存フィールドから導出)

`selected: bool` は既存フィールド。channels.html の「現在の selected 表示」は本一覧の `selected`・`label`・`pubkey` から導出する(新フィールド追加なし — spec Assumptions)。selected が archived/unusable になった場合、`selected()` のセマンティクス拡張(data-model §Selected Persona)により全要素の `selected` が false になり、UI は「未選択/警告」表示へ落ちる。

---

## 5. エラーコード一覧(本機能で新規)

| code | HTTP | 意味 |
|------|------|------|
| `broadcasting_locked` | 409 | 配信中は selected ペルソナの 切替/破棄/アーカイブを行えない |
| `persona_not_selectable` | 409 | archived ペルソナは選択できない |

`persona_unusable`(422)は既存コードの再利用。エラー応答は `{"error":"<code>"}` のみ(内部情報を含めない — Principle II)。

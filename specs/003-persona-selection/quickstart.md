# Quickstart 検証ガイド: 掲載前のペルソナ選択

**Feature**: [spec.md](./spec.md) | **Contracts**: [contracts/local-api.md](./contracts/local-api.md) | **Data model**: [data-model.md](./data-model.md)

本機能が end-to-end で満たされていることを確認するための実行手順。詳細な契約・データ制約は上記リンク先を正とし、ここでは再掲しない。

## 前提

- リポジトリルートで `cargo build` が通ること。
- ローカル起動後、UI(`http://127.0.0.1:<http_port>/`)へアクセスできること。変更系 API は `X-Api-Token`(`GET /api/v1/token` で取得)が必要。
- 上流 PeerCast(PeerCastStation もしくは peercast-yt)を用意し、配信の開始/停止で PCP チャンネルの到着/切断を再現できること(配信中ロックの検証に必要)。

## 起動

```
cargo run
```

`GET /api/v1/token` でセッショントークンを取得し、以降の `PUT`/`DELETE` の `X-Api-Token` に用いる。

---

## シナリオ 1: 掲載前に名乗るペルソナを選ぶ(US1 / SC-001)

1. `POST /api/v1/personas`(`{"label":"メイン"}`)で 1 個目を作成 → **自動的に selected** になることを確認(`GET /personas` の `selected: true`)。
2. `POST /api/v1/personas`(`{"label":"サブ"}`)で 2 個目を作成 → selected は**移らない**(メインのまま)ことを確認(FR-004)。
3. personas.html で「サブ」の**選択**ボタンを押す(または `PUT /personas/{sub}` に `{"select":true}`)→ 204。
4. 期待: 「現在選択中」バナーとチャンネル一覧画面(channels.html)の選択中表示が**即時**サブへ追随する(SC-001/SC-004)。

## シナリオ 2: 0 個 / 未選択の導線(US1 シナリオ 2・FR-013)

1. ペルソナが 0 個の状態で personas.html を開く → **作成を促す導線**が出る。
2. channels.html の選択中表示が「未選択(ペルソナを作成してください)」であることを確認。

## シナリオ 3: 選択可能条件(US1 / FR-002)

1. あるペルソナをアーカイブ(`PUT {state:"archived"}`、非配信中)。
2. personas.html でそのペルソナの選択ボタンが**無効(グレーアウト)**であることを確認。
3. `PUT /personas/{archived}` に `{"select":true}` を直接送る → **409 `persona_not_selectable`**(UI だけの防御ではないこと)。
4. (可能なら)復号不可(unusable)ペルソナで同操作 → **422 `persona_unusable`**。

## シナリオ 4: 配信中ロック(US2 / SC-002・SC-005 — 中核)

1. selected=A の状態で、上流 PeerCast で**配信を開始**し、PCP チャンネルが到着 → live 発行されることを確認(`GET /announced` に出る、`GET /status` の `broadcasting: true`)。
2. personas.html で A の**選択解除相当の切替**・**破棄**・**アーカイブ**ボタンが**無効化**され、「配信中はペルソナを変更できません」の理由が表示されることを確認。
3. 直接 API で以下を送り、いずれも **409 `broadcasting_locked`** を確認:
   - `PUT /personas/{B}` に `{"select":true}`(別ペルソナへ切替)
   - `PUT /personas/{A}` に `{"state":"archived"}`
   - `DELETE /personas/{A}?confirm=true`
4. selected が A のまま変わらないこと、配信が継続することを確認。
5. 配信中でも許可される操作を確認:
   - `PUT /personas/{A}` に `{"label":"新名"}` → 204(FR-006)
   - 配信に無関係な別ペルソナ C の作成・アーカイブ・破棄 → 成功(FR-007)

## シナリオ 5: ロック解除(US2 シナリオ 5 / SC-003)

1. 上流で**配信を停止**(または PCP を切断)→ ended が発行され `GET /status` が `broadcasting: false` になることを確認(異常切断でも同様 — FR-009)。
2. `PUT /personas/{B}` に `{"select":true}` → **204**(追加のリセット操作なしに再び選択できる)。

## シナリオ 6: selected が後から利用不可/アーカイブ(US3 シナリオ 2 / FR-011)

1. 非配信中に selected をアーカイブ、または鍵復号が失敗する状況(別環境の DB 持込み等)を再現。
2. 期待: 「現在選択中」表示が**警告状態**になり再選択を促される。`GET /personas` の該当要素 `selected` が false(未選択相当)になり、掲載は**保留**に落ちる(新規到着チャンネルが発行されない)。

---

## 自動テストでの担保(実装フェーズ)

| 検証 | テスト種別 | 対応 |
|------|-----------|------|
| select の active+usable ガード | ユニット(`identity`) | R4 / FR-002 |
| 配信中の 切替/破棄/アーカイブ拒否 | ユニット + 契約(409) | SC-002 |
| 予約→署名の相互排他(selected 凍結) | 並行性統合テスト | SC-005 / R2 |
| ended → 集合空 → 再選択可 | 統合テスト | SC-003 |
| ネガティブ(直接 API バイパス試行) | cucumber セキュリティシナリオ | Principle IV |
| `GET /status` の broadcasting | 契約テスト(`announced`) | R6 |

`cargo fmt -- --check` と `cargo clippy` を実装完了前に通すこと(CLAUDE.md / CI)。

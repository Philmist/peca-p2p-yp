# Phase 1 Data Model: 掲載前のペルソナ選択

**Feature**: [spec.md](./spec.md) | **Plan**: [plan.md](./plan.md) | **Research**: [research.md](./research.md)

本機能は新規の永続テーブルを追加しない。既存エンティティ(Persona / Settings)の**制約と状態遷移**を拡張し、揮発の共有状態 `BroadcastState` を新設する。正準の永続スキーマは 001 の data-model を正とし、本書はその差分を定める。

---

## エンティティ

### Persona(既存 — 選択可能性の制約を追加)

`personas` テーブル(pubkey, label, state, secret_enc, created_at)。本機能で追加する不変条件・導出属性:

| 属性 | 型 | 説明 | 本機能での制約 |
|------|----|------|----------------|
| `state` | `active` \| `archived` | ローカル状態 | **選択の前提**: `active` のみ選択可能(FR-002) |
| `usable`(導出) | bool | keystore で復号可能か | **選択の前提**: `usable == true` のみ選択可能。復号失敗は選択不可(FR-002) |
| `selected`(導出) | bool | 現在選択中か | 「§Selected Persona」参照。高々 1 つ(FR-003) |

**選択可能(selectable)の定義**: `state == Active && usable == true`。この条件を満たさないペルソナへの選択操作は UI で無効化し、バックエンドでも拒否する(FR-002)。

### Selected Persona(既存 Settings キー — セマンティクス拡張)

`settings` テーブルの `selected_persona` キー(値 = pubkey hex64)。次に掲載するチャンネルの既定署名鍵(グローバル、高々 1 つ)。

**導出セマンティクスの拡張(FR-011 / research R5)** — `selected()` が実効的に返す値:

| `selected_persona` の指す先 | 従来 | 本機能 |
|-----------------------------|------|--------|
| 存在し active かつ usable | その pubkey | その pubkey |
| 破棄済み(行なし) | None | None |
| archived | (その pubkey を返していた) | **None**(未選択相当・警告表示・掲載保留) |
| unusable(復号失敗) | (その pubkey を返していた) | **None**(未選択相当・警告表示・掲載保留) |

設定値そのものは archived/unusable でも消去しない(再 active 化・鍵復元で復帰可能)。判定は都度行う。

**自動選択(既存維持 — FR-004)**: 最初のペルソナ作成時のみ自動的に selected にする。2 個目以降は自動変更しない。自動選択も選択可能ガードを通る(作成直後は active+usable のため通過)。

### BroadcastState(新規 — 揮発の共有状態)

配信中(1 つ以上ネットワークへ発行中)のチャンネル集合と、選択変更との相互排他ロック。永続化しない(プロセス終了で消える)。`src/broadcast.rs`。

| フィールド | 型 | 説明 |
|-----------|----|------|
| `channels` | `Mutex<HashSet<ChannelId>>` | 現在配信中(初回発行済み・未終了)のチャンネル ID(hex32 小文字)集合。相互排他ロックの本体 |

**API(概念)**:

| 操作 | 意味 | 使用者 |
|------|------|--------|
| `is_broadcasting() -> bool` | 集合が非空か(= 配信中か。FR-008 の「配信中」定義) | `AppState`(status)・ロック判定 |
| `reserve_and_read_selected(...)`(概念) | ロック下で selected 読取 + チャンネル予約を原子的に行う | `PublishEngine`(発行開始) |
| `release(channel_id)` | チャンネルを集合から除去(終了・署名失敗の巻き戻し) | `PublishEngine` |
| `guard_selected_mutation(...)`(概念) | ロック下で「配信中なら拒否」を判定してから selected 変更/破棄/アーカイブを行う | `IdentityManager` |

**不変条件**:

- **INV-1(相互排他)**: 「配信中集合への予約 + selected 読取」と「selected の変更/破棄/アーカイブ」は同一ロック下で相互排他に実行される(research R2)。
- **INV-2(予約先行)**: あるチャンネルの初回署名は、当該チャンネルを配信中集合へ予約した**後に**行う。署名は集合ロックの外で行い、失敗時は予約を解除する。
- **INV-3(確実な解錠)**: チャンネルは終了発行(`publish_ended`)で集合から必ず除去される。PCP 異常切断も ended 経路を通るため配信中状態が取り残されない(FR-009、spec edge case)。

---

## 状態遷移

### selected の変更可否(配信中ロック — FR-005/FR-007/FR-009)

```
                 ┌─────────────────────────────────────────────┐
                 │  is_broadcasting == false(発行中チャンネル0)  │
                 │  ─ selected の 切替/破棄/アーカイブ: 許可      │
                 │  ─ 選択可能条件(active+usable)は別途要求      │
                 └───────────────┬─────────────────────────────┘
                                 │ 初回発行(予約)= 集合が空→非空
                                 ▼
                 ┌─────────────────────────────────────────────┐
                 │  is_broadcasting == true(発行中チャンネル≥1) │
                 │  ─ selected ペルソナの 切替: 409 broadcasting_locked │
                 │  ─ selected ペルソナの 破棄: 409             │
                 │  ─ selected ペルソナの アーカイブ: 409        │
                 │  ─ selected ペルソナの label 変更: 許可(FR-006)│
                 │  ─ 他ペルソナの 作成/破棄/アーカイブ: 許可(FR-007)│
                 └───────────────┬─────────────────────────────┘
                                 │ 全チャンネル終了(ended)= 集合が非空→空
                                 ▼
                      (上の「false」状態へ戻る = ロック解除・FR-009)
```

### 操作 × 状態のマトリクス

| 操作対象 | 非配信中 | 配信中(対象が selected) | 配信中(対象が非 selected) |
|----------|----------|--------------------------|----------------------------|
| selected を別ペルソナへ切替(`select`) | 許可(可能条件を満たせば) | **409** | **409** ※ | 
| ペルソナ破棄(`delete`) | 許可 | **409** | 許可 |
| アーカイブ(`set_state`→archived) | 許可 | **409** | 許可 |
| label 変更(`set_label`) | 許可 | 許可(FR-006) | 許可 |
| ペルソナ作成(`create`) | 許可 | 許可(FR-007) | 許可 |

※ `select` は selected **自体**を動かす操作であり、切替先ペルソナが selected か否かに依らず、配信中は一律 409(切替先が何であれ selected を動かせない)。したがって右 2 列(配信中)は両方 409 になる。「配信中(対象が非 selected)」列が意味を持つのは `delete` / `set_state(→archived)` / `set_label` のように「操作対象ペルソナ」が定まる操作のみ(FR-007)。

### 選択可能ガード(FR-002 — 配信状態と直交)

| 対象ペルソナ | `select` の結果 |
|-------------|-----------------|
| active かつ usable | 許可(非配信中のとき) |
| archived | 拒否 409 `persona_not_selectable` |
| unusable(復号失敗) | 拒否 422 `persona_unusable` |
| 存在しない | 拒否 404 `not_found` |

---

## エラー写像(IdentityError → HTTP)

| `IdentityError` | HTTP | code | 契機 |
|-----------------|------|------|------|
| `BroadcastingLocked`(新規) | 409 | `broadcasting_locked` | 配信中に selected の切替/破棄/アーカイブ |
| `NotSelectable`(新規) | 409 | `persona_not_selectable` | archived を選択 |
| `Unusable`(既存) | 422 | `persona_unusable` | 復号不可を選択 / 共有保管物 Unavailable |
| `NotFound`(既存) | 404 | `not_found` | 対象なし |
| `Crypto` / `Store`(既存) | 500 | `internal` | 内部エラー |

---

## 検証(Success Criteria への対応)

| 不変条件 / 制約 | 対応 SC | 検証手段 |
|-----------------|---------|----------|
| 配信中に selected が変化しない | SC-005 | 並行性統合テスト(予約 vs select の相互排他)+ cucumber ネガティブ |
| 配信中の切替/破棄/アーカイブが UI・API 双方で不成立 | SC-002 | ユニット(IdentityManager)+ 契約テスト(409) |
| 停止後は追加リセットなく再操作可能 | SC-003 | 統合テスト(ended → 集合空 → select 成功) |
| 選択は 1 クリック・即時反映 | SC-001 | UI 手動 + quickstart |
| selected が常に判別可能(未選択/警告も明示) | SC-004 | UI 手動 + quickstart |

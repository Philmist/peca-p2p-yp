# Phase 0 Research: 掲載前のペルソナ選択

**Feature**: [spec.md](./spec.md) | **Plan**: [plan.md](./plan.md) | **Date**: 2026-07-07

要件は事前の設計インタビュー(grill)で確定済み。本書は plan の Technical Context に残る技術的判断を確定し、実装の根拠を固定する。NEEDS CLARIFICATION は残っていない。

---

## R1. 配信中ロックの実施層 — バックエンド中核 vs Web ハンドラ

**Decision**: `IdentityManager`(バックエンド中核)に配信中ロックを実装する。Web ハンドラ(`personas.rs`)は結果の `IdentityError` を HTTP ステータスへ写像するのみ。

**Rationale**:

- spec FR-002/FR-005 は「UI のみの防御にしてはならない」を MUST とする。Web ハンドラでの事前判定だけでも「API 直叩き」は塞げるが、後述 R2 の TOCTOU レースを閉じるには「配信中判定」と「selected 変更」を**同一ロック下で原子的に**行う必要があり、その原子性は selected を所有する `IdentityManager` 内でしか担保できない。
- 既存の状態ガード(破棄済み selected を None 扱い等)は既に `IdentityManager` にある。選択可否・ロックも同じ層に集約するのが凝集度の面でも自然。

**Alternatives considered**:

- **Web ハンドラのみで判定**(`/status` の broadcasting フラグを見て 409 を返す): 実装は軽いが、フラグ読取と `identity.select()` の間に発行開始が割り込む TOCTOU 窓が残り、SC-005(MUST)を構造的に満たせない。棄却。

---

## R2. TOCTOU レースの構造的閉塞 — 予約(reserve)してから署名

**Decision**: 配信開始(あるチャンネルの初回発行)は、共有ロック下で **(a) selected を読み取り (b) 当該チャンネルを「配信中集合」へ予約** した後にロックを解放し、その後で署名・送信する。`select()`/`delete()`/`set_state(→archived)` は同じ共有ロックを取り、配信中集合が非空(該当ペルソナが対象)なら拒否する。

**Rationale**:

保護すべき不変条件は「あるチャンネル c が配信中の区間、selected は変化しない」= 「c の署名ペルソナが c の生存中に切り替わらない」(SC-005)。掲載エンジンは周期再発行のたびに `persona_for_channel` → selected を**再解決**するため、配信中に selected が変わると次の再発行で「旧 ended → 新 live」が発生する。単一の共有ロックで 2 つのクリティカルセクションを相互排他にすると:

- `select(B)` が先にロック取得 → 配信中集合が空 → selected=B 確定。続く発行は B を解決して開始。c は最初から B。切替なし。✓
- 発行開始が先にロック取得 → selected=A を読み予約 → 続く `select(B)` は集合非空 → 409 で拒否。c は A のまま。✓

**予約を署名の前に置く**のが要。もし「署名してから予約」にすると、署名中に `select(B)` が「まだ集合が空」と見て通り、その後で予約が入る、という窓が残る(発行済みは A・selected は B → 次回再発行で切替)。予約を先行させることで、発行開始と selected 変更のどちらが勝っても不変条件が保たれる。署名(暗号処理)はロック外で行い、ロック保持時間を最小化する。署名失敗時は予約を巻き戻す(集合から除去)。

**Alternatives considered**:

- **ペルソナのピン留め**(配信中チャンネルは `states[ch].persona` を固定し再解決しない): grill で棄却済み。`AnnouncedAdapter.persona_pubkey` の真実源切替や不一致警告が必要になり、掲載エンジンの改修範囲が広がる。ロック方式は「selected を凍結する」ことで真実源を単一に保てる。
- **`select()` の後検証**(書込み後に配信中なら巻き戻す): 巻き戻しの最中に再発行が走ると一貫性が崩れ、かえって複雑。棄却。

---

## R3. 配信中状態(BroadcastState)の所有と配線 — 循環依存の回避

**Decision**: 中立な新規モジュール `src/broadcast.rs` に `BroadcastState`(配信中チャンネル ID 集合を包む `Mutex` + 判定/予約/解除 API)を定義する。`IdentityManager`・`PublishEngine`・`AppState` がそれぞれ `Arc<BroadcastState>` を保持する(相互に相手を所有しない)。`main.rs` が 1 個の `Arc<BroadcastState>` を生成し 3 者へ配布する。

**Rationale**:

- `PublishEngine` は既に `Arc<IdentityManager>` を所有する。ロック判定のために `IdentityManager` が `PublishEngine` を参照すると相互所有(参照サイクル)になり `Arc` がリークする。共有状態を第三の中立オブジェクトに切り出すことで、どちらも相手を所有せずに同じ真実源を見られる。
- 既存 `PublishEngine.states`(per-channel の persona・created_at)はそのまま残す。`BroadcastState` は「どのチャンネルが配信中か」という集合のみを保持し、ロック判定の真実源とする。`states` への挿入/削除と同期して `BroadcastState` を更新する。
- テスト互換: 既存の `IdentityManager::new(store, keystore)` は多数のテストで使われる。`BroadcastState` フィールドは**既定で常に空(never broadcasting)**の `Arc` を持ち、`with_broadcast_state(Arc<BroadcastState>)` ビルダで共有インスタンスを注入する。既存テストは never-broadcasting のためロックガードが no-op になり挙動不変。

**Alternatives considered**:

- **`Weak<dyn BroadcastingStatus>` を identity へ注入**: 循環は切れるが、原子的な予約(R2)を `PublishEngine` 側に閉じ込められず、identity 側のロックと二重管理になる。共有状態方式のほうが単一ロックで原子性を担保できる。棄却。

---

## R4. 選択可能条件のガード — active かつ usable のみ(FR-002)

**Decision**: `IdentityManager::select()` に状態ガードを追加する。対象ペルソナが存在し、かつ `state == Active` かつ keystore で復号可能(`usable`)でなければ拒否する。新規エラーバリアント `IdentityError::NotSelectable` を追加し、Web 層で 409 `persona_not_selectable`(archived 等)/ 422 `persona_unusable`(復号不可、既存写像)へ写像する。

**Rationale**: UI のグレーアウトだけでは API 直叩きを防げない(FR-002 MUST)。`create()` の自動選択(最初のペルソナ)も本ガードを通るが、作成直後は active かつ usable なので通過する。共有保管物が Unavailable のときは既存の `signing_keys`/`create` と同様に利用不可を優先する。

**Alternatives considered**: 既存 `IdentityError::Unusable`(422)へ一本化する案もあるが、archived の選択(利用不可ではなく「対象外」)と復号失敗を利用者が区別できるよう別コードにする。

---

## R5. selected() のセマンティクス拡張 — archived / unusable も None 扱い(FR-011)

**Decision**: `IdentityManager::selected()` を、対象が破棄済みの場合に加えて **archived または復号不可(unusable)** の場合も `None` を返すよう拡張する。

**Rationale**: FR-011 は「selected が後からアーカイブ/利用不可になったら未選択相当として扱い、掲載は保留に落ちる」を要求する。`selected()` は `persona_for_channel` 経由で発行の署名鍵解決に使われるため、ここで None を返せば `publish_listing` が `Ok(false)`(保留)に落ち、警告表示(UI)と整合する。設定値自体(`selected_persona`)は消さず、再 active 化で復帰できる余地を残す(判定は都度)。

**設計上の含意**: selected が配信中に外部要因(OS 再インストール等の鍵消失)で unusable 化する事象はロックで防げない(利用者操作ではない)。この場合は既存の保留フォールバックに委ね、UI が警告と再選択導線を出す(FR-011、edge case)。ロックが守るのは「利用者操作による切替/破棄/アーカイブ」に限る。

---

## R6. 配信中フラグの UI 供給 — GET /status に broadcasting: bool 追加(FR-005 UI 側)

**Decision**: `GET /api/v1/status` の応答に `broadcasting: bool` を追加する。`AppState` に `Option<Arc<BroadcastState>>` を配線し、`StatusResponse` へ含める。UI は既存の 5 秒ポーリング(channels.html)に相乗りしてボタン無効化を判定する。

**Rationale**: 新規エンドポイントを増やさず既存ポーリングに乗せる(spec Assumptions)。UI の無効化はあくまで利便(誤操作の予防)であり、真の強制は R1 のバックエンドロック。ポーリング遅延による古い表示は edge case(競合)どおり、送信時に 409 で拒否され画面が最新化される。

**Alternatives considered**: channels 一覧(`/announced`)の非空で代用する案もあるが、`broadcasting` は「発行中か」の明示的真偽であり、保留チャンネル(未発行)を配信中に含めない FR-008 の定義と一致させるため専用フラグにする。

---

## R7. channels.html の selected 表示 — 既存 API から導出(新フィールド不要)

**Decision**: `GET /api/v1/personas` の各要素が既に持つ `selected: bool` と `label`・`pubkey` から、選択中ペルソナを人間可読(表示名 + 短縮 pubkey)で読み取り専用表示する。selected が導出できない(全て false = 未選択、または 0 個)場合は「未選択(ペルソナを作成してください)」/「選択中ペルソナが利用できません(再選択してください)」を表示する。

**Rationale**: spec Assumptions のとおり新データ項目は不要。R5 で selected() が archived/unusable を None 扱いにするため、`GET /personas` の `selected` フラグも該当時は全 false になり、UI は自然に「未選択/警告」表示へ落ちる。

---

## R8. Principle V(形式的検証)判定

**Decision**: **非該当**。判定と理由は [ADR-0011](../../docs/adr/0011-broadcasting-lock.md) に記録する(Principle VI MUST)。

**Rationale**: constitution Principle V のクリティカル 3 基準のうち、① 「新規設計であり既存の実績ある仕様・ライブラリの単純な利用ではない」が**不成立**。配信中ロックの中核は単一の共有ミューテックスによる 2 クリティカルセクションの相互排他であり、標準的な並行プリミティブの単純利用にとどまる(gossip 伝搬 [ADR-0005] のような創発的性質を持つ自前プロトコルではない)。相互排他が成立すれば不変条件は構成的に真になるため、TLC による網羅探索の限界価値は低い。③(ユーザー安全への影響)は成立するため、代替担保として **予約→署名の順序**と**配信中の切替拒否**を検証する並行性統合テストを必須とする(Principle V は SHOULD であり、理由を記録すれば非該当を許容)。

**Alternatives considered**: belt-and-suspenders で小さな PlusCal モデル(select vs publish-start の相互排他)を作る案。基準① 不成立のため必須ではなく、統合テストで十分と判断。将来ロック方式を多ロック化・再解決方式に変更する場合は本判定を再評価する。

---

## 確定事項サマリ

| 項目 | 確定 |
|------|------|
| ロック実施層 | `IdentityManager` 中核(Web は写像のみ) |
| レース閉塞 | 共有ロック下で予約→(ロック解放)→署名。予約先行が要 |
| 共有状態の所有 | 新規 `src/broadcast.rs` の `BroadcastState`、3 者が `Arc` 共有・相互非所有 |
| 選択ガード | active かつ usable のみ。`IdentityError::NotSelectable` 追加 |
| selected() 拡張 | 破棄済みに加え archived/unusable も None |
| ロック対象操作 | selected の 切替 / 破棄 / アーカイブ(→archived)。label 変更・他ペルソナは対象外 |
| エラーコード | 配信中: 409 `broadcasting_locked` / 選択不可: 409 `persona_not_selectable`(unusable は既存 422 `persona_unusable`) |
| status 拡張 | `GET /status` に `broadcasting: bool` |
| Principle V | 非該当(ADR-0011)。並行性統合テストで代替担保 |

# ADR-0005: gossip 伝搬プロトコルの形式的検証判定(Principle V)

**Status**: Accepted
**Date**: 2026-07-03
**Principles**: Principle V (Formal Verification for Critical Paths), Principle I, Principle II
**Task**: T009(Phase 2 実装前ゲート)。判定「該当」に伴う PlusCal モデルは T010

## 背景

gossip 伝搬プロトコル(重複抑制・再伝搬・接続時同期 — contracts/p2p-gossip.md)は
リレー非依存の新規設計であり、constitution Principle V の「クリティカル」判定候補である
(plan §Constitution Check、spec Assumptions「P2P プロトコルの新規性」)。
基準 3 点すべての充足可否を判定し、該当時の検証スコープを確定する。

## 判定: **該当**(3 基準すべてを満たす)

| 基準 | 判定 | 根拠 |
|------|------|------|
| ① 新規設計であり、既存の実績ある仕様・ライブラリの単純な利用ではない | **該当** | eager-push フラッディング + 重複抑制 + 接続時同期は自前プロトコル(research R13。libp2p 等は不採用)。plan §Constitution Check で判定済み |
| ② 競合状態・デッドロック・プロトコル違反がテストで再現困難な非自明さを持つ | **該当** | ループ不在・重複爆発不在は、フラッディング再伝搬 × DedupCache の期限切れ × EventStore 置換(LWW)× 接続時同期(SYNC 応答への伝搬規則 6 の適用)の相互作用から生じる**創発的性質**であり、単体の規則からは自明でない。多ノード・メッセージ順序・タイミング(期限切れと再受信の競合)に依存し、ユニット/統合テストでの網羅的再現は困難 |
| ③ 失敗がユーザー安全(Principle I)またはデータ整合性に直接影響する | **該当** | 重複爆発・伝搬ループは網全体の帯域・CPU を消費する自己 DoS となり全参加者の可用性を損なう(Principle I)。置換の単調性の破れ(旧イベントによる巻き戻し)は一覧の完全性(データ整合性)を破壊し、`ended` の巻き戻しは終了済みチャンネルの偽装継続を許す |

該当のため、PlusCal モデルを作成し TLC でデッドロック・不変条件違反を検査してから
gossip 中核(T017/T018/T037/T038)の実装に着手する(Principle V MUST)。

## 検証スコープ(モデル化する範囲)

対象: contracts/p2p-gossip.md **伝搬規則 1〜6** の状態機械。

- eager-push フラッディング(発行 = 格納 + 全ピア送信、再伝搬 = 受信元を除く全 established ピア)
- DedupCache による重複判定 — **期限切れ(保持 10 分)を非決定的に発生させる**(過大近似)
- EventStore の置換規則(last-write-wins、created_at 同値は event id 大)と
  **第二の防壁**(同一 event id 再受信の不格納・不再伝搬)
- 接続時同期(SYNC 応答として受信した EVENT への伝搬規則 1〜4 の適用 — 規則 6)
- 悪意ピアによる既発行イベントの再配信(リプレイ)— 署名は偽造できないため、
  流通しうるのは発行済みイベントのみという前提で任意タイミングの再注入を許す

検査する性質(contracts/p2p-gossip.md 伝搬規則 5 が明示する 3 性質 + 置換の単調性):

| 性質 | モデル上の定式化 |
|------|------------------|
| ループ不在・重複爆発不在 | `BoundedPropagation`: 各ノードは同一 event id を高々 1 回しか gossip 再伝搬しない(不変条件)+ `EventuallyQuiescent`: 転送中メッセージは必ず枯れる(時相性質 — 伝搬の終端保証) |
| live イベントの到達性 | `Convergence`: 静止状態では全ノードが最新イベントを保持する(不変条件) |
| 置換の単調性 | `StoreMonotonic`: EventStore が保持イベントより古い(または同値で id 劣後の)イベントへ後退しない(履歴変数との一致で検査) |
| デッドロック不在 | TLC の既定デッドロック検査(静止状態の自己ループのみを許容し、望まない停止と区別) |

トポロジはループ経路を含む完全グラフ(3 ノード)とし、created_at 同値・event id 相異の
イベント対を含めて置換のタイブレークの収束も検査する。

## モデル外とする範囲(理由と代替担保)

| 対象 | 理由 | 代替担保 |
|------|------|----------|
| イベント受信検証(署名・サイズ・時刻・PoW) | 実績ライブラリ+逐次検査であり基準①②不成立 | 契約テスト(T035)・ユニットテスト |
| PCP 状態機械 | 既存プロトコル準拠(constitution Principle V が明示的に対象外と定める) | 契約テスト(T023) |
| 接続管理・再接続バックオフ・PEX | ノード局所の状態のみで競合が単純。失敗の影響は接続性の一時低下にとどまり再試行で回復(基準②③不成立) | 統合テスト(T049 churn・T050 PEX) |
| EventStore 容量追い出し・鮮度期限 | 実時間(時計)のモデル化が必要で状態爆発。下記のとおり分析的に安全条件を導出し、設計制約として記録する | 下記「設計制約」+ ユニットテスト(T016) |
| レート制限・SYNC 平滑化 | 量的性質(帯域)であり状態機械の安全性と独立 | 契約(検査 2・6)+ 統合テスト |

## 設計制約の発見(分析結果 — 実装が守るべき不変条件)

**DedupCache の保持期間は鮮度窓(`freshness_window_sec`)以上でなければならない (MUST)。**

導出: EventStore から消えたイベント(容量追い出し・鮮度切れ)は「第二の防壁」の保護外に
なるため、DedupCache も期限切れした後に同一イベントを再受信すると再格納・再伝搬が起こりうる
(ループ再発の唯一の残余経路)。この経路が塞がる条件は「DedupCache が切れる時刻
(初回受信 + 保持期間)には当該イベントが必ず鮮度切れ(created_at + 600 秒 ≤ 初回受信 +
保持期間)となり、受信検証(expiration / 鮮度窓)で拒否される」ことであり、
初回受信 ≥ created_at から **保持期間 ≥ 鮮度窓** がその十分条件である。

既定値(保持 10 分 = 600 秒、鮮度窓 600 秒)は等号で成立するが、Settings で
`freshness_window_sec` を増やすと破れる。**実装(T016)は DedupCache 保持期間を
`max(600 秒, freshness_window_sec)` として連動させること**(tasks.md T016 に反映済み)。

## 成果物と実装ゲート

- PlusCal モデル: [docs/formal/gossip_propagation.tla](../formal/gossip_propagation.tla)
  (TLC 設定: 同 `.cfg`)
- 検証結果: [docs/formal/gossip_propagation-result.md](../formal/gossip_propagation-result.md)
  (検査した不変条件・状態数・発見事項)
- gossip 中核タスク(T017/T018/T037/T038)は上記モデルの TLC 検査完了後に着手する
  (Principle V — tasks.md Phase Dependencies に明記済み)
- 実装が伝搬規則を変更する場合は、モデルを先に更新し再検査してから実装する

## 原則参照

- Principle V: クリティカル判定基準 3 点の充足判定と理由の ADR 化(MUST)
- Principle I: 重複爆発 = 網全体の自己 DoS の予防
- Principle II: 悪意ピアのリプレイをモデルの敵対者として含める
- contracts/p2p-gossip.md 伝搬規則 1〜6 / §検証方法、research R13、data-model §EventStore / §DedupCache

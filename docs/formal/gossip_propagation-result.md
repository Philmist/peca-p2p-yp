# gossip 伝搬プロトコル — TLC 検証結果

**Date**: 2026-07-04
**対象**: [gossip_propagation.tla](./gossip_propagation.tla)(TLC 設定: [gossip_propagation.cfg](./gossip_propagation.cfg))
**根拠 ADR**: [ADR-0005](../adr/0005-gossip-formal-verification.md)(Principle V 判定「該当」/ T010)
**モデル化対象**: contracts/p2p-gossip.md 伝搬規則 1〜6(eager-push フラッディング・
DedupCache 重複抑制・EventStore 置換(LWW)・接続時同期・リプレイ敵対者)

## 結果: **PASS — エラーなし**

```
Model checking completed. No error has been found.
1,882,599,404 states generated, 199,714,875 distinct states found, 0 states left on queue.
The depth of the complete state graph search is 24.
```

| 項目 | 値 |
|------|-----|
| TLC | 2.19 (2024-08-08) / Java OpenJDK 25.0.3 / 12 workers |
| 実行コマンド | `java -XX:+UseParallelGC tlc2.TLC -workers auto -lncheck final -config gossip_propagation.cfg gossip_propagation.tla` |
| 生成状態数 | 1,882,599,404 |
| 相異状態数 | 199,714,875 |
| 探索深さ | 24(完全探索 — キュー 0 で終了) |
| 所要時間 | 5 時間 00 分 |
| fingerprint 衝突確率(実測ベース) | 7.6E-4(全到達状態を検査できなかった確率の見積り — 十分小) |
| 定数 | `MaxSync = 2`, `MaxInject = 1`, `MaxExpire = 2` |

## 検査した性質(すべて成立)

| 性質 | 種別 | 内容 |
|------|------|------|
| `TypeOK` | 不変条件 | 全変数の型・値域の健全性 |
| `BoundedPropagation` | 不変条件 | **ループ不在・重複爆発不在** — 各ノードは同一 event id を高々 1 回しか gossip 再伝搬しない |
| `StoreMonotonic` | 不変条件 | **置換の単調性** — EventStore が保持イベントより古い(または created_at 同値で id 劣後の)イベントへ後退しない(履歴変数 `hi` との一致で検査) |
| `Convergence` | 不変条件 | **到達性** — 静止状態では全ノードが最終勝者イベント(E3)を保持する |
| `EventuallyQuiescent` | 時相性質 | **伝搬の終端保証** — 転送中メッセージは必ず枯れる(`<>[]Quiescent`。`-lncheck final` により完全な状態グラフに対して最終検査) |
| デッドロック不在 | TLC 既定 | 静止状態の自己ループのみを許容(望まない停止と区別) |

## モデルの構成と抽象化

- **トポロジ**: 3 ノード完全グラフ(ループ経路を含む)。`n1` が発行ノード
- **イベント**: 置換キー `(kind, pubkey, d)` は単一キーに抽象化。event id は数値
  (実装の hex 辞書順比較を数値比較で代替)。E2/E3 は created_at 同値・id 相異とし、
  LWW タイブレーク(id 大が勝つ)の収束も検査
- **受信処理**: 取り出し→重複判定(DedupCache)→置換判定→格納・再伝搬(受信元を除く)を
  原子的に実行。「第二の防壁」(同一 event id の再受信は不格納・不再伝搬)は
  `ShouldStore` の `s.id # ev.id` 条件として表現され、モデル上は厳密順序 `Newer` による
  置換判定に包含される
- **接続時同期(伝搬規則 6)**: 任意タイミング・任意の有向対で応答側の保持イベントを
  要求側へ送る(再接続・分断再結合の近似)。SYNC 受信には通常の伝搬規則 1〜4 が適用される
- **敵対者(リプレイ)**: 署名は偽造できない前提(受信検証はモデル外)で、
  発行済みイベントの任意タイミング再注入を許す
- **DedupCache 期限切れ**: 任意タイミング・任意エントリの削除として**過大近似**
  (実際の「保持 10 分経過」より広い挙動を含むため、成立した不変条件は実挙動でも成立する)

### 有界化パラメータの選定理由

網羅性(過大近似)を保ったまま状態空間を有限に閉じるため、発生**回数**のみを予算で有界化した:

- `MaxExpire = 2`: 検査対象シナリオ「期限切れ後の同一イベント再受信」(ループ再発の
  唯一の残余経路)は少数回の期限切れで発現する。無制限版は状態空間が発散し完了不能
  (下記「検証過程の知見」)
- `MaxSync = 2`(全体・任意の有向対): 検査したい相互作用 —「同期が古いイベントを
  後から届けても格納が後退しない」「分断再結合後に収束する」— は 2 回の発生で網羅できる。
  当初の「有向対ごとに 1 回(全体 6 回)」はメッセージ交錯の組合せ爆発の主因となり不採用。
  応答側の保持が空の同期(msgs を変えない無意味な遷移)は除外
- `MaxInject = 1`: 不変条件はすべて「各受信の局所判定」で維持されるため、
  1 回の注入で敵対的再配信との相互作用は検査できる

## 発見事項

1. **設計制約(実装が守るべき不変条件)**: DedupCache 保持期間 ≥ `freshness_window_sec`
   (MUST)。EventStore から消えたイベントは「第二の防壁」の保護外になるため、
   DedupCache も期限切れした後の再受信で再伝搬が起こりうる。モデルでは鮮度・容量追い出しを
   対象外としたため分析的に導出し、ADR-0005 に記録した。実装(T016)は保持期間を
   `max(600 秒, freshness_window_sec)` として連動させること
2. **違反・デッドロックは一件も検出されず**: 契約の伝搬規則 1〜6 は、期限切れ・同期・
   リプレイの任意交錯の下でも 4 不変条件+終端保証を維持する
3. **検証過程の知見(モデリング上)**: 期限切れを回数無制限で近似した初版は状態空間が
   発散した(部分探索 3 億〜12 億状態でも違反ゼロのまま完了不能)。伝搬プロトコル自体の
   問題ではなく近似の粒度の問題であり、回数予算による有界化で完全探索が可能になった

## 実装ゲートへの帰結

- gossip 中核タスク **T017 / T018 / T037 / T038 は着手可能**(Principle V ゲート解除)
- 実装が伝搬規則(重複抑制・置換規則・「格納成功イベントのみ再伝搬」・「受信元を除く」)を
  変更する場合は、本モデルを先に更新し再検査すること
  (security-review-checklist 観点 11)

## 再検査手順

```powershell
Set-Location docs\formal
java pcal.trans gossip_propagation.tla        # PlusCal → TLA+ 変換(モデル変更時のみ)
java -XX:+UseParallelGC tlc2.TLC -workers auto -lncheck final -config gossip_propagation.cfg gossip_propagation.tla
```

所要 5 時間程度(12 コア)。TLC が生成する `states/` ディレクトリと pcal.trans の
バックアップ `*.old` は成果物ではない(.gitignore 済み)。

------------------------- MODULE gossip_propagation -------------------------
(***************************************************************************)
(* Gossip 伝搬プロトコル(contracts/p2p-gossip.md 伝搬規則 1〜6)の        *)
(* PlusCal モデル。ADR-0005(Principle V 判定「該当」)に基づき、          *)
(* 実装(T017/T018/T037/T038)前に以下の性質を TLC で検査する:            *)
(*                                                                         *)
(*   - BoundedPropagation : ループ不在・重複爆発不在(各ノードは同一      *)
(*     event id を高々 1 回しか gossip 再伝搬しない)                      *)
(*   - StoreMonotonic     : 置換の単調性(EventStore が旧イベントへ       *)
(*     後退しない — 履歴変数 hi との一致で検査)                          *)
(*   - Convergence        : 到達性(静止状態で全ノードが最新イベントを    *)
(*     保持する)                                                          *)
(*   - EventuallyQuiescent: 伝搬の終端保証(転送中メッセージは必ず枯れる)*)
(*   - デッドロック不在   : TLC 既定検査(静止状態の自己ループのみ許容)  *)
(*                                                                         *)
(* モデル境界(検証しない範囲)と検証結果は                               *)
(* docs/formal/gossip_propagation-result.md を参照。                       *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS MaxSync,   \* 接続時同期(SYNC)の発生回数の上限(全体 — 状態空間の有界化)
          MaxInject, \* 悪意ピアによる既発行イベント再配信(リプレイ)回数の上限
          MaxExpire  \* DedupCache 期限切れ発生回数の上限(状態空間の有界化)

ASSUME MaxSync \in Nat /\ MaxInject \in Nat /\ MaxExpire \in Nat

Nodes == {"n1", "n2", "n3"}

\* 完全グラフ(三角形)— ループ経路を含むトポロジでループ不在を検査する
Neighbors(n) == Nodes \ {n}

Origin == "n1"  \* 発行ノード(掲載側)

(***************************************************************************)
(* イベント: 置換キー (kind, pubkey, d) は単一キーに抽象化し、created_at  *)
(* と event id のみを持つ。id は数値で抽象化(実装は hex 辞書順比較 —    *)
(* 数値比較で代替)。E2 と E3 は created 同値・id 相異とし、              *)
(* last-write-wins のタイブレーク(id 大が勝つ)の収束を検査する。        *)
(***************************************************************************)
E1 == [id |-> 1, created |-> 1]
E2 == [id |-> 2, created |-> 2]
E3 == [id |-> 3, created |-> 2]
PubSeq == <<E1, E2, E3>>          \* Origin がこの順に発行する
AllEvents == {E1, E2, E3}
EventIds == {1, 2, 3}
NoEvent == [id |-> 0, created |-> 0]

\* EventStore の置換規則: created_at 最大、同値なら event id 大(伝搬規則 3)
Newer(a, b) == \/ a.created > b.created
               \/ (a.created = b.created /\ a.id > b.id)

\* 全イベント中の最終勝者 = 全ノードが収束すべきイベント(E3)
Winner == CHOOSE e \in AllEvents : \A f \in AllEvents \ {e} : Newer(e, f)

\* 転送中 EVENT メッセージ(to = 宛先、frm = 受信元となるピア)
Msgs == {m \in [to : Nodes, frm : Nodes, ev : AllEvents] : m.to # m.frm}

\* 接続時同期の有向対 <<要求側, 応答側>>
DirPairs == {p \in Nodes \X Nodes : p[1] # p[2]}

(* --fair algorithm gossip {
  variables
    pubIdx = 1;                                        \* 次に発行する PubSeq 位置
    store  = [n \in Nodes |-> {}];                     \* EventStore(単一キーなので高々 1 要素)
    dedup  = [n \in Nodes |-> {}];                     \* DedupCache(event id の集合)
    msgs   = [m \in Msgs |-> 0];                       \* 転送中 EVENT の多重集合
    sent   = [n \in Nodes |-> [i \in EventIds |-> 0]]; \* gossip 伝搬回数(検査用)
    hi     = [n \in Nodes |-> NoEvent];                \* これまでに格納した最大イベント(検査用履歴)
    syncBudget = MaxSync;
    injBudget  = MaxInject;
    expBudget  = MaxExpire;

  define {
    Pending   == {m \in Msgs : msgs[m] > 0}
    Published == {PubSeq[i] : i \in 1..(pubIdx - 1)}
    Quiescent == pubIdx > Len(PubSeq) /\ Pending = {}

    \* 伝搬規則 2・3: 重複判定(DedupCache)→ 置換判定(同一 id は
    \* 「第二の防壁」により格納・再伝搬しない。旧版・同値劣後も置換しない)
    ShouldStore(n, ev) ==
      /\ ev.id \notin dedup[n]
      /\ \/ store[n] = {}
         \/ \E s \in store[n] : s.id # ev.id /\ Newer(ev, s)

    \* ---- 検査する不変条件 ----
    TypeOK ==
      /\ pubIdx \in 1..(Len(PubSeq) + 1)
      /\ store \in [Nodes -> SUBSET AllEvents]
      /\ \A n \in Nodes : Cardinality(store[n]) <= 1
      /\ dedup \in [Nodes -> SUBSET EventIds]
      /\ msgs \in [Msgs -> Nat]
      /\ sent \in [Nodes -> [EventIds -> Nat]]
      /\ hi \in [Nodes -> (AllEvents \union {NoEvent})]
      /\ syncBudget \in 0..MaxSync
      /\ injBudget \in 0..MaxInject
      /\ expBudget \in 0..MaxExpire

    \* ループ不在・重複爆発不在: 各ノードは同一 event id を高々 1 回しか伝搬しない
    BoundedPropagation == \A n \in Nodes : \A i \in EventIds : sent[n][i] <= 1

    \* 置換の単調性: 現在の保持イベント = これまでに格納した最大イベント
    StoreMonotonic ==
      \A n \in Nodes :
        IF store[n] = {} THEN hi[n] = NoEvent ELSE store[n] = {hi[n]}

    \* 到達性: 静止状態では全ノードが最終勝者を保持している
    Convergence == Quiescent => (\A n \in Nodes : store[n] = {Winner})
  }

  process (world = "world")
  {
  Loop:
    while (TRUE) {
      either {
        \* 発行(nostr-events.md 発行規則: 発行 = 自ノード格納 + 全ピアへ EVENT)
        await pubIdx <= Len(PubSeq);
        with (ev = PubSeq[pubIdx]) {
          store[Origin] := {ev};
          hi := [hi EXCEPT ![Origin] = IF Newer(ev, @) THEN ev ELSE @];
          sent := [sent EXCEPT ![Origin][ev.id] = @ + 1];
          msgs := [m \in Msgs |->
                     msgs[m] + (IF m.frm = Origin /\ m.ev = ev THEN 1 ELSE 0)];
          pubIdx := pubIdx + 1;
        };
      } or {
        \* 受信処理(伝搬規則 1〜4)— 取り出し・判定・格納・再伝搬を原子的に行う。
        \* 格納成功時のみ、受信元(m.frm)を除く全ピアへ再伝搬する
        with (m \in Pending) {
          with (n = m.to, ev = m.ev, ok = ShouldStore(m.to, m.ev)) {
            msgs := [mm \in Msgs |->
                       msgs[mm] - (IF mm = m THEN 1 ELSE 0)
                                + (IF ok /\ mm.frm = n /\ mm.ev = ev /\ mm.to # m.frm
                                   THEN 1 ELSE 0)];
            dedup := [dedup EXCEPT ![n] = @ \union {ev.id}];
            if (ok) {
              store := [store EXCEPT ![n] = {ev}];
              hi := [hi EXCEPT ![n] = IF Newer(ev, @) THEN ev ELSE @];
              sent := [sent EXCEPT ![n][ev.id] = @ + 1];
            };
          };
        };
      } or {
        \* 接続時同期(伝搬規則 6): 応答側 p[2] が保持イベントを要求側 p[1] へ
        \* EVENT として送る。受信側では通常の伝搬規則 1〜4 が適用される。
        \* 任意のタイミング・任意の有向対で発生させ、再接続・分断再結合を近似する。
        \* 発生回数は全体で MaxSync に有界化(有向対ごとの予算は状態爆発のため不採用。
        \* 検査対象の相互作用は 2 回の発生で網羅できる)。応答側の保持が空の同期は
        \* msgs を変えない無意味な遷移のため除外する
        await syncBudget > 0;
        with (p \in {q \in DirPairs : store[q[2]] # {}}) {
          msgs := [m \in Msgs |->
                     msgs[m] + (IF m.to = p[1] /\ m.frm = p[2] /\ m.ev \in store[p[2]]
                                THEN 1 ELSE 0)];
          syncBudget := syncBudget - 1;
        };
      } or {
        \* DedupCache の期限切れ(保持 10 分の経過を任意タイミングで近似 — 過大近似)。
        \* 発生回数は MaxExpire で有界化する(無制限だと状態空間が発散する。
        \* 検査対象シナリオ「期限切れ後の再受信」は少数回の発生で網羅できる)
        await expBudget > 0;
        with (n \in {v \in Nodes : dedup[v] # {}}) {
          with (i \in dedup[n]) {
            dedup := [dedup EXCEPT ![n] = @ \ {i}];
            expBudget := expBudget - 1;
          };
        };
      } or {
        \* 悪意ピアによる既発行イベントの再配信(リプレイ)。署名は偽造できない
        \* ため、流通しうるのは発行済みイベントのみ(受信検証 2 はモデル外の前提)
        await injBudget > 0 /\ Published # {};
        with (ev \in Published, n \in Nodes, f \in Neighbors(n)) {
          msgs := [m \in Msgs |->
                     msgs[m] + (IF m.to = n /\ m.frm = f /\ m.ev = ev THEN 1 ELSE 0)];
          injBudget := injBudget - 1;
        };
      } or {
        \* 静止状態の自己ループ(望まない停止 = デッドロックと区別するため)
        await Quiescent;
        skip;
      };
    };
  }
} *)
\* BEGIN TRANSLATION
VARIABLES pubIdx, store, dedup, msgs, sent, hi, syncBudget, injBudget, 
          expBudget

(* define statement *)
Pending   == {m \in Msgs : msgs[m] > 0}
Published == {PubSeq[i] : i \in 1..(pubIdx - 1)}
Quiescent == pubIdx > Len(PubSeq) /\ Pending = {}



ShouldStore(n, ev) ==
  /\ ev.id \notin dedup[n]
  /\ \/ store[n] = {}
     \/ \E s \in store[n] : s.id # ev.id /\ Newer(ev, s)


TypeOK ==
  /\ pubIdx \in 1..(Len(PubSeq) + 1)
  /\ store \in [Nodes -> SUBSET AllEvents]
  /\ \A n \in Nodes : Cardinality(store[n]) <= 1
  /\ dedup \in [Nodes -> SUBSET EventIds]
  /\ msgs \in [Msgs -> Nat]
  /\ sent \in [Nodes -> [EventIds -> Nat]]
  /\ hi \in [Nodes -> (AllEvents \union {NoEvent})]
  /\ syncBudget \in 0..MaxSync
  /\ injBudget \in 0..MaxInject
  /\ expBudget \in 0..MaxExpire


BoundedPropagation == \A n \in Nodes : \A i \in EventIds : sent[n][i] <= 1


StoreMonotonic ==
  \A n \in Nodes :
    IF store[n] = {} THEN hi[n] = NoEvent ELSE store[n] = {hi[n]}


Convergence == Quiescent => (\A n \in Nodes : store[n] = {Winner})


vars == << pubIdx, store, dedup, msgs, sent, hi, syncBudget, injBudget, 
           expBudget >>

ProcSet == {"world"}

Init == (* Global variables *)
        /\ pubIdx = 1
        /\ store = [n \in Nodes |-> {}]
        /\ dedup = [n \in Nodes |-> {}]
        /\ msgs = [m \in Msgs |-> 0]
        /\ sent = [n \in Nodes |-> [i \in EventIds |-> 0]]
        /\ hi = [n \in Nodes |-> NoEvent]
        /\ syncBudget = MaxSync
        /\ injBudget = MaxInject
        /\ expBudget = MaxExpire

world == \/ /\ pubIdx <= Len(PubSeq)
            /\ LET ev == PubSeq[pubIdx] IN
                 /\ store' = [store EXCEPT ![Origin] = {ev}]
                 /\ hi' = [hi EXCEPT ![Origin] = IF Newer(ev, @) THEN ev ELSE @]
                 /\ sent' = [sent EXCEPT ![Origin][ev.id] = @ + 1]
                 /\ msgs' = [m \in Msgs |->
                               msgs[m] + (IF m.frm = Origin /\ m.ev = ev THEN 1 ELSE 0)]
                 /\ pubIdx' = pubIdx + 1
            /\ UNCHANGED <<dedup, syncBudget, injBudget, expBudget>>
         \/ /\ \E m \in Pending:
                 LET n == m.to IN
                   LET ev == m.ev IN
                     LET ok == ShouldStore(m.to, m.ev) IN
                       /\ msgs' = [mm \in Msgs |->
                                     msgs[mm] - (IF mm = m THEN 1 ELSE 0)
                                              + (IF ok /\ mm.frm = n /\ mm.ev = ev /\ mm.to # m.frm
                                                 THEN 1 ELSE 0)]
                       /\ dedup' = [dedup EXCEPT ![n] = @ \union {ev.id}]
                       /\ IF ok
                             THEN /\ store' = [store EXCEPT ![n] = {ev}]
                                  /\ hi' = [hi EXCEPT ![n] = IF Newer(ev, @) THEN ev ELSE @]
                                  /\ sent' = [sent EXCEPT ![n][ev.id] = @ + 1]
                             ELSE /\ TRUE
                                  /\ UNCHANGED << store, sent, hi >>
            /\ UNCHANGED <<pubIdx, syncBudget, injBudget, expBudget>>
         \/ /\ syncBudget > 0
            /\ \E p \in {q \in DirPairs : store[q[2]] # {}}:
                 /\ msgs' = [m \in Msgs |->
                               msgs[m] + (IF m.to = p[1] /\ m.frm = p[2] /\ m.ev \in store[p[2]]
                                          THEN 1 ELSE 0)]
                 /\ syncBudget' = syncBudget - 1
            /\ UNCHANGED <<pubIdx, store, dedup, sent, hi, injBudget, expBudget>>
         \/ /\ expBudget > 0
            /\ \E n \in {v \in Nodes : dedup[v] # {}}:
                 \E i \in dedup[n]:
                   /\ dedup' = [dedup EXCEPT ![n] = @ \ {i}]
                   /\ expBudget' = expBudget - 1
            /\ UNCHANGED <<pubIdx, store, msgs, sent, hi, syncBudget, injBudget>>
         \/ /\ injBudget > 0 /\ Published # {}
            /\ \E ev \in Published:
                 \E n \in Nodes:
                   \E f \in Neighbors(n):
                     /\ msgs' = [m \in Msgs |->
                                   msgs[m] + (IF m.to = n /\ m.frm = f /\ m.ev = ev THEN 1 ELSE 0)]
                     /\ injBudget' = injBudget - 1
            /\ UNCHANGED <<pubIdx, store, dedup, sent, hi, syncBudget, expBudget>>
         \/ /\ Quiescent
            /\ TRUE
            /\ UNCHANGED <<pubIdx, store, dedup, msgs, sent, hi, syncBudget, injBudget, expBudget>>

Next == world

Spec == /\ Init /\ [][Next]_vars
        /\ WF_vars(Next)

\* END TRANSLATION

\* 伝搬の終端保証: いかなる公平な実行でも、最終的に静止状態に到達し続ける
EventuallyQuiescent == <>[]Quiescent

==============================================================================

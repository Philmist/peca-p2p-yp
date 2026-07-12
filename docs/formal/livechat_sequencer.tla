------------------------- MODULE livechat_sequencer -------------------------
(***************************************************************************)
(* 実況スレのホスト = シーケンサ状態機械                                   *)
(* (specs/006-livechat-thread contracts/thread-delivery.md)の PlusCal    *)
(* モデル。ADR-0014(Principle V 判定「該当」)に基づき、シーケンサ実装    *)
(* (tasks T030)前に以下の性質を TLC で検査する:                          *)
(*                                                                         *)
(*   - AssignedOnce    : 採番の一意性(どの書き込みイベントも高々 1 つの  *)
(*     (gen, res_no) にしか採番されない — 再送・移行境界の二重採番なし。  *)
(*     不変条件 O1)                                                       *)
(*   - NoOverLimit     : 上限超過採番なし(各世代の確定数 <= ResLimit)   *)
(*   - FrozenGenStable : 旧世代(移行済み)のログは移行時点から不変       *)
(*     (凍結スレへの採番なし — 不変条件 T1)                              *)
(*   - ClosedStable    : クローズ・クラッシュ後に採番が増えない(T1)     *)
(*   - DisplayPrefix   : 各参加者の表示列は常にホスト確定列の接頭辞       *)
(*     (欠番なし単調増加 = 不変条件 T3 の参加者側到達形。全端末で同一    *)
(*     res_no が同一イベントに解決される = SC-002 / 不変条件 O2)         *)
(*   - DeleteOnClose   : クローズ通知を処理した参加者のスレデータは空     *)
(*     (揮発 — FR-014/FR-015)                                            *)
(*   - Convergence     : 静止状態では接続中参加者の表示列 = ホスト確定列  *)
(*     (確定情報の全接続参加者への到達)                                  *)
(*   - EventuallyQuiescent : 公平な実行は必ず静止に到達し続ける           *)
(*     (ライブネス)                                                      *)
(*   - デッドロック不在: TLC 既定検査(静止状態の自己ループのみ許容)     *)
(*                                                                         *)
(* モデル境界(検証しない範囲 — 代替担保は ADR-0014 §2 非該当表):       *)
(*   - 署名・サイズ・PoW・BAN・レートの受信検証は契約テストで担保する。   *)
(*     偽 ORDER はスレ主署名検証で破棄され状態機械に到達しない(FR-011) *)
(*   - トランスポート(TCP)は順序保存・信頼配送とし、参加者ごとの        *)
(*     chan(FIFO 列)で表現する。接続断 = chan の消失                    *)
(*   - RESEND_REQ は「再接続時の since_seq 差分同期」(Reconnect)に      *)
(*     吸収する。in-order 転送では seq の飛びは接続断でのみ生じるため、   *)
(*     セッション内の欠落検出→再送要求は同じ機構の別入口である。         *)
(*     参加者側の O2 規則(連続する seq のみ適用)は Deliver のガードで   *)
(*     モデル化する                                                       *)
(*   - ORDER の entries バッチ(1 通で複数採番)は 1 件/通に抽象化する   *)
(*     (seq = res_no。バッチはこのモデルの列を区切り直すだけで性質に     *)
(*     影響しない)                                                       *)
(*   - JOIN/WELCOME/チャレンジ検証・announce による接続先発見は           *)
(*     Reconnect に原子化する。板設定配布・互換 API はモデル外            *)
(*                                                                         *)
(* モデル化で導出した設計制約(実装 T030 が守るべき規則):                *)
(*   - D1: ホストは採番前に event_id の重複を板単位(世代を跨いで)で     *)
(*     排除しなければならない (MUST)。参加者は確定通知(ORDER)を受け取る *)
(*     前に切断されると同じイベントを再送しうるため、重複排除がないと     *)
(*     同一イベントが二つの res_no を得て AssignedOnce が破れる           *)
(*     (HostProcess の採番ガード w.id \notin AssignedIds が対応)         *)
(*   - D2: 旧世代宛(移行境界)の書き込みの「新スレへ採番」を選ぶ場合、   *)
(*     イベントの thread タグは旧 gen のままである(署名済みで書き換え   *)
(*     不能)。採番先スレと thread タグの不一致を許容するか、旧世代宛を   *)
(*     常に定型拒否するかは T030 で確定する(本モデルは両挙動とも安全で   *)
(*     あることを検査する)                                               *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS MaxGen,    \* スレ世代数の上限(状態空間の有界化。移行は MaxGen-1 回)
          ResLimit,  \* 1 スレのレス上限(板設定 res_limit の縮小モデル)
          MaxWrites, \* 新規書き込みの発生回数の上限(状態空間の有界化)
          MaxRetry,  \* 書き込み再送(確定未達での再提出)回数の上限
          MaxDisc    \* 参加者の切断(凍結)発生回数の上限

ASSUME /\ MaxGen \in Nat \ {0}
       /\ ResLimit \in Nat \ {0}
       /\ MaxWrites \in Nat
       /\ MaxRetry \in Nat
       /\ MaxDisc \in Nat

Parts == {"p1", "p2"}   \* 参加者(視聴者ノード)。ホストは world 内の採番動作として表現
Gens  == 1..MaxGen
Writes == 1..MaxWrites  \* 書き込みイベント id(= event_id の抽象)

Range(s) == {s[i] : i \in DOMAIN s}
PrefixOf(s, t) == Len(s) <= Len(t) /\ \A i \in DOMAIN s : s[i] = t[i]

\* 参加者チャネル上のメッセージ(ord = RES+ORDER の合成 1 通に抽象化)
OrdMsg(g, i, w) == [t |-> "ord", g |-> g, i |-> i, w |-> w]
Msgs == [t : {"ord"}, g : Gens, i : 1..ResLimit, w : Writes]
          \union [t : {"next"}, g : Gens]
          \union [t : {"close"}]

(* --fair algorithm livechat {
  variables
    activeGen = 1;                                   \* 現行(Active)世代 — 不変条件 T2 は「板ごとに変数 1 個」で構造的に成立
    phase = "active";                                \* ホスト/板の状態: active / closed(明示クローズ)/ crashed(通知なき停止)
    log = [g \in Gens |-> <<>>];                     \* ホストの確定ログ(世代ごと)。log[g][i] = res_no i に採番したイベント
    frozenLen = [g \in Gens |-> 0];                  \* 移行・クローズ時点のログ長スナップショット(検査用履歴)
    pending = {};                                    \* ホスト未処理の書き込み {[id, gen]}(gen = 送信者が現行と考えた世代)
    wIdx = 1;                                        \* 次に発生する書き込み id
    retryBudget = MaxRetry;
    discBudget = MaxDisc;
    conn = [p \in Parts |-> "joined"];               \* joined / frozen(凍結)/ closed(クローズ通知処理済み)
    knownGen = [p \in Parts |-> 1];                  \* 参加者が現行と考える世代(NEXT_THREAD 受信・再接続で更新)
    pv = [p \in Parts |-> [g \in Gens |-> <<>>]];    \* 参加者の表示列(確定レスのみ — FR-008)
    chan = [p \in Parts |-> <<>>];                   \* ホスト→参加者の FIFO(TCP の in-order 配送)

  define {
    Joined == {p \in Parts : conn[p] = "joined"}
    AssignedIds == UNION {Range(log[g]) : g \in Gens}
    RoomInActive == Len(log[activeGen]) < ResLimit
    Quiescent == pending = {} /\ (\A p \in Parts : chan[p] = <<>>)

    \* ---- 検査する不変条件 ----
    TypeOK ==
      /\ activeGen \in Gens
      /\ phase \in {"active", "closed", "crashed"}
      /\ log \in [Gens -> Seq(Writes)]
      /\ frozenLen \in [Gens -> 0..ResLimit]
      /\ pending \subseteq [id : Writes, gen : Gens]
      /\ wIdx \in 1..(MaxWrites + 1)
      /\ retryBudget \in 0..MaxRetry
      /\ discBudget \in 0..MaxDisc
      /\ conn \in [Parts -> {"joined", "frozen", "closed"}]
      /\ knownGen \in [Parts -> Gens]
      /\ pv \in [Parts -> [Gens -> Seq(Writes)]]
      /\ chan \in [Parts -> Seq(Msgs)]

    \* 採番の一意性(O1): 同一イベントが二つの (gen, res_no) を得ない
    \* (再送 × 移行境界を含む。設計制約 D1 の検査対象)
    AssignedOnce ==
      \A g1 \in Gens : \A g2 \in Gens :
        \A i \in DOMAIN log[g1] : \A j \in DOMAIN log[g2] :
          (log[g1][i] = log[g2][j]) => (g1 = g2 /\ i = j)

    \* 上限超過採番なし(res_no は 1..ResLimit に収まる)
    NoOverLimit == \A g \in Gens : Len(log[g]) <= ResLimit

    \* 旧世代(移行済み = 凍結)のログは移行時点から不変(T1)
    FrozenGenStable == \A g \in Gens : (g < activeGen) => Len(log[g]) = frozenLen[g]

    \* クローズ・クラッシュ後に採番が増えない(T1)
    ClosedStable ==
      (phase \in {"closed", "crashed"}) => Len(log[activeGen]) = frozenLen[activeGen]

    \* 参加者の既知世代は現行世代を超えない(旧世代宛 = 移行境界の書き込みのみ生じる)
    KnownGenBound == \A p \in Parts : knownGen[p] <= activeGen

    \* 表示列は常にホスト確定列の接頭辞(T3/O2: 欠番・順序分裂・異内容の混入なし)
    DisplayPrefix == \A p \in Parts : \A g \in Gens : PrefixOf(pv[p][g], log[g])

    \* クローズ通知を処理した参加者はスレデータを保持しない(揮発 — FR-014/FR-015)
    DeleteOnClose ==
      \A p \in Parts : (conn[p] = "closed") => (\A g \in Gens : pv[p][g] = <<>>)

    \* 到達性: 静止状態では接続中の全参加者が現行世代の全確定情報を表示済み
    Convergence ==
      (Quiescent /\ phase = "active") =>
        \A p \in Joined : pv[p][activeGen] = log[activeGen]
  }

  process (world = "world")
  {
  Loop:
    while (TRUE) {
      either {
        \* 書き込み送信: 接続中の参加者が、自分の知る現行世代宛にレスを送る。
        \* NEXT_THREAD が未達だと旧世代宛になる(移行境界の競合の源)
        await wIdx <= MaxWrites;
        with (p \in Joined) {
          pending := pending \union {[id |-> wIdx, gen |-> knownGen[p]]};
          wIdx := wIdx + 1;
        };
      } or {
        \* 書き込み再送: 確定通知を受け取る前に切断した参加者は同じイベントを
        \* 再提出しうる(過大近似: 任意の既送信 id を任意の接続中参加者が再送する)。
        \* ホスト側の重複排除(設計制約 D1)がないと AssignedOnce が破れる
        await retryBudget > 0 /\ wIdx > 1;
        with (p \in Joined, i \in 1..(wIdx - 1)) {
          pending := pending \union {[id |-> i, gen |-> knownGen[p]]};
          retryBudget := retryBudget - 1;
        };
      } or {
        \* ホストの書き込み処理(thread-delivery.md 書き込み/移行境界規則)。
        \* 受信検証 1〜7(署名・サイズ・PoW 等)通過後の採番判定のみをモデル化
        await pending # {} /\ phase # "crashed";
        with (w \in pending) {
          either {
            \* 採番: 現行世代宛(w.gen = activeGen)は必須経路、旧世代宛
            \* (w.gen < activeGen — 移行境界)は「新スレへ採番」の選択肢。
            \* ガード w.id \notin AssignedIds = 重複排除(設計制約 D1)
            await /\ phase = "active"
                  /\ w.id \notin AssignedIds
                  /\ w.gen <= activeGen
                  /\ RoomInActive;
            with (idx = Len(log[activeGen]) + 1) {
              log[activeGen] := Append(log[activeGen], w.id);
              \* RES + ORDER(seq = idx)を全接続参加者(送信者含む)へ配布
              chan := [p \in Parts |->
                         IF conn[p] = "joined"
                         THEN Append(chan[p], OrdMsg(activeGen, idx, w.id))
                         ELSE chan[p]];
            };
            pending := pending \ {w};
          } or {
            \* 破棄: 重複(D1)・クローズ後・満杯(移行前)・旧世代宛の定型拒否。
            \* 現行世代宛かつ空きありの正当な書き込みは破棄できない(await)
            await \/ phase = "closed"
                  \/ w.id \in AssignedIds
                  \/ ~RoomInActive
                  \/ w.gen < activeGen;
            pending := pending \ {w};
          };
        };
      } or {
        \* 次スレ移行(FR-013)。トリガーは「レス上限到達」と「配信者の明示操作」の
        \* 両方 — モデルでは「Active 中の任意時点で移行しうる」に抽象化する
        \* (満杯時の移行も明示操作も同じ遷移。満杯のまま移行しない実行も許し、
        \* その場合の書き込みは定型拒否で静止に到達する)
        await phase = "active" /\ activeGen < MaxGen;
        with (ng = activeGen + 1) {
          frozenLen[activeGen] := Len(log[activeGen]);
          chan := [p \in Parts |->
                     IF conn[p] = "joined"
                     THEN Append(chan[p], [t |-> "next", g |-> ng])
                     ELSE chan[p]];
          activeGen := ng;
        };
      } or {
        \* 明示クローズ(FR-014): スレ主署名付き THREAD_CLOSE を配布し、
        \* 以後の書き込みは受理しない。未処理の書き込みは定型拒否(pending 破棄)
        await phase = "active";
        phase := "closed";
        frozenLen[activeGen] := Len(log[activeGen]);
        pending := {};
        chan := [p \in Parts |->
                   IF conn[p] = "joined"
                   THEN Append(chan[p], [t |-> "close"])
                   ELSE chan[p]];
      } or {
        \* 通知なき停止(クラッシュ・瞬断): 全参加者は TCP 断で凍結(FR-014)。
        \* 転送中メッセージ・ホスト未処理の書き込みは失われる
        await phase = "active";
        phase := "crashed";
        frozenLen[activeGen] := Len(log[activeGen]);
        pending := {};
        conn := [p \in Parts |-> IF conn[p] = "joined" THEN "frozen" ELSE conn[p]];
        chan := [p \in Parts |-> <<>>];
      } or {
        \* 参加者の切断(凍結): 転送中メッセージは失われる。表示済みデータは
        \* 保持され閲覧継続(FR-014 — DisplayPrefix が凍結中も接頭辞性を保証)
        await discBudget > 0;
        with (p \in Joined) {
          conn[p] := "frozen";
          chan[p] := <<>>;
          discBudget := discBudget - 1;
        };
      } or {
        \* 再接続(JOIN → チャレンジ検証 → WELCOME → since_seq 差分同期を原子化)。
        \* 現行世代の未受信分(since_seq = 表示済み長)をホストが再送する(FR-010)。
        \* 途中参加(表示済み 0 からの全ログ同期)も同じ遷移で表現される。
        \* 旧世代の未受信分は同期されない(凍結スレは取得済み分のみ — FR-014)
        await phase = "active";
        with (p \in {q \in Parts : conn[q] = "frozen"}) {
          with (have = Len(pv[p][activeGen])) {
            conn[p] := "joined";
            knownGen[p] := activeGen;
            chan[p] := [k \in 1..(Len(log[activeGen]) - have) |->
                          OrdMsg(activeGen, have + k, log[activeGen][have + k])];
          };
        };
      } or {
        \* 参加者の受信処理(FIFO 先頭から 1 通)
        with (p \in {q \in Parts : conn[q] = "joined" /\ chan[q] # <<>>}) {
          with (m = Head(chan[p])) {
            chan[p] := Tail(chan[p]);
            if (m.t = "ord") {
              \* O2: seq が連続するときのみ表示を進める(飛びは適用しない)。
              \* in-order 転送では飛びは生じない(防御的ガードとして規則を明示)
              if (m.i = Len(pv[p][m.g]) + 1) {
                pv[p][m.g] := Append(pv[p][m.g], m.w);
              };
            } else if (m.t = "next") {
              \* NEXT_THREAD: 以後の書き込みは新世代宛になる
              knownGen[p] := m.g;
            } else {
              \* THREAD_CLOSE: スレデータを削除(揮発 — FR-014/FR-015)
              pv[p] := [g \in Gens |-> <<>>];
              conn[p] := "closed";
            };
          };
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
VARIABLES activeGen, phase, log, frozenLen, pending, wIdx, retryBudget, 
          discBudget, conn, knownGen, pv, chan

(* define statement *)
Joined == {p \in Parts : conn[p] = "joined"}
AssignedIds == UNION {Range(log[g]) : g \in Gens}
RoomInActive == Len(log[activeGen]) < ResLimit
Quiescent == pending = {} /\ (\A p \in Parts : chan[p] = <<>>)


TypeOK ==
  /\ activeGen \in Gens
  /\ phase \in {"active", "closed", "crashed"}
  /\ log \in [Gens -> Seq(Writes)]
  /\ frozenLen \in [Gens -> 0..ResLimit]
  /\ pending \subseteq [id : Writes, gen : Gens]
  /\ wIdx \in 1..(MaxWrites + 1)
  /\ retryBudget \in 0..MaxRetry
  /\ discBudget \in 0..MaxDisc
  /\ conn \in [Parts -> {"joined", "frozen", "closed"}]
  /\ knownGen \in [Parts -> Gens]
  /\ pv \in [Parts -> [Gens -> Seq(Writes)]]
  /\ chan \in [Parts -> Seq(Msgs)]



AssignedOnce ==
  \A g1 \in Gens : \A g2 \in Gens :
    \A i \in DOMAIN log[g1] : \A j \in DOMAIN log[g2] :
      (log[g1][i] = log[g2][j]) => (g1 = g2 /\ i = j)


NoOverLimit == \A g \in Gens : Len(log[g]) <= ResLimit


FrozenGenStable == \A g \in Gens : (g < activeGen) => Len(log[g]) = frozenLen[g]


ClosedStable ==
  (phase \in {"closed", "crashed"}) => Len(log[activeGen]) = frozenLen[activeGen]


KnownGenBound == \A p \in Parts : knownGen[p] <= activeGen


DisplayPrefix == \A p \in Parts : \A g \in Gens : PrefixOf(pv[p][g], log[g])


DeleteOnClose ==
  \A p \in Parts : (conn[p] = "closed") => (\A g \in Gens : pv[p][g] = <<>>)


Convergence ==
  (Quiescent /\ phase = "active") =>
    \A p \in Joined : pv[p][activeGen] = log[activeGen]


vars == << activeGen, phase, log, frozenLen, pending, wIdx, retryBudget, 
           discBudget, conn, knownGen, pv, chan >>

ProcSet == {"world"}

Init == (* Global variables *)
        /\ activeGen = 1
        /\ phase = "active"
        /\ log = [g \in Gens |-> <<>>]
        /\ frozenLen = [g \in Gens |-> 0]
        /\ pending = {}
        /\ wIdx = 1
        /\ retryBudget = MaxRetry
        /\ discBudget = MaxDisc
        /\ conn = [p \in Parts |-> "joined"]
        /\ knownGen = [p \in Parts |-> 1]
        /\ pv = [p \in Parts |-> [g \in Gens |-> <<>>]]
        /\ chan = [p \in Parts |-> <<>>]

world == \/ /\ wIdx <= MaxWrites
            /\ \E p \in Joined:
                 /\ pending' = (pending \union {[id |-> wIdx, gen |-> knownGen[p]]})
                 /\ wIdx' = wIdx + 1
            /\ UNCHANGED <<activeGen, phase, log, frozenLen, retryBudget, discBudget, conn, knownGen, pv, chan>>
         \/ /\ retryBudget > 0 /\ wIdx > 1
            /\ \E p \in Joined:
                 \E i \in 1..(wIdx - 1):
                   /\ pending' = (pending \union {[id |-> i, gen |-> knownGen[p]]})
                   /\ retryBudget' = retryBudget - 1
            /\ UNCHANGED <<activeGen, phase, log, frozenLen, wIdx, discBudget, conn, knownGen, pv, chan>>
         \/ /\ pending # {} /\ phase # "crashed"
            /\ \E w \in pending:
                 \/ /\ /\ phase = "active"
                       /\ w.id \notin AssignedIds
                       /\ w.gen <= activeGen
                       /\ RoomInActive
                    /\ LET idx == Len(log[activeGen]) + 1 IN
                         /\ log' = [log EXCEPT ![activeGen] = Append(log[activeGen], w.id)]
                         /\ chan' = [p \in Parts |->
                                       IF conn[p] = "joined"
                                       THEN Append(chan[p], OrdMsg(activeGen, idx, w.id))
                                       ELSE chan[p]]
                    /\ pending' = pending \ {w}
                 \/ /\ \/ phase = "closed"
                       \/ w.id \in AssignedIds
                       \/ ~RoomInActive
                       \/ w.gen < activeGen
                    /\ pending' = pending \ {w}
                    /\ UNCHANGED <<log, chan>>
            /\ UNCHANGED <<activeGen, phase, frozenLen, wIdx, retryBudget, discBudget, conn, knownGen, pv>>
         \/ /\ phase = "active" /\ activeGen < MaxGen
            /\ LET ng == activeGen + 1 IN
                 /\ frozenLen' = [frozenLen EXCEPT ![activeGen] = Len(log[activeGen])]
                 /\ chan' = [p \in Parts |->
                               IF conn[p] = "joined"
                               THEN Append(chan[p], [t |-> "next", g |-> ng])
                               ELSE chan[p]]
                 /\ activeGen' = ng
            /\ UNCHANGED <<phase, log, pending, wIdx, retryBudget, discBudget, conn, knownGen, pv>>
         \/ /\ phase = "active"
            /\ phase' = "closed"
            /\ frozenLen' = [frozenLen EXCEPT ![activeGen] = Len(log[activeGen])]
            /\ pending' = {}
            /\ chan' = [p \in Parts |->
                          IF conn[p] = "joined"
                          THEN Append(chan[p], [t |-> "close"])
                          ELSE chan[p]]
            /\ UNCHANGED <<activeGen, log, wIdx, retryBudget, discBudget, conn, knownGen, pv>>
         \/ /\ phase = "active"
            /\ phase' = "crashed"
            /\ frozenLen' = [frozenLen EXCEPT ![activeGen] = Len(log[activeGen])]
            /\ pending' = {}
            /\ conn' = [p \in Parts |-> IF conn[p] = "joined" THEN "frozen" ELSE conn[p]]
            /\ chan' = [p \in Parts |-> <<>>]
            /\ UNCHANGED <<activeGen, log, wIdx, retryBudget, discBudget, knownGen, pv>>
         \/ /\ discBudget > 0
            /\ \E p \in Joined:
                 /\ conn' = [conn EXCEPT ![p] = "frozen"]
                 /\ chan' = [chan EXCEPT ![p] = <<>>]
                 /\ discBudget' = discBudget - 1
            /\ UNCHANGED <<activeGen, phase, log, frozenLen, pending, wIdx, retryBudget, knownGen, pv>>
         \/ /\ phase = "active"
            /\ \E p \in {q \in Parts : conn[q] = "frozen"}:
                 LET have == Len(pv[p][activeGen]) IN
                   /\ conn' = [conn EXCEPT ![p] = "joined"]
                   /\ knownGen' = [knownGen EXCEPT ![p] = activeGen]
                   /\ chan' = [chan EXCEPT ![p] = [k \in 1..(Len(log[activeGen]) - have) |->
                                                     OrdMsg(activeGen, have + k, log[activeGen][have + k])]]
            /\ UNCHANGED <<activeGen, phase, log, frozenLen, pending, wIdx, retryBudget, discBudget, pv>>
         \/ /\ \E p \in {q \in Parts : conn[q] = "joined" /\ chan[q] # <<>>}:
                 LET m == Head(chan[p]) IN
                   /\ chan' = [chan EXCEPT ![p] = Tail(chan[p])]
                   /\ IF m.t = "ord"
                         THEN /\ IF m.i = Len(pv[p][m.g]) + 1
                                    THEN /\ pv' = [pv EXCEPT ![p][m.g] = Append(pv[p][m.g], m.w)]
                                    ELSE /\ TRUE
                                         /\ pv' = pv
                              /\ UNCHANGED << conn, knownGen >>
                         ELSE /\ IF m.t = "next"
                                    THEN /\ knownGen' = [knownGen EXCEPT ![p] = m.g]
                                         /\ UNCHANGED << conn, pv >>
                                    ELSE /\ pv' = [pv EXCEPT ![p] = [g \in Gens |-> <<>>]]
                                         /\ conn' = [conn EXCEPT ![p] = "closed"]
                                         /\ UNCHANGED knownGen
            /\ UNCHANGED <<activeGen, phase, log, frozenLen, pending, wIdx, retryBudget, discBudget>>
         \/ /\ Quiescent
            /\ TRUE
            /\ UNCHANGED <<activeGen, phase, log, frozenLen, pending, wIdx, retryBudget, discBudget, conn, knownGen, pv, chan>>

Next == world

Spec == /\ Init /\ [][Next]_vars
        /\ WF_vars(Next)

\* END TRANSLATION

\* 到達の終端保証: いかなる公平な実行でも、最終的に静止状態に到達し続ける
EventuallyQuiescent == <>[]Quiescent

==============================================================================

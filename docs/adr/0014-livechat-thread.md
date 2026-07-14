# ADR-0014: 配信実況スレ(P2P 掲示板)— 脅威追加・Principle V 判定・kind 採番

**Status**: Accepted
**Date**: 2026-07-12
**Principles**: Principle I (Safety First), Principle II (Security by Design), Principle V (Formal Verification for Critical Paths), Principle VI (Principle Traceability)
**Task**: T002(006 Phase 1 実装前ゲート)。PlusCal モデルは T003、TLC 検査は T004(結果は本 ADR §TLC 検査結果へ追記する)。**T060(2026-07-14)で最終化** — 実装で確定した設計判断・逸脱(§5)と SJIS 仮説の状態(§6)を追記した

## 背景

006-livechat-thread は、PeerCast 配信のリアルタイム実況を中央サーバーなしの
スレッドフロート型掲示板として提供する(spec / plan)。発見 = 既存 gossip への
kind 31311 announce 相乗り、配送 = ホスト(= 配信者)直結の星型でホストが
シーケンサとして採番・順序確定情報(kind 21311)を配布、表現 = 各端末が確定順序から
掲示板を再構成する 3 層構成である。

plan §Constitution Check は本機能のゲート通過条件として、(a) 脅威モデルへの追加を
ADR に記録すること、(b) Principle V の該当/非該当判定理由を ADR に明記すること、
(c) kind 31311/21311 の採番根拠を記録すること — を課した。本 ADR はその 3 点を確定する。

## 1. 脅威モデルへの追加(ADR-0004 への追記)

ADR-0004 の多層緩和(署名検証必須・ミュート・ピア選別・任意 PoW・資源上限)は
gossip 面(announce)にそのまま適用される。本機能で新たに生じる脅威と緩和を追加する:

| # | 脅威 | 内容 | 緩和(多層) | 根拠 |
|---|------|------|-------------|------|
| A1 | **announce 反射攻撃** | 攻撃者が第三者のアドレスを `tip` に記載した偽 announce を伝搬させ、多数の視聴者ノードに一斉接続させる(発見網を反射攻撃の母体化) | (1) 署名者 = 対象チャンネルの掲載ペルソナ一致必須 — 不一致は不可視 + `livechat_announce_invalid`(FR-003)。(2) **announce 受信のみでは接続しない** — 接続は利用者の明示操作起点(FR-004 / SC-005)。(3) 接続時チャレンジ — スレ主ペルソナ鍵の保持を Schnorr 署名で検証、失敗は切断 + `livechat_challenge_failed` + 指数バックオフ(FR-005)。三層により「偽 announce を伝搬させても、被指定アドレスへの接続は人数 × 明示操作に限られ、接続しても 1 回のチャレンジ失敗で止まる」 | spec US3 / contracts/thread-events.md |
| A2 | **偽 ORDER(順序確定情報の偽造・改竄)** | スレ主以外が偽の採番結果を配布し、端末間のレス番号・アンカー解決を分裂させる(SC-002 の破壊) | ORDER はスレ主ペルソナの署名必須 — 不一致は破棄 + `livechat_order_invalid`(FR-011)。参加者側検証はサイズ → 署名 → スレ主一致 → seq 連続性 → res_no 連続性の順(contracts/thread-events.md)。seq 欠落時は表示を進めず再送要求(不変条件 O2) | spec US3 / data-model 不変条件 O1/O2 |
| A3 | **荒らし(書き込み洪水・鍵使い捨て・接続占有)** | 大量書き込み・板鍵を捨てながらの BAN 回避・接続枠(128)の占有 | (1) レート: `thread_write_rate`(板鍵単位)+ `thread_msg_rate`(接続単位・制御込み)、違反は破棄 + `livechat_write_rejected`、継続で切断(FR-021)。(2) 初見板鍵に `first_post_pow_bits`(既定 20 bits — NIP-13)の初回 PoW — 鍵使い捨てに累積コストを課す(research R6)。(3) BAN は採番拒否 — 理由非開示の定型応答で荒らしへの情報提供を避ける(FR-018/FR-019)。(4) 参加上限超過は定型 `THREAD_REJECT(full)`(FR-006) | spec US3/US4 / contracts/thread-delivery.md §防御 |
| A4 | **互換 API の抜け道化** | loopback の互換 API(トークンなし)を経由した検証バイパス・LAN 露出 | 専用リスナー分離(既定 `127.0.0.1:7183`・非 loopback バインドは起動拒否)+ Host 検証 + レート + ボディ上限。書き込みは通常経路と**同一の検証**を通す(FR-026/FR-028 — 抜け道禁止)。ADR-0012(index.txt)と同型の構成 | research R5 / contracts/compat-api.md |
| A5 | **スレ主(マスター)の身元露出** | Winny2 BBS の致命的欠陥 = マスター匿名性の破綻の再来 | 構造的回避: スレ主を**配信者(身元が元々公開の主体)に限定**する(FR-001)。匿名スレ主の汎用分散 BBS はスコープ外と明記(spec 背景)。参加者側は板鍵(ペルソナと構造分離・エクスポートなし — research R8)で身元をリンクさせない | spec 背景 / docs/research/winny2-bbs.md |

残余リスク: 板鍵の固定 ID は板内での長期追跡を許す(緩和 = 明示ローテーション —
spec Assumptions に受容記録)。31311 の `tip` は第三者がリレーへ再公開しうるが、
30311 と同じく元々公開の掲載情報であり追加露出はない(受容 — thread-events.md §NIP 適合)。

## 2. Principle V 判定(形式的検証)

### 該当: ホスト = シーケンサの中核状態機械(research R9)

採番・順序確定情報の配布・次スレ移行(レス上限到達と配信者明示操作の両トリガー —
FR-013)・凍結/クローズを対象とし、3 基準すべてを満たすため**該当**と判定する:

| 基準 | 判定 | 根拠 |
|------|------|------|
| ① 新規設計であり、既存の実績ある仕様・ライブラリの単純な利用ではない | **該当** | ホスト採番 + 署名付き順序確定の配布 + スレッドフロート移行は自前プロトコル。Winny2 BBS(マスターモデル)は先行事例だが仕様・実装とも流用しない(spec 背景) |
| ② 競合状態・デッドロック・プロトコル違反がテストで再現困難な非自明さを持つ | **該当** | 「同時書き込み × レス上限到達 × 次スレ移行 × 切断/再接続(since_seq 差分同期)× 書き込み再送」の相互作用から生じる競合(移行境界の二重採番・欠番・上限超過・再同期漏れ)はタイミング依存の創発的性質であり、ユニット/統合テストでの網羅は困難 |
| ③ 失敗がユーザー安全(Principle I)またはデータ整合性に直接影響する | **該当** | 採番の重複・欠番・順序分裂は全端末のレス番号・アンカー解決の不一致(SC-002 = 本機能の存在理由)としてデータ整合性を直接破壊する |

該当のため、PlusCal モデル
[docs/formal/livechat_sequencer.tla](../formal/livechat_sequencer.tla) を作成し(T003)、
TLC でデッドロック・不変条件・ライブネスを検査してから(T004)、シーケンサ実装
(tasks T030)に着手する(実装前ゲート — Principle V MUST)。

検査する性質(tasks T003 / data-model の不変条件との対応):

| 性質 | モデル上の定式化 |
|------|------------------|
| 採番の一意性・移行境界の二重採番なし(O1) | `AssignedOnce`: どのイベントも高々 1 つの (gen, res_no) にしか採番されない(再送・世代跨ぎを含む) |
| 欠番なし単調増加(T3) | ログを列で表現し `DisplayPrefix`: 各参加者の表示列は常にホスト確定列の接頭辞(= res_no の飛び・順序分裂なし) |
| 上限超過採番なし | `NoOverLimit`: 各世代の確定数 ≤ res_limit |
| 凍結・クローズ後の採番なし(T1) | `FrozenGenStable` / `ClosedStable`: 旧世代・クローズ後のログ長が移行/クローズ時点から不変 |
| クローズの揮発(FR-014/FR-015) | `DeleteOnClose`: クローズ通知を処理した参加者のスレデータは空 |
| 確定情報の全接続参加者への到達(ライブネス) | `Convergence`(静止状態で接続中参加者の表示列 = ホスト確定列)+ `EventuallyQuiescent`(公平な実行は必ず静止に到達) |
| デッドロック不在 | TLC 既定検査(静止状態の自己ループのみ許容 — ADR-0005 と同型) |

### 非該当: 上記以外の本機能構成部分(research R9)

| 対象 | 理由 | 代替担保 |
|------|------|----------|
| 互換 API(subject.txt / dat / SETTING.TXT / bbs.cgi) | HTTP リクエスト/レスポンスの単純写像で状態機械の競合がない(基準②不成立) | 契約テスト(tests/contract/compat_bbs.rs)・SC-007 実機確認 |
| 板設定の配布(SETTINGS) | 単一送信者(板主)の置換配布で競合が単純(基準②不成立)。名無し名の確定時点固定は dat 出力側の規則 | 契約テスト・cucumber |
| announce の伝搬 | 既存 gossip 状態機械(ADR-0005 で検証済み)への kind 追加のみ(基準①不成立) | 既存モデルの結論を継承 + 契約テスト(gossip 検査 #7/#8 の追加分岐) |
| イベント受信検証(署名・サイズ・PoW・レート・BAN) | 実績ライブラリ + 逐次検査(基準①②不成立)。ADR-0005 の同判定と同じ | 契約テスト(thread_events / thread_delivery のネガティブ) |
| チャレンジ認証・バックオフ | 単発の要求応答 + ノード局所状態で競合が単純(基準②不成立) | 契約テスト(T018/T035) |

## 3. kind 採番の根拠(31311 / 21311)

- **kind 1311(レス)**: NIP-53 "Live Chat Message" の予約定義(001
  contracts/nostr-events.md が将来フェーズとして固定)の履行(research R1)。
  採否の列挙は contracts/thread-events.md §NIP 適合を正とする
- **kind 31311(スレ announce)**: addressable 範囲(30000–39999)— 置換規則
  `(kind, pubkey, d)` が「板ごとに最新 announce 1 件」の要件と一致し、`expiration`
  鮮度管理も 30311 と同一規則を再利用できる。番号は 1311 との対応が読み取れる 31311 を選定
- **kind 21311(順序確定情報)**: ephemeral 範囲(20000–29999)— 「保存しない」意味論が
  揮発性(FR-015)と一致する。gossip には流さず(MUST NOT)、スレ配送セッション内のみで伝送
- **一次資料確認(T001 — 2026-07-12)**: nostr-protocol/nips `master` の Event Kinds
  レジストリを直接取得して突合し、**21311 / 31311 とも未割当**であることを確認した。
  213xx・313xx 帯の kind は存在せず、20000 番台にも個別割当はない。NIP-53 の割当は
  1311 / 10312 / 30311 / 30312 / 30313 の 5 種で近接衝突なし。
  thread-events.md §NIP 適合の改番基準(割当が実況・チャット近接領域で誤解釈リスクを
  持つ場合のみ `livechat2` で改番 MUST)に照らし、**改番不要**と判断する
  (詳細記録: specs/006-livechat-thread/research.md R2)
- 両 kind とも **peca 固有 kind であり NIP 互換を主張しない**。将来の割当衝突への対応
  基準は contracts/thread-events.md §NIP 適合に規定済み(リレー非接続のため実害は
  命名衝突のみ — 受容)

## 4. モデル化で導出した設計制約(T003 — 実装 T030 が守るべき規則)

ADR-0005 の「設計制約の発見」と同じく、モデル作成の過程で契約に明記されていない
規則が 2 点見つかった(詳細はモデルのヘッダコメント参照):

- **D1: ホストは採番前に event_id の重複を板単位(世代を跨いで)で排除しなければ
  ならない (MUST)**。導出: 参加者は順序確定情報(ORDER)を受け取る前に切断されると
  同じイベントを再送しうる(thread-events.md の受信検証 1〜7 に重複排除の段がない)。
  重複排除がないと同一イベントが二つの res_no を得て採番の一意性(O1)が破れる。
  世代を跨ぐのは、旧世代末尾で確定したイベントの再送が次スレで再採番されるケースを
  塞ぐため。モデルでは採番ガード `w.id \notin AssignedIds` が対応し、TLC はこの
  ガードの下で `AssignedOnce` を検査する(T004)
- **D2: 移行境界の「新スレへ採番」を選ぶ場合、イベントの thread タグは旧 gen の
  まま**である(署名済みで書き換え不能)。採番先スレと thread タグの不一致を参加者側
  検証が許容するか、旧世代宛を常に定型拒否するかを T030 で確定すること(モデルは
  契約が許す両挙動とも安全性が保たれることを検査する)

## TLC 検査結果(T004 — 2026-07-12)

**結果: パス(エラーなし)** — シーケンサ実装(T030)のゲート条件を満たす。

- 実行: TLC2 Version 2.19(tla2tools)、12 ワーカー。
  `java -XX:+UseParallelGC tlc2.TLC -workers auto -config livechat_sequencer.cfg livechat_sequencer.tla`
  (ログ: [docs/formal/livechat_sequencer.log.txt](../formal/livechat_sequencer.log.txt))
- 定数(検査対象シナリオが生じる最小値): MaxGen=2, ResLimit=2, MaxWrites=3(= ResLimit+1
  — 上限到達 + 境界越えに必要な最小), MaxRetry=1, MaxDisc=1
- 検査した不変条件(9): TypeOK / AssignedOnce / NoOverLimit / FrozenGenStable /
  ClosedStable / KnownGenBound / DisplayPrefix / DeleteOnClose / Convergence
- 検査した時相性質: EventuallyQuiescent(公平な実行の静止到達)+ TLC 既定の
  デッドロック検査(違反なし)
- 状態空間: 610,015 状態生成 / **108,626 到達状態**、探索深さ 20、所要 3 秒。
  フィンガープリント衝突確率(楽観推定)3.0E-9
- 発見事項: TLC 検査での新規違反なし。モデル化段階で導出した設計制約 D1/D2(§4)が
  本検査の前提ガードとして成立していることを確認(D1 の重複排除ガードの下で
  AssignedOnce が全状態で成立)
- 備考: SYMMETRY(p1/p2 圧縮)はライブネス検査との併用が不健全なため不使用

## 5. 実装で確定した設計判断・逸脱(T060 — 2026-07-14)

実装(Phase 2〜10)を通じて、§4 の未確定点(D2)の決着と、契約に対する受容済み逸脱が
確定した。いずれもモデルの安全性(§TLC 検査結果)を損なわない範囲に収める。

- **D2 の決着: 移行境界は「常に定型拒否」を選ぶ(`accept_write`)**。モデルは「新スレへ
  採番」「定型拒否」の両方が安全と検査済みだが、実装は**常に定型拒否**を採る。理由:
  書き込みイベントは署名済みで `thread` タグ(board_id・gen)を書き換えられないため、
  「新スレへ採番」を選ぶと採番先スレの gen とイベント内 gen が食い違い、参加者側の
  ORDER 検証(スレ主一致 + 対象スレ世代一致)で「別スレの ORDER」として拒否されうる。
  常に定型拒否なら参加者は `NEXT_THREAD` 受信後に再送すればよく、より単純・安全側に
  倒せる(src/livechat/registry.rs `accept_write` 手順 1 の意図コメント)。
- **板スコープ状態の世代跨ぎ引き継ぎ(D1 の対称)**。`known_board_keys`(初回 PoW 判定)・
  `write_windows`(レート窓)・BAN/ConnBan・板設定は**板 = ペルソナ単位のスコープ**であり
  スレ(世代)に依存しないため、次スレ移行後も保持する。research R6 は「板鍵が当該**板**で
  未知なら first_post_pow_bits を要求」と規定しており世代単位ではない — 移行のたびに初見
  扱いへ戻すと移行済みの書き込み者へ不要な PoW 再計算を強いる(`migrate_to_next_generation_locked`)。
- **Last-Modified はホスト受信時刻で単調化**。投稿者(参加者の板鍵)が申告する
  `Res.created_at` は未検証(ホスト検証 1〜7 に時刻検査なし)であり、過去日時申告で
  互換 API の dat `Last-Modified` を後退させるキャッシュ汚染を許すため、確定時に
  `max(既存値, ホスト受信時刻)` で更新する(A3 の派生対策 — T055 レビュー)。
- **互換 API のリモート板対応(Phase 8 逸脱の解消 — T069)**。Phase 8 では互換 API を
  自ノードホスト板限定として受容していたが、Phase 10 で参加者セッション(T064)を
  常駐化したため、**「スレを開く」で常駐セッションを維持している他ノード板(接続中・
  凍結中)も互換 API で解決**する(compat-api.md §板の URL 対応に一致)。ただし参加者
  セッションは前世代を保持しないため、**リモート板の dat は現行世代のみ**(旧世代 key は
  404 — compat-api.md の「保持していない dat は 404」の許容範囲。自板は直近 1 世代を
  保持し従来どおり)。合成した `Last-Modified` は確定レス申告 created_at の最大値に依存する
  ベストエフォート(ホスト時計を持たないリモート視聴側の制約 — src/web/compat/mod.rs
  `session_view_to_snapshot`)。
- **tip(announce のホスト到達アドレス)の導出**。スレ開設時の `tip` は「掲載中 30311
  チャンネルの tracker の IP + 自ノードの P2P `listen_port`」で合成する(利用者確定方針)。
  tracker 無し(firewalled)・P2P 待受無し(`listen_port=0`)は開設不可(視聴者へ到達
  アドレスを提示できないため — src/main.rs `LivechatAdapter::derive_tip`)。

## 6. SJIS 仮説の状態(T060 / T062 未了)

互換 API は encoding_rs(CP932)で全応答を Shift_JIS エンコードし、変換不能文字は数値
文字参照(`&#dddd;`)で保全、受理時は数値文字参照(`&#dddd;` / `&#xhhhh;`)を展開する
(contracts/compat-api.md §受け口 / T053)。この設計は「**既存の専ブラ・コメントビューワが
CP932 + 数値文字参照 + 2ch 形式(subject.txt / dat / SETTING.TXT / bbs.cgi)を期待する**」
という仮説(research R5)に立つ。

- **自動検証済み(契約レベル)**: SJIS 往復・数値文字参照保全・実体参照エスケープの
  一意規則・dat 追記不変性・loopback 外/Host 不正の定型拒否は契約テスト
  (tests/contract/compat_bbs.rs)+ 各モジュールの `#[cfg(test)]` で担保している。
- **未検証(実機)**: 仮説の最終確証 = **利用者所有の実況ツール一式**に対する SC-007 実機
  確認(T062 — hex64 板名の登録可否・SJIS/数値文字参照描画・Range/304 の噛み合い・dat
  落ち解釈・SETTING.TXT キー突合・head.txt 表示・確認画面なし書き込み・固定 ID の NG 機能の
  8 観点)は**利用者の協力が必要なため未実施**(本タスク群のスコープ外)。
- **判断**: 現状は「契約テストで裏付けた実装 + 実機未検証の仮説」であり、リリース前に
  T062 を実施して仮説を確証すべき(不成立項目は追加タスク化 — tasks.md T062 の手順に従う)。
  実機検証の結果は specs/006-livechat-thread/research.md R5 へ追記する。

## 成果物と実装ゲート

- PlusCal モデル: [docs/formal/livechat_sequencer.tla](../formal/livechat_sequencer.tla)
  (TLC 設定: 同 `.cfg`)— T003
- シーケンサ実装(tasks T030)は TLC 検査(T004)のパス後に着手する(Principle V —
  tasks.md Phase Dependencies に明記済み)
- 実装が採番・移行・凍結/クローズの規則を変更する場合は、モデルを先に更新し
  再検査してから実装する(ADR-0005 と同じ運用)

## 原則参照

- Principle I: 反射攻撃の母体化防止(A1)・順序分裂の防止(A2)・スレ主匿名性問題の構造的回避(A5)
- Principle II: 多層検証の共有・チャレンジ認証・PoW/レート・互換 API の抜け道禁止(A3/A4)
- Principle V: 判定基準 3 点の充足判定と理由の ADR 化(MUST)・実装前ゲート
- Principle VI: 本 ADR・contracts・data-model の相互参照
- 入力: spec US3/US4・plan §Constitution Check・research R1/R2/R5/R6/R8/R9・ADR-0004/0005/0012

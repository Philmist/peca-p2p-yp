# ADR-0013: PEX 破棄の良性/不審分類とセキュリティイベント記録条件

**Status**: Accepted(2026-07-09 ユーザー指示による実装 / スコープは 2026-07-08 Clarification で確定)
**Date**: 2026-07-09
**Principles**: Principle I (Safety First), Principle II (Security by Design),
Principle IV (Behavior-Driven Testing), Principle V (Formal Verification),
Principle VI (Principle Traceability)
**Refines**: specs/001-nostr-p2p-yp/contracts/p2p-gossip.md 検査 5 の「違反時ログ」条件

## 背景

受信 `PEERS`(ピア交換)検証(contracts/p2p-gossip.md 検査 5)は、自アドレス・重複・
件数超過・形式不正・長さ超過・ホスト名を破棄し、破棄が 1 件でもあると `pex_rejected`
セキュリティイベント(WARN)を記録していた。

しかし健全な網では「自ノードを知っている全ピア」が PEX で自ノードのアドレスを送り返すため、
**自己アドレスの反射は接続のたびに常時発生する正常な現象**である。dual-stack 環境では
同一ノードが複数表記(IPv4 / IPv6)で候補化され、バッチ内重複も自然に生じる。これらを
`pex_rejected` として記録し続けると、セキュリティログが良性イベントで埋もれ、真に不審な
内容(件数超過・形式不正・ホスト名等)の検知信号としての価値が損なわれる(偽陽性による
監視の実質的無力化 — Principle I / II のセキュリティ監視の形骸化)。

VPS 2 ノードでの実運用ログで、着信ピアごとに自ノードの v4/v6 アドレスが反射され
`pex_rejected` が常時発火することを確認した。これが本 ADR の直接の契機である。

要件は specs/005-pex-self-reject/spec.md、設計は同 plan.md / research.md / data-model.md /
contracts/pex-rejection-classification.md を正とする。

## 決定

1. **破棄理由を良性/不審に二分する**: `validate_incoming_peers` の戻り値 `IncomingPex` に
   `benign_rejected` と `suspicious_rejected` の 2 リストを持たせ、破棄を理由で分類する。
   - **良性**(benign): 自己アドレス一致・バッチ内 canonical 重複。健全な網で常時発生する
     反射・dual-stack 重複。
   - **不審**(suspicious): 件数超過(バッチ全破棄)・`parse_addr` 失敗(形式不正・長さ超過・
     ブラケットなし複数コロン)・ホスト名(ADR-0010 名前空間分離違反)。protocol 逸脱・
     不正入力で攻撃兆候になりうる。
2. **記録条件を不審の有無に変える**: `pex_rejected` セキュリティイベントは**不審な破棄が
   1 件以上あるときのみ**記録する(MUST)。良性のみ(または破棄ゼロ)のときは記録しない
   (MUST NOT)。**良性と不審が混在する場合は不審があるため記録する**(見逃さない)。
3. **良性破棄は debug へ格下げする**: 良性破棄があったときは `tracing::debug!`(target
   `p2p`)で接続元 `source` と良性破棄件数のみを出力する。既定 INFO では出力されず、
   `RUST_LOG=p2p=debug` 有効時のみ観測できる。
4. **防御は不変**: 良性/不審いずれの破棄も、候補に登録しない破棄挙動(検査 5 の防御)は
   変更前と完全に同一とする(MUST NOT change)。本 ADR は**ログ分類のみ**を規定する。

## Security Requirements との整合

Constitution §Security Requirements は「接続拒否・不正なリクエスト・認証失敗はログに記録
しなければならない」と定める。自己アドレス反射・重複は**不正なリクエストではなく**、健全な
網の正常な動作の一部である。したがってこれを記録対象から外すことは本要件と矛盾しない。
不正な内容(件数超過・形式不正・ホスト名)は引き続き 100% 記録され、要件は維持される。

本変更はセキュリティイベントの記録条件を変える「セキュリティに関わる変更」であり、
`docs/adr/security-review-checklist.md` の適用結果を
`specs/005-pex-self-reject/checklists/security.md` に記録する(Principle III / 実装中ゲート 6)。

## Principle IV(自動テスト検証)の適用範囲

良性(自己アドレスのみ→無記録・重複のみ→無記録)・不審(件数超過/形式不正/ホスト名→記録)・
混在(良性+不審→記録)の各ネガティブシナリオを、`validate_incoming_peers` の単体テスト・
契約テスト(`tests/contract/pex.rs`)と Gherkin(`tests/features/security.feature`)で
自動検証する(FR-006)。回帰として、いずれのケースでも `accepted`(候補登録)集合が
変更前と同一であることをアサートする(FR-004 / SC-004)。

## Principle V(形式的検証)の判断

本機能は**クリティアル非該当**とする。クリティカル 3 基準(新規設計・非自明な並行性・
安全/整合性への直接影響)をいずれも満たさない —

- **新規性なし**: 新規の並行アルゴリズム・プロトコル状態機械を導入しない。既存の同期的
  検証結果(`IncomingPex`)に対するログ分類の分岐追加にすぎない。
- **並行性なし**: 分類は純粋関数 `validate_incoming_peers` 内に閉じる。呼び出し側
  (`runtime.rs` の `Message::Peers`)は記録先の分岐を増やすのみで、新規の共有状態・
  ロック・タスクを持ち込まない。
- **安全/整合性への直接影響なし**: 防御(破棄)の挙動は不変。変わるのはログ分類のみ。

よって PlusCal/TLA+ モデルは作成しない。

## 否定した選択肢

- **良性・不審で別カテゴリのセキュリティイベントを作る** — data-model に新カテゴリ追加が
  必要で過剰。良性は「セキュリティイベントではない」が正しい切り分け(記録の有無で表現する)。
- **呼び出し側(`runtime.rs`)で `is_self` を再チェックして分岐** — 検証関数と破棄判定が
  二重化し、count>64 の全破棄や混在時の判定が煩雑になり乖離の温床。破棄理由を知っている
  検証関数に分類を持たせる。
- **良性化の対象を自己アドレスのみに限る** — 重複も健全な網(とくに dual-stack)で日常的に
  発生する良性現象であり、同じ扱いが妥当(2026-07-08 Clarification で自己アドレス+重複に確定)。
- **件数超過も良性に含める** — 正当なピアは 64 件以下しか送らない。超過は protocol 逸脱で
  攻撃兆候になりうるため不審に残す。

## 帰結

- contracts/p2p-gossip.md 検査 5 の「違反時ログ」を精緻化し、良性/不審分類 invariant を追記する
- data-model.md の `pex_rejected` 行を「不審な破棄のみ記録」へ更新する
- `CONTEXT.md` の SecurityEvent 記述に乖離があれば追随する
- 実装は `src/p2p/pex.rs`(分類)と `src/p2p/runtime.rs`(記録分岐)に閉じる

## 原則参照

- Principle I: 偽陽性の除去でセキュリティ監視の実効性を回復。不審な破棄の記録は一切減らさない
  (混在時も記録)ことで見逃しを防ぐ
- Principle II: 入力検証・破棄判定は不変。debug ログの自ノードアドレスは自分自身のみで情報
  漏洩なし(FR-007)。`pex_rejected` の `source` / `detail` は従来どおり内部情報を漏洩しない
- Principle IV: 良性・不審・混在のネガティブシナリオと `accepted` 回帰の自動テスト(本 ADR)
- Principle V: クリティカル非該当の判断と理由の記録(本 ADR)
- Principle VI: specs/005-pex-self-reject(spec / plan / research / data-model / contracts)と
  本 ADR の相互参照

# Security / Privacy Requirements Checklist: 掲載前のペルソナ選択

**Purpose**: 配信中ロック・リンク推定防止・selected ガード・バックエンド強制に関する**要件そのものの品質**(完全性・明確性・一貫性・測定可能性・網羅性)をリリースゲート観点で検査する。実装の動作確認ではなく、要件が正しく書けているかを検証する「英語のユニットテスト」。
**Created**: 2026-07-07
**Feature**: [spec.md](../spec.md) | Base: [ADR-0011](../../../docs/adr/0011-broadcasting-lock.md), [ADR-0004 §7](../../../docs/adr/0004-threat-model.md)

## Requirement Completeness(プライバシー保護要件の網羅)

- [x] CHK001 「配信中ペルソナ入替の禁止」が測定可能な成功基準として定義されているか [Completeness, Spec §SC-005]
- [x] CHK002 ロック対象の破壊的操作(切替/破棄/アーカイブ)が漏れなく列挙されているか [Completeness, Spec §FR-005]
- [x] CHK003 配信中でも許可される操作(selected の label 変更・他ペルソナ操作)が明示的に定義されているか [Completeness, Spec §FR-006/§FR-007]
- [x] CHK004 selected が後から archived / unusable になった場合の要件(未選択相当・警告・掲載保留)が定義されているか [Completeness, Spec §FR-011]
- [x] CHK005 選択可能条件(active かつ usable)がバックエンド拒否要件として定義されているか [Completeness, Spec §FR-002]
- [x] CHK006 リンク推定の観測情報(接続元 IP・トラッカー IP)に対する匿名性保証の**限界**が要件として記載され、過度な期待を与えない旨が明示されているか [Completeness, ADR-0004 §7]

## Requirement Clarity(曖昧語の定量化・定義)

- [x] CHK007 「配信中」が「1 つ以上のチャンネルが実際にネットワークへ発行中」と一意に定義され、保留(未発行)を含めない旨が明確か [Clarity, Spec §FR-008]
- [x] CHK008 「usable(利用可能)」が「鍵復号が成功する」と定義され、archived と区別されているか [Clarity, Spec §FR-002, data-model §Persona]
- [x] CHK009 「即時反映(体感的に待たされずに)」が主観語のままか、検証可能な基準として扱えるよう補足されているか [Ambiguity, Spec §SC-001]
- [x] CHK010 「未選択相当(None)」の扱いが、設定値の消去ではなく都度判定であることを含めて明確に定義されているか [Clarity, data-model §Selected Persona]
- [x] CHK011 「selected ペルソナのみをロック対象とする」の範囲が、非 selected ペルソナへの操作と明確に線引きされているか [Clarity, Spec §FR-007]

## Requirement Consistency(要件間の整合)

- [x] CHK012 FR-005(配信中の切替拒否)と FR-012(切替に確認ダイアログを設けない)が、対象状況の違いで矛盾なく成立しているか [Consistency, Spec §FR-005/§FR-012]
- [x] CHK013 FR-011(selected の archived → 未選択相当)と、既存 selected() の破棄済みのみ None 扱いだった挙動との差分が data-model で一貫して記述されているか [Consistency, data-model §Selected Persona]
- [x] CHK014 FR-004(自動選択)と FR-002(選択可能条件)が、作成直後の active+usable で矛盾なく両立する旨が示されているか [Consistency, Spec §FR-004/§FR-002]
- [x] CHK015 ADR-0011 の「selected 凍結」方針と、掲載パイプライン非変更(FR-015)の主張が整合しているか(周期再発行の再解決が凍結下で同一値になる根拠) [Consistency, Spec §FR-015, ADR-0011 決定 §1]

## Acceptance Criteria Quality(成功基準の測定可能性)

- [x] CHK016 SC-002(配信中の切替/破棄/アーカイブ成功率 0%)が UI 経由・直接 API 経由の双方で測定対象と明記されているか [Measurability, Spec §SC-002]
- [x] CHK017 SC-005(リンク推定シグナルを生じない)が「同一チャンネル上で旧 ended→新 live が発生しない」という観測可能事象へ翻訳されているか [Measurability, Spec §SC-005]
- [x] CHK018 SC-003(停止後は追加リセットなく再操作可能)の「追加リセットなし」が検証可能な条件として定義されているか [Measurability, Spec §SC-003]
- [x] CHK019 SC-004(選択中ペルソナが常に判別可能・未表示状態が発生しない)が未選択/警告状態の明示を含めて測定可能か [Measurability, Spec §SC-004]

## Scenario / Edge Case Coverage(異常系・境界の要件)

- [x] CHK020 PCP 異常切断・強制終了でもロックが確実に解除される要件(ended 経路で配信中状態が残らない)が定義されているか [Coverage, Spec §FR-009, edge case]
- [x] CHK021 UI 表示が古いまま制限操作を送る競合(TOCTOU)時の要件(拒否+理由提示+画面最新化)が定義されているか [Coverage, Spec edge case]
- [x] CHK022 selected ペルソナが配信中に外部要因(鍵消失)で unusable 化した場合の扱いが、ロック対象外・保留フォールバックとして定義されているか [Coverage, research R5]
- [x] CHK023 ペルソナ 0 個・選択対象なしの状態の要件が定義されているか [Coverage, Spec §FR-013]
- [x] CHK024 破棄済みペルソナが selected として残存した場合の未選択相当の扱いが定義されているか [Edge Case, Spec edge case]

## Backend Enforcement(UI のみの防御禁止 — Principle II)

- [x] CHK025 「UI 無効化だけでなくバックエンドでも拒否」が要件として MUST 化され、UI 無効化が best-effort に留まる旨が明確か [Clarity, Spec §FR-002/§FR-005, ADR-0011 決定 §1]
- [x] CHK026 エラー応答が定型コード `{"error":"<code>"}` のみで内部情報を漏らさない要件が本機能の新規コードにも適用される旨が示されているか [Completeness, Principle II, contracts §5]
- [x] CHK027 破壊的操作(delete/archive/export)の既存確認要件が本機能で維持される旨が明示されているか [Consistency, Spec §FR-012]

## Formal Verification 判定(Principle V・VI)

- [x] CHK028 Principle V のクリティカル 3 基準の充足判定と非該当理由が ADR に記録されているか [Traceability, ADR-0011 §Principle V]
- [x] CHK029 非該当時の代替担保(予約→署名順序の並行性統合テスト・cucumber ネガティブ)が要件として定義されているか [Completeness, ADR-0011 §Principle V, quickstart]
- [x] CHK030 判定の再評価トリガ(ロック方式変更・per-channel 割当導入等)が記載されているか [Coverage, ADR-0011 §Principle V]

## Notes

- Check items off as completed: `[x]`
- 各項目は「要件が正しく書けているか」を検査する。実装の動作確認(テスト実行結果)ではない。
- `[Gap]` は spec/plan/contracts に該当記述が欠けていないかの検査、`[Ambiguity]`/`[Conflict]` は明確化/矛盾解消が必要な箇所。

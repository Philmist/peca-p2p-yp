# API Contract Requirements Checklist: 掲載前のペルソナ選択

**Purpose**: 本機能が追加/変更する API 契約(PUT/DELETE personas のロック・選択ガード、GET /status の broadcasting、エラーコード)の**契約記述そのものの品質**(完全性・明確性・一貫性・測定可能性)をリリースゲート観点で検査する。エンドポイントの動作確認ではなく契約が正しく書けているかの検証。
**Created**: 2026-07-07
**Feature**: [spec.md](../spec.md) | Contract: [contracts/local-api.md](../contracts/local-api.md) | Base: [001 local-api](../../001-nostr-p2p-yp/contracts/local-api.md)

## Requirement Completeness(契約の網羅)

- [ ] CHK031 `PUT /personas/{pubkey}` の追加拒否条件(archived/unusable/配信中)が全パターン列挙されているか [Completeness, contracts §1]
- [ ] CHK032 `DELETE /personas/{pubkey}` の配信中ロック条件(対象が selected かつ配信中)が定義されているか [Completeness, contracts §2]
- [ ] CHK033 `GET /status` への `broadcasting` フィールド追加が応答スキーマとして定義され、未配線時の既定値(false)まで記載されているか [Completeness, contracts §3]
- [ ] CHK034 本機能で新規となるエラーコード(`broadcasting_locked`・`persona_not_selectable`)が一覧化され、既存 `persona_unusable` の再利用が明示されているか [Completeness, contracts §5]
- [ ] CHK035 `GET /personas` を変更しない(selected 表示は既存フィールドから導出)方針が契約に明記されているか [Completeness, contracts §4]

## Requirement Clarity(契約の一意性)

- [ ] CHK036 各拒否条件に対応する HTTP ステータス(409/422/404)が一意に対応づけられ、曖昧さがないか [Clarity, contracts §1/§5]
- [ ] CHK037 複数フィールド同時指定時の適用順(label→state→select→channel_id)と「最初に失敗したガードのステータスを返す」挙動が明記されているか [Clarity, contracts §1]
- [ ] CHK038 「別ペルソナへの切替は配信中一律 409」という select の扱いが、data-model のマトリクス注記と齟齬なく明確か [Ambiguity, contracts §1, data-model §操作マトリクス]
- [ ] CHK039 `broadcasting` の定義が「発行中(BroadcastState 非空)」であり保留チャンネルを含めない旨が status 契約に明記されているか [Clarity, contracts §3, Spec §FR-008]
- [ ] CHK040 label 変更が配信中でも 204 で許可される旨が契約に明示されているか [Clarity, contracts §1, Spec §FR-006]

## Requirement Consistency(既存契約との整合)

- [ ] CHK041 追加拒否条件が 001 の既存保護方針(Host/トークン/レート/ボディ上限)・エラー形式を変更しないことが差分文書で担保されているか [Consistency, contracts 前文]
- [ ] CHK042 IdentityError → HTTP 写像(data-model)と contracts のステータス/コード対応が完全に一致しているか [Consistency, data-model §エラー写像, contracts §5]
- [ ] CHK043 `GET /status` の既存フィールド(pcp_listening 等)が不変で `broadcasting` のみ追加である旨が示されているか [Consistency, contracts §3]
- [ ] CHK044 `persona_not_selectable`(409)と `persona_unusable`(422)の使い分け基準(archived か復号不可か)が一貫して定義されているか [Consistency, contracts §1/§5, data-model §選択可能ガード]

## Acceptance Criteria Quality(契約の受け入れ記述)

- [ ] CHK045 契約の受け入れシナリオが Given/When/Then で記述され、正常系と拒否系の双方を含むか [Measurability, contracts §1 Gherkin]
- [ ] CHK046 ネガティブシナリオ(配信中の直接 API 切替/破棄/アーカイブ → 409、archived/unusable の選択 → 拒否)が受け入れとして明記されているか [Coverage, contracts §1, Principle IV]
- [ ] CHK047 ロック解除の受け入れ(全チャンネル ended 後に select が 204)が定義されているか [Coverage, contracts §1, Spec §SC-003]
- [ ] CHK048 `broadcasting` の true/false 両条件の受け入れが定義されているか [Coverage, contracts §3]

## Dependencies & Assumptions(前提の妥当性)

- [ ] CHK049 「配信中判定の真実源は掲載エンジンの発行中状態(BroadcastState)」という前提が契約と data-model で一致しているか [Assumption, Spec Assumptions, data-model §BroadcastState]
- [ ] CHK050 「channels.html の selected 表示は既存 GET /personas から導出でき新フィールド不要」という前提が契約 §4 で担保されているか [Assumption, Spec Assumptions, contracts §4]
- [ ] CHK051 UI のボタン無効化が `GET /status` の既存 5 秒ポーリングに相乗りするという前提が記載され、無効化が強制でない旨が明確か [Assumption, contracts §3, research R6]

## Notes

- Check items off as completed: `[x]`
- 各項目は契約記述の品質検査であり、エンドポイントの実挙動テストではない。
- 実挙動の検証は tests/ の契約テスト・cucumber(実装フェーズ)で担保する — 本チェックリストはその前段の「契約が正しく書けているか」を問う。

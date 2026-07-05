# リリース前セキュリティ最終確認の記録(T059)

**Date**: 2026-07-04
**Principles**: Principle I (User Safety First), Principle II (Security by Design)
**位置づけ**: constitution **リリース前ゲート 8〜10** の適用記録(tasks.md T059)。
番号なしファイル名は ADR 連番との衝突回避のため([security-review-checklist.md](./security-review-checklist.md) と同様)。

## ゲート 8: Gherkin シナリオ完全通過

`cargo test --test cucumber` — **5 フィーチャ / 17 シナリオ / 67 ステップすべて通過**
(2026-07-04 実行)。ネガティブシナリオを含む:

- `tests/features/security.feature`(T055): spec セキュリティシナリオ 5 件
  (過大ペイロード拒否・なりすまし検出・大量偽登録耐性・危険 URL 警告・
  ペルソナ切替と破棄)+ quickstart 手順 7 の各項(64KB 超フレーム / 16KB 超イベント /
  署名不正 / PEX 不正アドレス)— SC-005, SC-007
- us1_announce / us2_discover / us3_resilience / outbound_only(T022/T034/T045/T051)

## ゲート 9: セキュリティスキャン(既知脆弱性チェック)

2026-07-04 実行(ADR-0001 のツール構成):

| ツール | 結果 |
|--------|------|
| `cargo audit`(286 依存クレート) | **脆弱性 0 件**。警告 1 件のみ: RUSTSEC-2024-0384(`instant` unmaintained — 推移的依存。脆弱性ではなく保守停止の告知。許容済み警告) |
| Trivy(`trivy fs --scanners vuln --severity HIGH,CRITICAL`) | **High/Critical 0 件**(Cargo.lock) |

→ **High/Critical 未緩和ゼロ**(Principle I)を満たす。

## SecurityEvent 全 12 カテゴリと実装ログ出力の一致確認

`SecurityCategory::ALL`(src/security/mod.rs — data-model §SecurityEvent の全量)と
実装の発火箇所を突合した:

| カテゴリ | 発火箇所 |
|----------|----------|
| `pcp_reject` | src/pcp/session.rs |
| `p2p_invalid_frame` | src/p2p/session.rs(順序違反)・src/p2p/frame.rs(不正 JSON) |
| `p2p_oversize` | src/p2p/frame.rs(検査 1) |
| `p2p_rate_limited` | src/p2p/session.rs(検査 2)・src/p2p/runtime.rs(検査 6) |
| `event_oversize` / `event_invalid_sig` / `event_invalid_format` / `event_time_skew` / `event_pow_insufficient` | src/event/schema.rs(受信検証 1〜6 → hub 経由で記録) |
| `pex_rejected` | src/p2p/runtime.rs(検査 5) |
| `http_rate_limited` | src/web/mod.rs(local API)・src/yp/index_txt.rs |
| `url_warning` | src/p2p/hub.rs(**本確認で乖離を検出し追加** — 下記) |

**検出した乖離と是正**: `url_warning` は定義のみで発火箇所が存在しなかった。
gossip 受信で格納成功したイベントのコンタクト URL が http/https 以外の場合に
記録するよう `src/p2p/hub.rs` へ追加した(data-model §SecurityEvent
「URL 警告判定の発動(FR-012)」)。ユニットテスト
(`warned_contact_url_is_logged_on_ingest`)と cucumber
(「危険なコンタクト URL の警告」シナリオ)で検証済み。

## 全エラー応答の内部情報漏洩なし

- **local API**: エラー応答は `{"error":"<code>"}` の定型のみ
  (`tests/contract/local_api.rs` で 401/403/413/429 各系統を検証済み)
- **P2P**: CLOSE reason は `close_reason` 定数(定型コード)のみ
  (`src/p2p/frame.rs`。T055「過大な P2P フレームの切断」で実接続の reason を検証)
- **セキュリティログ**: `detail` は固定文字列のみ。T055
  「エラー応答は内部情報を漏洩してはならない」ステップで、記録された全行の
  `detail` にパス・スタックトレース類が含まれないことを機械検証
- **nsec**: 応答本文のみに使用し、ログ・セキュリティイベントへの記録なし
  (`rg -g '*.rs' 'nsec'` で export 経路のみを確認 — ADR-0003 §2)

## セキュリティレビュー観点チェックリスト(T011)の適用記録

Phase 7 の変更(セキュリティに関わる変更 = `src/p2p/hub.rs` の `url_warning` 記録追加、
`src/security/` 変更なし)への適用結果:

- [x] 1 入力検証(サイズ): 変更なし(既存上限を維持。scale テストは既定値構成のみ)
- [x] 2 入力検証(形式・内容): 変更なし(url_warning の記録は格納**成功後**であり検証順序に影響しない)
- [x] 4 エラー応答: 新規ログ detail は固定文字列 `contact url scheme is not http/https` のみ
- [x] 8 セキュリティイベントログ: data-model 既定義カテゴリの発火追加(新設なし)。集約・ローテーションは SecurityLog 経由で維持
- [x] 9 レート制限・資源上限: 変更なし。追加の `ChannelListing::from_event` は格納成功イベントに限定(検証済み入力)
- [x] 11 P2P 伝搬の不変条件: 変更なし(記録のみ追加。再伝搬・重複抑制のロジック不変 — PlusCal モデル再検査不要)
- [x] 13 テスト対応: unit + cucumber のネガティブ/ポジティブ両系を追加
- 3, 5, 6, 7, 10, 12, 14: 非該当(バインド・暗号・秘密情報・未検証情報・依存クレートの変更なし。追加コメントは記載済み)

## ゲート 10: ドキュメント更新(T060 の実施記録)

### CONTEXT.md

リポジトリ直下に新規作成(モジュール構成・信頼境界・用語集・ADR の所在 —
docs/agents/domain.md の単一コンテキスト構成)。

### ADR-0002〜0006 と実装の突合(2026-07-04)

| ADR | 主要決定 | 実装との一致 |
|-----|----------|--------------|
| 0002 | kind 30311・鮮度 600 秒/expiration・援用境界(`event/`/`p2p/` 分離・`nostr-sdk` 非依存) | 一致(Cargo.toml に `nostr` のみ。リレー関連コードなし)。**トラッカー解決の検証可能な仮定(TIP のみで視聴開始)は未検証のまま** — T058(実機検証)で記録予定 |
| 0003 | DPAPI 保管・nsec エクスポート受け入れ基準 3 点・破棄=行削除・復号失敗=利用不可 | 一致(`src/identity/mod.rs`・`src/web/personas.rs`。cucumber「ペルソナの切替と破棄」で検証) |
| 0004 | pubkey クォータ ≤64・PoW 既定 0・ミュート・リンク推定注意文言(UI 常設)・URL スキーム警告 | 一致(`src/event/store.rs`・`ui/personas.html` 常設文言・`url_warning` は本ゲートで発火箇所を追加) |
| 0005 | 形式的検証「該当」・伝搬 4 不変条件・DedupCache ≥ 鮮度窓の連動 | 一致(`docs/formal/gossip_propagation.tla` + TLC 結果あり。Phase 7 で伝搬規則の変更なし → 再検査不要) |
| 0006 | 平文 TCP・plain HTTP 受容・`pcp_bind`/`http_bind` loopback 強制・LAN 公開オプトイン v1 非実装 | 一致(`src/config.rs` 検証拒否・`src/web/settings.rs` 400 応答・オプトイン実装なし) |

ADR-0007(ライセンス — T063)は 2026-07-05 に作成済み(MIT 確定・`LICENSE` 配置・constitution
v1.1.1 反映)。突合: 実装との衝突なし(連携はプロセス間 TCP のみ・クリーンルーム実装の維持)。
残タスクは T057 README(ライセンス表記は ADR-0007 に従う)。

### docs/formal/

`gossip_propagation.tla` / `gossip_propagation-result.md` は Phase 2 の TLC 検査結果から
変更なし。Phase 7 に伝搬規則(重複抑制・置換・再伝搬・SYNC)へ触れる変更はない
(`url_warning` 記録は格納成功後の観測のみ)ため、モデル・結果とも最新状態。

### checklists(specs/001-nostr-p2p-yp/checklists/)

requirements(12)・p2p(20)・security(23)・interop(14)の全 69 項目が消し込み済み
(2026-07-03)であることを確認(2026-07-04 再集計: 未完了 0)。消し込み後の実装
(Phase 3〜7)は tasks.md の Amendment 群に追記された確定値に従っており、
契約・データモデルとの乖離は上表の突合で検出されていない。

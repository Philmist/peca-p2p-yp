# Research: 読み取り専用 index.txt の LAN 公開

**Date**: 2026-07-08 | **Plan**: [plan.md](plan.md)

前セッションの grill-me(7 分岐、全ユーザー承認済み — handoff 参照)で主要判断は確定済み。
本書は残る実装レベルの未知を解消し、確定判断の技術的根拠を記録する。

## R1: private/LAN 判定の実装方式

- **Decision**: 許可リスト方式の新 helper `require_lan_or_loopback(key, value)` を
  `src/config.rs` に追加する。判定は `IpAddr::to_canonical()` で IPv4-mapped IPv6 を
  正規化した上で:
  - IPv4: `is_loopback() || is_private() || is_link_local()`(すべて std 安定 API)
  - IPv6: `is_loopback()` || ULA(`segments()[0] & 0xfe00 == 0xfc00` — fc00::/7)
    || リンクローカル(`segments()[0] & 0xffc0 == 0xfe80` — fe80::/10)を手動ビット判定
  - 上記以外はすべて拒否(新 `ConfigError::NonLanBind { key }`)
- **Rationale**: 許可リストは unspecified(0.0.0.0 / ::)・グローバルユニキャスト・
  CGNAT(100.64.0.0/10 — spec 確認で「含めない」確定)を個別に列挙せず構造的に弾ける
  (Principle II)。IPv6 の `is_unique_local` / `is_unicast_link_local` は MSRV 依存の
  安定化時期に揺れがあるため、2 行のビット判定 + ゴールデンテーブルのユニットテストで
  固定する方が確実。`to_canonical()` は安定 API で、`::ffff:192.168.1.1` のような
  mapped 表記の判定漏れを防ぐ。
- **Alternatives considered**:
  - 拒否リスト方式(グローバルのみ列挙)— 新割当や特殊帯域の漏れリスクがあり却下。
  - `ipnet` 等の外部クレート — 依存追加に見合わない(判定は数行)。

## R2: 第 2 リスナーの構成方式

- **Decision**: `src/web/mod.rs` に `build_index_router(state: AppState) -> Router` を追加:
  `Router::new().merge(index_txt::routes()).fallback(定型404).with_state(state)`。
  `main.rs` §15 の直後で `index_bind` 非空時に `TcpListener::bind` →
  `axum::serve(...).with_graceful_shutdown(...)` を `tokio::spawn` し
  `handles` に push(既存サブシステムと同じ shutdown パターン)。
- **Rationale**: grill 確定判断 2(専用第 2 リスナー)。`index_txt::routes()` は
  `AppState` のみに依存し(調査済み)、`AppState` は全フィールド `Arc` で安価に clone
  できる。API ルートを物理的に持たないため、ミドルウェアの経路フィルタの実装ミスという
  故障モードが存在しない(Principle II)。`index_txt::routes()` は `pub(crate)` のままで
  よい(ルーター合成は同一クレート内の `web::mod`)。
- **Alternatives considered**: 単一リスナー + 接続元/経路フィルタ(B 案)— grill で却下
  済み(フィルタのバグ = 全 API の LAN 露出という故障モードを持つ)。
- **確認済みの付随事項**:
  - **レート制限の共有**: `AppState.index_txt_rate_limiter` は Arc 共有のため loopback /
    LAN 両リスナーで per-IP 10 req/秒が一体で効く。接続元 IP が異なるため実質独立であり、
    同一 IP が両経路を使う場合も合算 10 req/秒は安全側 — このままとする。
  - **Host 検証**: index.txt ハンドラは Host 検証を持たない(Host 検証は `/api/v1` の
    DNS rebinding 対策であり、認証も状態変更もない公開一覧には不要)。LAN リスナーにも
    適用しない。
  - **connect-info**: レート制限の接続元取得のため、第 2 リスナーも
    `into_make_service_with_connect_info::<SocketAddr>()` で serve する(既存 §17 と同じ)。
  - **GET/HEAD 以外**: axum が 405(空ボディ + Allow)を自動応答。内部情報を含まないため
    定型エラー要件(FR-002)を満たす。未定義パスは fallback の定型 404 JSON。

## R3: bind 失敗の縮退継続と状態伝搬

- **Decision**: 第 2 リスナーの bind 失敗は `bind_error()`(即終了)を使わず、
  `tracing::warn!` + 露出状態オブジェクトへの反映のみ行い起動を継続する。
  露出状態は起動時に確定する不変データ `IndexLanStatus { bind: String, listening: bool,
  error: Option<&'static str> }` を `AppState.index_lan: Option<Arc<IndexLanStatus>>`
  として注入(`None` = 機能無効)。`GET /api/v1/status` は `index_txt_lan` オブジェクト
  (3 状態: 無効 / 露出中 / 設定有効だが失敗)として返す。
- **Rationale**: grill 確定判断 7。既存 3 バインドは本体機能なので fail-fast のまま
  (FR-007 MUST NOT)。失敗理由は既存 `bind_error` と同じ ErrorKind → 定型コードの写像で
  内部情報を漏らさない(`addr_in_use` / `permission_denied` / `addr_not_available` /
  `unknown`)。実行時に変化しない値なので `Arc<Mutex<...>>` は不要。
- **Alternatives considered**: 起動後リトライ(常駐再バインド)— 要件外の複雑化。
  再起動で再試行できれば十分(設定変更も再起動要求)。

## R4: SecurityCategory の追加

- **Decision**: `SecurityCategory::IndexTxtLanExposed`(`"index_txt_lan_exposed"`)を追加。
  `ALL` 配列 14→15、`as_str()`、網羅テストを同時更新。記録条件は
  「`index_bind` が**非 loopback** かつ bind **成功**」時に起動時 1 件
  (source = バインドアドレス、detail = 定型文言)。
- **Rationale**: grill 確定判断 5。loopback 値での起動は露出でないため記録しない
  (spec US3 シナリオ 5)。bind 失敗時は露出が発生していないため記録しない
  (/status の failed 状態で可視化)。既存カテゴリの「違反を弾いた」記録と毛色が違う
  「運用状態の監査」であることは enum のドキュメントコメントに明記する。
- **Alternatives considered**: 汎用 `ConfigurationWarning` カテゴリ — 集計・フィルタで
  index.txt 露出を特定できなくなるため却下。

## R5: 設定キー・CLI・設定 API の拡張

- **Decision**:
  - `Settings` に `index_bind: String`(既定 `""`)、キー `"index_bind"` を追加
    (13→14 キー。load は lenient / save は全キー書出しの既存規約に従う)。
  - `validate()` に「空なら無効(検証スキップ)、非空なら `require_lan_or_loopback`」を追加。
    単一アドレス制約(FR-009)は `SocketAddr` 直パースで自然に担保(カンマ入りはパース
    失敗 → `InvalidBind`)。
  - `CliOverrides` に `index_bind: Option<String>` + `--index-bind`(`--key value` /
    `--key=value` 両形式)を追加。
  - `web/settings.rs` の `BIND_KEYS` を 3→4(`index_bind` 追加)— 既存の
    `restart_required` / `restart_keys` 応答形をそのまま利用(spec Assumptions の
    「既存と同形」を確定)。検証エラーの HTTP 写像に `NonLanBind` → 400 定型コードを追加。
- **Rationale**: すべて既存パターンの延長で、新しい形式・応答形を発明しない
  (Principle III — 保守性)。
- **Alternatives considered**: `index_bind` を bool + ポート番号の 2 キーに分割 — 既存の
  バインド系キーの書式(`addr:port` 文字列)と不整合になるため却下。

## R6: UI 警告ゲートの実装方式

- **Decision**: `ui/settings.html` に (a) `BIND_KEYS`(JS 側)へ `index_bind` 追加、
  (b) 保存時に `index_bind` が非空かつ非 loopback(`127.` / `[::1]` 接頭辞判定)なら、
  警告文とチェックボックス(「掲載一覧が LAN 内で平文・無認証のまま取得・改ざんされうる
  ことを理解した」)を表示し、チェックされるまで送信をブロックする。
- **Rationale**: grill 確定判断 5(1 項目警告 + 明示確認)。チェックボックスは
  `confirm()` ダイアログと違い DOM テスト可能で、既存 UI の素朴な HTML+JS 構成に合う。
  バックエンドの強制は「危険域(グローバル等)の検証拒否」までとし、「確認済みか」は
  UI 層の責務(CLI / API 直接呼び出しは明示指定自体をオプトインとみなす — spec Assumptions)。
- **Alternatives considered**: PUT に `confirm: true` フィールドを要求 — API 契約の変更が
  index_bind だけ特殊形になり、自動化クライアントを壊すため却下。

## R7: ADR の構成(ADR-0006 決定 4 の部分 supersede)

- **Decision**: 新規 **ADR-0012**(`docs/adr/0012-index-txt-lan-exposure.md`、既存 ADR は
  0011 まで)を Proposed で起草(Phase 1 成果物)。ADR-0006 決定 4 を「read-only
  index.txt に限り」部分 supersede し、決定 4 の将来解禁条件 2 項目との対応表を含める。
  Principle V(形式的検証)の「クリティカル非該当」判断も本 ADR に記録する。
  **ADR-0012 のユーザー承認を実装開始の前ゲートとし、承認と同時に ADR-0006 冒頭へ
  部分 supersede の追記を行う**(tasks の先頭タスク)。
- **Rationale**: grill 確定(spec Assumptions)。Accepted な ADR-0006 の本文改変は
  ADR-0012 承認前に行わない(権威文書の整合性維持)。
- **Alternatives considered**: ADR-0006 の直接改訂のみ — supersede 履歴が追えなくなるため却下。

## 未解決事項

なし(spec の [NEEDS CLARIFICATION] は 0 件、本書で実装レベルの未知も解消)。

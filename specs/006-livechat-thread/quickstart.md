# Quickstart: 配信実況スレ(P2P 掲示板)の検証

**Feature**: `006-livechat-thread` | **参照**: [spec.md](./spec.md), [contracts/](./contracts/)

本機能が端到端で成立することを確認する実行手順。実装の詳細は tasks.md と各契約を参照。

## 前提

- Rust ツールチェーン(edition 2024)。`cargo build` が通ること
- 形式的検証(§6)のみ TLA+ Toolbox / TLC が必要(`docs/formal/README.md` 参照)

## 1. 自動テスト一括

```powershell
cargo fmt -- --check
cargo clippy --all-targets
cargo test                        # unit + 契約 + 縮小統合
cargo test --test cucumber        # Gherkin(spec US1〜US6 のシナリオ)
```

期待: 全パス。実装前は US1〜US6 対応の feature が**失敗すること**を先に確認する
(テストファースト — 実装中ゲート 5)。

## 2. 開設・発見・閲覧(US1 / SC-005・SC-006)

```powershell
cargo test --test livechat -- us1
```

多ノード統合テスト(配信者 1 + 視聴者 2)で検証する内容:

- スレ開設 → kind 31311 announce が gossip で伝搬し、一覧に表示される
- announce 受信のみでは外向き接続 0 件(SC-005)
- 利用者操作起点で接続 → チャレンジ検証 → 全レス同期・表示(鍵なしで閲覧可)
- 既存チャンネル発見の SC(掲載 60 秒以内)が維持される(SC-006 — scale テストに
  announce 負荷を追加した構成で確認)

## 3. 書き込みと確定表示(US2 / SC-001・SC-002)

```powershell
cargo test --test livechat -- us2
```

- 3 ノード同時書き込みで全端末のレス番号・並び・アンカー解決が一致(不一致 0 — SC-002)
- 「送信中」→ 確定表示の遷移、名前欄 `#` 除去、名無しのデフォルト名適用
- バースト 30 レス/分・参加者 100 接続の負荷構成で p99 ≤ 5 秒(SC-001。
  `-- --ignored` の負荷プロファイルで実行)

## 4. セキュリティ・ネガティブ(US3・US4 / SC-004)

```powershell
cargo test --test thread_events          # イベント契約(ネガティブ含む)
cargo test --test thread_delivery        # モックピアによる配送契約ネガティブ
cargo test --test cucumber -- -i tests/features/security.feature
```

- 署名不一致 announce / 偽 ORDER / 過大レス / レート違反 → 100% 不可視 +
  SecurityEvent 記録(SC-004。カテゴリは data-model §SecurityEvent)
- 第三者アドレスを指す announce → チャレンジ失敗 → 切断 + バックオフ
- BAN 鍵の採番拒否(理由非開示)・NG のローカル欠番・初回 PoW 不足の拒否

## 5. ライフサイクルと互換 API(US5・US6 / SC-003・SC-007)

```powershell
cargo test --test livechat -- us5
cargo test --test compat_bbs
```

- レス上限(テスト用に小さく設定)→ 次スレ移行・旧スレ書き込み不可
- ホスト kill → 凍結(閲覧継続・書き込み不可)、明示クローズ → データ削除
- 4000 レス済みスレへの途中参加 → 15 秒以内に全ログ(SC-003)

互換 API の手動確認(ノード起動後):

```powershell
# スレ一覧(Shift_JIS)
curl.exe -s http://127.0.0.1:7183/<board_id>/subject.txt --output subject.txt
# 書き込み(bbs.cgi 相当)
curl.exe -s -X POST http://127.0.0.1:7183/test/bbs.cgi --data "bbs=<board_id>&key=<key>&FROM=&mail=&MESSAGE=%83e%83X%83g"
# 反映確認
curl.exe -s http://127.0.0.1:7183/<board_id>/dat/<key>.dat --output thread.dat
```

期待: subject.txt / dat が契約形式(contracts/compat-api.md)で返り、書き込みが
採番確定後の dat 再取得に反映される。loopback 外からのアクセスと不正 Host は定型拒否。

**受け入れ(SC-007)**: 利用者所有の実況ツール一式を `http://127.0.0.1:7183/<board_id>/`
に向け、無改修でスレ一覧取得・レス取得・書き込みが成立することを実機確認する
(001 R5 の YP ブラウザ実機検証と同型。結果は research R5 に追記)。

確認観点(contracts/compat-api.md の仮説領域 — interop checklist より転記):

- [ ] hex 64 桁の板ディレクトリ名を外部板として登録・巡回できるか(不可なら短縮エイリアスを検討)
- [ ] Shift_JIS 出力・数値文字参照(`&#dddd;`)・実体参照エスケープが正しく描画されるか
- [ ] 差分取得(Range/206)・更新チェック(If-Modified-Since/304)が専ブラの再取得動作と噛み合うか(あぼーん誤検知が起きないか)
- [ ] 保持しない dat への定型 404 が「dat 落ち」として解釈されるか(誤動作しないか)
- [ ] SETTING.TXT のキー集合(BBS_MESSAGE_COUNT=2048 ほか)が専ブラの入力制限に反映されるか(jpnkn / EX0ch 系の提示キーとの突合)
- [ ] head.txt の生 Markdown 表示が許容範囲か
- [ ] クッキー確認画面なしの直接受理で書き込みが成功扱いになるか(`<title>書きこみました。</title>` 判定)
- [ ] 板鍵固定 ID(日替わりなし)で専ブラの ID NG 機能が機能するか

## 6. 形式的検証(Principle V / research R9)

```powershell
# docs/formal/README.md のセットアップ後
java -cp <tla2tools.jar> tlc2.TLC docs/formal/livechat_sequencer.tla -config docs/formal/livechat_sequencer.cfg
```

期待: デッドロックなし・不変条件(採番一意・欠番なし単調増加・上限超過なし・
移行境界の二重採番なし)違反なし。結果は ADR-0014 に記録。

## 完了判定

- [ ] §1 の全自動テストがパス(実装前は失敗を確認済み)
- [ ] §2〜§5 の各シナリオが契約どおりに動作
- [ ] SC-007 実機確認済み(結果を research R5 へ追記)
- [ ] §6 の TLC 検査パスと ADR-0014 の記録
- [ ] `cargo audit` / gitleaks / CI(fmt・clippy)クリーン

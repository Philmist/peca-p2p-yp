# Quickstart: PEX 自己アドレス拒否の良性化 検証

本機能が満たすべき挙動を、単体/契約/BDD と実運用の両面で検証する手順。

## 前提

- Rust ツールチェイン(edition 2024)
- リポジトリルートで実行

## 1. 静的チェック

```bash
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
```

## 2. 単体・契約テスト(分類ロジック)

```bash
# pex 分類の単体テスト(--lib 限定。cucumber は harness=false でフィルタ引数を受けない)
cargo test --lib p2p::pex

# 契約テスト(良性/不審/混在/回帰)。Cargo.toml のテストターゲット名は pex(path = tests/contract/pex.rs)
cargo test --test pex
```

**期待**:
- 自己アドレスのみ / 重複のみの `PEERS` → `has_suspicious() == false`(記録されない)
- 件数超過 / 形式不正 / ホスト名 → `has_suspicious() == true`(記録される)
- 混在 → `has_suspicious() == true`
- どのケースでも `accepted` は変更前と同一(回帰なし)

## 3. BDD(Gherkin)シナリオ

```bash
cargo test --test cucumber   # または既定の cucumber ランナー
```

`tests/features/security.feature` の追加シナリオ:
- 「自己アドレスのみの PEX 破棄はセキュリティイベントを生成しない」
- 「重複のみの PEX 破棄はセキュリティイベントを生成しない」
- 「不正な PEX 内容(件数超過/形式不正)はセキュリティイベントを生成する」
- 「良性と不正の混在はセキュリティイベントを生成する」

## 4. 実運用での確認(SC-001 / SC-003)

3 ノードの健全な網(自己反射のみ)で:

```bash
# 良性反射時: pex_rejected が出ないこと
journalctl -u peca-p2p-yp --no-pager -o cat | grep -c 'pex_rejected'   # → 増えない

# debug 有効時: 良性破棄が観測できること
RUST_LOG=p2p=debug でノードを起動し、良性破棄の debug 行(source + 件数)を確認
```

**期待(受け入れ基準)**:
- SC-001: 自己反射・重複のみの接続で `pex_rejected` の記録が 0 件
- SC-002: 件数超過・形式不正を含む `PEERS` で `pex_rejected` が 100% 記録
- SC-003: debug ログで良性破棄を source/件数の粒度で追跡可能
- SC-004: 破棄(候補登録しない)挙動は変更前と同一

## 5. ドキュメント整合(FR-005)

- `specs/001-nostr-p2p-yp/contracts/p2p-gossip.md` 検査5 の違反時ログ条件が更新されている
- `specs/001-nostr-p2p-yp/data-model.md` の `pex_rejected` 記録条件が更新されている
- `docs/adr/0013-pex-benign-rejection.md` が存在し、良性/不審の切り分け・Principle V 非クリティカル
  判断・Security Requirements 整合が記載されている

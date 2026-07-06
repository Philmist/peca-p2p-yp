# Quickstart: Linux 対応の検証手順(002-linux-support)

**Plan**: [plan.md](./plan.md) | **Contracts**: [contracts/](./contracts/)

本機能が end-to-end で動くことを確認する検証シナリオ集。実装詳細は tasks.md と
contracts/ を正とする。

## 前提

- Linux x86_64(systemd 採用ディストリビューション。検証 4 以外は systemd 不要)
- Rust stable toolchain(edition 2024 対応)
- 到達可能な既知ピア(Windows 版ノードでも可 — SC-005 の相互検証に使う)

## 検証 1: Linux でのビルドとテスト(FR-001, SC-002)

```bash
cargo fmt -- --check
cargo clippy --all-targets
cargo build --release
cargo test            # unit + contract + integration + cucumber
```

**期待**: すべて成功。`windows` クレートは Linux ビルドの依存グラフに現れない
(`cargo tree | grep -i windows` が Win32 クレートを含まない)。

## 検証 2: 手動起動で利用者ノードとして稼働(US1, SC-001)

```bash
./target/release/peca-p2p-yp
# 既定 data-dir: $XDG_STATE_HOME/peca-p2p-yp(未設定なら ~/.local/state/peca-p2p-yp)
```

1. UI(`http://127.0.0.1:7180/`)からピアを追加 → gossip セッション確立をログで確認
2. 他ノードが掲載したチャンネルが一覧と `http://127.0.0.1:7180/index.txt` に現れる
3. loopback 強制(ADR-0006)が有効: `--http-bind 0.0.0.0:7180` が拒否される

**期待**: 追加の対話操作なしに起動〜最初のチャンネル発見まで完了(SC-001)。

## 検証 3: ペルソナ at-rest 保護(US2, SC-003, SC-005)

```bash
# ペルソナ作成(UI または API)
curl -s -X POST http://127.0.0.1:7180/api/v1/personas -d '{"label":"linux-test"}' -H 'content-type: application/json'
```

1. **平文非保存**: `strings ~/.local/state/peca-p2p-yp/app.db | grep -c <パターン>` が 0。
   検査パターンは秘密鍵の **hex(64 桁・大文字/小文字)と bech32(`nsec1…`)の両表現**。
   `secret_enc` が `PYK1` + scheme 0x02 で始まる(contracts/key-envelope.md)。
   バイトレベルの平文 32 bytes 非含有は契約テスト 1 が機械検証するため、本 strings 検査は
   文字列表現の混入がないことを確認する運用上の補助である(SC-003 の判定は両者の組)
2. **鍵分離**: `master.key`(32 bytes・`0600`)が存在。DB だけを別マシンへコピーしても
   復号不能(全ペルソナ利用不可表示、ノードは稼働継続 — FR-006/SC-006)
3. **他アカウント遮断**: 別ユーザーから `cat master.key` / `cat app.db` が Permission denied
4. **是正**: `chmod 644 master.key` して再起動 → `0600` へ自動是正され、
   `security.log` に `key_permission_fixed` が記録される(FR-013)
5. **是正不能の部分劣化**: root で `chown root master.key` して再起動 → 警告 +
   全ペルソナ利用不可、ただし index.txt・一覧・gossip は継続(FR-013)
6. **相互発見**: このペルソナで掲載したチャンネルが Windows ノードで発見でき、逆も成立
   (SC-005)
7. **エクスポート/破棄**: nsec エクスポート(confirm 必須)と破棄が Windows 版と同一の
   意味論(ADR-0003)。ログ・security.log に秘密鍵が出ない — 検査範囲は hex(64 桁)・
   bech32(`nsec1…`)・その部分文字列・`Debug` 表現(FR-011、contracts/key-envelope.md §4)

## 検証 4: systemd サービス(US3, SC-004)

```bash
sudo useradd --system --home-dir /var/lib/peca-p2p-yp --shell /usr/sbin/nologin peca-p2p-yp
sudo install -m 755 target/release/peca-p2p-yp /usr/local/bin/
sudo install -m 644 contrib/systemd/peca-p2p-yp.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now peca-p2p-yp
```

1. `systemctl is-active peca-p2p-yp` → `active`(READY 通知後に start が返る — FR-009)
2. `journalctl -u peca-p2p-yp` に起動サマリが出る。ANSI エスケープ・秘密鍵が含まれない
   (FR-011)
3. `time sudo systemctl stop peca-p2p-yp` → 90 秒以内・`ExecMainStatus=0`(FR-008/SC-004)
4. `sudo systemctl start peca-p2p-yp && sudo kill -9 $(systemctl show -p MainPID --value peca-p2p-yp)`
   → 自動再起動(Restart=on-failure)
5. data-dir は `/var/lib/peca-p2p-yp`(StateDirectory — FR-010)
6. 上記 1〜5 がデスクトップセッション・GUI・対話操作なしで完了している(nologin の
   サービスアカウントによるヘッドレス無人稼働 — FR-005/SC-003)

## 検証 5: 複数インスタンス(FR-010)

```bash
./peca-p2p-yp --data-dir /tmp/nodeA --http-bind 127.0.0.1:7180 --pcp-bind 127.0.0.1:7146 --p2p-bind 0.0.0.0:7147 &
./peca-p2p-yp --data-dir /tmp/nodeB --http-bind 127.0.0.1:7190 --pcp-bind 127.0.0.1:7156 --p2p-bind 0.0.0.0:7157 &
```

**期待**: 両ノードが独立の DB・master.key で同時稼働し、相互にピア登録して gossip できる。

## 検証 6: 起動失敗の定型エラー(FR-014)

```bash
./peca-p2p-yp --p2p-bind 0.0.0.0:80        # 特権ポート(非 root)
./peca-p2p-yp --http-bind 127.0.0.1:7180   # 使用中ポート(検証 2 稼働中に)
```

**期待**: 原因が識別できる定型メッセージ + 非 0 終了。スタックトレース・内部パスなし。

## 検証 7: Windows 後方互換(FR-001)

Windows で既存(002 実装前)DB を持つ環境を 002 実装版へ更新して起動。

**期待**: レガシー DPAPI BLOB のペルソナがそのまま利用可能(読込後方互換)。新規作成
ペルソナは `PYK1` + scheme 0x01 で保存され、以後も利用可能。

## CI での自動検証(SC-002)

`.github/workflows/ci.yml` の windows-latest / ubuntu-latest 両ジョブで
fmt / clippy / 全テスト(cucumber 含む)が同一に通過すること。

## セキュリティシナリオとテストの対応(トレーサビリティ)

spec のセキュリティシナリオ 7 本の検証割り当て(実装済みテスト ID へ更新済み — T034)。
contract テストは `tests/contract/`、cucumber シナリオは `tests/features/security.feature`
(ステップ定義: `tests/steps/keystore.rs`)にある。割り当てのないシナリオはない。

| Gherkin シナリオ(spec) | contract テスト | cucumber(security.feature) | 手動(quickstart) |
|---|---|---|---|
| 平文非永続化 | `key_envelope.rs::protect_output_is_envelope_without_plaintext`(#1) | 「ペルソナ秘密鍵の平文非永続化」 | 検証3-1 |
| 別アカウント復号不能 | `key_envelope.rs::different_master_key_cannot_decrypt`(#8) | —(マルチアカウントは CI 不能) | 検証3-2, 3-3 |
| パーミッション自動是正 | `cli_config.rs::loose_master_key_and_db_are_fixed_and_recorded`(§7 #3) | 「緩いパーミッションの自動是正」 | 検証3-4 |
| 是正不能の部分劣化 | `cli_config.rs::unfixable_symlink_is_reported_and_recorded`(§7 #4) | 「是正不能時の部分劣化」 | 検証3-5 |
| 復号不能データの隔離 | `key_envelope.rs::corrupted_payload_is_unusable_not_panic`(#3)/`foreign_and_unknown_scheme_is_unusable`(#4)/`legacy_blob_without_magic_is_unusable_on_unix`(#6) | 「復号不能データの隔離」 | 検証3-2 |
| 秘密鍵のログ非出力 | —(全テストの事後アサーション) | 各シナリオの事後アサーション(hex 64 桁・`nsec1…`・部分文字列・Debug 表現の非出力検査) | 検証3-7, 検証4-2 |
| バインド失敗時の非漏洩 | `platform_startup.rs::port_in_use_exits_nonzero_with_typed_message`(§7 #5) | — | 検証6 |

補助対応(表外): data-dir 解決順は `cli_config.rs::cli_data_dir_beats_all_env_sources` /
`state_directory_beats_xdg_and_home` / `state_directory_colon_separated_uses_first` /
`xdg_state_home_beats_home` / `home_dot_local_state_fallback` / `unresolvable_without_any_source_returns_err`
(cli-config §7 #1・#2)、複数インスタンスは `platform_startup.rs::two_instances_run_concurrently`
(§7 #6)、SIGTERM/sd_notify は `graceful_shutdown.rs::sigterm_causes_graceful_shutdown_with_exit_code_0` /
`sd_notify_ready_and_stopping_delivered_via_notify_socket`(systemd-service §1)が検証する。

FR-007(nsec エクスポート・破棄の同一意味論)は新規テストを追加せず、既存 001 の
contract / cucumber テストが Linux CI でも同一に通過すること(SC-002 — research R10)で
検証する。手動確認は検証3-7。

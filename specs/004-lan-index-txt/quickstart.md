# Quickstart 検証ガイド: index.txt の LAN 公開

**Spec**: [spec.md](spec.md) | **Contract**: [contracts/index-txt-lan.md](contracts/index-txt-lan.md)

実装完了後に機能が end-to-end で成立していることを確認する手順。SC-001〜SC-006 に対応する。

## 前提

- 本ソフトのビルド済みバイナリ(`cargo build --release`)
- 検証ホストの LAN アドレス(例: `192.168.1.10`)。別 PC がない場合は同一 LAN の
  スマートフォンのブラウザや、`curl --interface` 相当で代替可
- **Windows**: 初回の非 loopback バインド時に Windows Defender ファイアウォールの
  確認プロンプトが出る。「プライベートネットワーク」での許可が必要
- **Linux(systemd)**: ユニットで動かしている場合は `ExecStart` に `--index-bind` を
  追加するか、設定 UI で保存後にサービス再起動(`systemctl restart ...`)

## 1. 回帰なし(SC-001)

```powershell
# index_bind を設定せず従来どおり起動
.\peca-p2p-yp.exe
```

- 起動サマリログに LAN 公開の記載がないこと
- `GET http://127.0.0.1:7180/api/v1/status` の `index_txt_lan.enabled` が `false`
- セキュリティイベントに `index_txt_lan_exposed` が 1 件もないこと

## 2. 有効化と取得(SC-002)

```powershell
.\peca-p2p-yp.exe --index-bind 192.168.1.10:7180
```

別 PC から:

```text
http://192.168.1.10:7180/index.txt
```

- 200 で一覧が返り、内容が同一ホストの `http://127.0.0.1:7180/index.txt` と一致すること
  (`index_txt_encoding` を `shift_jis` に変えた場合も両者が一致して追随すること)
- HEAD リクエストも GET と同じ `Content-Type` で応答すること

## 3. 攻撃面の限定(SC-003)

別 PC から以下を試行し、**すべて失敗**することを確認:

| 試行 | 期待 |
|------|------|
| `http://192.168.1.10:7180/` (UI) | 404 `{"error":"not_found"}` |
| `http://192.168.1.10:7180/api/v1/status` | 404 `{"error":"not_found"}` |
| `http://192.168.1.10:7180/api/v1/settings` へ PUT | 404(トークン有無に関わらず) |
| `/index.txt` へ POST | 405 |
| `192.168.1.10` の PCP ポート(7144)へ接続 | 到達不可 / 即切断(従来どおり) |

## 4. 危険値の拒否(SC-004)

```powershell
.\peca-p2p-yp.exe --index-bind 0.0.0.0:7180      # → 設定エラーで起動拒否
.\peca-p2p-yp.exe --index-bind 203.0.113.5:7180  # → 設定エラーで起動拒否
.\peca-p2p-yp.exe --index-bind 100.64.0.1:7180   # → 設定エラーで起動拒否(CGNAT)
```

設定 UI / `PUT /api/v1/settings` でも同値が 400(`non_lan_bind`)で拒否されること。

## 5. 縮退継続(SC-005)

```powershell
# 例: index_bind に管理ポートと同一の 127.0.0.1:7180 を指定(バインド競合を誘発)
.\peca-p2p-yp.exe --index-bind 127.0.0.1:7180
```

- 本体が起動し続けること(UI・API・PCP が同一ホストで利用可能)
- 警告ログ(定型文言)が出ること
- `index_txt_lan` が `{"enabled":true,"listening":false,"error":"addr_in_use"}` になること

## 6. 警告ゲートと監査(SC-006 / FR-006)

1. 設定 UI(`http://127.0.0.1:7180/settings.html`)で `index_bind` に
   `192.168.1.10:7180` を入力して保存を試みる
   - 警告 1 項目が表示され、チェックボックスを入れるまで保存できないこと
   - 保存後に「再起動が必要です(変更: index_bind)」が表示されること
2. 再起動後:
   - セキュリティイベントに `index_txt_lan_exposed` が 1 件記録されること
   - `index_txt_lan` が露出中(`listening: true`)を示すこと
3. `index_bind` を `127.0.0.1:7181`(loopback)にして再起動した場合は
   `index_txt_lan_exposed` が記録**されない**こと

## 自動テストの実行

```powershell
cargo fmt -- --check
cargo test --test cli_config --test local_api   # 契約テスト
cargo test --test index_lan                     # 統合テスト(新規)
cargo test                                      # 全体
```

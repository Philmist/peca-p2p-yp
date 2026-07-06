# Contract: systemd サービス統合(002-linux-support)

**Principles**: I, II | **FR**: FR-008, FR-009, FR-010, FR-011, FR-012 | **SC**: SC-004

サービスとしての振舞い(プロセス契約)と、配布する unit 定義例の契約。

## 1. プロセス契約(systemd から見た振舞い)

| 事象 | 契約 |
|------|------|
| 起動完了 | 全リスナー(HTTP・PCP・P2P(有効時))のバインド成功**後**に `READY=1` を通知する(SHOULD — FR-009)。`NOTIFY_SOCKET` 未設定時は通知なしで正常稼働(MUST) |
| 停止要求 | `SIGTERM` 受信で `STOPPING=1` を通知し、既存 graceful shutdown 経路(watch チャネル伝播)で全サブシステムを停止して終了コード 0 で終える(MUST — FR-008)。systemd 既定タイムアウト 90 秒以内(SC-004) |
| `SIGINT` | SIGTERM と同一挙動(手動起動の Ctrl+C 互換) |
| 起動失敗 | バインド失敗・権限不足は原因が識別できる定型メッセージを stderr/ログへ出して非 0 終了(MUST — FR-014)。内部詳細(スタックトレース・絶対パス)は出さない(MUST NOT) |
| 終了コード | 0 = 正常停止 / 1 = 実行時異常 / 2 = 引数・設定不正(既存 main.rs の規約を維持) |
| ログ | stdout へ出力(journald が捕捉 — FR-011)。非端末では ANSI 無効。秘密鍵・nsec は出力しない(MUST NOT) |
| 状態配置 | `$STATE_DIRECTORY` があれば data-dir として使用(`--data-dir` が優先 — contracts/cli-config.md) |

### sd_notify プロトコル(自前実装 — research R5)

- `$NOTIFY_SOCKET` のパス(先頭 `@` は abstract socket: `\0` に読替)へ `UnixDatagram` で送信
- 送信内容: `READY=1`(起動完了時)/ `STOPPING=1`(停止開始時)
- 送信失敗は無視する(ログ debug のみ。稼働へ影響させない MUST — FR-009)

## 2. unit 定義例(`contrib/systemd/peca-p2p-yp.service`)

```ini
[Unit]
Description=peca-p2p-yp - decentralized P2P yellow pages node
After=network-online.target
Wants=network-online.target

[Service]
Type=notify
ExecStart=/usr/local/bin/peca-p2p-yp
User=peca-p2p-yp
Group=peca-p2p-yp
StateDirectory=peca-p2p-yp
StateDirectoryMode=0700
UMask=0077
Restart=on-failure
RestartSec=5

# ハードニング(最小権限 — Principle II)
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
RestrictNamespaces=true
LockPersonality=true
MemoryDenyWriteExecute=true
RestrictRealtime=true
RestrictSUIDSGID=true
SystemCallArchitectures=native
SystemCallFilter=@system-service
CapabilityBoundingSet=

[Install]
WantedBy=multi-user.target
```

### 設計上の要点(契約)

- `Type=notify`: READY 通知(§1)と対。通知非対応環境向けに `Type=simple` でも動作する
  こと(MUST)
- `StateDirectory=peca-p2p-yp`: `/var/lib/peca-p2p-yp` を作成・所有権設定し
  `$STATE_DIRECTORY` を注入(FR-010)。`StateDirectoryMode=0700` + `UMask=0077` で
  新規ファイルが既定で群/他者不可視(FR-013 の予防側)
- `TimeoutStopSec` は**指定しない**(systemd 既定 90 秒に委ねる — spec Clarifications)
- `RestrictAddressFamilies` に `AF_UNIX` を含める(NOTIFY_SOCKET 送信に必要)
- `Restart=on-failure`: 異常終了時の自動復帰(US3 シナリオ 4)。正常停止(exit 0)では
  再起動しない
- 複数インスタンス(FR-010)はテンプレート unit(`peca-p2p-yp@.service`、
  `StateDirectory=peca-p2p-yp/%i` + `ExecStart` にポート指定)で実現できることを
  quickstart で示す(SHOULD)
- `DynamicUser=yes` は採用しない(既定例は静的ユーザー — research R9)。コメントで言及可

## 3. 受け入れ検証(US3 対応)

| # | 手順 | 期待 |
|---|------|------|
| 1 | `systemctl start` | `systemctl is-active` = active(READY 通知後に start が返る) |
| 2 | `systemctl stop` | 90 秒以内に正常停止(`ExecMainStatus=0`、journal に shutdown ログ) |
| 3 | `journalctl -u peca-p2p-yp` | 起動サマリ・停止ログが読める。秘密鍵・nsec・ANSI エスケープが含まれない |
| 4 | メインプロセスを `kill -9` | `Restart=on-failure` により自動再起動 |
| 5 | `NOTIFY_SOCKET` なし(手動起動) | 通知なしで正常稼働 |

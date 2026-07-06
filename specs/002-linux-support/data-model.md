# Data Model: Linux 対応(002-linux-support)

**Date**: 2026-07-06 | **Plan**: [plan.md](./plan.md) | **Research**: [research.md](./research.md)

既存データモデル(specs/001-nostr-p2p-yp/data-model.md)への差分のみを記す。
SQLite スキーマ(テーブル・列)は **変更しない**。変更は `secret_enc` の中身の形式と、
ファイルシステム上のエンティティ・列挙型の追加である。

## 変更エンティティ

### Persona(既存 — `secret_enc` の形式変更)

| 項目 | 変更前 | 変更後 |
|------|--------|--------|
| `secret_enc` (BLOB) | 生 DPAPI BLOB(Windows 専用) | **鍵エンベロープ**(下記)。レガシー生 DPAPI BLOB も読込のみ許容 |

- 検証規則: 復号は `keystore::unprotect` 経由のみ。復号失敗・未知/他プラットフォーム
  スキームは `usable: false`(FR-006 — 既存 ADR-0003 §4 の挙動を継承)
- 状態遷移(`created → active ⇄ archived → deleted`)は不変

## 新規エンティティ

### 鍵エンベロープ(KeyEnvelope — `secret_enc` の内容形式)

保護された秘密鍵表現(spec Key Entities)の具象。contracts/key-envelope.md が正。

| フィールド | 型/サイズ | 説明 |
|-----------|----------|------|
| `magic` | 4 bytes = `"PYK1"` | エンベロープ識別。なければレガシー DPAPI BLOB |
| `scheme` | 1 byte | `0x01` dpapi-user / `0x02` xchacha20-mk-v1(将来: `0x03+` 予約) |
| `payload` | 可変 | scheme 依存(下記) |

- scheme `0x01`: `payload` = DPAPI BLOB(Windows のみ復号可)
- scheme `0x02`: `payload` = `nonce(24) || ciphertext(32+16)`。AAD = `magic || scheme`
- 不変条件: 新規書込みは常にエンベロープ形式。復号可否は (scheme, プラットフォーム,
  鍵素材) の組で決まり、不可なら当該ペルソナのみ Unusable

### マスター鍵ファイル(MasterKeyFile — Linux/unix のみ)

| 属性 | 値 | 説明 |
|------|-----|------|
| パス | `<data-dir>/master.key` | DB と同居するが別ファイル(FR-003 — DB 単体持出しで復号不能) |
| 内容 | 32 bytes 生バイナリ | OS CSPRNG で初回起動時に生成 |
| パーミッション | `0600`(必須) | 生成は `O_CREAT|O_EXCL` + mode 0600。緩ければ自動是正(FR-013) |
| ライフサイクル | 生成 → 使用 → (削除 = 全ペルソナ復号不能) | ローテーション・バックアップ機構は v1 非目標(nsec エクスポートが唯一のバックアップ — ADR-0003 §2) |

- 検証規則: 読込時にサイズ = 32 bytes を検証。不一致は「破損」として全ペルソナ Unusable +
  警告(発見・伝搬は継続)
- メモリ上の鍵素材は使用後 `zeroize`(research R2)

### プラットフォーム実行環境(PlatformPaths — 実行時導出、非永続)

data-dir の解決結果と実行文脈。contracts/cli-config.md が正。

| 属性 | 説明 |
|------|------|
| `data_dir` | 解決順: `--data-dir` > `$STATE_DIRECTORY`(unix) > OS 既定(`%APPDATA%\peca-p2p-yp` / `$XDG_STATE_HOME/peca-p2p-yp` ← 既定 `~/.local/state/peca-p2p-yp`) |
| `notify_socket` | `$NOTIFY_SOCKET`(unix・任意)。あれば READY/STOPPING を通知(FR-009) |

### サービス定義(ServiceUnit — 配布物、実行時データではない)

`contrib/systemd/peca-p2p-yp.service`。属性(Type=notify、StateDirectory、Restart、
ハードニング群)は contracts/systemd-service.md が正(FR-012)。

## 変更列挙型

### SecurityCategory(既存 12 件 → 14 件)

| 追加値 | ログ表記 | 契機 |
|--------|---------|------|
| `KeyPermissionFixed` | `key_permission_fixed` | 保管物(master.key / app.db / -wal / -shm / data-dir)の緩いパーミッションを自動是正した(FR-013) |
| `KeyPermissionUnfixable` | `key_permission_unfixable` | 是正に失敗し、影響ペルソナを利用不可とした(FR-013) |

- `SecurityCategory::ALL` は 14 件に更新(リリース前ゲートの一致確認対象)
- 記録内容に秘密鍵・鍵素材を含めてはならない(MUST NOT — FR-011)。対象パスは
  data-dir 相対名のみ記録(内部絶対パスの漏洩防止 — Principle II)

### KeystoreHealth(新規 — 実行時状態、非永続)

起動時パーミッション検査(research R7)の結果。IdentityManager の応答に影響する。

| 値 | 意味 | 影響 |
|----|------|------|
| `Ok` | 保管物は健全(是正済み含む) | 通常動作 |
| `Unavailable` | 共有保管物が是正不能・master.key 破損等 | 全ペルソナ `usable:false`、作成・署名・エクスポートは Unusable エラー。発見・伝搬(US1)は継続 |

- `Unavailable` は原因(パーミッション是正不能 / master.key 破損 / master.key 消失疑い)を
  保持し、原因ごとに異なる定型警告で記録する(contracts/key-envelope.md「障害原因の識別」—
  利用者がログから区別できること MUST)
- local-api への影響(既存 001 local-api 契約のエラー形式を再利用し、新設のエラー形式は
  導入しない):
  - ペルソナ一覧: 200 応答のまま全要素 `usable: false`(復号不能ペルソナの既存表現と同一)
  - 作成・署名(掲載)・エクスポート・破棄などの鍵操作: 復号不能ペルソナへの操作と同一の
    既存「利用不可」エラー応答(ADR-0003 §4 の挙動を共有保管物起因へ拡張)
  - 発見・伝搬系(index.txt・チャンネル一覧・gossip・ピア管理): 影響なし(FR-013 MUST)

## 関係図(差分)

```text
Persona.secret_enc ──(形式)── KeyEnvelope ──(scheme 0x02 の復号)── MasterKeyFile
IdentityManager ──(参照)── KeystoreHealth ←──(起動時検査)── PlatformPaths.data_dir
SecurityLog ←── KeyPermissionFixed / KeyPermissionUnfixable
```

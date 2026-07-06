# Contract: 鍵エンベロープと master.key(002-linux-support)

**Principles**: I, II | **FR**: FR-003, FR-004, FR-005, FR-006 | **正**: 本書(実装・テストは本書に従う)

`personas.secret_enc`(SQLite BLOB)の内容形式と、Linux マスター鍵ファイルの契約。
確定後は docs/adr/0008-linux-key-protection.md に決定として転記する。

## 1. エンベロープ形式

```text
secret_enc := magic || scheme || payload
  magic  : 4 bytes  = 0x50 0x59 0x4B 0x31 ("PYK1")
  scheme : 1 byte
  payload: 可変長(scheme 依存)
```

| scheme | 名称 | payload | 復号可能条件 |
|--------|------|---------|--------------|
| `0x01` | dpapi-user | DPAPI BLOB(CryptProtectData / ユーザースコープ / UI 禁止) | Windows + 同一ユーザープロファイル |
| `0x02` | xchacha20-mk-v1 | `nonce(24) || ct_and_tag(48)` | unix + 復号可能な `master.key` |
| その他 | (予約) | — | 常に Unusable(前方互換: エラーにせず利用不可扱い) |

### scheme 0x02 の暗号仕様

- AEAD: XChaCha20-Poly1305(RustCrypto `chacha20poly1305`)
- key: `master.key` の 32 bytes
- nonce: 暗号化ごとに OS CSPRNG で 24 bytes 生成し payload 先頭へ前置
- AAD: `magic || scheme`(= `"PYK1" || 0x02`)— エンベロープヘッダの改竄で復号失敗となること
- 平文: ペルソナ秘密鍵 32 bytes(nostr secp256k1 secret key)

## 2. 読込(unprotect)規則

1. 先頭 4 bytes が `"PYK1"` → scheme で分岐
   - 現プラットフォームで復号可能な scheme → 復号を試行。失敗(tag 不一致・DPAPI エラー)は
     **Unusable**
   - 復号不能な scheme(他プラットフォーム由来・未知値)→ **Unusable**(パニック・起動失敗に
     してはならない MUST NOT — FR-006)
2. magic なし → **レガシー生 DPAPI BLOB** とみなす
   - Windows: 従来どおり `CryptUnprotectData` を試行(既存 DB の後方互換 MUST)
   - unix: **Unusable**
3. Unusable は当該ペルソナのみに影響し、起動・他ペルソナ・発見伝搬機能は継続する(MUST)

## 3. 書込(protect)規則

- 新規作成・再暗号化は**常に**エンベロープ形式で書く(MUST)。レガシー形式での新規書込みは
  禁止(MUST NOT)
- Windows → scheme 0x01、unix → scheme 0x02
- 既存レガシー BLOB の一括マイグレーションは行わない(読込のみ後方互換)。**使用時の
  書き換え(再暗号化)も行わない**(読込専用) — レガシー形式が当該ペルソナの破棄まで
  無期限に残存することは受容する。DPAPI 保護自体は scheme 0x01 と同水準であり、残存が
  保護水準を下げることはなく、書換え契機を作らないことで障害面を増やさない
- 新形式で保管した後の旧バージョンへのロールバックはスコープ外。旧実装はエンベロープを
  DPAPI BLOB として復号を試みて失敗し、既存挙動(ADR-0003 §4)どおり当該ペルソナのみ
  利用不可となる(起動失敗・データ破壊は生じない)

## 4. セキュリティ不変条件

- 平文秘密鍵は永続化しない(MUST NOT — FR-003)。ログ・セキュリティイベント・エラー文言にも
  出さない(MUST NOT — FR-011 / ADR-0003 §2)
- 復号は書込み時と同一プラットフォーム系でのみ成立し、DB 単体の持出しでは復号できない
  (MUST — FR-003: scheme 0x02 は master.key が必須、scheme 0x01 は DPAPI ユーザー鍵が必須)
- 書込みは常に最新形式のため、形式ダウングレードの攻撃面は存在しない
- メモリ上の平文・鍵素材は使用後に消去する(zeroize — SHOULD)。対象範囲: マスター鍵
  (32 bytes)・復号後のペルソナ秘密鍵・protect/unprotect の中間バッファ。MUST としないのは
  Rust ではムーブ・コピー・レジスタ/スワップ残留まで消去を保証できず best-effort に留まる
  ため(プロセスメモリを読める主体は下記のとおり脅威モデル外)
- 秘密鍵・nsec の非出力(FR-011)の検査範囲: hex(64 桁)・bech32(`nsec1…`)・その部分
  文字列・`Debug`/`Display` 表現を含む。鍵素材を保持する型は `Debug` 出力で内容を表示しない
  (redacted)こと(MUST)
- **脅威モデルの限界(受容 — spec Assumptions と同一)**: root など data-dir 全体
  (master.key 込み)を読める主体、および実行アカウント自身のプロセスメモリを読める主体には
  保護されない。scheme 0x02 の他アカウント遮断(FR-004)はファイルパーミッションのみに
  依存し、DPAPI(scheme 0x01)のユーザー鍵への暗号学的拘束より保証水準は低い — 自己完結
  制約(FR-003/FR-005)とのトレードオフとして受容する

## 5. master.key(unix)

| 項目 | 契約 |
|------|------|
| パス | `<data-dir>/master.key` |
| 内容 | 32 bytes 生バイナリ(OS CSPRNG) |
| 生成 | keystore 初期化時(起動時・リスナーバインド前)に存在しなければ `O_CREAT|O_EXCL` + mode `0600` で原子的に作成(TOCTOU 回避 MUST)。対話操作を要求しない(MUST NOT — FR-005)。`O_EXCL` が `EEXIST` で失敗した場合(並行生成の競合)は既存ファイルの読込へフォールバックし、両プロセスが同一鍵に収束する(MUST)。生成時点で DB に scheme 0x02 のペルソナが既に存在する場合は「保護鍵消失の可能性」を示す警告を記録してから生成する(MUST — 暗黙の新鍵生成により既存ペルソナが Unusable になる事象を利用者が識別できること) |
| サイズ検証 | 読込時に 32 bytes 一致を検証。不一致 = 破損 → 全ペルソナ Unusable + 警告、発見・伝搬は継続 |
| パーミッション | `0600`。緩ければ自動是正 + `key_permission_fixed` 記録。是正不能 → `key_permission_unfixable` + 全ペルソナ Unusable(FR-013) |
| 削除時 | 全ペルソナ復号不能(= Unusable)。復元手段は提供しない(ADR-0003 §3 と同思想。nsec エクスポートが唯一のバックアップ) |

- 同一 data-dir での複数プロセス同時稼働はサポート外(FR-010 は data-dir の個別指定を
  要求する)。上記 `EEXIST` フォールバックは生成競合を安全に収束させるための規定であり、
  同時稼働の保証ではない(DB アクセスの整合は SQLite のファイルロックに委ねる)

### 障害原因の識別(FR-006/FR-013 — 利用者向け区別可能性)

保管物起因で利用不可となる各原因は、それぞれ**異なる定型警告メッセージ**で記録し、利用者が
ログ・表示から区別できなければならない(MUST):

| 原因 | 識別(ログ・警告の要旨) | 影響範囲 |
|------|------------------------|----------|
| master.key 破損(サイズ不一致) | 「保護鍵ファイルが破損している」 | 全ペルソナ Unusable |
| パーミッション是正不能 | 「保管ファイルのアクセス権を是正できない」+ `key_permission_unfixable` | 全ペルソナ Unusable |
| master.key 消失疑い(暗号化済みペルソナ存在下での新規生成) | 「保護鍵が見つからないため新規生成した。既存ペルソナは復号できない」 | 既存 scheme 0x02 ペルソナ Unusable |
| 個別ペルソナの復号失敗(tag 不一致・他プラットフォーム scheme・レガシー持込) | 当該ペルソナのみ `usable:false`(一覧・UI 表示)。全体警告は出さない | 当該ペルソナのみ |

メッセージは定型文とし、鍵素材・絶対パス・内部実装詳細を含めない(MUST NOT — FR-011/FR-014)。

## 6. 契約テスト(tests/contract/key_envelope.rs)

| # | 検証 | 対応シナリオ |
|---|------|-------------|
| 1 | protect の出力が `PYK1` + 現プラットフォーム scheme で始まり、平文 32 bytes を含まない | spec「平文非保存」 |
| 2 | roundtrip: protect → unprotect で平文一致 | US2 シナリオ 1 |
| 3 | payload/tag を 1 bit 破壊 → Unusable(パニックしない) | FR-006 |
| 4 | 他プラットフォーム scheme・未知 scheme → Unusable | spec「復号不能データの隔離」 |
| 5 | (windows) magic なしレガシー BLOB が復号できる | 後方互換 MUST |
| 6 | (unix) magic なし BLOB → Unusable | 読込規則 2 |
| 7 | (unix) master.key 欠如時、keystore 初期化がファイルを 0600 で生成する(既存の暗号化済みペルソナがある場合は警告が記録される) | §5 生成 |
| 8 | (unix) 別の master.key では復号できない | FR-004(別アカウント相当) |
| 9 | (unix) AAD(scheme byte)改竄で復号失敗 | §1 AAD |

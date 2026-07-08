# Data Model: 読み取り専用 index.txt の LAN 公開

**Date**: 2026-07-08 | **Plan**: [plan.md](plan.md) | **Research**: [research.md](research.md)

## 1. Settings(既存エンティティの拡張)

settings テーブル(rusqlite)に 1 キー追加。13→14 キー。

| フィールド | キー | 型 | 既定値 | 検証 |
|-----------|------|----|--------|------|
| `index_bind` | `"index_bind"` | `String` | `""`(機能無効) | 空 = 検証スキップ。非空 = `require_lan_or_loopback`(下記 §2) |

- **load**: 未保存・解釈不能は既定値へフォールバック(既存の lenient 規約)
- **save**: 全キー書出し(既存規約)
- **CLI 上書き**: `CliOverrides.index_bind: Option<String>`(`--index-bind`)
- **再起動要求**: バインド系キー(`BIND_KEYS`)に追加(3→4)。変更保存時は
  `restart_required: true` / `restart_keys: ["index_bind", ...]` を応答(既存形)

## 2. 検証規則 `require_lan_or_loopback`(新規、config.rs)

入力: 設定値文字列 → `SocketAddr` パース(失敗 = `InvalidBind`)→ IP を
`to_canonical()` で正規化 → 許可リスト判定。

### 判定テーブル(ゴールデン/ネガティブ — ユニットテストと 1:1 対応)

| 入力アドレス | 分類 | 判定 |
|--------------|------|------|
| `127.0.0.1:7180` / `[::1]:7180` | loopback | ✅ 受理 |
| `192.168.1.10:7180` / `10.0.0.5:7180` / `172.16.0.1:7180` | RFC 1918 | ✅ 受理 |
| `172.31.255.254:7180` | RFC 1918(172.16/12 の上端境界) | ✅ 受理 |
| `169.254.10.1:7180` | IPv4 リンクローカル | ✅ 受理 |
| `[fe80::1]:7180` | IPv6 リンクローカル(fe80::/10) | ✅ 受理 |
| `[febf:ffff::1]:7180` | fe80::/10 の上端境界 | ✅ 受理 |
| `[fe80::1%3]:7180` | ゾーン ID(数値)付きリンクローカル — 書式解釈可能なら検証通過、bind 失敗は縮退(spec Edge Cases) | ✅ 受理 |
| `[fd12:3456::1]:7180` / `[fc00::1]:7180` | IPv6 ULA(fc00::/7) | ✅ 受理 |
| `[fdff:ffff::1]:7180` | fc00::/7 の上端境界 | ✅ 受理 |
| `[::ffff:192.168.1.10]:7180` | v4-mapped(canonical 化で private) | ✅ 受理 |
| `0.0.0.0:7180` / `[::]:7180` | unspecified | ❌ `NonLanBind` |
| `203.0.113.5:7180` / `[2001:db8::1]:7180` | グローバルユニキャスト | ❌ `NonLanBind` |
| `100.64.0.1:7180` / `100.127.255.254:7180` | CGNAT / 共有アドレス空間 | ❌ `NonLanBind`(spec 確定: 含めない) |
| `100.63.255.254:7180` | CGNAT 直前(グローバル扱い — 判別境界にならず両側拒否) | ❌ `NonLanBind` |
| `172.32.0.1:7180` | RFC 1918 の境界外 | ❌ `NonLanBind` |
| `[fec0::1]:7180` | fe80::/10 の直外(旧 site-local) | ❌ `NonLanBind` |
| `[fe00::1]:7180` | fc00::/7 の直外 | ❌ `NonLanBind` |
| `192.168.1.10`(ポート欠落)/ `a,b`(複数)/ `[fe80::1%eth0]:7180`(非数値ゾーン — std パース不可)/ 空白等 | 書式不正 | ❌ `InvalidBind` |
| `""`(空文字) | 機能無効 | ✅(検証自体をスキップ — `validate()` 側) |

### ConfigError の追加

| variant | Display(内部情報なし) | HTTP 写像(settings PUT) |
|---------|------------------------|---------------------------|
| `NonLanBind { key }` | `{key} は loopback または LAN 内のプライベートアドレスのみ指定できます` | 400 + 定型コード(例: `"non_lan_bind"`) |

既存 `NonLoopbackBind` / `InvalidBind` は不変(`http_bind` / `pcp_bind` の意味を変えない)。

## 3. IndexLanStatus(新規、web/mod.rs — 実行時状態)

起動時に一度だけ確定する不変値。`AppState.index_lan: Option<Arc<IndexLanStatus>>`
(`None` = `index_bind` 空 = 機能無効)。

| フィールド | 型 | 意味 |
|-----------|----|------|
| `bind` | `String` | 設定されたバインド先(検証済み値の文字列表現) |
| `listening` | `bool` | bind 成功して待受中か |
| `error` | `Option<&'static str>` | 失敗理由の定型コード(`addr_in_use` / `permission_denied` / `addr_not_available` / `unknown`)。`listening: true` なら `None` |

### 状態遷移(3 状態 — spec Key Entities「露出状態」)

```text
index_bind = ""        → 無効     (AppState.index_lan = None)
index_bind 非空 + bind 成功 → 露出中   (Some { listening: true,  error: None })
index_bind 非空 + bind 失敗 → 設定有効だが失敗 (Some { listening: false, error: Some(code) })
```

状態は起動時に確定し、実行中は変化しない(`index_bind` 変更は再起動要求 — FR-004)。

## 4. SecurityCategory(既存 enum の拡張)

| variant | `as_str()` | 記録契機 | source | 集約 |
|---------|-----------|---------|--------|------|
| `IndexTxtLanExposed` | `"index_txt_lan_exposed"` | 起動時、`index_bind` が**非 loopback** かつ bind **成功**のとき 1 件 | バインドアドレス文字列 | 既存の窓集約に従う(起動時 1 件のため実質単発) |

- `ALL` 配列 14→15、網羅テスト(`ALL` を舐めて `as_str` 重複なし)を同時更新
- **記録しない**ケース: `index_bind` が loopback 値 / bind 失敗 / 機能無効
- 既存カテゴリと異なり「違反の拒否」ではなく「利用者が選んだ露出状態の監査」である旨を
  doc コメントに明記

## 5. `GET /api/v1/status` 応答の拡張

既存応答に `index_txt_lan` オブジェクトを追加(詳細は
[contracts/index-txt-lan.md](contracts/index-txt-lan.md) §3)。

| JSON パス | 型 | 無効時 | 露出中 | 失敗時 |
|-----------|----|--------|--------|--------|
| `index_txt_lan.enabled` | bool | `false` | `true` | `true` |
| `index_txt_lan.bind` | string \| null | `null` | 設定値 | 設定値 |
| `index_txt_lan.listening` | bool | `false` | `true` | `false` |
| `index_txt_lan.error` | string \| null | `null` | `null` | 定型コード |

## 6. 関係図

```text
Settings.index_bind ──validate()──▶ require_lan_or_loopback(§2)
        │                                   │ 拒否: NonLanBind / InvalidBind
        │ 非空(検証済み)
        ▼
main.rs §15.5: TcpListener::bind ──成功──▶ build_index_router(AppState clone)
        │                        │          + SecurityLog.log(IndexTxtLanExposed)※非 loopback 時
        │                        └─────────▶ IndexLanStatus { listening: true }
        └──失敗(縮退継続)────────────────▶ IndexLanStatus { listening: false, error }
                                            │
AppState.index_lan ◀────────────────────────┘
        │
        ▼
GET /api/v1/status → index_txt_lan(§5)
```

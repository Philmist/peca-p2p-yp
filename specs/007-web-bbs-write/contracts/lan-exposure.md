# Contract: LAN 公開(設定キー・検証規則・監査)

**Feature**: 007-web-bbs-write | **参照**: [research.md](../research.md) R1/R7/R8、
[data-model.md](../data-model.md)、ADR-0012(踏襲元)、ADR-0015(本機能で新規)

対象は Web UI + JSON API(`http_bind`)と 2ch 互換 API(`compat_bbs_bind`)の LAN 公開。
index.txt(`index_bind` — ADR-0012)の挙動は変更しない。

## 1. 設定キー

| キー | 型 / 既定 | 意味 |
|------|-----------|------|
| `http_bind` | string / `127.0.0.1:7180` | Web UI + JSON API の待受アドレス。loopback または LAN 内プライベートアドレスを許可 |
| `http_lan_consent` | bool / `false` | `http_bind` 非 loopback 時に必須の明示同意 |
| `compat_bbs_bind` | string / `127.0.0.1:7183`(空 = 無効) | 互換 API の待受アドレス。同上 |
| `compat_bbs_lan_consent` | bool / `false` | `compat_bbs_bind` 非 loopback 時に必須の明示同意 |

公開面ごとに独立(片面のみの LAN 公開が可能)。CLI 上書きにも同キーを提供する。

## 2. bind アドレス検証規則(FR-008 / SC-003)

判定は必ず `ip.to_canonical()` で正規化してから行う(v4-mapped 誤判定防止)。

| 指定アドレス | 判定 |
|--------------|------|
| loopback(`127.0.0.0/8`, `::1`) | 許可(同意キー不要) |
| RFC 1918(`10/8`, `172.16/12`, `192.168/16`) | 許可(同意キー必須) |
| IPv4 リンクローカル(`169.254/16`) | 許可(同意キー必須) |
| IPv6 リンクローカル(`fe80::/10`)・ULA(`fc00::/7`) | 許可(同意キー必須) |
| 未指定(`0.0.0.0`, `::`) | **拒否**(設定エラー) |
| CGNAT(`100.64.0.0/10`) | **拒否** |
| グローバルユニキャスト・その他 | **拒否** |
| v4-mapped(`::ffff:192.168.x.y` 等) | 正規化後の実体で上記判定 |

- 非 loopback 許可アドレスでも、対応する `*_lan_consent` が `false` なら**設定エラー**
  として起動/適用を拒否する(FR-010 の明示確認 — 2 要素オプトイン)
- 検証位置: `Settings::validate`(config ロード・CLI 上書き・設定 UI 適用のすべての経路)

## 3. Host ヘッダ検証(FR-009)

ホワイトリスト(完全一致):

- 常に: `127.0.0.1:{port}` / `localhost:{port}` / `[::1]:{port}`
- 非 loopback bind 時に追加: `{bind_ip}:{port}`(IPv6 は `[{bind_ip}]:{port}`)

ホワイトリスト外・Host 欠落は 403 で拒否し、既存の拒否ログ(`forbidden_host` /
`compat_bbs_denied`)に記録する。ホスト名(mDNS 等)は許可しない(IP リテラルのみ)。

## 4. 送信元 IP 検証(FR-021 — 多層防御)

非 loopback 待受時、接続の送信元 IP を `to_canonical()` 正規化後に検証し、
loopback / RFC 1918 / リンクローカル / ULA 以外からのリクエストは 403 で拒否 +
セキュリティログ記録する。Web UI 面・互換 API 面の双方に適用する。

## 5. 認証と保護水準(FR-011 / FR-021 / SC-008)

| 面 | 認証 | 保護(loopback と同一水準 — 緩和なし) |
|-----|------|------|
| Web UI + JSON API | GET 不要 / 変更系 `X-Api-Token`(トークンは `GET /api/v1/token` で LAN 内に配布 — 平文共有は受容リスク) | per-IP 20 req/秒・ボディ上限・URL/ヘッダ上限 |
| 互換 API | **無認証**(専ブラはトークンを送れない — MAY 受理) | per-IP 20 req/秒・ボディ 64KB・Host 検証・送信元 LAN 限定 |

書き込みの検証(署名・PoW・レート制限・名前欄 `#` 除去・サイズ上限)は書き込み元に
よらず 006 と同一(FR-003/FR-012/FR-022)。

## 6. 明示確認と監査(FR-010 / SC-004)

- 設定 UI(`ui/settings.html`)は非 loopback bind の保存時に「LAN 内で平文・実質無認証の
  まま読み書きされうる」旨の警告を表示し、確認操作で `*_lan_consent` を立てて保存する
- 非 loopback 待受に**成功した起動時**、面ごとに SecurityEvent を 1 件記録する:
  `WebUiLanExposed` / `CompatBbsLanExposed`(`IndexTxtLanExposed` と同パターン)
- bind 失敗は縮退継続(致命でない — ADR-0012 決定 4 踏襲)

## 7. Gherkin シナリオ(ネガティブ含む — Principle IV)

```gherkin
Feature: 書き込みを含む LAN 公開の境界 (Principle I, II)

  Scenario: グローバルアドレスへの公開拒否
    Given http_bind に "203.0.113.5:7180" が指定されている
    When 設定を検証する
    Then 設定エラーとなり待受は開始されない

  Scenario: 未指定アドレスの拒否
    Given compat_bbs_bind に "0.0.0.0:7183" が指定されている
    When 設定を検証する
    Then 設定エラーとなる

  Scenario: CGNAT アドレスの拒否
    Given http_bind に "100.64.1.2:7180" が指定されている
    When 設定を検証する
    Then 設定エラーとなる

  Scenario: 同意キーなしの LAN bind 拒否
    Given http_bind に "192.168.1.10:7180" が指定され http_lan_consent が false
    When 設定を検証する
    Then 設定エラーとなる(明示確認なしに公開されない)

  Scenario: v4-mapped プライベートアドレスの正規化判定
    Given http_bind に "[::ffff:192.168.1.10]:7180" が指定され http_lan_consent が true
    When 設定を検証する
    Then 正規化後にプライベートと判定され許可される

  Scenario: LAN 公開起動の監査記録
    Given http_bind "192.168.1.10:7180" と http_lan_consent true
    When 起動して待受に成功する
    Then SecurityEvent WebUiLanExposed が 1 件記録される

  Scenario: ホワイトリスト外 Host の拒否
    Given LAN 公開中の Web UI
    When Host ヘッダ "evil.example:7180" のリクエストが届く
    Then 403 で拒否されセキュリティログに記録される

  Scenario: 許可範囲外の送信元の拒否
    Given LAN 公開中の互換 API
    When 送信元 IP がグローバルアドレスのリクエストが届く
    Then 403 で拒否される

  Scenario: LAN 側のレート制限は loopback と同一
    Given LAN 公開中の互換 API
    When 同一送信元から 20 req/秒を超えるリクエストが届く
    Then 429 で拒否される(loopback 側と同じ上限)
```

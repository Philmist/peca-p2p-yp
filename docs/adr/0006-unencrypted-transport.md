# ADR-0006: P2P トランスポート非暗号化と平文 HTTP の判断

**Status**: Accepted
**Date**: 2026-07-03
**Superseded (partial)**: 決定 4 は read-only index.txt に限り
[ADR-0012](0012-index-txt-lan-exposure.md) が部分 supersede する(2026-07-08)。
HTTP API / UI / PCP の loopback 強制は不変
**Principles**: Principle I (Safety First), Principle II (Security by Design), Principle VI (Principle Traceability)
**Task**: T008(Phase 2 実装前ゲート)。LAN 公開オプトインの v1 実装可否(デッドライン:
Phase 2 実装前ゲート完了)を本 ADR で確定する

## 背景

P2P gossip(contracts/p2p-gossip.md)は平文 TCP、index.txt(contracts/http-yp.md)は
plain HTTP(ユーザー要求)で供給する設計である。plan §Constitution Check はこの判断の
ADR 化を義務づけ、contracts/p2p-gossip.md §脅威と対応範囲は「イベント署名の保護範囲」の
明示を本 ADR に委譲している。

## 決定 1: P2P gossip はトランスポート暗号化を行わない(平文 TCP)

- **完全性・真正性**: 掲載情報(イベント)は secp256k1 Schnorr 署名で保護される。
  経路上で改ざんされたイベントは受信検証 2(署名)で破棄される
- **機密性**: 掲載情報は全参加者に公開するためのデータであり、機密性要件がない
- **自前暗号の不在**: 平文トランスポートは「自前の暗号アルゴリズム禁止」(Principle II)に
  抵触する要素を持たない。署名は既存ライブラリ(`nostr` クレート)に委ねる

## 決定 2: イベント署名の保護範囲の明確化(制御メッセージは保護外)

「完全性はイベント署名で担保」の適用範囲は **`EVENT` メッセージ内のイベント JSON のみ**である。
制御メッセージ(HELLO / HELLO_ACK / SYNC_REQ / SYNC_DONE / GET_PEERS / PEERS / PING / PONG /
CLOSE)の経路上改ざんは署名保護の範囲外であり、メッセージごとに影響を評価して受容する:

| メッセージ | 経路上改ざんの影響 | 緩和 / 受容根拠 |
|-----------|--------------------|-----------------|
| HELLO/HELLO_ACK `listen_port` | 偽ポートの申告 | もともと未検証の申告値。実接続検証(verified)を経るまで使用しない(Principle II) |
| HELLO/HELLO_ACK `ts` | 偽の時刻申告 | 通知のみに使用(中央値判定)。イベント検証・接続判断に使用禁止(MUST NOT — 契約に明記済み) |
| HELLO `version` / `nonce` | 非互換偽装・自己接続誤検出 | 切断・候補除外に至るが、経路上攻撃者は TCP RST でも同じ結果を起こせる(下記) |
| **PEERS(ピアリスト毒入れ)** | 偽アドレスの注入 | **実接続検証+未検証再共有禁止+受信検査 5**(contracts/p2p-gossip.md)。ADR-0004 §3 の反射評価も参照 |
| SYNC_REQ `since` | 応答範囲の操作 | 応答側は `created_at ≥ max(since, now − freshness_window_sec)` でクランプ(契約)— 拡大不可 |
| CLOSE 偽造 / セッション切断 | 可用性(切断) | 経路上攻撃者(on-path)は暗号化しても TCP RST・パケット遮断で常に切断できる。暗号化は可用性を守らない |

**受容の中核論拠**: 経路上攻撃者に対して暗号化が守れるのは機密性(要件なし)と
完全性(イベントは署名済み)であり、残る攻撃(切断・遮断)は暗号化では防げない。
一方 TLS/Noise の導入は、(a) 証明書または鍵配布という新たな中央依存点(FR-002/FR-014 の
動機と矛盾)か TOFU の弱い保証、(b) 依存グラフと監査面積の増大(research R13 が libp2p を
却下した理由と同根)をもたらす。費用対効果が成立しないため v1 では採用しない。

## 決定 3: index.txt を plain HTTP で供給するリスクの受容

- 既定バインドは `127.0.0.1:7180`(loopback のみ)であり、改ざん・盗聴は同一ホスト内に
  限られる — この前提込みでリスクを受容する(contracts/http-yp.md 冒頭)
- plain HTTP は既存 YP ブラウザ互換(FR-004)のユーザー要求でもある
- LAN 公開時の扱いは決定 4 に従う(v1 では LAN 公開自体が不可)

## 決定 4: LAN 公開オプトイン(HTTP/PCP)は v1 では実装しない

- **方式**: `pcp_bind` / `http_bind` は **loopback アドレスのみ受理**する。
  `PUT /api/v1/settings` で非 loopback 値が指定された場合は 400(定型エラー)で拒否する
  (実装先: T013 の設定検証 + T062 の設定 API)。`p2p_bind` は従来どおり外部露出可
  (唯一の外部露出ポート — research R12)。PCP の「loopback 以外からの接続は即切断」
  (contracts/pcp-announce.md)は v1 では常時適用となる
- **理由**:
  1. **最小権限・攻撃面最小化**(Principle II): 平文経路上の `X-Api-Token` 盗聴・PCP への
     LAN 内接続という追加リスクを、警告受容型(利用者の判断に委ねる)で持ち込むより、
     v1 では構造的に排除する方が安全側
  2. **需要が未確認**: PeerCastStation・YP ブラウザは同一 PC での利用が典型。
     別マシン構成の需要は現時点で確認されていない(YAGNI)
  3. **回避策の存在**: どうしても必要な利用者は OS 機能(`netsh interface portproxy` 等)で
     自己責任の転送を構成できる。本ソフトウェアが安全性を保証しない経路であることが明確になる
- **将来の解禁条件**(本 ADR の改訂を要する):
  1. contracts/local-api.md §保護方針の警告 2 項目((1) 攻撃面が LAN 全体へ拡大、
     (2) 平文 HTTP のため `X-Api-Token` を含む全トラフィックが LAN 内で盗聴可能)を
     設定画面に MUST で実装する(T062 相当)
  2. 有効化は明示的な確認操作を伴うオプトインとし、既定は loopback のまま変えない
- **帰結**:
  - T062(設定 API/UI)の LAN 公開警告 2 項目は v1 では不要。代わりに非 loopback 値の
    拒否検証を実装する
  - T026(PCP セッション)の「LAN 公開オプトイン無効の間は loopback 外を即切断」は
    v1 では恒真の条件となる

## 否定した選択肢

- **TLS(自己署名証明書)** — 検証基盤がなく MITM 耐性が実質 TOFU。証明書管理の複雑性のみ増える
- **Noise Protocol / libp2p secio 相当** — 鍵配布・アイデンティティ管理の導入が必要になり、
  ピアの匿名参加(アカウント不要)と依存最小化に反する(research R13)
- **制御メッセージへの署名付与** — ノード鍵(ペルソナと別の恒久鍵)の導入が必要になり、
  ノードの追跡可能性(プライバシー低下)と鍵管理の複雑化を招く。守れるのは上表のとおり
  限定的で、可用性攻撃は防げない
- **LAN 公開オプトインの v1 実装(警告つき)** — 決定 4 の理由 1〜3 により見送り。
  契約(local-api.md)の警告要件は将来の解禁条件として維持する

## 原則参照

- Principle I: 誤った安全性の期待(暗号化すれば安全・警告すれば安全)を作らない
- Principle II: 最小権限(loopback 強制)・trust nothing(申告値の不使用)・自前暗号の不在
- Principle VI: plan §Constitution Check「P2P トランスポート非暗号化の判断」の ADR 化義務の履行
- FR-002 / FR-004 / FR-014、research R12 / R13、contracts/p2p-gossip.md / http-yp.md / local-api.md / pcp-announce.md

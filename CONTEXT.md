# CONTEXT — peca-p2p-yp

分散型配信情報共有ネットワーク(YP 代替)。中央 YP サーバーを利用者ノードのみの
純粋 P2P ネットワークで置き換える Windows 用の単一 Rust バイナリ。
nostr の援用は**イベント形式・署名(データスキーマ)に限定**し(FR-014)、伝送は
独自 gossip プロトコルで行う。仕様は `specs/001-nostr-p2p-yp/`(spec / plan / data-model /
contracts / research)、設計判断は `docs/adr/` を正とする。

## 用語(glossary)

| 用語 | 意味 | 避ける同義語 |
|------|------|--------------|
| **チャンネル(配信情報)** | 1 つの配信。kind 30311 イベントとして掲載され、`(author_pubkey, channel_id)` で識別される | 「番組」「ストリーム」 |
| **ペルソナ** | 発信者識別子 = nostr 鍵ペア。複数保持・切替・破棄でき、相互にリンクされない(FR-013) | 「アカウント」「ユーザー」 |
| **利用者ノード(ノード)** | 本ソフトウェアの 1 プロセス。配信者・視聴者の区別なく同一 | 「サーバー」「リレー」 |
| **ピア** | 自ノードから見た他の利用者ノード。手動登録 + ピア交換(PEX)で獲得 | — |
| **掲載(announce)** | PCP で受けたチャンネル情報を署名イベントとして発行・伝搬すること | 「登録」 |
| **発見(discover)** | gossip 受信イベントを多段検証して一覧を構築すること | — |
| **gossip** | ノード間の独自ワイヤプロトコル(TCP + 長さ前置 JSON フレーム)。eager-push フラッディング + 重複抑制 | 「リレー通信」(存在しない) |
| **鮮度窓** | 最終更新から 600 秒(既定)。超過チャンネルは一覧から自動除去(FR-006) | — |
| **ミュート** | pubkey / channel 単位のローカル非表示(OR 適用)。ネットワークへ出ない | 「ブロック」 |

## モジュール構成(src/)

| モジュール | 責務 | 主要契約 |
|-----------|------|----------|
| `pcp/` | PCP アナウンス受信(atom コーデック・HELO/OLEH・BCST 解析) | contracts/pcp-announce.md |
| `event/` | 30311 スキーマ・署名検証・EventStore(置換・クォータ・DedupCache)・発行エンジン・一覧ビュー | contracts/nostr-events.md |
| `p2p/` | gossip フレーミング・セッション状態機械・受信パイプライン・接続時同期・ピア管理・PEX・UPnP・ハブ | contracts/p2p-gossip.md |
| `yp/` | index.txt 生成(18 フィールド・Shift_JIS) | contracts/http-yp.md |
| `web/` | axum ルーター・ローカル JSON API・UI 静的配信・保護層(Host/トークン/レート/ボディ上限) | contracts/local-api.md |
| `identity/` | ペルソナ鍵管理(DPAPI 保管・nsec エクスポート・破棄) | ADR-0003 |
| `store/` | SQLite 永続化(personas / peers / mutes / settings) | data-model.md |
| `security/` | 入力検証ヘルパ・SecurityEvent 12 カテゴリ・ローテーション付きログ | data-model §SecurityEvent |
| `config.rs` | Settings 既定値と検証(バインド系は loopback 強制 — ADR-0006 決定 4) | data-model §Settings |
| `main.rs` | 起動配線と graceful shutdown | — |

`ui/` は Web UI 静的アセット(ビルド時埋め込み)。`event/`(スキーマ)と `p2p/`(伝送)の
分離は nostr 援用境界(FR-014)をモジュール境界で強制するためのもの — ADR-0002 §3。

## 信頼境界

| 境界 | 露出 | 検証方針 |
|------|------|----------|
| **P2P gossip(`p2p/`)** | インターネット(既定 `0.0.0.0:7147`。唯一の外部露出) | 最大の攻撃面。フレーム長 64KB → レート(256KB/s・200msg/s)→ JSON → イベント検証(サイズ 16KB→署名→形式→時刻→内容→PoW)の多段検証。違反は破棄+切断+セキュリティイベント |
| **PCP(`pcp/`)** | loopback のみ(`127.0.0.1:7146`、非 loopback は検証拒否) | 利用者自身の PeerCastStation が相手。atom ネスト ≤8・≤64KB、文字列は切詰め許容 |
| **ローカル HTTP(`web/` `yp/`)** | loopback のみ(`127.0.0.1:7180`) | Host 検証(DNS rebinding 対策)・変更系は `X-Api-Token`・レート制限・ボディ ≤64KB・定型エラー(内部情報漏洩禁止) |

横断原則: 「真に信頼できるのは自分だけ」— 他ノード由来の情報(イベント・PEX アドレス・
HELLO 申告値)はすべて自ノードで検証してから使用する(FR-015 / Principle II)。
未検証ピアの再共有禁止。トランスポートは非暗号化(完全性はイベント署名で担保 — ADR-0006)。

## 設計判断の所在(ADR)

- ADR-0001: セキュリティスキャン(cargo audit + Trivy、clippy)
- ADR-0002: kind 30311 採用・鮮度管理・nostr 援用境界・トラッカー解決の検証可能な仮定
- ADR-0003: DPAPI 鍵保管・nsec エクスポート・破棄の非可逆性
- ADR-0004: 脅威モデル(多層緩和・pubkey クォータ・PEX 残余リスク・プライバシー方針)
- ADR-0005: gossip 形式的検証「該当」判定(PlusCal モデル = `docs/formal/gossip_propagation.tla`)
- ADR-0006: トランスポート非暗号化・plain HTTP・LAN 公開オプトイン v1 非実装
- ADR-0007: 許容的ライセンス(MIT)と GPL 結合回避の根拠
- ADR-0008: P2P 待受のデュアルスタック化(`p2p_bind` カンマ区切り複数バインド)
- ADR-0009: Linux 鍵保護(keystore 抽象・エンベロープ・マスター鍵)
- ADR-0010: DNS 解決ピアの限定サポート(manual 限定・名前空間分離・resolved_ip 射影)
- security-review-checklist.md: セキュリティ PR のレビュー観点(実装中ゲート 6)
- release-gate-check-2026-07-04.md: リリース前ゲート 8〜10 の適用記録

## テスト構成(tests/)

- `features/` + `steps/` + `cucumber.rs`: spec の Gherkin シナリオ(US1〜3・着信不可・セキュリティ)
- `contract/`: 契約テスト(PCP / gossip フレーム / 30311 / 受信検証 / index.txt / local API / PEX)。
  `contract/fixtures/gossip_vectors.json` は実装とモックピアの共有フィクスチャ
- `integration/`: モックピア + 実 `P2pRuntime` の多ノード E2E(掲載・発見・障害耐性・外向きのみ・規模)
- `common/mock_peer.rs`: gossip 契約の参照実装(モックピア)+ `TestNode` ハーネス

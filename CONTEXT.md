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
| **実況スレ** | 配信中チャンネルに配信者が開設する実況掲示板。板 = 配信者ペルソナ単位でアクティブスレ高々 1 本(FR-012)。発見は gossip への announce 相乗り、配送はホスト直結の星型 | 「チャット」「コメント欄」 |
| **板鍵** | 実況スレへの書き込み身元(ペルソナとは別系統・構造分離・エクスポート不可 — FR-016)。板単位で 1 本、明示ローテーション可 | 「投稿者鍵」 |
| **凍結(Frozen)** | ホストとの接続喪失・次スレ移行で旧スレが入る状態。書き込み不可・取得済みレスの閲覧は継続 | — |
| **互換 API** | 各ノードが自分のためだけに提供する 2ch 形式互換の loopback 専用受け口(subject.txt/dat/SETTING.TXT/bbs.cgi、Shift_JIS)。自ノードホスト板のみが対象 | 「BBS API」 |

## モジュール構成(src/)

| モジュール | 責務 | 主要契約 |
|-----------|------|----------|
| `pcp/` | PCP アナウンス受信(atom コーデック・HELO/OLEH・BCST 解析) | contracts/pcp-announce.md |
| `event/` | 30311 スキーマ・署名検証・EventStore(置換・クォータ・DedupCache)・発行エンジン・一覧ビュー。`event/livechat.rs` は kind 1311(レス)/21311(順序確定情報)/31311(スレ announce)のスキーマ・直列化・検証(nostr 援用境界内 — FR-014) | contracts/nostr-events.md、contracts/thread-events.md |
| `p2p/` | gossip フレーミング・セッション状態機械・受信パイプライン・接続時同期・ピア管理・PEX・UPnP・ハブ。HELLO `features` の `livechat1` でスレ配送セッション(THREAD_JOIN 等)を同一待受に多重化(1 TCP 接続 = 1 用途) | contracts/p2p-gossip.md、contracts/thread-delivery.md |
| `livechat/` | 実況スレのホスト(採番シーケンサ・次スレ移行・明示クローズ)・参加者セッション・スレ状態機械・板鍵管理・NG/BAN。援用境界の外(nostr はイベント封筒のみ — ADR-0002 §3) | contracts/thread-delivery.md、data-model.md(006) |
| `yp/` | index.txt 生成(18 フィールド・Shift_JIS) | contracts/http-yp.md |
| `web/` | axum ルーター・ローカル JSON API・UI 静的配信・保護層(Host/トークン/レート/ボディ上限)。`web/livechat.rs` はスレ一覧・板設定・NG/BAN の操作 API。`web/compat/` は実況スレの 2ch 形式互換 API(subject.txt/dat/SETTING.TXT/head.txt/bbs.cgi)専用の第 2 loopback リスナー(`/api/v1` とは独立した状態・自ノードホスト板のみが対象) | contracts/local-api.md、contracts/compat-api.md |
| `identity/` | ペルソナ鍵管理(DPAPI 保管・nsec エクスポート・破棄) | ADR-0003 |
| `store/` | SQLite 永続化(personas / peers / mutes / settings / board_keys / livechat_moderation / board_settings) | data-model.md |
| `security/` | 入力検証ヘルパ・SecurityEvent 21 カテゴリ・ローテーション付きログ | data-model §SecurityEvent |
| `config.rs` | Settings 既定値と検証(バインド系は loopback 強制 — ADR-0006 決定 4。例外: `index_bind` のみ loopback / LAN 許可 — ADR-0012。`compat_bbs_bind` は loopback 強制) | data-model §Settings |
| `main.rs` | 起動配線と graceful shutdown | — |

`ui/` は Web UI 静的アセット(ビルド時埋め込み)。`event/`(スキーマ)と `p2p/`(伝送)の
分離は nostr 援用境界(FR-014)をモジュール境界で強制するためのもの — ADR-0002 §3。
`livechat/`(スレ配送・状態機械)も同じ境界の外側にあり、nostr の援用はイベント封筒
(`event/livechat.rs`)のみに限定される(006 spec 背景)。

## 信頼境界

| 境界 | 露出 | 検証方針 |
|------|------|----------|
| **P2P gossip(`p2p/`)** | インターネット(既定 `0.0.0.0:7147`。唯一の外部露出) | 最大の攻撃面。フレーム長 64KB → レート(256KB/s・200msg/s)→ JSON → イベント検証(サイズ 16KB→署名→形式→時刻→内容→PoW)の多段検証。違反は破棄+切断+セキュリティイベント |
| **PCP(`pcp/`)** | loopback のみ(`127.0.0.1:7146`、非 loopback は検証拒否) | 利用者自身の PeerCastStation が相手。atom ネスト ≤8・≤64KB、文字列は切詰め許容 |
| **ローカル HTTP(`web/` `yp/`)** | loopback のみ(`127.0.0.1:7180`) | Host 検証(DNS rebinding 対策)・変更系は `X-Api-Token`・レート制限・ボディ ≤64KB・定型エラー(内部情報漏洩禁止) |
| **index.txt(オプトイン時)** | LAN(`index_bind` 非空時のみ。既定は無効 — ADR-0012) | 読み取り専用 index.txt の GET/HEAD 専用の第 2 受け口。バインドは loopback / LAN 内プライベートアドレスのみ受理・それ以外は起動拒否。API/UI は物理的に非搭載(それ以外は定型 404)・サイズ上限とレート制限は loopback 側と共有・非 loopback 露出は監査イベント記録 |
| **スレ配送(`livechat/`)** | P2P gossip と同一ポート(既定 `0.0.0.0:7147`。HELLO `features` の `livechat1` で多重化) | announce(kind 31311)はチャンネル掲載ペルソナと同一署名必須(FR-003)。接続はスレを開く明示操作のみ起点(announce 受信のみでは接続しない — FR-004)。接続時チャレンジで接続先の真正性を検証(FR-005)。レス(kind 1311)はホストが多段検証(署名→形式→スレ状態→BAN→PoW→レート)後に採番、順序確定情報(kind 21311)はスレ主署名必須(FR-011) |
| **互換 API(`web/compat/`)** | loopback のみ(既定 `127.0.0.1:7183`、`compat_bbs_bind` 空文字で無効化・非 loopback は起動拒否) | `/api/v1` とは物理的に分離した専用リスナー(トークン保護を持たない代わりに Host 検証・レート制限・ボディ ≤64KB)。書き込み(bbs.cgi)は通常の書き込み経路(`LivechatRegistry::accept_write`)と完全に同一の検証を経る(FR-028 — 抜け道禁止)。**自ノードホスト板のみが対象**(リモート板は非対応) |

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
- ADR-0011: 配信中ペルソナロック(掲載前選択とリンク推定の構造的防止)+ Principle V 判定
- ADR-0012: read-only index.txt の LAN 公開オプトイン(ADR-0006 決定 4 の部分 supersede)
- ADR-0013: PEX 破棄の良性/不審分類(`pex_rejected` は不審な破棄のみ記録・良性は debug 格下げ)
- ADR-0014: 実況スレ脅威モデル追加(announce 反射攻撃・偽 ORDER・荒らし)・Principle V 該当判定(採番シーケンサ状態機械)・kind 1311/21311/31311 の採番根拠
- security-review-checklist.md: セキュリティ PR のレビュー観点(実装中ゲート 6)
- release-gate-check-2026-07-04.md: リリース前ゲート 8〜10 の適用記録

## テスト構成(tests/)

- `features/` + `steps/` + `cucumber.rs`: spec の Gherkin シナリオ(US1〜3・着信不可・セキュリティ、
  `features/livechat.feature` + `steps/livechat.rs` は実況スレ US1〜6)
- `contract/`: 契約テスト(PCP / gossip フレーム / 30311 / 受信検証 / index.txt / local API / PEX /
  `thread_events.rs`(1311/21311/31311 スキーマ)/ `thread_delivery.rs`(配送層の防御・偽 ORDER 等)/
  `compat_bbs.rs`(2ch 互換 API・dat 追記不変性・Last-Modified 単調性))。
  `contract/fixtures/gossip_vectors.json` は実装とモックピアの共有フィクスチャ
- `integration/`: モックピア + 実 `P2pRuntime` の多ノード E2E(掲載・発見・障害耐性・外向きのみ・規模。
  `livechat.rs` はスレ配送の多ノード E2E、`scale.rs` は SC-006 のスレ announce 負荷併設を含む)
- `common/mock_peer.rs`: gossip 契約の参照実装(モックピア)+ `TestNode` ハーネス。
  `common/livechat_host.rs` はスレホストの参照実装ハーネス

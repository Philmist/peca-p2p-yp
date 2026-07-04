# Contract: PCP アナウンス受信(PeerCastStation ⇔ 本ソフトウェア)

**Role**: 本ソフトウェアは PeerCastStation から見て「掲載先 YP」として振る舞う。
仕様の根拠は参考資料 gist(PCP YellowPage protocol, PeerCastStation 実装準拠)。
クリーンルーム実装であり GPL コードは参照しない(research R9)。

## 待受

- TCP `127.0.0.1:7146`(既定、設定変更可)。PeerCastStation 側は掲載 YP として
  `pcp://127.0.0.1:7146/` を指定する

## セッションフロー(受理側)

```text
accept → クライアントの PCP_HELO 受信 → PCP_OLEH 応答
      → PCP_BCST(チャンネル情報)受信を継続処理
      → playing=false の BCST で当該チャンネルが ended / PCP_QUIT・切断で全チャンネル ended
```

- `PCP_HELO` の BroadcastID(GUID)でセッションを識別する
- `PCP_OLEH` 応答には、参考資料 gist のハンドシェイク仕様に準拠した応答 atom
  (agent 名・バージョン・接続元から観測した IP・ポート)を含める。agent 名は
  `peca-p2p-yp/<semver>` を名乗る(互換性検証の識別のため固定書式とする)
- **1 セッション内の複数チャンネル**: 1 つの PCP セッション(BroadcastID)は複数チャンネルの
  BCST を含みうる。チャンネルは ChannelID 単位で AnnouncedChannel(data-model.md)を構成し、
  1 セッションあたりの同時掲載チャンネル数は ≤ 16(超過分は無視+`pcp_reject` ログ)。
  同時セッション上限(32)は TCP 接続単位で適用する
- `PCP_BCST` 内のチャンネル情報 atom(`name`/`gnre`/`desc`/`url`/`bitr`/`type`/
  `titl`/`crea`/`albm`)と `PCP_HOST`(グローバル IP:port、`numl`/`numr`、flg1)を
  data-model.md の AnnouncedChannel に写像する
- 受信内容の変更検知(または受信そのもの)を契機にイベントを再発行し gossip へ伝搬する(contracts/nostr-events.md, p2p-gossip.md)
- **セッション終了と ended**(2026-07-04 実装時改訂): `PCP_QUIT` または **TCP 切断
  (PCP_QUIT を伴わない異常切断を含む)** で当該セッションの**全チャンネル**を `ended` とし、
  `status=ended` の最終イベントを発行する(鮮度切れを待たない — data-model.md
  AnnouncedChannel の状態遷移と同一)。`playing=false` の BCST は**当該 ChannelID のみ**を
  `ended` とする — BCST はチャンネル単位の信号であり、同一セッションで複数チャンネルを
  掲載中に無関係な live チャンネルを巻き込まないため(単一チャンネル運用では従前と同一挙動)
- 本ソフトウェアから切断する場合は `PCP_QUIT` を送る。BAN 相当機能(helo_disable)は実装しない

## 入力検証(Principle II)

| 項目 | 上限/規則 | 違反時 |
|------|-----------|--------|
| atom ネスト深さ | ≤ 8 | 切断+`pcp_reject` ログ |
| 1 atom ペイロード | ≤ 64KB | 同上 |
| 1 セッション累積受信レート | ≤ 64KB/秒 | 同上 |
| 同時アナウンスセッション数 | ≤ 32 | 新規接続拒否 |
| 文字列フィールド | UTF-8 として解釈し制御文字除去、長さは data-model.md 準拠 | 超過分切詰め |
| GUID | 16 バイト固定 | 切断 |

- **未知・非対応の atom は無視する**(切断しない)。デバッグレベルでログ記録するが、
  セキュリティイベントとはしない(将来のクライアント拡張との前方互換のため)
- **切詰め許容の根拠**(gossip 側の「破棄+切断」との非対称は意図的): 文字列長超過を
  切詰めで許容するのは、送信元がローカルの PeerCastStation(loopback、利用者自身の
  ソフトウェア)であり、正当な配信を長さ超過だけで失敗させないため。インターネットに
  露出する gossip 受信(contracts/p2p-gossip.md)が違反を破棄+切断とするのは、
  Principle II の適用強度を信頼境界の露出度に応じて変えているためである
- エラー応答・ログに内部情報(パス・スタックトレース)を含めてはならない (MUST NOT)
- loopback 以外からの接続は、LAN 公開オプトインが無効の間は即切断する

## 明示的な非対応(v1)

- Tracker Lookup(`GET /channel/<id>` + HTTP 503 + PCP_HOST 応答)は v1 では提供しない。
  視聴側は index.txt の TIP フィールドでトラッカーへ到達する。将来要望があれば追加
- ルート YP 間のホスト転送(PCP_BCST の中継ネットワーク)は実装しない(P2P gossip が代替)

## 検証方法

- `tests/contract/`: HELO→OLEH→BCST→QUIT のフィクスチャバイト列で往復を検証
- 統合テスト: PCP 疑似クライアント(テスト用実装)で announce→イベント発行・伝搬→ended までを通す
- 受け入れ: 実機 PeerCastStation からの掲載(quickstart.md 手順 3)

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
      → playing=false の BCST または PCP_QUIT で ended
```

- `PCP_HELO` の BroadcastID(GUID)でセッションを識別する
- `PCP_BCST` 内のチャンネル情報 atom(`name`/`gnre`/`desc`/`url`/`bitr`/`type`/
  `titl`/`crea`/`albm`)と `PCP_HOST`(グローバル IP:port、`numl`/`numr`、flg1)を
  data-model.md の AnnouncedChannel に写像する
- 受信内容の変更検知(または受信そのもの)を契機にイベントを再発行し gossip へ伝搬する(contracts/nostr-events.md, p2p-gossip.md)
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

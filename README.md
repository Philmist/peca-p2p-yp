# peca-p2p-yp

分散型配信情報共有ネットワーク(YP 代替)— PeerCast の YellowPage(YP)を、
中央サーバーに依存しない純粋 P2P で置き換えるソフトウェアです。

## これは何か

PeerCastの視聴は現在YellowPage(YP)に頼っており、
YPが運営を断念するとPeerCast自体がほぼ使えなくなるという事態に陥ることになります。
実際過去にはCPやTPといったYPが閉鎖することで利用者が新規YPへ移動するという事態が発生しました。
もしも既存YPが閉鎖した時に他のYPがなかったらどうなるでしょうか？

本ソフトウェアは配信者と視聴者が特定のYPサーバーを介さず、
利用者ノード間の直接通信(gossip 型 P2P)でYPに載っている情報である
チャンネル一覧・トラッカー情報を共有できるようにすることで
特定のYP運営者に頼らないことを目指しています。

- **リレーサーバー不要** — ネットワークは利用者ノードのみで構成されます。
- **既存クライアントと互換** — PeerCastStation 等の既存 PeerCast クライアントを
  改造せずに、従来の YP と同じように利用できます。
- **nostr スキーマを援用** — イベント形式・署名(NIP のデータ構造)だけを流用し、
  伝送はリレーを前提としない独自 P2P プロトコルで行います。
- **匿名フレンドリー** — 複数のペルソナ(識別子)を自由に作成・切替・破棄できます。

対応 OS: **Windows 10 / 11 (x64)** / **Linux x86_64**(systemd 採用ディストリビューション第一。systemd なしでも手動起動できます)

## 15 分ではじめる

事前に、接続先ピアのアドレス(`host:port`)を掲示板 / SNS などで 1 件以上入手しておいてください。
既定のシードピアは同梱されません。

### 1. 入手して起動する

1. 配布された `peca-p2p-yp.exe` を任意のフォルダに置きます(インストール不要)。
2. `peca-p2p-yp.exe` を実行します。
3. ブラウザで `http://127.0.0.1:7180/` を開きます(操作画面はすべてこの UI から行います)。

### 2. ペルソナを作る(配信する場合)

- ペルソナ画面で新規作成します(label は任意・ローカル専用)。
- 秘密鍵は端末内に暗号化して保管されます(Windows: DPAPI / Linux: データディレクトリ内の
  マスター鍵ファイルによる保護)。バックアップが必要な場合は
  nsec エクスポートを使ってください(**破棄すると復元できません**)。

### 3. ピアアドレスを貼り付ける

- ピア画面に、入手済みのアドレス(`host:port`、複数可)を貼り付けて一括登録します。
- 状態画面で established ピア数が 1 以上になれば接続成功です。以降はピア交換(PEX)で
  接続先が自動的に広がります。
- この時点でチャンネル一覧でピアから受けとっている配信チャンネルを見ることが出来ます。

### 4. YPをアプリに登録する

#### 4-1. PeCaRecorderの場合

- 全般の設定→YPから `http://127.0.0.1:7180/` を登録します。
- 正しく登録されていれば `http://127.0.0.1:7180/channels.html` と同じチャンネルが表示されます。

#### 4-2. PeerCastStationの場合 (HTML UI)

PeerCastStationが `127.0.0.1:7144` で待ち受けしていることを前提とします。

- `http://127.0.0.1:7144/html/settings.html` を開きます。
- YellowPage設定で以下の項目のYPを追加します。
    - プロトコル: `PCP`
    - 配信掲載URL: `pcp://127.0.0.1:7146`
    - チャンネル一覧URL: `http://127.0.0.1:7180/index.txt`

### 5. 登録したYPを使う

- `index.txt`経由で視聴する場合はこの時点で普通のYPと同様に使えるはずです。
- PeerCastStation経由で配信する場合は登録したYPを選択して配信してください。
    - ソースに"他のチャンネル"を選んで配信することで他YPと同時に掲載することが可能です。
- チャンネル一覧ページ経由での視聴は(まだ)対応していません。

## 着信できない(NAT 内の)環境について

ポート開放ができない環境でも、**外向き接続のみですべての機能を利用できます**
(掲載・発見・ピア交換すべて可能です)。この場合、UI の状態表示は
「外向き接続のみで参加中」になります。

着信可能なノードは網の維持に貢献するため、可能なら着信を有効にすることを推奨します。
本ソフトは起動時に UPnP による自動ポートマッピングを試みます(失敗しても全機能は動作します)。

### 手動でポートフォワードする場合

UPnP が使えないルーターでは、以下を手動で設定すると着信可能になります。

- 転送するポート: **P2P 待受ポート(既定 `7147`, TCP)**
- 転送先: 本ソフトを動かしている PC のプライベート IP
- UI の状態表示が着信可能に変わることを確認してください。

> HTTP UI(`7180`)と PCP 受信(`7146`)は既定で loopback(`127.0.0.1`)のみを待ち受けます。
> これらを外部公開する設定は現在提供していません(ローカルでの操作専用です)。

## 既定のポート

| 用途 | 既定バインド | 変更用オプション |
|------|--------------|------------------|
| 操作用 Web UI / `index.txt`(HTTP) | `127.0.0.1:7180` | `--http-bind` |
| PeerCastStation からの掲載受信(PCP) | `127.0.0.1:7146` | `--pcp-bind` |
| ピア間 P2P(gossip) | `0.0.0.0:7147` | `--p2p-bind` |

IPv6アドレスを指定する場合は`[::1]:7180`というようなブラケット形式で指定してください。
PCP待ち受けのIPv6アドレスは対応していません。

同一 PC で複数ノードを起動する場合は、各ポートと `--data-dir` をノードごとに分けてください。
`--data-dir`を指定することでそのディレクトリに設定ファイルを保存することが可能です。

## Linux で使う

Linux でも Windows 版と同じ機能(発見・伝搬・ペルソナ掲載)が動作します。
追加の常駐サービスやデスクトップ環境は不要で、ヘッドレス環境で無人稼働できます。

### 手動起動

```bash
cargo build --release
./target/release/peca-p2p-yp
```

- データディレクトリの既定は `$XDG_STATE_HOME/peca-p2p-yp`
  (未設定なら `~/.local/state/peca-p2p-yp`)。`--data-dir` で変更できます。
- ペルソナの秘密鍵は、データディレクトリ直下の `master.key`(パーミッション `0600`)を
  マスター鍵として暗号化保管されます。`master.key` を失うと既存ペルソナは復号できなく
  なるため、バックアップが必要な場合は nsec エクスポートを使ってください。
- 起動時にデータディレクトリ・鍵ファイルのパーミッションを検査し、緩い場合は自動的に
  是正します。是正できない場合はペルソナ機能のみ停止し、発見・伝搬は継続します。

### systemd サービスとして常時稼働させる

同梱の unit 定義例 [`contrib/systemd/peca-p2p-yp.service`](./contrib/systemd/peca-p2p-yp.service)
で登録できます(READY 通知・SIGTERM での安全終了・異常時自動再起動・ハードニング設定済み)。

```bash
# 専用のサービスアカウント(ログイン不可)を作成
sudo useradd --system --home-dir /var/lib/peca-p2p-yp --shell /usr/sbin/nologin peca-p2p-yp

# バイナリと unit を配置して起動
sudo install -m 755 target/release/peca-p2p-yp /usr/local/bin/
sudo install -m 644 contrib/systemd/peca-p2p-yp.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now peca-p2p-yp
```

- 状態確認: `systemctl status peca-p2p-yp` / ログ確認: `journalctl -u peca-p2p-yp`
- データディレクトリは `StateDirectory=` により `/var/lib/peca-p2p-yp` が使われます。
- 停止は `systemctl stop`(SIGTERM で安全終了、systemd 既定タイムアウト 90 秒以内)。
  異常終了時は `Restart=on-failure` により自動再起動します。

同一ホストで複数インスタンスを動かす場合は、テンプレート unit(`peca-p2p-yp@.service`)を
作成し、`StateDirectory=peca-p2p-yp/%i` とインスタンスごとのポート指定
(`ExecStart=/usr/local/bin/peca-p2p-yp --http-bind ... --pcp-bind ... --p2p-bind ...`)で
data-dir とポートを分離してください(`peca-p2p-yp@a.service` のように起動できます)。

## ビルド(開発者向け)

利用者は配布バイナリのみで動作します。ソースからビルドする場合(Windows / Linux 共通):

```powershell
cargo build --release   # 生成物: target\release\peca-p2p-yp.exe(Linux: target/release/peca-p2p-yp)
cargo test              # unit + contract + integration + cucumber
cargo fmt -- --check    # 整形チェック(CI と同じ)
```

## ドキュメント

- 設計コンテキスト: [`CONTEXT.md`](./CONTEXT.md)
- アーキテクチャ決定記録: [`docs/adr/`](./docs/adr/)
- 仕様・契約: [`specs/001-nostr-p2p-yp/`](./specs/001-nostr-p2p-yp/) /
  [`specs/002-linux-support/`](./specs/002-linux-support/)

## ライセンス

本ソフトウェアは **MIT License** で公開されています。詳細は [`LICENSE`](./LICENSE) を参照してください。

Copyright (c) 2026 Philmist

### ライセンス補足

PeerCastStation(GPLv3-or-later)とはプロセス間の TCP 連携のみを行い、リンク・結合はしていません。
PeerCastのプロトコルであるPCPの実装は既存の実装であるPeerCastStationやpeercast-ytによることなく、
PCPのパケット解析文書によりのみ行いGPLの伝播を避けています。

# Contract: CLI・パス解決・プラットフォーム挙動(002-linux-support)

**Principles**: I, II | **FR**: FR-001, FR-010, FR-013, FR-014

コマンドライン・環境変数・ディレクトリ解決のプラットフォーム横断契約。
既存 CLI(`--pcp-bind` / `--http-bind` / `--p2p-bind` / `--data-dir`)の意味論は不変。
ネットワーク契約(local-api / http-yp / p2p-gossip / pcp-announce / nostr-events)は
**一切変更しない**(FR-002 — Windows 版と機能的同等)。

## 1. data-dir 解決順(全 OS 共通の優先順位)

| 優先 | ソース | 対象 OS | 例 |
|------|--------|---------|----|
| 1 | `--data-dir <path>` | 共通 | 複数インスタンス分離の第一手段(FR-010) |
| 2 | `$STATE_DIRECTORY` | unix | systemd `StateDirectory=` が注入(`/var/lib/peca-p2p-yp`)。複数パス列挙時は先頭を使う |
| 3a | `%APPDATA%\peca-p2p-yp` | Windows | 既存挙動(不変) |
| 3b | `$XDG_STATE_HOME/peca-p2p-yp` | unix | XDG Base Directory(state) |
| 4 | `~/.local/state/peca-p2p-yp` | unix | `XDG_STATE_HOME` 未設定時の XDG 既定 |

- 解決不能(Windows: `APPDATA` 未設定 / unix: `HOME` も未設定)は定型メッセージ +
  終了コード 2(FR-014。既存挙動と同じ)
- data-dir は存在しなければ再帰作成する。unix では mode `0700` で作成(FR-013 予防)
- 配置物: `app.db`(+ WAL/SHM)・`master.key`(unix)・`security.log`

## 2. 環境変数

| 変数 | OS | 用途 |
|------|----|------|
| `STATE_DIRECTORY` | unix | data-dir(§1 優先 2) |
| `NOTIFY_SOCKET` | unix | sd_notify 送信先(任意 — contracts/systemd-service.md) |
| `XDG_STATE_HOME` | unix | data-dir 既定(§1 優先 3b) |
| `APPDATA` | Windows | data-dir 既定(既存) |
| `RUST_LOG` | 共通 | ログ追加指定(既存挙動不変: INFO への追加) |

新設の独自環境変数は導入しない(設定は DB 内 settings + CLI 上書きの既存モデルを維持)。

## 3. シグナル・終了

| シグナル | OS | 挙動 |
|---------|----|------|
| SIGTERM | unix | graceful shutdown(FR-008) |
| SIGINT / Ctrl+C | 共通 | graceful shutdown(既存挙動) |

終了コード: 0 正常 / 1 実行時異常 / 2 引数・設定不正(既存規約 — 不変)。

## 4. 起動時パーミッション検査(unix のみ — FR-013)

**起動順序(unix)**: data-dir 作成(`0700`)→ Store(SQLite)オープン → keystore 初期化
(master.key 読込。無ければ mode `0600` で生成 — contracts/key-envelope.md §5)→ **本検査**
→ リスナーバインド。master.key は生成直後でも必ず検査対象に含まれる(生成は mode `0600` の
ため通常 no-op であり、初回起動でも順序矛盾は生じない)。

対象と是正値:

1. data-dir → `0700`
2. `master.key` → `0600`
3. `app.db`・`app.db-wal`・`app.db-shm`(存在するもの)→ `0600`

- **判定基準**: mode の group/other ビット(`0o077`)が 1 つでも立っていれば是正対象とし、
  ファイルは `0600`・ディレクトリは `0700` へ `chmod` する。owner ビットのみが既定より
  厳しい場合(例: `0400`)は他ユーザーへの開放がないため是正しない(是正対象は「他ユーザー
  への開放」のみ)
- **是正不能の条件**(いずれかに該当で是正失敗): `chmod` の失敗(他ユーザー所有による
  `EPERM`・読み取り専用ファイルシステムの `EROFS`・その他 I/O エラー)、または対象が
  シンボリックリンクである場合(追従して是正せず是正不能として扱う — symlink 経由で第三者
  ファイルの mode を変更する事故・攻撃を避ける)
- 是正成功: `key_permission_fixed` をセキュリティイベントに記録し、稼働継続(MUST)
- 是正失敗: `key_permission_unfixable` を記録・警告し、KeystoreHealth = Unavailable
  (全ペルソナ `usable:false`)。**起動と発見・伝搬機能は継続する**(MUST)
- 記録するパスは data-dir 相対名のみ(絶対パス非漏洩 — Principle II)
- Windows では本検査は no-op(DPAPI がアカウントスコープを担保)

### 検査範囲の意図的除外(記録)

- **`security.log`**: 是正対象に含めない(意図的除外)。鍵素材・秘匿情報を含まず
  (FR-011・data-model §SecurityCategory)、data-dir `0700` により他ユーザーからは到達
  不能であるため、data-dir の是正が実質的な防壁となる
- **POSIX ACL・共有グループ等**: パーミッションビット以外のアクセス経路は検査対象外
  (意図的除外)。ACL 付与は管理者の明示操作であり利用者の意図とみなす(既定インストール
  では発生しない)
- **稼働中の再検査**: 行わない(起動時検査のみ)。稼働中にアクセス権を緩められるのは実行
  アカウント自身か root に限られ、常時監視(inotify 等)は複雑度に見合わない。次回起動時に
  検知・是正される
- **予防の成立根拠(systemd / 手動起動の双方 — FR-013 予防側)**: master.key は umask に
  依存せず mode `0600` で明示作成される。SQLite が作る DB/WAL/SHM は umask に依存するが、
  data-dir を `0700` で作成するため他ユーザーは内部ファイルへ到達できない(主防壁)。
  systemd 実行時は `UMask=0077` + `StateDirectoryMode=0700` が個別ファイルも `0600` に
  する(多層防御)。手動起動で緩い umask の場合も data-dir `0700` が防壁となり、個別
  ファイルは本検査(次回起動時含む)が是正する

## 5. 起動失敗エラーの合否基準(FR-014)

- **合格条件**: メッセージから (a) 失敗した操作(どのリスナー / どの保管物か — 例
  「HTTP 待受アドレスにバインドできません」)と (b) 原因種別(使用中・権限不足・パス解決
  不能 等)が判別できること。終了コードは 2(引数・設定起因)または 1(実行時異常)
- **失格条件**: スタックトレース、パニック出力、data-dir 外の内部絶対パス、原因種別へ
  翻訳されていない OS エラーの生文字列のみの出力、依存クレート名等の内部実装詳細を含むこと

## 6. ヘルプ・文言

- `--help` の `--data-dir` 既定値説明はプラットフォーム別に正しい既定を表示する
  (Windows: `%APPDATA%\peca-p2p-yp` / Linux: `$XDG_STATE_HOME/peca-p2p-yp` ほか §1)
- エラー・UI 文言から「DPAPI」等の Windows 固有名を排し、プラットフォーム中立な
  「保護された保管」表現へ(挙動変更なし)

## 7. 検証(契約テスト・cucumber 対応)

| # | 検証 | FR |
|---|------|----|
| 1 | `--data-dir` が全ソースに優先 | FR-010 |
| 2 | (unix) `STATE_DIRECTORY` > `XDG_STATE_HOME` > `~/.local/state` の順 | FR-010 |
| 3 | (unix) 緩いパーミッションの master.key/DB が 0600 へ是正され `key_permission_fixed` が記録される | FR-013 |
| 4 | (unix) 是正不能時に全ペルソナ利用不可 + index.txt/一覧提供は継続 | FR-013 |
| 5 | 使用中ポートで起動 → 定型メッセージ + 非 0 終了、内部詳細なし | FR-014 |
| 6 | 異なる data-dir + ポートで 2 インスタンス同時稼働 | FR-010 |

# Formal Verification — セットアップと使い方

コンスティテューション Principle V に基づき、クリティカルなコンポーネントは
PlusCal / TLA+ による形式的検証を行う。

## 環境

| 項目 | 値 |
|------|-----|
| ツールキット | TLA+ Toolbox |
| インストールパス | `C:\Tools\TLA-toolbox\` |
| `tla2tools.jar` | `C:\Tools\TLA-toolbox\tla2tools.jar` |
| CLASSPATH | システム環境変数として設定済み (`Machine` スコープ) |
| Java | OpenJDK 25.0.3 (Microsoft) |

## コマンドリファレンス

```powershell
# PlusCal → TLA+ 変換
java pcal.trans <file.tla>

# TLA+ 構文チェック
java tla2sany.SANY <file.tla>

# TLC モデル検査
java -XX:+UseParallelGC tlc2.TLC <file.tla>
# または設定ファイル (.cfg) を使う場合:
java -XX:+UseParallelGC tlc2.TLC -config <file.cfg> <file.tla>
```

## ディレクトリ構成

```
docs/formal/
├── README.md          # このファイル
└── <component>/
    ├── <Model>.tla    # PlusCal + TLA+ スペック
    ├── <Model>.cfg    # TLC 設定ファイル
    └── result.md      # 検証結果と発見した不変条件
```

## 新しいモデルを追加する手順

1. `docs/formal/<component>/` ディレクトリを作成する
2. `.tla` ファイルにPlusCalアルゴリズムを記述する
3. `java pcal.trans <file.tla>` でTLA+に変換する
4. `.cfg` ファイルで検査する不変条件とテンポラル性質を定義する
5. `java -XX:+UseParallelGC tlc2.TLC -config <file.cfg> <file.tla>` で検査する
6. 結果を `result.md` に記録する (発見した問題・確認した不変条件・状態数)

## クリティカルと判断する基準 (Principle V)

以下に該当する箇所はPlusCalモデルを作成しなければならない:

- 並行処理や競合状態が発生しうる箇所
- セキュリティに直結する認証・認可ロジック
- ネットワークプロトコルの状態機械
- データ整合性を保証しなければならない箇所

判断した理由はADR に記録する。

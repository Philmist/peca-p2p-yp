# language: ja
機能: 書き込みを含む LAN 公開の境界 (Principle I, II)
  # 007-web-bbs-write / ADR-0015 / contracts/lan-exposure.md §7
  # ネガティブシナリオを含む(Principle IV)。bind 検証は Settings::validate、
  # Host/送信元/レートは互換 API ルーター、監査は SecurityEvent で確認する。

  シナリオ: グローバルアドレスへの公開拒否
    前提 http_bind に "203.0.113.5:7180" が指定されている
    もし 設定を検証する
    ならば 設定エラーとなり待受は開始されない

  シナリオ: 未指定アドレスの拒否
    前提 compat_bbs_bind に "0.0.0.0:7183" が指定されている
    もし 設定を検証する
    ならば 設定エラーとなる

  シナリオ: CGNAT アドレスの拒否
    前提 http_bind に "100.64.1.2:7180" が指定されている
    もし 設定を検証する
    ならば 設定エラーとなる

  シナリオ: 同意キーなしの LAN bind 拒否
    前提 http_bind に "192.168.1.10:7180" が指定され http_lan_consent が false
    もし 設定を検証する
    ならば 設定エラーとなる（明示確認なしに公開されない）

  シナリオ: v4-mapped プライベートアドレスの正規化判定
    前提 http_bind に "[::ffff:192.168.1.10]:7180" が指定され http_lan_consent が true
    もし 設定を検証する
    ならば 正規化後にプライベートと判定され許可される

  シナリオ: LAN 公開起動の監査記録
    前提 http_bind "192.168.1.10:7180" と http_lan_consent true
    もし 起動して待受に成功する
    ならば SecurityEvent WebUiLanExposed が 1 件記録される

  シナリオ: ホワイトリスト外 Host の拒否
    前提 LAN 公開中の互換 API
    もし Host ヘッダ "evil.example:7183" のリクエストが届く
    ならば 403 で拒否される

  シナリオ: 許可範囲外の送信元の拒否
    前提 LAN 公開中の互換 API
    もし 送信元 IP がグローバルアドレスのリクエストが届く
    ならば 403 で拒否される

  シナリオ: LAN 側のレート制限は loopback と同一
    前提 LAN 公開中の互換 API
    もし 同一送信元から 20 req/秒を超えるリクエストが届く
    ならば 429 で拒否される（loopback 側と同じ上限）

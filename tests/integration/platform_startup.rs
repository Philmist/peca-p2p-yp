//! T011 統合テスト: プロセス起動 — ポート競合・多インスタンス同時稼働
//! (contracts/cli-config.md §5 FR-014 合否基準・§7 #5・#6)
//!
//! 実バイナリ(`CARGO_BIN_EXE_peca-p2p-yp`)を `std::process::Command` で起動し、
//! 以下を検証する:
//! 1. 使用中ポートへのバインド失敗 → 非 0 終了 + 定型メッセージ(§7 #5)
//! 2. 異なる data-dir + ポートで 2 インスタンス同時稼働(§7 #6 / FR-010)
//!
//! **テストファースト(Principle IV)**: 1-(c) の原因種別アサーションは現行実装で
//! 失敗する(「HTTP 待受アドレスにバインドできませんでした」のみで原因種別なし)。
//! T013 で文言整備後にパスするよう設計されている。

use std::net::TcpListener;
use std::path::Path;
use std::time::Duration;

// ---------------------------------------------------------------------------
// 補助: Drop ガード
// ---------------------------------------------------------------------------

/// 子プロセスを Drop 時に確実に kill する RAII ガード。
/// テスト失敗時のプロセスリーク防止(タスク指示)。
struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// ---------------------------------------------------------------------------
// 補助: ポート確保
// ---------------------------------------------------------------------------

/// n 個の空きポートを同時バインドで重複なく確保する。
///
/// バインド → ポート取得 → drop(一括)の順で行うため、n 個のポートが互いに重複しない。
/// drop 後に他プロセスが同ポートを確保する可能性はテスト環境で許容する。
fn free_ports(n: usize) -> Vec<u16> {
    let listeners: Vec<TcpListener> = (0..n)
        .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
        .collect();
    let ports = listeners
        .iter()
        .map(|l| l.local_addr().unwrap().port())
        .collect();
    // listeners ここで drop → ポート一括解放
    ports
}

// ---------------------------------------------------------------------------
// 補助: バイナリ起動
// ---------------------------------------------------------------------------

/// stdout/stderr を null に捨ててバイナリを起動する(2 インスタンス用)。
fn spawn_instance(http_port: u16, pcp_port: u16, data_dir: &Path) -> std::process::Child {
    std::process::Command::new(env!("CARGO_BIN_EXE_peca-p2p-yp"))
        .args([
            "--http-bind",
            &format!("127.0.0.1:{http_port}"),
            "--pcp-bind",
            &format!("127.0.0.1:{pcp_port}"),
            "--p2p-bind",
            "",
            "--data-dir",
            data_dir.to_str().unwrap(),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("バイナリの起動に失敗しました")
}

// ---------------------------------------------------------------------------
// 補助: HTTP ヘルスチェック(reqwest 依存なし)
// ---------------------------------------------------------------------------

/// raw TCP で `GET /index.txt` を送信してステータスコードだけを取得する。
///
/// サーバ未起動・接続拒否・タイムアウトはすべて `None` を返す。
/// Host ヘッダは `loopback_hosts(port)` のホワイトリストに含まれる
/// `127.0.0.1:{port}` を使う(cli-config §7 #6)。
fn http_get_status(port: u16) -> Option<u16> {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .ok()?;
    write!(
        stream,
        "GET /index.txt HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\n\r\n"
    )
    .ok()?;
    let mut buf = [0u8; 128];
    let n = stream.read(&mut buf).ok()?;
    std::str::from_utf8(&buf[..n])
        .ok()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// 100ms 間隔 × 100 回ポーリングし、`/index.txt` が 200 を返すまで待つ。
/// タイムアウト(10 秒)を超えると false を返す。
fn wait_for_200(port: u16) -> bool {
    for _ in 0..100 {
        if http_get_status(port) == Some(200) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

// ---------------------------------------------------------------------------
// テスト 1: 使用中ポートで起動 → 非 0 終了 + 定型メッセージ(§7 #5)
// ---------------------------------------------------------------------------

/// contracts/cli-config.md §7 #5 / §5 FR-014 合否基準を検証する。
///
/// (a) 終了コード非 0、(b) HTTP の文言、(c) 原因種別、(d) 失格条件の不在
///
/// **テストファースト**: (c) の原因種別アサーションは現行実装で**失敗する**。
/// 現行の「HTTP 待受アドレスにバインドできませんでした」には原因種別が含まれない
/// (T013 の文言整備後にパスする設計)。
#[test]
fn port_in_use_exits_nonzero_with_typed_message() {
    // HTTP ポートを占有したまま保持する(バイナリ実行中もバインド保持)
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let occupied_port = occupied.local_addr().unwrap().port();

    // PCP ポートを動的確保(P2P は空文字で無効)
    let pcp_port = free_ports(1)[0];
    let data_dir = tempfile::tempdir().unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_peca-p2p-yp"))
        .args([
            "--http-bind",
            &format!("127.0.0.1:{occupied_port}"),
            "--pcp-bind",
            &format!("127.0.0.1:{pcp_port}"),
            "--p2p-bind",
            "",
            "--data-dir",
            data_dir.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    // occupied は output() 後に drop(バイナリはすでに終了)
    drop(occupied);

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined = format!("{stderr}{stdout}");

    // (a) 終了コード非 0(現行実装は 1)
    assert_ne!(
        output.status.code().unwrap_or(-1),
        0,
        "バインド失敗時は非 0 で終了すること: {:?}\nstderr: {stderr}",
        output.status.code(),
    );

    // (b) stderr に「どのリスナーか」(HTTP)が判別できる定型文言
    assert!(
        stderr.contains("HTTP"),
        "stderr に HTTP の文言が必要(§5 合格条件 a): {stderr}",
    );

    // (c) stderr に原因種別が判別できる文言 ← 現行実装では失敗する(T013 修正予定)
    //
    // 現行メッセージ「HTTP 待受アドレスにバインドできませんでした」には
    // 「使用中」等の原因種別が含まれないため、このアサーションは失敗する。
    // T013 の文言整備後にパスする設計(cli-config.md §5 合格条件 b)。
    assert!(
        stderr.contains("使用中")
            || stderr.contains("already in use")
            || stderr.contains("アドレスが使用")
            || stderr.contains("ポートが使用"),
        "stderr に原因種別(使用中等)が必要(§5 合格条件 b — T013 修正予定): {stderr}",
    );

    // (d) 失格条件の不在(§5 失格条件)
    assert!(
        !combined.contains("panicked"),
        "パニック出力を含んではならない: {combined}",
    );
    assert!(
        !combined.contains("RUST_BACKTRACE"),
        "バックトレース出力を含んではならない: {combined}",
    );
    assert!(
        !combined.contains("os error"),
        "OS エラー生文字列を含んではならない(§5 失格条件): {combined}",
    );
    assert!(
        !combined.contains("src/"),
        "内部ソースパスを含んではならない(§5 失格条件): {combined}",
    );
    // data-dir 絶対パスの漏洩確認
    let data_dir_str = data_dir.path().to_str().unwrap();
    assert!(
        !stderr.contains(data_dir_str),
        "stderr に data-dir 絶対パスが漏洩している(§5 失格条件): {stderr}",
    );
}

// ---------------------------------------------------------------------------
// テスト 2: 2 インスタンス同時稼働(§7 #6 / FR-010)
// ---------------------------------------------------------------------------

/// 異なる data-dir + ポートで 2 インスタンスが同時に /index.txt を提供できることを検証する。
///
/// contracts/cli-config.md §7 #6、FR-010(複数インスタンス分離)に対応。
#[test]
fn two_instances_run_concurrently() {
    let ports = free_ports(4);
    let http1 = ports[0];
    let pcp1 = ports[1];
    let http2 = ports[2];
    let pcp2 = ports[3];
    let dir1 = tempfile::tempdir().unwrap();
    let dir2 = tempfile::tempdir().unwrap();

    // 両インスタンスを先に起動(並行して初期化が走る)
    let proc1 = KillOnDrop(spawn_instance(http1, pcp1, dir1.path()));
    let proc2 = KillOnDrop(spawn_instance(http2, pcp2, dir2.path()));

    // 双方が /index.txt で 200 を返すまでポーリング(100ms × 100 回 = 最大 10 秒)
    let ok1 = wait_for_200(http1);
    let ok2 = wait_for_200(http2);

    // Drop ガードで確実に kill・wait(アサーション前後どちらで panic しても安全)
    drop(proc1);
    drop(proc2);

    assert!(
        ok1,
        "インスタンス 1(port={http1})の /index.txt が 200 を返さなかった",
    );
    assert!(
        ok2,
        "インスタンス 2(port={http2})の /index.txt が 200 を返さなかった",
    );
}

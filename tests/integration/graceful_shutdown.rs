//! T025 統合テスト: graceful shutdown — SIGTERM・sd_notify 連携(unix のみ)
//!
//! 実バイナリ(`CARGO_BIN_EXE_peca-p2p-yp`)を `std::process::Command` で起動し、
//! 以下を検証する(FR-008/FR-009、systemd-service.md §1):
//! 1. SIGTERM 受信 → graceful shutdown → 終了コード 0
//!    NOTIFY_SOCKET 未設定でも正常稼働すること(FR-009 MUST — criterion ③)
//! 2. `NOTIFY_SOCKET` に一時 UnixDatagram を指定した場合、全リスナーバインド後に
//!    `READY=1`・停止開始時に `STOPPING=1` が届くこと(FR-009 — criterion ②)
//!
//! **テストファースト(Principle IV)**: T026〜T028 が未実装の状態では:
//! - テスト 1: SIGTERM に対してデフォルト動作(シグナル終了)となり exit code が
//!   None(シグナル終了)または非 0 になるためアサーション失敗
//! - テスト 2: READY=1/STOPPING=1 が送信されないため recv() がタイムアウトし失敗

// ファイル全体を unix 限定とする(SIGTERM・UnixDatagram は POSIX 専用)
#![cfg(unix)]

use std::net::TcpListener;
use std::time::Duration;

// ---------------------------------------------------------------------------
// 補助: Drop ガード
// ---------------------------------------------------------------------------

/// 子プロセスを Drop 時に確実に kill する RAII ガード。
/// テスト失敗(パニック)時のプロセスリーク防止。
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
fn free_ports(n: usize) -> Vec<u16> {
    let listeners: Vec<TcpListener> = (0..n)
        .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
        .collect();
    let ports = listeners
        .iter()
        .map(|l| l.local_addr().unwrap().port())
        .collect();
    // listeners はここで drop → ポート一括解放
    ports
}

// ---------------------------------------------------------------------------
// 補助: HTTP ヘルスチェック
// ---------------------------------------------------------------------------

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

/// 100ms 間隔 × 100 回ポーリングし `/index.txt` が 200 を返すまで待つ(最大 10 秒)。
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
// 補助: SIGTERM 送信・終了待ち
// ---------------------------------------------------------------------------

/// `kill -TERM <pid>` でプロセスに SIGTERM を送る。
/// 追加クレート不要(POSIX `kill` コマンド経由)。
fn send_sigterm(pid: u32) {
    let _ = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status();
}

/// `child.try_wait()` をポーリングして終了を待ち、タイムアウト後は `None` を返す。
fn wait_for_exit(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Option<std::process::ExitStatus> {
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if start.elapsed() >= timeout => return None,
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return None,
        }
    }
}

// ---------------------------------------------------------------------------
// テスト 1: SIGTERM → graceful shutdown → 終了コード 0(FR-008 / criterion ①③)
// ---------------------------------------------------------------------------

/// SIGTERM 受信で graceful shutdown し終了コード 0 で終了すること。
/// NOTIFY_SOCKET 未設定でも正常稼働すること(criterion ③)。
///
/// **テストファースト**: T027/T028 未実装では SIGTERM でシグナル終了(exit code = None)
/// となるため、`assert_eq!(status.code(), Some(0))` が失敗する。
#[test]
fn sigterm_causes_graceful_shutdown_with_exit_code_0() {
    let ports = free_ports(2);
    let http_port = ports[0];
    let pcp_port = ports[1];
    let data_dir = tempfile::tempdir().unwrap();

    let child = std::process::Command::new(env!("CARGO_BIN_EXE_peca-p2p-yp"))
        .args([
            "--http-bind",
            &format!("127.0.0.1:{http_port}"),
            "--pcp-bind",
            &format!("127.0.0.1:{pcp_port}"),
            "--p2p-bind",
            "",
            "--data-dir",
            data_dir.path().to_str().unwrap(),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("バイナリの起動に失敗しました");

    let mut guard = KillOnDrop(child);

    // 起動完了(HTTP 200)まで待つ
    assert!(
        wait_for_200(http_port),
        "プロセスが起動しませんでした(port={http_port})",
    );

    // SIGTERM を送信
    send_sigterm(guard.0.id());

    // 終了コード 0 で終了することを確認(タイムアウト 10 秒)
    let status = wait_for_exit(&mut guard.0, Duration::from_secs(10));
    assert_eq!(
        status.map(|s| s.code()),
        Some(Some(0)),
        "SIGTERM 後は終了コード 0 で終了すること(シグナル終了では None になる)",
    );
    // KillOnDrop は drop で kill + wait するが既に終了済みのため問題なし
}

// ---------------------------------------------------------------------------
// テスト 2: NOTIFY_SOCKET 経由で READY=1・STOPPING=1 が届くこと(FR-009 / criterion ②)
// ---------------------------------------------------------------------------

/// `NOTIFY_SOCKET` に一時 UnixDatagram を指定した場合:
/// - 全リスナーバインド後に `READY=1` が届く
/// - 停止開始時に `STOPPING=1` が届く
/// - SIGTERM で終了コード 0 になる
///
/// **テストファースト**: T026 未実装では READY=1/STOPPING=1 が送信されないため、
/// `server.recv()` が 10 秒後にタイムアウトして失敗する。
#[test]
fn sd_notify_ready_and_stopping_delivered_via_notify_socket() {
    use std::os::unix::net::UnixDatagram;

    let ports = free_ports(2);
    let http_port = ports[0];
    let pcp_port = ports[1];
    let data_dir = tempfile::tempdir().unwrap();
    let sock_dir = tempfile::tempdir().unwrap();
    let sock_path = sock_dir.path().join("notify.sock");

    // 受信側(systemd 相当)ソケットをバインドする
    let server = UnixDatagram::bind(&sock_path).unwrap();
    // READY=1 受信のタイムアウト: 最大 10 秒(起動完了まで)
    server
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();

    // バイナリ起動(NOTIFY_SOCKET を設定)
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_peca-p2p-yp"))
        .args([
            "--http-bind",
            &format!("127.0.0.1:{http_port}"),
            "--pcp-bind",
            &format!("127.0.0.1:{pcp_port}"),
            "--p2p-bind",
            "",
            "--data-dir",
            data_dir.path().to_str().unwrap(),
        ])
        .env("NOTIFY_SOCKET", &sock_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("バイナリの起動に失敗しました");

    let mut guard = KillOnDrop(child);

    // READY=1 を受信する(全リスナーバインド後 — systemd-service §1)
    let mut buf = [0u8; 256];
    let n = server
        .recv(&mut buf)
        .expect("READY=1 の受信がタイムアウトしました(T026/T028 が未実装の可能性)");
    let msg = std::str::from_utf8(&buf[..n]).unwrap_or("");
    assert!(
        msg.contains("READY=1"),
        "READY=1 が届かなかった: {msg:?}(バイナリが異なるメッセージを送信した可能性)",
    );

    // SIGTERM 送信
    send_sigterm(guard.0.id());

    // STOPPING=1 を受信する(停止開始時 — systemd-service §1)
    server
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let n = server
        .recv(&mut buf)
        .expect("STOPPING=1 の受信がタイムアウトしました(T026/T028 が未実装の可能性)");
    let msg = std::str::from_utf8(&buf[..n]).unwrap_or("");
    assert!(
        msg.contains("STOPPING=1"),
        "STOPPING=1 が届かなかった: {msg:?}",
    );

    // 終了コード 0 で終了することを確認(タイムアウト 10 秒)
    let status = wait_for_exit(&mut guard.0, Duration::from_secs(10));
    assert_eq!(
        status.map(|s| s.code()),
        Some(Some(0)),
        "SIGTERM 後は終了コード 0 で終了すること",
    );
}

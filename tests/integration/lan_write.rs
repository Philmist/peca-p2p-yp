//! T016 統合テスト: 書き込みを含む LAN 公開(007 ADR-0015 / contracts/lan-exposure.md)
//!
//! 実バイナリ(`CARGO_BIN_EXE_peca-p2p-yp`)を `--http-bind` / `--compat-bbs-bind` に
//! **このホストの LAN IP** を指定し(2 要素オプトインの同意キー付き)起動して、LAN 公開の
//! 監査・到達性・保護水準を実プロセス + 実 TCP で検証する。非 loopback の bind 可能アドレスが
//! 無い環境(CI 等)ではスキップする(index_lan.rs の露出監査テストと同じ方針)。
//!
//! 検証対象(lan-exposure.md §4/§5/§6):
//! - (a) 非 loopback 待受成功時、面ごとに SecurityEvent が 1 件記録される
//!   (`web_ui_lan_exposed` / `compat_bbs_lan_exposed`)。source はバインドアドレス。
//! - (b) LAN アドレス経由の読み取り(`GET /api/v1/token`・互換 subject.txt)が成立する。
//! - (d 部分) 互換 API 面が LAN 経由でも 006 と同一の検証経路(bbs.cgi)へ到達する
//!   (未開設板への書き込みは通常経路と同じ定型 ERROR ページ = Web UI 専用の抜け道がない)。
//!
//! (c) 別ノード相当からの書き込みがホスト採番を経て全ノードで同一 res_no・同一並び順・
//! 同一アンカーに確定すること、および (d) の**確定**書き込みは、採番・署名・PoW・レート等の
//! 検証経路が **bind アドレスに依存しない**ため、006-livechat-thread の採番収束テスト
//! (`tests/integration/livechat.rs` / `tests/steps/livechat.rs`)と互換 API 書き込みテスト
//! (`tests/contract/compat_bbs.rs`)が既に担保する。本ファイルは LAN 公開層(bind/Host/
//! 送信元/監査)に固有の振る舞いへ集中する。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::time::Duration;

// ---------------------------------------------------------------------------
// 補助
// ---------------------------------------------------------------------------

struct KillOnDrop(std::process::Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_ports(n: usize) -> Vec<u16> {
    let listeners: Vec<TcpListener> = (0..n)
        .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
        .collect();
    listeners
        .iter()
        .map(|l| l.local_addr().unwrap().port())
        .collect()
}

/// このホストに割り当てられた、bind 可能で loopback でない IPv4 アドレスを探す。
fn detect_non_loopback_bindable_ip() -> Option<String> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("192.0.2.1:9").ok()?; // TEST-NET-1(到達不要)
    let local = sock.local_addr().ok()?.ip();
    if local.is_loopback() || local.is_unspecified() {
        return None;
    }
    TcpListener::bind(format!("{local}:0")).ok()?;
    Some(local.to_string())
}

struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

/// 任意ホストへ生 HTTP/1.0 リクエストを送る(Host は接続先 `ip:port` を付与)。
fn http_request_on(
    ip: &str,
    port: u16,
    method: &str,
    path: &str,
    extra_headers: &[&str],
    body: &[u8],
) -> Option<HttpResponse> {
    let mut stream = TcpStream::connect((ip, port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .ok()?;
    let mut req = format!("{method} {path} HTTP/1.0\r\nHost: {ip}:{port}\r\n");
    for h in extra_headers {
        req.push_str(h);
        req.push_str("\r\n");
    }
    if !body.is_empty() {
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).ok()?;
    if !body.is_empty() {
        stream.write_all(body).ok()?;
    }
    stream.flush().ok()?;
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    parse_response(&raw)
}

fn parse_response(raw: &[u8]) -> Option<HttpResponse> {
    let sep = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&raw[..sep]).ok()?;
    let body = raw[sep + 4..].to_vec();
    let status: u16 = head
        .split("\r\n")
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    Some(HttpResponse { status, body })
}

/// LAN の `ip:port` へ 100ms×100 回、`GET /api/v1/token` が 200 になるまで待つ。
fn wait_for_token_200_on(ip: &str, port: u16) -> bool {
    for _ in 0..100 {
        if let Some(r) = http_request_on(ip, port, "GET", "/api/v1/token", &[], &[])
            && r.status == 200
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// http_bind / compat_bbs_bind を LAN IP に(同意キー付きで)指定してノードを起動する。
fn spawn_lan_node(
    http_bind: &str,
    compat_bbs_bind: &str,
    pcp_port: u16,
    data_dir: &std::path::Path,
) -> KillOnDrop {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_peca-p2p-yp"));
    cmd.args([
        "--http-bind",
        http_bind,
        "--http-lan-consent",
        "true",
        "--compat-bbs-bind",
        compat_bbs_bind,
        "--compat-bbs-lan-consent",
        "true",
        "--pcp-bind",
        &format!("127.0.0.1:{pcp_port}"),
        "--p2p-bind",
        "",
        "--data-dir",
        data_dir.to_str().unwrap(),
    ]);
    let child = cmd
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("バイナリの起動に失敗しました");
    KillOnDrop(child)
}

/// security.log から指定カテゴリのイベント行を集める。
fn events_of(data_dir: &std::path::Path, category: &str) -> Vec<serde_json::Value> {
    let content = std::fs::read_to_string(data_dir.join("security.log")).unwrap_or_default();
    content
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["category"] == category)
        .collect()
}

// ---------------------------------------------------------------------------
// (a) LAN 公開の監査 + (b) 到達性 + (d 部分) 互換 API の同一保護経路
// ---------------------------------------------------------------------------

#[test]
fn lan_exposure_records_audit_and_is_reachable() {
    let Some(lan_ip) = detect_non_loopback_bindable_ip() else {
        eprintln!("非 loopback の bind 可能アドレスが無い環境のためスキップ");
        return;
    };
    let ports = free_ports(3);
    let (http_port, compat_port, pcp_port) = (ports[0], ports[1], ports[2]);
    let http_bind = format!("{lan_ip}:{http_port}");
    let compat_bind = format!("{lan_ip}:{compat_port}");
    let data_dir = tempfile::tempdir().unwrap();

    let _node = spawn_lan_node(&http_bind, &compat_bind, pcp_port, data_dir.path());
    assert!(
        wait_for_token_200_on(&lan_ip, http_port),
        "LAN の Web UI リスナーが起動しませんでした({http_bind})"
    );
    // 起動直後の監査イベント書き込みを確実に拾うため少し待つ。
    std::thread::sleep(Duration::from_millis(300));

    // (a) 面ごとに 1 件ずつ記録され、source はバインドアドレス。
    let web = events_of(data_dir.path(), "web_ui_lan_exposed");
    assert_eq!(web.len(), 1, "web_ui_lan_exposed は 1 件");
    assert_eq!(web[0]["source"], http_bind, "source は http bind アドレス");
    let compat = events_of(data_dir.path(), "compat_bbs_lan_exposed");
    assert_eq!(compat.len(), 1, "compat_bbs_lan_exposed は 1 件");
    assert_eq!(
        compat[0]["source"], compat_bind,
        "source は compat bind アドレス"
    );

    // (b) LAN 経由の読み取り: 互換 subject.txt は未開設板で 404(リスナー到達 + 同一挙動)。
    let r = http_request_on(
        &lan_ip,
        compat_port,
        "GET",
        "/deadbeef/subject.txt",
        &[],
        &[],
    )
    .expect("互換 LAN リスナーへ到達できること");
    assert_eq!(
        r.status, 404,
        "未開設板の subject.txt は 404(到達している証拠)"
    );

    // (d 部分) 互換 API の書き込み経路が LAN 経由でも通常経路と同一である
    // (未開設板への bbs.cgi は Web UI 専用の抜け道を作らず、通常の ERROR ページを返す)。
    let form = b"bbs=deadbeef&key=1700000000&FROM=&mail=&MESSAGE=test";
    let r = http_request_on(
        &lan_ip,
        compat_port,
        "POST",
        "/test/bbs.cgi",
        &["Content-Type: application/x-www-form-urlencoded"],
        form,
    )
    .expect("互換 LAN リスナーへ POST 到達できること");
    assert_eq!(r.status, 200, "bbs.cgi は 200 で応答ページを返す");
    let body = String::from_utf8_lossy(&r.body);
    assert!(
        body.contains("ERROR"),
        "未開設板への書き込みは通常経路と同じ ERROR ページになる(抜け道なし)"
    );
}

/// loopback bind(既定運用)では LAN 露出監査イベントは記録されない(非退行)。
#[test]
fn loopback_bind_records_no_lan_exposure() {
    let ports = free_ports(3);
    let (http_port, compat_port, pcp_port) = (ports[0], ports[1], ports[2]);
    let http_bind = format!("127.0.0.1:{http_port}");
    let compat_bind = format!("127.0.0.1:{compat_port}");
    let data_dir = tempfile::tempdir().unwrap();

    let _node = spawn_lan_node(&http_bind, &compat_bind, pcp_port, data_dir.path());
    assert!(
        wait_for_token_200_on("127.0.0.1", http_port),
        "loopback Web UI リスナーが起動しませんでした"
    );
    std::thread::sleep(Duration::from_millis(300));

    assert_eq!(
        events_of(data_dir.path(), "web_ui_lan_exposed").len(),
        0,
        "loopback では web_ui_lan_exposed を記録しない"
    );
    assert_eq!(
        events_of(data_dir.path(), "compat_bbs_lan_exposed").len(),
        0,
        "loopback では compat_bbs_lan_exposed を記録しない"
    );
}

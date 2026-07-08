//! T007/T010 統合テスト: 読み取り専用 index.txt の LAN 公開(オプトイン)
//!
//! 実バイナリ(`CARGO_BIN_EXE_peca-p2p-yp`)を `std::process::Command` で起動し、
//! `--index-bind` で第 2 リスナー(テストでは 127.0.0.1 の別ポートで代替 — 検証は
//! 通るが「第 2 リスナー」経路を通る)を立て、生 TCP の HTTP/1.0 クライアントで
//! 挙動を検証する。実プロセス + 実 TCP を選ぶ理由は、要件「index_bind 空なら
//! 第 2 リスナー不存在(接続拒否)」がリスナーの有無そのものを検証対象とするため、
//! ルーターの直接呼び出し(oneshot)では表現できないことによる。
//!
//! ## US1(T007)
//! 1. LAN リスナーへの `GET /index.txt` が loopback 側と同一内容・同一 Content-Type
//!    (`index_txt_encoding` 共有。Shift_JIS 切替時も一致)
//! 2. `HEAD /index.txt` が GET と整合(同一 Content-Type・ボディなし)
//! 3. `index_bind` 空なら第 2 リスナーが存在しない(接続拒否)
//! 4. 同一送信元 10 req/秒超過で 429 `{"error":"rate_limited"}`
//!
//! ## US2(T010)
//! 1. LAN リスナーへの `/api/v1/status`・`/api/v1/settings`(PUT 含む)→ 404
//!    `{"error":"not_found"}`
//! 2. `/`・静的アセットパス → 404 定型 JSON
//! 3. `POST /index.txt` → 405(空ボディ + `Allow` ヘッダ)
//! 4. 管理 HTTP 受け口の loopback 強制が LAN リスナー有効時にも不変
//! 5. URL 長 >1KB / ヘッダ合計 >8KB → 400 `{"error":"request_too_large"}`
//!
//! **テストファースト(Principle IV)**: T008/T009/T011 未実装では `--index-bind`
//! に対して第 2 リスナーが起動しないため、LAN ポートへの接続が拒否され、
//! GET/HEAD の 200 アサーションや 429・404・405・400 の検証が失敗する。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

// ---------------------------------------------------------------------------
// 補助: 子プロセスの Drop ガード
// ---------------------------------------------------------------------------

/// 子プロセスを Drop 時に確実に kill する RAII ガード(テスト失敗時のリーク防止)。
struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// ---------------------------------------------------------------------------
// 補助: 空きポート確保
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
// 補助: 生 TCP の HTTP/1.0 クライアント
// ---------------------------------------------------------------------------

/// パース済み HTTP 応答。
struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpResponse {
    /// ヘッダを大文字小文字無視で引く。
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// 生ヘッダ・ボディ付きの HTTP/1.0 リクエストを送り、応答をパースして返す。
/// `extra_headers` は `Host` の後に差し込む追加ヘッダ行(末尾 CRLF なし)。
fn http_request(
    port: u16,
    method: &str,
    path: &str,
    extra_headers: &[&str],
    body: &[u8],
) -> Option<HttpResponse> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .ok()?;

    let mut req = format!("{method} {path} HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\n");
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

    // HTTP/1.0 なので応答完了でサーバが接続を閉じる → EOF まで読む。
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

/// 生バイト列を status・headers・body へパースする。
fn parse_response(raw: &[u8]) -> Option<HttpResponse> {
    // ヘッダとボディの境界(空行)を探す。
    let sep = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&raw[..sep]).ok()?;
    let body = raw[sep + 4..].to_vec();

    let mut lines = head.split("\r\n");
    let status_line = lines.next()?;
    let status: u16 = status_line.split_whitespace().nth(1)?.parse().ok()?;

    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();

    Some(HttpResponse {
        status,
        headers,
        body,
    })
}

/// 100ms 間隔 × 100 回(最大 10 秒)ポーリングし、`GET /index.txt` が 200 を
/// 返すまで待つ。
fn wait_for_index_200(port: u16) -> bool {
    for _ in 0..100 {
        if let Some(r) = http_request(port, "GET", "/index.txt", &[], &[])
            && r.status == 200
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

// ---------------------------------------------------------------------------
// 補助: バイナリ起動
// ---------------------------------------------------------------------------

/// テスト用にバイナリを起動する。`index_bind` が `Some` なら `--index-bind` を渡す。
/// `index_encoding` を指定すると設定 DB に事前保存してから起動する。
fn spawn_node(
    http_port: u16,
    pcp_port: u16,
    index_bind: Option<&str>,
    data_dir: &std::path::Path,
) -> KillOnDrop {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_peca-p2p-yp"));
    cmd.args([
        "--http-bind",
        &format!("127.0.0.1:{http_port}"),
        "--pcp-bind",
        &format!("127.0.0.1:{pcp_port}"),
        "--p2p-bind",
        "",
        "--data-dir",
        data_dir.to_str().unwrap(),
    ]);
    if let Some(ib) = index_bind {
        cmd.args(["--index-bind", ib]);
    }
    let child = cmd
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("バイナリの起動に失敗しました");
    KillOnDrop(child)
}

// ---------------------------------------------------------------------------
// US1(T007)
// ---------------------------------------------------------------------------

/// LAN リスナー(第 2 ポート)への GET /index.txt が loopback 側と同一内容・
/// 同一 Content-Type を返す。HEAD も GET と整合する。
#[test]
fn lan_listener_serves_same_index_txt_as_loopback() {
    let ports = free_ports(3);
    let (http_port, pcp_port, index_port) = (ports[0], ports[1], ports[2]);
    let data_dir = tempfile::tempdir().unwrap();
    let index_bind = format!("127.0.0.1:{index_port}");

    let _node = spawn_node(http_port, pcp_port, Some(&index_bind), data_dir.path());

    assert!(
        wait_for_index_200(http_port),
        "loopback リスナーが起動しませんでした(port={http_port})"
    );
    assert!(
        wait_for_index_200(index_port),
        "LAN(第 2)リスナーが起動しませんでした(port={index_port})"
    );

    // (1) GET が同一内容・同一 Content-Type
    let loop_get = http_request(http_port, "GET", "/index.txt", &[], &[]).unwrap();
    let lan_get = http_request(index_port, "GET", "/index.txt", &[], &[]).unwrap();
    assert_eq!(loop_get.status, 200);
    assert_eq!(lan_get.status, 200);
    assert_eq!(
        lan_get.body, loop_get.body,
        "LAN 側と loopback 側の index.txt 本文が一致する"
    );
    assert_eq!(
        lan_get.header("content-type"),
        loop_get.header("content-type"),
        "Content-Type が一致する(index_txt_encoding 共有)"
    );
    // 既定は UTF-8。
    assert_eq!(
        lan_get.header("content-type"),
        Some("text/plain; charset=UTF-8")
    );

    // (2) HEAD が GET と整合(同一 Content-Type・ボディなし)
    let lan_head = http_request(index_port, "HEAD", "/index.txt", &[], &[]).unwrap();
    assert_eq!(lan_head.status, 200);
    assert_eq!(
        lan_head.header("content-type"),
        lan_get.header("content-type"),
        "HEAD の Content-Type が GET と一致する"
    );
    assert!(lan_head.body.is_empty(), "HEAD はボディなし");
}

/// `index_txt_encoding = shift_jis` を設定 DB に保存して起動した場合、LAN 側の
/// Content-Type も Shift_JIS になり loopback 側と一致する。
#[test]
fn lan_listener_shares_shift_jis_encoding() {
    let ports = free_ports(3);
    let (http_port, pcp_port, index_port) = (ports[0], ports[1], ports[2]);
    let data_dir = tempfile::tempdir().unwrap();
    let index_bind = format!("127.0.0.1:{index_port}");

    // 起動前に index_txt_encoding を shift_jis へ設定しておく。
    {
        use peca_p2p_yp::store::Store;
        let store = Store::open_in_dir(data_dir.path()).unwrap();
        store
            .set_setting("index_txt_encoding", "shift_jis")
            .unwrap();
    }

    let _node = spawn_node(http_port, pcp_port, Some(&index_bind), data_dir.path());
    assert!(wait_for_index_200(index_port), "LAN リスナー未起動");

    let loop_get = http_request(http_port, "GET", "/index.txt", &[], &[]).unwrap();
    let lan_get = http_request(index_port, "GET", "/index.txt", &[], &[]).unwrap();
    assert_eq!(
        lan_get.header("content-type"),
        Some("text/plain; charset=Shift_JIS"),
        "LAN 側も Shift_JIS を共有する"
    );
    assert_eq!(
        lan_get.header("content-type"),
        loop_get.header("content-type"),
        "loopback 側と一致する"
    );
}

/// `index_bind` 空(既定・フラグなし)なら第 2 リスナーは存在しない。
/// 直前まで使っていた空きポートへの接続が拒否されることで確認する。
#[test]
fn no_second_listener_when_index_bind_empty() {
    let ports = free_ports(3);
    let (http_port, pcp_port, index_port) = (ports[0], ports[1], ports[2]);
    let data_dir = tempfile::tempdir().unwrap();

    // --index-bind を渡さない = 機能無効。
    let _node = spawn_node(http_port, pcp_port, None, data_dir.path());
    assert!(
        wait_for_index_200(http_port),
        "loopback リスナーが起動しませんでした"
    );

    // 第 2 リスナー相当のポートへは接続できない(誰も listen していない)。
    let connect = TcpStream::connect(("127.0.0.1", index_port));
    assert!(
        connect.is_err(),
        "index_bind 空のとき第 2 リスナー用ポートは接続拒否されるべき"
    );
}

/// 同一送信元からの 10 req/秒超過で 429 `{"error":"rate_limited"}` を返す。
#[test]
fn lan_listener_rate_limits_over_10_per_sec() {
    let ports = free_ports(3);
    let (http_port, pcp_port, index_port) = (ports[0], ports[1], ports[2]);
    let data_dir = tempfile::tempdir().unwrap();
    let index_bind = format!("127.0.0.1:{index_port}");

    let _node = spawn_node(http_port, pcp_port, Some(&index_bind), data_dir.path());
    assert!(wait_for_index_200(index_port), "LAN リスナー未起動");

    // 同一秒内に連続アクセスして 429 が現れるまで送る。11 リクエスト目で超過するはず。
    let mut saw_429 = false;
    let mut rate_limited_body = Vec::new();
    for _ in 0..20 {
        if let Some(r) = http_request(index_port, "GET", "/index.txt", &[], &[])
            && r.status == 429
        {
            saw_429 = true;
            rate_limited_body = r.body;
            break;
        }
    }
    assert!(
        saw_429,
        "同一送信元 10 req/秒超過で 429 が返るべき(20 連続で 429 が観測されなかった)"
    );
    let json: serde_json::Value = serde_json::from_slice(&rate_limited_body).unwrap();
    assert_eq!(json["error"], "rate_limited");
}

// ---------------------------------------------------------------------------
// US2(T010)
// ---------------------------------------------------------------------------

/// LAN リスナーは index.txt 以外のパスへ定型 404 `{"error":"not_found"}` を返す
/// (API・UI・静的アセットは物理的に存在しない)。
#[test]
fn lan_listener_returns_not_found_for_api_and_ui() {
    let ports = free_ports(3);
    let (http_port, pcp_port, index_port) = (ports[0], ports[1], ports[2]);
    let data_dir = tempfile::tempdir().unwrap();
    let index_bind = format!("127.0.0.1:{index_port}");

    let _node = spawn_node(http_port, pcp_port, Some(&index_bind), data_dir.path());
    assert!(wait_for_index_200(index_port), "LAN リスナー未起動");

    // (1) API パス(GET / PUT)→ 404 定型 JSON
    for (method, path) in [
        ("GET", "/api/v1/status"),
        ("GET", "/api/v1/settings"),
        ("PUT", "/api/v1/settings"),
    ] {
        let r = http_request(index_port, method, path, &[], &[]).unwrap();
        assert_eq!(r.status, 404, "{method} {path} は 404 であるべき");
        let json: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(
            json["error"], "not_found",
            "{method} {path} の定型 404 JSON"
        );
    }

    // (2) `/` と静的アセットパス → 404 定型 JSON
    for path in ["/", "/settings.html", "/index.html"] {
        let r = http_request(index_port, "GET", path, &[], &[]).unwrap();
        assert_eq!(r.status, 404, "{path} は 404 であるべき");
        let json: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(json["error"], "not_found", "{path} の定型 404 JSON");
    }
}

/// `POST /index.txt` は 405(空ボディ + `Allow` ヘッダ)を返す。
#[test]
fn lan_listener_rejects_post_index_txt_with_405() {
    let ports = free_ports(3);
    let (http_port, pcp_port, index_port) = (ports[0], ports[1], ports[2]);
    let data_dir = tempfile::tempdir().unwrap();
    let index_bind = format!("127.0.0.1:{index_port}");

    let _node = spawn_node(http_port, pcp_port, Some(&index_bind), data_dir.path());
    assert!(wait_for_index_200(index_port), "LAN リスナー未起動");

    let r = http_request(index_port, "POST", "/index.txt", &[], b"x").unwrap();
    assert_eq!(r.status, 405, "POST /index.txt は 405");
    assert!(r.body.is_empty(), "405 は空ボディ(axum 定型)");
    assert!(
        r.header("allow").is_some(),
        "405 は Allow ヘッダを含む(axum 定型)"
    );
}

/// LAN リスナー有効時でも、管理 HTTP 受け口の loopback 側 API は Host 検証で
/// 保護され続ける(不変)。LAN ポートには API が存在せず、loopback ポートの API は
/// 不正 Host を 403 で弾く。
#[test]
fn loopback_api_host_guard_unchanged_with_lan_listener() {
    let ports = free_ports(3);
    let (http_port, pcp_port, index_port) = (ports[0], ports[1], ports[2]);
    let data_dir = tempfile::tempdir().unwrap();
    let index_bind = format!("127.0.0.1:{index_port}");

    let _node = spawn_node(http_port, pcp_port, Some(&index_bind), data_dir.path());
    assert!(wait_for_index_200(http_port), "loopback リスナー未起動");

    // 不正 Host で loopback API を叩く → 403(Host 検証は不変)。Host を完全に制御する
    // 生リクエストで送る(http_request は正しい Host を自動付与してしまうため使わない)。
    let raw = http_raw(
        http_port,
        "GET /api/v1/token HTTP/1.0\r\nHost: evil.example.com\r\n\r\n",
    )
    .unwrap();
    assert_eq!(raw.status, 403, "不正 Host の loopback API は 403(不変)");
}

/// URL 長 >1KB / ヘッダ合計 >8KB は 400 `{"error":"request_too_large"}`。
#[test]
fn lan_listener_rejects_oversized_request() {
    let ports = free_ports(3);
    let (http_port, pcp_port, index_port) = (ports[0], ports[1], ports[2]);
    let data_dir = tempfile::tempdir().unwrap();
    let index_bind = format!("127.0.0.1:{index_port}");

    let _node = spawn_node(http_port, pcp_port, Some(&index_bind), data_dir.path());
    assert!(wait_for_index_200(index_port), "LAN リスナー未起動");

    // URL 長 > 1KB
    let long_query = "a".repeat(1100);
    let r = http_request(
        index_port,
        "GET",
        &format!("/index.txt?{long_query}"),
        &[],
        &[],
    )
    .unwrap();
    assert_eq!(r.status, 400, "URL 長超過は 400");
    let json: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(json["error"], "request_too_large");

    // ヘッダ合計 > 8KB
    let big = "X-Pad: ".to_string() + &"b".repeat(9000);
    let r = http_request(index_port, "GET", "/index.txt", &[&big], &[]).unwrap();
    assert_eq!(r.status, 400, "ヘッダ合計超過は 400");
    let json: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(json["error"], "request_too_large");
}

// ---------------------------------------------------------------------------
// 補助: Host を完全に制御する生リクエスト(host_guard 検証用)
// ---------------------------------------------------------------------------

/// 完全な生リクエスト文字列を送り、応答をパースして返す(Host を含め呼び出し側が
/// 全ヘッダを制御する)。
fn http_raw(port: u16, request: &str) -> Option<HttpResponse> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    stream.write_all(request.as_bytes()).ok()?;
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

// ---------------------------------------------------------------------------
// US3(T014): SecurityEvent index_txt_lan_exposed の記録条件
// ---------------------------------------------------------------------------

/// data-dir 配下の security.log を読み、`index_txt_lan_exposed` カテゴリの行数を数える。
/// ログ未生成(イベントなし)は 0。
fn count_index_lan_exposed_events(data_dir: &std::path::Path) -> usize {
    let path = data_dir.join("security.log");
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    content
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["category"] == "index_txt_lan_exposed")
        .count()
}

/// 非 loopback の index_bind + bind 成功時、起動時に index_txt_lan_exposed が 1 件記録され、
/// source がバインドアドレスであること。
///
/// 非 loopback かつ確実に bind 可能なアドレスは、このホストの LAN IP を動的に取得して
/// 用いる(loopback 帯 127.x は is_loopback=true で記録されないため使えない)。取得
/// できない環境(CI 等で LAN IP 無し)ではスキップする。
#[test]
fn security_event_recorded_for_non_loopback_bind() {
    let Some(lan_ip) = detect_non_loopback_bindable_ip() else {
        eprintln!("非 loopback のバインド可能アドレスが無い環境のためスキップ");
        return;
    };
    let ports = free_ports(3);
    let (http_port, pcp_port, index_port) = (ports[0], ports[1], ports[2]);
    let data_dir = tempfile::tempdir().unwrap();
    let index_bind = format!("{lan_ip}:{index_port}");

    let _node = spawn_node(http_port, pcp_port, Some(&index_bind), data_dir.path());
    // LAN リスナーの起動完了を待つ(bind 成功の確認)。
    assert!(
        wait_for_index_200_on(&lan_ip, index_port),
        "非 loopback LAN リスナーが起動しませんでした({index_bind})"
    );
    // 起動直後のイベント書き込みを確実に拾うため少し待つ。
    std::thread::sleep(Duration::from_millis(300));

    let count = count_index_lan_exposed_events(data_dir.path());
    assert_eq!(count, 1, "非 loopback + bind 成功で 1 件記録されるべき");

    // source がバインドアドレスであること。
    let path = data_dir.path().join("security.log");
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let event = content
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["category"] == "index_txt_lan_exposed")
        .expect("index_txt_lan_exposed イベントが存在するべき");
    assert_eq!(event["source"], index_bind, "source はバインドアドレス");
}

/// loopback 値(127.0.0.1)では露出ではないため index_txt_lan_exposed は 0 件。
#[test]
fn no_security_event_for_loopback_bind() {
    let ports = free_ports(3);
    let (http_port, pcp_port, index_port) = (ports[0], ports[1], ports[2]);
    let data_dir = tempfile::tempdir().unwrap();
    let index_bind = format!("127.0.0.1:{index_port}");

    let _node = spawn_node(http_port, pcp_port, Some(&index_bind), data_dir.path());
    assert!(wait_for_index_200(index_port), "LAN リスナー未起動");
    std::thread::sleep(Duration::from_millis(300));

    assert_eq!(
        count_index_lan_exposed_events(data_dir.path()),
        0,
        "loopback 値では記録しない"
    );
}

/// 機能無効(index_bind 空)では index_txt_lan_exposed は 0 件。
#[test]
fn no_security_event_when_disabled() {
    let ports = free_ports(3);
    let (http_port, pcp_port, _index_port) = (ports[0], ports[1], ports[2]);
    let data_dir = tempfile::tempdir().unwrap();

    let _node = spawn_node(http_port, pcp_port, None, data_dir.path());
    assert!(wait_for_index_200(http_port), "loopback リスナー未起動");
    std::thread::sleep(Duration::from_millis(300));

    assert_eq!(
        count_index_lan_exposed_events(data_dir.path()),
        0,
        "機能無効では記録しない"
    );
}

/// 指定アドレス・ポートで GET /index.txt が 200 を返すまで待つ。
fn wait_for_index_200_on(ip: &str, port: u16) -> bool {
    for _ in 0..100 {
        if let Some(r) = http_request_on(ip, port, "GET", "/index.txt", &[], &[])
            && r.status == 200
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// 任意の接続先ホストへ HTTP/1.0 リクエストを送る(http_request の host 指定版)。
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

/// このホストに割り当てられた、bind 可能で loopback でない IPv4 アドレスを探す。
/// 見つからなければ `None`(CI 等で LAN IP が無い環境)。
///
/// UDP ソケットを外向きに「接続」して OS が選ぶ送信元アドレスを得る(パケットは
/// 送らない)。得たアドレスが loopback でなければ、それを bind 候補として返す。
fn detect_non_loopback_bindable_ip() -> Option<String> {
    use std::net::UdpSocket;
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    // 実際には送信しない。ルーティング上の送信元アドレス選択のためだけに connect する。
    sock.connect("192.0.2.1:9").ok()?; // TEST-NET-1(到達不要)
    let local = sock.local_addr().ok()?.ip();
    if local.is_loopback() || local.is_unspecified() {
        return None;
    }
    // 実際に bind できるか確認(bind 可能でなければ候補にしない)。
    let candidate = format!("{local}:0");
    TcpListener::bind(&candidate).ok()?;
    Some(local.to_string())
}

// ---------------------------------------------------------------------------
// US4(T021): 第 2 リスナー bind 失敗時の縮退継続
// ---------------------------------------------------------------------------

/// loopback http ポートから `GET /api/v1/status` を取得して JSON を返す
/// (Host 検証は http_request が正しい Host を自動付与するため通る)。
fn fetch_status(http_port: u16) -> Option<serde_json::Value> {
    let r = http_request(http_port, "GET", "/api/v1/status", &[], &[])?;
    if r.status != 200 {
        return None;
    }
    serde_json::from_slice(&r.body).ok()
}

/// index_bind を「テスト側が既に掴んでいるポート」にすると bind 競合(AddrInUse)で
/// 第 2 リスナーが上がらないが、本体(loopback UI/API)は稼働継続し、status の
/// index_txt_lan が {enabled:true, listening:false, error:"addr_in_use"} になる。
/// SecurityEvent は記録されない。
#[test]
fn bind_conflict_degrades_but_keeps_running() {
    let ports = free_ports(3);
    let (http_port, pcp_port, index_port) = (ports[0], ports[1], ports[2]);
    let data_dir = tempfile::tempdir().unwrap();

    // index 用ポートをテスト側で占有したままにする(競合を作る)。
    let _occupier = TcpListener::bind(("127.0.0.1", index_port)).unwrap();
    let index_bind = format!("127.0.0.1:{index_port}");

    let _node = spawn_node(http_port, pcp_port, Some(&index_bind), data_dir.path());
    // 本体(loopback)は継続稼働する。
    assert!(
        wait_for_index_200(http_port),
        "bind 競合でも本体 loopback は稼働継続するべき"
    );

    // status に失敗理由が反映される。
    let mut lan = None;
    for _ in 0..50 {
        if let Some(status) = fetch_status(http_port) {
            lan = Some(status["index_txt_lan"].clone());
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let lan = lan.expect("status を取得できませんでした");
    assert_eq!(lan["enabled"], true, "機能は有効(設定あり)");
    assert_eq!(lan["listening"], false, "bind 競合で待受していない");
    assert_eq!(lan["error"], "addr_in_use", "競合の定型コード");

    // 露出は発生していないため SecurityEvent は 0 件。
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        count_index_lan_exposed_events(data_dir.path()),
        0,
        "bind 失敗では露出イベントを記録しない"
    );
}

/// index_bind を http_bind と**同一ポート**(自己競合)にした場合の縮退継続。
/// spec §Edge Cases「index_bind が既存の管理用受け口と同一 → 競合 → US4 縮退」を検証する。
///
/// リスナー起動順が「http bind 先行 → index bind 試行」でなければ、http bind が競合で
/// 落ちて本体ごと終了し、本テストは status 取得不能で失敗する(この順序不変性の回帰固定)。
#[test]
fn self_conflict_with_http_port_degrades_but_keeps_running() {
    let ports = free_ports(2);
    let (http_port, pcp_port) = (ports[0], ports[1]);
    let data_dir = tempfile::tempdir().unwrap();
    // index_bind = http と同一ポート(loopback なので検証は通る・自己競合を誘発)。
    let index_bind = format!("127.0.0.1:{http_port}");

    let _node = spawn_node(http_port, pcp_port, Some(&index_bind), data_dir.path());
    // http bind が先行して成功するため、本体(loopback)は継続稼働する。
    assert!(
        wait_for_index_200(http_port),
        "自己競合でも本体 loopback は稼働継続するべき(http bind が先行して成功する)"
    );

    let mut lan = None;
    for _ in 0..50 {
        if let Some(status) = fetch_status(http_port) {
            lan = Some(status["index_txt_lan"].clone());
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let lan = lan.expect("status を取得できませんでした(本体が落ちた可能性)");
    assert_eq!(lan["enabled"], true, "機能は有効(設定あり)");
    assert_eq!(lan["listening"], false, "自己競合で待受していない");
    assert_eq!(lan["error"], "addr_in_use", "競合の定型コード");

    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        count_index_lan_exposed_events(data_dir.path()),
        0,
        "bind 失敗では露出イベントを記録しない"
    );
}

/// このホストに割り当てられていない LAN プライベートアドレス(検証は通るが bind 不可)
/// では、第 2 リスナーが上がらず error が非 null になり、本体は稼働継続する。
/// 具体的なエラーコードはプラットフォーム差があるため、listening:false・error 非 null を検証する。
#[test]
fn unavailable_address_degrades_but_keeps_running() {
    let ports = free_ports(3);
    let (http_port, pcp_port, index_port) = (ports[0], ports[1], ports[2]);
    let data_dir = tempfile::tempdir().unwrap();

    // 10.255.255.254 は RFC1918(検証通過)だが通常このホストに割り当てられていない
    // → bind は AddrNotAvailable 相当で失敗する。
    let index_bind = format!("10.255.255.254:{index_port}");

    let _node = spawn_node(http_port, pcp_port, Some(&index_bind), data_dir.path());
    assert!(
        wait_for_index_200(http_port),
        "存在しないアドレスでも本体 loopback は稼働継続するべき"
    );

    let mut lan = None;
    for _ in 0..50 {
        if let Some(status) = fetch_status(http_port) {
            lan = Some(status["index_txt_lan"].clone());
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let lan = lan.expect("status を取得できませんでした");
    assert_eq!(lan["enabled"], true);
    assert_eq!(lan["listening"], false, "bind 失敗で待受していない");
    assert!(
        lan["error"].is_string(),
        "失敗理由の定型コードが入る(プラットフォーム差のため値は非 null のみ検証)"
    );
    assert_eq!(
        count_index_lan_exposed_events(data_dir.path()),
        0,
        "bind 失敗では露出イベントを記録しない"
    );
}

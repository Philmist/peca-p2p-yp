//! 起動配線(T020)
//!
//! 設定読込 → ストア → セキュリティログ → P2P(待受+外向き接続維持ループ)→ Web の
//! 起動監視と graceful shutdown を行う。起動フローは config.rs / web/mod.rs / p2p/runtime.rs
//! の公開 API を配線するのみで、業務ロジックは各モジュールが持つ。
//!
//! 終了コード: 引数・設定の不正は 2、実行時の初期化・サーバ異常は 1。
//! エラー文言は定型で内部情報を含めない(Principle II)。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::sync::watch;

use peca_p2p_yp::config::{self, CliOverrides, Settings};
use peca_p2p_yp::p2p::peers::{PeerManager, PeerManagerConfig};
use peca_p2p_yp::p2p::runtime::P2pRuntime;
use peca_p2p_yp::security::SecurityLog;
use peca_p2p_yp::store::Store;
use peca_p2p_yp::web::{AppState, build_router};

#[tokio::main]
async fn main() {
    if let Err(code) = run().await {
        std::process::exit(code);
    }
}

/// 起動シーケンス本体。終了コードを `Err(code)` で返す。
async fn run() -> Result<(), i32> {
    // 1. CLI パース(不正引数は定型メッセージで exit)。
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.iter().any(|a| a == "--help" || a == "-h") {
        print_usage();
        return Ok(());
    }
    let overrides = CliOverrides::parse(raw).map_err(exit_config)?;

    // 2. data-dir 解決 → Store オープン。
    let data_dir = resolve_data_dir(&overrides)?;
    let store = Arc::new(Store::open_in_dir(&data_dir).map_err(|e| {
        eprintln!("{e}");
        1
    })?);

    // 3. Settings 読込 + 上書き + 検証(拒否は定型エラーで exit)。
    let mut settings = Settings::load(&store).map_err(|e| {
        eprintln!("{e}");
        1
    })?;
    settings.apply_overrides(&overrides);
    settings.validate().map_err(exit_config)?;

    // 4. tracing 初期化(コンソール)。
    init_tracing();

    // 5. SecurityLog(データディレクトリ配下)。
    let security = Arc::new(
        SecurityLog::new(data_dir.join("security.log")).map_err(|_| {
            eprintln!("セキュリティログを初期化できませんでした");
            1
        })?,
    );

    // 6. バインドアドレスの確定(検証済み)。
    let p2p_addr = settings.p2p_addr().map_err(exit_config)?;
    let http_addr = settings.http_addr().map_err(exit_config)?;

    // 7. ピア管理(Settings の目標値を反映)。
    let peer_config = PeerManagerConfig {
        outbound_target: settings.p2p_outbound_target as usize,
        inbound_max: settings.p2p_inbound_max as usize,
        ..PeerManagerConfig::default()
    };
    let peers = Arc::new(PeerManager::new(Arc::clone(&store), peer_config));

    // 8. 起動時 nonce(自己接続検出用)と待受ポート。
    let nonce: u64 = rand::random();
    let listen_port = p2p_addr.map(|a| a.port()).unwrap_or(0);

    // 9. graceful shutdown 伝播チャネル。
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // 10. P2P 待受のバインド(失敗は起動失敗)。空文字設定なら待受なし(外向きのみ)。
    let p2p_listener = match p2p_addr {
        Some(addr) => Some(TcpListener::bind(addr).await.map_err(|_| {
            eprintln!("P2P 待受アドレスにバインドできませんでした");
            1
        })?),
        None => None,
    };

    // 11. P2P ランタイム起動。
    let runtime = Arc::new(P2pRuntime::new(
        Arc::clone(&peers),
        Arc::clone(&security),
        nonce,
        listen_port,
    ));
    let p2p_handles = Arc::clone(&runtime).spawn(p2p_listener, shutdown_rx.clone());

    // 12. Web 起動。
    let state = AppState::new(Arc::clone(&store), Arc::clone(&security), http_addr.port());
    let app = build_router(state);
    let http_listener = TcpListener::bind(http_addr).await.map_err(|_| {
        eprintln!("HTTP 待受アドレスにバインドできませんでした");
        1
    })?;

    // 13. 起動サマリ(バインドアドレス・既知ピア数のみ。内部情報なし)。
    let known_peers = store.count_peers().unwrap_or(0);
    let p2p_desc = p2p_addr
        .map(|a| a.to_string())
        .unwrap_or_else(|| "無効(外向きのみ)".to_string());
    tracing::info!(
        target: "startup",
        http = %http_addr,
        p2p = %p2p_desc,
        outbound_target = settings.p2p_outbound_target,
        inbound_max = settings.p2p_inbound_max,
        known_peers,
        "起動しました(停止は Ctrl+C)"
    );

    // 14. serve + graceful shutdown(Ctrl+C で全サブシステムへ伝播)。
    //     レート制限の接続元取得に connect-info が必須(T019 申し送り)。
    let serve = axum::serve(
        http_listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!(target: "startup", "shutdown 要求を受信しました");
        let _ = shutdown_tx.send(true);
    });

    let serve_result = serve.await;

    // 15. P2P ループの終了を待つ(shutdown を検知して自走終了する)。
    for handle in p2p_handles {
        let _ = handle.await;
    }

    if serve_result.is_err() {
        eprintln!("HTTP サーバが異常終了しました");
        return Err(1);
    }
    tracing::info!(target: "startup", "停止しました");
    Ok(())
}

/// data-dir を解決する。`--data-dir` 未指定時は `%APPDATA%\peca-p2p-yp`。
fn resolve_data_dir(overrides: &CliOverrides) -> Result<PathBuf, i32> {
    if let Some(dir) = &overrides.data_dir {
        return Ok(dir.clone());
    }
    match std::env::var_os("APPDATA") {
        Some(base) => Ok(PathBuf::from(base).join("peca-p2p-yp")),
        None => {
            eprintln!("APPDATA が未設定です。--data-dir を指定してください");
            Err(2)
        }
    }
}

/// tracing のコンソール出力を初期化する(INFO 以上)。
fn init_tracing() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_target(false)
        .init();
}

/// 設定・引数エラーを終了コード 2 へ写像しつつ定型文言を表示する。
fn exit_config(e: config::ConfigError) -> i32 {
    eprintln!("{e}");
    2
}

fn print_usage() {
    println!(
        "peca-p2p-yp — 分散型配信情報共有ネットワーク(YP 代替)\n\
         \n\
         使い方: peca-p2p-yp [オプション]\n\
         \n\
         オプション:\n\
         \x20 --p2p-bind <host:port>   P2P 待受(空文字で待受無効=外向きのみ)\n\
         \x20 --http-bind <host:port>  HTTP(UI・index.txt)待受(loopback のみ)\n\
         \x20 --pcp-bind <host:port>   PCP アナウンス待受(loopback のみ)\n\
         \x20 --data-dir <path>        データディレクトリ(既定: %APPDATA%\\peca-p2p-yp)\n\
         \x20 -h, --help               このヘルプを表示\n"
    );
}

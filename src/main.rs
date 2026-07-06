//! 起動配線(T020 + Phase 3/4 統合)
//!
//! 設定読込 → ストア → セキュリティログ → gossip ハブ → P2P(待受+外向き維持)→
//! PCP アナウンス待受 → 掲載エンジン(ペルソナ署名・再発行)→ Web の起動監視と
//! graceful shutdown を行う。起動フローは各モジュールの公開 API を配線するのみで、
//! 業務ロジックは各モジュールが持つ。
//!
//! 終了コード: 引数・設定の不正は 2、実行時の初期化・サーバ異常は 1。
//! エラー文言は定型で内部情報を含めない(Principle II)。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

use peca_p2p_yp::config::{self, CliOverrides, Settings};
use peca_p2p_yp::event::publish::{EventSink, PublishEngine};
use peca_p2p_yp::event::schema::{ChannelListing, VerifyConfig};
use peca_p2p_yp::event::store::StoreConfig;
use peca_p2p_yp::identity::{IdentityManager, Keystore, keystore};
use peca_p2p_yp::p2p::hub::GossipHub;
use peca_p2p_yp::p2p::peers::{PeerManager, PeerManagerConfig, ReachabilityState};
use peca_p2p_yp::p2p::runtime::P2pRuntime;
use peca_p2p_yp::p2p::upnp::{self, InboundReachable};
use peca_p2p_yp::pcp::channel::{AnnouncedChannel, ChannelChange, ChannelRegistry, ChannelState};
use peca_p2p_yp::security::SecurityLog;
use peca_p2p_yp::store::Store;
use peca_p2p_yp::web::announced::{
    AnnouncedProvider, AnnouncedSummary, ClockSkewStatus, NodeStatusProvider, clock_skew_status,
};
use peca_p2p_yp::web::{AppState, build_router};

/// 鮮度切れ・期限切れイベントの物理回収(sweep)周期。
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

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
    let pcp_addr = settings.pcp_addr().map_err(exit_config)?;

    // 7. ピア管理(Settings の目標値を反映)。
    let peer_config = PeerManagerConfig {
        outbound_target: settings.p2p_outbound_target as usize,
        inbound_max: settings.p2p_inbound_max as usize,
        ..PeerManagerConfig::default()
    };
    let peers = Arc::new(PeerManager::new(Arc::clone(&store), peer_config));

    // 8. gossip ハブ(EventStore・DedupCache・一覧ビュー・再伝搬 — T037/T039)。
    let store_config = StoreConfig {
        freshness_window_sec: settings.freshness_window_sec,
        event_store_max: settings.event_store_max as usize,
        ..StoreConfig::default()
    };
    let verify = VerifyConfig {
        max_clock_skew_sec: settings.max_clock_skew_sec,
        min_pow_bits: settings.min_pow_bits.min(255) as u8,
    };
    let hub = GossipHub::new(
        Arc::clone(&store),
        Arc::clone(&security),
        store_config,
        verify,
    );

    // 9. 起動時 nonce(自己接続検出用)と待受ポート。
    let nonce: u64 = rand::random();
    let listen_port = p2p_addr.map(|a| a.port()).unwrap_or(0);

    // 10. graceful shutdown 伝播チャネル。
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // 11. P2P 待受のバインド(失敗は起動失敗)。空文字設定なら待受なし(外向きのみ)。
    let p2p_listener = match p2p_addr {
        Some(addr) => Some(
            TcpListener::bind(addr)
                .await
                .map_err(|e| bind_error("P2P", &e))?,
        ),
        None => None,
    };

    // 12. P2P ランタイム起動(ハブへ受信処理・SYNC・再伝搬を委譲)。
    let has_listener = p2p_listener.is_some();
    let runtime = Arc::new(P2pRuntime::new(
        Arc::clone(&peers),
        Arc::clone(&security),
        Arc::clone(&hub),
        nonce,
        listen_port,
        settings.pex_enabled,
    ));
    // 全ピア到達不能状態と回復通知の共有ハンドル(status API・回復再発行と共有 — T047/T048)。
    let reachability = runtime.reachability();
    let mut handles = Arc::clone(&runtime).spawn(p2p_listener, shutdown_rx.clone());

    // 12a. 着信可否の共有状態(UPnP — T053 / FR-016)。待受なしは常に到達不能、
    //      待受あり + UPnP 無効は直接待受として到達可能、待受あり + UPnP 有効は
    //      マッピング成功まで到達不能(タスクが更新する)。
    let inbound_reachable =
        InboundReachable::new(upnp::decide_initial(has_listener, settings.upnp_enabled));
    if has_listener && settings.upnp_enabled {
        let reachable = inbound_reachable.clone();
        let sd = shutdown_rx.clone();
        handles.push(tokio::spawn(async move {
            upnp::run(listen_port, reachable, sd).await;
        }));
    }

    // 13. PCP アナウンス待受(loopback のみ — ADR-0006 決定 4)。
    let registry = ChannelRegistry::new();
    let pcp_listener = TcpListener::bind(pcp_addr)
        .await
        .map_err(|e| bind_error("PCP", &e))?;
    {
        let registry = Arc::clone(&registry);
        let security = Arc::clone(&security);
        let sd = shutdown_rx.clone();
        handles.push(tokio::spawn(async move {
            peca_p2p_yp::pcp::session::serve(pcp_listener, registry, security, sd).await;
        }));
    }

    // 14. ペルソナ管理と掲載エンジン(T028/T029)。
    //     keystore 初期化: 既存に自プラットフォーム scheme のペルソナがあるか判定し、
    //     master.key を読込/生成する(ADR-0008 §4/§5)。
    // TODO(T020/T021): KeystoreInit の反映(KeystoreHealth 化)と起動順序の是正
    //   (keystore 初期化をリスナーバインド前へ移動)。
    let has_encrypted_personas = store
        .list_personas()
        .map(|ps| {
            ps.iter()
                .any(|p| keystore::is_current_scheme(&p.secret_enc))
        })
        .unwrap_or(false);
    let (keystore, _keystore_init) =
        Keystore::open(&data_dir, has_encrypted_personas).map_err(|e| {
            eprintln!("{e}");
            1
        })?;
    let identity = Arc::new(IdentityManager::new(Arc::clone(&store), keystore));
    let sink: Arc<dyn EventSink> = Arc::new(HubSink(Arc::clone(&hub)));
    let engine = Arc::new(PublishEngine::new(
        Arc::clone(&identity),
        sink,
        settings.republish_interval_sec,
    ));
    // 14a. PCP 変更契機の即時発行(announced/updated → live、ended → 最終発行)。
    handles.push(spawn_publish_bridge(
        registry.subscribe(),
        Arc::clone(&engine),
        shutdown_rx.clone(),
    ));
    // 14b. 周期再発行ループ(掲載中スナップショット)。
    let snapshot_registry = Arc::clone(&registry);
    let snapshot: Arc<dyn Fn() -> Vec<ChannelListing> + Send + Sync> = Arc::new(move || {
        snapshot_registry
            .snapshot()
            .iter()
            .filter(|c| c.state != ChannelState::Ended)
            .map(AnnouncedChannel::to_listing)
            .collect()
    });
    handles.push(Arc::clone(&engine).spawn_republish_loop(snapshot, shutdown_rx.clone()));
    // 14c. 全断→回復時の即時再発行(US3 シナリオ 3)。周期再発行(60 秒)を待たず、
    //      いずれかのピアと再確立した瞬間に掲載中チャンネルを再送する。
    {
        let recovery_registry = Arc::clone(&registry);
        let recovery_snapshot: Arc<dyn Fn() -> Vec<ChannelListing> + Send + Sync> =
            Arc::new(move || {
                recovery_registry
                    .snapshot()
                    .iter()
                    .filter(|c| c.state != ChannelState::Ended)
                    .map(AnnouncedChannel::to_listing)
                    .collect()
            });
        handles.push(spawn_recovery_republish(
            Arc::clone(&reachability),
            Arc::clone(&engine),
            recovery_snapshot,
            shutdown_rx.clone(),
        ));
    }
    // 14d. 鮮度切れ・期限切れイベントの物理回収。
    handles.push(spawn_sweep_loop(Arc::clone(&hub), shutdown_rx.clone()));

    // 15. Web 起動(一覧・ペルソナ・掲載状態の供給元を注入)。
    let state = AppState::new(Arc::clone(&store), Arc::clone(&security), http_addr.port())
        .with_directory(Arc::clone(&hub) as Arc<_>)
        .with_identity(Arc::clone(&identity))
        .with_announced(Arc::new(AnnouncedAdapter {
            registry: Arc::clone(&registry),
            identity: Arc::clone(&identity),
        }))
        .with_node_status(Arc::new(StatusAdapter {
            hub: Arc::clone(&hub),
            reachability: Arc::clone(&reachability),
            inbound_reachable: inbound_reachable.clone(),
            pcp_listening: true,
            max_clock_skew_sec: settings.max_clock_skew_sec as i64,
        }));
    let app = build_router(state);
    let http_listener = TcpListener::bind(http_addr)
        .await
        .map_err(|e| bind_error("HTTP", &e))?;

    // 16. 起動サマリ(バインドアドレス・既知ピア数のみ。内部情報なし)。
    let known_peers = store.count_peers().unwrap_or(0);
    let p2p_desc = p2p_addr
        .map(|a| a.to_string())
        .unwrap_or_else(|| "無効(外向きのみ)".to_string());
    tracing::info!(
        target: "startup",
        http = %http_addr,
        p2p = %p2p_desc,
        pcp = %pcp_addr,
        outbound_target = settings.p2p_outbound_target,
        inbound_max = settings.p2p_inbound_max,
        known_peers,
        "起動しました(停止は Ctrl+C)"
    );

    // 17. serve + graceful shutdown(Ctrl+C で全サブシステムへ伝播)。
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

    // 18. 各サブシステムの終了を待つ(shutdown を検知して自走終了する)。
    for handle in handles {
        let _ = handle.await;
    }

    if serve_result.is_err() {
        eprintln!("HTTP サーバが異常終了しました");
        return Err(1);
    }
    tracing::info!(target: "startup", "停止しました");
    Ok(())
}

// ---------------------------------------------------------------------------
// 配線アダプタ(業務ロジックなし — lib の trait を bin で結線する)
// ---------------------------------------------------------------------------

/// [`PublishEngine`] → [`GossipHub`] の発行受け口。
struct HubSink(Arc<GossipHub>);

impl EventSink for HubSink {
    fn publish_local(&self, event: nostr::Event) -> bool {
        self.0.publish_local(event).should_propagate()
    }
}

/// PCP レジストリ + ペルソナ割当 → 掲載状態 API(T031)の供給元。
struct AnnouncedAdapter {
    registry: Arc<ChannelRegistry>,
    identity: Arc<IdentityManager>,
}

impl AnnouncedProvider for AnnouncedAdapter {
    fn list(&self) -> Vec<AnnouncedSummary> {
        self.registry
            .snapshot()
            .into_iter()
            .map(|ch| {
                let channel_id = ch.channel_id_hex();
                let persona_pubkey = self
                    .identity
                    .persona_for_channel(&channel_id)
                    .ok()
                    .flatten();
                AnnouncedSummary {
                    channel_id,
                    name: ch.name,
                    genre: ch.genre,
                    description: ch.description,
                    contact_url: ch.contact_url,
                    bitrate_kbps: ch.bitrate_kbps as u64,
                    content_type: ch.content_type,
                    tracker: ch.tracker.unwrap_or_default(),
                    listeners: ch.listeners,
                    relays: ch.relays_cnt,
                    started_at: ch.started_at,
                    state: match ch.state {
                        ChannelState::Announced => "announced",
                        ChannelState::Updating => "updating",
                        ChannelState::Ended => "ended",
                    }
                    .to_string(),
                    persona_pubkey,
                }
            })
            .collect()
    }
}

/// gossip ハブ + 到達性状態 → 全体状態 API(T031 基本形 + T048 拡張)の供給元。
struct StatusAdapter {
    hub: Arc<GossipHub>,
    reachability: Arc<ReachabilityState>,
    /// 着信可否の共有状態(UPnP マッピング成否・直接待受 — T053)。
    inbound_reachable: InboundReachable,
    pcp_listening: bool,
    /// 時計ずれ警告のしきい値(受信検証と一致 — data-model §Settings)。
    max_clock_skew_sec: i64,
}

impl NodeStatusProvider for StatusAdapter {
    fn pcp_listening(&self) -> bool {
        self.pcp_listening
    }
    fn established(&self) -> (usize, usize) {
        self.hub.established_counts()
    }
    fn all_peers_unreachable(&self) -> bool {
        self.reachability.is_all_unreachable()
    }
    fn clock_skew(&self) -> ClockSkewStatus {
        clock_skew_status(&self.hub.clock_skew_samples(), self.max_clock_skew_sec)
    }
    fn inbound_reachable(&self) -> bool {
        self.inbound_reachable.get()
    }
}

/// PCP 変更通知を掲載エンジンの発行へ橋渡しするタスク。
fn spawn_publish_bridge(
    mut rx: broadcast::Receiver<ChannelChange>,
    engine: Arc<PublishEngine>,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                msg = rx.recv() => match msg {
                    Ok(ChannelChange::Announced(ch)) | Ok(ChannelChange::Updated(ch)) => {
                        if let Err(e) = engine.publish_listing(&ch.to_listing()) {
                            tracing::warn!(target: "publish", "掲載の発行に失敗しました: {e}");
                        }
                    }
                    Ok(ChannelChange::Ended(ch)) => {
                        if let Err(e) = engine.publish_ended(&ch.to_listing()) {
                            tracing::warn!(target: "publish", "終了イベントの発行に失敗しました: {e}");
                        }
                    }
                    // 取りこぼし(Lagged)は次の周期再発行が回復する。
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            }
        }
    })
}

/// 全断→回復時に掲載中チャンネルを即時再発行するタスク(US3 シナリオ 3 — T047)。
///
/// [`ReachabilityState::recovered`] は全ピア到達不能からいずれかのピアと再確立した
/// ときに解ける。回復のたびに現在の掲載中スナップショットを再発行する。
fn spawn_recovery_republish(
    reachability: Arc<ReachabilityState>,
    engine: Arc<PublishEngine>,
    snapshot: Arc<dyn Fn() -> Vec<ChannelListing> + Send + Sync>,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                _ = reachability.recovered() => {
                    if *shutdown.borrow() {
                        break;
                    }
                    for listing in snapshot() {
                        if let Err(e) = engine.publish_listing(&listing) {
                            tracing::debug!(target: "publish", "回復時の再発行に失敗しました: {e}");
                        }
                    }
                }
            }
        }
    })
}

/// 鮮度切れ・期限切れイベントの物理回収ループ。
fn spawn_sweep_loop(hub: Arc<GossipHub>, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(SWEEP_INTERVAL);
        ticker.tick().await; // 起動直後の即時 tick は読み捨てる
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                _ = ticker.tick() => {
                    if *shutdown.borrow() {
                        break;
                    }
                    hub.sweep();
                }
            }
        }
    })
}

/// data-dir を解決し、存在しなければ作成する(contracts/cli-config.md §1)。
///
/// 解決優先順は `platform::ensure_data_dir` に委譲する(unix では mode 0700 で作成)。
/// 解決不能の場合は定型メッセージを標準エラーへ出力して終了コード 2 を返す(FR-014)。
fn resolve_data_dir(overrides: &CliOverrides) -> Result<PathBuf, i32> {
    peca_p2p_yp::platform::ensure_data_dir(overrides.data_dir.as_deref()).map_err(|msg| {
        eprintln!("{msg}");
        2
    })
}

/// tracing のコンソール出力を初期化する。既定は INFO で、`RUST_LOG` は
/// **INFO への追加指定**として重ねる(例: `RUST_LOG=pcp=debug` で通常ログ +
/// PCP セッションのデバッグ出力。素の EnvFilter と違い既定ログは消えない)。
fn init_tracing() {
    let extra = std::env::var("RUST_LOG").unwrap_or_default();
    let directives = if extra.is_empty() {
        "info".to_string()
    } else {
        format!("info,{extra}")
    };
    let filter = tracing_subscriber::EnvFilter::builder().parse_lossy(&directives);
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

/// 設定・引数エラーを終了コード 2 へ写像しつつ定型文言を表示する。
fn exit_config(e: config::ConfigError) -> i32 {
    eprintln!("{e}");
    2
}

/// リスナーバインド失敗を「どのリスナーか + 原因種別」の定型メッセージへ写像し、
/// 実行時異常の終了コード 1 を返す(cli-config.md §5 / FR-014)。
///
/// OS エラーの生文字列・絶対パス・依存クレート名など内部実装詳細は出力しない
/// (§5 失格条件 / FR-011)。原因種別へ翻訳できない場合は原因なしの定型文言に留める。
fn bind_error(listener: &str, err: &std::io::Error) -> i32 {
    use std::io::ErrorKind;
    let base = format!("{listener} 待受アドレスにバインドできませんでした");
    let msg = match err.kind() {
        ErrorKind::AddrInUse => format!("{base}(ポートが使用中です)"),
        ErrorKind::PermissionDenied => format!("{base}(権限が不足しています)"),
        ErrorKind::AddrNotAvailable => format!("{base}(指定アドレスが利用できません)"),
        _ => base,
    };
    eprintln!("{msg}");
    1
}

fn print_usage() {
    // `--data-dir` 既定値の説明はプラットフォーム別に正しい既定を表示する
    //(cli-config.md §1 の解決順・§6)。表示のみで挙動は変えない。
    #[cfg(windows)]
    let data_dir_default = "%APPDATA%\\peca-p2p-yp";
    #[cfg(unix)]
    let data_dir_default = "$XDG_STATE_HOME/peca-p2p-yp(未設定時 ~/.local/state/peca-p2p-yp、systemd 下は $STATE_DIRECTORY)";
    println!(
        "peca-p2p-yp — 分散型配信情報共有ネットワーク(YP 代替)\n\
         \n\
         使い方: peca-p2p-yp [オプション]\n\
         \n\
         オプション:\n\
         \x20 --p2p-bind <host:port>   P2P 待受(空文字で待受無効=外向きのみ)\n\
         \x20 --http-bind <host:port>  HTTP(UI・index.txt)待受(loopback のみ)\n\
         \x20 --pcp-bind <host:port>   PCP アナウンス待受(loopback のみ)\n\
         \x20 --data-dir <path>        データディレクトリ(既定: {data_dir_default})\n\
         \x20 -h, --help               このヘルプを表示\n"
    );
}

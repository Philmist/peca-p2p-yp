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

use peca_p2p_yp::broadcast::BroadcastState;
use peca_p2p_yp::config::{self, CliOverrides, Settings};
use peca_p2p_yp::event::publish::{EventSink, PublishEngine};
use peca_p2p_yp::event::schema::{ChannelListing, VerifyConfig};
use peca_p2p_yp::event::store::StoreConfig;
use peca_p2p_yp::identity::{IdentityManager, Keystore, KeystoreHealth, keystore};
use peca_p2p_yp::livechat::board::BoardKeyManager;
use peca_p2p_yp::p2p::hub::GossipHub;
use peca_p2p_yp::p2p::peers::{PeerManager, PeerManagerConfig, ReachabilityState};
use peca_p2p_yp::p2p::runtime::{P2pRuntime, bind_listener};
use peca_p2p_yp::p2p::upnp::{self, InboundReachable};
use peca_p2p_yp::pcp::channel::{AnnouncedChannel, ChannelChange, ChannelRegistry, ChannelState};
use peca_p2p_yp::security::SecurityLog;
use peca_p2p_yp::store::Store;
use peca_p2p_yp::web::announced::{
    AnnouncedProvider, AnnouncedSummary, ClockSkewStatus, NodeStatusProvider, clock_skew_status,
};
use peca_p2p_yp::web::{AppState, IndexLanStatus, build_index_router, build_router};

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

    // 5b. keystore 初期化 + 起動時パーミッション検査(リスナーバインド前 —
    //     contracts/cli-config.md §4 起動順序: data-dir 作成(0700)→ Store →
    //     keystore 初期化(master.key 読込/生成)→ パーミッション検査 → リスナーバインド)。
    //     既存に自プラットフォーム scheme のペルソナがあるかで「保護鍵消失疑い」を判定する。
    let has_encrypted_personas = store
        .list_personas()
        .map(|ps| {
            ps.iter()
                .any(|p| keystore::is_current_scheme(&p.secret_enc))
        })
        .unwrap_or(false);
    let (keystore, keystore_init) =
        Keystore::open(&data_dir, has_encrypted_personas).map_err(|e| {
            eprintln!("{e}");
            1
        })?;
    // 緩いパーミッションは 0600/0700 へ是正し、是正不能なら全ペルソナ利用不可へ倒す
    // (発見・伝搬は継続 — FR-013)。unix のみ実体があり Windows は no-op。
    let permission = peca_p2p_yp::platform::enforce_permissions(&data_dir, &security);
    let keystore_health = KeystoreHealth::evaluate(permission.is_healthy(), keystore_init);

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
    // スレ機能無効時は gossip 受信の announce(kind 31311)を不可視化する(006 data-model)。
    hub.set_livechat_enabled(settings.livechat_enabled);

    // 9. 起動時 nonce(自己接続検出用)。
    let nonce: u64 = rand::random();

    // 10. graceful shutdown 伝播チャネル。
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // 11. P2P 待受のバインド(ADR-0008: カンマ区切りの複数アドレスを個別にバインド)。
    //     一部の失敗は WARN で記録して残りで続行する(決定2)。空設定なら待受なし
    //     (外向きのみ — FR-016)。指定が非空なのに一つも上がらなければ起動失敗。
    let mut p2p_listeners = Vec::new();
    // 成功したバインドの実アドレス(申告ポート・起動サマリ・UPnP 判定に使う)。
    let mut p2p_bound: Vec<SocketAddr> = Vec::new();
    for addr in &p2p_addr {
        match bind_listener(*addr) {
            Ok(l) => {
                if let Ok(bound) = l.local_addr() {
                    p2p_bound.push(bound);
                }
                p2p_listeners.push(l);
            }
            Err(e) => {
                tracing::warn!(target: "startup", %addr, error = %e, "P2P 待受アドレスにバインドできませんでした");
            }
        }
    }
    if !p2p_addr.is_empty() && p2p_listeners.is_empty() {
        eprintln!("P2P 待受アドレスにバインドできませんでした(全て失敗)");
        return Err(1);
    }

    // 申告 listen_port は成功した先頭バインドのポート(ADR-0008 決定3)。待受なしは 0。
    let listen_port = p2p_bound.first().map(|a| a.port()).unwrap_or(0);
    // ポートが揃っていない場合、他ピアへ申告されるのは先頭ポートのみ(決定3 — 非推奨
    // 構成のため黙殺せず WARN する)。
    if p2p_bound.iter().any(|a| a.port() != listen_port) {
        tracing::warn!(
            target: "startup",
            announced = listen_port,
            "p2p_bind のポートが揃っていません。他ピアへ申告されるのは先頭ポートのみです"
        );
    }
    // 起動サマリ用の待受アドレス表記(spawn で listeners を move する前に確定させる)。
    let p2p_desc = if p2p_bound.is_empty() {
        "無効(外向きのみ)".to_string()
    } else {
        p2p_bound
            .iter()
            .map(|a| a.to_string())
            .collect::<Vec<_>>()
            .join(",")
    };

    // 12. P2P ランタイム起動(ハブへ受信処理・SYNC・再伝搬を委譲)。
    // スレ配送(006-livechat-thread): livechat_enabled のときホストレジストリを配線する。
    // 無効時は None を渡し、THREAD_JOIN を定型 unknown_thread で拒否する(FR-006)。
    let has_listener = !p2p_listeners.is_empty();
    let livechat = settings.livechat_enabled.then(|| {
        peca_p2p_yp::livechat::registry::LivechatRegistry::new(
            settings.thread_max_participants as usize,
        )
    });
    let runtime = Arc::new(P2pRuntime::new_with_livechat(
        Arc::clone(&peers),
        Arc::clone(&security),
        Arc::clone(&hub),
        nonce,
        listen_port,
        settings.pex_enabled,
        livechat,
    ));
    // 全ピア到達不能状態と回復通知の共有ハンドル(status API・回復再発行と共有 — T047/T048)。
    let reachability = runtime.reachability();
    let mut handles = Arc::clone(&runtime).spawn(p2p_listeners, shutdown_rx.clone());

    // 12z. スレ announce の定期発行(T019 — FR-002)。開設中の全スレの kind 31311 を
    //      republish_interval_sec 間隔でハブへローカル発行する(expiration=created_at+600
    //      は封筒側が付与)。開設スレが無ければ何もしない(スレ開設は T024 の明示操作)。
    if let Some(livechat) = runtime.livechat().cloned() {
        let hub = Arc::clone(&hub);
        let interval = settings.republish_interval_sec.max(1);
        let mut sd = shutdown_rx.clone();
        handles.push(tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval));
            loop {
                tokio::select! {
                    _ = sd.changed() => break,
                    _ = ticker.tick() => {
                        if *sd.borrow() {
                            break;
                        }
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        for event in livechat.build_announce_events(now, 0) {
                            hub.publish_local(event);
                        }
                    }
                }
            }
        }));
    }

    // 12a. 着信可否の共有状態(UPnP — T053 / FR-016)。待受なしは常に到達不能、
    //      待受あり + UPnP 無効は直接待受として到達可能、待受あり + UPnP 有効は
    //      マッピング成功まで到達不能(タスクが更新する)。UPnP は IPv4 NAT のみを
    //      対象とするため(ADR-0008 決定3)、IPv4 リスナーが無ければマッピングを
    //      行わず(死んだマッピング・誤った到達可能表示を防ぐ)直接待受として扱う。
    let use_upnp = settings.upnp_enabled && p2p_bound.iter().any(|a| a.is_ipv4());
    let inbound_reachable = InboundReachable::new(upnp::decide_initial(has_listener, use_upnp));
    if use_upnp {
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

    // 14. ペルソナ管理と掲載エンジン(T028/T029)。keystore 初期化・パーミッション検査は
    //     リスナーバインド前(手順 5b)に済ませており、その健全性を IdentityManager へ渡す
    //     (Unavailable なら全ペルソナ利用不可・鍵操作は利用不可エラー — FR-013)。
    // 配信中ロックの共有状態(ADR-0011)。identity(selected 変更ガード)・engine
    //(発行開始の予約)・AppState(status 表示)へ同一インスタンスを配布し、発行開始と
    // selected 変更を単一ロックで相互排他にする。
    let broadcast = Arc::new(BroadcastState::new());

    // 14a. 板鍵管理(006-livechat-thread T012/T056)。ペルソナとは別系統の keystore
    //      インスタンスを用いる(`board_keys` テーブルはペルソナと識別子・テーブルを
    //      共有しない — FR-016)。`keystore` は直後に IdentityManager へ move するため、
    //      ここで独立した 2 本目を開く(unix は master.key の再読込のみ・パーミッション
    //      是正は 5b で済んでいるため再実行しない — 冪等な読み込み操作)。失敗時は
    //      互換 API の書き込み機能のみ縮退させる(発見・伝搬・閲覧は継続 — Principle I)。
    let board_keystore = Keystore::open(&data_dir, has_encrypted_personas)
        .map(|(ks, _)| ks)
        .unwrap_or_else(|_| Keystore::ephemeral());
    let board_keys = Arc::new(BoardKeyManager::new(Arc::clone(&store), board_keystore));

    let identity = Arc::new(
        IdentityManager::new_with_health(Arc::clone(&store), keystore, keystore_health)
            .with_broadcast_state(Arc::clone(&broadcast)),
    );
    let sink: Arc<dyn EventSink> = Arc::new(HubSink(Arc::clone(&hub)));
    let engine = Arc::new(PublishEngine::new(
        Arc::clone(&identity),
        sink,
        settings.republish_interval_sec,
        Arc::clone(&broadcast),
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

    // 15. HTTP 本体(管理 UI/API)のリスナーを先に bind する(fail-fast)。
    //     index.txt の LAN 公開(第 2 リスナー)の bind 試行より**前**に行うことで、
    //     index_bind が管理用受け口と同一ポート(自己競合)の場合でも、本体側が先に
    //     ポートを確保し、競合するのは第 2 リスナー側になる(spec §Edge Cases — 自己競合は
    //     US4 の縮退継続として扱い、本体は落とさない)。既存 3 受け口の fail-fast は不変。
    let http_listener = TcpListener::bind(http_addr)
        .await
        .map_err(|e| bind_error("HTTP", &e))?;

    // 15a. index.txt の LAN 公開(オプトイン — ADR-0012)の bind 試行(AppState 注入前)。
    //      index_bind 非空時のみ index.txt 専用の第 2 リスナーを bind する。既存 3 受け口
    //      (HTTP/PCP/P2P)と違い bind 失敗は致命エラーとせず(bind_error を使わず)、
    //      WARN + 状態への失敗理由反映のみで本体は継続稼働する(FR-007)。
    //      起動時に一度だけ確定する不変状態 IndexLanStatus を組み立て、AppState へ注入する。
    //      検証は Settings::validate() 済み(空=無効、非空=loopback/LAN のみ)。
    //
    //      listener は state 構築後に serve するため Option で持ち越す。パース済み
    //      アドレスは 15b の loopback 判定で再利用する(再パース回避)。
    let (index_lan_status, index_listener, index_addr): (
        Option<Arc<IndexLanStatus>>,
        Option<TcpListener>,
        Option<SocketAddr>,
    ) = if settings.index_bind.is_empty() {
        (None, None, None)
    } else {
        // validate 済みのため通常はパース成功する。防御的に失敗も縮退継続で扱う。
        match settings.index_bind.parse::<SocketAddr>() {
            Ok(index_addr) => match TcpListener::bind(index_addr).await {
                Ok(listener) => (
                    Some(Arc::new(IndexLanStatus {
                        bind: settings.index_bind.clone(),
                        listening: true,
                        error: None,
                    })),
                    Some(listener),
                    Some(index_addr),
                ),
                Err(e) => {
                    // 縮退継続: ErrorKind → 定型コードへ写像し状態へ反映(内部情報なし)。
                    let code = index_bind_error_code(&e);
                    tracing::warn!(
                        target: "startup",
                        index_bind = %settings.index_bind,
                        error = code,
                        "index.txt の LAN リスナーにバインドできませんでした(本体は継続します)"
                    );
                    (
                        Some(Arc::new(IndexLanStatus {
                            bind: settings.index_bind.clone(),
                            listening: false,
                            error: Some(code),
                        })),
                        None,
                        Some(index_addr),
                    )
                }
            },
            Err(_) => {
                tracing::warn!(
                    target: "startup",
                    index_bind = %settings.index_bind,
                    "index_bind の書式を解釈できませんでした(本体は継続します)"
                );
                (
                    Some(Arc::new(IndexLanStatus {
                        bind: settings.index_bind.clone(),
                        listening: false,
                        error: Some("unknown"),
                    })),
                    None,
                    None,
                )
            }
        }
    };

    // 15b. LAN 露出の監査イベント(ADR-0012)。**非 loopback かつ bind 成功**のときのみ
    //      起動時に 1 件記録する(loopback 値・bind 失敗・機能無効では記録しない)。
    //      loopback 判定は検証(require_lan_or_loopback)と同じく to_canonical() 後に行い、
    //      v4-mapped loopback([::ffff:127.0.0.1])を誤って露出と記録しない。
    //      source はバインドアドレス、detail は定型文言。アドレスは 15a のパース済みを
    //      使う(bind 成功時は必ず Some)。
    if let Some(status) = &index_lan_status
        && status.listening
        && let Some(addr) = index_addr
        && !addr.ip().to_canonical().is_loopback()
    {
        security.log(
            peca_p2p_yp::security::SecurityCategory::IndexTxtLanExposed,
            &settings.index_bind,
            "index.txt is exposed to LAN",
        );
    }

    let index_lan_desc = match &index_lan_status {
        None => "無効".to_string(),
        Some(s) if s.listening => format!("{}(LAN 公開)", s.bind),
        Some(s) => format!("バインド失敗:{}(継続)", s.error.unwrap_or("unknown")),
    };

    // 15c. Web 起動(一覧・ペルソナ・掲載状態・LAN 露出状態の供給元を注入)。
    let mut state = AppState::new(Arc::clone(&store), Arc::clone(&security), http_addr.port())
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
        }))
        .with_broadcast(Arc::clone(&broadcast));
    if let Some(status) = index_lan_status {
        state = state.with_index_lan(status);
    }
    // 実況スレ一覧・板設定の供給元(T024 — web/livechat.rs の LivechatDirectory)。
    // TODO(統括): LivechatRegistry(自板)・gossip ハブ(他ノード板 31311)を束ねた実装を
    // ここで生成して `.with_livechat_directory(...)` を配線する。registry.rs / hub.rs に
    // 読み取り専用の公開 API が必要になる見込み(web/livechat.rs モジュール doc 参照)。
    // 現時点では未配線(None)のため、スレ一覧 API は空・詳細 API は 404 を返す。
    let app = build_router(state.clone());

    // 15d. bind に成功していれば index.txt 専用の第 2 リスナーを serve する(既存
    //      サブシステムと同じ shutdown_rx watch 経路 + handles へ push)。
    if let Some(listener) = index_listener {
        let index_app = build_index_router(state.clone());
        let sd = shutdown_rx.clone();
        handles.push(tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                index_app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async move {
                let mut sd = sd;
                let _ = sd.changed().await;
            })
            .await;
        }));
    }

    // 15e. 2ch 互換 API(006-livechat-thread T052)専用 loopback リスナー。
    //      `compat_bbs_bind` は起動時に Settings::validate() 済み(空文字 = 機能無効・
    //      非空は loopback のみ受理 — 非 loopback は既に手順 0 で起動拒否済み)。index.txt
    //      LAN リスナーと異なり、compat_bbs_bind は loopback しか許容しないため bind 失敗を
    //      「縮退継続」として扱う(致命エラーにしない — 互換 API はブリッジ機能であり
    //      本体の発見・伝搬・掲載を道連れにしない)。`livechat_enabled=false` のときは
    //      registry が空(全板 404)になるだけで、リスナー自体は起動時設定どおりに動く。
    if !settings.compat_bbs_bind.is_empty() {
        match settings.compat_bbs_bind.parse::<SocketAddr>() {
            Ok(compat_addr) => match TcpListener::bind(compat_addr).await {
                Ok(listener) => {
                    let registry = runtime.livechat().cloned().unwrap_or_else(|| {
                        peca_p2p_yp::livechat::registry::LivechatRegistry::new(
                            settings.thread_max_participants as usize,
                        )
                    });
                    let compat_state = peca_p2p_yp::web::compat::CompatState {
                        registry,
                        board_keys: Arc::clone(&board_keys),
                        security: Arc::clone(&security),
                        allowed_hosts: Arc::new(peca_p2p_yp::web::loopback_hosts(
                            compat_addr.port(),
                        )),
                        rate_limiter: Arc::new(peca_p2p_yp::web::RateLimiter::per_second(
                            peca_p2p_yp::web::compat::RATE_LIMIT_PER_SEC,
                        )),
                    };
                    let compat_app = peca_p2p_yp::web::compat::routes(compat_state);
                    let sd = shutdown_rx.clone();
                    handles.push(tokio::spawn(async move {
                        let _ = axum::serve(
                            listener,
                            compat_app.into_make_service_with_connect_info::<SocketAddr>(),
                        )
                        .with_graceful_shutdown(async move {
                            let mut sd = sd;
                            let _ = sd.changed().await;
                        })
                        .await;
                    }));
                }
                Err(e) => {
                    tracing::warn!(
                        target: "startup",
                        compat_bbs_bind = %settings.compat_bbs_bind,
                        error = %e.kind().to_string(),
                        "互換 API のリスナーにバインドできませんでした(本体は継続します)"
                    );
                }
            },
            Err(_) => {
                tracing::warn!(
                    target: "startup",
                    compat_bbs_bind = %settings.compat_bbs_bind,
                    "compat_bbs_bind の書式を解釈できませんでした(本体は継続します)"
                );
            }
        }
    }

    // 16. 起動サマリ(バインドアドレス・既知ピア数のみ。内部情報なし)。
    let known_peers = store.count_peers().unwrap_or(0);
    tracing::info!(
        target: "startup",
        http = %http_addr,
        p2p = %p2p_desc,
        pcp = %pcp_addr,
        index_lan = %index_lan_desc,
        outbound_target = settings.p2p_outbound_target,
        inbound_max = settings.p2p_inbound_max,
        known_peers,
        "起動しました(停止は Ctrl+C / SIGTERM)"
    );

    // 16a. 全リスナーバインド成功後に READY=1 を通知する(FR-009 — systemd-service §1)。
    //      NOTIFY_SOCKET 未設定時は no-op。送信失敗は稼働へ影響しない(MUST)。
    peca_p2p_yp::platform::sd_notify("READY=1");

    // 17. serve + graceful shutdown(SIGTERM/SIGINT/Ctrl+C で全サブシステムへ伝播)。
    //     レート制限の接続元取得に connect-info が必須(T019 申し送り)。
    //     platform::shutdown_signal() が unix では SIGTERM+SIGINT、Windows では ctrl_c を
    //     待つ(contracts/cli-config.md §3、research R6 — T027/T028)。
    let serve = axum::serve(
        http_listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        peca_p2p_yp::platform::shutdown_signal().await;
        tracing::info!(target: "startup", "shutdown 要求を受信しました");
        // 停止開始を systemd へ通知してから既存 watch チャネル経路で停止伝播する
        // (STOPPING=1 → shutdown_tx 送信の順 — systemd-service §1、FR-008)。
        peca_p2p_yp::platform::sd_notify("STOPPING=1");
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
///
/// 出力先が端末でない場合(journald 捕捉・パイプ等)は ANSI エスケープを無効化する
/// (FR-011 — research R8、contracts/systemd-service.md §1 ログ契約)。
fn init_tracing() {
    use std::io::IsTerminal;

    let extra = std::env::var("RUST_LOG").unwrap_or_default();
    let directives = if extra.is_empty() {
        "info".to_string()
    } else {
        format!("info,{extra}")
    };
    let filter = tracing_subscriber::EnvFilter::builder().parse_lossy(&directives);
    // 端末でない出力先(journald・パイプ)では ANSI エスケープを出力しない(FR-011)。
    let ansi = std::io::stdout().is_terminal();
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(ansi)
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

/// index.txt LAN リスナーの bind 失敗を `IndexLanStatus.error` の定型コードへ写像する
/// (ADR-0012 / research R3 — `GET /api/v1/status` で返す。内部情報を含めない)。
///
/// 本体を止める [`bind_error`] と違い、これは縮退継続用の状態コード写像であり終了しない
/// (既存 3 受け口の fail-fast は不変 — FR-007)。
fn index_bind_error_code(err: &std::io::Error) -> &'static str {
    use std::io::ErrorKind;
    match err.kind() {
        ErrorKind::AddrInUse => "addr_in_use",
        ErrorKind::PermissionDenied => "permission_denied",
        ErrorKind::AddrNotAvailable => "addr_not_available",
        _ => "unknown",
    }
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

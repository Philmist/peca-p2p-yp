//! cucumber ハーネス(T004/T022/T034/T045/T051)
//!
//! World は全フィーチャで共有し、ステップ定義はストーリー別モジュールに置く:
//! - `steps/us1.rs` — US1 掲載(T022 骨格 → T033 で実装)
//! - `steps/us2.rs` — US2 発見(T034 骨格 → T044 で実装)
//! - `steps/us3.rs` — US3 継続性(T045 骨格 → T049 で実装)
//! - `steps/outbound_only.rs` — 着信不可ノード参加(T051 骨格 → T054 で実装)
//!
//! `fail_on_skipped` により、未実装ステップのシナリオは失敗として報告される
//! (Principle IV — テストファーストの失敗確認)。

use cucumber::World;

/// gossip 契約参照実装(モックピア)。us1/us2 の両ステップが共有する
/// (二重取り込みは clippy::duplicate_mod になるためルートで一元化)。
#[path = "common/mock_peer.rs"]
pub(crate) mod mock_peer;

#[path = "steps/outbound_only.rs"]
mod outbound_only;
#[path = "steps/security.rs"]
mod security;
#[path = "steps/us1.rs"]
mod us1;
#[path = "steps/us2.rs"]
mod us2;
#[path = "steps/us3.rs"]
mod us3;

/// 全フィーチャ共通の World。ストーリー実装時に必要なフィールドを追加する。
#[derive(Debug, Default, World)]
pub struct AppWorld {
    /// US1(掲載)シナリオの状態。Background で初期化する(T033)。
    us1: Option<us1::Us1World>,
    /// US2(発見)シナリオの状態。各シナリオの Given で初期化する(T044)。
    us2: Option<us2::Us2World>,
    /// US3(継続性)シナリオの状態。各シナリオの Given で初期化する(T049)。
    us3: Option<us3::Us3World>,
    /// 着信不可ノード参加シナリオの状態。各シナリオの Given で初期化する(T054)。
    outbound: Option<outbound_only::OutboundWorld>,
    /// セキュリティシナリオの状態。各シナリオの Given で初期化する(T055)。
    security: Option<security::SecurityWorld>,
}

/// ステップの async 未来型は debug ビルドで巨大になり、Windows 既定の main スレッド
/// スタック(1MB)を超える(STATUS_STACK_OVERFLOW)。大きいスタックのスレッドで
/// ランタイムを駆動する。
fn main() {
    const STACK_SIZE: usize = 16 * 1024 * 1024;
    std::thread::Builder::new()
        .stack_size(STACK_SIZE)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_stack_size(STACK_SIZE)
                .build()
                .expect("tokio ランタイム構築")
                .block_on(async {
                    AppWorld::cucumber()
                        // シナリオを直列実行する(既定は最大 64 並行)。各シナリオは
                        // 独自の P2P ノード(MockPeer / TestNode)を起動し TCP 接続・SYNC を
                        // 行うため、並行実行するとコア数の少ない CI ランナー(windows-latest)
                        // 上で接続確立が枯渇し、どのシナリオが落ちるか非決定的なフレークと
                        // なる。直列化すれば各シナリオが資源を占有でき、決定的に成立する。
                        .max_concurrent_scenarios(1usize)
                        .fail_on_skipped()
                        .run_and_exit("tests/features")
                        .await;
                });
        })
        .expect("cucumber スレッド起動")
        .join()
        .expect("cucumber スレッド終了");
}

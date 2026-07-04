//! cucumber ハーネス(T004/T022/T034)
//!
//! World は全フィーチャで共有し、ステップ定義はストーリー別モジュールに置く:
//! - `steps/us1.rs` — US1 掲載(T022 骨格 → T033 で実装)
//! - `steps/us2.rs` — US2 発見(T034 骨格 → T044 で実装)
//!
//! `fail_on_skipped` により、未実装ステップのシナリオは失敗として報告される
//! (Principle IV — テストファーストの失敗確認)。

use cucumber::World;

/// gossip 契約参照実装(モックピア)。us1/us2 の両ステップが共有する
/// (二重取り込みは clippy::duplicate_mod になるためルートで一元化)。
#[path = "common/mock_peer.rs"]
pub(crate) mod mock_peer;

#[path = "steps/us1.rs"]
mod us1;
#[path = "steps/us2.rs"]
mod us2;

/// 全フィーチャ共通の World。ストーリー実装時に必要なフィールドを追加する。
#[derive(Debug, Default, World)]
pub struct AppWorld {
    /// US1(掲載)シナリオの状態。Background で初期化する(T033)。
    us1: Option<us1::Us1World>,
    /// US2(発見)シナリオの状態。各シナリオの Given で初期化する(T044)。
    us2: Option<us2::Us2World>,
}

#[tokio::main]
async fn main() {
    AppWorld::cucumber()
        .fail_on_skipped()
        .run_and_exit("tests/features")
        .await;
}

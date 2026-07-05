//! 接続時同期 SYNC(T038)
//!
//! contracts/p2p-gossip.md §メッセージ種別・§受信検証パイプライン 検査 6 の同期部分。
//!
//! - **要求側**: established 直後に `SYNC_REQ`(since = now − freshness_window_sec)を送る。
//! - **応答側**: live かつ鮮度窓内かつ `created_at ≥ max(since, now − window)` のイベントを
//!   上限 `event_store_max` 件返し、最後に `SYNC_DONE(count)`。イベント選定は
//!   [`crate::p2p::ingest::IngestState::sync_events`] が担う。
//! - **平滑化(MUST)**: 応答側は受信側レート上限(256KB/秒・200 msg/秒)**以下**に平滑化して
//!   送る。正当な同期がレート制限で切断されない両立条件([`stream_sync_response`])。
//! - **受信側の検査 6**: 1 回の SYNC_REQ 応答として受信した EVENT が `event_store_max` 件を
//!   超えたら切断+`p2p_rate_limited`([`SyncCounter`])。

use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;
use tokio::time::Instant;

use crate::p2p::frame::Message;

/// 応答平滑化のメッセージ数上限(受信側 200 msg/秒に対し余裕を持たせる)。
///
/// 並走する再伝搬 EVENT も受信側のレート枠を消費するため、上限より低く設定する。
pub const SYNC_SAFE_MSGS_PER_SEC: usize = 150;
/// 応答平滑化のバイト上限(受信側 256KB/秒に対し余裕を持たせる)。
pub const SYNC_SAFE_BYTES_PER_SEC: usize = 192 * 1024;

/// 要求側が送る `SYNC_REQ` の `since`(= now − freshness_window_sec、下限 0)。
pub fn sync_req_since(now: u64, freshness_window_sec: u64) -> i64 {
    now.saturating_sub(freshness_window_sec) as i64
}

/// メッセージのワイヤ概算バイト数(長さ前置 4 バイト込み)。平滑化の計量用。
fn est_wire_bytes(message: &Message) -> usize {
    serde_json::to_vec(message)
        .map(|v| v.len() + 4)
        .unwrap_or(4)
}

/// SYNC 応答(EVENT の列 + 末尾 SYNC_DONE)を平滑化して `tx` へ送る。
///
/// 受信側レート上限以下に保つため、固定 1 秒窓で [`SYNC_SAFE_MSGS_PER_SEC`] 件 /
/// [`SYNC_SAFE_BYTES_PER_SEC`] バイトを超えそうになったら次窓まで待つ。`tx` が閉じたら
/// (接続切断)途中で打ち切る。`event_messages` は `Message::Event` の列、`count` は件数。
pub async fn stream_sync_response(
    event_messages: Vec<Message>,
    count: u32,
    tx: UnboundedSender<Message>,
) {
    let mut window_start = Instant::now();
    let mut msgs = 0usize;
    let mut bytes = 0usize;

    for message in event_messages {
        let size = est_wire_bytes(&message);
        if msgs + 1 > SYNC_SAFE_MSGS_PER_SEC || bytes + size > SYNC_SAFE_BYTES_PER_SEC {
            let elapsed = window_start.elapsed();
            if elapsed < Duration::from_secs(1) {
                tokio::time::sleep(Duration::from_secs(1) - elapsed).await;
            }
            window_start = Instant::now();
            msgs = 0;
            bytes = 0;
        }
        if tx.send(message).is_err() {
            // 接続が閉じた。SYNC_DONE も送れないため打ち切る。
            return;
        }
        msgs += 1;
        bytes += size;
    }
    let _ = tx.send(Message::SyncDone { count });
}

/// 受信側の SYNC 応答量トラッカー(検査 6)。
///
/// `SYNC_REQ` 送信後、`SYNC_DONE` 受信までに受け取った EVENT 数を数え、`event_store_max` を
/// 超えたら切断すべきと判定する。EVENT はローカル gossip と区別できないため、SYNC 待機中に
/// 受けた EVENT を保守的に計上する(DoS ガード)。
#[derive(Debug)]
pub struct SyncCounter {
    pending: bool,
    received: usize,
    max: usize,
}

impl SyncCounter {
    /// 上限(`event_store_max`)を与えて作る。
    pub fn new(max: usize) -> Self {
        Self {
            pending: false,
            received: 0,
            max,
        }
    }

    /// `SYNC_REQ` を送った(応答待機を開始)。
    pub fn begin(&mut self) {
        self.pending = true;
        self.received = 0;
    }

    /// EVENT を 1 件受けた。上限超過(切断すべき)なら `true`。
    pub fn on_event(&mut self) -> bool {
        if !self.pending {
            return false;
        }
        self.received += 1;
        self.received > self.max
    }

    /// `SYNC_DONE` を受けた(応答完了)。
    pub fn on_done(&mut self) {
        self.pending = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::sync::mpsc;

    #[test]
    fn since_is_now_minus_window_floored_at_zero() {
        assert_eq!(sync_req_since(1000, 600), 400);
        assert_eq!(sync_req_since(100, 600), 0);
    }

    #[test]
    fn counter_trips_over_max() {
        let mut c = SyncCounter::new(2);
        assert!(!c.on_event(), "pending でないと計上しない");
        c.begin();
        assert!(!c.on_event());
        assert!(!c.on_event());
        assert!(c.on_event(), "max=2 を超える 3 件目で切断");
    }

    #[test]
    fn counter_resets_between_sync_rounds() {
        let mut c = SyncCounter::new(1);
        c.begin();
        assert!(!c.on_event());
        c.on_done();
        // 次のラウンドは 0 から
        c.begin();
        assert!(!c.on_event());
    }

    #[tokio::test]
    async fn small_response_sends_all_then_done() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let msgs = vec![
            Message::Event {
                event: json!({"a":1}),
            },
            Message::Event {
                event: json!({"a":2}),
            },
        ];
        stream_sync_response(msgs, 2, tx).await;
        let m1 = rx.recv().await.unwrap();
        let m2 = rx.recv().await.unwrap();
        let done = rx.recv().await.unwrap();
        assert!(matches!(m1, Message::Event { .. }));
        assert!(matches!(m2, Message::Event { .. }));
        assert_eq!(done, Message::SyncDone { count: 2 });
    }

    #[tokio::test]
    async fn aborts_when_receiver_dropped() {
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        // 受信側が消えていても panic せず戻る。
        stream_sync_response(
            vec![Message::Event {
                event: json!({"a":1}),
            }],
            1,
            tx,
        )
        .await;
    }
}

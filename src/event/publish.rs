//! 掲載エンジン(T029 — contracts/nostr-events.md 発行規則)
//!
//! AnnouncedChannel(PCP 層)から変換された [`ChannelListing`] を情報源に、
//! チャンネルへ割り当てたペルソナで kind 30311 を署名し発行する:
//!
//! - 発行 = 自ノードの EventStore へ格納し、established 全ピアへ `EVENT` 送信
//!   (格納・送信は [`EventSink`] の実装 = gossip ハブが担う)
//! - `republish_interval_sec`(既定 60 秒)周期の再発行 + PCP 変更契機の即時再発行
//! - 配信終了(playing=false / PCP 切断)時に `status=ended` の最終発行
//! - 署名鍵は当該チャンネルに割り当てたペルソナのもの。他ペルソナの情報を
//!   イベントに含めてはならない(FR-013 — 契約テスト T024 で検証)
//!
//! 掲載中のペルソナ再割当は本エンジンが検出し、**旧ペルソナで `ended` を発行してから**
//! 新ペルソナで live を発行する(旧鍵の行が鮮度切れまで一覧に残る二重表示を防ぐ)。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nostr::Event;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::event::schema::{ChannelListing, ChannelStatus, EventBuildError};
use crate::identity::{IdentityError, IdentityManager};

/// 発行イベントの受け口(gossip ハブが実装する)。
///
/// 実装の契約: イベントを自ノードの EventStore へ格納し、格納成功
/// (置換規則で伝搬対象になった)なら established 全ピアへ `EVENT` 送信する。
pub trait EventSink: Send + Sync {
    /// 格納・伝搬されたら `true`(置換規則で棄却されたら `false`)。
    fn publish_local(&self, event: Event) -> bool;
}

/// 発行時のエラー。
#[derive(Debug)]
pub enum PublishError {
    /// ペルソナ関連(利用不可・破棄済み等)。
    Identity(IdentityError),
    /// イベント構築・署名の失敗。
    Build(EventBuildError),
}

impl std::fmt::Display for PublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PublishError::Identity(e) => write!(f, "{e}"),
            PublishError::Build(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for PublishError {}

/// チャンネルごとの発行状態(再割当検出・created_at 単調性)。
struct ChannelPublishState {
    /// 最後に署名したペルソナ pubkey。
    persona: String,
    /// 最後に使った created_at(同一秒内の連続更新でも置換が成立するよう単調増加させる)。
    last_created_at: u64,
}

/// 掲載エンジン。`Arc` 共有で PCP 購読タスク・周期再発行ループから使う。
pub struct PublishEngine {
    identity: Arc<IdentityManager>,
    sink: Arc<dyn EventSink>,
    republish_interval: Duration,
    /// 発行側 PoW 難易度(v1 既定 0 = 付与しない)。
    pow_bits: u8,
    states: Mutex<HashMap<String, ChannelPublishState>>,
}

/// ポイズン時も内部値を回収してロックを返す(パニックしない)。
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl PublishEngine {
    /// エンジンを作成する。
    pub fn new(
        identity: Arc<IdentityManager>,
        sink: Arc<dyn EventSink>,
        republish_interval_sec: u64,
    ) -> Self {
        Self {
            identity,
            sink,
            republish_interval: Duration::from_secs(republish_interval_sec.max(1)),
            pow_bits: 0,
            states: Mutex::new(HashMap::new()),
        }
    }

    /// 掲載中チャンネルの現在値を発行する(PCP 変更契機・周期再発行の共通経路)。
    ///
    /// ペルソナ未選択・未割当なら発行せず `Ok(false)`(利用者がペルソナを
    /// 作成・選択するまで掲載は保留される)。
    pub fn publish_listing(&self, listing: &ChannelListing) -> Result<bool, PublishError> {
        let Some(pubkey) = self
            .identity
            .persona_for_channel(&listing.channel_id)
            .map_err(PublishError::Identity)?
        else {
            tracing::warn!(
                target: "publish",
                channel = %listing.channel_id,
                "ペルソナ未選択のため掲載を保留します"
            );
            return Ok(false);
        };

        // 掲載中の再割当を検出したら、旧ペルソナで ended を先に発行する。
        let previous = lock(&self.states)
            .get(&listing.channel_id)
            .map(|s| s.persona.clone());
        if let Some(prev) = previous
            && prev != pubkey
        {
            let mut ended = listing.clone();
            ended.status = ChannelStatus::Ended;
            // 旧ペルソナが破棄済みで署名できない場合は諦める(鮮度切れで自然除去)。
            if let Err(e) = self.sign_and_send(&prev, &ended) {
                tracing::debug!(
                    target: "publish",
                    channel = %listing.channel_id,
                    "旧ペルソナでの ended 発行に失敗しました: {e}"
                );
            }
        }

        self.sign_and_send(&pubkey, listing)?;
        Ok(true)
    }

    /// 配信終了の最終発行(`status=ended`)。以後このチャンネルの発行状態を破棄する。
    pub fn publish_ended(&self, listing: &ChannelListing) -> Result<bool, PublishError> {
        let persona = lock(&self.states)
            .get(&listing.channel_id)
            .map(|s| s.persona.clone());
        let Some(pubkey) = (match persona {
            Some(p) => Some(p),
            None => self
                .identity
                .persona_for_channel(&listing.channel_id)
                .map_err(PublishError::Identity)?,
        }) else {
            // 一度も掲載していない(ペルソナ未選択のまま終了)なら何もしない。
            lock(&self.states).remove(&listing.channel_id);
            return Ok(false);
        };

        let mut ended = listing.clone();
        ended.status = ChannelStatus::Ended;
        let result = self.sign_and_send(&pubkey, &ended);
        lock(&self.states).remove(&listing.channel_id);
        result?;
        Ok(true)
    }

    /// 署名して sink へ渡し、発行状態(ペルソナ・created_at 単調性)を更新する。
    fn sign_and_send(&self, pubkey: &str, listing: &ChannelListing) -> Result<(), PublishError> {
        let keys = self
            .identity
            .signing_keys(pubkey)
            .map_err(PublishError::Identity)?;

        // 同一秒内の連続発行でも置換(last-write-wins)が確実に成立するよう単調増加させる。
        let created_at = {
            let states = lock(&self.states);
            let floor = states
                .get(&listing.channel_id)
                .map(|s| s.last_created_at + 1)
                .unwrap_or(0);
            unix_now().max(floor)
        };

        let event = listing
            .sign(&keys, created_at, self.pow_bits)
            .map_err(PublishError::Build)?;
        self.sink.publish_local(event);

        lock(&self.states).insert(
            listing.channel_id.clone(),
            ChannelPublishState {
                persona: pubkey.to_string(),
                last_created_at: created_at,
            },
        );
        Ok(())
    }

    /// 周期再発行ループを起動する(republish_interval_sec ごとに snapshot を再発行)。
    ///
    /// `snapshot` は掲載中チャンネルの現在値(PCP レジストリのビュー)を返すこと。
    pub fn spawn_republish_loop(
        self: Arc<Self>,
        snapshot: Arc<dyn Fn() -> Vec<ChannelListing> + Send + Sync>,
        mut shutdown: watch::Receiver<bool>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(self.republish_interval);
            // 最初の tick(即時)は読み捨てる — 起動直後の発行は変更契機側が担う。
            ticker.tick().await;
            loop {
                tokio::select! {
                    _ = shutdown.changed() => break,
                    _ = ticker.tick() => {
                        if *shutdown.borrow() {
                            break;
                        }
                        for listing in snapshot() {
                            if let Err(e) = self.publish_listing(&listing) {
                                tracing::debug!(target: "publish", "周期再発行に失敗しました: {e}");
                            }
                        }
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::schema::Track;
    use crate::identity::Keystore;
    use crate::store::Store;

    /// 発行イベントを記録するだけの sink。
    #[derive(Default)]
    struct RecordingSink {
        events: Mutex<Vec<Event>>,
    }

    impl EventSink for RecordingSink {
        fn publish_local(&self, event: Event) -> bool {
            lock(&self.events).push(event);
            true
        }
    }

    impl RecordingSink {
        fn events(&self) -> Vec<Event> {
            lock(&self.events).clone()
        }
    }

    fn listing() -> ChannelListing {
        ChannelListing {
            channel_id: "0123456789abcdef0123456789abcdef".into(),
            title: "テスト配信".into(),
            summary: None,
            genre: Some("game".into()),
            status: ChannelStatus::Live,
            starts: 1_700_000_000,
            current_participants: 1,
            streaming: None,
            bitrate_kbps: Some(500),
            content_type: Some("FLV".into()),
            tip: Some("198.51.100.1:7144".into()),
            contact: None,
            relays: 0,
            track: Some(Track::default()),
        }
    }

    fn engine() -> (Arc<PublishEngine>, Arc<IdentityManager>, Arc<RecordingSink>) {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let identity = Arc::new(IdentityManager::new(store, Keystore::ephemeral()));
        let sink = Arc::new(RecordingSink::default());
        let engine = Arc::new(PublishEngine::new(
            Arc::clone(&identity),
            Arc::clone(&sink) as Arc<dyn EventSink>,
            60,
        ));
        (engine, identity, sink)
    }

    fn tag_value(event: &Event, name: &str) -> Option<String> {
        event.tags.iter().find_map(|t| {
            let s = t.as_slice();
            (s.first().map(String::as_str) == Some(name)).then(|| s[1].clone())
        })
    }

    #[test]
    fn held_until_persona_exists_then_published() {
        let (engine, identity, sink) = engine();
        // ペルソナがないうちは保留
        assert!(!engine.publish_listing(&listing()).unwrap());
        assert!(sink.events().is_empty());

        // ペルソナ作成(自動選択)後は発行される
        let p = identity.create("配信用").unwrap();
        assert!(engine.publish_listing(&listing()).unwrap());
        let events = sink.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].pubkey.to_hex(), p.pubkey);
        assert!(
            events[0].verify().is_ok(),
            "発行イベントは検証可能な署名を持つ"
        );
        assert_eq!(tag_value(&events[0], "status").as_deref(), Some("live"));
    }

    #[test]
    fn ended_final_event_and_state_cleanup() {
        let (engine, identity, sink) = engine();
        identity.create("配信用").unwrap();
        engine.publish_listing(&listing()).unwrap();
        assert!(engine.publish_ended(&listing()).unwrap());

        let events = sink.events();
        assert_eq!(events.len(), 2);
        assert_eq!(tag_value(&events[1], "status").as_deref(), Some("ended"));
        assert!(
            lock(&engine.states).is_empty(),
            "終了後は発行状態を破棄する"
        );
    }

    #[test]
    fn created_at_is_monotonic_within_same_second() {
        let (engine, identity, sink) = engine();
        identity.create("配信用").unwrap();
        engine.publish_listing(&listing()).unwrap();
        engine.publish_listing(&listing()).unwrap();
        engine.publish_listing(&listing()).unwrap();
        let events = sink.events();
        assert!(
            events[0].created_at < events[1].created_at
                && events[1].created_at < events[2].created_at,
            "同一秒内の連続発行でも created_at は厳密に単調増加する"
        );
    }

    #[test]
    fn reassignment_publishes_ended_under_old_persona_first() {
        let (engine, identity, sink) = engine();
        let a = identity.create("A").unwrap();
        let b = identity.create("B").unwrap();
        let l = listing();

        engine.publish_listing(&l).unwrap(); // A(選択中)で live
        identity.assign_channel(&l.channel_id, &b.pubkey).unwrap();
        engine.publish_listing(&l).unwrap(); // 再割当 → A で ended → B で live

        let events = sink.events();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].pubkey.to_hex(), a.pubkey);
        assert_eq!(tag_value(&events[1], "status").as_deref(), Some("ended"));
        assert_eq!(
            events[1].pubkey.to_hex(),
            a.pubkey,
            "ended は旧ペルソナで発行"
        );
        assert_eq!(
            events[2].pubkey.to_hex(),
            b.pubkey,
            "live は新ペルソナで発行"
        );
        assert_eq!(tag_value(&events[2], "status").as_deref(), Some("live"));
    }

    #[test]
    fn ended_without_any_publish_is_noop() {
        let (engine, _identity, sink) = engine();
        // ペルソナ未作成のまま終了 → 何も発行しない
        assert!(!engine.publish_ended(&listing()).unwrap());
        assert!(sink.events().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn republish_loop_reissues_snapshot() {
        let (engine, identity, sink) = engine();
        identity.create("配信用").unwrap();
        let snapshot: Arc<dyn Fn() -> Vec<ChannelListing> + Send + Sync> =
            Arc::new(|| vec![listing()]);
        let (tx, rx) = watch::channel(false);

        let handle = Arc::clone(&engine).spawn_republish_loop(snapshot, rx);
        // 2 周期分すすめる(paused クロックは sleep で自動前進)
        tokio::time::sleep(Duration::from_secs(121)).await;
        tx.send(true).unwrap();
        handle.await.unwrap();

        let n = sink.events().len();
        assert!(n >= 2, "60 秒周期で再発行される(実測 {n} 件)");
    }
}

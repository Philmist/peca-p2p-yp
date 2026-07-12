//! イベント層(nostr 援用境界 — ADR-0002 §3)
//!
//! 本モジュールだけが `nostr` クレート(データ構造・署名・検証)に依存する。
//! `src/p2p/` は署名済みイベント JSON をオペークなペイロードとして運ぶ。
//!
//! - [`schema`] — kind 30311 のタグ写像・署名生成/検証・受信検証パイプライン(T015)
//! - [`store`] — EventStore(置換ストア)・DedupCache(重複抑制)(T016)
//! - [`view`] — DiscoveredChannel ビュー(T039)
//! - [`publish`] — 掲載エンジン(T029)
//! - [`livechat`] — kind 1311/21311/31311 のスキーマ・検証(006-livechat-thread T010)

pub mod livechat;
pub mod publish;
pub mod schema;
pub mod store;
pub mod view;

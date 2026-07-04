//! イベント層(nostr 援用境界 — ADR-0002 §3)
//!
//! 本モジュールだけが `nostr` クレート(データ構造・署名・検証)に依存する。
//! `src/p2p/` は署名済みイベント JSON をオペークなペイロードとして運ぶ。
//!
//! - [`schema`] — kind 30311 のタグ写像・署名生成/検証・受信検証パイプライン(T015)
//! - [`store`] — EventStore(置換ストア)・DedupCache(重複抑制)(T016)

pub mod schema;
pub mod store;

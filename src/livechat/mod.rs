//! livechat モジュール骨格(T006)
//!
//! 配送・状態機械を担う新規モジュール。nostr 援用境界の外(ADR-0002 §3)——
//! イベント封筒自体のスキーマ・検証は [`crate::event::livechat`] が担う。
//!
//! - [`host`] — シーケンサ(採番・ORDER 発行・次スレ移行・BAN 強制・PoW/レート判定)(T019〜)
//! - [`session`] — 参加者セッション(JOIN/チャレンジ検証/同期/凍結・復帰)(T022〜)
//! - [`thread`] — スレ状態(Active/Frozen/Closed・レス列・seq 検証)(T013)
//! - [`board`] — 板・板設定・板鍵(ローテーション)(T012)
//! - [`moderation`] — NG/BAN(完全鍵照合)(T041〜)

pub mod board;
pub mod host;
pub mod moderation;
pub mod participant;
pub mod registry;
pub mod session;
pub mod thread;

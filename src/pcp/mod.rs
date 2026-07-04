//! PCP アナウンス受信層(T025〜T027)
//!
//! PeerCastStation から PCP(PeerCast Protocol)で受けたチャンネル情報を受理し、掲載中
//! チャンネルとして管理する。contracts/pcp-announce.md のクリーンルーム実装であり、GPL コードは
//! 参照しない(research R9)。掲載イベント(kind 30311)の署名・伝搬は上位(掲載エンジン
//! T029・event/p2p 層)の責務で、本層は**ペルソナを知らない**。
//!
//! - [`atom`]: PCP atom コーデック(符号化/復号・入力検証)
//! - [`channel`]: [`AnnouncedChannel`](channel::AnnouncedChannel) と
//!   [`ChannelRegistry`](channel::ChannelRegistry)(検証・メモリ管理・変更通知)
//! - [`session`]: announce セッション状態機械と待受サーバ [`serve`](session::serve)

pub mod atom;
pub mod channel;
pub mod session;

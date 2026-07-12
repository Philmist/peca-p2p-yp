//! ホスト側シーケンサ骨格(T006)
//!
//! 採番・ORDER 発行・次スレ移行・BAN 強制・PoW/レート判定を担う(T019/T021/T023/
//! T030/T032/T036/T037/T042/T044/T046/T047)。TLC 検査済み PlusCal モデル
//! (docs/formal/livechat_sequencer.tla)に対応するコードは意図コメント必須(T030)。

//! スレ状態骨格(T006)
//!
//! Thread(board_id / channel / gen / key / title / res_limit スナップショット /
//! state)・BoardSettings・Res・OrderInfo と状態遷移(Active/Frozen/Closed)・
//! 不変条件 T1/T2/T3 を実装する(T013・data-model §エンティティ)。

//! 配信中ロック(T002/T022 — ADR-0011、data-model §BroadcastState)
//!
//! 「配信中(1 つ以上のチャンネルを実際にネットワークへ発行中)」であるかを表す
//! 揮発の共有状態。selected ペルソナの変更/破棄/アーカイブと発行開始(予約)を
//! **単一のミューテックス**で相互排他にし、配信中の区間に selected が入れ替わる
//! (= 同一 ChannelID 上で旧ペルソナ ended → 新ペルソナ live というリンク推定
//! シグナル — ADR-0004 §7)を構造的に防ぐ。
//!
//! `IdentityManager`・`PublishEngine`・Web の `AppState` がそれぞれ
//! `Arc<BroadcastState>` を保持する(相互に相手を所有しない — 循環依存回避、research R3)。
//!
//! ## 不変条件(保安上複雑なロジック — 意図明記は Principle III MUST)
//!
//! - **INV-1(相互排他)**: 「配信中集合への予約 + 署名ペルソナ読取」と「selected の
//!   変更/破棄/アーカイブ判定」は同一ロック([`channels`](BroadcastState::channels))下で
//!   相互排他に実行される。どちらのクリティカルセクションが先に成立しても不変条件
//!   「配信中の区間、当該チャンネルの署名ペルソナは変化しない」が保たれる(research R2)。
//! - **INV-2(予約先行)**: あるチャンネルの初回署名は、当該チャンネルを配信中集合へ
//!   **予約した後に**行う([`reserve_and_read_selected`](BroadcastState::reserve_and_read_selected))。
//!   **署名の暗号処理はロックの外**で行い(ロック保持時間を最小化)、失敗時は予約を
//!   巻き戻す。予約を署名より先に置かないと「署名中に select が空集合を見て通る」窓が
//!   残る(research R2)。なお `read`(署名ペルソナ解決 = `persona_for_channel` →
//!   `selected`)は selected の usable 判定のため復号(`unprotect`)を伴い、これは**意図的に
//!   ロック下**で行う(FR-011/R5)。selected の usable 判定を `select`/archive の直列化と
//!   同じロック下に置くことで「判定は通ったが直後に archive されて archived 鍵で署名」という
//!   TOCTOU を閉塞する。ロック下で禁じるのは**署名の暗号処理**であって解決時の復号ではない。
//! - **INV-3(確実な解錠)**: チャンネルは終了発行(`publish_ended`)・署名失敗の巻き戻しで
//!   配信中集合から必ず除去される([`release`](BroadcastState::release))。PCP 異常切断も
//!   ended 経路を通るため配信中状態が取り残されない(FR-009)。

use std::collections::HashSet;
use std::sync::{Mutex, MutexGuard};

/// 配信中チャンネル集合と、選択変更との相互排他ロック(永続化しない)。
///
/// 既定(`new`)は空集合 = never-broadcasting。未配線の経路(既存テスト等)は
/// 常に空の `Arc` を持つため、ロックガードは no-op になり挙動が変わらない(research R3)。
#[derive(Debug, Default)]
pub struct BroadcastState {
    /// 現在配信中(初回発行済み・未終了)のチャンネル ID(hex32 小文字)集合。
    /// 相互排他ロックの本体(INV-1)。
    channels: Mutex<HashSet<String>>,
}

/// ポイズン時も内部値を回収してロックを返す(パニックしない)。
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl BroadcastState {
    /// never-broadcasting(空集合)の共有状態を作る。
    pub fn new() -> Self {
        Self::default()
    }

    /// 1 つ以上のチャンネルを発行中か(= 配信中。FR-008 の「配信中」定義)。
    ///
    /// 保留中(未発行)チャンネルは集合に入れないため含まれない(FR-008)。
    pub fn is_broadcasting(&self) -> bool {
        !lock(&self.channels).is_empty()
    }

    /// 発行開始: ロック下で署名ペルソナを解決し、当該チャンネルを配信中集合へ予約する
    /// (INV-1/INV-2)。
    ///
    /// `read` はロック保持下で評価する「このチャンネルの署名ペルソナ解決」
    /// (`persona_for_channel` 相当)。`Some` を返したときのみチャンネルを予約する
    /// (`None` = ペルソナ未選択・archived・利用不可で掲載保留のときは予約しない)。
    /// `read` は selected の usable 判定で復号を伴い、それは `select`/archive との原子性の
    /// ため**意図的にロック下**で行う(TOCTOU 閉塞 — INV-2)。一方、返したペルソナでの
    /// **署名**(暗号処理)は呼び出し側が**ロックの外で**行う(INV-2 — ロック保持中に
    /// 署名の暗号処理をしない)。
    pub fn reserve_and_read_selected<E>(
        &self,
        channel_id: &str,
        read: impl FnOnce() -> Result<Option<String>, E>,
    ) -> Result<Option<String>, E> {
        let mut channels = lock(&self.channels);
        let persona = read()?;
        if persona.is_some() {
            channels.insert(channel_id.to_ascii_lowercase());
        }
        Ok(persona)
    }

    /// 解錠: チャンネルを配信中集合から除去する(終了発行・署名失敗の巻き戻し — INV-3)。
    pub fn release(&self, channel_id: &str) {
        lock(&self.channels).remove(&channel_id.to_ascii_lowercase());
    }

    /// selected 変更のガード: ロック下で現在の配信中真偽を確定し、`mutate` を評価する
    /// (INV-1)。
    ///
    /// `mutate` はロック保持下で評価され、引数 `broadcasting`(配信中集合が非空か)を見て
    /// 「配信中は selected の切替/破棄/アーカイブを拒否」を判断する(呼び出し側 =
    /// `IdentityManager` が対象が selected か否かも含めて判定し、拒否なら
    /// `BroadcastingLocked` を返す)。変更(SQLite の 1 行更新)自体もこのロック下で行い、
    /// 発行開始の予約と原子的に相互排他する。
    pub fn guard_selected_mutation<T, E>(
        &self,
        mutate: impl FnOnce(bool) -> Result<T, E>,
    ) -> Result<T, E> {
        let channels = lock(&self.channels);
        let broadcasting = !channels.is_empty();
        mutate(broadcasting)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_never_broadcasting() {
        let bs = BroadcastState::new();
        assert!(!bs.is_broadcasting(), "既定は never-broadcasting");
    }

    #[test]
    fn reserve_marks_broadcasting_then_release_clears() {
        let bs = BroadcastState::new();
        let ch = "0123456789abcdef0123456789abcdef";
        // Some を返す read は予約する。
        let got = bs
            .reserve_and_read_selected::<()>(ch, || Ok(Some("pk".to_string())))
            .unwrap();
        assert_eq!(got.as_deref(), Some("pk"));
        assert!(bs.is_broadcasting(), "予約後は配信中");
        bs.release(ch);
        assert!(!bs.is_broadcasting(), "解錠後は非配信中");
    }

    #[test]
    fn reserve_with_none_does_not_broadcast() {
        let bs = BroadcastState::new();
        let ch = "0123456789abcdef0123456789abcdef";
        let got = bs.reserve_and_read_selected::<()>(ch, || Ok(None)).unwrap();
        assert_eq!(got, None);
        assert!(!bs.is_broadcasting(), "ペルソナ未選択(保留)は予約しない");
    }

    #[test]
    fn guard_reports_broadcasting_flag() {
        let bs = BroadcastState::new();
        // 非配信中: broadcasting=false が渡る。
        let flag = bs.guard_selected_mutation::<bool, ()>(Ok).unwrap();
        assert!(!flag);
        // 予約後: broadcasting=true が渡る。
        bs.reserve_and_read_selected::<()>("aa".repeat(16).as_str(), || Ok(Some("pk".into())))
            .unwrap();
        let flag = bs.guard_selected_mutation::<bool, ()>(Ok).unwrap();
        assert!(flag);
    }
}

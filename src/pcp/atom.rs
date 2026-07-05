//! PCP atom コーデック(T025)
//!
//! contracts/pcp-announce.md の PCP アナウンス受信で用いる atom の符号化/復号。
//! 参考資料 gist(PeerCastStation 実装準拠)のクリーンルーム実装であり GPL コードは
//! 参照しない(research R9)。
//!
//! ## ワイヤ形式
//! atom = `名前(4 バイト FourCC)` + `長さ(4 バイト・リトルエンディアン)` + ペイロード。
//! 長さフィールドの最上位ビット(`0x8000_0000`)が立っていれば**親 atom**で、下位 31 ビットが
//! 子 atom の個数を表す(子 atom が続く)。立っていなければ**データ atom**で、長さはペイロードの
//! バイト数を表す。名前が 4 文字未満のときは末尾を `0x00` で詰める。
//! 文字列ペイロードは末尾に NUL 終端を付ける(PeerCast の慣行)。
//!
//! ## 入力検証(Principle II)
//! - atom のネスト深さ ≤ 8(超過は [`AtomError::NestTooDeep`])
//! - 1 データ atom のペイロード ≤ 64KB(超過は [`AtomError::PayloadTooLarge`]。**ペイロードを
//!   確保する前に長さ前置で拒否**する — 過大メモリ確保を避ける)
//! - 1 親 atom の子個数 ≤ 1024([`AtomError::TooManyChildren`]。巨大な個数申告による
//!   無制限バッファリングを防ぐ)
//!
//! **未知・非対応の atom はコーデックでは区別しない**(汎用の atom 木として復号する)。
//! 未知 atom の無視は解釈側([`crate::pcp::session`])の責務で、切断・セキュリティイベントとは
//! しない(前方互換)。

use std::borrow::Cow;

/// 1 データ atom のペイロード上限(64KB)。
pub const MAX_ATOM_PAYLOAD: usize = 64 * 1024;
/// atom のネスト深さ上限。
pub const MAX_NEST_DEPTH: usize = 8;
/// 1 親 atom の子 atom 個数上限(無制限バッファリング防止)。
pub const MAX_CHILDREN: usize = 1024;
/// 親 atom を表す長さフィールドの最上位ビット。
const PARENT_FLAG: u32 = 0x8000_0000;

/// atom 名(4 バイト FourCC)。4 文字未満は末尾 `0x00` 詰め。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AtomId([u8; 4]);

impl AtomId {
    /// 4 バイトからそのまま作る。
    pub const fn new(bytes: [u8; 4]) -> Self {
        Self(bytes)
    }

    /// 名前文字列から作る(4 バイトを超える分は切詰め、不足は `0x00` 詰め)。
    pub fn from_name(name: &str) -> Self {
        let mut b = [0u8; 4];
        for (dst, src) in b.iter_mut().zip(name.bytes()) {
            *dst = src;
        }
        Self(b)
    }

    /// 生の 4 バイト。
    pub fn as_bytes(&self) -> &[u8; 4] {
        &self.0
    }

    /// 末尾 NUL を除いた名前(表示・ログ用。非 UTF-8 は lossy 変換)。
    pub fn name(&self) -> Cow<'_, str> {
        let end = self.0.iter().position(|&b| b == 0).unwrap_or(4);
        String::from_utf8_lossy(&self.0[..end])
    }

    /// 指定名と一致するか。
    pub fn matches(&self, name: &str) -> bool {
        *self == AtomId::from_name(name)
    }
}

/// PCP atom(親 = 子の列、データ = バイト列)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Atom {
    /// 子 atom を持つ親 atom。
    Parent(AtomId, Vec<Atom>),
    /// バイト列ペイロードを持つデータ atom。
    Data(AtomId, Vec<u8>),
}

/// atom 復号のエラー(いずれも入力検証違反 — Principle II)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtomError {
    /// ネスト深さが上限を超えた。
    NestTooDeep,
    /// データ atom のペイロードが上限を超えた。
    PayloadTooLarge,
    /// 親 atom の子個数が上限を超えた。
    TooManyChildren,
}

impl AtomError {
    /// セキュリティイベント・CLOSE 用の短い理由(内部情報を含めない — Principle II)。
    pub fn reason(self) -> &'static str {
        match self {
            AtomError::NestTooDeep => "atom nesting too deep",
            AtomError::PayloadTooLarge => "atom payload too large",
            AtomError::TooManyChildren => "too many child atoms",
        }
    }
}

impl std::fmt::Display for AtomError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.reason())
    }
}

impl std::error::Error for AtomError {}

impl Atom {
    /// 親 atom を作る。
    pub fn parent(name: &str, children: Vec<Atom>) -> Atom {
        Atom::Parent(AtomId::from_name(name), children)
    }

    /// データ atom を作る。
    pub fn data(name: &str, payload: Vec<u8>) -> Atom {
        Atom::Data(AtomId::from_name(name), payload)
    }

    /// バイト列データ atom を作る。
    pub fn bytes(name: &str, payload: &[u8]) -> Atom {
        Atom::Data(AtomId::from_name(name), payload.to_vec())
    }

    /// NUL 終端付き文字列データ atom を作る(PeerCast の慣行)。
    pub fn str(name: &str, s: &str) -> Atom {
        let mut payload = s.as_bytes().to_vec();
        payload.push(0);
        Atom::Data(AtomId::from_name(name), payload)
    }

    /// 4 バイト LE の整数データ atom を作る。
    pub fn i32(name: &str, v: i32) -> Atom {
        Atom::Data(AtomId::from_name(name), v.to_le_bytes().to_vec())
    }

    /// 2 バイト LE の整数データ atom を作る。
    pub fn i16(name: &str, v: i16) -> Atom {
        Atom::Data(AtomId::from_name(name), v.to_le_bytes().to_vec())
    }

    /// 2 バイト LE の符号なし整数データ atom を作る(ポート番号など)。
    pub fn u16v(name: &str, v: u16) -> Atom {
        Atom::Data(AtomId::from_name(name), v.to_le_bytes().to_vec())
    }

    /// 1 バイトの整数データ atom を作る。
    pub fn u8v(name: &str, v: u8) -> Atom {
        Atom::Data(AtomId::from_name(name), vec![v])
    }

    /// atom 名。
    pub fn id(&self) -> &AtomId {
        match self {
            Atom::Parent(id, _) | Atom::Data(id, _) => id,
        }
    }

    /// 親 atom なら子の列。
    pub fn children(&self) -> Option<&[Atom]> {
        match self {
            Atom::Parent(_, c) => Some(c),
            Atom::Data(_, _) => None,
        }
    }

    /// データ atom ならペイロード。
    pub fn payload(&self) -> Option<&[u8]> {
        match self {
            Atom::Data(_, p) => Some(p),
            Atom::Parent(_, _) => None,
        }
    }

    /// 子 atom(親の直下)を名前で探す。データ atom は常に `None`。
    pub fn find(&self, name: &str) -> Option<&Atom> {
        let target = AtomId::from_name(name);
        self.children()?.iter().find(|a| *a.id() == target)
    }

    /// データペイロードを LE 整数として読む(長さ 1/2/4 に対応)。
    ///
    /// 長さ 4 は符号付き `i32`、長さ 1/2 は符号なしゼロ拡張。他の長さは `None`。
    pub fn as_i32(&self) -> Option<i32> {
        let p = self.payload()?;
        match p.len() {
            1 => Some(p[0] as i32),
            2 => Some(u16::from_le_bytes([p[0], p[1]]) as i32),
            4 => Some(i32::from_le_bytes([p[0], p[1], p[2], p[3]])),
            _ => None,
        }
    }

    /// データペイロードを文字列として読む(末尾 NUL を除去、非 UTF-8 は lossy)。
    pub fn as_str(&self) -> Option<String> {
        let p = self.payload()?;
        let end = p.iter().rposition(|&b| b != 0).map(|i| i + 1).unwrap_or(0);
        Some(String::from_utf8_lossy(&p[..end]).into_owned())
    }

    /// atom をワイヤバイト列へ符号化して `out` の末尾へ追記する。
    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Atom::Data(id, payload) => {
                out.extend_from_slice(id.as_bytes());
                out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
                out.extend_from_slice(payload);
            }
            Atom::Parent(id, children) => {
                out.extend_from_slice(id.as_bytes());
                let flagged = PARENT_FLAG | (children.len() as u32);
                out.extend_from_slice(&flagged.to_le_bytes());
                for child in children {
                    child.encode(out);
                }
            }
        }
    }

    /// atom をワイヤバイト列へ符号化する。
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode(&mut out);
        out
    }

    /// `buf` の先頭から 1 つの atom を復号する。
    ///
    /// - `Ok(Some((atom, consumed)))`: `consumed` バイトを消費して 1 atom を復号した
    /// - `Ok(None)`: まだフレームが完成していない(バイト不足 — 呼び出し側は追加受信する)
    /// - `Err(_)`: 入力検証違反(ネスト過深・過大ペイロード・過多な子個数)
    pub fn try_decode(buf: &[u8]) -> Result<Option<(Atom, usize)>, AtomError> {
        decode_at(buf, 1)
    }
}

/// `depth`(1 起点)を追跡しながら 1 atom を復号する。
fn decode_at(buf: &[u8], depth: usize) -> Result<Option<(Atom, usize)>, AtomError> {
    if depth > MAX_NEST_DEPTH {
        return Err(AtomError::NestTooDeep);
    }
    // ヘッダ(名前 4 + 長さ 4)が揃うまで待つ。
    if buf.len() < 8 {
        return Ok(None);
    }
    let id = AtomId::new([buf[0], buf[1], buf[2], buf[3]]);
    let len_raw = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);

    if len_raw & PARENT_FLAG != 0 {
        let count = (len_raw & !PARENT_FLAG) as usize;
        if count > MAX_CHILDREN {
            return Err(AtomError::TooManyChildren);
        }
        let mut pos = 8;
        let mut children = Vec::with_capacity(count.min(16));
        for _ in 0..count {
            match decode_at(&buf[pos..], depth + 1)? {
                Some((child, used)) => {
                    pos += used;
                    children.push(child);
                }
                // 子が途中までしか届いていない → フレーム未完成。
                None => return Ok(None),
            }
        }
        Ok(Some((Atom::Parent(id, children), pos)))
    } else {
        let len = len_raw as usize;
        // ペイロードを確保する前に上限で拒否する(過大メモリ確保の防止)。
        if len > MAX_ATOM_PAYLOAD {
            return Err(AtomError::PayloadTooLarge);
        }
        if buf.len() < 8 + len {
            return Ok(None);
        }
        Ok(Some((Atom::Data(id, buf[8..8 + len].to_vec()), 8 + len)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_atom_roundtrips() {
        let atom = Atom::str("name", "テスト配信");
        let bytes = atom.to_bytes();
        let (decoded, used) = Atom::try_decode(&bytes).unwrap().unwrap();
        assert_eq!(used, bytes.len());
        assert_eq!(decoded, atom);
        assert_eq!(decoded.as_str().as_deref(), Some("テスト配信"));
    }

    #[test]
    fn integer_atoms_roundtrip() {
        assert_eq!(Atom::i32("bitr", 1500).as_i32(), Some(1500));
        assert_eq!(Atom::i32("numl", -1).as_i32(), Some(-1));
        assert_eq!(Atom::i16("port", 7144).as_i32(), Some(7144));
        assert_eq!(Atom::u8v("flg1", 0x1C).as_i32(), Some(0x1C));
    }

    #[test]
    fn parent_atom_roundtrips_and_finds_children() {
        let atom = Atom::parent(
            "chan",
            vec![
                Atom::bytes("cid", &[0xAB; 16]),
                Atom::parent("info", vec![Atom::str("name", "A")]),
            ],
        );
        let bytes = atom.to_bytes();
        let (decoded, _) = Atom::try_decode(&bytes).unwrap().unwrap();
        assert_eq!(decoded, atom);
        assert_eq!(
            decoded.find("cid").and_then(|a| a.payload()),
            Some(&[0xAB; 16][..])
        );
        let info = decoded.find("info").unwrap();
        assert_eq!(
            info.find("name").and_then(|a| a.as_str()).as_deref(),
            Some("A")
        );
    }

    #[test]
    fn short_name_is_null_padded() {
        let id = AtomId::from_name("id");
        assert_eq!(id.as_bytes(), &[b'i', b'd', 0, 0]);
        assert_eq!(id.name(), "id");
        assert!(id.matches("id"));
    }

    #[test]
    fn incomplete_header_needs_more() {
        assert_eq!(Atom::try_decode(&[1, 2, 3]).unwrap(), None);
    }

    #[test]
    fn incomplete_payload_needs_more() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&10u32.to_le_bytes());
        bytes.extend_from_slice(&[0, 1, 2]); // 10 バイト宣言に対し 3 バイトのみ
        assert_eq!(Atom::try_decode(&bytes).unwrap(), None);
    }

    #[test]
    fn payload_over_limit_rejected_before_alloc() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"big\0");
        bytes.extend_from_slice(&((MAX_ATOM_PAYLOAD as u32) + 1).to_le_bytes());
        // ペイロードは 1 バイトも付いていないが、長さ前置だけで拒否される。
        assert_eq!(Atom::try_decode(&bytes), Err(AtomError::PayloadTooLarge));
    }

    #[test]
    fn payload_at_limit_accepted() {
        let atom = Atom::data("dat\0".trim_end_matches('\0'), vec![0u8; MAX_ATOM_PAYLOAD]);
        let bytes = atom.to_bytes();
        let (decoded, used) = Atom::try_decode(&bytes).unwrap().unwrap();
        assert_eq!(used, bytes.len());
        assert_eq!(decoded.payload().unwrap().len(), MAX_ATOM_PAYLOAD);
    }

    #[test]
    fn nesting_at_depth_8_ok_depth_9_rejected() {
        // 深さ 8(最深の子はデータ)は受理。
        let mut atom = Atom::data("d", vec![1]);
        for _ in 0..7 {
            atom = Atom::parent("p", vec![atom]);
        }
        assert!(Atom::try_decode(&atom.to_bytes()).unwrap().is_some());

        // 深さ 9 は拒否。
        let deeper = Atom::parent("p", vec![atom]);
        assert_eq!(
            Atom::try_decode(&deeper.to_bytes()),
            Err(AtomError::NestTooDeep)
        );
    }

    #[test]
    fn too_many_children_rejected() {
        // 子個数だけ過大に申告する(実体は付けない)。
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"par\0");
        bytes.extend_from_slice(&(PARENT_FLAG | (MAX_CHILDREN as u32 + 1)).to_le_bytes());
        assert_eq!(Atom::try_decode(&bytes), Err(AtomError::TooManyChildren));
    }

    #[test]
    fn sequential_atoms_consume_incrementally() {
        let a = Atom::str("name", "A");
        let b = Atom::i32("bitr", 42);
        let mut buf = a.to_bytes();
        buf.extend_from_slice(&b.to_bytes());
        let (first, used) = Atom::try_decode(&buf).unwrap().unwrap();
        assert_eq!(first, a);
        let (second, used2) = Atom::try_decode(&buf[used..]).unwrap().unwrap();
        assert_eq!(second, b);
        assert_eq!(used + used2, buf.len());
    }
}

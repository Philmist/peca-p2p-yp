//! gossip ワイヤプロトコルのフレーミングとメッセージ型(T017)
//!
//! contracts/p2p-gossip.md §トランスポート・§メッセージ種別 の正実装。
//!
//! フレーム = `長さ(4 バイト BE、ペイロードのバイト数)` + `ペイロード(JSON, UTF-8)`。
//! 上限は**ペイロード ≤ 64KB**(検査 1)。長さ前置がこれを超えるフレームは
//! ペイロードを読む前に拒否する(過大長ペイロードのメモリ確保を避ける — Principle II)。
//!
//! 本モジュールはトランスポート非依存のフレーム入出力とメッセージ型のみを担う。
//! 受信検証(署名・伝搬・重複抑制)・状態遷移・レート制限は担当しない
//! (それぞれ T037 受信パイプライン・T017 [`crate::p2p::session`]・T016 の責務)。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::security::SecurityCategory;

/// フレームのペイロード上限(検査 1)。長さ前置がこの値を超えると `p2p_oversize`。
pub const MAX_FRAME_PAYLOAD: usize = 64 * 1024;

/// HELLO / HELLO_ACK の本体(contracts/p2p-gossip.md §メッセージ種別)。
///
/// いずれのフィールドも**相手の申告値であり未検証**(Principle II)。
/// `ts` は受信側の時計ずれ自己診断にのみ用い、イベント検証・接続可否の判断に
/// 使用してはならない (MUST NOT — T048 が使用)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// プロトコルバージョン(v1 = 1)。互換判定は**完全一致**。
    pub version: u32,
    /// 相手が TCP 待受中のポート。待受なしは 0。
    pub listen_port: u16,
    /// 機能フラグ。v1 は空配列を送る。**未知値は無視しなければならない (MUST)**。
    pub features: Vec<String>,
    /// 起動時生成の乱数。自己接続検出に用いる。
    pub nonce: u64,
    /// 送信時点のローカル時刻(unix 秒)。未検証の申告値。
    pub ts: i64,
}

/// gossip メッセージ(内部タグ `type` 付き JSON オブジェクト)。
///
/// **未知フィールドは無視**(serde 既定 — 前方互換)、**未知 `type` はデコード失敗**として
/// [`FrameError::InvalidFrame`](検査 3 → `p2p_invalid_frame`)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Message {
    /// 接続開始(発→受)。最初のフレームでなければ切断。
    #[serde(rename = "HELLO")]
    Hello(Hello),
    /// 受理応答(受→発)。バージョン非互換は CLOSE。
    #[serde(rename = "HELLO_ACK")]
    HelloAck(Hello),
    /// イベントの伝搬。`event` は生の nostr イベント JSON を保持する。
    ///
    /// イベント検証(サイズ・署名・形式・時刻・内容・PoW)は T037 の責務で本タスク外。
    /// [`Value`] を用いるのは意図的: byte-exact な保存は不要で、T037 の id/sig 再計算は
    /// ワイヤのバイト順ではなくフィールド `[0,pubkey,created_at,kind,tags,content]` から
    /// 正規配列を組み直すため、キー順を保存しない `Value` で必要十分。
    #[serde(rename = "EVENT")]
    Event { event: Value },
    /// 接続時同期の要求。`since` は unix 秒。
    #[serde(rename = "SYNC_REQ")]
    SyncReq { since: i64 },
    /// SYNC_REQ への応答完了(応答本体は EVENT の列)。
    #[serde(rename = "SYNC_DONE")]
    SyncDone { count: u32 },
    /// ピア交換の要求(FR-015)。
    #[serde(rename = "GET_PEERS")]
    GetPeers,
    /// 検証済みピアの共有(`host:port` 文字列配列、≤ 64 件)。
    #[serde(rename = "PEERS")]
    Peers { peers: Vec<String> },
    /// keepalive 要求。
    #[serde(rename = "PING")]
    Ping { nonce: u64 },
    /// keepalive 応答。
    #[serde(rename = "PONG")]
    Pong { nonce: u64 },
    /// 正常切断。`reason` は定型コードのみ(内部情報を含めてはならない — MUST NOT)。
    #[serde(rename = "CLOSE")]
    Close { reason: String },
}

/// CLOSE の定型 reason コード(内部情報を含めない — MUST NOT)。
///
/// 受信側は前方互換のため未知コードを許容する(文字列として保持)。送信時は本定数を使う。
pub mod close_reason {
    /// バージョン非互換。
    pub const INCOMPATIBLE: &str = "incompatible";
    /// フレーム/JSON/順序違反(`p2p_invalid_frame`)。
    pub const INVALID_FRAME: &str = "invalid_frame";
    /// フレーム長超過(`p2p_oversize`)。
    pub const OVERSIZE: &str = "oversize";
    /// 受信レート超過(`p2p_rate_limited`)。
    pub const RATE_LIMITED: &str = "rate_limited";
    /// 自己接続の検出。
    pub const SELF_CONNECT: &str = "self_connect";
    /// 通常の終了。
    pub const GOING_AWAY: &str = "going_away";
}

/// フレーム受信の結果(メッセージと消費したワイヤバイト数)。
///
/// `wire_len` は長さ前置 4 バイトを含むフレーム全体のバイト数で、受信レート計上
/// ([`crate::p2p::session`] 検査 2)に用いる。
#[derive(Debug, Clone, PartialEq)]
pub struct IncomingFrame {
    /// デコード済みメッセージ。
    pub message: Message,
    /// フレーム全体(4 + ペイロード)のバイト数。
    pub wire_len: usize,
}

/// フレーム入出力のエラー。
#[derive(Debug)]
pub enum FrameError {
    /// フレーム境界での正常な接続終了(未使用: 呼び出し側は `Ok(None)` で受ける)。
    Closed,
    /// フレーム途中での接続終了(切り詰め)。
    Truncated,
    /// 長さ前置が上限超過(検査 1 → `p2p_oversize`)。
    Oversize,
    /// JSON パース不能・未知 `type`(検査 3 → `p2p_invalid_frame`)。
    InvalidFrame,
    /// 下層 I/O エラー。
    Io(std::io::Error),
}

impl FrameError {
    /// セキュリティイベントとして記録すべき場合、`(カテゴリ, CLOSE reason)` を返す。
    ///
    /// 接続終了・I/O エラー(`Closed`/`Truncated`/`Io`)は攻撃とは限らないため `None`。
    pub fn security(&self) -> Option<(SecurityCategory, &'static str)> {
        match self {
            FrameError::Oversize => Some((SecurityCategory::P2pOversize, close_reason::OVERSIZE)),
            FrameError::InvalidFrame => Some((
                SecurityCategory::P2pInvalidFrame,
                close_reason::INVALID_FRAME,
            )),
            FrameError::Closed | FrameError::Truncated | FrameError::Io(_) => None,
        }
    }
}

impl PartialEq for FrameError {
    fn eq(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (FrameError::Closed, FrameError::Closed)
                | (FrameError::Truncated, FrameError::Truncated)
                | (FrameError::Oversize, FrameError::Oversize)
                | (FrameError::InvalidFrame, FrameError::InvalidFrame)
        )
    }
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::Closed => f.write_str("connection closed"),
            FrameError::Truncated => f.write_str("frame truncated"),
            FrameError::Oversize => f.write_str("frame oversize"),
            FrameError::InvalidFrame => f.write_str("invalid frame"),
            FrameError::Io(_) => f.write_str("io error"),
        }
    }
}

impl std::error::Error for FrameError {}

/// `buf` を満たすまで読む。境界での正常な EOF は `Ok(false)`、途中 EOF は `Truncated`。
async fn read_exact_or_eof<R: AsyncRead + Unpin>(
    reader: &mut R,
    buf: &mut [u8],
) -> Result<bool, FrameError> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = reader
            .read(&mut buf[filled..])
            .await
            .map_err(FrameError::Io)?;
        if n == 0 {
            if filled == 0 {
                return Ok(false);
            }
            return Err(FrameError::Truncated);
        }
        filled += n;
    }
    Ok(true)
}

/// 1 フレームを読む。分割・結合到着のいずれにも対応する。
///
/// - `Ok(Some(frame))`: 1 フレームを読み取った
/// - `Ok(None)`: フレーム境界での正常な接続終了
/// - `Err(_)`: 過大長・不正 JSON・切り詰め・I/O エラー
pub async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<Option<IncomingFrame>, FrameError> {
    let mut len_buf = [0u8; 4];
    if !read_exact_or_eof(reader, &mut len_buf).await? {
        return Ok(None);
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    // 検査 1: ペイロードを確保する前に上限を検査する。
    if len > MAX_FRAME_PAYLOAD {
        return Err(FrameError::Oversize);
    }
    let mut payload = vec![0u8; len];
    if !read_exact_or_eof(reader, &mut payload).await? {
        return Err(FrameError::Truncated);
    }
    let message = decode_payload(&payload)?;
    Ok(Some(IncomingFrame {
        message,
        wire_len: 4 + len,
    }))
}

/// ペイロード(JSON バイト列)をメッセージへデコードする(検査 3)。
pub fn decode_payload(payload: &[u8]) -> Result<Message, FrameError> {
    serde_json::from_slice(payload).map_err(|_| FrameError::InvalidFrame)
}

/// メッセージを長さ前置フレームのバイト列へ符号化する。
///
/// ペイロードが上限を超える場合は `Oversize`(送信側で過大フレームを出さない)。
pub fn encode(message: &Message) -> Result<Vec<u8>, FrameError> {
    let payload = serde_json::to_vec(message).map_err(|_| FrameError::InvalidFrame)?;
    if payload.len() > MAX_FRAME_PAYLOAD {
        return Err(FrameError::Oversize);
    }
    let mut out = (payload.len() as u32).to_be_bytes().to_vec();
    out.extend_from_slice(&payload);
    Ok(out)
}

/// 1 フレームを書き出してフラッシュする。
pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    message: &Message,
) -> Result<(), FrameError> {
    let bytes = encode(message)?;
    writer.write_all(&bytes).await.map_err(FrameError::Io)?;
    writer.flush().await.map_err(FrameError::Io)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[tokio::test]
    async fn round_trip_all_variants() {
        let msgs = vec![
            Message::Hello(Hello {
                version: 1,
                listen_port: 7147,
                features: vec![],
                nonce: u64::MAX,
                ts: 1720000000,
            }),
            Message::HelloAck(Hello {
                version: 1,
                listen_port: 0,
                features: vec!["x".into()],
                nonce: 1,
                ts: 1,
            }),
            Message::Event {
                event: serde_json::json!({"kind":30311,"nonce":u64::MAX}),
            },
            Message::SyncReq { since: 4102444800 },
            Message::SyncDone { count: 42 },
            Message::GetPeers,
            Message::Peers {
                peers: vec!["1.2.3.4:7147".into()],
            },
            Message::Ping { nonce: 9 },
            Message::Pong { nonce: 9 },
            Message::Close {
                reason: close_reason::INCOMPATIBLE.into(),
            },
        ];
        for m in msgs {
            let bytes = encode(&m).unwrap();
            let mut cur = Cursor::new(bytes);
            let got = read_frame(&mut cur).await.unwrap().unwrap();
            assert_eq!(got.message, m);
        }
    }

    #[tokio::test]
    async fn u64_max_nonce_survives_internally_tagged_roundtrip() {
        // 内部タグ enum は serde の Content バッファを通るため u64 精度を確認する。
        let m = Message::Ping { nonce: u64::MAX };
        let bytes = encode(&m).unwrap();
        let mut cur = Cursor::new(bytes);
        let got = read_frame(&mut cur).await.unwrap().unwrap();
        assert_eq!(got.message, Message::Ping { nonce: u64::MAX });
    }

    #[tokio::test]
    async fn unknown_type_is_invalid_frame() {
        let payload = br#"{"type":"WAT"}"#;
        assert_eq!(
            decode_payload(payload).unwrap_err(),
            FrameError::InvalidFrame
        );
    }

    #[tokio::test]
    async fn unknown_fields_ignored() {
        let payload = br#"{"type":"PING","nonce":7,"future":123}"#;
        assert_eq!(decode_payload(payload).unwrap(), Message::Ping { nonce: 7 });
    }

    #[tokio::test]
    async fn oversize_length_prefix_rejected() {
        let over = (MAX_FRAME_PAYLOAD as u32 + 1).to_be_bytes();
        let mut cur = Cursor::new(over.to_vec());
        assert_eq!(
            read_frame(&mut cur).await.unwrap_err(),
            FrameError::Oversize
        );
    }

    #[tokio::test]
    async fn clean_eof_at_boundary() {
        let mut cur = Cursor::new(Vec::<u8>::new());
        assert!(read_frame(&mut cur).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn truncated_frame_errors() {
        // 長さ 10 を宣言しつつ 3 バイトしか続かない。
        let mut bytes = 10u32.to_be_bytes().to_vec();
        bytes.extend_from_slice(&[1, 2, 3]);
        let mut cur = Cursor::new(bytes);
        assert_eq!(
            read_frame(&mut cur).await.unwrap_err(),
            FrameError::Truncated
        );
    }
}

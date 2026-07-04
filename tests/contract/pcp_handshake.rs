//! PCP 契約テスト(T023)— pcp エージェントが実装する。
//! contracts/pcp-announce.md: HELO→OLEH→BCST→QUIT フィクスチャ往復+ネガティブ。
//!
//! 検査対象:
//! - HELO→OLEH の往復(OLEH に agent 名 `peca-p2p-yp/<semver>` の固定書式を含む)
//! - BCST によるチャンネル登録(announced→updating)と変更通知
//! - PCP_QUIT / TCP 異常切断 での全チャンネル即 ended(鮮度切れを待たない)
//! - ネガティブ: atom ネスト深さ >8・64KB 超ペイロード・不正 GUID・
//!   loopback 外接続 → 切断+`pcp_reject`
//! - 未知 atom は無視して切断しない・1 セッション 17 チャンネル目は無視+`pcp_reject`

use std::net::SocketAddr;

use peca_p2p_yp::pcp::atom::{Atom, AtomError, MAX_ATOM_PAYLOAD};
use peca_p2p_yp::pcp::channel::{ChannelChange, ChannelRegistry};
use peca_p2p_yp::pcp::session::{
    AnnounceAction, AnnounceSession, PCP_PROTOCOL_VERSION, agent_name, source_is_loopback,
};
use peca_p2p_yp::security::SecurityLog;

// PCP_HOST flg1 のビット(session.rs と同値)。
const FLG1_DIRECT: u8 = 0x04;
const FLG1_PUSH: u8 = 0x08; // firewalled
const FLG1_RECV: u8 = 0x10; // playing

// ---------------------------------------------------------------------------
// テスト補助
// ---------------------------------------------------------------------------

fn temp_security() -> (std::sync::Arc<SecurityLog>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let log = std::sync::Arc::new(SecurityLog::new(dir.path().join("security.log")).unwrap());
    (log, dir)
}

fn helo(sid: &[u8; 16]) -> Atom {
    // 余分な atom(port/ver/agnt)は本ソフトウェアが無視することも兼ねて含める。
    Atom::parent(
        "helo",
        vec![
            Atom::bytes("sid", sid),
            Atom::i16("port", 7144),
            Atom::i32("ver", 1234),
            Atom::str("agnt", "PeerCastStation/3.0"),
        ],
    )
}

/// 実 PCP の BCST 形(chan の ID は `id`、host は bcst 直下、IP は LE 格納 =
/// ワイヤ上オクテット逆順)。contracts/pcp-announce.md 2026-07-04 実機検証改訂。
#[allow(clippy::too_many_arguments)]
fn bcst(cid: &[u8; 16], bcid: &[u8; 16], name: &str, bitrate: i32, flg1: u8) -> Atom {
    Atom::parent(
        "bcst",
        vec![
            Atom::u8v("ttl", 11), // 無視される atom
            Atom::bytes("cid", cid),
            Atom::parent(
                "chan",
                vec![
                    Atom::bytes("id", cid),
                    Atom::bytes("bcid", bcid),
                    Atom::parent(
                        "info",
                        vec![
                            Atom::str("name", name),
                            Atom::str("gnre", "Game"),
                            Atom::str("desc", "説明"),
                            Atom::str("url", "http://example.com/"),
                            Atom::i32("bitr", bitrate),
                            Atom::str("type", "FLV"),
                        ],
                    ),
                    Atom::parent(
                        "trck",
                        vec![
                            Atom::str("titl", "song"),
                            Atom::str("crea", "artist"),
                            Atom::str("albm", "album"),
                        ],
                    ),
                ],
            ),
            Atom::parent(
                "host",
                vec![
                    Atom::bytes("cid", cid),
                    Atom::bytes("ip", &[1, 2, 0, 192]), // 192.0.2.1(LE 格納)
                    Atom::i16("port", 7144),
                    Atom::i32("numl", 5),
                    Atom::i32("numr", 2),
                    Atom::u8v("flg1", flg1),
                ],
            ),
        ],
    )
}

/// atom をワイヤバイト列へ符号化してから復号し直す(コーデック往復を経由させる)。
fn roundtrip(atom: &Atom) -> Atom {
    let bytes = atom.to_bytes();
    let (decoded, used) = Atom::try_decode(&bytes)
        .expect("復号エラーなし")
        .expect("完全なフレーム");
    assert_eq!(used, bytes.len(), "消費バイト数がフレーム長と一致する");
    decoded
}

fn feed(session: &mut AnnounceSession, atom: &Atom) -> Vec<AnnounceAction> {
    let bytes = atom.to_bytes();
    let decoded = roundtrip(atom);
    session
        .on_atom(bytes.len(), decoded)
        .expect("正常 atom は切断されない")
}

// ---------------------------------------------------------------------------
// HELO → OLEH
// ---------------------------------------------------------------------------

#[test]
fn helo_produces_oleh_with_fixed_agent_name() {
    let (sec, _g) = temp_security();
    let registry = ChannelRegistry::new();
    let mut session = AnnounceSession::new(registry, sec, "127.0.0.1:50000".into());

    let sid = [0x11u8; 16];
    let actions = feed(&mut session, &helo(&sid));

    let AnnounceAction::Send(oleh) = actions.first().expect("OLEH を返す");
    // 往復可能な有効 atom であること
    let oleh = roundtrip(oleh);
    assert!(oleh.id().matches("oleh"), "応答は OLEH");

    // agent 名は `peca-p2p-yp/<semver>` の固定書式
    let agnt = oleh
        .find("agnt")
        .and_then(|a| a.as_str())
        .expect("agnt atom");
    assert_eq!(agnt, agent_name());
    assert!(agnt.starts_with("peca-p2p-yp/"), "固定書式: {agnt}");
    assert_eq!(agnt, format!("peca-p2p-yp/{}", env!("CARGO_PKG_VERSION")));

    // sid は自ノードの SessionID(HELO sid のエコーはクライアントの自己接続判定を
    // 誤発火させるため禁止 — 本家 PeerCast 互換)。ver を含む
    let oleh_sid = oleh.find("sid").and_then(|a| a.payload()).expect("sid atom");
    assert_eq!(oleh_sid.len(), 16);
    assert_ne!(oleh_sid, &sid[..], "HELO sid をエコーしない");
    assert_eq!(
        oleh.find("ver").and_then(|a| a.as_i32()),
        Some(PCP_PROTOCOL_VERSION)
    );
    // 接続元から観測した IP を rip(LE 格納 = オクテット逆順)、
    // HELO で申告された待受ポートのエコーを port として返す
    assert_eq!(
        oleh.find("rip").and_then(|a| a.payload()),
        Some(&[1u8, 0, 0, 127][..])
    );
    assert_eq!(oleh.find("port").and_then(|a| a.as_i32()), Some(7144));
}

// ---------------------------------------------------------------------------
// BCST → チャンネル登録・更新・通知
// ---------------------------------------------------------------------------

#[test]
fn bcst_registers_then_updates_channel_with_notifications() {
    let (sec, _g) = temp_security();
    let registry = ChannelRegistry::new();
    let mut rx = registry.subscribe();
    let mut session = AnnounceSession::new(registry.clone(), sec, "127.0.0.1:50001".into());

    let sid = [0x22u8; 16];
    let cid = [0xAAu8; 16];
    feed(&mut session, &helo(&sid));

    // 初回 BCST(playing) → announced
    feed(&mut session, &bcst(&cid, &sid, "配信A", 500, FLG1_RECV | FLG1_DIRECT));
    let snap = registry.snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].channel_id, cid);
    assert_eq!(snap[0].name, "配信A");
    assert_eq!(snap[0].bitrate_kbps, 500);
    assert_eq!(snap[0].listeners, 5);
    assert_eq!(snap[0].relays_cnt, 2);
    assert_eq!(snap[0].tracker.as_deref(), Some("192.0.2.1:7144"));
    match rx.try_recv().expect("announced 通知") {
        ChannelChange::Announced(ch) => assert_eq!(ch.channel_id, cid),
        other => panic!("announced を期待: {other:?}"),
    }

    // 2 回目 BCST(同一 cid・名称変更) → updated
    feed(&mut session, &bcst(&cid, &sid, "配信A2", 800, FLG1_RECV | FLG1_DIRECT));
    let snap = registry.snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].name, "配信A2");
    assert_eq!(snap[0].bitrate_kbps, 800);
    match rx.try_recv().expect("updated 通知") {
        ChannelChange::Updated(ch) => assert_eq!(ch.name, "配信A2"),
        other => panic!("updated を期待: {other:?}"),
    }
}

#[test]
fn firewalled_bcst_has_no_tracker() {
    let (sec, _g) = temp_security();
    let registry = ChannelRegistry::new();
    let mut session = AnnounceSession::new(registry.clone(), sec, "127.0.0.1:50002".into());
    feed(&mut session, &helo(&[0x33u8; 16]));

    let cid = [0xBBu8; 16];
    // PUSH(firewalled)ビットあり → tracker 省略
    feed(&mut session, &bcst(&cid, &[0x33u8; 16], "FW", 400, FLG1_RECV | FLG1_PUSH));
    let snap = registry.snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].tracker, None, "firewalled はトラッカー空");
}

// ---------------------------------------------------------------------------
// 終了(QUIT / playing=false / TCP 異常切断)
// ---------------------------------------------------------------------------

#[test]
fn quit_ends_all_channels() {
    let (sec, _g) = temp_security();
    let registry = ChannelRegistry::new();
    let mut rx = registry.subscribe();
    let mut session = AnnounceSession::new(registry.clone(), sec, "127.0.0.1:50003".into());
    let sid = [0x44u8; 16];
    feed(&mut session, &helo(&sid));
    feed(&mut session, &bcst(&[0xC1u8; 16], &sid, "A", 500, FLG1_RECV));
    feed(&mut session, &bcst(&[0xC2u8; 16], &sid, "B", 500, FLG1_RECV));
    // 通知を読み飛ばす
    let _ = rx.try_recv();
    let _ = rx.try_recv();

    // QUIT → benign 切断
    let quit = Atom::parent("quit", vec![]);
    let disc = session
        .on_atom(quit.to_bytes().len(), roundtrip(&quit))
        .expect_err("QUIT は切断を返す");
    assert!(disc.category.is_none(), "QUIT はセキュリティイベントではない");

    // 呼び出し側(pump)の切断後処理に相当
    session.end_all();
    assert!(registry.snapshot().is_empty(), "全チャンネルが ended で除去される");

    // ended 通知が 2 件、最終状態を伴って届く
    let mut ended = 0;
    while let Ok(change) = rx.try_recv() {
        if let ChannelChange::Ended(ch) = change {
            assert!(!ch.channel_id.iter().all(|&b| b == 0));
            ended += 1;
        }
    }
    assert_eq!(ended, 2, "全チャンネルの ended 通知");
}

#[test]
fn tcp_abnormal_close_ends_immediately() {
    // PCP_QUIT なしの異常切断相当: pump が end_all を呼ぶ
    let (sec, _g) = temp_security();
    let registry = ChannelRegistry::new();
    let mut rx = registry.subscribe();
    let mut session = AnnounceSession::new(registry.clone(), sec, "127.0.0.1:50004".into());
    let sid = [0x55u8; 16];
    feed(&mut session, &helo(&sid));
    feed(&mut session, &bcst(&[0xD1u8; 16], &sid, "A", 500, FLG1_RECV));
    let _ = rx.try_recv();

    session.end_all();
    assert!(registry.snapshot().is_empty());
    assert!(matches!(rx.try_recv(), Ok(ChannelChange::Ended(_))));
}

#[test]
fn playing_false_ends_that_channel() {
    let (sec, _g) = temp_security();
    let registry = ChannelRegistry::new();
    let mut session = AnnounceSession::new(registry.clone(), sec, "127.0.0.1:50005".into());
    let sid = [0x66u8; 16];
    feed(&mut session, &helo(&sid));
    let a = [0xE1u8; 16];
    let b = [0xE2u8; 16];
    feed(&mut session, &bcst(&a, &sid, "A", 500, FLG1_RECV));
    feed(&mut session, &bcst(&b, &sid, "B", 500, FLG1_RECV));
    assert_eq!(registry.snapshot().len(), 2);

    // A について playing=false(RECV ビットなし)→ A のみ ended
    feed(&mut session, &bcst(&a, &sid, "A", 500, FLG1_DIRECT));
    let snap = registry.snapshot();
    assert_eq!(snap.len(), 1, "A のみ除去される");
    assert_eq!(snap[0].channel_id, b);
}

// ---------------------------------------------------------------------------
// ネガティブ: コーデックの上限
// ---------------------------------------------------------------------------

#[test]
fn nesting_deeper_than_8_is_rejected() {
    // 深さ 9 の入れ子を作る
    let mut atom = Atom::parent("l8", vec![Atom::data("dat", vec![1])]);
    for i in (0..8).rev() {
        atom = Atom::parent(&format!("l{i}"), vec![atom]);
    }
    let bytes = atom.to_bytes();
    match Atom::try_decode(&bytes) {
        Err(AtomError::NestTooDeep) => {}
        other => panic!("NestTooDeep を期待: {other:?}"),
    }
}

#[test]
fn payload_over_64kb_is_rejected() {
    // 長さ前置が 64KB を超える data atom(ペイロード確保前に拒否)
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"big\0");
    bytes.extend_from_slice(&((MAX_ATOM_PAYLOAD as u32) + 1).to_le_bytes());
    match Atom::try_decode(&bytes) {
        Err(AtomError::PayloadTooLarge) => {}
        other => panic!("PayloadTooLarge を期待: {other:?}"),
    }
}

#[test]
fn incomplete_buffer_needs_more_bytes() {
    let atom = helo(&[1u8; 16]);
    let bytes = atom.to_bytes();
    // 末尾を欠く → まだ完成していない(エラーではなく None)
    let partial = &bytes[..bytes.len() - 4];
    assert!(matches!(Atom::try_decode(partial), Ok(None)));
}

// ---------------------------------------------------------------------------
// ネガティブ: セッション層
// ---------------------------------------------------------------------------

#[test]
fn invalid_guid_length_is_rejected_with_pcp_reject() {
    let (sec, _g) = temp_security();
    let registry = ChannelRegistry::new();
    let mut session = AnnounceSession::new(registry, sec, "127.0.0.1:50006".into());

    // sid が 15 バイト(16 バイト固定に反する)
    let bad = Atom::parent("helo", vec![Atom::bytes("sid", &[0u8; 15])]);
    let disc = session
        .on_atom(bad.to_bytes().len(), roundtrip(&bad))
        .expect_err("不正 GUID は切断");
    assert_eq!(
        disc.category,
        Some(peca_p2p_yp::security::SecurityCategory::PcpReject)
    );
}

#[test]
fn unknown_atom_is_ignored_not_disconnected() {
    let (sec, _g) = temp_security();
    let registry = ChannelRegistry::new();
    let mut session = AnnounceSession::new(registry.clone(), sec, "127.0.0.1:50007".into());
    feed(&mut session, &helo(&[0x77u8; 16]));

    // 未知 atom → 無視(切断しない・登録変化なし)
    let unknown = Atom::parent("xxxx", vec![Atom::str("yyyy", "z")]);
    let actions = session
        .on_atom(unknown.to_bytes().len(), roundtrip(&unknown))
        .expect("未知 atom は切断されない");
    assert!(actions.is_empty());
    assert!(registry.snapshot().is_empty());
}

#[test]
fn seventeenth_channel_is_ignored_and_logged() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("security.log");
    let sec = std::sync::Arc::new(SecurityLog::new(&path).unwrap());
    let registry = ChannelRegistry::new();
    let mut session = AnnounceSession::new(registry.clone(), sec.clone(), "127.0.0.1:50008".into());
    let sid = [0x88u8; 16];
    feed(&mut session, &helo(&sid));

    // 16 チャンネルまで登録
    for i in 0..16u8 {
        let mut cid = [0u8; 16];
        cid[0] = i + 1;
        feed(&mut session, &bcst(&cid, &sid, "ch", 500, FLG1_RECV));
    }
    assert_eq!(registry.snapshot().len(), 16);

    // 17 個目 → 無視+pcp_reject(切断しない)
    let mut cid17 = [0u8; 16];
    cid17[0] = 200;
    let actions = feed(&mut session, &bcst(&cid17, &sid, "over", 500, FLG1_RECV));
    assert!(actions.is_empty());
    assert_eq!(registry.snapshot().len(), 16, "17 個目は無視される");

    sec.flush();
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains("pcp_reject"), "pcp_reject が記録される: {content}");
}

#[test]
fn non_loopback_source_is_rejected() {
    let public: SocketAddr = "198.51.100.9:7146".parse().unwrap();
    let loopback: SocketAddr = "127.0.0.1:7146".parse().unwrap();
    assert!(!source_is_loopback(&public));
    assert!(source_is_loopback(&loopback));
    let v6: SocketAddr = "[::1]:7146".parse().unwrap();
    assert!(source_is_loopback(&v6));
}

// ---------------------------------------------------------------------------
// レート制限
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// serve() 実 TCP 経路(HELO バイト往復・BCST 登録・TCP 切断で ended)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn serve_roundtrips_over_tcp_and_ends_on_disconnect() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::time::{Duration, sleep, timeout};

    let (sec, _g) = temp_security();
    let registry = ChannelRegistry::new();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::watch::channel(false);
    let server = tokio::spawn(peca_p2p_yp::pcp::session::serve(
        listener,
        registry.clone(),
        sec,
        rx,
    ));

    let mut client = TcpStream::connect(addr).await.unwrap();
    let sid = [0x5Au8; 16];
    client.write_all(&helo(&sid).to_bytes()).await.unwrap();
    client.flush().await.unwrap();

    // OLEH フレームを完成まで読む
    let mut acc = Vec::new();
    let mut buf = vec![0u8; 4096];
    let oleh = loop {
        let n = timeout(Duration::from_secs(5), client.read(&mut buf))
            .await
            .expect("読み取りタイムアウトなし")
            .expect("読み取り成功");
        assert!(n > 0, "OLEH が返る");
        acc.extend_from_slice(&buf[..n]);
        if let Some((atom, _)) = Atom::try_decode(&acc).unwrap() {
            break atom;
        }
    };
    assert!(oleh.id().matches("oleh"));
    assert_eq!(oleh.find("agnt").and_then(|a| a.as_str()), Some(agent_name()));

    // BCST 送信 → 登録
    client
        .write_all(&bcst(&[0x6Bu8; 16], &sid, "E2E", 700, FLG1_RECV | FLG1_DIRECT).to_bytes())
        .await
        .unwrap();
    client.flush().await.unwrap();
    let mut registered = false;
    for _ in 0..100 {
        if registry.len() == 1 {
            registered = true;
            break;
        }
        sleep(Duration::from_millis(20)).await;
    }
    assert!(registered, "BCST でチャンネルが登録される");

    // TCP 切断(PCP_QUIT なし)→ 即 ended
    drop(client);
    let mut ended = false;
    for _ in 0..100 {
        if registry.is_empty() {
            ended = true;
            break;
        }
        sleep(Duration::from_millis(20)).await;
    }
    assert!(ended, "TCP 異常切断で全チャンネルが ended");

    let _ = tx.send(true);
    let _ = server.await;
}

#[test]
fn session_rate_limit_disconnects_with_pcp_reject() {
    let (sec, _g) = temp_security();
    let registry = ChannelRegistry::new();
    let mut session = AnnounceSession::new(registry, sec, "127.0.0.1:50009".into())
        .with_clock(Box::new(|| 1000));
    feed(&mut session, &helo(&[0x99u8; 16]));

    // 同一秒窓で 64KB/秒 を超えるまで無害 atom を投入
    let filler = Atom::data("padd", vec![0u8; 8192]);
    let mut disconnected = None;
    for _ in 0..64 {
        match session.on_atom(filler.to_bytes().len(), roundtrip(&filler)) {
            Ok(_) => {}
            Err(d) => {
                disconnected = Some(d);
                break;
            }
        }
    }
    let d = disconnected.expect("64KB/秒 超過で切断");
    assert_eq!(
        d.category,
        Some(peca_p2p_yp::security::SecurityCategory::PcpReject)
    );
}

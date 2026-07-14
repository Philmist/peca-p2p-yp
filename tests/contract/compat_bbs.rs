//! T051 互換 API 契約テスト(contracts/compat-api.md)。
//!
//! 単体レベルのエスケープ規則・SJIS 変換・フォーム解析は `src/web/compat/*.rs` の
//! `#[cfg(test)]` が既に厚く覆っている。本ファイルは公開クレート API のみを使い、
//! 実際の axum ルーター(`peca_p2p_yp::web::compat::routes`)を通した**契約書レベルの
//! 振る舞い**を確認する:
//!
//! - 各エンドポイントの形式(subject.txt/dat/SETTING.TXT/head.txt/bbs.cgi)
//! - SJIS 変換・数値文字参照保全(受信/応答の両方向)
//! - 実体参照エスケープの一意規則(dat)
//! - dat 追記不変性(MUST)
//! - loopback 外 / Host 不正の定型拒否
//! - エラー定型(`<title>ERROR!</title>`)・内部情報非漏洩

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{HeaderValue, Method, Request, StatusCode, header};
use nostr::Keys;
use tower::ServiceExt;

use peca_p2p_yp::identity::Keystore;
use peca_p2p_yp::livechat::board::BoardKeyManager;
use peca_p2p_yp::livechat::manager::ParticipantManager;
use peca_p2p_yp::livechat::registry::LivechatRegistry;
use peca_p2p_yp::livechat::thread::BoardSettings;
use peca_p2p_yp::security::SecurityLog;
use peca_p2p_yp::store::Store;
use peca_p2p_yp::web::RateLimiter;
use peca_p2p_yp::web::compat::{CompatState, RATE_LIMIT_PER_SEC, routes, sjis};

const GUID: &str = "0123456789abcdef0123456789abcdef";
const GOOD_HOST: &str = "127.0.0.1:7183";

fn channel_of(board_id: &str) -> String {
    format!("30311:{board_id}:{GUID}")
}

fn test_state() -> CompatState {
    let registry = LivechatRegistry::new(128);
    let board_keys = Arc::new(BoardKeyManager::new(
        Arc::new(Store::open_in_memory().unwrap()),
        Keystore::ephemeral(),
    ));
    let dir = tempfile::tempdir().unwrap();
    let security = Arc::new(SecurityLog::new(dir.path().join("s.log")).unwrap());
    std::mem::forget(dir);
    let mut hosts = HashSet::new();
    hosts.insert(GOOD_HOST.to_string());
    hosts.insert("localhost:7183".to_string());
    let manager = ParticipantManager::new(Arc::clone(&board_keys), None);
    CompatState {
        registry,
        board_keys,
        manager,
        security,
        allowed_hosts: Arc::new(hosts),
        rate_limiter: Arc::new(RateLimiter::per_second(RATE_LIMIT_PER_SEC)),
    }
}

fn open_board(state: &CompatState, persona: &Keys, settings: BoardSettings) -> String {
    let board_id = persona.public_key().to_hex();
    state
        .registry
        .open_thread(
            persona.clone(),
            channel_of(&board_id),
            1,
            1_700_000_000,
            "実況スレ",
            settings,
            "198.51.100.1:7147",
        )
        .unwrap();
    board_id
}

fn get_req(uri: &str, host: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().method(Method::GET).uri(uri);
    if let Some(h) = host {
        b = b.header(header::HOST, h);
    }
    let mut req = b.body(Body::empty()).unwrap();
    let addr: SocketAddr = "127.0.0.1:60000".parse().unwrap();
    req.extensions_mut().insert(ConnectInfo(addr));
    req
}

fn post_req(uri: &str, host: Option<&str>, body: Vec<u8>) -> Request<Body> {
    let mut b = Request::builder().method(Method::POST).uri(uri);
    if let Some(h) = host {
        b = b.header(header::HOST, h);
    }
    b = b.header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    let mut req = b.body(Body::from(body)).unwrap();
    let addr: SocketAddr = "127.0.0.1:60001".parse().unwrap();
    req.extensions_mut().insert(ConnectInfo(addr));
    req
}

async fn body_bytes(resp: axum::response::Response) -> Vec<u8> {
    to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec()
}

async fn body_sjis_text(resp: axum::response::Response) -> String {
    sjis::decode(&body_bytes(resp).await)
}

// ---------------------------------------------------------------------------
// loopback 外 / Host 不正の定型拒否(FR-026)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn non_loopback_host_header_is_rejected_with_403() {
    let state = test_state();
    let app = routes(state);
    let resp = app
        .oneshot(get_req("/ab/subject.txt", Some("evil.example.com")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn missing_host_header_is_rejected_with_403() {
    let state = test_state();
    let app = routes(state);
    let resp = app.oneshot(get_req("/ab/subject.txt", None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn loopback_host_variants_are_accepted() {
    for host in ["127.0.0.1:7183", "localhost:7183"] {
        let state = test_state();
        let app = routes(state);
        let resp = app
            .oneshot(get_req("/unknown/subject.txt", Some(host)))
            .await
            .unwrap();
        // Host 検証は通過し、板が未知のため 404(403 ではないことを確認する)。
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "host={host}");
    }
}

// ---------------------------------------------------------------------------
// subject.txt(スレ一覧)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn subject_txt_format_is_key_dat_title_res_count() {
    let state = test_state();
    let persona = Keys::generate();
    let board_id = open_board(&state, &persona, BoardSettings::default());
    let app = routes(state);
    let resp = app
        .oneshot(get_req(
            &format!("/{board_id}/subject.txt"),
            Some(GOOD_HOST),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(content_type, "text/plain; charset=Shift_JIS");
    let text = body_sjis_text(resp).await;
    assert_eq!(text, "1700000000.dat<>実況スレ (0)\n");
}

#[tokio::test]
async fn subject_txt_unknown_board_is_404() {
    let state = test_state();
    let app = routes(state);
    let resp = app
        .oneshot(get_req("/ff/subject.txt", Some(GOOD_HOST)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// SETTING.TXT / head.txt(FR-027)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn setting_txt_reports_all_required_keys() {
    let state = test_state();
    let persona = Keys::generate();
    let board_id = open_board(
        &state,
        &persona,
        BoardSettings {
            title: "実況板".into(),
            res_limit: 500,
            noname_name: "名無しさん".into(),
            ..Default::default()
        },
    );
    let app = routes(state);
    let resp = app
        .oneshot(get_req(
            &format!("/{board_id}/SETTING.TXT"),
            Some(GOOD_HOST),
        ))
        .await
        .unwrap();
    let text = body_sjis_text(resp).await;
    for expected in [
        "BBS_TITLE=実況板",
        "BBS_NONAME_NAME=名無しさん",
        "BBS_LINE_NUMBER=32",
        "BBS_MESSAGE_COUNT=2048",
        "BBS_NAME_COUNT=64",
        "BBS_MAIL_COUNT=64",
        "BBS_MAX_RES=500",
    ] {
        assert!(text.contains(expected), "missing: {expected} in {text}");
    }
}

#[tokio::test]
async fn head_txt_returns_markdown_verbatim() {
    let state = test_state();
    let persona = Keys::generate();
    let board_id = open_board(
        &state,
        &persona,
        BoardSettings {
            local_rules: "# ルール\n**荒らし禁止**\n[link](http://example.com)".into(),
            ..Default::default()
        },
    );
    let app = routes(state);
    let resp = app
        .oneshot(get_req(&format!("/{board_id}/head.txt"), Some(GOOD_HOST)))
        .await
        .unwrap();
    let text = body_sjis_text(resp).await;
    assert_eq!(
        text, "# ルール\n**荒らし禁止**\n[link](http://example.com)",
        "Markdown は平文のまま(描画しない — research R7)"
    );
}

// ---------------------------------------------------------------------------
// dat(確定レス・エスケープ一意規則・追記不変性)
// ---------------------------------------------------------------------------

fn seed(state: &CompatState, board_id: &str, board_key: &Keys, body: &str, created_at: u64) {
    let ev = peca_p2p_yp::livechat::registry::sign_res(
        board_key,
        board_id,
        &channel_of(board_id),
        1,
        body,
        created_at,
    )
    .unwrap();
    state
        .registry
        .seed_confirmed_res(board_id, &ev, created_at)
        .unwrap();
}

#[tokio::test]
async fn dat_line_format_matches_contract() {
    let state = test_state();
    let persona = Keys::generate();
    let board_id = open_board(&state, &persona, BoardSettings::default());
    let board_key = Keys::generate();
    seed(&state, &board_id, &board_key, "テスト本文", 1_700_000_001);
    let app = routes(state);
    let resp = app
        .oneshot(get_req(
            &format!("/{board_id}/dat/1700000000.dat"),
            Some(GOOD_HOST),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let text = body_sjis_text(resp).await;
    let line = text.lines().next().unwrap();
    let fields: Vec<&str> = line.split("<>").collect();
    assert_eq!(
        fields.len(),
        5,
        "名前<>メール<>日付ID<>本文<>タイトル: {line}"
    );
    assert!(fields[2].contains("ID:"), "日付欄に ID 表示: {}", fields[2]);
    assert_eq!(fields[3], "テスト本文");
    assert_eq!(fields[4], "実況スレ");
}

#[tokio::test]
async fn dat_escapes_ampersand_lt_gt_quote_in_order() {
    let state = test_state();
    let persona = Keys::generate();
    let board_id = open_board(&state, &persona, BoardSettings::default());
    let board_key = Keys::generate();
    seed(&state, &board_id, &board_key, r#"A&B<C>D"E"#, 1_700_000_001);
    let app = routes(state);
    let resp = app
        .oneshot(get_req(
            &format!("/{board_id}/dat/1700000000.dat"),
            Some(GOOD_HOST),
        ))
        .await
        .unwrap();
    let text = body_sjis_text(resp).await;
    assert!(
        text.contains("A&amp;B&lt;C&gt;D&quot;E"),
        "一意規則(& < > \" の順)でエスケープされる: {text}"
    );
}

#[tokio::test]
async fn dat_anchor_becomes_gt_gt_n() {
    let state = test_state();
    let persona = Keys::generate();
    let board_id = open_board(&state, &persona, BoardSettings::default());
    let board_key = Keys::generate();
    seed(&state, &board_id, &board_key, ">>1 に返信", 1_700_000_001);
    let app = routes(state);
    let resp = app
        .oneshot(get_req(
            &format!("/{board_id}/dat/1700000000.dat"),
            Some(GOOD_HOST),
        ))
        .await
        .unwrap();
    let text = body_sjis_text(resp).await;
    assert!(text.contains("&gt;&gt;1 に返信"));
}

#[tokio::test]
async fn dat_newline_becomes_br() {
    let state = test_state();
    let persona = Keys::generate();
    let board_id = open_board(&state, &persona, BoardSettings::default());
    let board_key = Keys::generate();
    seed(&state, &board_id, &board_key, "行1\n行2", 1_700_000_001);
    let app = routes(state);
    let resp = app
        .oneshot(get_req(
            &format!("/{board_id}/dat/1700000000.dat"),
            Some(GOOD_HOST),
        ))
        .await
        .unwrap();
    let text = body_sjis_text(resp).await;
    assert!(text.contains("行1<br>行2"));
}

#[tokio::test]
async fn dat_unconvertible_char_uses_numeric_char_ref() {
    let state = test_state();
    let persona = Keys::generate();
    let board_id = open_board(&state, &persona, BoardSettings::default());
    let board_key = Keys::generate();
    seed(&state, &board_id, &board_key, "a🚀b", 1_700_000_001);
    let app = routes(state);
    let resp = app
        .oneshot(get_req(
            &format!("/{board_id}/dat/1700000000.dat"),
            Some(GOOD_HOST),
        ))
        .await
        .unwrap();
    let text = body_sjis_text(resp).await;
    assert!(
        text.contains("&#128640;"),
        "SJIS 変換不能文字は数値文字参照で保全: {text}"
    );
    // 数値文字参照(&#128640;)自体は <> を含まないため、dat のフィールド区切り(<>)
    // 数と衝突しない(1 レス行は必ず 4 個の <> 区切りを持つ)。
    let line = text.lines().next().unwrap();
    assert_eq!(
        line.matches("<>").count(),
        4,
        "数値文字参照混入後もフィールド区切りは 4 個のまま: {line}"
    );
}

#[tokio::test]
async fn dat_unknown_key_is_404() {
    let state = test_state();
    let persona = Keys::generate();
    let board_id = open_board(&state, &persona, BoardSettings::default());
    let app = routes(state);
    let resp = app
        .oneshot(get_req(
            &format!("/{board_id}/dat/9999999999.dat"),
            Some(GOOD_HOST),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn dat_closed_thread_is_404_dat_ochi() {
    // 専ブラは非 200 を「dat 落ち」として扱う想定(クローズで削除済みの dat)。
    let state = test_state();
    let persona = Keys::generate();
    let board_id = open_board(&state, &persona, BoardSettings::default());
    state
        .registry
        .close_thread(&board_id, 1_700_000_500)
        .unwrap();
    let app = routes(state);
    let resp = app
        .oneshot(get_req(
            &format!("/{board_id}/dat/1700000000.dat"),
            Some(GOOD_HOST),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// dat 追記不変性(MUST): 一度応答したバイト列は以後の応答で接頭辞として不変。
#[tokio::test]
async fn dat_append_invariance_is_prefix_stable() {
    let state = test_state();
    let persona = Keys::generate();
    let board_id = open_board(&state, &persona, BoardSettings::default());
    let board_key = Keys::generate();
    seed(&state, &board_id, &board_key, "一つ目", 1_700_000_001);

    let app1 = routes(state.clone());
    let resp1 = app1
        .oneshot(get_req(
            &format!("/{board_id}/dat/1700000000.dat"),
            Some(GOOD_HOST),
        ))
        .await
        .unwrap();
    let before = body_bytes(resp1).await;

    seed(&state, &board_id, &board_key, "二つ目", 1_700_000_002);
    seed(&state, &board_id, &board_key, "三つ目", 1_700_000_003);

    let app2 = routes(state);
    let resp2 = app2
        .oneshot(get_req(
            &format!("/{board_id}/dat/1700000000.dat"),
            Some(GOOD_HOST),
        ))
        .await
        .unwrap();
    let after = body_bytes(resp2).await;

    assert!(
        after.starts_with(&before),
        "追記後も既存部分はバイト列として不変でなければならない(MUST)"
    );
    assert!(after.len() > before.len(), "追記により長さは増える");
}

/// 凍結(次スレ移行)後も取得済み範囲の dat は不変であることを確認する。
#[tokio::test]
async fn dat_append_invariance_holds_across_thread_migration() {
    let state = test_state();
    let persona = Keys::generate();
    let board_id = open_board(&state, &persona, BoardSettings::default());
    let board_key = Keys::generate();
    seed(&state, &board_id, &board_key, "移行前のレス", 1_700_000_001);

    let app1 = routes(state.clone());
    let resp1 = app1
        .oneshot(get_req(
            &format!("/{board_id}/dat/1700000000.dat"),
            Some(GOOD_HOST),
        ))
        .await
        .unwrap();
    let before = body_bytes(resp1).await;

    state
        .registry
        .start_next_generation(&board_id, 1_700_001_000, "次スレ")
        .unwrap();

    let app2 = routes(state);
    let resp2 = app2
        .oneshot(get_req(
            &format!("/{board_id}/dat/1700000000.dat"),
            Some(GOOD_HOST),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp2.status(),
        StatusCode::OK,
        "次スレ移行後も凍結スレの dat は取得できる(直近 1 世代)"
    );
    let after = body_bytes(resp2).await;
    assert_eq!(before, after, "次スレ移行後も取得済み範囲は不変");
}

/// Last-Modified / If-Modified-Since(304)。
#[tokio::test]
async fn dat_conditional_get_returns_304_when_not_modified() {
    let state = test_state();
    let persona = Keys::generate();
    let board_id = open_board(&state, &persona, BoardSettings::default());
    let board_key = Keys::generate();
    seed(&state, &board_id, &board_key, "本文", 1_700_000_001);
    let app = routes(state);
    let mut req = get_req(&format!("/{board_id}/dat/1700000000.dat"), Some(GOOD_HOST));
    req.headers_mut().insert(
        header::IF_MODIFIED_SINCE,
        HeaderValue::from_str(&peca_p2p_yp::web::compat::dat::format_http_date(
            1_700_000_001,
        ))
        .unwrap(),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
}

/// Range(206)。
#[tokio::test]
async fn dat_range_returns_206_partial_content() {
    let state = test_state();
    let persona = Keys::generate();
    let board_id = open_board(&state, &persona, BoardSettings::default());
    let board_key = Keys::generate();
    seed(&state, &board_id, &board_key, "本文", 1_700_000_001);
    let app = routes(state);
    let mut req = get_req(&format!("/{board_id}/dat/1700000000.dat"), Some(GOOD_HOST));
    req.headers_mut()
        .insert(header::RANGE, HeaderValue::from_static("bytes=0-5"));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
}

/// 充足不能な Range(416)。
#[tokio::test]
async fn dat_range_unsatisfiable_returns_416() {
    let state = test_state();
    let persona = Keys::generate();
    let board_id = open_board(&state, &persona, BoardSettings::default());
    let board_key = Keys::generate();
    seed(&state, &board_id, &board_key, "本文", 1_700_000_001);
    let app = routes(state);
    let mut req = get_req(&format!("/{board_id}/dat/1700000000.dat"), Some(GOOD_HOST));
    req.headers_mut().insert(
        header::RANGE,
        HeaderValue::from_static("bytes=99999-100000"),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
}

/// gzip 等の内容符号化を行わない(identity 固定)。
#[tokio::test]
async fn dat_does_not_use_content_encoding_gzip() {
    let state = test_state();
    let persona = Keys::generate();
    let board_id = open_board(&state, &persona, BoardSettings::default());
    let board_key = Keys::generate();
    seed(&state, &board_id, &board_key, "本文", 1_700_000_001);
    let app = routes(state);
    let mut req = get_req(&format!("/{board_id}/dat/1700000000.dat"), Some(GOOD_HOST));
    req.headers_mut()
        .insert(header::ACCEPT_ENCODING, HeaderValue::from_static("gzip"));
    let resp = app.oneshot(req).await.unwrap();
    let encoding = resp
        .headers()
        .get(header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok());
    assert_ne!(
        encoding,
        Some("gzip"),
        "Accept-Encoding を無視し identity で返す"
    );
}

// ---------------------------------------------------------------------------
// bbs.cgi(FR-028〜FR-030)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bbs_cgi_success_writes_and_reflects_in_dat() {
    let state = test_state();
    let persona = Keys::generate();
    let board_id = open_board(
        &state,
        &persona,
        BoardSettings {
            first_post_pow_bits: 0,
            ..Default::default()
        },
    );
    let body = format!("bbs={board_id}&key=1700000000&FROM=&mail=&MESSAGE=hello").into_bytes();
    let app = routes(state.clone());
    let resp = app
        .oneshot(post_req("/test/bbs.cgi", Some(GOOD_HOST), body))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let text = body_sjis_text(resp).await;
    assert!(text.contains("<title>書きこみました。</title>"));

    // dat 再取得で反映を確認する(US6 シナリオ 3 — 採番確定は非同期のため成功応答は
    // 「ホストへの送信受理」を意味し、反映は dat 再取得で確認される)。
    let app2 = routes(state);
    let resp2 = app2
        .oneshot(get_req(
            &format!("/{board_id}/dat/1700000000.dat"),
            Some(GOOD_HOST),
        ))
        .await
        .unwrap();
    let dat_text = body_sjis_text(resp2).await;
    assert!(dat_text.contains("hello"));
}

#[tokio::test]
async fn bbs_cgi_subject_thread_creation_is_rejected_with_typical_error() {
    let state = test_state();
    let persona = Keys::generate();
    let board_id = open_board(&state, &persona, BoardSettings::default());
    let body = format!("bbs={board_id}&MESSAGE=test&subject=new+thread").into_bytes();
    let app = routes(state);
    let resp = app
        .oneshot(post_req("/test/bbs.cgi", Some(GOOD_HOST), body))
        .await
        .unwrap();
    let text = body_sjis_text(resp).await;
    assert!(text.contains("<title>ERROR!</title>"));
    assert!(text.contains("ERROR:"));
}

#[tokio::test]
async fn bbs_cgi_expands_numeric_char_refs_in_message() {
    let state = test_state();
    let persona = Keys::generate();
    let board_id = open_board(
        &state,
        &persona,
        BoardSettings {
            first_post_pow_bits: 0,
            ..Default::default()
        },
    );
    // "a&#128640;b" を percent-encode したフォーム値を送る。
    let raw_message = "a&#128640;b";
    let percent_encoded: String = raw_message
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    let body = format!("bbs={board_id}&key=1700000000&MESSAGE={percent_encoded}").into_bytes();
    let app = routes(state.clone());
    app.oneshot(post_req("/test/bbs.cgi", Some(GOOD_HOST), body))
        .await
        .unwrap();

    let app2 = routes(state);
    let resp2 = app2
        .oneshot(get_req(
            &format!("/{board_id}/dat/1700000000.dat"),
            Some(GOOD_HOST),
        ))
        .await
        .unwrap();
    let dat_text = body_sjis_text(resp2).await;
    // 数値文字参照は展開後 SJIS 変換不能のため、再度数値文字参照として出力される
    // (受理時展開 → 保存 → 出力時に SJIS 不能文字は再び数値文字参照化 — 往復の契約)。
    assert!(dat_text.contains("&#128640;"), "dat: {dat_text}");
}

#[tokio::test]
async fn bbs_cgi_never_returns_confirmation_page_or_cookie() {
    let state = test_state();
    let persona = Keys::generate();
    let board_id = open_board(
        &state,
        &persona,
        BoardSettings {
            first_post_pow_bits: 0,
            ..Default::default()
        },
    );
    let body = format!("bbs={board_id}&key=1700000000&MESSAGE=test").into_bytes();
    let app = routes(state);
    let resp = app
        .oneshot(post_req("/test/bbs.cgi", Some(GOOD_HOST), body))
        .await
        .unwrap();
    assert!(
        resp.headers().get(header::SET_COOKIE).is_none(),
        "Cookie は発行しない(FR-026)"
    );
}

#[tokio::test]
async fn bbs_cgi_error_never_leaks_internal_details() {
    let state = test_state();
    let body = b"bbs=nonexistent&MESSAGE=test".to_vec();
    let app = routes(state);
    let resp = app
        .oneshot(post_req("/test/bbs.cgi", Some(GOOD_HOST), body))
        .await
        .unwrap();
    let text = body_sjis_text(resp).await;
    assert!(text.contains("<title>ERROR!</title>"));
    assert!(!text.contains("panic"));
    assert!(!text.to_lowercase().contains("unwrap"));
    assert!(!text.contains(".rs:"));
}

// ---------------------------------------------------------------------------
// 未定義パスの定型 404
// ---------------------------------------------------------------------------

#[tokio::test]
async fn undefined_path_returns_typical_404() {
    let state = test_state();
    let app = routes(state);
    let resp = app
        .oneshot(get_req("/some/random/path", Some(GOOD_HOST)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// レビュー対応の回帰テスト(HTTP 層 — 名無し名確定固定 / Last-Modified 単調性)
// ---------------------------------------------------------------------------

/// 指摘 1: レス確定 → noname_name 変更 → 追加レス確定 → dat 再取得で、
/// 既存行が接頭辞としてバイト不変であることを確認する(dat 追記不変性 MUST)。
#[tokio::test]
async fn dat_append_invariance_holds_across_noname_name_change() {
    let state = test_state();
    let persona = Keys::generate();
    let board_id = open_board(
        &state,
        &persona,
        BoardSettings {
            noname_name: "名無しさん".into(),
            ..Default::default()
        },
    );
    let board_key = Keys::generate();
    seed(&state, &board_id, &board_key, "1 件目", 1_700_000_001);

    let app1 = routes(state.clone());
    let resp1 = app1
        .oneshot(get_req(
            &format!("/{board_id}/dat/1700000000.dat"),
            Some(GOOD_HOST),
        ))
        .await
        .unwrap();
    let before = body_bytes(resp1).await;
    let before_text = sjis::decode(&before);
    assert!(
        before_text.contains("名無しさん<>"),
        "1 件目は変更前の noname_name で確定: {before_text}"
    );

    // 板主が noname_name を変更する。
    state
        .registry
        .update_settings(
            &board_id,
            BoardSettings {
                noname_name: "変更後の名無し".into(),
                ..Default::default()
            },
        )
        .unwrap();

    seed(&state, &board_id, &board_key, "2 件目", 1_700_000_002);

    let app2 = routes(state);
    let resp2 = app2
        .oneshot(get_req(
            &format!("/{board_id}/dat/1700000000.dat"),
            Some(GOOD_HOST),
        ))
        .await
        .unwrap();
    let after = body_bytes(resp2).await;
    let after_text = sjis::decode(&after);

    assert!(
        after.starts_with(&before),
        "既存行(1 件目)は noname_name 変更後もバイト列として不変でなければならない(MUST): \
         before={before_text:?} after={after_text:?}"
    );
    assert!(
        after_text.contains("変更後の名無し<>"),
        "2 件目は変更後の noname_name で確定: {after_text}"
    );
    assert!(
        !after_text
            .lines()
            .next()
            .unwrap()
            .contains("変更後の名無し"),
        "1 件目の行は変更後の名無し名を含まない(遡及しない): {after_text}"
    );
}

/// 指摘 2: 過去の created_at を持つレスが確定した後も、dat の Last-Modified は
/// 後退しないことを確認する(キャッシュ汚染攻撃の防止)。
#[tokio::test]
async fn dat_last_modified_does_not_regress_with_backdated_created_at() {
    let state = test_state();
    let persona = Keys::generate();
    let board_id = open_board(&state, &persona, BoardSettings::default());
    let board_key = Keys::generate();

    // 1 件目を確定させ、Last-Modified を観測する。
    seed(&state, &board_id, &board_key, "1 件目", 1_700_000_001);
    let app1 = routes(state.clone());
    let resp1 = app1
        .oneshot(get_req(
            &format!("/{board_id}/dat/1700000000.dat"),
            Some(GOOD_HOST),
        ))
        .await
        .unwrap();
    let last_modified_1 = resp1
        .headers()
        .get(header::LAST_MODIFIED)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let unix_1 = peca_p2p_yp::web::compat::dat::parse_http_date(&last_modified_1).unwrap();

    // 2 件目は「過去日時」を申告する書き込みを模す(投稿者申告 created_at は未検証)。
    seed(
        &state,
        &board_id,
        &board_key,
        "2 件目(過去申告)",
        1_000_000_000,
    );

    let app2 = routes(state);
    let resp2 = app2
        .oneshot(get_req(
            &format!("/{board_id}/dat/1700000000.dat"),
            Some(GOOD_HOST),
        ))
        .await
        .unwrap();
    let last_modified_2 = resp2
        .headers()
        .get(header::LAST_MODIFIED)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let unix_2 = peca_p2p_yp::web::compat::dat::parse_http_date(&last_modified_2).unwrap();

    assert!(
        unix_2 >= unix_1,
        "過去日時を申告するレスが確定しても Last-Modified は後退しない \
         (キャッシュ汚染攻撃の防止): before={unix_1} after={unix_2}"
    );
}

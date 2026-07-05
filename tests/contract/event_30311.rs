//! 30311 発行契約テスト(T024)— pcp エージェントが実装する。
//! contracts/nostr-events.md: AnnouncedChannel→30311 タグ写像ゴールデン+発行規則。
//!
//! 検査対象:
//! - 必須タグ(d/title/status/starts/expiration)と peca 拡張タグ(bitrate/type/tip/contact/relays/track)
//! - expiration = created_at + 600
//! - status=ended の写像
//! - firewalled(tracker なし)時は tip / streaming を省略
//! - listeners/relays 負値(-1)はタグ省略
//! - content は空文字列
//! - 他ペルソナ情報の不混入(直列化イベントに署名鍵以外のペルソナ情報が含まれない — FR-013)

use nostr::{Event, JsonUtil, Keys};

use peca_p2p_yp::pcp::channel::{AnnouncedChannel, ChannelState, TrackInfo};

const CREATED_AT: u64 = 1_700_000_000;

fn sample() -> AnnouncedChannel {
    AnnouncedChannel {
        channel_id: [0xABu8; 16],
        name: "テスト配信".into(),
        genre: "game".into(),
        description: "説明文".into(),
        contact_url: "https://example.com/".into(),
        bitrate_kbps: 1500,
        content_type: "FLV".into(),
        track: TrackInfo {
            title: "song".into(),
            creator: "artist".into(),
            album: "album".into(),
        },
        tracker: Some("198.51.100.1:7144".into()),
        listeners: 5,
        relays_cnt: 3,
        started_at: CREATED_AT,
        state: ChannelState::Announced,
    }
}

/// 単純タグ `[name, value]` の値。
fn tag_value<'a>(event: &'a Event, name: &str) -> Option<&'a str> {
    event.tags.iter().find_map(|tag| {
        let s = tag.as_slice();
        if s.first().map(String::as_str) == Some(name) {
            s.get(1).map(String::as_str)
        } else {
            None
        }
    })
}

/// peca 拡張タグ `["peca", sub, value, ..]` の第 1 値。
fn peca_value<'a>(event: &'a Event, sub: &str) -> Option<&'a str> {
    event.tags.iter().find_map(|tag| {
        let s = tag.as_slice();
        if s.first().map(String::as_str) == Some("peca")
            && s.get(1).map(String::as_str) == Some(sub)
        {
            s.get(2).map(String::as_str)
        } else {
            None
        }
    })
}

fn sign(ch: &AnnouncedChannel, keys: &Keys) -> Event {
    ch.to_listing().sign(keys, CREATED_AT, 0).expect("署名成功")
}

#[test]
fn maps_required_and_extension_tags() {
    let keys = Keys::generate();
    let event = sign(&sample(), &keys);

    // 必須タグ
    assert_eq!(
        tag_value(&event, "d"),
        Some("abababababababababababababababab")
    );
    assert_eq!(tag_value(&event, "title"), Some("テスト配信"));
    assert_eq!(tag_value(&event, "status"), Some("live"));
    assert_eq!(tag_value(&event, "starts"), Some("1700000000"));

    // peca 拡張タグ
    assert_eq!(peca_value(&event, "bitrate"), Some("1500"));
    assert_eq!(peca_value(&event, "type"), Some("FLV"));
    assert_eq!(peca_value(&event, "tip"), Some("198.51.100.1:7144"));
    assert_eq!(peca_value(&event, "contact"), Some("https://example.com/"));
    assert_eq!(peca_value(&event, "relays"), Some("3"));

    // current_participants(listeners)
    assert_eq!(tag_value(&event, "current_participants"), Some("5"));

    // streaming = pcp://<tracker>/<channel_id>
    assert_eq!(
        tag_value(&event, "streaming"),
        Some("pcp://198.51.100.1:7144/abababababababababababababababab")
    );

    // track タグ(空 url 要素を含む 6 要素)
    let track = event
        .tags
        .iter()
        .map(|t| t.as_slice())
        .find(|s| {
            s.first().map(String::as_str) == Some("peca")
                && s.get(1).map(String::as_str) == Some("track")
        })
        .expect("track タグ");
    assert_eq!(track[2].as_str(), "song");
    assert_eq!(track[3].as_str(), "artist");
    assert_eq!(track[4].as_str(), "album");
    assert_eq!(track[5].as_str(), "", "track url 要素は v1 では常に空");
}

#[test]
fn expiration_is_created_at_plus_600() {
    let keys = Keys::generate();
    let event = sign(&sample(), &keys);
    assert_eq!(
        tag_value(&event, "expiration"),
        Some((CREATED_AT + 600).to_string().as_str())
    );
}

#[test]
fn ended_state_maps_to_ended_status() {
    let keys = Keys::generate();
    let mut ch = sample();
    ch.state = ChannelState::Ended;
    let event = sign(&ch, &keys);
    assert_eq!(tag_value(&event, "status"), Some("ended"));
}

#[test]
fn firewalled_omits_tip_and_streaming() {
    let keys = Keys::generate();
    let mut ch = sample();
    ch.tracker = None;
    let event = sign(&ch, &keys);
    assert_eq!(peca_value(&event, "tip"), None, "tip は省略");
    assert_eq!(tag_value(&event, "streaming"), None, "streaming は省略");
}

#[test]
fn negative_counts_omit_tags() {
    let keys = Keys::generate();
    let mut ch = sample();
    ch.listeners = -1;
    ch.relays_cnt = -1;
    let event = sign(&ch, &keys);
    assert_eq!(tag_value(&event, "current_participants"), None);
    assert_eq!(peca_value(&event, "relays"), None);
}

#[test]
fn content_is_empty() {
    let keys = Keys::generate();
    let event = sign(&sample(), &keys);
    assert_eq!(event.content, "", "全情報はタグで表現し content は空");
}

#[test]
fn no_persona_info_leaks_beyond_signing_key() {
    // ペルソナのローカル表示名などがイベントに混入しないこと(FR-013)。
    // AnnouncedChannel はそもそも persona_id/label を持たないため、直列化イベントには
    // 署名鍵(pubkey/sig)以外のペルソナ情報が現れない。
    let keys = Keys::generate();
    let event = sign(&sample(), &keys);
    let json = event.as_json();

    // 署名鍵は当然含まれる
    assert!(json.contains(&keys.public_key().to_hex()));

    // 想定外のペルソナ識別子が現れないこと(タグ名は既知のもののみ)
    let allowed_first: &[&str] = &[
        "d",
        "title",
        "summary",
        "t",
        "status",
        "starts",
        "current_participants",
        "streaming",
        "expiration",
        "peca",
    ];
    for tag in event.tags.iter() {
        let first = tag.as_slice().first().map(String::as_str).unwrap_or("");
        assert!(
            allowed_first.contains(&first),
            "未知のタグ名(ペルソナ情報の混入疑い): {first}"
        );
    }
    // ラベル的な文字列が紛れ込まないこと(サンプルにローカル label は与えていない)
    assert!(!json.to_lowercase().contains("\"label\""));
}

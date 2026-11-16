use super::*;
use crate::sdp::{Direction, MediaProto, SessionDescription};

const IP: &str = "192.0.2.1";
const PORT: u16 = 16384;

fn session(caps: LocalCaps) -> MediaSession {
    MediaSession::new(caps, IP.parse().unwrap(), PORT)
}

fn caps_with(codecs: &[AudioCodec], srtp: SrtpPolicy) -> LocalCaps {
    LocalCaps {
        codecs: codecs.to_vec(),
        srtp,
        ..Default::default()
    }
}

fn parse(s: &str) -> SessionDescription {
    SessionDescription::parse(s).unwrap_or_else(|e| panic!("{e}: {s}"))
}

fn key(suite: SrtpSuite) -> String {
    SrtpKeyMaterial::generate(suite).to_base64()
}

fn lines(sdp: &str) -> Vec<&str> {
    sdp.split("\r\n").filter(|l| !l.is_empty()).collect()
}

fn has(sdp: &str, line: &str) -> bool {
    lines(sdp).contains(&line)
}

fn body_without_o(sdp: &str) -> String {
    lines(sdp)
        .into_iter()
        .filter(|l| !l.starts_with("o="))
        .collect::<Vec<_>>()
        .join("\r\n")
}

fn version_of(sdp: &str) -> u64 {
    parse(sdp).origin.unwrap().sess_version.parse().unwrap()
}

// ── fixtures ────────────────────────────────────────────────────────────

/// FreeSWITCH 1.10-style offer without SRTP.
fn fs_offer_avp() -> String {
    "v=0\r\n\
o=FreeSWITCH 1759900000 1759900001 IN IP4 203.0.113.10\r\n\
s=FreeSWITCH\r\n\
c=IN IP4 203.0.113.10\r\n\
t=0 0\r\n\
m=audio 24002 RTP/AVP 111 9 0 8 102 101\r\n\
a=rtpmap:111 opus/48000/2\r\n\
a=fmtp:111 useinbandfec=1; maxaveragebitrate=30000; maxplaybackrate=16000; ptime=20; minptime=10; maxptime=40; stereo=0\r\n\
a=rtpmap:9 G722/8000\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:8 PCMA/8000\r\n\
a=rtpmap:102 telephone-event/48000\r\n\
a=rtpmap:101 telephone-event/8000\r\n\
a=fmtp:101 0-16\r\n\
a=ptime:20\r\n\
a=sendrecv\r\n"
        .into()
}

/// FreeSWITCH SAVP offer with the four suites, FS `AES_CM_256_*` spelling,
/// rtcp-mux absent, ptime:20.
fn fs_offer_savp() -> String {
    format!(
        "v=0\r\n\
o=FreeSWITCH 1759900000 1759900002 IN IP4 203.0.113.10\r\n\
s=FreeSWITCH\r\n\
c=IN IP4 203.0.113.10\r\n\
t=0 0\r\n\
m=audio 24004 RTP/SAVP 111 9 0 8 102 101\r\n\
a=rtpmap:111 opus/48000/2\r\n\
a=fmtp:111 useinbandfec=1; stereo=0\r\n\
a=rtpmap:9 G722/8000\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:8 PCMA/8000\r\n\
a=rtpmap:102 telephone-event/48000\r\n\
a=rtpmap:101 telephone-event/8000\r\n\
a=fmtp:101 0-16\r\n\
a=ptime:20\r\n\
a=sendrecv\r\n\
a=crypto:1 AES_CM_256_HMAC_SHA1_80 inline:{k1}\r\n\
a=crypto:2 AES_CM_256_HMAC_SHA1_32 inline:{k2}\r\n\
a=crypto:3 AES_CM_128_HMAC_SHA1_80 inline:{k3}\r\n\
a=crypto:4 AES_CM_128_HMAC_SHA1_32 inline:{k4}\r\n",
        k1 = key(SrtpSuite::Aes256CmHmacSha1_80),
        k2 = key(SrtpSuite::Aes256CmHmacSha1_32),
        k3 = key(SrtpSuite::AesCm128HmacSha1_80),
        k4 = key(SrtpSuite::AesCm128HmacSha1_32),
    )
}

/// Yealink-like: G.711 + G.722 + G.729, te/8000 only, no rtpmap for static PTs
/// other than G.729.
fn yealink_offer() -> String {
    "v=0\r\n\
o=- 20008 20008 IN IP4 10.1.1.20\r\n\
s=SDP data\r\n\
c=IN IP4 10.1.1.20\r\n\
t=0 0\r\n\
m=audio 11780 RTP/AVP 0 8 9 18 101\r\n\
a=rtpmap:18 G729/8000\r\n\
a=fmtp:18 annexb=no\r\n\
a=rtpmap:101 telephone-event/8000\r\n\
a=fmtp:101 0-15\r\n\
a=ptime:30\r\n\
a=sendrecv\r\n"
        .into()
}

/// Polycom-like SAVP offer (32-bit tag preferred, lifetime + session params).
fn polycom_offer() -> String {
    format!(
        "v=0\r\n\
o=- 1 1 IN IP4 10.2.2.30\r\n\
s=-\r\n\
c=IN IP4 10.2.2.30\r\n\
t=0 0\r\n\
m=audio 2224 RTP/SAVP 9 0 8 13 101\r\n\
a=rtpmap:9 G722/8000\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:8 PCMA/8000\r\n\
a=rtpmap:13 CN/8000\r\n\
a=rtpmap:101 telephone-event/8000\r\n\
a=fmtp:101 0-15\r\n\
a=sendrecv\r\n\
a=crypto:1 AES_CM_128_HMAC_SHA1_32 inline:{k1}|2^31 UNENCRYPTED_SRTCP\r\n\
a=crypto:2 AES_CM_128_HMAC_SHA1_80 inline:{k2}|2^31\r\n",
        k1 = key(SrtpSuite::AesCm128HmacSha1_32),
        k2 = key(SrtpSuite::AesCm128HmacSha1_80),
    )
}

/// Minimal G.711 offer builder: proto, extra media attrs, extra `m=audio` tail.
fn simple_offer(proto: &str, attrs: &[String]) -> String {
    let mut s = format!(
        "v=0\r\no=- 5 5 IN IP4 198.51.100.7\r\ns=-\r\nc=IN IP4 198.51.100.7\r\nt=0 0\r\nm=audio 4000 {proto} 0 8 101\r\na=rtpmap:101 telephone-event/8000\r\n"
    );
    for a in attrs {
        s.push_str(a);
        s.push_str("\r\n");
    }
    s
}

fn crypto_attr(tag: u32, suite: &str, suite_for_key: SrtpSuite) -> String {
    format!("a=crypto:{tag} {suite} inline:{}", key(suite_for_key))
}

fn offer_variant(kind: &str) -> String {
    match kind {
        "avp" => simple_offer("RTP/AVP", &[]),
        "avp+crypto" => simple_offer(
            "RTP/AVP",
            &[crypto_attr(1, "AES_CM_128_HMAC_SHA1_80", SrtpSuite::AesCm128HmacSha1_80)],
        ),
        "savp" => simple_offer(
            "RTP/SAVP",
            &[
                crypto_attr(1, "AES_CM_128_HMAC_SHA1_80", SrtpSuite::AesCm128HmacSha1_80),
                crypto_attr(2, "AES_CM_128_HMAC_SHA1_32", SrtpSuite::AesCm128HmacSha1_32),
            ],
        ),
        "savp-unsupported" => simple_offer(
            "RTP/SAVP",
            &["a=crypto:1 AES_CM_128_F8_HMAC_SHA1_80 inline:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_string()],
        ),
        "savp-no-crypto" => simple_offer("RTP/SAVP", &[]),
        "savp-second-supported" => simple_offer(
            "RTP/SAVP",
            &[
                "a=crypto:7 AES_CM_128_F8_HMAC_SHA1_80 inline:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_string(),
                crypto_attr(9, "AES_256_CM_HMAC_SHA1_80", SrtpSuite::Aes256CmHmacSha1_80),
            ],
        ),
        _ => panic!("unknown variant"),
    }
}

// ── offer generation ────────────────────────────────────────────────────

#[test]
fn offer_default_structure() {
    let mut s = session(LocalCaps::default());
    let sdp = s.create_offer();
    assert!(sdp.ends_with("\r\n"));
    let l = lines(&sdp);
    assert_eq!(l[0], "v=0");
    assert!(
        l[1].starts_with(&format!("o=- {} 1 IN IP4 {IP}", s.sess_id())),
        "{}",
        l[1]
    );
    assert_eq!(l[2], "s=-");
    assert_eq!(l[3], format!("c=IN IP4 {IP}"));
    assert_eq!(l[4], "t=0 0");
    assert_eq!(l[5], format!("m=audio {PORT} RTP/AVP 111 9 0 8 101 102"));
    assert!(has(&sdp, "a=rtpmap:111 opus/48000/2"));
    assert!(has(
        &sdp,
        "a=fmtp:111 useinbandfec=1;maxplaybackrate=48000;stereo=0;sprop-stereo=0"
    ));
    assert!(has(&sdp, "a=rtpmap:9 G722/8000"));
    assert!(has(&sdp, "a=rtpmap:0 PCMU/8000"));
    assert!(has(&sdp, "a=rtpmap:8 PCMA/8000"));
    assert!(has(&sdp, "a=rtpmap:101 telephone-event/8000"));
    assert!(has(&sdp, "a=rtpmap:102 telephone-event/48000"));
    assert!(has(&sdp, "a=fmtp:101 0-16"));
    assert!(has(&sdp, "a=fmtp:102 0-16"));
    assert!(has(&sdp, "a=sendrecv"));
    assert!(has(&sdp, &format!("a=rtcp:{}", PORT + 1)));
    assert!(!sdp.contains("a=rtcp-mux"));
    assert!(!sdp.contains("a=ptime"));
    assert!(!sdp.contains("a=crypto"));
    assert!(!sdp.contains("maxptime"));
    // Round-trips through our own parser.
    let p = parse(&sdp);
    assert_eq!(p.media[0].fmts, vec![111, 9, 0, 8, 101, 102]);
}

#[test]
fn offer_without_opus_has_only_te8000() {
    let mut s = session(caps_with(
        &[AudioCodec::Pcmu, AudioCodec::Pcma],
        SrtpPolicy::Off,
    ));
    let sdp = s.create_offer();
    assert!(has(&sdp, &format!("m=audio {PORT} RTP/AVP 0 8 101")));
    assert!(!sdp.contains("48000"));
    assert!(!sdp.contains("opus"));
}

#[test]
fn offer_opus_params_and_custom_pts() {
    let caps = LocalCaps {
        codecs: vec![AudioCodec::Opus],
        opus: OpusSdpParams {
            useinbandfec: false,
            maxplaybackrate: 16000,
            maxaveragebitrate: Some(20000),
        },
        opus_pt: 96,
        te_pt: 120,
        comfort_noise: true,
        ptime_ms: Some(30),
        rtcp_mux: true,
        ..Default::default()
    };
    let mut s = session(caps);
    let sdp = s.create_offer();
    assert!(has(&sdp, &format!("m=audio {PORT} RTP/AVP 96 120 121 13")));
    assert!(has(
        &sdp,
        "a=fmtp:96 useinbandfec=0;maxplaybackrate=16000;stereo=0;sprop-stereo=0;maxaveragebitrate=20000"
    ));
    assert!(has(&sdp, "a=rtpmap:121 telephone-event/48000"));
    assert!(has(&sdp, "a=rtpmap:13 CN/8000"));
    // 30 ms is not encodable by Opus: offered ptime snaps to 20.
    assert!(has(&sdp, "a=ptime:20"));
    assert!(has(&sdp, "a=rtcp-mux"));
    assert!(!sdp.contains("a=rtcp:"));
}

#[test]
fn offer_telephone_event_can_be_disabled() {
    let caps = LocalCaps {
        telephone_event: false,
        ..Default::default()
    };
    let sdp = session(caps).create_offer();
    assert!(!sdp.contains("telephone-event"));
}

#[test]
fn offer_srtp_policies() {
    for (policy, proto, ncrypto) in [
        (SrtpPolicy::Off, "RTP/AVP", 0),
        (SrtpPolicy::Optional, "RTP/AVP", 4),
        (SrtpPolicy::Mandatory, "RTP/SAVP", 4),
    ] {
        let mut s = session(caps_with(&[AudioCodec::Pcmu], policy));
        let sdp = s.create_offer();
        let p = parse(&sdp);
        assert_eq!(p.media[0].proto.as_str(), proto, "{policy:?}");
        assert_eq!(p.media[0].crypto.len(), ncrypto, "{policy:?}");
        let mut keys = std::collections::HashSet::new();
        for (i, c) in p.media[0].crypto.iter().enumerate() {
            assert_eq!(c.tag, i as u32 + 1);
            let suite = SrtpSuite::from_sdp_name(&c.suite).unwrap();
            assert_eq!(suite, SrtpSuite::ALL[i]);
            let inline = c.first_inline().unwrap();
            SrtpContext::from_inline(suite, inline).expect("valid key");
            assert!(keys.insert(inline.to_string()), "keys must be distinct");
        }
    }
}

#[test]
fn offer_srtp_suite_subset_and_order() {
    let caps = LocalCaps {
        srtp: SrtpPolicy::Mandatory,
        srtp_suites: vec![
            SrtpSuite::Aes256CmHmacSha1_80,
            SrtpSuite::AesCm128HmacSha1_80,
        ],
        ..Default::default()
    };
    let p = parse(&session(caps).create_offer());
    let c = &p.media[0].crypto;
    assert_eq!(c.len(), 2);
    assert_eq!(
        (c[0].tag, c[0].suite.as_str()),
        (1, "AES_256_CM_HMAC_SHA1_80")
    );
    assert_eq!(
        (c[1].tag, c[1].suite.as_str()),
        (2, "AES_CM_128_HMAC_SHA1_80")
    );
}

#[test]
fn offer_directions_follow_local_hold() {
    for (hold, dir) in [
        (LocalHold::None, "a=sendrecv"),
        (LocalHold::SendOnly, "a=sendonly"),
        (LocalHold::Inactive, "a=inactive"),
    ] {
        let mut s = session(LocalCaps::default());
        s.set_local_hold(hold);
        let sdp = s.create_offer();
        assert!(has(&sdp, dir), "{hold:?}");
        assert_eq!(
            sdp.matches("a=send").count() + sdp.matches("a=inactive").count(),
            1
        );
    }
}

#[test]
fn offer_ipv6() {
    let mut s = MediaSession::new(LocalCaps::default(), "2001:db8::1".parse().unwrap(), 20000);
    let sdp = s.create_offer();
    assert!(has(&sdp, "c=IN IP6 2001:db8::1"));
    assert!(lines(&sdp)[1].ends_with("IN IP6 2001:db8::1"));
}

// ── o= version stability ────────────────────────────────────────────────

#[test]
fn version_is_stable_until_content_changes() {
    let mut s = session(caps_with(&[AudioCodec::Pcmu], SrtpPolicy::Optional));
    let a = s.create_offer();
    let b = s.current_sdp();
    let c = s.current_sdp();
    assert_eq!(a, b);
    assert_eq!(b, c);
    assert_eq!(version_of(&a), 1);

    s.set_local_hold(LocalHold::SendOnly);
    let d = s.current_sdp();
    assert_eq!(version_of(&d), 2);
    assert_eq!(s.current_sdp(), d);

    s.set_local_hold(LocalHold::None);
    let e = s.current_sdp();
    assert_eq!(version_of(&e), 3);
    assert_eq!(body_without_o(&a), body_without_o(&e));

    s.set_local_address("192.0.2.9".parse().unwrap(), 20000);
    assert_eq!(version_of(&s.current_sdp()), 4);
    // sess_id never changes
    assert_eq!(
        parse(&a).origin.unwrap().sess_id,
        parse(&s.current_sdp()).origin.unwrap().sess_id
    );
}

#[test]
fn srtp_keys_are_stable_across_reoffers() {
    let mut s = session(caps_with(&[AudioCodec::Pcmu], SrtpPolicy::Mandatory));
    let a = parse(&s.create_offer());
    s.set_local_hold(LocalHold::Inactive);
    let b = parse(&s.current_sdp());
    assert_eq!(a.media[0].crypto, b.media[0].crypto);
}

#[test]
fn answer_is_identical_for_session_refresh() {
    let mut s = session(caps_with(
        &[AudioCodec::Opus, AudioCodec::Pcmu],
        SrtpPolicy::Optional,
    ));
    let offer = parse(&fs_offer_savp());
    let (a1, n1) = s.apply_offer(&offer).unwrap();
    let (a2, n2) = s.apply_offer(&offer).unwrap();
    assert_eq!(a1, a2);
    assert_eq!(version_of(&a1), version_of(&a2));
    // Same key => no remote change.
    assert!(!n2.srtp.as_ref().unwrap().remote_changed);
    assert_eq!(
        n1.srtp.as_ref().unwrap().local_key_b64,
        n2.srtp.as_ref().unwrap().local_key_b64
    );
}

#[test]
fn hold_toggle_bumps_answer_version() {
    let mut s = session(caps_with(&[AudioCodec::Pcmu], SrtpPolicy::Off));
    let offer = parse(&yealink_offer());
    let (a1, _) = s.apply_offer(&offer).unwrap();
    s.set_local_hold(LocalHold::SendOnly);
    let (a2, _) = s.apply_offer(&offer).unwrap();
    s.set_local_hold(LocalHold::None);
    let (a3, _) = s.apply_offer(&offer).unwrap();
    assert_eq!(version_of(&a1), 1);
    assert_eq!(version_of(&a2), 2);
    assert_eq!(version_of(&a3), 3);
    assert_eq!(body_without_o(&a1), body_without_o(&a3));
}

// ── answer: codec selection ─────────────────────────────────────────────

fn answer_codec(
    local: &[AudioCodec],
    order: AnswerOrder,
    offer: &str,
) -> Result<(String, Negotiated), NegError> {
    let caps = LocalCaps {
        codecs: local.to_vec(),
        answer_order: order,
        ..Default::default()
    };
    session(caps).apply_offer(&parse(offer))
}

#[test]
fn codec_selection_table() {
    use AudioCodec::*;
    // (local list, offer, order, expected codec, expected PT)
    let cases: Vec<(Vec<AudioCodec>, String, AnswerOrder, AudioCodec, u8)> = vec![
        (
            vec![Pcma, Pcmu],
            fs_offer_avp(),
            AnswerOrder::Offer,
            Pcmu,
            0,
        ),
        (
            vec![Pcma, Pcmu],
            fs_offer_avp(),
            AnswerOrder::Local,
            Pcma,
            8,
        ),
        (
            vec![Opus, G722, Pcmu, Pcma],
            fs_offer_avp(),
            AnswerOrder::Offer,
            Opus,
            111,
        ),
        (
            vec![Pcmu, Opus],
            fs_offer_avp(),
            AnswerOrder::Offer,
            Opus,
            111,
        ),
        (
            vec![Pcmu, Opus],
            fs_offer_avp(),
            AnswerOrder::Local,
            Pcmu,
            0,
        ),
        (
            vec![G722, Pcma],
            fs_offer_avp(),
            AnswerOrder::Offer,
            G722,
            9,
        ),
        (
            vec![Pcma, G722],
            fs_offer_avp(),
            AnswerOrder::Offer,
            G722,
            9,
        ),
        (
            vec![Pcma, G722],
            fs_offer_avp(),
            AnswerOrder::Local,
            Pcma,
            8,
        ),
        (
            vec![Pcmu, Pcma, G722],
            yealink_offer(),
            AnswerOrder::Offer,
            Pcmu,
            0,
        ),
        (
            vec![G722, Pcma],
            yealink_offer(),
            AnswerOrder::Offer,
            Pcma,
            8,
        ),
        (
            vec![G722, Pcma],
            yealink_offer(),
            AnswerOrder::Local,
            G722,
            9,
        ),
        (
            vec![Opus, Pcma],
            yealink_offer(),
            AnswerOrder::Offer,
            Pcma,
            8,
        ),
        (
            vec![G722, Pcmu],
            polycom_offer(),
            AnswerOrder::Offer,
            G722,
            9,
        ),
        (
            vec![Pcmu, G722],
            polycom_offer(),
            AnswerOrder::Offer,
            G722,
            9,
        ),
        (
            vec![Pcmu, G722],
            polycom_offer(),
            AnswerOrder::Local,
            Pcmu,
            0,
        ),
    ];
    for (i, (local, offer, order, codec, pt)) in cases.into_iter().enumerate() {
        let caps = LocalCaps {
            codecs: local.clone(),
            answer_order: order,
            srtp: SrtpPolicy::Optional,
            ..Default::default()
        };
        let (answer, n) = session(caps).apply_offer(&parse(&offer)).unwrap();
        assert_eq!(
            (n.codec, n.pt),
            (codec, pt),
            "case {i}: {local:?} {order:?}"
        );
        // Exactly one codec in the answer (+ te).
        let m = &parse(&answer).media[0];
        let audio: Vec<_> = m
            .fmts
            .iter()
            .filter(|&&p| {
                m.rtpmap_for(p)
                    .is_some_and(|r| !r.name.eq_ignore_ascii_case("telephone-event"))
            })
            .collect();
        assert_eq!(audio, vec![&pt], "case {i}: {answer}");
        assert_eq!(n.clock_rate, codec.rtp_clock_rate());
    }
}

#[test]
fn case_insensitive_rtpmap_names_and_dynamic_pt_numbers() {
    let offer = "v=0\r\no=- 1 1 IN IP4 1.1.1.1\r\ns=-\r\nc=IN IP4 1.1.1.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 96 97 98\r\na=rtpmap:96 OPUS/48000/2\r\na=rtpmap:97 Telephone-Event/48000\r\na=rtpmap:98 telephone-event/8000\r\n";
    let (ans, n) = answer_codec(&[AudioCodec::Opus], AnswerOrder::Offer, offer).unwrap();
    assert_eq!((n.codec, n.pt), (AudioCodec::Opus, 96));
    assert_eq!(n.te, Some((97, 48000)));
    assert!(has(&ans, "a=rtpmap:96 opus/48000/2"));
    assert!(has(&ans, "a=rtpmap:97 telephone-event/48000"));
    // PT 98 must not be answered (checked per line: random o=/port values may contain "98").
    assert!(!ans.lines().any(|l| {
        l.starts_with("a=rtpmap:98 ")
            || l.starts_with("a=fmtp:98 ")
            || (l.starts_with("m=audio") && l.split(' ').skip(3).any(|p| p == "98"))
    }));
}

#[test]
fn no_common_codec_is_488() {
    let offer = "v=0\r\no=- 1 1 IN IP4 1.1.1.1\r\ns=-\r\nc=IN IP4 1.1.1.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 18 3\r\na=rtpmap:18 G729/8000\r\n";
    let e = answer_codec(&[AudioCodec::Pcmu], AnswerOrder::Offer, offer).unwrap_err();
    assert!(matches!(e, NegError::NotAcceptable(_)));
    assert_eq!(e.status_code(), 488);
    // Opus offered with the wrong clock is not Opus.
    let offer = "v=0\r\no=- 1 1 IN IP4 1.1.1.1\r\ns=-\r\nc=IN IP4 1.1.1.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 96\r\na=rtpmap:96 opus/16000/2\r\n";
    assert!(answer_codec(&[AudioCodec::Opus], AnswerOrder::Offer, offer).is_err());
}

#[test]
fn telephone_event_matches_codec_clock() {
    use AudioCodec::*;
    // FS offer carries te/48000 (102) and te/8000 (101).
    let (a, n) = answer_codec(&[Opus], AnswerOrder::Offer, &fs_offer_avp()).unwrap();
    assert_eq!(n.te, Some((102, 48000)));
    assert!(has(&a, "a=rtpmap:102 telephone-event/48000"));
    assert!(!a.contains("rtpmap:101"));
    for c in [G722, Pcmu, Pcma] {
        let (a, n) = answer_codec(&[c], AnswerOrder::Offer, &fs_offer_avp()).unwrap();
        assert_eq!(n.te, Some((101, 8000)), "{c:?}");
        assert!(!a.contains("rtpmap:102"), "{c:?}");
    }
    // Opus offered with only te/8000 -> te/8000.
    let offer = "v=0\r\no=- 1 1 IN IP4 1.1.1.1\r\ns=-\r\nc=IN IP4 1.1.1.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 111 101\r\na=rtpmap:111 opus/48000/2\r\na=rtpmap:101 telephone-event/8000\r\n";
    let (_, n) = answer_codec(&[Opus], AnswerOrder::Offer, offer).unwrap();
    assert_eq!(n.te, Some((101, 8000)));
    // No te offered -> none; te disabled locally -> none.
    let offer = "v=0\r\no=- 1 1 IN IP4 1.1.1.1\r\ns=-\r\nc=IN IP4 1.1.1.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0\r\n";
    let (a, n) = answer_codec(&[Pcmu], AnswerOrder::Offer, offer).unwrap();
    assert_eq!(n.te, None);
    assert!(!a.contains("telephone-event"));
    let caps = LocalCaps {
        telephone_event: false,
        ..Default::default()
    };
    let (_, n) = session(caps).apply_offer(&parse(&fs_offer_avp())).unwrap();
    assert_eq!(n.te, None);
}

#[test]
fn comfort_noise_only_if_both() {
    let offer = parse(&polycom_offer()); // offers CN (13); SAVP
    for (local_cn, expect) in [(true, Some(13)), (false, None)] {
        let caps = LocalCaps {
            codecs: vec![AudioCodec::Pcmu],
            comfort_noise: local_cn,
            srtp: SrtpPolicy::Optional,
            ..Default::default()
        };
        let (a, n) = session(caps).apply_offer(&offer).unwrap();
        assert_eq!(n.cn_pt, expect);
        assert_eq!(a.contains("CN/8000"), local_cn);
    }
    // Offer without CN, local CN on.
    let caps = LocalCaps {
        codecs: vec![AudioCodec::Pcmu],
        comfort_noise: true,
        ..Default::default()
    };
    let (_, n) = session(caps).apply_offer(&parse(&yealink_offer())).unwrap();
    assert_eq!(n.cn_pt, None);
    // Never CN with Opus.
    let offer = "v=0\r\no=- 1 1 IN IP4 1.1.1.1\r\ns=-\r\nc=IN IP4 1.1.1.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 111 13\r\na=rtpmap:111 opus/48000/2\r\n";
    let caps = LocalCaps {
        codecs: vec![AudioCodec::Opus],
        comfort_noise: true,
        ..Default::default()
    };
    assert_eq!(
        session(caps).apply_offer(&parse(offer)).unwrap().1.cn_pt,
        None
    );
}

#[test]
fn opus_remote_params() {
    let (a, n) = answer_codec(&[AudioCodec::Opus], AnswerOrder::Offer, &fs_offer_avp()).unwrap();
    let r = n.opus_remote.unwrap();
    assert_eq!(
        r,
        OpusSdpParams {
            useinbandfec: true,
            maxplaybackrate: 16000,
            maxaveragebitrate: Some(30000)
        }
    );
    // Our own answer fmtp states OUR parameters and mono.
    assert!(has(
        &a,
        "a=fmtp:111 useinbandfec=1;maxplaybackrate=48000;stereo=0;sprop-stereo=0"
    ));
    // Absent fmtp => RFC 7587 defaults (no FEC, 48k).
    let offer = "v=0\r\no=- 1 1 IN IP4 1.1.1.1\r\ns=-\r\nc=IN IP4 1.1.1.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 111\r\na=rtpmap:111 opus/48000/2\r\n";
    let (_, n) = answer_codec(&[AudioCodec::Opus], AnswerOrder::Offer, offer).unwrap();
    assert_eq!(
        n.opus_remote,
        Some(OpusSdpParams {
            useinbandfec: false,
            maxplaybackrate: 48000,
            maxaveragebitrate: None
        })
    );
    // Non-Opus: none.
    let (_, n) = answer_codec(&[AudioCodec::Pcmu], AnswerOrder::Offer, &fs_offer_avp()).unwrap();
    assert!(n.opus_remote.is_none());
}

#[test]
fn ptime_echo_and_defaults() {
    // Remote ptime echoed.
    let (a, n) = answer_codec(&[AudioCodec::Pcmu], AnswerOrder::Offer, &yealink_offer()).unwrap();
    assert!(has(&a, "a=ptime:30"));
    assert_eq!(n.ptime_ms, 30);
    // No remote ptime, none configured -> no attribute, 20 default.
    let (a, n) = answer_codec(
        &[AudioCodec::Pcmu],
        AnswerOrder::Offer,
        &simple_offer("RTP/AVP", &[]),
    )
    .unwrap();
    assert!(!a.contains("a=ptime"));
    assert_eq!(n.ptime_ms, 20);
    // Configured ptime used when remote has none.
    let caps = LocalCaps {
        ptime_ms: Some(40),
        ..Default::default()
    };
    let (a, n) = session(caps)
        .apply_offer(&parse(&simple_offer("RTP/AVP", &[])))
        .unwrap();
    assert!(has(&a, "a=ptime:40"));
    assert_eq!(n.ptime_ms, 40);
    // Remote ptime wins over configured.
    let caps = LocalCaps {
        ptime_ms: Some(40),
        ..Default::default()
    };
    let (_, n) = session(caps).apply_offer(&parse(&yealink_offer())).unwrap();
    assert_eq!(n.ptime_ms, 30);
    // Out-of-range remote ptime is ignored.
    let (a, n) = answer_codec(
        &[AudioCodec::Pcmu],
        AnswerOrder::Offer,
        &simple_offer("RTP/AVP", &["a=ptime:200".into()]),
    )
    .unwrap();
    assert!(!a.contains("a=ptime"));
    assert_eq!(n.ptime_ms, 20);
}

#[test]
fn no_forced_maxptime_or_other_extras() {
    let (a, _) = answer_codec(&[AudioCodec::Opus], AnswerOrder::Offer, &fs_offer_avp()).unwrap();
    assert!(!a.contains("maxptime"));
    assert!(!a.contains("a=rtcp-mux"));
    assert!(has(&a, "s=-"));
    assert!(has(&a, "t=0 0"));
}

// ── rtcp / mux / addresses ──────────────────────────────────────────────

#[test]
fn rtcp_mux_only_if_offered_and_enabled() {
    let mux_offer = simple_offer("RTP/AVP", &["a=rtcp-mux".into()]);
    for (offered, local, expect) in [
        (true, true, true),
        (true, false, false),
        (false, true, false),
        (false, false, false),
    ] {
        let offer = if offered {
            mux_offer.clone()
        } else {
            simple_offer("RTP/AVP", &[])
        };
        let caps = LocalCaps {
            rtcp_mux: local,
            ..Default::default()
        };
        let (a, n) = session(caps).apply_offer(&parse(&offer)).unwrap();
        assert_eq!(n.rtcp_mux, expect, "offered={offered} local={local}");
        assert_eq!(a.contains("a=rtcp-mux"), expect);
        assert_eq!(a.contains("a=rtcp:"), !expect);
        assert_eq!(n.remote_rtcp.is_none(), expect);
    }
}

#[test]
fn remote_addresses() {
    let (_, n) = answer_codec(&[AudioCodec::Pcmu], AnswerOrder::Offer, &fs_offer_avp()).unwrap();
    assert_eq!(n.remote_rtp, "203.0.113.10:24002".parse().unwrap());
    assert_eq!(n.remote_rtcp, Some("203.0.113.10:24003".parse().unwrap()));
    // explicit a=rtcp with address, media-level c= override
    let offer = "v=0\r\no=- 1 1 IN IP4 1.1.1.1\r\ns=-\r\nc=IN IP4 1.1.1.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0\r\nc=IN IP4 9.9.9.9\r\na=rtcp:4010 IN IP4 8.8.8.8\r\n";
    let (_, n) = answer_codec(&[AudioCodec::Pcmu], AnswerOrder::Offer, offer).unwrap();
    assert_eq!(n.remote_rtp, "9.9.9.9:4000".parse().unwrap());
    assert_eq!(n.remote_rtcp, Some("8.8.8.8:4010".parse().unwrap()));
    // media-level c= only (no session c=)
    let offer = "v=0\r\no=- 1 1 IN IP6 2001:db8::5\r\ns=-\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0\r\nc=IN IP6 2001:db8::5\r\n";
    let mut s6 = MediaSession::new(LocalCaps::default(), "2001:db8::1".parse().unwrap(), 20000);
    let (_, n) = s6.apply_offer(&parse(offer)).unwrap();
    assert_eq!(n.remote_rtp, "[2001:db8::5]:4000".parse().unwrap());
}

#[test]
fn missing_connection_is_malformed() {
    let offer = "v=0\r\no=- 1 1 IN IP4 1.1.1.1\r\ns=-\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0\r\n";
    let e = answer_codec(&[AudioCodec::Pcmu], AnswerOrder::Offer, offer).unwrap_err();
    assert!(matches!(e, NegError::Malformed(_)));
    assert_eq!(e.status_code(), 400);
}

// ── media section handling ──────────────────────────────────────────────

#[test]
fn non_audio_and_extra_audio_are_rejected_with_port_zero() {
    let offer = "v=0\r\no=- 1 1 IN IP4 1.1.1.1\r\ns=-\r\nc=IN IP4 1.1.1.1\r\nt=0 0\r\n\
m=video 5000 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\n\
m=audio 4000 RTP/AVP 0 101\r\na=rtpmap:101 telephone-event/8000\r\n\
m=audio 4002 RTP/SAVP 8\r\n\
m=application 5002 UDP/DTLS/SCTP webrtc-datachannel\r\n";
    let (a, n) = answer_codec(
        &[AudioCodec::Pcmu, AudioCodec::Pcma],
        AnswerOrder::Offer,
        offer,
    )
    .unwrap();
    assert_eq!(n.remote_rtp.port(), 4000);
    let ls = lines(&a);
    let ms: Vec<&&str> = ls.iter().filter(|l| l.starts_with("m=")).collect();
    assert_eq!(ms.len(), 4, "{a}");
    assert_eq!(*ms[0], "m=video 0 RTP/AVP 96");
    assert_eq!(*ms[1], format!("m=audio {PORT} RTP/AVP 0 101"));
    assert_eq!(*ms[2], "m=audio 0 RTP/SAVP 8");
    assert_eq!(*ms[3], "m=application 0 UDP/DTLS/SCTP webrtc-datachannel");
    // Rejected sections carry no attributes (a= lines only after our m=audio).
    let pos = ls
        .iter()
        .position(|l| l.starts_with("m=audio 16384"))
        .unwrap();
    assert!(ls[..pos].iter().all(|l| !l.starts_with("a=")));
    // Re-parse: answer media count matches.
    assert_eq!(parse(&a).media.len(), 4);
}

#[test]
fn audio_port_zero_only_is_488() {
    let offer = "v=0\r\no=- 1 1 IN IP4 1.1.1.1\r\ns=-\r\nc=IN IP4 1.1.1.1\r\nt=0 0\r\nm=audio 0 RTP/AVP 0\r\n";
    assert_eq!(
        answer_codec(&[AudioCodec::Pcmu], AnswerOrder::Offer, offer)
            .unwrap_err()
            .status_code(),
        488
    );
    let video_only = "v=0\r\no=- 1 1 IN IP4 1.1.1.1\r\ns=-\r\nc=IN IP4 1.1.1.1\r\nt=0 0\r\nm=video 5000 RTP/AVP 96\r\n";
    assert_eq!(
        answer_codec(&[AudioCodec::Pcmu], AnswerOrder::Offer, video_only)
            .unwrap_err()
            .status_code(),
        488
    );
}

#[test]
fn unsupported_transport_is_488() {
    let offer = "v=0\r\no=- 1 1 IN IP4 1.1.1.1\r\ns=-\r\nc=IN IP4 1.1.1.1\r\nt=0 0\r\nm=audio 4000 UDP/TLS/RTP/SAVPF 111\r\na=rtpmap:111 opus/48000/2\r\n";
    assert_eq!(
        answer_codec(&[AudioCodec::Opus], AnswerOrder::Offer, offer)
            .unwrap_err()
            .status_code(),
        488
    );
}

#[test]
fn avpf_proto_is_echoed() {
    let offer = simple_offer("RTP/AVPF", &[]);
    let (a, _) = answer_codec(&[AudioCodec::Pcmu], AnswerOrder::Offer, &offer).unwrap();
    assert_eq!(parse(&a).media[0].proto, MediaProto::RtpAvpf);
}

// ── SRTP on the answer side ─────────────────────────────────────────────

#[derive(Debug, PartialEq)]
enum Expect {
    /// Answer proto, SRTP used, answered tag, suite.
    Srtp(&'static str, u32, SrtpSuite),
    Plain,
    Reject,
}

#[test]
fn srtp_answer_matrix() {
    use Expect::*;
    use SrtpPolicy::*;
    let s80 = SrtpSuite::AesCm128HmacSha1_80;
    let a256 = SrtpSuite::Aes256CmHmacSha1_80;
    let table: Vec<(SrtpPolicy, &str, Expect)> = vec![
        (Off, "avp", Plain),
        (Off, "avp+crypto", Plain),
        (Off, "savp", Reject),
        (Off, "savp-unsupported", Reject),
        (Off, "savp-no-crypto", Reject),
        (Optional, "avp", Plain),
        (Optional, "avp+crypto", Srtp("RTP/AVP", 1, s80)),
        (Optional, "savp", Srtp("RTP/SAVP", 1, s80)),
        (Optional, "savp-unsupported", Reject),
        (Optional, "savp-no-crypto", Reject),
        (Optional, "savp-second-supported", Srtp("RTP/SAVP", 9, a256)),
        (Mandatory, "avp", Reject),
        (Mandatory, "avp+crypto", Srtp("RTP/AVP", 1, s80)),
        (Mandatory, "savp", Srtp("RTP/SAVP", 1, s80)),
        (Mandatory, "savp-unsupported", Reject),
        (Mandatory, "savp-no-crypto", Reject),
        (
            Mandatory,
            "savp-second-supported",
            Srtp("RTP/SAVP", 9, a256),
        ),
    ];
    for (policy, variant, expect) in table {
        let offer = offer_variant(variant);
        let mut s = session(caps_with(&[AudioCodec::Pcmu], policy));
        let r = s.apply_offer(&parse(&offer));
        let ctx = format!("{policy:?} x {variant}");
        match (expect, r) {
            (Reject, Err(NegError::NotAcceptable(_))) => {}
            (Plain, Ok((a, n))) => {
                let m = &parse(&a).media[0];
                assert_eq!(m.proto, MediaProto::RtpAvp, "{ctx}");
                assert!(m.crypto.is_empty(), "{ctx}");
                assert!(n.srtp.is_none(), "{ctx}");
            }
            (Srtp(proto, tag, suite), Ok((a, n))) => {
                let m = &parse(&a).media[0];
                assert_eq!(m.proto.as_str(), proto, "{ctx}");
                assert_eq!(m.crypto.len(), 1, "{ctx}");
                assert_eq!(m.crypto[0].tag, tag, "{ctx}");
                assert_eq!(
                    SrtpSuite::from_sdp_name(&m.crypto[0].suite),
                    Some(suite),
                    "{ctx}"
                );
                let p = n.srtp.expect(&ctx);
                assert_eq!(p.suite, suite, "{ctx}");
                assert!(!p.remote_changed, "{ctx}");
                // Our answered key is our key for that suite and is valid.
                assert_eq!(
                    m.crypto[0].first_inline().unwrap(),
                    format!("inline:{}", p.local_key_b64)
                );
                SrtpContext::from_sdes_key(suite, &p.local_key_b64).unwrap();
                SrtpContext::from_inline(suite, &p.remote_key_b64).unwrap();
            }
            (e, r) => panic!("{ctx}: expected {e:?}, got {r:?}"),
        }
    }
}

#[test]
fn srtp_suite_restrictions_are_respected() {
    // Only AES-256 allowed locally; offer has 128 first then 256.
    let offer = format!(
        "v=0\r\no=- 1 1 IN IP4 1.1.1.1\r\ns=-\r\nc=IN IP4 1.1.1.1\r\nt=0 0\r\nm=audio 4000 RTP/SAVP 0\r\na=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:{}\r\na=crypto:2 AES_CM_256_HMAC_SHA1_32 inline:{}\r\n",
        key(SrtpSuite::AesCm128HmacSha1_80),
        key(SrtpSuite::Aes256CmHmacSha1_32)
    );
    let caps = LocalCaps {
        codecs: vec![AudioCodec::Pcmu],
        srtp: SrtpPolicy::Mandatory,
        srtp_suites: vec![SrtpSuite::Aes256CmHmacSha1_32],
        ..Default::default()
    };
    let (a, n) = session(caps.clone()).apply_offer(&parse(&offer)).unwrap();
    assert_eq!(n.srtp.unwrap().suite, SrtpSuite::Aes256CmHmacSha1_32);
    let c = &parse(&a).media[0].crypto[0];
    assert_eq!(c.tag, 2);
    assert_eq!(c.suite, "AES_256_CM_HMAC_SHA1_32");
    // Only 128 offered -> reject.
    let offer128 = format!(
        "v=0\r\no=- 1 1 IN IP4 1.1.1.1\r\ns=-\r\nc=IN IP4 1.1.1.1\r\nt=0 0\r\nm=audio 4000 RTP/SAVP 0\r\na=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:{}\r\n",
        key(SrtpSuite::AesCm128HmacSha1_80)
    );
    assert!(session(caps).apply_offer(&parse(&offer128)).is_err());
}

#[test]
fn bad_keys_are_skipped() {
    // First crypto line has a key of the wrong length; second is fine.
    let offer = format!(
        "v=0\r\no=- 1 1 IN IP4 1.1.1.1\r\ns=-\r\nc=IN IP4 1.1.1.1\r\nt=0 0\r\nm=audio 4000 RTP/SAVP 0\r\na=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:QUJD\r\na=crypto:2 AES_CM_128_HMAC_SHA1_32 inline:{}|2^20\r\n",
        key(SrtpSuite::AesCm128HmacSha1_32)
    );
    let mut s = session(caps_with(&[AudioCodec::Pcmu], SrtpPolicy::Mandatory));
    let (_, n) = s.apply_offer(&parse(&offer)).unwrap();
    let p = n.srtp.unwrap();
    assert_eq!(p.suite, SrtpSuite::AesCm128HmacSha1_32);
    assert!(
        p.remote_key_b64.ends_with("|2^20"),
        "lifetime preserved: {}",
        p.remote_key_b64
    );
}

#[test]
fn freeswitch_savp_offer_picks_first_supported_and_aliases_work() {
    let mut s = session(caps_with(
        &[AudioCodec::Opus, AudioCodec::Pcmu],
        SrtpPolicy::Mandatory,
    ));
    let (a, n) = s.apply_offer(&parse(&fs_offer_savp())).unwrap();
    // FS lists AES_CM_256_HMAC_SHA1_80 first (alias spelling); we answer with the RFC spelling.
    assert_eq!(
        n.srtp.as_ref().unwrap().suite,
        SrtpSuite::Aes256CmHmacSha1_80
    );
    assert!(a.contains("a=crypto:1 AES_256_CM_HMAC_SHA1_80 inline:"));
    assert_eq!(n.codec, AudioCodec::Opus);
    assert_eq!(n.pt, 111);
    assert_eq!(n.te, Some((102, 48000)));
    assert_eq!(n.ptime_ms, 20);
    assert!(a.contains("RTP/SAVP"));
    // Local preference limited to 128-bit/80: tag 3.
    let caps = LocalCaps {
        srtp: SrtpPolicy::Mandatory,
        srtp_suites: vec![SrtpSuite::AesCm128HmacSha1_80],
        ..Default::default()
    };
    let (a, _) = session(caps).apply_offer(&parse(&fs_offer_savp())).unwrap();
    assert!(a.contains("a=crypto:3 AES_CM_128_HMAC_SHA1_80 inline:"));
}

#[test]
fn polycom_savp_offer() {
    let caps = LocalCaps {
        codecs: vec![AudioCodec::Pcmu, AudioCodec::G722],
        srtp: SrtpPolicy::Optional,
        ..Default::default()
    };
    let (a, n) = session(caps).apply_offer(&parse(&polycom_offer())).unwrap();
    assert_eq!(n.codec, AudioCodec::G722);
    // Line 1 carries UNENCRYPTED_SRTCP, which we do not implement: it is
    // skipped and line 2 wins; lifetime kept on the remote key.
    let p = n.srtp.unwrap();
    assert_eq!(p.suite, SrtpSuite::AesCm128HmacSha1_80);
    assert!(p.remote_key_b64.ends_with("|2^31"));
    assert!(a.contains("a=crypto:2 AES_CM_128_HMAC_SHA1_80 inline:"));
    assert!(!a.contains("a=crypto:1"));
    assert!(!a.contains("UNENCRYPTED"));
}

#[test]
fn remote_changed_detection() {
    let mut s = session(caps_with(&[AudioCodec::Pcmu], SrtpPolicy::Optional));
    let o1 = parse(&offer_variant("savp"));
    let (_, n1) = s.apply_offer(&o1).unwrap();
    assert!(!n1.srtp.as_ref().unwrap().remote_changed);
    let (_, n2) = s.apply_offer(&o1).unwrap();
    assert!(!n2.srtp.as_ref().unwrap().remote_changed);
    // New key from the peer (e.g. after a transfer / re-INVITE).
    let o2 = parse(&offer_variant("savp"));
    let (_, n3) = s.apply_offer(&o2).unwrap();
    assert!(n3.srtp.as_ref().unwrap().remote_changed);
    // Our own key never changes.
    assert_eq!(
        n1.srtp.unwrap().local_key_b64,
        n3.srtp.unwrap().local_key_b64
    );
    // Stable again afterwards.
    let (_, n4) = s.apply_offer(&o2).unwrap();
    assert!(!n4.srtp.unwrap().remote_changed);
}

// ── direction matrix ────────────────────────────────────────────────────

#[test]
fn direction_matrix_answer() {
    use Direction::*;
    use LocalHold as H;
    // (hold, remote dir, c=0.0.0.0) -> (our dir, send, recv, remote_hold)
    let table = [
        (H::None, SendRecv, false, SendRecv, true, true, false),
        (H::None, SendOnly, false, RecvOnly, false, true, true),
        (H::None, RecvOnly, false, SendOnly, true, false, false),
        (H::None, Inactive, false, Inactive, false, false, true),
        (H::SendOnly, SendRecv, false, SendOnly, true, false, false),
        (H::SendOnly, SendOnly, false, Inactive, false, false, true),
        (H::SendOnly, RecvOnly, false, SendOnly, true, false, false),
        (H::SendOnly, Inactive, false, Inactive, false, false, true),
        (H::Inactive, SendRecv, false, Inactive, false, false, false),
        (H::Inactive, SendOnly, false, Inactive, false, false, true),
        (H::Inactive, RecvOnly, false, Inactive, false, false, false),
        (H::Inactive, Inactive, false, Inactive, false, false, true),
        (H::None, SendRecv, true, RecvOnly, false, true, true),
        (H::SendOnly, SendRecv, true, Inactive, false, false, true),
        (H::None, RecvOnly, true, Inactive, false, false, true),
    ];
    for (hold, rdir, zero, odir, send, recv, rhold) in table {
        let ip = if zero { "0.0.0.0" } else { "198.51.100.7" };
        let offer = format!(
            "v=0\r\no=- 5 5 IN IP4 {ip}\r\ns=-\r\nc=IN IP4 {ip}\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0\r\na={}\r\n",
            rdir.as_str()
        );
        let mut s = session(caps_with(&[AudioCodec::Pcmu], SrtpPolicy::Off));
        s.set_local_hold(hold);
        let (a, n) = s.apply_offer(&parse(&offer)).unwrap();
        let ctx = format!("{hold:?} vs {rdir:?} zero={zero}");
        assert_eq!(parse(&a).media[0].direction, odir, "{ctx}");
        assert_eq!(n.local_direction, odir, "{ctx}");
        assert_eq!((n.send_enabled, n.recv_enabled), (send, recv), "{ctx}");
        assert_eq!(n.remote_hold, rhold, "{ctx}");
    }
}

#[test]
fn session_level_direction_is_inherited() {
    let offer = "v=0\r\no=- 1 1 IN IP4 1.1.1.1\r\ns=-\r\nc=IN IP4 1.1.1.1\r\nt=0 0\r\na=sendonly\r\nm=audio 4000 RTP/AVP 0\r\n";
    let (a, n) = answer_codec(&[AudioCodec::Pcmu], AnswerOrder::Offer, offer).unwrap();
    assert!(has(&a, "a=recvonly"));
    assert!(n.remote_hold && !n.send_enabled && n.recv_enabled);
}

// ── offerer side: applying answers ──────────────────────────────────────

/// Build an answer matching our offer with given tweaks.
fn answer_for(
    offer_sdp: &str,
    proto: Option<&str>,
    dir: &str,
    extra: &[String],
) -> SessionDescription {
    let o = parse(offer_sdp);
    let m = &o.media[0];
    let mut s = format!(
        "v=0\r\no=- 77 77 IN IP4 203.0.113.50\r\ns=-\r\nc=IN IP4 203.0.113.50\r\nt=0 0\r\nm=audio 30000 {} 0 101\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=fmtp:101 0-16\r\na=ptime:20\r\na={dir}\r\n",
        proto.unwrap_or(m.proto.as_str())
    );
    for e in extra {
        s.push_str(e);
        s.push_str("\r\n");
    }
    parse(&s)
}

#[test]
fn apply_answer_plain() {
    let mut s = session(LocalCaps::default());
    let offer = s.create_offer();
    let n = s
        .apply_answer(&answer_for(&offer, None, "sendrecv", &[]))
        .unwrap();
    assert_eq!((n.codec, n.pt, n.clock_rate), (AudioCodec::Pcmu, 0, 8000));
    assert_eq!(n.te, Some((101, 8000)));
    assert_eq!(n.remote_rtp, "203.0.113.50:30000".parse().unwrap());
    assert_eq!(n.remote_rtcp, Some("203.0.113.50:30001".parse().unwrap()));
    assert!(n.srtp.is_none());
    assert!(n.send_enabled && n.recv_enabled && !n.remote_hold);
    assert_eq!(n.local_direction, Direction::SendRecv);
    assert_eq!(s.negotiated(), Some(&n));
}

#[test]
fn apply_answer_srtp_variants() {
    // Optional: answer with crypto (SAVP or AVP) and without.
    for (policy, proto, with_crypto, ok_srtp) in [
        (SrtpPolicy::Optional, Some("RTP/AVP"), true, true),
        (SrtpPolicy::Optional, Some("RTP/SAVP"), true, true),
        (SrtpPolicy::Optional, Some("RTP/AVP"), false, false),
        (SrtpPolicy::Mandatory, Some("RTP/SAVP"), true, true),
    ] {
        let mut s = session(caps_with(&[AudioCodec::Pcmu], policy));
        let offer = s.create_offer();
        let extra = if with_crypto {
            vec![crypto_attr(
                3,
                "AES_CM_128_HMAC_SHA1_32",
                SrtpSuite::AesCm128HmacSha1_32,
            )]
        } else {
            vec![]
        };
        let n = s
            .apply_answer(&answer_for(&offer, proto, "sendrecv", &extra))
            .unwrap();
        assert_eq!(n.srtp.is_some(), ok_srtp, "{policy:?} {proto:?}");
        if let Some(p) = n.srtp {
            assert_eq!(p.suite, SrtpSuite::AesCm128HmacSha1_32);
            // The key we proffered for that suite appears in our offer under tag 3.
            let o = parse(&offer);
            let c = o.media[0].crypto.iter().find(|c| c.tag == 3).unwrap();
            assert_eq!(
                c.first_inline().unwrap(),
                format!("inline:{}", p.local_key_b64)
            );
            assert!(!p.remote_changed);
        }
    }
}

#[test]
fn apply_answer_srtp_errors() {
    let mut s = session(caps_with(&[AudioCodec::Pcmu], SrtpPolicy::Mandatory));
    let offer = s.create_offer();
    // Tag not offered.
    let e = s
        .apply_answer(&answer_for(
            &offer,
            None,
            "sendrecv",
            &[crypto_attr(
                9,
                "AES_CM_128_HMAC_SHA1_80",
                SrtpSuite::AesCm128HmacSha1_80,
            )],
        ))
        .unwrap_err();
    assert!(matches!(e, NegError::NotAcceptable(_)));
    // Tag 1 but a different suite than we offered under tag 1.
    let e = s
        .apply_answer(&answer_for(
            &offer,
            None,
            "sendrecv",
            &[crypto_attr(
                1,
                "AES_CM_128_HMAC_SHA1_32",
                SrtpSuite::AesCm128HmacSha1_32,
            )],
        ))
        .unwrap_err();
    assert!(matches!(e, NegError::NotAcceptable(_)));
    // Mandatory but no crypto in the answer.
    assert!(
        s.apply_answer(&answer_for(&offer, None, "sendrecv", &[]))
            .is_err()
    );
    // Unknown suite.
    assert!(
        s.apply_answer(&answer_for(
            &offer,
            None,
            "sendrecv",
            &["a=crypto:1 FOO inline:AAAA".into()]
        ))
        .is_err()
    );
    // Garbage key.
    assert!(
        s.apply_answer(&answer_for(
            &offer,
            None,
            "sendrecv",
            &["a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:QUJD".into()]
        ))
        .is_err()
    );
    // Optional + SAVP answer without crypto.
    let mut s = session(caps_with(&[AudioCodec::Pcmu], SrtpPolicy::Optional));
    let offer = s.create_offer();
    assert!(
        s.apply_answer(&answer_for(&offer, Some("RTP/SAVP"), "sendrecv", &[]))
            .is_err()
    );
}

#[test]
fn apply_answer_off_ignores_unsolicited_crypto_but_rejects_savp() {
    let mut s = session(LocalCaps::default());
    let offer = s.create_offer();
    let n = s
        .apply_answer(&answer_for(
            &offer,
            None,
            "sendrecv",
            &[crypto_attr(
                1,
                "AES_CM_128_HMAC_SHA1_80",
                SrtpSuite::AesCm128HmacSha1_80,
            )],
        ))
        .unwrap();
    assert!(n.srtp.is_none());
    assert!(
        s.apply_answer(&answer_for(&offer, Some("RTP/SAVP"), "sendrecv", &[]))
            .is_err()
    );
}

#[test]
fn apply_answer_errors() {
    let mut s = session(LocalCaps::default());
    // No outstanding offer.
    let any = parse(&fs_offer_avp());
    assert!(matches!(s.apply_answer(&any), Err(NegError::Malformed(_))));
    let offer = s.create_offer();
    // Rejected stream.
    let rej = parse(
        "v=0\r\no=- 1 1 IN IP4 1.1.1.1\r\ns=-\r\nc=IN IP4 1.1.1.1\r\nt=0 0\r\nm=audio 0 RTP/AVP 0\r\n",
    );
    assert!(matches!(
        s.apply_answer(&rej),
        Err(NegError::NotAcceptable(_))
    ));
    // Codec we never offered (PCMA offered, G.729 answered).
    let mut s2 = session(caps_with(&[AudioCodec::Pcmu], SrtpPolicy::Off));
    s2.create_offer();
    let bad = parse(
        "v=0\r\no=- 1 1 IN IP4 1.1.1.1\r\ns=-\r\nc=IN IP4 1.1.1.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 18\r\na=rtpmap:18 G729/8000\r\n",
    );
    assert!(matches!(
        s2.apply_answer(&bad),
        Err(NegError::NotAcceptable(_))
    ));
    // No connection.
    let nc = parse("v=0\r\no=- 1 1 IN IP4 1.1.1.1\r\ns=-\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0\r\n");
    assert!(matches!(s.apply_answer(&nc), Err(NegError::Malformed(_))));
    // Empty media.
    let none = parse("v=0\r\no=- 1 1 IN IP4 1.1.1.1\r\ns=-\r\nt=0 0\r\n");
    assert!(matches!(s.apply_answer(&none), Err(NegError::Malformed(_))));
    let _ = offer;
}

#[test]
fn apply_answer_direction_matrix() {
    use Direction::*;
    use LocalHold as H;
    // (our hold, answer dir) -> (send, recv, remote_hold)
    let table = [
        (H::None, SendRecv, true, true, false),
        (H::None, SendOnly, false, true, true),
        (H::None, RecvOnly, true, false, false),
        (H::None, Inactive, false, false, true),
        (H::SendOnly, RecvOnly, true, false, false),
        (H::SendOnly, SendRecv, true, false, false),
        (H::SendOnly, SendOnly, false, false, true),
        (H::SendOnly, Inactive, false, false, true),
        (H::Inactive, SendRecv, false, false, false),
        (H::Inactive, Inactive, false, false, true),
    ];
    for (hold, adir, send, recv, rhold) in table {
        let mut s = session(caps_with(&[AudioCodec::Pcmu], SrtpPolicy::Off));
        s.set_local_hold(hold);
        let offer = s.create_offer();
        let n = s
            .apply_answer(&answer_for(&offer, None, adir.as_str(), &[]))
            .unwrap();
        let ctx = format!("{hold:?} / answer {adir:?}");
        assert_eq!(
            (n.send_enabled, n.recv_enabled, n.remote_hold),
            (send, recv, rhold),
            "{ctx}"
        );
        let expect_local = match hold {
            H::None => SendRecv,
            H::SendOnly => SendOnly,
            H::Inactive => Inactive,
        };
        assert_eq!(n.local_direction, expect_local, "{ctx}");
    }
}

#[test]
fn offer_answer_two_sessions_agree() {
    use AudioCodec::*;
    let codec_lists: Vec<Vec<AudioCodec>> = vec![
        vec![Opus, G722, Pcmu, Pcma],
        vec![Pcmu],
        vec![Pcma, Pcmu],
        vec![G722, Opus],
    ];
    let policies = [SrtpPolicy::Off, SrtpPolicy::Optional, SrtpPolicy::Mandatory];
    for offerer_codecs in &codec_lists {
        for answerer_codecs in &codec_lists {
            for order in [AnswerOrder::Offer, AnswerOrder::Local] {
                for &po in &policies {
                    for &pa in &policies {
                        let common = offerer_codecs.iter().any(|c| answerer_codecs.contains(c));
                        let srtp_ok = !matches!(
                            (po, pa),
                            (SrtpPolicy::Off, SrtpPolicy::Mandatory)
                                | (SrtpPolicy::Mandatory, SrtpPolicy::Off)
                        );
                        let mut a = session(LocalCaps {
                            codecs: offerer_codecs.clone(),
                            srtp: po,
                            rtcp_mux: true,
                            ..Default::default()
                        });
                        let mut b = session(LocalCaps {
                            codecs: answerer_codecs.clone(),
                            srtp: pa,
                            answer_order: order,
                            rtcp_mux: true,
                            ..Default::default()
                        });
                        let offer = a.create_offer();
                        let ctx = format!(
                            "{offerer_codecs:?} -> {answerer_codecs:?} {order:?} {po:?}/{pa:?}"
                        );
                        match b.apply_offer(&parse(&offer)) {
                            Err(e) => {
                                assert!(!common || !srtp_ok, "{ctx}: unexpected {e}");
                                assert_eq!(e.status_code(), 488, "{ctx}");
                            }
                            Ok((answer, nb)) => {
                                assert!(common && srtp_ok, "{ctx}: should have failed");
                                let na = a
                                    .apply_answer(&parse(&answer))
                                    .unwrap_or_else(|e| panic!("{ctx}: {e}\n{offer}\n{answer}"));
                                assert_eq!((na.codec, na.pt), (nb.codec, nb.pt), "{ctx}");
                                assert_eq!(na.te, nb.te, "{ctx}");
                                assert_eq!(na.rtcp_mux, nb.rtcp_mux, "{ctx}");
                                assert!(na.rtcp_mux, "{ctx}");
                                assert_eq!(na.srtp.is_some(), nb.srtp.is_some(), "{ctx}");
                                if let (Some(sa), Some(sb)) = (&na.srtp, &nb.srtp) {
                                    assert_eq!(sa.suite, sb.suite, "{ctx}");
                                    assert_eq!(
                                        format!("inline:{}", sa.local_key_b64),
                                        sb.remote_key_b64,
                                        "{ctx}"
                                    );
                                    assert_eq!(
                                        format!("inline:{}", sb.local_key_b64),
                                        sa.remote_key_b64,
                                        "{ctx}"
                                    );
                                    // Both ends can actually talk.
                                    let mut tx =
                                        SrtpContext::from_sdes_key(sa.suite, &sa.local_key_b64)
                                            .unwrap();
                                    let mut rx =
                                        SrtpContext::from_inline(sb.suite, &sb.remote_key_b64)
                                            .unwrap();
                                    let pkt = crate::rtp::RtpHeader::build(
                                        0,
                                        1,
                                        160,
                                        7,
                                        false,
                                        &[1, 2, 3],
                                    );
                                    let prot = tx.protect(&pkt).unwrap();
                                    assert_eq!(rx.unprotect(&prot).unwrap(), pkt, "{ctx}");
                                }
                                // Preferred codec selection consistency.
                                let expect = match order {
                                    AnswerOrder::Offer => {
                                        offerer_codecs.iter().find(|c| answerer_codecs.contains(c))
                                    }
                                    AnswerOrder::Local => {
                                        answerer_codecs.iter().find(|c| offerer_codecs.contains(c))
                                    }
                                };
                                assert_eq!(Some(&nb.codec), expect, "{ctx}");
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn offerless_invite_flow_with_fs_answer() {
    // We send an offer in the 2xx; FreeSWITCH answers in the ACK.
    let mut s = session(caps_with(
        &[AudioCodec::Opus, AudioCodec::Pcmu],
        SrtpPolicy::Mandatory,
    ));
    let offer = s.create_offer();
    let o = parse(&offer);
    let c = &o.media[0].crypto;
    // FS picks our tag 2 (AES_256...80 is tag 2 in default order) and answers with its own key.
    let t = c
        .iter()
        .find(|c| c.suite == "AES_256_CM_HMAC_SHA1_80")
        .unwrap()
        .tag;
    let answer = format!(
        "v=0\r\no=FreeSWITCH 1 2 IN IP4 203.0.113.10\r\ns=FreeSWITCH\r\nc=IN IP4 203.0.113.10\r\nt=0 0\r\nm=audio 24010 RTP/SAVP 111 102\r\na=rtpmap:111 opus/48000/2\r\na=fmtp:111 useinbandfec=1; stereo=0\r\na=rtpmap:102 telephone-event/48000\r\na=ptime:20\r\na=sendrecv\r\na=crypto:{t} AES_CM_256_HMAC_SHA1_80 inline:{}\r\n",
        key(SrtpSuite::Aes256CmHmacSha1_80)
    );
    let n = s.apply_answer(&parse(&answer)).unwrap();
    assert_eq!((n.codec, n.pt), (AudioCodec::Opus, 111));
    assert_eq!(n.te, Some((102, 48000)));
    assert_eq!(
        n.srtp.as_ref().unwrap().suite,
        SrtpSuite::Aes256CmHmacSha1_80
    );
    assert!(n.opus_remote.unwrap().useinbandfec);
}

#[test]
fn sdp_less_reinvite_reoffers_current_state() {
    let mut s = session(caps_with(
        &[AudioCodec::Opus, AudioCodec::Pcmu],
        SrtpPolicy::Optional,
    ));
    let (answer, n) = s.apply_offer(&parse(&fs_offer_savp())).unwrap();
    let re = s.current_sdp();
    assert_eq!(re, answer, "unchanged state => identical SDP, same version");
    // The re-offer is parseable and keeps our codec/crypto/keys.
    let p = parse(&re);
    assert_eq!(p.media[0].fmts[0], n.pt);
    assert_eq!(p.media[0].crypto.len(), 1);
    // With hold applied it becomes a hold offer with the same keys, version+1.
    s.set_local_hold(LocalHold::SendOnly);
    let held = s.current_sdp();
    assert!(has(&held, "a=sendonly"));
    assert_eq!(version_of(&held), version_of(&re) + 1);
    assert_eq!(parse(&held).media[0].crypto, p.media[0].crypto);
    // Peer's answer to this offer is accepted (tag echoes).
    let tag = p.media[0].crypto[0].tag;
    let suite = p.media[0].crypto[0].suite.clone();
    let ans = format!(
        "v=0\r\no=- 1 1 IN IP4 203.0.113.10\r\ns=-\r\nc=IN IP4 203.0.113.10\r\nt=0 0\r\nm=audio 24002 RTP/AVP 111 102\r\na=rtpmap:111 opus/48000/2\r\na=rtpmap:102 telephone-event/48000\r\na=recvonly\r\na=crypto:{tag} {suite} inline:{}\r\n",
        key(SrtpSuite::from_sdp_name(&suite).unwrap())
    );
    let n2 = s.apply_answer(&parse(&ans)).unwrap();
    assert!(n2.srtp.is_some());
    assert!(n2.send_enabled && !n2.recv_enabled);
    // The proto in a re-offer of an answered-AVP+crypto (OSRTP) stays AVP.
    assert_eq!(p.media[0].proto, MediaProto::RtpSavp);
}

#[test]
fn current_sdp_without_negotiation_is_a_fresh_offer() {
    let mut s = session(LocalCaps::default());
    let sdp = s.current_sdp();
    assert!(sdp.contains("m=audio"));
    assert_eq!(version_of(&sdp), 1);
}

#[test]
fn reinvite_changing_codec_changes_body() {
    let mut s = session(caps_with(
        &[AudioCodec::Pcmu, AudioCodec::Pcma],
        SrtpPolicy::Off,
    ));
    let o1 = simple_offer("RTP/AVP", &[]); // PCMU PCMA te
    let (a1, n1) = s.apply_offer(&parse(&o1)).unwrap();
    let o2 = o1.replace("RTP/AVP 0 8 101", "RTP/AVP 8 101");
    let (a2, n2) = s.apply_offer(&parse(&o2)).unwrap();
    assert_eq!(n1.codec, AudioCodec::Pcmu);
    assert_eq!(n2.codec, AudioCodec::Pcma);
    assert_eq!(version_of(&a1) + 1, version_of(&a2));
}

#[test]
fn set_local_address_changes_ports_in_next_sdp() {
    let mut s = session(caps_with(&[AudioCodec::Pcmu], SrtpPolicy::Off));
    s.set_local_address("10.0.0.5".parse().unwrap(), 30000);
    assert_eq!(s.local_addr(), ("10.0.0.5".parse().unwrap(), 30000));
    assert_eq!(s.local_rtcp_port(), 30001);
    let sdp = s.create_offer();
    assert!(has(&sdp, "c=IN IP4 10.0.0.5"));
    assert!(sdp.contains("m=audio 30000 "));
    assert!(has(&sdp, "a=rtcp:30001"));
}

#[test]
fn garbage_offers_never_panic() {
    let bases = [
        fs_offer_avp(),
        fs_offer_savp(),
        yealink_offer(),
        polycom_offer(),
    ];
    for base in bases {
        for end in (0..=base.len()).step_by(3) {
            if !base.is_char_boundary(end) {
                continue;
            }
            if let Ok(sd) = SessionDescription::parse(&base[..end]) {
                let mut s = session(caps_with(
                    &[AudioCodec::Opus, AudioCodec::Pcmu],
                    SrtpPolicy::Optional,
                ));
                let _ = s.apply_offer(&sd);
                let mut s = session(caps_with(
                    &[AudioCodec::Opus, AudioCodec::Pcmu],
                    SrtpPolicy::Mandatory,
                ));
                s.create_offer();
                let _ = s.apply_answer(&sd);
            }
        }
    }
}

// ── review regressions ──────────────────────────────────────────────────

fn opus_offer(extra: &str) -> String {
    format!(
        "v=0\r\no=- 1 1 IN IP4 1.1.1.1\r\ns=-\r\nc=IN IP4 1.1.1.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 111\r\na=rtpmap:111 opus/48000/2\r\n{extra}"
    )
}

#[test]
fn send_ptime_is_encodable_by_the_chosen_codec() {
    // Remote asks Opus for 30 ms: snaps to 20 (encodable), and the encoder accepts it.
    let (a, n) = session(LocalCaps::default())
        .apply_offer(&parse(&opus_offer("a=ptime:30\r\n")))
        .unwrap();
    assert_eq!(n.codec, AudioCodec::Opus);
    assert_eq!(n.ptime_ms, 20);
    assert!(has(&a, "a=ptime:20"));
    let mut enc = crate::codec::make_encoder(AudioCodec::Opus, &Default::default()).unwrap();
    let pcm = vec![0i16; AudioCodec::Opus.samples_per_frame(n.ptime_ms)];
    enc.encode(&pcm, &mut Vec::new()).unwrap();
    // Configured 30 ms for Opus.
    let caps = LocalCaps {
        ptime_ms: Some(30),
        ..Default::default()
    };
    let (_, n) = session(caps).apply_offer(&parse(&opus_offer(""))).unwrap();
    assert_eq!(n.ptime_ms, 20);
    // 50 -> 40 (clamp_opus_ptime),
    let (_, n) = session(LocalCaps::default())
        .apply_offer(&parse(&opus_offer("a=ptime:50\r\n")))
        .unwrap();
    assert_eq!(n.ptime_ms, 40);
    // Capped by a=maxptime (and by the Opus fmtp maxptime).
    let (_, n) = session(LocalCaps::default())
        .apply_offer(&parse(&opus_offer("a=ptime:60\r\na=maxptime:40\r\n")))
        .unwrap();
    assert_eq!(n.ptime_ms, 40);
    let (_, n) = session(LocalCaps::default())
        .apply_offer(&parse(&opus_offer(
            "a=ptime:60\r\na=fmtp:111 maxptime=20\r\n",
        )))
        .unwrap();
    assert_eq!(n.ptime_ms, 20);
    // G.711: any multiple of 10, capped by maxptime.
    let (_, n) = answer_codec(
        &[AudioCodec::Pcmu],
        AnswerOrder::Offer,
        &simple_offer("RTP/AVP", &["a=ptime:50".into(), "a=maxptime:30".into()]),
    )
    .unwrap();
    assert_eq!(n.ptime_ms, 30);
    let (_, n) = answer_codec(
        &[AudioCodec::Pcmu],
        AnswerOrder::Offer,
        &simple_offer("RTP/AVP", &["a=ptime:25".into()]),
    )
    .unwrap();
    assert_eq!(n.ptime_ms % 10, 0);
    // Answer path: Opus answer with ptime 30.
    let mut s = session(LocalCaps::default());
    s.create_offer();
    let ans = "v=0\r\no=- 1 1 IN IP4 1.1.1.1\r\ns=-\r\nc=IN IP4 1.1.1.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 111\r\na=rtpmap:111 opus/48000/2\r\na=ptime:30\r\n";
    assert_eq!(s.apply_answer(&parse(ans)).unwrap().ptime_ms, 20);
}

#[test]
fn crypto_session_params_are_not_silently_accepted() {
    let k = key(SrtpSuite::AesCm128HmacSha1_80);
    let mk = |lines: &[String]| {
        format!(
            "v=0\r\no=- 1 1 IN IP4 1.1.1.1\r\ns=-\r\nc=IN IP4 1.1.1.1\r\nt=0 0\r\nm=audio 4000 RTP/SAVP 0\r\n{}",
            lines.iter().map(|l| format!("{l}\r\n")).collect::<String>()
        )
    };
    let caps = || LocalCaps {
        codecs: vec![AudioCodec::Pcmu],
        srtp: SrtpPolicy::Mandatory,
        ..Default::default()
    };
    for sp in [
        "UNENCRYPTED_SRTP",
        "UNAUTHENTICATED_SRTP",
        "KDR=1",
        "UNENCRYPTED_SRTCP",
        "FEC_ORDER=SRTP_FEC",
        "FEC_KEY=inline:AAAA",
        "WSH=64",
    ] {
        let only = mk(&[format!(
            "a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:{k} {sp}"
        )]);
        let e = session(caps()).apply_offer(&parse(&only)).unwrap_err();
        assert_eq!(e.status_code(), 488, "{sp}");
        // A later clean line wins.
        let two = mk(&[
            format!("a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:{k} {sp}"),
            format!(
                "a=crypto:2 AES_CM_128_HMAC_SHA1_32 inline:{}",
                key(SrtpSuite::AesCm128HmacSha1_32)
            ),
        ]);
        let (a, _) = session(caps()).apply_offer(&parse(&two)).unwrap();
        assert!(
            a.contains("a=crypto:2 AES_CM_128_HMAC_SHA1_32"),
            "{sp}: {a}"
        );
    }
    // KDR=0 is equivalent to none.
    let ok = mk(&[format!(
        "a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:{k} KDR=0"
    )]);
    assert!(session(caps()).apply_offer(&parse(&ok)).is_ok());
    // Answers carrying unsupported params are rejected.
    for sp in ["UNENCRYPTED_SRTP", "KDR=4"] {
        let mut s = session(caps());
        s.create_offer();
        let ans = mk(&[format!(
            "a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:{k} {sp}"
        )]);
        // our offer's key is irrelevant; tag/suite must match what we offered
        let e = s.apply_answer(&parse(&ans)).unwrap_err();
        assert_eq!(e.status_code(), 488, "{sp}");
    }
}

#[test]
fn offerer_uses_its_own_pts_for_receive() {
    let mut s = session(LocalCaps::default());
    let offer = s.create_offer();
    assert!(offer.contains("a=rtpmap:111 opus/48000/2"));
    let ans = "v=0\r\no=- 1 1 IN IP4 1.1.1.1\r\ns=-\r\nc=IN IP4 1.1.1.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 96 97\r\na=rtpmap:96 opus/48000/2\r\na=rtpmap:97 telephone-event/48000\r\n";
    let n = s.apply_answer(&parse(ans)).unwrap();
    // We send with the answer's PTs...
    assert_eq!((n.pt, n.te), (96, Some((97, 48000))));
    // ...and receive on the PTs of our offer.
    assert_eq!(n.rx_pt, 111);
    assert_eq!(n.rx_te, Some((102, 48000)));
    assert_eq!(n.rx_alt_pts, vec![96]);
}

#[test]
fn offerer_rx_cn_and_g711_pts() {
    let caps = LocalCaps {
        codecs: vec![AudioCodec::Pcmu],
        comfort_noise: true,
        ..Default::default()
    };
    let mut s = session(caps);
    s.create_offer();
    let ans = "v=0\r\no=- 1 1 IN IP4 1.1.1.1\r\ns=-\r\nc=IN IP4 1.1.1.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0 13 120\r\na=rtpmap:120 telephone-event/8000\r\n";
    let n = s.apply_answer(&parse(ans)).unwrap();
    assert_eq!((n.pt, n.rx_pt), (0, 0));
    assert_eq!(n.cn_pt, Some(13));
    assert_eq!(n.rx_cn_pt, Some(13));
    assert_eq!(n.te, Some((120, 8000)));
    assert_eq!(n.rx_te, Some((101, 8000)));
    assert!(n.rx_alt_pts.is_empty());
}

#[test]
fn answerer_rx_equals_tx() {
    let (_, n) = answer_codec(&[AudioCodec::Opus], AnswerOrder::Offer, &fs_offer_avp()).unwrap();
    assert_eq!(n.rx_pt, n.pt);
    assert_eq!(n.rx_te, n.te);
    assert_eq!(n.rx_cn_pt, n.cn_pt);
    assert!(n.rx_alt_pts.is_empty());
    // Duplicate PTs for the codec in the offer are accepted as alternates.
    let offer = "v=0\r\no=- 1 1 IN IP4 1.1.1.1\r\ns=-\r\nc=IN IP4 1.1.1.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 111 112\r\na=rtpmap:111 opus/48000/2\r\na=rtpmap:112 opus/48000/2\r\n";
    let (_, n) = answer_codec(&[AudioCodec::Opus], AnswerOrder::Offer, offer).unwrap();
    assert_eq!((n.rx_pt, n.rx_alt_pts.clone()), (111, vec![112]));
}

#[test]
fn zero_connection_sendonly_offer_is_recvonly_answer() {
    let offer = "v=0\r\no=- 1 1 IN IP4 0.0.0.0\r\ns=-\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0\r\na=sendonly\r\n";
    let (a, n) = answer_codec(&[AudioCodec::Pcmu], AnswerOrder::Offer, offer).unwrap();
    assert!(has(&a, "a=recvonly"));
    assert!(n.recv_enabled && !n.send_enabled && n.remote_hold);
}

#[test]
fn answer_uses_the_offered_audio_mline_index() {
    let offer = "v=0\r\no=- 1 1 IN IP4 1.1.1.1\r\ns=-\r\nc=IN IP4 1.1.1.1\r\nt=0 0\r\nm=video 5000 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\nm=audio 4000 RTP/AVP 0\r\n";
    let mut s = session(LocalCaps::default());
    s.apply_offer(&parse(offer)).unwrap();
    // SDP-less re-INVITE: we re-offer the current state, peer mirrors m-line order.
    let reoffer = s.current_sdp();
    assert_eq!(parse(&reoffer).media.len(), 2);
    let ans = "v=0\r\no=- 1 2 IN IP4 1.1.1.1\r\ns=-\r\nc=IN IP4 1.1.1.1\r\nt=0 0\r\nm=video 0 RTP/AVP 96\r\nm=audio 4000 RTP/AVP 0\r\n";
    let n = s.apply_answer(&parse(ans)).unwrap();
    assert_eq!(n.remote_rtp.port(), 4000);
    // The wrong media at that index is malformed.
    let bad = "v=0\r\no=- 1 2 IN IP4 1.1.1.1\r\ns=-\r\nc=IN IP4 1.1.1.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0\r\nm=video 0 RTP/AVP 96\r\n";
    assert_eq!(s.apply_answer(&parse(bad)).unwrap_err().status_code(), 400);
}

#[test]
fn address_family_mismatch_is_488() {
    let offer = "v=0\r\no=- 1 1 IN IP6 2001:db8::5\r\ns=-\r\nc=IN IP6 2001:db8::5\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0\r\n";
    let e = session(LocalCaps::default())
        .apply_offer(&parse(offer))
        .unwrap_err();
    assert_eq!(e.status_code(), 488);
}

#[test]
fn colliding_or_invalid_pts_are_reassigned() {
    let pts_of = |caps: LocalCaps| {
        let o = session(caps).create_offer();
        let m = o.lines().find(|l| l.starts_with("m=")).unwrap().to_string();
        let pts: Vec<String> = m.split(' ').skip(3).map(String::from).collect();
        let mut d = pts.clone();
        d.sort();
        d.dedup();
        assert_eq!(d.len(), pts.len(), "duplicate PT in {m}");
        for p in &pts {
            let n: u8 = p.parse().unwrap();
            assert!(n <= 127);
        }
        (o, pts)
    };
    // te_pt collides with opus_pt.
    let (o, _) = pts_of(LocalCaps {
        opus_pt: 102,
        te_pt: 101,
        ..Default::default()
    });
    assert!(o.contains("opus/48000/2"));
    // te48 (te_pt + 1) collides with opus_pt.
    pts_of(LocalCaps {
        opus_pt: 102,
        te_pt: 101,
        ..Default::default()
    });
    pts_of(LocalCaps {
        opus_pt: 112,
        te_pt: 111,
        ..Default::default()
    });
    // Out of dynamic range / overflow.
    for (op, tp) in [(5u8, 0u8), (200, 255), (127, 127), (96, 95), (111, 127)] {
        let s = session(LocalCaps {
            opus_pt: op,
            te_pt: tp,
            ..Default::default()
        });
        let c = s.caps().clone();
        assert!((96..=127).contains(&c.opus_pt), "{op} {tp}");
        assert!((96..=126).contains(&c.te_pt), "{op} {tp}");
        let all = [c.opus_pt, c.te_pt, c.te_pt + 1];
        assert!(all[0] != all[1] && all[0] != all[2], "{op} {tp}: {all:?}");
        pts_of(LocalCaps {
            opus_pt: op,
            te_pt: tp,
            ..Default::default()
        });
    }
    // Valid values are left alone.
    let s = session(LocalCaps::default());
    assert_eq!((s.caps().opus_pt, s.caps().te_pt), (111, 101));
}

#[test]
fn unparseable_mline_is_malformed_400() {
    let offer = "v=0\r\no=- 1 1 IN IP4 1.1.1.1\r\ns=-\r\nc=IN IP4 1.1.1.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0\r\nm=video notaport RTP/AVP 96\r\n";
    let e = session(LocalCaps::default())
        .apply_offer(&parse(offer))
        .unwrap_err();
    assert_eq!(e.status_code(), 400);
    assert!(matches!(e, NegError::Malformed(_)));
}

#[test]
fn savp_and_avp_audio_mlines_with_srtp_off() {
    let k = key(SrtpSuite::AesCm128HmacSha1_80);
    let offer = format!(
        "v=0\r\no=- 1 1 IN IP4 1.1.1.1\r\ns=-\r\nc=IN IP4 1.1.1.1\r\nt=0 0\r\nm=audio 4000 RTP/SAVP 0\r\na=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:{k}\r\nm=audio 4002 RTP/AVP 0\r\n"
    );
    let (a, n) = answer_codec(&[AudioCodec::Pcmu], AnswerOrder::Offer, &offer).unwrap();
    assert_eq!(n.remote_rtp.port(), 4002);
    let ms: Vec<&str> = lines(&a)
        .into_iter()
        .filter(|l| l.starts_with("m="))
        .collect();
    assert_eq!(
        ms,
        vec![
            "m=audio 0 RTP/SAVP 0".to_string(),
            format!("m=audio {PORT} RTP/AVP 0")
        ]
    );
    // Still 488 if nothing is acceptable.
    let only = format!(
        "v=0\r\no=- 1 1 IN IP4 1.1.1.1\r\ns=-\r\nc=IN IP4 1.1.1.1\r\nt=0 0\r\nm=audio 4000 RTP/SAVP 0\r\na=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:{k}\r\n"
    );
    assert_eq!(
        answer_codec(&[AudioCodec::Pcmu], AnswerOrder::Offer, &only)
            .unwrap_err()
            .status_code(),
        488
    );
}

#[test]
fn origin_address_is_fixed_for_the_session() {
    let mut s = session(caps_with(&[AudioCodec::Pcmu], SrtpPolicy::Off));
    let first = s.create_offer();
    s.set_local_address("10.0.0.5".parse().unwrap(), 30000);
    let second = s.create_offer();
    let o = |sdp: &str| parse(sdp).origin.unwrap();
    assert_eq!(o(&first).addr, IP);
    assert_eq!(o(&second).addr, IP);
    assert_eq!(o(&first).sess_id, o(&second).sess_id);
    assert_eq!(version_of(&first) + 1, version_of(&second));
    assert!(has(&second, "c=IN IP4 10.0.0.5"));
}

#[test]
fn rebind_rewrites_c_and_port_bumps_version_and_keeps_origin() {
    let mut s = session(caps_with(&[AudioCodec::Pcmu], SrtpPolicy::Off));
    let first = s.create_offer();
    let ip6: IpAddr = "2001:db8::7".parse().unwrap();
    s.rebind("10.9.8.7".parse().unwrap(), 40000);
    let second = s.current_sdp();
    assert!(has(&second, "c=IN IP4 10.9.8.7"), "{second}");
    assert!(second.contains("m=audio 40000 "), "{second}");
    assert!(has(&second, "a=rtcp:40001"), "{second}");
    let (o1, o2) = (
        parse(&first).origin.unwrap(),
        parse(&second).origin.unwrap(),
    );
    assert_eq!(o1.sess_id, o2.sess_id);
    assert_eq!(o1.addr, o2.addr, "o= address is fixed");
    assert_eq!(version_of(&first) + 1, version_of(&second));
    // An unchanged re-offer does not bump again.
    assert_eq!(version_of(&second), version_of(&s.current_sdp()));
    // IPv4 -> IPv6: c= follows the family.
    s.rebind(ip6, 40010);
    let third = s.current_sdp();
    assert!(has(&third, "c=IN IP6 2001:db8::7"), "{third}");
    assert_eq!(version_of(&second) + 1, version_of(&third));
}

#[test]
fn rebind_keeps_hold_direction_and_srtp_keys() {
    let mut s = session(caps_with(&[AudioCodec::Pcmu], SrtpPolicy::Optional));
    let first = s.create_offer();
    let crypto = |sdp: &str| -> Vec<String> {
        lines(sdp)
            .into_iter()
            .filter(|l| l.starts_with("a=crypto:"))
            .map(str::to_string)
            .collect()
    };
    assert!(!crypto(&first).is_empty(), "{first}");
    s.set_local_hold(LocalHold::SendOnly);
    let held = s.current_sdp();
    assert!(has(&held, "a=sendonly"), "{held}");
    s.rebind("10.9.8.7".parse().unwrap(), 40000);
    let moved = s.current_sdp();
    assert!(has(&moved, "a=sendonly"), "hold direction kept: {moved}");
    assert!(has(&moved, "c=IN IP4 10.9.8.7"), "{moved}");
    assert_eq!(
        crypto(&first),
        crypto(&moved),
        "SDES keys are not re-generated"
    );
    assert_eq!(s.local_hold(), LocalHold::SendOnly);
}

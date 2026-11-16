//! Header and SDP helpers shared by the call code.

use std::time::Duration;

use rsipstack::sip::{Header, Headers};
use siple_config::{AccountConfig, MediaConfig};
use siple_media::codec::AudioCodec;
use siple_media::sdp::SessionDescription;
use siple_media::sdp_neg::{AnswerOrder, LocalCaps, SrtpPolicy as NegPolicy};
use siple_media::srtp::SrtpSuite as NegSuite;

use crate::types::ALLOW;

/// All values of the header called `name` (case-insensitive), one entry per header line.
pub(crate) fn header_values(headers: &Headers, name: &str) -> Vec<String> {
    headers
        .iter()
        .filter(|h| h.name().eq_ignore_ascii_case(name))
        .map(|h| h.value().trim().to_string())
        .collect()
}

/// First value of the header called `name`.
pub(crate) fn header_first(headers: &Headers, name: &str) -> Option<String> {
    header_values(headers, name).into_iter().next()
}

/// Whether a comma separated header (`Require`, `Supported`, ...) holds `token`.
pub(crate) fn has_token(headers: &Headers, name: &str, token: &str) -> bool {
    header_values(headers, name)
        .iter()
        .flat_map(|v| v.split(','))
        .any(|t| t.trim().eq_ignore_ascii_case(token))
}

/// All tokens of a comma separated header, lower-cased and without parameters.
pub(crate) fn tokens(headers: &Headers, name: &str) -> Vec<String> {
    header_values(headers, name)
        .iter()
        .flat_map(|v| v.split(','))
        .map(|t| {
            t.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        })
        .filter(|t| !t.is_empty())
        .collect()
}

/// Splits a `name-addr` / `addr-spec` header value into `(display name, URI)`.
/// `"Alice" <sip:a@x>;tag=1` gives `(Some("Alice"), "sip:a@x")`.
pub(crate) fn parse_name_addr(v: &str) -> (Option<String>, String) {
    let v = v.trim();
    if let (Some(lt), Some(gt)) = (v.find('<'), v.find('>'))
        && lt < gt
    {
        let display = v[..lt].trim().trim_matches('"').trim();
        let uri = v[lt + 1..gt].trim().to_string();
        return ((!display.is_empty()).then(|| display.to_string()), uri);
    }
    let uri = v.split(';').next().unwrap_or("").trim().to_string();
    (None, uri)
}

/// Parsed RFC 3326 `Reason` header.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Reason {
    /// Protocol token (`SIP`, `Q.850`), upper-cased; `None` for a value that is not RFC 3326.
    pub protocol: Option<String>,
    pub cause: Option<u16>,
    pub text: Option<String>,
}

impl Reason {
    /// The cause when the protocol is `SIP` and it is a status code.
    pub(crate) fn sip_cause(&self) -> Option<u16> {
        match (&self.protocol, self.cause) {
            (Some(p), Some(c)) if p == "SIP" && (100..700).contains(&c) => Some(c),
            _ => None,
        }
    }

    /// The cause when the protocol is `Q.850`.
    pub(crate) fn q850_cause(&self) -> Option<u16> {
        match (&self.protocol, self.cause) {
            (Some(p), Some(c)) if p == "Q.850" => Some(c),
            _ => None,
        }
    }
}

/// `Reason: SIP;cause=200;text="Call completed elsewhere"`. A value that does not follow
/// RFC 3326 is returned verbatim as the text.
pub(crate) fn parse_reason(v: &str) -> Reason {
    let mut out = Reason::default();
    let mut structured = false;
    for (i, p) in v.split(';').enumerate() {
        let p = p.trim();
        if let Some(c) = p.strip_prefix("cause=") {
            out.cause = c.trim().parse::<u16>().ok();
            structured = true;
        } else if let Some(t) = p.strip_prefix("text=") {
            out.text = Some(t.trim().trim_matches('"').to_string());
            structured = true;
        } else if i == 0 && !p.contains('=') && !p.is_empty() {
            out.protocol = Some(p.to_ascii_uppercase());
        }
    }
    if !structured && !v.trim().is_empty() {
        out.protocol = None;
        out.text = Some(v.trim().to_string());
    }
    out
}

/// Seconds from `answer-after=N` parameters, if present.
fn answer_after_secs(v: &str) -> Option<u64> {
    let lower = v.to_ascii_lowercase();
    let i = lower.find("answer-after")?;
    let rest = lower[i + "answer-after".len()..].trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Intercom request of an INVITE: `Call-Info: ...;answer-after=N` or an auto-answer
/// `Alert-Info`. Returns the delay before answering.
pub(crate) fn intercom_delay(headers: &Headers) -> Option<Duration> {
    for v in header_values(headers, "call-info") {
        if let Some(n) = answer_after_secs(&v) {
            return Some(Duration::from_secs(n));
        }
    }
    for v in header_values(headers, "alert-info") {
        let l = v.to_ascii_lowercase();
        if let Some(n) = answer_after_secs(&v) {
            return Some(Duration::from_secs(n));
        }
        if l.contains("auto answer")
            || l.contains("auto-answer")
            || l.contains("autoanswer")
            || l.contains("answer-now")
        {
            return Some(Duration::ZERO);
        }
    }
    None
}

/// Who refreshes a session (RFC 4028 `refresher` parameter; `uac` / `uas` refer to the roles in
/// the dialog-creating INVITE).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refresher {
    Uac,
    Uas,
}

impl Refresher {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Uac => "uac",
            Self::Uas => "uas",
        }
    }
}

/// `Session-Expires: <delta>[;refresher=uac|uas]` of a message.
pub(crate) fn parse_session_expires(headers: &Headers) -> Option<(u32, Option<Refresher>)> {
    // `x` is the compact form (RFC 4028 §3).
    let v = header_first(headers, "session-expires").or_else(|| header_first(headers, "x"))?;
    let mut parts = v.split(';');
    let delta = parts.next()?.trim().parse::<u32>().ok()?;
    let refresher = parts.find_map(|p| {
        let (k, val) = p.trim().split_once('=')?;
        if !k.trim().eq_ignore_ascii_case("refresher") {
            return None;
        }
        match val.trim().to_ascii_lowercase().as_str() {
            "uac" => Some(Refresher::Uac),
            "uas" => Some(Refresher::Uas),
            _ => None,
        }
    });
    Some((delta, refresher))
}

/// `Min-SE` of a message, in seconds.
pub(crate) fn parse_min_se(headers: &Headers) -> Option<u32> {
    header_first(headers, "min-se")?
        .split(';')
        .next()?
        .trim()
        .parse()
        .ok()
}

/// Whether the message's `Allow` header lists `method`.
pub(crate) fn allows(headers: &Headers, method: &str) -> bool {
    has_token(headers, "allow", method)
}

/// Headers that go on every SDP-carrying response / request.
pub(crate) fn sdp_headers(cfg: &AccountConfig) -> Vec<Header> {
    vec![
        Header::ContentType("application/sdp".into()),
        Header::Allow(ALLOW.into()),
        Header::Supported(cfg.supported_tokens().into()),
    ]
}

/// Result of looking for an SDP body.
pub(crate) enum SdpBody {
    /// No body.
    None,
    /// A parsed SDP.
    Sdp(String, Box<SessionDescription>),
    /// Body with an unsupported content type.
    WrongType,
    /// SDP that does not parse.
    Malformed(String),
}

pub(crate) fn sdp_body(headers: &Headers, body: &[u8]) -> SdpBody {
    if body.is_empty() {
        return SdpBody::None;
    }
    let ct = header_first(headers, "content-type").unwrap_or_default();
    if !ct
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .eq_ignore_ascii_case("application/sdp")
    {
        return SdpBody::WrongType;
    }
    let text = String::from_utf8_lossy(body).into_owned();
    match SessionDescription::parse(&text) {
        Ok(sd) => SdpBody::Sdp(text, Box::new(sd)),
        Err(e) => SdpBody::Malformed(e.to_string()),
    }
}

fn codec_of(c: siple_config::Codec) -> AudioCodec {
    match c {
        siple_config::Codec::Opus => AudioCodec::Opus,
        siple_config::Codec::G722 => AudioCodec::G722,
        siple_config::Codec::Pcmu => AudioCodec::Pcmu,
        siple_config::Codec::Pcma => AudioCodec::Pcma,
    }
}

fn policy_of(p: siple_config::SrtpPolicy) -> NegPolicy {
    match p {
        siple_config::SrtpPolicy::Off => NegPolicy::Off,
        siple_config::SrtpPolicy::Optional => NegPolicy::Optional,
        siple_config::SrtpPolicy::Mandatory => NegPolicy::Mandatory,
    }
}

fn suite_of(s: siple_config::SrtpSuite) -> NegSuite {
    match s {
        siple_config::SrtpSuite::Aes128Cm80 => NegSuite::AesCm128HmacSha1_80,
        siple_config::SrtpSuite::Aes128Cm32 => NegSuite::AesCm128HmacSha1_32,
        siple_config::SrtpSuite::Aes256Cm80 => NegSuite::Aes256CmHmacSha1_80,
        siple_config::SrtpSuite::Aes256Cm32 => NegSuite::Aes256CmHmacSha1_32,
    }
}

/// SDP capabilities of a call from the effective account / media configuration.
pub(crate) fn local_caps(
    cfg: &AccountConfig,
    media: &MediaConfig,
    codecs: Option<&[siple_config::Codec]>,
    srtp: Option<siple_config::SrtpPolicy>,
) -> LocalCaps {
    let list = codecs.unwrap_or_else(|| cfg.effective_codecs(media));
    LocalCaps {
        codecs: list.iter().copied().map(codec_of).collect(),
        telephone_event: true,
        comfort_noise: media.comfort_noise,
        srtp: policy_of(srtp.unwrap_or(cfg.srtp)),
        srtp_suites: cfg.srtp_suites.iter().copied().map(suite_of).collect(),
        rtcp_mux: media.rtcp_mux,
        ptime_ms: None,
        answer_order: match media.codec_answer_order {
            siple_config::CodecAnswerOrder::Offer => AnswerOrder::Offer,
            siple_config::CodecAnswerOrder::Local => AnswerOrder::Local,
        },
        ..LocalCaps::default()
    }
}

/// CSeq number of a response.
pub(crate) fn resp_cseq(resp: &rsipstack::sip::Response) -> Option<u32> {
    use rsipstack::sip::prelude::HeadersExt;
    resp.cseq_header().ok()?.seq().ok()
}

/// CSeq number of a request.
pub(crate) fn req_cseq(req: &rsipstack::sip::Request) -> Option<u32> {
    use rsipstack::sip::prelude::HeadersExt;
    req.cseq_header().ok()?.seq().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_addr_forms() {
        assert_eq!(
            parse_name_addr("\"Alice A\" <sip:a@x.com>;tag=1"),
            (Some("Alice A".into()), "sip:a@x.com".into())
        );
        assert_eq!(
            parse_name_addr("<sip:a@x.com>"),
            (None, "sip:a@x.com".into())
        );
        assert_eq!(
            parse_name_addr("sip:a@x.com;party=calling"),
            (None, "sip:a@x.com".into())
        );
        assert_eq!(
            parse_name_addr("Bob <sip:b@x.com>"),
            (Some("Bob".into()), "sip:b@x.com".into())
        );
    }

    #[test]
    fn reason_header() {
        let r = parse_reason("SIP;cause=200;text=\"Call completed elsewhere\"");
        assert_eq!(r.sip_cause(), Some(200));
        assert_eq!(r.q850_cause(), None);
        assert_eq!(r.text.as_deref(), Some("Call completed elsewhere"));
        let r = parse_reason("Q.850;cause=16;text=\"NORMAL_CLEARING\"");
        assert_eq!((r.sip_cause(), r.q850_cause()), (None, Some(16)));
        assert_eq!(r.text.as_deref(), Some("NORMAL_CLEARING"));
        let r = parse_reason("busy");
        assert_eq!(
            (r.sip_cause(), r.q850_cause(), r.text.as_deref()),
            (None, None, Some("busy"))
        );
    }

    #[test]
    fn intercom_detection() {
        let h =
            |name: &str, v: &str| -> Headers { vec![Header::Other(name.into(), v.into())].into() };
        assert_eq!(
            intercom_delay(&h("Call-Info", "<sip:x@y>;answer-after=3")),
            Some(Duration::from_secs(3))
        );
        assert_eq!(
            intercom_delay(&h("Alert-Info", "<http://x>;info=alert-autoanswer")),
            Some(Duration::ZERO)
        );
        assert_eq!(
            intercom_delay(&h("Alert-Info", "Auto Answer")),
            Some(Duration::ZERO)
        );
        assert_eq!(
            intercom_delay(&h("Alert-Info", "<http://x>;info=ringtone1")),
            None
        );
        assert_eq!(intercom_delay(&Headers::default()), None);
    }

    #[test]
    fn session_timer_header_parsing() {
        let h: Headers = vec![
            Header::Other("Session-Expires".into(), "1800;refresher=UAS".into()),
            Header::Other("Min-SE".into(), "90".into()),
            Header::Other("Allow".into(), "INVITE, update, BYE".into()),
        ]
        .into();
        assert_eq!(
            parse_session_expires(&h),
            Some((1800, Some(Refresher::Uas)))
        );
        assert_eq!(parse_min_se(&h), Some(90));
        assert!(allows(&h, "UPDATE") && !allows(&h, "PRACK"));
        let h: Headers = vec![Header::Other("Session-Expires".into(), "600".into())].into();
        assert_eq!(parse_session_expires(&h), Some((600, None)));
        let h: Headers = vec![Header::Other("Session-Expires".into(), "x".into())].into();
        assert_eq!(parse_session_expires(&h), None);
        assert_eq!(parse_session_expires(&Headers::default()), None);
        assert_eq!(parse_min_se(&Headers::default()), None);
    }
}

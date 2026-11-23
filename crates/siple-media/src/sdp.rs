//! SDP parsing and data model (RFC 8866 subset, tolerant).
//!
//! The parser never panics: unknown or malformed lines are skipped (a
//! malformed `m=` line becomes a port-0 placeholder media section so that its
//! attribute lines do not leak into the previous section). Both CRLF and LF
//! line endings are accepted, as is stray whitespace.

use std::collections::BTreeMap;
use std::net::IpAddr;

/// Failure to find any SDP content at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SdpError(pub String);

impl std::fmt::Display for SdpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid SDP: {}", self.0)
    }
}
impl std::error::Error for SdpError {}

/// Media direction attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Direction {
    #[default]
    SendRecv,
    SendOnly,
    RecvOnly,
    Inactive,
}

impl Direction {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "sendrecv" => Some(Self::SendRecv),
            "sendonly" => Some(Self::SendOnly),
            "recvonly" => Some(Self::RecvOnly),
            "inactive" => Some(Self::Inactive),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SendRecv => "sendrecv",
            Self::SendOnly => "sendonly",
            Self::RecvOnly => "recvonly",
            Self::Inactive => "inactive",
        }
    }

    /// Whether the party that wrote this direction sends media.
    pub fn sends(&self) -> bool {
        matches!(self, Self::SendRecv | Self::SendOnly)
    }

    /// Whether the party that wrote this direction receives media.
    pub fn receives(&self) -> bool {
        matches!(self, Self::SendRecv | Self::RecvOnly)
    }

    pub fn from_flags(send: bool, recv: bool) -> Self {
        match (send, recv) {
            (true, true) => Self::SendRecv,
            (true, false) => Self::SendOnly,
            (false, true) => Self::RecvOnly,
            (false, false) => Self::Inactive,
        }
    }
}

/// `o=` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub username: String,
    pub sess_id: String,
    pub sess_version: String,
    /// Origin address as written (may be a hostname).
    pub addr: String,
}

/// Transport protocol of an `m=` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaProto {
    RtpAvp,
    RtpSavp,
    RtpAvpf,
    RtpSavpf,
    Other(String),
}

impl MediaProto {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_uppercase().as_str() {
            "RTP/AVP" => Self::RtpAvp,
            "RTP/SAVP" => Self::RtpSavp,
            "RTP/AVPF" => Self::RtpAvpf,
            "RTP/SAVPF" => Self::RtpSavpf,
            _ => Self::Other(s.to_string()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::RtpAvp => "RTP/AVP",
            Self::RtpSavp => "RTP/SAVP",
            Self::RtpAvpf => "RTP/AVPF",
            Self::RtpSavpf => "RTP/SAVPF",
            Self::Other(s) => s,
        }
    }

    /// Secure profile (SAVP / SAVPF).
    pub fn is_secure(&self) -> bool {
        matches!(self, Self::RtpSavp | Self::RtpSavpf)
    }

    /// Any of the four plain/secure RTP profiles.
    pub fn is_rtp(&self) -> bool {
        !matches!(self, Self::Other(_))
    }
}

/// `a=rtpmap` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtpMap {
    pub name: String,
    pub clock: u32,
    pub channels: Option<u8>,
}

/// `a=rtcp:` attribute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtcpAttr {
    pub port: u16,
    pub addr: Option<IpAddr>,
}

/// `a=crypto:` line (RFC 4568).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CryptoLine {
    pub tag: u32,
    /// Suite name exactly as written.
    pub suite: String,
    /// Key params, split on `;` (each e.g. `inline:KEY|2^31`).
    pub key_params: Vec<String>,
    /// Trailing session params (e.g. `UNENCRYPTED_SRTP`).
    pub session_params: Vec<String>,
}

impl CryptoLine {
    /// First `inline:` key-params entry.
    pub fn first_inline(&self) -> Option<&str> {
        self.key_params
            .iter()
            .map(|s| s.as_str())
            .find(|s| s.starts_with("inline:"))
    }
}

/// Generic attribute kept verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attr {
    pub name: String,
    pub value: Option<String>,
}

/// One `m=` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaDescription {
    /// `audio`, `video`, `application`, ...
    pub kind: String,
    pub port: u16,
    pub proto: MediaProto,
    /// Format tokens as written (also valid for non-RTP protocols).
    pub fmts_raw: Vec<String>,
    /// Numeric payload types (RTP profiles only; non-numeric tokens dropped).
    pub fmts: Vec<u8>,
    /// `c=` override at media level.
    pub connection: Option<IpAddr>,
    pub rtpmap: BTreeMap<u8, RtpMap>,
    /// Raw fmtp parameter strings per PT.
    pub fmtp: BTreeMap<u8, String>,
    pub ptime: Option<u32>,
    pub maxptime: Option<u32>,
    /// Effective direction (session-level direction inherited, default sendrecv).
    pub direction: Direction,
    /// True if a direction attribute appeared at media level.
    pub direction_explicit: bool,
    pub rtcp: Option<RtcpAttr>,
    pub rtcp_mux: bool,
    pub crypto: Vec<CryptoLine>,
    /// Everything else, in order.
    pub attrs: Vec<Attr>,
}

impl MediaDescription {
    fn placeholder() -> Self {
        Self {
            kind: String::new(),
            port: 0,
            proto: MediaProto::Other(String::new()),
            fmts_raw: Vec::new(),
            fmts: Vec::new(),
            connection: None,
            rtpmap: BTreeMap::new(),
            fmtp: BTreeMap::new(),
            ptime: None,
            maxptime: None,
            direction: Direction::SendRecv,
            direction_explicit: false,
            rtcp: None,
            rtcp_mux: false,
            crypto: Vec::new(),
            attrs: Vec::new(),
        }
    }

    pub fn is_audio(&self) -> bool {
        self.kind.eq_ignore_ascii_case("audio")
    }

    /// rtpmap for `pt`, falling back to RFC 3551 static defaults
    /// (0 PCMU, 8 PCMA, 9 G722, 13 CN, plus a few others).
    pub fn rtpmap_for(&self, pt: u8) -> Option<RtpMap> {
        if let Some(m) = self.rtpmap.get(&pt) {
            return Some(m.clone());
        }
        let (name, clock) = match pt {
            0 => ("PCMU", 8000),
            3 => ("GSM", 8000),
            4 => ("G723", 8000),
            8 => ("PCMA", 8000),
            9 => ("G722", 8000),
            13 => ("CN", 8000),
            18 => ("G729", 8000),
            _ => return None,
        };
        Some(RtpMap {
            name: name.to_string(),
            clock,
            channels: None,
        })
    }

    /// Value of fmtp parameter `key` (case-insensitive) for `pt`.
    pub fn fmtp_param(&self, pt: u8, key: &str) -> Option<&str> {
        self.fmtp.get(&pt)?.split(';').find_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            k.trim().eq_ignore_ascii_case(key).then_some(v.trim())
        })
    }

    fn add_attr(&mut self, name: &str, value: Option<&str>) {
        let lname = name.to_ascii_lowercase();
        match (lname.as_str(), value) {
            ("rtpmap", Some(v)) => {
                if let Some((pt, map)) = parse_rtpmap(v) {
                    self.rtpmap.insert(pt, map);
                }
            }
            ("fmtp", Some(v)) => {
                if let Some((pt, rest)) = v.trim().split_once(char::is_whitespace)
                    && let Ok(pt) = pt.parse::<u8>()
                {
                    self.fmtp.insert(pt, rest.trim().to_string());
                }
            }
            ("ptime", Some(v)) => self.ptime = v.trim().parse().ok().filter(|&p| p > 0),
            ("maxptime", Some(v)) => self.maxptime = v.trim().parse().ok().filter(|&p| p > 0),
            ("sendrecv" | "sendonly" | "recvonly" | "inactive", _) => {
                if let Some(d) = Direction::parse(&lname) {
                    self.direction = d;
                    self.direction_explicit = true;
                }
            }
            ("rtcp-mux", _) => self.rtcp_mux = true,
            ("rtcp", Some(v)) => {
                let mut it = v.split_whitespace();
                if let Some(port) = it.next().and_then(|p| p.parse::<u16>().ok()) {
                    // optional "IN IP4 addr"
                    let addr = it.nth(2).and_then(|a| a.parse::<IpAddr>().ok());
                    self.rtcp = Some(RtcpAttr { port, addr });
                }
            }
            ("crypto", Some(v)) => {
                if let Some(c) = parse_crypto(v) {
                    self.crypto.push(c);
                }
            }
            _ => self.attrs.push(Attr {
                name: name.to_string(),
                value: value.map(str::to_string),
            }),
        }
    }
}

/// Whole session description.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionDescription {
    pub origin: Option<Origin>,
    /// Session-level `c=` address.
    pub connection: Option<IpAddr>,
    /// Session-level direction attribute, if any.
    pub direction: Option<Direction>,
    pub media: Vec<MediaDescription>,
    /// Session-level attributes not otherwise modeled.
    pub attrs: Vec<Attr>,
}

impl SessionDescription {
    /// Parse an SDP body. Fails only if there is no recognizable SDP at all
    /// (no `v=` line and no `m=` line).
    pub fn parse(input: &str) -> Result<Self, SdpError> {
        let mut sd = SessionDescription::default();
        let mut saw_v = false;
        let mut cur: Option<MediaDescription> = None;

        for raw in input.split(['\n', '\r']) {
            let line = raw.trim();
            let b = line.as_bytes();
            if b.len() < 2 || b[1] != b'=' {
                continue;
            }
            let val = line[2..].trim();
            match b[0] {
                b'v' => saw_v = true,
                b'o' => {
                    if sd.origin.is_none() {
                        sd.origin = parse_origin(val);
                    }
                }
                b'c' => {
                    let ip = parse_connection(val);
                    match cur.as_mut() {
                        Some(m) => m.connection = ip.or(m.connection),
                        None => sd.connection = ip.or(sd.connection),
                    }
                }
                b'm' => {
                    if let Some(m) = cur.take() {
                        sd.media.push(m);
                    }
                    cur = Some(parse_media_line(val));
                }
                b'a' => {
                    let (name, value) = match val.split_once(':') {
                        Some((n, v)) => (n.trim(), Some(v.trim())),
                        None => (val, None),
                    };
                    if name.is_empty() {
                        continue;
                    }
                    match cur.as_mut() {
                        Some(m) => m.add_attr(name, value),
                        None => {
                            let l = name.to_ascii_lowercase();
                            if let Some(d) = Direction::parse(&l) {
                                sd.direction = Some(d);
                            } else {
                                sd.attrs.push(Attr {
                                    name: name.to_string(),
                                    value: value.map(str::to_string),
                                });
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(m) = cur.take() {
            sd.media.push(m);
        }
        if !saw_v && sd.media.is_empty() {
            return Err(SdpError("no v= or m= line".into()));
        }
        if let Some(sdir) = sd.direction {
            for m in sd.media.iter_mut().filter(|m| !m.direction_explicit) {
                m.direction = sdir;
            }
        }
        Ok(sd)
    }

    /// Effective connection address of `media` (media-level override first).
    pub fn connection_for(&self, media: &MediaDescription) -> Option<IpAddr> {
        media.connection.or(self.connection)
    }
}

fn parse_origin(v: &str) -> Option<Origin> {
    let t: Vec<&str> = v.split_whitespace().collect();
    if t.len() < 6 {
        return None;
    }
    Some(Origin {
        username: t[0].to_string(),
        sess_id: t[1].to_string(),
        sess_version: t[2].to_string(),
        addr: t[5].to_string(),
    })
}

fn parse_connection(v: &str) -> Option<IpAddr> {
    let t: Vec<&str> = v.split_whitespace().collect();
    if t.len() < 3 || !t[0].eq_ignore_ascii_case("IN") {
        return None;
    }
    t[2].split('/').next()?.parse().ok()
}

fn parse_media_line(v: &str) -> MediaDescription {
    let mut m = MediaDescription::placeholder();
    let t: Vec<&str> = v.split_whitespace().collect();
    if t.len() < 3 {
        return m;
    }
    let Some(port) = t[1].split('/').next().and_then(|p| p.parse::<u16>().ok()) else {
        return m;
    };
    m.kind = t[0].to_ascii_lowercase();
    m.port = port;
    m.proto = MediaProto::parse(t[2]);
    m.fmts_raw = t[3..].iter().map(|s| s.to_string()).collect();
    if m.proto.is_rtp() {
        m.fmts = m
            .fmts_raw
            .iter()
            .filter_map(|s| s.parse::<u8>().ok())
            .collect();
    }
    m
}

fn parse_rtpmap(v: &str) -> Option<(u8, RtpMap)> {
    let (pt, rest) = v.trim().split_once(char::is_whitespace)?;
    let pt: u8 = pt.parse().ok()?;
    let mut it = rest.trim().split('/');
    let name = it.next()?.trim();
    let clock: u32 = it.next()?.trim().parse().ok()?;
    if name.is_empty() || clock == 0 {
        return None;
    }
    let channels = it.next().and_then(|c| c.trim().parse().ok());
    Some((
        pt,
        RtpMap {
            name: name.to_string(),
            clock,
            channels,
        },
    ))
}

fn parse_crypto(v: &str) -> Option<CryptoLine> {
    let mut it = v.split_whitespace();
    let tag: u32 = it.next()?.parse().ok()?;
    let suite = it.next()?.to_string();
    let keys = it.next()?;
    let key_params: Vec<String> = keys
        .split(';')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if key_params.is_empty() {
        return None;
    }
    Some(CryptoLine {
        tag,
        suite,
        key_params,
        session_params: it.map(str::to_string).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS_OFFER: &str = "v=0\r\no=FreeSWITCH 1700000000 1700000001 IN IP4 203.0.113.10\r\ns=FreeSWITCH\r\nc=IN IP4 203.0.113.10\r\nt=0 0\r\nm=audio 20000 RTP/AVP 111 9 0 8 102 101\r\na=rtpmap:111 opus/48000/2\r\na=fmtp:111 useinbandfec=1;stereo=0\r\na=rtpmap:102 telephone-event/48000\r\na=rtpmap:101 telephone-event/8000\r\na=fmtp:101 0-16\r\na=ptime:20\r\na=sendrecv\r\na=crypto:1 AES_CM_256_HMAC_SHA1_80 inline:AAAA|2^31 UNENCRYPTED_SRTCP\r\nb=AS:64\r\nm=video 0 RTP/AVP 96\r\n";

    #[test]
    fn parses_full_offer() {
        let sd = SessionDescription::parse(FS_OFFER).unwrap();
        let o = sd.origin.as_ref().unwrap();
        assert_eq!(o.username, "FreeSWITCH");
        assert_eq!(o.sess_version, "1700000001");
        assert_eq!(sd.connection, Some("203.0.113.10".parse().unwrap()));
        assert_eq!(sd.media.len(), 2);
        let a = &sd.media[0];
        assert_eq!(a.port, 20000);
        assert_eq!(a.proto, MediaProto::RtpAvp);
        assert_eq!(a.fmts, vec![111, 9, 0, 8, 102, 101]);
        assert_eq!(a.rtpmap[&111].channels, Some(2));
        assert_eq!(a.rtpmap_for(9).unwrap().name, "G722");
        assert_eq!(a.fmtp_param(111, "UseInbandFec"), Some("1"));
        assert_eq!(a.ptime, Some(20));
        assert_eq!(a.direction, Direction::SendRecv);
        assert_eq!(a.crypto.len(), 1);
        assert_eq!(a.crypto[0].suite, "AES_CM_256_HMAC_SHA1_80");
        assert_eq!(a.crypto[0].first_inline(), Some("inline:AAAA|2^31"));
        assert_eq!(a.crypto[0].session_params, vec!["UNENCRYPTED_SRTCP"]);
        assert_eq!(sd.media[1].kind, "video");
        assert_eq!(sd.media[1].port, 0);
    }

    #[test]
    fn lf_only_whitespace_and_session_direction() {
        let s = "v=0\no=- 1 1 IN IP4 1.1.1.1 \n  c=IN IP4 1.1.1.1\na=sendonly\nm=audio 4000 RTP/SAVPF 0\n a=rtcp-mux \nm=audio 4002 RTP/AVP 8\na=recvonly\n";
        let sd = SessionDescription::parse(s).unwrap();
        assert_eq!(sd.direction, Some(Direction::SendOnly));
        assert_eq!(sd.media[0].direction, Direction::SendOnly);
        assert!(sd.media[0].rtcp_mux);
        assert_eq!(sd.media[0].proto, MediaProto::RtpSavpf);
        assert_eq!(sd.media[1].direction, Direction::RecvOnly);
    }

    #[test]
    fn rtcp_attr_and_connection_override() {
        let s = "v=0\r\nc=IN IP4 1.1.1.1\r\nm=audio 4000 RTP/AVP 0\r\nc=IN IP6 ::1\r\na=rtcp:4010 IN IP4 9.9.9.9\r\n";
        let sd = SessionDescription::parse(s).unwrap();
        let m = &sd.media[0];
        assert_eq!(sd.connection_for(m), Some("::1".parse().unwrap()));
        let r = m.rtcp.as_ref().unwrap();
        assert_eq!(r.port, 4010);
        assert_eq!(r.addr, Some("9.9.9.9".parse().unwrap()));
    }

    #[test]
    fn static_defaults_and_case_insensitive_names() {
        let s = "v=0\r\nm=audio 4000 RTP/AVP 0 8 9 13 96\r\na=rtpmap:96 OPUS/48000/2\r\n";
        let m = &SessionDescription::parse(s).unwrap().media[0];
        assert_eq!(m.rtpmap_for(0).unwrap().name, "PCMU");
        assert_eq!(m.rtpmap_for(8).unwrap().name, "PCMA");
        assert_eq!(m.rtpmap_for(9).unwrap().clock, 8000);
        assert_eq!(m.rtpmap_for(13).unwrap().name, "CN");
        assert_eq!(m.rtpmap_for(96).unwrap().name, "OPUS");
        assert!(m.rtpmap_for(97).is_none());
    }

    #[test]
    fn malformed_media_line_does_not_pollute_previous() {
        let s = "v=0\r\nm=audio 4000 RTP/AVP 0\r\nm=audio notaport RTP/AVP 8\r\na=sendonly\r\n";
        let sd = SessionDescription::parse(s).unwrap();
        assert_eq!(sd.media.len(), 2);
        assert_eq!(sd.media[0].direction, Direction::SendRecv);
        assert_eq!(sd.media[1].port, 0);
    }

    #[test]
    fn rejects_empty_and_garbage() {
        assert!(SessionDescription::parse("").is_err());
        assert!(SessionDescription::parse("hello world\n\n\u{0}\u{1}").is_err());
    }

    #[test]
    fn never_panics_on_truncation_and_garbage() {
        for end in 0..=FS_OFFER.len() {
            if FS_OFFER.is_char_boundary(end) {
                let _ = SessionDescription::parse(&FS_OFFER[..end]);
            }
        }
        let junk = [
            "m=",
            "m=audio",
            "m=audio 99999 RTP/AVP 0",
            "m=audio -1 RTP/AVP 0",
            "m=audio 1/2/3 RTP/AVP 999 abc",
            "a=",
            "a=:",
            "a=rtpmap:",
            "a=rtpmap:999 x/y",
            "a=rtpmap:0 PCMU/0",
            "a=fmtp:",
            "a=fmtp:x y",
            "a=crypto:",
            "a=crypto:1",
            "a=crypto:x y z",
            "a=rtcp:99999",
            "a=ptime:-1",
            "c=",
            "c=IN",
            "c=IN IP4 x/y/z",
            "o=",
            "é=é",
            "é",
            "=",
            "a=é:é",
        ];
        for j in junk {
            let s = format!("v=0\r\nm=audio 4000 RTP/AVP 0\r\n{j}\r\n");
            let _ = SessionDescription::parse(&s);
            let _ = SessionDescription::parse(j);
        }
        // pseudo-random bytes
        let mut x: u32 = 12345;
        for _ in 0..200 {
            let mut s = String::from("v=0\r\nm=audio 1 RTP/AVP 0\r\n");
            for _ in 0..80 {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                let c = b"a=: \nm/1|;"[(x % 10) as usize];
                s.push(c as char);
            }
            let _ = SessionDescription::parse(&s);
        }
    }
}

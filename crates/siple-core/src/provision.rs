//! `siple:provision` setup links: parsing, building and the `provision.*` API methods.
//!
//! A setup link carries the non-secret part of an account so an administrator can hand it to
//! users. It is untrusted input: the parser accepts exactly nine account fields
//! ([`LINK_FIELDS`]), refuses everything else with an error (never silently dropping a key),
//! never carries or accepts a password, and never echoes a submitted value in an error message.
//! Nothing here contacts a server or writes a file; the GUI turns a parsed link into a Review
//! screen and the user decides step by step (see the setup-links guide).
//!
//! Format: `siple:provision?d=<base64url(JSON)>` with JSON
//! `{"v":1,"account":{"username":"1001","domain":"example.test",...}}`.

use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::backend::AppBackend;
use crate::config::edit::{AccountDraft, AccountField, EditValue, suggested_account_id};
use crate::config::{AccountConfig, Config, Profile, SrtpPolicy, Transport};
use crate::control::methods::parse_params;
use crate::control::{ApiError, MethodInfo};
use crate::uri::{check_chars, host_ok, user_ok};

/// The only schema version this build reads.
pub const VERSION: u64 = 1;
/// Longest whole link accepted (checked before anything is decoded), in bytes.
pub const MAX_LINK_LEN: usize = 2048;
/// Longest decoded JSON accepted, in bytes.
pub const MAX_JSON_LEN: usize = 1536;
/// What every setup link starts with.
pub const PREFIX: &str = "siple:provision?d=";

/// The account fields a link may carry (required: `username`, `domain`). The single source for
/// docs and schemas.
pub const LINK_FIELDS: [AccountField; 9] = [
    AccountField::Username,
    AccountField::Domain,
    AccountField::DisplayName,
    AccountField::AuthUsername,
    AccountField::Registrar,
    AccountField::OutboundProxy,
    AccountField::Transport,
    AccountField::Srtp,
    AccountField::Profile,
];

const MAX_USER: usize = 64;
const MAX_DISPLAY_CHARS: usize = 64;
const MAX_DISPLAY_BYTES: usize = 256;

/// Why a setup link was refused. Messages are fixed text; they never contain a submitted value
/// (a key name appears only if it looks like a plain config key).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupLinkError {
    /// Longer than [`MAX_LINK_LEN`].
    TooLong,
    /// Not a `siple:` link.
    NotASetupLink,
    /// A `siple:` link with an action other than `provision`.
    UnsupportedAction,
    /// The query is not exactly `d=<data>`, or the link has spaces, control characters or
    /// escapes.
    BadQuery(&'static str),
    /// The data is not base64url.
    BadEncoding,
    /// The data is not the expected JSON.
    BadJson(&'static str),
    /// `v` is greater than [`VERSION`].
    NewerVersion,
    /// `v` is missing or not a positive integer.
    BadVersion,
    /// A `password` key is present.
    PasswordNotAllowed,
    /// A real config key that links may not set (the key, if it is plain).
    FieldNotAllowed(Option<String>),
    /// A key that is not a config key at all.
    UnknownField(Option<String>),
    /// A required field is absent or empty.
    MissingField(&'static str),
    /// A value is not acceptable: field name, fixed reason.
    InvalidField(&'static str, &'static str),
}

impl SetupLinkError {
    /// Stable machine-readable code (`data.code` in the API).
    pub fn code(&self) -> &'static str {
        match self {
            Self::TooLong => "too_long",
            Self::NotASetupLink => "not_a_setup_link",
            Self::UnsupportedAction => "unsupported_action",
            Self::BadQuery(_) => "bad_query",
            Self::BadEncoding => "bad_encoding",
            Self::BadJson(_) => "bad_json",
            Self::NewerVersion => "newer_version",
            Self::BadVersion => "bad_version",
            Self::PasswordNotAllowed => "password_not_allowed",
            Self::FieldNotAllowed(_) => "field_not_allowed",
            Self::UnknownField(_) => "unknown_field",
            Self::MissingField(_) => "missing_field",
            Self::InvalidField(..) => "invalid_field",
        }
    }

    /// The field the error is about, when it is safe to name.
    pub fn field(&self) -> Option<&str> {
        match self {
            Self::FieldNotAllowed(k) | Self::UnknownField(k) => k.as_deref(),
            Self::MissingField(f) | Self::InvalidField(f, _) => Some(f),
            Self::PasswordNotAllowed => Some("password"),
            _ => None,
        }
    }
}

impl std::fmt::Display for SetupLinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLong => write!(f, "the setup link is longer than {MAX_LINK_LEN} bytes"),
            Self::NotASetupLink => f.write_str("this is not a Siple setup link"),
            Self::UnsupportedAction => f.write_str("this kind of siple: link is not supported"),
            Self::BadQuery(r) => write!(f, "the setup link is malformed: {r}"),
            Self::BadEncoding => f.write_str("the setup link data is not valid base64url"),
            Self::BadJson(r) => write!(f, "the setup link data is not valid: {r}"),
            Self::NewerVersion => f.write_str("this setup link needs a newer version of Siple"),
            Self::BadVersion => f.write_str("the setup link has a missing or unsupported version"),
            Self::PasswordNotAllowed => f.write_str("setup links must not contain a password"),
            Self::FieldNotAllowed(Some(k)) => {
                write!(f, "setup links may not set `{k}`")
            }
            Self::FieldNotAllowed(None) => f.write_str("setup links may not set a field"),
            Self::UnknownField(Some(k)) => write!(f, "the setup link has an unknown field `{k}`"),
            Self::UnknownField(None) => f.write_str("the setup link has an unknown field"),
            Self::MissingField(n) => write!(f, "the setup link has no `{n}`"),
            Self::InvalidField(n, r) => write!(f, "`{n}` {r}"),
        }
    }
}

impl std::error::Error for SetupLinkError {}

impl SetupLinkError {
    /// The API error (`invalid_params`, `{reason: "invalid_link", code, field?}`).
    pub fn to_api_error(&self) -> ApiError {
        crate::uri::LinkError::Setup(self.clone()).to_api_error()
    }
}

/// Something worth a second look before the user continues.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupWarning {
    /// The registrar or proxy host is neither the domain nor a subdomain of it.
    ServerOutsideDomain {
        /// The server host.
        host: String,
        /// The account domain.
        domain: String,
    },
    /// A server is given as an IP address.
    IpLiteral(String),
    /// A server is this computer.
    Loopback(String),
    /// A server name has an `xn--` label (shown as typed, never decoded).
    Punycode(String),
    /// SRTP is off: calls are not encrypted.
    SrtpOff,
}

impl SetupWarning {
    /// Stable code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::ServerOutsideDomain { .. } => "server_outside_domain",
            Self::IpLiteral(_) => "ip_literal",
            Self::Loopback(_) => "loopback",
            Self::Punycode(_) => "punycode",
            Self::SrtpOff => "srtp_off",
        }
    }

    /// Text for the user.
    pub fn message(&self) -> String {
        match self {
            Self::ServerOutsideDomain { host, domain } => {
                format!("{host} is not part of {domain}")
            }
            Self::IpLiteral(h) => format!("{h} is an IP address, not a server name"),
            Self::Loopback(h) => format!("{h} points to this computer, not to a server"),
            Self::Punycode(h) => {
                format!("{h} contains an encoded international name; check it letter by letter")
            }
            Self::SrtpOff => "Calls will not be encrypted (SRTP is off)".to_string(),
        }
    }
}

/// The account part of a link. Strings are validated; `""` means unset.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SetupAccount {
    /// SIP user name (required).
    pub username: String,
    /// SIP domain, lowercase (required).
    pub domain: String,
    /// Display name.
    pub display_name: String,
    /// Digest auth user name.
    pub auth_username: String,
    /// Registrar `host[:port]`, lowercase.
    pub registrar: String,
    /// Outbound proxy `host[:port]`, lowercase.
    pub outbound_proxy: String,
    /// Transport; `None` = `auto`.
    pub transport: Option<Transport>,
    /// SRTP policy; `None` = `optional`.
    pub srtp: Option<SrtpPolicy>,
    /// Vendor profile; `None` = `generic`.
    pub profile: Option<Profile>,
}

/// A parsed setup link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupLink {
    /// Schema version (always [`VERSION`] for now).
    pub version: u64,
    /// The account.
    pub account: SetupAccount,
}

/// The link spelling.
pub fn transport_name(t: Transport) -> &'static str {
    match t {
        Transport::Udp => "udp",
        Transport::Tcp => "tcp",
        Transport::Tls => "tls",
        Transport::Auto => "auto",
    }
}

/// The link spelling.
pub fn srtp_name(s: SrtpPolicy) -> &'static str {
    match s {
        SrtpPolicy::Off => "off",
        SrtpPolicy::Optional => "optional",
        SrtpPolicy::Mandatory => "mandatory",
    }
}

/// The link spelling.
pub fn profile_name(p: Profile) -> &'static str {
    match p {
        Profile::Generic => "generic",
        Profile::Yealink => "yealink",
        Profile::Polycom => "polycom",
        Profile::Cisco => "cisco",
        Profile::Grandstream => "grandstream",
    }
}

/// `host` of `host[:port]`.
fn host_of(hp: &str) -> &str {
    if hp.starts_with('[') {
        hp.find(']').map_or(hp, |e| &hp[..=e])
    } else {
        hp.split(':').next().unwrap_or(hp)
    }
}

fn host_strict(h: &str) -> bool {
    if !host_ok(h) {
        return false;
    }
    if h.starts_with('[') {
        return true;
    }
    h.split('.')
        .all(|l| !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-'))
}

/// `host[:port]` with a strict host and a port of 1-65535, host lowercased.
fn norm_hostport(s: &str) -> Option<String> {
    let (host, port) = if s.starts_with('[') {
        let end = s.find(']')?;
        let (h, rest) = s.split_at(end + 1);
        let port = if rest.is_empty() {
            None
        } else {
            Some(rest.strip_prefix(':')?)
        };
        (h, port)
    } else {
        match s.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (s, None),
        }
    };
    if !host_strict(host) {
        return None;
    }
    let mut out = host.to_ascii_lowercase();
    if let Some(p) = port {
        if p.is_empty() || p.len() > 5 || !p.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        if p.parse::<u16>().ok()? == 0 {
            return None;
        }
        out.push(':');
        out.push_str(p);
    }
    Some(out)
}

/// Characters a display name may not contain: anything that is invisible, reorders text or breaks
/// out of the quoted `"name" <uri>` of the From header (which rsipstack writes unescaped).
fn bad_display_char(c: char) -> bool {
    use unicode_general_category::{GeneralCategory, get_general_category};
    c.is_control()
        // Would close the quoted string or the `<uri>`: `a" <sip:evil@x>;tag=y;q="` spoofs From.
        || matches!(c, '"' | '\\' | '<' | '>')
        // All of Unicode category Cf: bidi controls, zero-width characters, soft hyphen,
        // invisible operators, tag characters, Arabic number signs and so on.
        || get_general_category(c) == GeneralCategory::Format
        || matches!(
            c,
            // Line and paragraph separators.
            '\u{2028}' | '\u{2029}'
                // Variation selectors.
                | '\u{FE00}'..='\u{FE0F}'
                | '\u{E0100}'..='\u{E01EF}'
                // Combining grapheme joiner.
                | '\u{034F}'
                // Hangul fillers (render as blanks).
                | '\u{115F}' | '\u{1160}' | '\u{3164}' | '\u{FFA0}'
        )
}

/// Hosts the system resolver reads as an IPv4 address in a short or numeric form (`127.1`,
/// `2130706433`, `0x7f.1`): the last label is all digits or starts with `0x`. Only a full
/// dotted quad is allowed as an IPv4 address.
fn ipv4_shorthand(h: &str) -> bool {
    if h.starts_with('[') || h.parse::<std::net::Ipv4Addr>().is_ok() {
        return false;
    }
    let last = h.rsplit('.').next().unwrap_or(h);
    last.bytes().all(|c| c.is_ascii_digit())
        || last.get(..2).is_some_and(|p| p.eq_ignore_ascii_case("0x"))
}

const SHORTHAND_REASON: &str = "use a full IPv4 address or a host name";

fn sanitize_key(k: &str) -> Option<String> {
    let b = k.as_bytes();
    let ok = !b.is_empty()
        && b.len() <= 32
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_');
    ok.then(|| k.to_string())
}

/// Whether `k` is a key of `[[account]]` or a top-level config section.
fn known_config_key(k: &str) -> bool {
    let has = |v: Value| v.as_object().is_some_and(|o| o.contains_key(k));
    has(serde_json::to_value(AccountConfig::default()).unwrap_or(Value::Null))
        || has(serde_json::to_value(Config::default()).unwrap_or(Value::Null))
}

fn check_user(name: &'static str, s: &str) -> Result<(), SetupLinkError> {
    if s.len() > MAX_USER || !user_ok(s) || !s.bytes().any(|c| c.is_ascii_alphanumeric()) {
        return Err(SetupLinkError::InvalidField(
            name,
            "is not a valid user name",
        ));
    }
    Ok(())
}

fn check_server(name: &'static str, s: &str) -> Result<String, SetupLinkError> {
    let lower = s.to_ascii_lowercase();
    if lower.starts_with("sip:") || lower.starts_with("sips:") {
        return Err(SetupLinkError::InvalidField(
            name,
            "must be a host name (optionally with :port), without sip:",
        ));
    }
    let n = norm_hostport(s).ok_or(SetupLinkError::InvalidField(
        name,
        "must be a host name, optionally with :port",
    ))?;
    if ipv4_shorthand(host_of(&n)) {
        return Err(SetupLinkError::InvalidField(name, SHORTHAND_REASON));
    }
    Ok(n)
}

fn parse_enum<T: Copy>(
    name: &'static str,
    s: &str,
    table: &[(&str, T)],
) -> Result<Option<T>, SetupLinkError> {
    if s.is_empty() {
        return Ok(None);
    }
    table
        .iter()
        .find(|(k, _)| *k == s)
        .map(|(_, v)| Some(*v))
        .ok_or(SetupLinkError::InvalidField(
            name,
            "has an unsupported value",
        ))
}

/// Validates the `account` object of a link (or the `fields` of `provision.build`): the one
/// place the allow-list, the value types and the per-field rules live.
pub fn account_from_fields(map: &Map<String, Value>) -> Result<SetupAccount, SetupLinkError> {
    if map.contains_key("password") {
        return Err(SetupLinkError::PasswordNotAllowed);
    }
    for k in map.keys() {
        if !LINK_FIELDS.iter().any(|f| f.key() == k) {
            let key = sanitize_key(k);
            return Err(if known_config_key(k) {
                SetupLinkError::FieldNotAllowed(key)
            } else {
                SetupLinkError::UnknownField(key)
            });
        }
    }
    let mut s: BTreeMap<AccountField, &str> = BTreeMap::new();
    for f in LINK_FIELDS {
        match map.get(f.key()) {
            None => {}
            Some(Value::String(v)) => {
                s.insert(f, v.as_str());
            }
            Some(_) => {
                return Err(SetupLinkError::InvalidField(f.key(), "must be a string"));
            }
        }
    }
    let get = |f: AccountField| s.get(&f).copied().unwrap_or("");
    let username = get(AccountField::Username);
    let domain = get(AccountField::Domain);
    if username.is_empty() {
        return Err(SetupLinkError::MissingField("username"));
    }
    if domain.is_empty() {
        return Err(SetupLinkError::MissingField("domain"));
    }
    check_user("username", username)?;
    let auth = get(AccountField::AuthUsername);
    if !auth.is_empty() {
        check_user("auth_username", auth)?;
    }
    if !host_strict(domain) {
        return Err(SetupLinkError::InvalidField(
            "domain",
            "must be a host name without a port",
        ));
    }
    if ipv4_shorthand(domain) {
        return Err(SetupLinkError::InvalidField("domain", SHORTHAND_REASON));
    }
    let display = get(AccountField::DisplayName);
    if display.chars().count() > MAX_DISPLAY_CHARS
        || display.len() > MAX_DISPLAY_BYTES
        || display.chars().any(bad_display_char)
    {
        return Err(SetupLinkError::InvalidField(
            "display_name",
            "is too long or contains quotes, angle brackets, backslashes, or invisible or control characters",
        ));
    }
    let registrar = match get(AccountField::Registrar) {
        "" => String::new(),
        v => check_server("registrar", v)?,
    };
    let outbound_proxy = match get(AccountField::OutboundProxy) {
        "" => String::new(),
        v => check_server("outbound_proxy", v)?,
    };
    let transport = parse_enum(
        "transport",
        get(AccountField::Transport),
        &[
            ("udp", Transport::Udp),
            ("tcp", Transport::Tcp),
            ("tls", Transport::Tls),
            ("auto", Transport::Auto),
        ],
    )?;
    let srtp = parse_enum(
        "srtp",
        get(AccountField::Srtp),
        &[
            ("off", SrtpPolicy::Off),
            ("optional", SrtpPolicy::Optional),
            ("mandatory", SrtpPolicy::Mandatory),
        ],
    )?;
    let profile = parse_enum(
        "profile",
        get(AccountField::Profile),
        &[
            ("generic", Profile::Generic),
            ("yealink", Profile::Yealink),
            ("polycom", Profile::Polycom),
            ("cisco", Profile::Cisco),
            ("grandstream", Profile::Grandstream),
        ],
    )?;
    Ok(SetupAccount {
        username: username.to_string(),
        domain: domain.to_ascii_lowercase(),
        display_name: display.to_string(),
        auth_username: auth.to_string(),
        registrar,
        outbound_proxy,
        transport,
        srtp,
        profile,
    })
}

/// Typed second pass over the JSON, to catch duplicate keys the `Value` parse merged.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct RawLink {
    v: u64,
    account: RawAccount,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct RawAccount {
    username: Option<String>,
    domain: Option<String>,
    display_name: Option<String>,
    auth_username: Option<String>,
    registrar: Option<String>,
    outbound_proxy: Option<String>,
    transport: Option<String>,
    srtp: Option<String>,
    profile: Option<String>,
}

/// Validates decoded link JSON (checks 5 to 9 of the parser).
pub(crate) fn parse_json(bytes: &[u8]) -> Result<SetupLink, SetupLinkError> {
    if bytes.len() > MAX_JSON_LEN {
        return Err(SetupLinkError::BadJson("the data is too large"));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| SetupLinkError::BadJson("not UTF-8"))?;
    let value: Value =
        serde_json::from_str(text).map_err(|_| SetupLinkError::BadJson("not JSON"))?;
    let Value::Object(top) = value else {
        return Err(SetupLinkError::BadJson("not a JSON object"));
    };
    // Version first, so a future link reports "needs a newer Siple" whatever else it has.
    let version = match top.get("v") {
        Some(Value::Number(n)) => n.as_u64().filter(|v| *v > 0),
        _ => None,
    }
    .ok_or(SetupLinkError::BadVersion)?;
    if version > VERSION {
        return Err(SetupLinkError::NewerVersion);
    }
    if top.contains_key("password") {
        return Err(SetupLinkError::PasswordNotAllowed);
    }
    for k in top.keys() {
        if k != "v" && k != "account" {
            let key = sanitize_key(k);
            return Err(if known_config_key(k) {
                SetupLinkError::FieldNotAllowed(key)
            } else {
                SetupLinkError::UnknownField(key)
            });
        }
    }
    let account = match top.get("account") {
        None => return Err(SetupLinkError::MissingField("account")),
        Some(Value::Object(m)) => account_from_fields(m)?,
        Some(_) => {
            return Err(SetupLinkError::InvalidField("account", "must be an object"));
        }
    };
    serde_json::from_str::<RawLink>(text)
        .map_err(|_| SetupLinkError::BadJson("a key appears twice"))?;
    Ok(SetupLink { version, account })
}

/// Parses a `siple:provision` link. Pure: no network, no file access.
pub fn parse(link: &str) -> Result<SetupLink, SetupLinkError> {
    let link = link.trim();
    if link.len() > MAX_LINK_LEN {
        return Err(SetupLinkError::TooLong);
    }
    check_chars(link).map_err(|_| {
        SetupLinkError::BadQuery("it contains spaces, control characters or escapes")
    })?;
    let (scheme, rest) = link.split_once(':').ok_or(SetupLinkError::NotASetupLink)?;
    if !scheme.eq_ignore_ascii_case("siple") {
        return Err(SetupLinkError::NotASetupLink);
    }
    let rest = rest.strip_prefix("//").unwrap_or(rest);
    let (action, query) = match rest.split_once('?') {
        Some((a, q)) => (a, Some(q)),
        None => (rest, None),
    };
    let action = action.strip_suffix('/').unwrap_or(action);
    if !action.eq_ignore_ascii_case("provision") {
        return Err(SetupLinkError::UnsupportedAction);
    }
    let query = query.ok_or(SetupLinkError::BadQuery("there is no data"))?;
    let payload = query
        .strip_prefix("d=")
        .filter(|p| !p.contains(['&', '#', '?']))
        .ok_or(SetupLinkError::BadQuery(
            "expected exactly one parameter, `d`",
        ))?;
    let payload = payload
        .strip_suffix("==")
        .or_else(|| payload.strip_suffix('='))
        .unwrap_or(payload);
    if payload.is_empty()
        || !payload
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err(SetupLinkError::BadEncoding);
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| SetupLinkError::BadEncoding)?;
    parse_json(&bytes)
}

impl SetupAccount {
    /// The set fields as `(key, value)` in link order.
    fn pairs(&self) -> Vec<(&'static str, String)> {
        let mut v = Vec::new();
        let mut put = |k: AccountField, s: &str| {
            if !s.is_empty() {
                v.push((k.key(), s.to_string()));
            }
        };
        put(AccountField::Username, &self.username);
        put(AccountField::Domain, &self.domain);
        put(AccountField::DisplayName, &self.display_name);
        put(AccountField::AuthUsername, &self.auth_username);
        put(AccountField::Registrar, &self.registrar);
        put(AccountField::OutboundProxy, &self.outbound_proxy);
        if let Some(t) = self.transport {
            put(AccountField::Transport, transport_name(t));
        }
        if let Some(s) = self.srtp {
            put(AccountField::Srtp, srtp_name(s));
        }
        if let Some(p) = self.profile {
            put(AccountField::Profile, profile_name(p));
        }
        v
    }

    /// The set fields as a JSON object (what the link's `account` holds).
    pub fn to_json_object(&self) -> Value {
        Value::Object(
            self.pairs()
                .into_iter()
                .map(|(k, v)| (k.to_string(), Value::String(v)))
                .collect(),
        )
    }

    /// Where the account will connect: the outbound proxy, else the registrar, else the domain.
    pub fn next_hop(&self) -> String {
        [&self.outbound_proxy, &self.registrar, &self.domain]
            .into_iter()
            .find(|s| !s.is_empty())
            .cloned()
            .unwrap_or_default()
    }

    /// Things to look at twice (see [`SetupWarning`]).
    pub fn warnings(&self) -> Vec<SetupWarning> {
        let mut out: Vec<SetupWarning> = Vec::new();
        let mut push = |w: SetupWarning| {
            if !out.contains(&w) {
                out.push(w);
            }
        };
        let domain = self.domain.as_str();
        let mut hosts: Vec<(&str, bool)> = vec![(domain, false)];
        for s in [&self.registrar, &self.outbound_proxy] {
            if !s.is_empty() {
                hosts.push((host_of(s), true));
            }
        }
        for (h, is_server) in hosts {
            if is_server && h != domain && !h.ends_with(&format!(".{domain}")) {
                push(SetupWarning::ServerOutsideDomain {
                    host: h.to_string(),
                    domain: domain.to_string(),
                });
            }
            let bare = h.trim_start_matches('[').trim_end_matches(']');
            let v4 = bare.parse::<std::net::Ipv4Addr>().ok();
            let v6 = bare.parse::<std::net::Ipv6Addr>().ok();
            if v4.is_some() || v6.is_some() {
                push(SetupWarning::IpLiteral(h.to_string()));
            }
            if h == "localhost"
                || h.ends_with(".localhost")
                || v4.is_some_and(|a| a.is_loopback() || a.is_unspecified())
                || v6.is_some_and(|a| {
                    a.is_loopback()
                        || a.is_unspecified()
                        || a.to_ipv4_mapped()
                            .is_some_and(|m| m.is_loopback() || m.is_unspecified())
                })
            {
                push(SetupWarning::Loopback(h.to_string()));
            }
            if h.split('.').any(|l| l.starts_with("xn--")) {
                push(SetupWarning::Punycode(h.to_string()));
            }
        }
        if self.srtp == Some(SrtpPolicy::Off) {
            push(SetupWarning::SrtpOff);
        }
        out
    }

    /// The account draft this link prefills. `transport` is always present (`auto` when the link
    /// has none) because the config default is `udp`. Never has a password.
    pub fn draft(&self) -> AccountDraft {
        let mut fields: BTreeMap<AccountField, EditValue> = BTreeMap::new();
        for (k, v) in self.pairs() {
            if let Some(f) = LINK_FIELDS.iter().find(|f| f.key() == k) {
                fields.insert(*f, EditValue::Str(v));
            }
        }
        fields
            .entry(AccountField::Transport)
            .or_insert_with(|| EditValue::Str("auto".into()));
        AccountDraft {
            id: suggested_account_id(&self.username, &self.domain),
            fields,
        }
    }

    /// The id of a configured account this link would duplicate (same derived id, or same user
    /// and case-insensitive domain).
    pub fn existing_in(&self, accounts: &[AccountConfig]) -> Option<String> {
        let id = suggested_account_id(&self.username, &self.domain);
        accounts
            .iter()
            .find(|a| {
                a.id.eq_ignore_ascii_case(&id)
                    || (a.username == self.username && a.domain.eq_ignore_ascii_case(&self.domain))
            })
            .map(|a| a.id.clone())
    }
}

impl SetupLink {
    /// A link for `account`.
    pub fn new(account: SetupAccount) -> Self {
        Self {
            version: VERSION,
            account,
        }
    }

    /// Compact JSON (the data of the link), fields in a fixed order.
    pub fn to_json(&self) -> String {
        let q = |s: &str| serde_json::to_string(s).unwrap_or_default();
        let fields: Vec<String> = self
            .account
            .pairs()
            .into_iter()
            .map(|(k, v)| format!("{}:{}", q(k), q(&v)))
            .collect();
        format!(
            "{{\"v\":{},\"account\":{{{}}}}}",
            self.version,
            fields.join(",")
        )
    }

    /// The `siple:provision?d=...` link. Fails with `TooLong` when it would not fit the limits.
    pub fn encode(&self) -> Result<String, SetupLinkError> {
        let json = self.to_json();
        if json.len() > MAX_JSON_LEN {
            return Err(SetupLinkError::TooLong);
        }
        let link = format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(json.as_bytes()));
        if link.len() > MAX_LINK_LEN {
            return Err(SetupLinkError::TooLong);
        }
        Ok(link)
    }

    /// What `provision.parse` reports about the link (without `existing`, which needs the
    /// configured accounts): `{version, account, draft: {id, fields}, next_hop, warnings}`.
    pub fn report(&self) -> Value {
        let a = &self.account;
        let d = a.draft();
        json!({
            "version": self.version,
            "account": a.to_json_object(),
            "draft": {"id": d.id, "fields": d.fields},
            "next_hop": a.next_hop(),
            "warnings": a.warnings().iter()
                .map(|w| json!({"code": w.code(), "message": w.message()}))
                .collect::<Vec<_>>(),
        })
    }

    /// The link for a configured account. The saved values go through the same validators as a
    /// received link, so the result always parses; a value a link cannot express is an
    /// `InvalidField` error. The password is never included.
    pub fn from_account(a: &AccountConfig) -> Result<Self, SetupLinkError> {
        let mut m = Map::new();
        let mut put = |f: AccountField, s: &str| {
            if !s.is_empty() {
                m.insert(f.key().into(), Value::String(s.to_string()));
            }
        };
        put(AccountField::Username, &a.username);
        put(AccountField::Domain, &a.domain);
        put(AccountField::DisplayName, &a.display_name);
        put(AccountField::AuthUsername, &a.auth_username);
        put(AccountField::Registrar, &a.registrar);
        put(AccountField::OutboundProxy, &a.outbound_proxy);
        put(AccountField::Transport, transport_name(a.transport));
        if a.srtp != SrtpPolicy::Optional {
            put(AccountField::Srtp, srtp_name(a.srtp));
        }
        if a.profile != Profile::Generic {
            put(AccountField::Profile, profile_name(a.profile));
        }
        let link = Self::new(account_from_fields(&m)?);
        link.encode()?;
        Ok(link)
    }
}

// ---- API ---------------------------------------------------------------------------------------

/// Methods handled by [`AppBackend::provision_method`].
pub const PROVISION_METHODS: &[&str] = &["provision.parse", "provision.build"];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ParseParams {
    link: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BuildParams {
    account: Option<String>,
    fields: Option<Map<String, Value>>,
}

/// Schemas and descriptions of [`PROVISION_METHODS`].
pub fn provision_methods() -> Vec<MethodInfo> {
    let keys: Vec<&str> = LINK_FIELDS.iter().map(|f| f.key()).collect();
    vec![
        MethodInfo::new(
            "provision.parse",
            &format!(
                "Validate a `siple:provision` setup link without any side effect: no network, no file is written, no window opens (`app.open_uri` opens the GUI's Review screen). Result {{version, account (the fields the link sets), draft {{id, fields}}, next_hop (the host the account would connect to), warnings [{{code, message}}] (server_outside_domain, ip_literal, loopback, punycode, srtp_off), existing (the id of a configured account the link would duplicate, else null)}}. `draft` is accepted as-is by `config.edit` `add_account`, and by `account.test` once `password` is added to `draft.fields`. A link carries only these account fields: {}; `username` and `domain` are required. `display_name` may not contain quotes, backslashes, angle brackets or invisible characters, and a host must be a name or a full IPv4 address (not shorthand such as 127.1). It never carries a password; a `password` key, any other config key and any unknown key are refused. Links are at most {MAX_LINK_LEN} bytes. Errors: invalid_params with data {{reason: \"invalid_link\", code, field?}}; code is one of too_long, not_a_setup_link, unsupported_action, bad_query, bad_encoding, bad_json, newer_version, bad_version, password_not_allowed, field_not_allowed, unknown_field, missing_field, invalid_field.",
                keys.join(", ")
            ),
            json!({"type":"object","properties":{
                "link":{"type":"string","maxLength":MAX_LINK_LEN,"description":"The setup link, e.g. siple:provision?d=eyJ2IjoxLC4uLn0."}},
                "required":["link"],"additionalProperties":false}),
        ),
        MethodInfo::new(
            "provision.build",
            "Build a `siple:provision` setup link, from a configured account (`account`: its id) or from `fields` (an object with the allowed fields, as in `provision.parse`'s `account`). The link never contains a password; `fields` with a `password` key is refused (password_not_allowed). Result {link, length, fields (what the link carries), omitted (account keys set in the config file that the link does not carry, such as password, password_store, tls_fingerprint; only with `account`)}. Links contain the user name and server names, so treat them as semi-private. Errors: not_found, invalid_params with data {reason: \"invalid_link\", code, field?} (for example invalid_field for a saved value a link cannot express).",
            json!({"type":"object","properties":{
                "account":{"type":"string","description":"Id of a configured account."},
                "fields":{"type":"object","description":"Link fields: username, domain, display_name, auth_username, registrar, outbound_proxy, transport, srtp, profile (all strings)."}},
                "oneOf":[{"required":["account"]},{"required":["fields"]}],"additionalProperties":false}),
        ),
    ]
}

impl AppBackend {
    /// Dispatches `method` (one of [`PROVISION_METHODS`]).
    pub(crate) async fn provision_method(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Value, ApiError> {
        match method {
            "provision.parse" => {
                let p: ParseParams = parse_params(params)?;
                let link = parse(&p.link).map_err(|e| e.to_api_error())?;
                let mut v = link.report();
                v["existing"] = json!(link.account.existing_in(&self.inner.config.load().accounts));
                Ok(v)
            }
            "provision.build" => {
                let p: BuildParams = parse_params(params)?;
                let (link, omitted) = match (p.account, p.fields) {
                    (Some(id), None) => {
                        let acct = self
                            .inner
                            .config
                            .load()
                            .accounts
                            .iter()
                            .find(|a| a.id == id)
                            .cloned()
                            .ok_or_else(|| {
                                ApiError::not_found(format!("unknown account {id:?}"))
                            })?;
                        let link = SetupLink::from_account(&acct).map_err(|e| e.to_api_error())?;
                        (link, self.omitted_keys(&acct))
                    }
                    (None, Some(fields)) => {
                        let account = account_from_fields(&fields).map_err(|e| e.to_api_error())?;
                        (SetupLink::new(account), Vec::new())
                    }
                    _ => {
                        return Err(ApiError::invalid_params(
                            "give exactly one of `account` (a configured account id) or `fields`",
                        ));
                    }
                };
                let text = link.encode().map_err(|e| e.to_api_error())?;
                Ok(json!({
                    "link": text,
                    "length": text.len(),
                    "fields": link.account.to_json_object(),
                    "omitted": omitted,
                }))
            }
            other => Err(ApiError::method_not_found(other)),
        }
    }

    /// Keys set in the config file for `acct` that a link does not carry.
    fn omitted_keys(&self, acct: &AccountConfig) -> Vec<String> {
        let view = self.inner.config_view();
        let mut out: Vec<String> = view["provenance"]["account"][acct.id.as_str()]
            .as_object()
            .map(|m| {
                m.iter()
                    .filter(|(k, v)| {
                        v.as_str() == Some("file")
                            && !LINK_FIELDS.iter().any(|f| f.key() == k.as_str())
                            && k.as_str() != "id"
                            && k.as_str() != "enabled"
                    })
                    .map(|(k, _)| k.clone())
                    .collect()
            })
            .unwrap_or_default();
        if !acct.password.is_empty() && !out.iter().any(|k| k == "password") {
            out.push("password".into());
        }
        out.sort();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = "siple:provision?d=eyJ2IjoxLCJhY2NvdW50Ijp7InVzZXJuYW1lIjoiMTAwMSIsImRvbWFpbiI6ImV4YW1wbGUudGVzdCIsImRpc3BsYXlfbmFtZSI6IkZyb250IGRlc2siLCJyZWdpc3RyYXIiOiJwYnguZXhhbXBsZS50ZXN0OjUwNjEiLCJ0cmFuc3BvcnQiOiJ0bHMiLCJzcnRwIjoibWFuZGF0b3J5In19";

    fn link(json: &str) -> String {
        format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(json.as_bytes()))
    }

    fn acct(fields: &str) -> String {
        link(&format!("{{\"v\":1,\"account\":{{{fields}}}}}"))
    }

    fn ok(l: &str) -> SetupLink {
        parse(l).unwrap_or_else(|e| panic!("{l}: {e}"))
    }

    fn code(l: &str) -> &'static str {
        match parse(l) {
            Ok(_) => panic!("accepted: {l}"),
            Err(e) => e.code(),
        }
    }

    const MIN: &str = r#""username":"1001","domain":"example.test""#;

    #[test]
    fn valid_links() {
        let l = ok(EXAMPLE);
        assert_eq!(l.version, 1);
        assert_eq!(l.account.username, "1001");
        assert_eq!(l.account.domain, "example.test");
        assert_eq!(l.account.display_name, "Front desk");
        assert_eq!(l.account.registrar, "pbx.example.test:5061");
        assert_eq!(l.account.transport, Some(Transport::Tls));
        assert_eq!(l.account.srtp, Some(SrtpPolicy::Mandatory));
        assert_eq!(l.encode().unwrap(), EXAMPLE, "encoding is stable");
        assert_eq!(EXAMPLE.len(), 234);

        let m = ok(&acct(MIN));
        assert_eq!(m.account.transport, None);
        assert_eq!(m.account.srtp, None);
        assert_eq!(m.account.profile, None);
        // Forms of the prefix.
        let d = acct(MIN);
        let payload = d.strip_prefix(PREFIX).unwrap();
        for form in [
            format!("siple://provision?d={payload}"),
            format!("siple:provision/?d={payload}"),
            format!("SIPLE:PROVISION?d={payload}"),
            format!("  siple:Provision?d={payload}\n"),
            format!("siple:provision?d={payload}="),
            format!("siple:provision?d={payload}=="),
        ] {
            assert_eq!(ok(&form), m, "{form}");
        }
        // Hosts are lowercased; IPv6 with a port.
        let l = ok(&acct(
            r#""username":"a","domain":"EXAMPLE.Test","registrar":"[2001:DB8::1]:5061","outbound_proxy":"Edge.Example.TEST""#,
        ));
        assert_eq!(l.account.domain, "example.test");
        assert_eq!(l.account.registrar, "[2001:db8::1]:5061");
        assert_eq!(l.account.outbound_proxy, "edge.example.test");
        // Empty optional string = absent.
        let l = ok(&acct(&format!(
            r#"{MIN},"display_name":"","registrar":"","transport":"","srtp":"","profile":"""#
        )));
        assert_eq!(l.account, ok(&acct(MIN)).account);
        // Emoji display names are fine.
        ok(&acct(&format!(r#"{MIN},"display_name":"Lobby 📞""#)));
    }

    #[test]
    fn rejects_bad_links() {
        assert_eq!(code(""), "not_a_setup_link");
        assert_eq!(code(&"a".repeat(MAX_LINK_LEN + 1)), "too_long");
        assert_eq!(code("siplex:provision?d=e30"), "not_a_setup_link");
        assert_eq!(code("sip:a@b.test"), "not_a_setup_link");
        assert_eq!(code("siple:call?d=e30"), "unsupported_action");
        assert_eq!(code("siple:"), "unsupported_action");
        assert_eq!(code("siple:provision"), "bad_query");
        let d = acct(MIN);
        let p = d.strip_prefix(PREFIX).unwrap();
        assert_eq!(code("siple:provision?x=1"), "bad_query");
        assert_eq!(code(&format!("siple:provision?d={p}&d={p}")), "bad_query");
        assert_eq!(code(&format!("siple:provision?d={p}&x=1")), "bad_query");
        assert_eq!(code(&format!("{d}#frag")), "bad_query");
        assert_eq!(code(&format!("{d} x")), "bad_query");
        assert_eq!(code(&format!("{d}%0a")), "bad_query");
        assert_eq!(code(&format!("{d}\n{p}")), "bad_query");
        assert_eq!(code(&format!("{d}%41")), "bad_encoding");
        assert_eq!(code("siple:provision?d="), "bad_encoding");
        assert_eq!(code("siple:provision?d=a+b/c"), "bad_encoding");
        assert_eq!(code("siple:provision?d=ab=cd"), "bad_encoding");
        assert_eq!(code("siple:provision?d=eyJ2IjoxfQ==="), "bad_encoding");
        assert_eq!(code("siple:provision?d=e"), "bad_encoding");
    }

    /// Long enough that the 2048 check (not the decoder) is what refuses it.
    #[test]
    fn size_limits() {
        let long = format!("{PREFIX}{}", "A".repeat(MAX_LINK_LEN));
        assert_eq!(code(&long), "too_long");
        assert_eq!(
            parse_json(&vec![b' '; MAX_JSON_LEN + 1])
                .unwrap_err()
                .code(),
            "bad_json"
        );
        // The largest encodable link fits.
        let a = SetupAccount {
            username: "u".repeat(64),
            domain: format!("{}.test", "d".repeat(60)),
            display_name: "x".repeat(64),
            auth_username: "a".repeat(64),
            registrar: format!("{}.test:5061", "r".repeat(60)),
            outbound_proxy: format!("{}.test:5061", "p".repeat(60)),
            transport: Some(Transport::Tls),
            srtp: Some(SrtpPolicy::Mandatory),
            profile: Some(Profile::Grandstream),
        };
        let l = SetupLink::new(a).encode().unwrap();
        assert!(l.len() <= MAX_LINK_LEN);
        ok(&l);
    }

    #[test]
    fn json_shape() {
        let raw = |j: &str| link(j);
        assert_eq!(
            code(&format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode([0xff, 0xfe]))),
            "bad_json"
        );
        assert_eq!(code(&raw("[1]")), "bad_json");
        assert_eq!(code(&raw("\"x\"")), "bad_json");
        assert_eq!(code(&raw("{}  x")), "bad_json");
        assert_eq!(code(&raw("")), "bad_encoding");
        assert_eq!(
            code(&raw(
                r#"{"v":1,"account":{"username":"a","domain":"b.test"}}x"#
            )),
            "bad_json"
        );
        // Duplicate keys.
        assert_eq!(
            code(&raw(
                r#"{"v":1,"account":{"username":"a","username":"b","domain":"b.test"}}"#
            )),
            "bad_json"
        );
        assert_eq!(
            code(&raw(
                r#"{"v":1,"v":1,"account":{"username":"a","domain":"b.test"}}"#
            )),
            "bad_json"
        );
        // Versions.
        for v in [
            r#"{"account":{"username":"a","domain":"b.test"}}"#,
            r#"{"v":0,"account":{"username":"a","domain":"b.test"}}"#,
            r#"{"v":"1","account":{"username":"a","domain":"b.test"}}"#,
            r#"{"v":1.0,"account":{"username":"a","domain":"b.test"}}"#,
            r#"{"v":-1,"account":{"username":"a","domain":"b.test"}}"#,
            r#"{"v":null,"account":{}}"#,
        ] {
            assert_eq!(code(&raw(v)), "bad_version", "{v}");
        }
        assert_eq!(
            code(&raw(r#"{"v":2,"account":{"newthing":1},"extra":true}"#)),
            "newer_version"
        );
        assert_eq!(code(&raw(r#"{"v":1}"#)), "missing_field");
        assert_eq!(code(&raw(r#"{"v":1,"account":[]}"#)), "invalid_field");
        assert_eq!(code(&raw(r#"{"v":1,"account":null}"#)), "invalid_field");
    }

    #[test]
    fn allow_list_is_strict() {
        for top in [
            "control", "speech", "logging", "calls", "ui", "network", "audio",
        ] {
            let j = format!(r#"{{"v":1,"account":{{{MIN}}},"{top}":{{"x":1}}}}"#);
            assert_eq!(code(&link(&j)), "field_not_allowed", "{top}");
        }
        assert_eq!(
            code(&link(&format!(
                r#"{{"v":1,"account":{{{MIN}}},"bogus":1}}"#
            ))),
            "unknown_field"
        );
        // A password wins over every other error, at either level.
        assert_eq!(
            code(&acct(&format!(
                r#"{MIN},"bogus":1,"password":"x","transport":"zzz""#
            ))),
            "password_not_allowed"
        );
        assert_eq!(
            code(&link(&format!(
                r#"{{"v":1,"account":{{{MIN}}},"bogus":1,"password":"x"}}"#
            ))),
            "password_not_allowed"
        );
        for key in [
            "tls_verify",
            "tls_fingerprint",
            "auto_answer",
            "allow_intercom",
            "auth_foreign",
            "extra_headers",
            "audio_source",
            "audio_sink",
            "local_port",
            "password_store",
            "id",
            "enabled",
            "dnd",
            "codecs",
            "blf",
            "bind_address",
            "tls_ca_file",
            "keepalive_secs",
        ] {
            let c = code(&acct(&format!(r#"{MIN},"{key}":"1""#)));
            assert!(
                c == "field_not_allowed" || c == "unknown_field",
                "{key}: {c}"
            );
        }
        // Every real account key that is not allowed is "not allowed", never ignored.
        let all = serde_json::to_value(AccountConfig::default()).unwrap();
        for key in all.as_object().unwrap().keys() {
            if LINK_FIELDS.iter().any(|f| f.key() == key) {
                continue;
            }
            let expect = if key == "password" {
                "password_not_allowed"
            } else {
                "field_not_allowed"
            };
            assert_eq!(
                code(&acct(&format!(r#"{MIN},"{key}":"1""#))),
                expect,
                "{key}"
            );
        }
    }

    #[test]
    fn errors_never_echo_values() {
        let evil = "\u{202e}evil";
        let secret = "s3cr3t-value-xyz";
        let bodies = [
            format!(r#"{MIN},"{evil}":"{secret}""#),
            format!(r#"{MIN},"Bogus_Key":"{secret}""#),
            format!(r#"{MIN},"display_name":"{secret}\u0000""#),
            format!(r#""username":"{secret}@x","domain":"example.test""#),
            format!(r#""username":"a","domain":"{secret}.test:99""#),
            format!(r#"{MIN},"registrar":"sip:{secret}""#),
            format!(r#"{MIN},"transport":"{secret}""#),
            format!(r#"{MIN},"password":"{secret}""#),
            format!(r#"{MIN},"{}":"x""#, "k".repeat(40)),
        ];
        for b in bodies {
            let e = parse(&acct(&b)).unwrap_err();
            let msg = e.to_string();
            assert!(!msg.contains(secret) && !msg.contains(evil), "{msg}");
            assert!(!msg.contains("kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk"), "{msg}");
            assert!(!msg.contains('\u{202e}'), "{msg}");
            let api = e.to_api_error();
            assert!(!format!("{:?}", api.extra).contains(secret));
            assert_eq!(api.extra.as_ref().unwrap()["reason"], "invalid_link");
            assert_eq!(api.extra.as_ref().unwrap()["code"], e.code());
        }
        // A plain key name is named; an odd one is "a field".
        assert!(
            parse(&acct(&format!(r#"{MIN},"auto_answer":"1""#)))
                .unwrap_err()
                .to_string()
                .contains("auto_answer")
        );
        assert_eq!(
            parse(&acct(&format!(r#"{MIN},"\u202eevil":"1""#))).unwrap_err(),
            SetupLinkError::UnknownField(None)
        );
        assert!(
            parse(&acct(&format!(r#"{MIN},"\u202eevil":"1""#)))
                .unwrap_err()
                .to_string()
                .contains("unknown field")
        );
    }

    #[test]
    fn field_rules() {
        let bad = |fields: &str| code(&acct(fields));
        // Types.
        assert_eq!(bad(r#""username":1,"domain":"a.test""#), "invalid_field");
        assert_eq!(bad(r#""username":null,"domain":"a.test""#), "invalid_field");
        assert_eq!(
            bad(r#""username":"a","domain":"a.test","srtp":true"#),
            "invalid_field"
        );
        assert_eq!(
            bad(r#""username":"a","domain":["a.test"]"#),
            "invalid_field"
        );
        // Required.
        assert_eq!(bad(r#""domain":"a.test""#), "missing_field");
        assert_eq!(bad(r#""username":"a""#), "missing_field");
        assert_eq!(bad(r#""username":"","domain":"a.test""#), "missing_field");
        // Usernames.
        for u in [
            "a@b",
            "a;b",
            "a%41",
            "a b",
            "a:b",
            "...",
            "é",
            &"a".repeat(65),
        ] {
            let j = serde_json::to_string(u).unwrap();
            assert_eq!(
                bad(&format!(r#""username":{j},"domain":"a.test""#)),
                "invalid_field",
                "{u}"
            );
        }
        ok(&acct(&format!(
            r#""username":"{}","domain":"a.test""#,
            "a".repeat(64)
        )));
        ok(&acct(r#""username":"+15551234567","domain":"a.test""#));
        assert_eq!(
            bad(&format!(r#"{MIN},"auth_username":"x;y""#)),
            "invalid_field"
        );
        // Domains.
        for d in [
            "a.test:5060",
            "a.test/x",
            "a_b.test",
            "-a.test",
            ".a.test",
            "a..test",
            "a.test.",
            "почта.test",
            "a b.test",
            "a@b.test",
            "[::1",
            "[zz]",
            "",
        ] {
            let j = serde_json::to_string(d).unwrap();
            let c = bad(&format!(r#""username":"a","domain":{j}"#));
            assert!(c == "invalid_field" || c == "missing_field", "{d}: {c}");
        }
        ok(&acct(r#""username":"a","domain":"[2001:db8::1]""#));
        // Registrar and proxy.
        for r in [
            "sip:pbx.test",
            "SIPS:pbx.test",
            "pbx.test:0",
            "pbx.test:70000",
            "pbx.test:x",
            "pbx.test:",
            "pbx.test:+5061",
            "pbx.test:1:2",
            "pbx.test/x",
            "pbx.test;transport=tcp",
            "a@pbx.test",
            "[::1]x",
            "[::1]:",
            "pbx.test:005061x",
        ] {
            for k in ["registrar", "outbound_proxy"] {
                assert_eq!(
                    bad(&format!(r#"{MIN},"{k}":"{r}""#)),
                    "invalid_field",
                    "{k}={r}"
                );
            }
        }
        ok(&acct(&format!(r#"{MIN},"registrar":"pbx.test:65535""#)));
        ok(&acct(&format!(r#"{MIN},"outbound_proxy":"[::1]:5060""#)));
        // Display names.
        for d in [
            "a\\nb",
            "a\\u0000b",
            "a\\u202eb",
            "a\\u200bb",
            "a\\ufeffb",
            "a\\u2066b",
            "a\\u00adb",
            "a\\u2028b",
            "a\\u061cb",
            "a\\udb40\\udc41b",
            "a\\u034fb",
            "a\\ufe0fb",
            "a\\u3164b",
            "a\\u115fb",
            "a\\uffa0b",
            "a\\udb40\\udd00b",
            "a\\u180eb",
            "a\\\"b",
            "a\\\\b",
            "a<b",
            "a>b",
            // Spoofs the From URI and tag in REGISTER.
            "a\\\" <sip:evil@evil.test>;tag=x;q=\\\"",
            &"x".repeat(65),
        ] {
            assert_eq!(
                bad(&format!(r#"{MIN},"display_name":"{d}""#)),
                "invalid_field",
                "{d}"
            );
        }
        ok(&acct(&format!(
            r#"{MIN},"display_name":"{}""#,
            "x".repeat(64)
        )));
        ok(&acct(&format!(
            r#"{MIN},"display_name":"{}""#,
            "📞".repeat(64)
        )));
        assert_eq!(
            bad(&format!(r#"{MIN},"display_name":"{}""#, "📞".repeat(65))),
            "invalid_field"
        );
        // Enums are exact lowercase.
        for (k, v) in [
            ("transport", "TLS"),
            ("transport", "sctp"),
            ("srtp", "Mandatory"),
            ("srtp", "on"),
            ("profile", "Yealink"),
            ("profile", "avaya"),
        ] {
            assert_eq!(
                bad(&format!(r#"{MIN},"{k}":"{v}""#)),
                "invalid_field",
                "{k}={v}"
            );
        }
        for (k, v) in [
            ("transport", "udp"),
            ("transport", "tcp"),
            ("transport", "tls"),
            ("transport", "auto"),
            ("srtp", "off"),
            ("srtp", "optional"),
            ("srtp", "mandatory"),
            ("profile", "generic"),
            ("profile", "yealink"),
            ("profile", "polycom"),
            ("profile", "cisco"),
            ("profile", "grandstream"),
        ] {
            ok(&acct(&format!(r#"{MIN},"{k}":"{v}""#)));
        }
    }

    #[test]
    fn warnings_matrix() {
        let w = |fields: &str| -> Vec<&'static str> {
            ok(&acct(fields))
                .account
                .warnings()
                .iter()
                .map(|w| w.code())
                .collect()
        };
        assert!(w(MIN).is_empty());
        assert!(w(&format!(r#"{MIN},"registrar":"example.test:5061""#)).is_empty());
        assert!(w(&format!(r#"{MIN},"registrar":"pbx.example.test""#)).is_empty());
        assert_eq!(
            w(&format!(r#"{MIN},"registrar":"sip.attacker.test""#)),
            ["server_outside_domain"]
        );
        // Label-boundary compare: "evilexample.test" is not under "example.test".
        assert_eq!(
            w(&format!(r#"{MIN},"outbound_proxy":"evilexample.test""#)),
            ["server_outside_domain"]
        );
        assert_eq!(
            w(&format!(r#"{MIN},"registrar":"192.0.2.10""#)),
            ["server_outside_domain", "ip_literal"]
        );
        assert_eq!(w(r#""username":"a","domain":"192.0.2.10""#), ["ip_literal"]);
        assert_eq!(
            w(&format!(r#"{MIN},"registrar":"[::1]:5060""#)),
            ["server_outside_domain", "ip_literal", "loopback"]
        );
        assert_eq!(
            w(&format!(r#"{MIN},"registrar":"127.0.0.2""#)),
            ["server_outside_domain", "ip_literal", "loopback"]
        );
        // IPv4-mapped loopback and the unspecified addresses are "this computer" too.
        for r in [
            "[::ffff:127.0.0.1]",
            "[::ffff:7f00:1]",
            "0.0.0.0",
            "[::]",
            "[::ffff:0.0.0.0]",
        ] {
            assert_eq!(
                w(&format!(r#"{MIN},"registrar":"{r}""#)),
                ["server_outside_domain", "ip_literal", "loopback"],
                "{r}"
            );
        }
        assert_eq!(
            w(&format!(r#"{MIN},"registrar":"localhost:5060""#)),
            ["server_outside_domain", "loopback"]
        );
        assert_eq!(
            w(r#""username":"a","domain":"xn--e1afmkfd.example""#),
            ["punycode"]
        );
        assert_eq!(w(&format!(r#"{MIN},"srtp":"off""#)), ["srtp_off"]);
        let l = ok(&acct(&format!(r#"{MIN},"registrar":"sip.attacker.test""#)));
        assert_eq!(
            l.account.warnings()[0].message(),
            "sip.attacker.test is not part of example.test"
        );
    }

    #[test]
    fn next_hop_and_draft() {
        let l = ok(EXAMPLE);
        assert_eq!(l.account.next_hop(), "pbx.example.test:5061");
        let l2 = ok(&acct(&format!(
            r#"{MIN},"registrar":"pbx.example.test","outbound_proxy":"edge.example.test:5070""#
        )));
        assert_eq!(l2.account.next_hop(), "edge.example.test:5070");
        assert_eq!(ok(&acct(MIN)).account.next_hop(), "example.test");
        let d = ok(&acct(MIN)).account.draft();
        assert_eq!(d.id, "1001.example.test");
        assert_eq!(
            d.fields.get(&AccountField::Transport),
            Some(&EditValue::Str("auto".into()))
        );
        assert!(!d.fields.contains_key(&AccountField::Password));
        assert!(d.fields.keys().all(|f| LINK_FIELDS.contains(f)));
        // The draft is a valid account for the config layer.
        let cfg = crate::config::edit::account_from_draft(&d).unwrap();
        assert_eq!(cfg.transport, Transport::Auto);
        assert_eq!(cfg.username, "1001");
    }

    #[test]
    fn derived_ids_are_safe() {
        let users = ["1001", "a.b", "+15551234567", "a'b", "x!y", "u~*()&=$,"];
        let domains = ["example.test", "[2001:db8::1]", "a-b.c.test", "192.0.2.1"];
        for u in users {
            for d in domains {
                let l = ok(&acct(&format!(r#""username":"{u}","domain":"{d}""#)));
                let id = l.account.draft().id;
                assert!(
                    !id.is_empty()
                        && id
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')),
                    "{id}"
                );
            }
        }
    }

    #[test]
    fn existing_accounts_are_found() {
        let l = ok(&acct(MIN)).account;
        let mut a = AccountConfig {
            id: "other".into(),
            username: "1001".into(),
            domain: "EXAMPLE.test".into(),
            ..Default::default()
        };
        assert_eq!(
            l.existing_in(std::slice::from_ref(&a)),
            Some("other".into())
        );
        a.username = "1002".into();
        assert_eq!(l.existing_in(std::slice::from_ref(&a)), None);
        a.id = "1001.Example.Test".into();
        assert_eq!(l.existing_in(std::slice::from_ref(&a)), Some(a.id.clone()));
    }

    #[test]
    fn round_trips() {
        let l = ok(EXAMPLE);
        assert_eq!(ok(&l.encode().unwrap()), l);
        let full = SetupLink::new(SetupAccount {
            username: "1001".into(),
            domain: "example.test".into(),
            display_name: "Ünï 'q' ; , 📞".into(),
            auth_username: "auth1001".into(),
            registrar: "pbx.example.test:5061".into(),
            outbound_proxy: "[2001:db8::1]:5070".into(),
            transport: Some(Transport::Auto),
            srtp: Some(SrtpPolicy::Off),
            profile: Some(Profile::Cisco),
        });
        assert_eq!(ok(&full.encode().unwrap()), full);
    }

    #[test]
    fn from_account_always_parses() {
        let mut a = AccountConfig {
            id: "x".into(),
            username: "1001".into(),
            domain: "Example.Test".into(),
            password: crate::config::Secret::new("hunter2"),
            transport: Transport::Tls,
            ..Default::default()
        };
        let l = SetupLink::from_account(&a).unwrap();
        let text = l.encode().unwrap();
        assert!(!text.contains("hunter2"));
        let json = String::from_utf8(
            URL_SAFE_NO_PAD
                .decode(text.strip_prefix(PREFIX).unwrap())
                .unwrap(),
        )
        .unwrap();
        assert!(
            !json.contains("password") && !json.contains("hunter2"),
            "{json}"
        );
        assert!(json.contains("\"transport\":\"tls\""));
        assert!(!json.contains("srtp") && !json.contains("profile"));
        assert_eq!(ok(&text), l);
        // Non-default values are carried.
        a.srtp = SrtpPolicy::Mandatory;
        a.profile = Profile::Yealink;
        a.registrar = "pbx.example.test:5061".into();
        a.display_name = "Desk".into();
        a.auth_username = "auth".into();
        let t = SetupLink::from_account(&a).unwrap().encode().unwrap();
        let back = ok(&t);
        assert_eq!(back.account.srtp, Some(SrtpPolicy::Mandatory));
        assert_eq!(back.account.profile, Some(Profile::Yealink));
        assert_eq!(back.account.registrar, "pbx.example.test:5061");
        // Values a link cannot express are refused.
        a.registrar = "sip:pbx.example.test".into();
        assert_eq!(
            SetupLink::from_account(&a).unwrap_err().code(),
            "invalid_field"
        );
        a.registrar.clear();
        a.username = "a;b".into();
        assert!(SetupLink::from_account(&a).is_err());
    }

    struct Xs(u64);
    impl Xs {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n.max(1) as u64) as usize
        }
    }

    fn mutate(rng: &mut Xs, bytes: &mut Vec<u8>, alphabet: &[u8]) {
        for _ in 0..=rng.below(4) {
            let i = rng.below(bytes.len() + 1);
            match rng.below(5) {
                0 if i < bytes.len() => bytes[i] = alphabet[rng.below(alphabet.len())],
                1 if i < bytes.len() => {
                    bytes.remove(i);
                }
                2 => bytes.insert(i, alphabet[rng.below(alphabet.len())]),
                3 if i < bytes.len() => bytes[i] ^= 1 << rng.below(8),
                _ if i < bytes.len() => {
                    let j = rng.below(bytes.len());
                    bytes.swap(i, j);
                }
                _ => {}
            }
        }
    }

    /// What every accepted link must satisfy.
    fn check_accepted(l: &SetupLink, link: &str) {
        assert!(link.trim().len() <= MAX_LINK_LEN);
        assert_eq!(l.version, VERSION);
        let obj = l.account.to_json_object();
        let m = obj.as_object().unwrap();
        for k in m.keys() {
            assert!(LINK_FIELDS.iter().any(|f| f.key() == k), "{k}");
        }
        assert!(!m.contains_key("password"));
        // Re-validating the accepted values changes nothing.
        assert_eq!(account_from_fields(m).as_ref(), Ok(&l.account), "{link}");
        let a = &l.account;
        assert!(!a.username.is_empty() && a.username.len() <= MAX_USER);
        assert!(!a.domain.is_empty() && a.domain.len() <= 253);
        assert!(a.display_name.chars().count() <= MAX_DISPLAY_CHARS);
        assert!(a.display_name.len() <= MAX_DISPLAY_BYTES);
        assert!(!a.display_name.chars().any(bad_display_char));
        for s in [&a.registrar, &a.outbound_proxy] {
            assert_eq!(s, &s.to_ascii_lowercase());
            assert!(
                !s.contains(|c: char| c.is_whitespace() || c.is_control() || c == '@' || c == ';')
            );
        }
        let id = a.draft().id;
        assert!(
            id.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        );
    }

    #[test]
    fn mutation_fuzz_is_safe() {
        let seeds = [
            EXAMPLE.to_string(),
            acct(MIN),
            acct(&format!(
                r#"{MIN},"display_name":"Front desk","auth_username":"a1","registrar":"pbx.example.test:5061","outbound_proxy":"[2001:db8::1]:5070","transport":"tls","srtp":"mandatory","profile":"yealink""#
            )),
        ];
        let json_alphabet = br#"{}[]",:\ 0123456789abcdefghijklmnopqrstuvwxyz_-.@;%"#;
        let b64_alphabet =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_=+/%&#?: \n";
        let mut rng = Xs(0x9E37_79B9_7F4A_7C15);
        let (mut accepted, mut total) = (0u32, 0u32);
        for i in 0..20_000u32 {
            let seed = &seeds[rng.below(seeds.len())];
            let text = if i % 2 == 0 {
                // Mutate the link text.
                let mut b = seed.clone().into_bytes();
                mutate(&mut rng, &mut b, b64_alphabet);
                String::from_utf8_lossy(&b).into_owned()
            } else {
                // Mutate the JSON, then re-encode it.
                let mut json = URL_SAFE_NO_PAD
                    .decode(seed.strip_prefix(PREFIX).unwrap())
                    .unwrap();
                mutate(&mut rng, &mut json, json_alphabet);
                format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(&json))
            };
            total += 1;
            if let Ok(l) = parse(&text) {
                accepted += 1;
                check_accepted(&l, &text);
                // Accepted links also survive being rebuilt.
                assert_eq!(ok(&l.encode().unwrap()), l);
            }
        }
        assert!(accepted > 50 && accepted < total, "{accepted}/{total}");
    }

    /// `a" <sip:evil@evil.test>;tag=x;q="` would spoof the From URI and tag, because rsipstack
    /// writes the display name unescaped.
    #[test]
    fn display_name_cannot_break_out_of_the_from_header() {
        let bad = |fields: &str| code(&acct(fields));
        let payload = r#"a\" <sip:evil@evil.test>;tag=x;q=\""#;
        assert_eq!(
            bad(&format!(r#"{MIN},"display_name":"{payload}""#)),
            "invalid_field"
        );
        for c in ["\\\"", "\\\\", "<", ">"] {
            assert_eq!(
                bad(&format!(r#"{MIN},"display_name":"a{c}b""#)),
                "invalid_field",
                "{c}"
            );
        }
        // Accounts saved with such a name cannot be turned into a link either.
        let a = AccountConfig {
            id: "x".into(),
            username: "1001".into(),
            domain: "example.test".into(),
            display_name: "a\"b".into(),
            ..Default::default()
        };
        assert!(SetupLink::from_account(&a).is_err());
    }

    /// The docs table is generated from top-level `properties`, so both params must be there.
    #[test]
    fn build_schema_lists_both_params() {
        let m = provision_methods();
        let b = m.iter().find(|m| m.name == "provision.build").unwrap();
        let props = b.params_schema["properties"].as_object().unwrap();
        assert!(props.contains_key("account") && props.contains_key("fields"));
        assert_eq!(b.params_schema["oneOf"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn invisible_display_characters_are_refused() {
        for c in [
            '\u{061C}',
            '\u{E0041}',
            '\u{034F}',
            '\u{FE0F}',
            '\u{3164}',
            '\u{E0100}',
        ] {
            assert!(bad_display_char(c), "{c:?}");
        }
        for c in ['a', 'é', ' ', ';', ',', '\'', '📞', '-'] {
            assert!(!bad_display_char(c), "{c:?}");
        }
    }

    #[test]
    fn ipv4_shorthand_hosts_are_refused() {
        let bad = |fields: &str| code(&acct(fields));
        for h in [
            "127.1",
            "2130706433",
            "10.1",
            "0x7f.1",
            "0X7F.0.0.1",
            "1.2.3.0x4",
            "a.b.1",
            "1.2.3",
            "1.2.3.4.5",
            "01.2.3.4",
        ] {
            for f in [
                format!(r#""username":"a","domain":"{h}""#),
                format!(r#"{MIN},"registrar":"{h}""#),
                format!(r#"{MIN},"outbound_proxy":"{h}:5060""#),
            ] {
                assert_eq!(bad(&f), "invalid_field", "{f}");
            }
        }
        // A full dotted quad, host names that merely contain digits, and v6 stay valid.
        for h in [
            "192.0.2.10",
            "1.example.test",
            "x1",
            "0xample.test",
            "a.0xb.test",
        ] {
            ok(&acct(&format!(r#"{MIN},"registrar":"{h}""#)));
        }
        ok(&acct(r#""username":"a","domain":"192.0.2.10""#));
    }

    #[test]
    fn rsipstack_agrees_on_hosts_and_users() {
        use rsipstack::sip::Uri;
        let accounts = [
            r#""username":"1001","domain":"example.test""#,
            r#""username":"+15551234567","domain":"a-b.example.test","registrar":"pbx.example.test:5061""#,
            r#""username":"u.v_w","domain":"[2001:db8::1]","registrar":"[2001:db8::2]:5070","outbound_proxy":"192.0.2.7:5060""#,
            r#""username":"a'b!~*()&=$,","domain":"EXAMPLE.TEST","registrar":"PBX.example.test""#,
        ];
        for f in accounts {
            let a = ok(&acct(f)).account;
            for hp in [&a.registrar, &a.outbound_proxy] {
                if hp.is_empty() {
                    continue;
                }
                let u = Uri::try_from(format!("sip:{hp}").as_str())
                    .unwrap_or_else(|e| panic!("{hp}: {e}"));
                assert!(
                    u.auth.is_none() && u.params.is_empty() && u.headers.is_empty(),
                    "{hp}"
                );
                assert_eq!(
                    u.host_with_port.to_string().to_lowercase(),
                    hp.to_lowercase(),
                    "{hp}"
                );
            }
            let aor = format!("sip:{}@{}", a.username, a.domain);
            let u = Uri::try_from(aor.as_str()).unwrap_or_else(|e| panic!("{aor}: {e}"));
            let auth = u.auth.as_ref().expect("user part");
            assert_eq!(auth.user, a.username, "{aor}");
            assert!(auth.password.is_none());
            assert!(u.params.is_empty() && u.headers.is_empty(), "{aor}");
            assert_eq!(
                u.host_with_port.to_string().to_lowercase(),
                a.domain,
                "{aor}"
            );
        }
    }

    /// The From header, built as `siple-sip`'s `identity_headers` does, parses back to the same
    /// URI and display name for every name a link may carry.
    #[test]
    fn rsipstack_agrees_on_display_names() {
        use rsipstack::sip::{Uri, typed};
        for name in [
            "Front desk",
            "Ünï Çödé",
            "a;b,c",
            "it's",
            "Lobby 📞",
            "a;tag=x;q=1",
            "sip:x@y",
            "a  b",
        ] {
            let json = serde_json::to_string(name).unwrap();
            let a = ok(&acct(&format!(r#"{MIN},"display_name":{json}"#))).account;
            assert_eq!(a.display_name, name);
            let uri = Uri::try_from(format!("sip:{}@{}", a.username, a.domain).as_str()).unwrap();
            let from = typed::From {
                display_name: Some(a.display_name.clone()),
                uri: uri.clone(),
                params: vec![],
            }
            .with_tag("tag123".into());
            let text = from.to_string();
            let back = typed::From::parse(&text).unwrap_or_else(|e| panic!("{text}: {e}"));
            assert_eq!(back.uri.to_string(), uri.to_string(), "{text}");
            assert_eq!(back.display_name.as_deref(), Some(name), "{text}");
            assert_eq!(back.tag().map(|t| t.to_string()).as_deref(), Some("tag123"));
        }
    }
}

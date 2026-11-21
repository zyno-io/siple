//! Credential and key redaction for logs and SIP traces.
//!
//! Two `[logging]` switches, both hot, both off by default:
//! - `redact_srtp_keys`: SDES master keys (`a=crypto:... inline:<key>`) become `<redacted>`;
//! - `redact_credentials`: the `response`, `nonce` and `cnonce` values of digest
//!   `Authorization` / `Proxy-Authorization` headers, the control token and every configured
//!   account password (4 characters or longer; shorter ones cannot be masked without mangling
//!   unrelated text) become `<redacted>`.
//!
//! One [`Redactor`] is shared by the application log (stderr, file and the `logs.tail` ring, via
//! [`RedactMakeWriter`]) and by the SIP trace writer. Whatever the switches say, the code never
//! logs a password: this module only masks what already reached a line.

use std::borrow::Cow;
use std::io::{self, Write};
use std::sync::Arc;

use arc_swap::ArcSwap;
use tracing_subscriber::fmt::MakeWriter;

use crate::config::{AccountConfig, LoggingConfig, Secret};

/// What replaces a masked value.
pub const MASK: &str = "<redacted>";

/// Shortest secret that is searched for literally.
const MIN_SECRET_LEN: usize = 4;

#[derive(Default)]
struct Policy {
    srtp: bool,
    credentials: bool,
    /// Literal secrets, longest first (only while `credentials` is on).
    secrets: Vec<String>,
}

impl std::fmt::Debug for Policy {
    /// Never prints the secrets themselves.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Policy")
            .field("srtp", &self.srtp)
            .field("credentials", &self.credentials)
            .field("secrets", &format_args!("<{} hidden>", self.secrets.len()))
            .finish()
    }
}

/// The hot-swappable redaction policy. See the module docs.
#[derive(Debug, Default)]
pub struct Redactor {
    policy: ArcSwap<Policy>,
}

impl Redactor {
    /// A redactor that does nothing until [`Redactor::update`] turns something on.
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// Replaces the policy from the effective config.
    pub fn update(&self, logging: &LoggingConfig, accounts: &[AccountConfig], token: &Secret) {
        let mut secrets: Vec<String> = Vec::new();
        if logging.redact_credentials {
            secrets = accounts
                .iter()
                .map(|a| a.password.expose())
                .chain(std::iter::once(token.expose()))
                .filter(|s| s.len() >= MIN_SECRET_LEN)
                .flat_map(forms)
                .collect();
            secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
            secrets.dedup();
        }
        self.policy.store(Arc::new(Policy {
            srtp: logging.redact_srtp_keys,
            credentials: logging.redact_credentials,
            secrets,
        }));
    }

    /// Whether any masking is on.
    pub fn active(&self) -> bool {
        let p = self.policy.load();
        p.srtp || p.credentials
    }

    /// `text` with everything the policy covers masked; borrowed when nothing changed.
    pub fn apply<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let p = self.policy.load();
        let mut out = Cow::Borrowed(text);
        if p.credentials {
            out = chain(out, mask_digest_headers);
            for s in &p.secrets {
                if out.contains(s.as_str()) {
                    out = Cow::Owned(out.replace(s.as_str(), MASK));
                }
            }
        }
        if p.srtp {
            out = chain(out, mask_srtp_keys);
        }
        out
    }
}

/// A secret as it can appear on a line: as is, percent-encoded (a URI) and JSON-escaped.
fn forms(secret: &str) -> Vec<String> {
    let pct = |hex_lower: bool| {
        let mut out = String::new();
        for b in secret.bytes() {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
                out.push(b as char);
            } else if hex_lower {
                out.push_str(&format!("%{b:02x}"));
            } else {
                out.push_str(&format!("%{b:02X}"));
            }
        }
        out
    };
    let json = serde_json::to_string(secret)
        .ok()
        .map(|j| j[1..j.len() - 1].to_string());
    let mut v = vec![secret.to_string(), pct(false), pct(true)];
    v.extend(json);
    v.sort();
    v.dedup();
    v
}

fn chain<'a>(text: Cow<'a, str>, f: impl for<'b> Fn(&'b str) -> Cow<'b, str>) -> Cow<'a, str> {
    match &text {
        Cow::Borrowed(t) => f(t),
        Cow::Owned(t) => match f(t) {
            Cow::Borrowed(_) => text,
            Cow::Owned(o) => Cow::Owned(o),
        },
    }
}

/// Masks the `response`, `nonce` and `cnonce` parameters of every `Authorization:` /
/// `Proxy-Authorization:` header in `text` (case-insensitive; also inside a longer line, such as a
/// log line quoting a header). The rest of the header (username, realm, uri, algorithm) stays,
/// which is what a bug report needs.
pub fn mask_digest_headers(text: &str) -> Cow<'_, str> {
    if !text.to_ascii_lowercase().contains("authorization") {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut in_header = false;
    for line in text.split_inclusive('\n') {
        if in_header && line.starts_with([' ', '\t']) {
            // A folded continuation of the header value.
            out.push_str(&mask_params(line));
            continue;
        }
        match header_value_start(line) {
            Some(split) => {
                in_header = true;
                out.push_str(&line[..split]);
                out.push_str(&mask_params(&line[split..]));
            }
            None => {
                in_header = false;
                out.push_str(line);
            }
        }
    }
    Cow::Owned(out)
}

/// Offset just after the colon of an `Authorization` / `Proxy-Authorization` header name in
/// `line` (whitespace before the colon allowed).
fn header_value_start(line: &str) -> Option<usize> {
    const NAME: &str = "authorization";
    let lower = line.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find(NAME) {
        let after = from + i + NAME.len();
        let rest = &lower[after..];
        let trimmed = rest.trim_start_matches([' ', '\t']);
        if trimmed.starts_with(':') {
            return Some(after + (rest.len() - trimmed.len()) + 1);
        }
        from = after;
    }
    None
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Masks `response=`, `nonce=` and `cnonce=` values in one header value.
fn mask_params(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let rest = &lower[i..];
        let key = ["response", "cnonce", "nonce"]
            .into_iter()
            .find(|k| rest.starts_with(k));
        let boundary = s[..i].chars().next_back().is_none_or(|c| !is_word(c));
        if let (Some(key), true) = (key, boundary) {
            let after_key = &s[i + key.len()..];
            let ws = after_key.len() - after_key.trim_start().len();
            if after_key[ws..].starts_with('=') {
                let value = after_key[ws + 1..].trim_start();
                let skipped = after_key.len() - value.len();
                let end = if let Some(q) = value.strip_prefix('"') {
                    q.find('"').map_or(value.len(), |e| e + 2)
                } else {
                    value
                        .find(|c: char| c == ',' || c.is_whitespace())
                        .unwrap_or(value.len())
                };
                out.push_str(&s[i..i + key.len() + skipped]);
                out.push('"');
                out.push_str(MASK);
                out.push('"');
                i += key.len() + skipped + end;
                continue;
            }
        }
        let c = s[i..].chars().next().expect("in bounds");
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// Masks SDES master keys: the base64 run after `inline:` (SDP `a=crypto` lines, also when quoted
/// in a log line).
pub fn mask_srtp_keys(text: &str) -> Cow<'_, str> {
    const KEY: &str = "inline:";
    // ASCII lowercasing keeps byte offsets, so positions found there index `text`.
    let lower = text.to_ascii_lowercase();
    if !lower.contains(KEY) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut pos = 0;
    while let Some(at) = lower[pos..].find(KEY) {
        let start = pos + at + KEY.len();
        out.push_str(&text[pos..start]);
        let tail = &text[start..];
        let key_len = tail
            .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=')))
            .unwrap_or(tail.len());
        if key_len > 0 {
            out.push_str(MASK);
        }
        pos = start + key_len;
    }
    out.push_str(&text[pos..]);
    Cow::Owned(out)
}

// ---- tracing writers ---------------------------------------------------------------------------

/// Wraps a [`MakeWriter`] so every event is redacted before it reaches the real writer. With
/// nothing to mask the events pass through unbuffered.
#[derive(Clone)]
pub struct RedactMakeWriter<M> {
    inner: M,
    redactor: Arc<Redactor>,
}

impl<M> RedactMakeWriter<M> {
    /// Wraps `inner`.
    pub fn new(inner: M, redactor: Arc<Redactor>) -> Self {
        Self { inner, redactor }
    }
}

/// The writer of [`RedactMakeWriter`].
pub struct RedactWriter<W: Write> {
    inner: W,
    /// `Some` while this event is buffered for redaction.
    buffered: Option<(Vec<u8>, Arc<Redactor>)>,
}

impl<W: Write> RedactWriter<W> {
    fn new(inner: W, redactor: &Arc<Redactor>) -> Self {
        let buffered = redactor
            .active()
            .then(|| (Vec::new(), Arc::clone(redactor)));
        Self { inner, buffered }
    }

    fn drain(&mut self) -> io::Result<()> {
        let Some((buf, redactor)) = &mut self.buffered else {
            return Ok(());
        };
        if buf.is_empty() {
            return Ok(());
        }
        let result = match std::str::from_utf8(buf) {
            Ok(text) => match redactor.apply(text) {
                Cow::Borrowed(_) => self.inner.write_all(buf),
                Cow::Owned(masked) => self.inner.write_all(masked.as_bytes()),
            },
            Err(_) => {
                let text = String::from_utf8_lossy(buf);
                self.inner.write_all(redactor.apply(&text).as_bytes())
            }
        };
        buf.clear();
        result
    }
}

impl<W: Write> Write for RedactWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        match &mut self.buffered {
            Some((buf, _)) => {
                buf.extend_from_slice(data);
                Ok(data.len())
            }
            None => self.inner.write(data),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.drain()?;
        self.inner.flush()
    }
}

impl<W: Write> Drop for RedactWriter<W> {
    fn drop(&mut self) {
        let _ = self.drain();
    }
}

impl<'a, M: MakeWriter<'a>> MakeWriter<'a> for RedactMakeWriter<M> {
    type Writer = RedactWriter<M::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        RedactWriter::new(self.inner.make_writer(), &self.redactor)
    }

    fn make_writer_for(&'a self, meta: &tracing::Metadata<'_>) -> Self::Writer {
        RedactWriter::new(self.inner.make_writer_for(meta), &self.redactor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(srtp: bool, credentials: bool) -> LoggingConfig {
        LoggingConfig {
            redact_srtp_keys: srtp,
            redact_credentials: credentials,
        }
    }

    fn account(password: &str) -> AccountConfig {
        AccountConfig {
            id: "a".into(),
            password: Secret::new(password),
            ..Default::default()
        }
    }

    const AUTH: &str = "REGISTER sip:pbx SIP/2.0\r\nAuthorization: Digest username=\"1001\", realm=\"pbx\", \
        nonce=\"abc123nonce\", uri=\"sip:pbx\", response=\"0123456789abcdef0123456789abcdef\", \
        algorithm=MD5, cnonce=\"clientnonce99\", qop=auth, nc=00000001\r\nCSeq: 2 REGISTER\r\n\r\n";

    #[test]
    fn digest_headers_lose_response_nonce_and_cnonce_only() {
        let out = mask_digest_headers(AUTH);
        for secret in [
            "abc123nonce",
            "0123456789abcdef0123456789abcdef",
            "clientnonce99",
        ] {
            assert!(!out.contains(secret), "{out}");
        }
        for kept in [
            "username=\"1001\"",
            "realm=\"pbx\"",
            "uri=\"sip:pbx\"",
            "algorithm=MD5",
            "qop=auth",
            "nc=00000001",
            "CSeq: 2 REGISTER",
        ] {
            assert!(out.contains(kept), "{kept} in {out}");
        }
        assert_eq!(out.matches("<redacted>").count(), 3, "{out}");
        // Proxy-Authorization, any case, and unquoted values.
        let p = "proxy-authorization: Digest nonce=abc, response = deadbeef ,username=\"u\"\r\n";
        let out = mask_digest_headers(p);
        assert!(!out.contains("abc") && !out.contains("deadbeef"), "{out}");
        assert!(out.contains("username=\"u\""));
        // A challenge (WWW-Authenticate) is not a credential and stays readable.
        let ch = "WWW-Authenticate: Digest realm=\"pbx\", nonce=\"serverNonce\"\r\n";
        assert_eq!(mask_digest_headers(ch), ch);
        // Not a header: a field merely named like one is left alone.
        let other = "X-Note: response=\"fine\"\r\n";
        assert_eq!(mask_digest_headers(other), other);
        // A header quoted inside a log line.
        let log = "sent: Authorization: Digest response=\"abcd\", username=\"x\"";
        assert!(!mask_digest_headers(log).contains("abcd"));
    }

    #[test]
    fn header_name_whitespace_folding_and_case_variants() {
        let spaced = "Authorization : Digest username=\"u\", response=\"aa11\"\r\n";
        assert!(!mask_digest_headers(spaced).contains("aa11"));
        let folded = "Proxy-Authorization: Digest username=\"u\",\r\n response=\"bb22\",\r\n\tnonce=\"cc33\"\r\nCSeq: 1\r\n";
        let out = mask_digest_headers(folded);
        assert!(!out.contains("bb22") && !out.contains("cc33"), "{out}");
        assert!(
            out.contains("CSeq: 1") && out.contains("username=\"u\""),
            "{out}"
        );
        // A continuation of some other header is left alone.
        let other = "Subject: x\r\n response=\"keep\"\r\n";
        assert_eq!(mask_digest_headers(other), other);
        let sdp = "a=crypto:1 X INLINE:QUJDREVGR0hJSktMTU5PUA==\r\n";
        assert!(!mask_srtp_keys(sdp).contains("QUJD"));
        assert!(mask_srtp_keys(sdp).contains("INLINE:<redacted>"));
    }

    #[test]
    fn secrets_are_masked_in_encoded_forms_and_never_debug_printed() {
        let r = Redactor::new();
        let pw = "p@ss \"w0rd\"/x";
        r.update(&cfg(false, true), &[account(pw)], &Secret::new(""));
        let pct = "p%40ss%20%22w0rd%22%2Fx";
        let json = "p@ss \\\"w0rd\\\"/x";
        let line = format!("raw={pw} uri=sip:{pct}@h json={json}");
        let out = r.apply(&line);
        for form in [pw, pct, json] {
            assert!(!out.contains(form), "{form} in {out}");
        }
        assert!(!format!("{r:?}").contains("w0rd"), "{r:?}");
        // The lowercase-hex form lowercases only the escapes, never the password's own letters.
        let r = Redactor::new();
        r.update(&cfg(false, true), &[account("Ab@Cd Ef")], &Secret::new(""));
        for form in ["Ab@Cd Ef", "Ab%40Cd%20Ef"] {
            assert!(!r.apply(&format!("x {form} y")).contains("Ab"), "{form}");
        }
        let r2 = Redactor::new();
        r2.update(&cfg(false, true), &[account("Pä Ss")], &Secret::new(""));
        for form in ["P%c3%a4%20Ss", "P%C3%A4%20Ss"] {
            assert!(!r2.apply(form).contains("Ss"), "{form}");
        }
    }

    #[test]
    fn srtp_keys_are_masked() {
        let sdp = "m=audio 4000 RTP/SAVP 0\r\na=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:WVNfX19zZWNyZXRrZXlfX19fX19fX19fX19fX19fX18=|2^31|1:1\r\na=rtpmap:0 PCMU/8000\r\n";
        let out = mask_srtp_keys(sdp);
        assert!(!out.contains("WVNf"), "{out}");
        assert!(out.contains("inline:<redacted>|2^31|1:1") && out.contains("a=rtpmap:0 PCMU/8000"));
        assert_eq!(mask_srtp_keys("nothing here"), "nothing here");
    }

    #[test]
    fn policy_is_off_by_default_and_hot() {
        let r = Redactor::new();
        assert!(!r.active());
        assert_eq!(r.apply(AUTH), AUTH);
        let token = Secret::new("tok-0123456789");
        let accounts = [account("hunter2-secret")];
        r.update(&cfg(false, true), &accounts, &token);
        assert!(r.active());
        let line = format!("{AUTH} pw=hunter2-secret token=tok-0123456789 short=pw"); // gitleaks:allow (test fixture)
        let out = r.apply(&line);
        assert!(
            !out.contains("hunter2-secret") && !out.contains("tok-0123456789"),
            "{out}"
        );
        assert!(
            out.contains("short=pw"),
            "secrets under 4 chars are not searched"
        );
        // SRTP stays visible while only credentials are on.
        let sdp = "a=crypto:1 X inline:QUJDREVGR0hJSktMTU5PUA==\r\n";
        assert_eq!(r.apply(sdp), sdp);
        r.update(&cfg(true, false), &accounts, &token);
        assert!(!r.apply(sdp).contains("QUJD"));
        // Credentials off again: the password passes (it is the operator's choice).
        assert!(r.apply("pw=hunter2-secret").contains("hunter2-secret"));
        r.update(&cfg(false, false), &accounts, &token);
        assert!(!r.active());
    }

    #[derive(Clone, Default)]
    struct Sink(Arc<std::sync::Mutex<Vec<u8>>>);
    impl Write for Sink {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl<'a> MakeWriter<'a> for Sink {
        type Writer = Sink;
        fn make_writer(&'a self) -> Sink {
            self.clone()
        }
    }

    #[test]
    fn make_writer_redacts_events_and_follows_the_policy() {
        let sink = Sink::default();
        let r = Redactor::new();
        let make = RedactMakeWriter::new(sink.clone(), r.clone());
        let sub = tracing_subscriber::fmt()
            .with_writer(make)
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(sub, || {
            tracing::info!("before: password=hunter2-secret");
            r.update(
                &cfg(false, true),
                &[account("hunter2-secret")],
                &Secret::new(""),
            );
            tracing::info!(
                "after: password=hunter2-secret {}",
                "Authorization: Digest response=\"cafe\""
            );
        });
        let text = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
        let (before, after) = text.split_once("after:").unwrap();
        assert!(before.contains("hunter2-secret"), "policy was off: {text}");
        assert!(
            !after.contains("hunter2-secret") && !after.contains("cafe"),
            "{text}"
        );
        assert!(after.contains("<redacted>"));
    }
}

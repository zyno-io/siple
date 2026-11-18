//! Configuration: schema, parsing, validation and default paths.

pub mod channel;
pub mod edit;
pub mod log_level;
mod migrate;
mod paths;
mod profile;
mod schema;
pub mod secrets;
pub mod shortcuts;

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

pub use migrate::*;
pub use paths::*;
pub use schema::*;

/// Smallest accepted `speech.tts_timeout_ms` / `speech.stt_timeout_ms`.
pub const SPEECH_TIMEOUT_MIN_MS: u32 = 1_000;
/// Largest accepted `speech.tts_timeout_ms` / `speech.stt_timeout_ms`. The control client's
/// request timeouts for speech-backed methods derive from this value.
pub const SPEECH_TIMEOUT_MAX_MS: u32 = 120_000;

/// Longest user-supplied string echoed back in an error message, in characters.
pub const ECHO_MAX_CHARS: usize = 64;

/// Debug-quotes a user-supplied value for an error message, cut to [`ECHO_MAX_CHARS`] characters
/// (a mistakenly passed base64 blob must not flood an error, or an LLM's context).
pub fn echo(s: &str) -> String {
    let total = s.chars().count();
    if total <= ECHO_MAX_CHARS {
        format!("{s:?}")
    } else {
        let head: String = s.chars().take(ECHO_MAX_CHARS).collect();
        format!("{head:?}... ({total} chars)")
    }
}

/// The commented example config embedded in the binary and written on first run.
pub const EXAMPLE_CONFIG: &str = include_str!("../config.example.toml");

/// Failure to read or parse a config file.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file could not be read.
    #[error("cannot read config {}: {source}", path.display())]
    Io {
        /// File that failed to open.
        path: PathBuf,
        /// Underlying error.
        source: std::io::Error,
    },
    /// The text is not valid TOML or does not match the schema. The message is built so that
    /// it never echoes a `password`/`token` line (see [`parse_error`]).
    #[error("invalid config{}{}: {message}{}",
        path.as_ref().map(|p| format!(" {}", p.display())).unwrap_or_default(),
        position.map(|(l, c)| format!(" at line {l}, column {c}")).unwrap_or_default(),
        snippet.as_ref().map(|s| format!("\n  | {s}")).unwrap_or_default())]
    Parse {
        /// File the text came from, if any.
        path: Option<PathBuf>,
        /// 1-based line and column of the problem, when known.
        position: Option<(usize, usize)>,
        /// Parser message (generic when the offending line holds a secret).
        message: String,
        /// The offending source line, omitted when it may contain a secret.
        snippet: Option<String>,
    },
}

/// Deserialises `text`, then resolves vendor profiles: the text is first parsed as-is (so errors
/// keep their line/column), and only configs with a non-generic profile are re-read as a TOML
/// table to fill in the bundled keys the file does not set (explicit config > profile > default).
fn parse_text(text: &str) -> Result<Config, toml::de::Error> {
    let cfg: Config = toml::from_str(text)?;
    if cfg.accounts.iter().all(|a| a.profile == Profile::Generic) {
        return Ok(cfg);
    }
    let mut table: toml::Table = toml::from_str(text)?;
    profile::apply_to_table(&mut table);
    table.try_into()
}

/// True if `line` assigns a secret-bearing key (`password` / `token`, case-insensitive,
/// also as the tail of a longer key such as `auth_token`).
fn line_has_secret_key(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    ["password", "token"].iter().any(|key| {
        l.match_indices(key).any(|(i, _)| {
            l[i + key.len()..]
                .trim_start_matches(['"', '\''])
                .trim_start()
                .starts_with('=')
        })
    })
}

/// Converts a toml error into a [`ConfigError::Parse`] without leaking secrets: toml's own
/// Display embeds the source line, which for `password = hunter2` would print the password.
fn parse_error(path: Option<PathBuf>, text: &str, err: &toml::de::Error) -> ConfigError {
    let mut message = err.message().to_string();
    let mut position = None;
    let mut snippet = None;
    if let Some(span) = err.span() {
        let start = span.start.min(text.len());
        let end = span.end.clamp(start, text.len());
        let line_start = text
            .get(..start)
            .map_or(0, |t| t.rfind('\n').map_or(0, |i| i + 1));
        let line_no = text.get(..start).map_or(0, |t| t.matches('\n').count()) + 1;
        let col = text.get(line_start..start).map_or(0, |t| t.chars().count()) + 1;
        position = Some((line_no, col));
        // Every line the span touches, from the start of its first line.
        let covered = text.get(line_start..).map_or("", |t| {
            let rel_end = (end - line_start).min(t.len());
            let upto = t[rel_end..].find('\n').map_or(t.len(), |i| rel_end + i);
            &t[..upto]
        });
        if covered.lines().any(line_has_secret_key) {
            message = "invalid value (details withheld: the line holds a secret)".to_string();
        } else {
            snippet = covered.lines().next().map(|l| l.trim_end().to_string());
        }
    }
    ConfigError::Parse {
        path,
        position,
        message,
        snippet,
    }
}

/// One semantic problem found by [`Config::validate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigIssue {
    /// Dotted path of the offending option, e.g. `account[1].username`.
    pub path: String,
    /// What is wrong.
    pub message: String,
}

impl fmt::Display for ConfigIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

impl Config {
    /// Parses TOML text. Does not run [`Config::validate`].
    pub fn parse(text: &str) -> Result<Config, ConfigError> {
        parse_text(text).map_err(|e| parse_error(None, text, &e))
    }

    /// Reads and parses a config file. Does not run [`Config::validate`].
    pub fn load(path: impl AsRef<Path>) -> Result<Config, ConfigError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        parse_text(&text).map_err(|e| parse_error(Some(path.to_path_buf()), &text, &e))
    }

    /// Serializes to TOML text (passwords included; call [`Config::redacted`] first for logs).
    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).expect("config always serializes")
    }

    /// Makes relative file references absolute against `config_dir`: TLS CA/cert/key paths and
    /// `file:` / `record:` audio paths. Does not check existence. Call after loading, before
    /// [`Config::warnings`]; note that [`Config::to_toml`] then emits the resolved paths.
    pub fn resolve_paths(&mut self, config_dir: &Path) {
        fn fix(field: &mut String, dir: &Path) {
            if !field.is_empty() && Path::new(field.as_str()).is_relative() {
                *field = dir.join(&*field).to_string_lossy().into_owned();
            }
        }
        for a in &mut self.accounts {
            fix(&mut a.tls_ca_file, config_dir);
            fix(&mut a.tls_cert, config_dir);
            fix(&mut a.tls_key, config_dir);
            if let AudioSource::File(p) = &mut a.audio_source
                && p.is_relative()
            {
                *p = config_dir.join(&*p);
            }
            if let AudioSink::Record(p) = &mut a.audio_sink
                && p.is_relative()
            {
                *p = config_dir.join(&*p);
            }
        }
    }

    /// Non-fatal problems: referenced TLS CA/cert/key files that do not exist. Paths are
    /// checked as written, so call [`Config::resolve_paths`] first.
    pub fn warnings(&self) -> Vec<ConfigIssue> {
        let mut out = Vec::new();
        for (i, a) in self.accounts.iter().enumerate() {
            for (field, value) in [
                ("tls_ca_file", &a.tls_ca_file),
                ("tls_cert", &a.tls_cert),
                ("tls_key", &a.tls_key),
            ] {
                if !value.is_empty() && !Path::new(value).exists() {
                    out.push(ConfigIssue {
                        path: format!("account[{i}].{field}"),
                        message: format!("file {value:?} does not exist"),
                    });
                }
            }
        }
        for s in shortcuts::inert(&self.ui.shortcuts, cfg!(target_os = "macos")) {
            out.push(ConfigIssue {
                path: format!("ui.shortcuts.{}", s.name()),
                message:
                    "Cmd is the macOS Command key; this chord never fires here (use Mod or Ctrl)"
                        .into(),
            });
        }
        out
    }

    /// Checks cross-field and range constraints, returning every problem found.
    pub fn validate(&self) -> Result<(), Vec<ConfigIssue>> {
        let mut v = Validator::default();
        v.general(&self.general);
        v.control(&self.control);
        v.network(&self.network);
        v.media(&self.media);
        v.speech(&self.speech);
        v.calls(&self.calls);
        v.ui(&self.ui);
        v.accounts(&self.accounts);
        if v.issues.is_empty() {
            Ok(())
        } else {
            Err(v.issues)
        }
    }
}

#[derive(Default)]
struct Validator {
    issues: Vec<ConfigIssue>,
}

impl Validator {
    fn err(&mut self, path: impl Into<String>, message: impl Into<String>) {
        self.issues.push(ConfigIssue {
            path: path.into(),
            message: message.into(),
        });
    }

    fn general(&mut self, g: &GeneralConfig) {
        if let Err(e) = log_level::validate_log_level(&g.log_level) {
            self.err("general.log_level", e);
        }
        no_newlines(self, "general.user_agent", &g.user_agent);
    }

    fn control(&mut self, c: &ControlConfig) {
        if !c.tcp_listen.is_empty() {
            match c.tcp_listen.parse::<SocketAddr>() {
                Err(_) => self.err(
                    "control.tcp_listen",
                    format!(
                        "{:?} is not a socket address like 127.0.0.1:5079",
                        c.tcp_listen
                    ),
                ),
                Ok(addr) if !addr.ip().is_loopback() => self.err(
                    "control.tcp_listen",
                    format!(
                        "{} is not a loopback address; the control API only listens on 127.0.0.0/8 or ::1",
                        addr.ip()
                    ),
                ),
                Ok(_) => {}
            }
        }
        if !c.named_pipe.is_empty() && !c.named_pipe.starts_with(r"\\.\pipe\") {
            self.err("control.named_pipe", r"must start with \\.\pipe\");
        }
        if c.unix_socket.contains('\0') {
            self.err("control.unix_socket", "must not contain NUL");
        }
    }

    fn network(&mut self, n: &NetworkConfig) {
        if n.probe_secs != 0 && !(5..=3600).contains(&n.probe_secs) {
            self.err(
                "network.probe_secs",
                "must be 0 (events only) or between 5 and 3600",
            );
        }
        if !(100..=60_000).contains(&n.debounce_ms) {
            self.err("network.debounce_ms", "must be between 100 and 60000");
        }
    }

    fn media(&mut self, m: &MediaConfig) {
        let [start, end] = m.rtp_port_range;
        if start < 1024 {
            self.err("media.rtp_port_range", "start must be at least 1024");
        }
        if start % 2 != 0 {
            self.err(
                "media.rtp_port_range",
                "start must be even (RTP/RTCP pairs)",
            );
        }
        if end < start || end - start < 1 {
            self.err(
                "media.rtp_port_range",
                "range must span at least 2 ports (end > start)",
            );
        }
        if m.codecs.is_empty() {
            self.err("media.codecs", "must list at least one codec");
        }
        duplicates(self, "media.codecs", &m.codecs);
        if m.jitter_max_ms == 0 {
            self.err("media.jitter_max_ms", "must be greater than 0");
        }
        if m.jitter_max_ms > 1000 {
            self.err("media.jitter_max_ms", "must be at most 1000");
        }
        if m.jitter_min_ms > m.jitter_max_ms {
            self.err("media.jitter_min_ms", "must not exceed jitter_max_ms");
        }
        if !(10..=10_000).contains(&m.dtmf_duration_ms) {
            self.err("media.dtmf_duration_ms", "must be between 10 and 10000");
        }
        if m.dtmf_gap_ms > 5_000 {
            self.err("media.dtmf_gap_ms", "must be at most 5000");
        }
        if !(5..=600).contains(&m.rx_history_secs) {
            self.err("media.rx_history_secs", "must be between 5 and 600");
        }
    }

    fn calls(&mut self, c: &CallsConfig) {
        if !(400..=699).contains(&c.decline_code) {
            self.err(
                "calls.decline_code",
                "must be a SIP status code between 400 and 699",
            );
        }
    }

    fn ui(&mut self, u: &UiConfig) {
        for (name, chord) in &u.shortcuts {
            let p = format!("ui.shortcuts.{name}");
            if shortcuts::Shortcut::from_name(name).is_none() {
                let known: Vec<&str> = shortcuts::Shortcut::all()
                    .into_iter()
                    .map(shortcuts::Shortcut::name)
                    .collect();
                self.err(
                    &p,
                    format!("unknown shortcut action (known: {})", known.join(", ")),
                );
            }
            match shortcuts::Chord::parse(chord) {
                Err(e) => self.err(&p, format!("not a key chord such as \"Mod+Shift+A\": {e}")),
                Ok(c) => {
                    if let Err(e) = c.bindable() {
                        self.err(&p, e);
                    }
                }
            }
        }
        for (a, b) in shortcuts::conflicts(&u.shortcuts, cfg!(target_os = "macos")) {
            // Name the override(s); a default never collides with another default.
            for (x, y) in [(a, b), (b, a)] {
                if x.is_overridden(&u.shortcuts) {
                    self.err(
                        format!("ui.shortcuts.{}", x.name()),
                        format!(
                            "the same key press as `{}` ({})",
                            y.name(),
                            y.effective(&u.shortcuts)
                        ),
                    );
                }
            }
        }
    }

    fn speech(&mut self, sp: &SpeechConfig) {
        for (key, argv, allowed, timeout) in [
            ("tts", &sp.tts_command, TTS_PLACEHOLDERS, sp.tts_timeout_ms),
            ("stt", &sp.stt_command, STT_PLACEHOLDERS, sp.stt_timeout_ms),
        ] {
            let path = format!("speech.{key}_command");
            if !argv.is_empty() && argv[0].trim().is_empty() {
                self.err(&path, "the program (first element) must not be empty");
            }
            for (i, arg) in argv.iter().enumerate() {
                if arg.contains('\0') {
                    self.err(&path, format!("element {i} must not contain NUL"));
                }
                match template_placeholders(arg) {
                    Err(e) => self.err(&path, format!("element {i}: {e}")),
                    Ok(names) => {
                        for n in names.iter().filter(|n| !allowed.contains(&n.as_str())) {
                            self.err(
                                &path,
                                format!(
                                    "element {i}: unknown placeholder `{{{n}}}` (allowed: {})",
                                    allowed.join(", ")
                                ),
                            );
                        }
                    }
                }
            }
            if !(SPEECH_TIMEOUT_MIN_MS..=SPEECH_TIMEOUT_MAX_MS).contains(&timeout) {
                self.err(
                    format!("speech.{key}_timeout_ms"),
                    format!("must be between {SPEECH_TIMEOUT_MIN_MS} and {SPEECH_TIMEOUT_MAX_MS}"),
                );
            }
        }
        for (path, value) in [
            ("speech.tts_voice", &sp.tts_voice),
            ("speech.stt_language", &sp.stt_language),
        ] {
            if !value.is_empty() && !is_speech_token(value) {
                self.err(
                    path,
                    "must match [A-Za-z0-9][A-Za-z0-9_.+:-]{0,63} (no leading `-`)",
                );
            }
        }
    }

    fn accounts(&mut self, accounts: &[AccountConfig]) {
        let mut first_by_id: HashMap<String, usize> = HashMap::new();
        let mut ports: HashMap<(u16, bool), usize> = HashMap::new();
        for (i, a) in accounts.iter().enumerate() {
            let p = format!("account[{i}]");
            if a.id.is_empty() {
                self.err(format!("{p}.id"), "must not be empty");
            } else {
                if !a
                    .id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
                {
                    self.err(
                        format!("{p}.id"),
                        format!("{:?} may only contain A-Z a-z 0-9 _ . -", a.id),
                    );
                }
                if let Some(&first) = first_by_id.get(a.id.to_ascii_lowercase().as_str()) {
                    self.err(
                        format!("{p}.id"),
                        format!(
                            "duplicate id {:?} (also account[{first}]); ids are compared case-insensitively",
                            a.id
                        ),
                    );
                } else {
                    first_by_id.insert(a.id.to_ascii_lowercase(), i);
                }
            }
            if a.enabled {
                if a.local_port != 0 {
                    // UDP and TCP/TLS are separate port spaces.
                    let key = (
                        a.local_port,
                        !matches!(a.transport, Transport::Tcp | Transport::Tls),
                    );
                    if let Some(&first) = ports.get(&key) {
                        self.err(
                            format!("{p}.local_port"),
                            format!(
                                "port {} already used by enabled account[{first}]",
                                a.local_port
                            ),
                        );
                    } else {
                        ports.insert(key, i);
                    }
                }
                if a.username.is_empty() {
                    self.err(format!("{p}.username"), "required for enabled accounts");
                }
                if a.domain.is_empty() {
                    self.err(format!("{p}.domain"), "required for enabled accounts");
                }
            }
            self.account(&p, a);
        }
    }

    fn account(&mut self, p: &str, a: &AccountConfig) {
        for (field, value) in [
            ("username", &a.username),
            ("auth_username", &a.auth_username),
        ] {
            if value.contains(char::is_whitespace) || value.contains('@') {
                self.err(format!("{p}.{field}"), "must not contain whitespace or '@'");
            }
        }
        for (field, value) in [
            ("domain", &a.domain),
            ("registrar", &a.registrar),
            ("outbound_proxy", &a.outbound_proxy),
            ("tls_server_name", &a.tls_server_name),
            ("mwi_uri", &a.mwi_uri),
        ] {
            if value.contains(char::is_whitespace) {
                self.err(format!("{p}.{field}"), "must not contain whitespace");
            }
        }
        if a.domain.contains(['@', '/']) {
            self.err(
                format!("{p}.domain"),
                "must be a bare host name (no '@' or '/')",
            );
        }
        if a.password_store == PasswordStore::Keyring && !a.password.is_empty() {
            self.err(
                format!("{p}.password"),
                "must be empty with password_store = \"keyring\" (the password lives in the system credential store; remove this key or set password_store = \"file\")",
            );
        }
        no_newlines(self, &format!("{p}.display_name"), &a.display_name);
        no_newlines(self, &format!("{p}.user_agent"), &a.user_agent);

        if !a.bind_address.is_empty() && a.bind_address.parse::<IpAddr>().is_err() {
            self.err(
                format!("{p}.bind_address"),
                format!("{:?} is not an IP address", a.bind_address),
            );
        }
        if a.public_address.contains(char::is_whitespace) {
            self.err(format!("{p}.public_address"), "must not contain whitespace");
        }
        if a.keepalive_secs != 0 && !(5..=3600).contains(&a.keepalive_secs) {
            self.err(
                format!("{p}.keepalive_secs"),
                "must be 0 (off) or between 5 and 3600",
            );
        }
        if a.keepalive_fail_threshold > 20 {
            self.err(
                format!("{p}.keepalive_fail_threshold"),
                "must be 0 (off) or between 1 and 20",
            );
        }
        if !a.tls_fingerprint.is_empty() && parse_fingerprint(&a.tls_fingerprint).is_none() {
            self.err(
                format!("{p}.tls_fingerprint"),
                "must be a SHA-256 fingerprint: 64 hex digits, optionally colon-separated",
            );
        }
        if a.tls_cert.is_empty() != a.tls_key.is_empty() {
            self.err(
                format!("{p}.tls_key"),
                "tls_cert and tls_key must be set together",
            );
        }
        if a.register && !(10..=86_400).contains(&a.expires) {
            self.err(format!("{p}.expires"), "must be between 10 and 86400");
        }
        if a.srtp != SrtpPolicy::Off && a.srtp_suites.is_empty() {
            self.err(
                format!("{p}.srtp_suites"),
                "must list at least one suite when srtp is not off",
            );
        }
        duplicates(self, &format!("{p}.srtp_suites"), &a.srtp_suites);
        duplicates(self, &format!("{p}.codecs"), &a.codecs);
        if !(400..=699).contains(&a.dnd_reject_code) {
            self.err(
                format!("{p}.dnd_reject_code"),
                "must be a SIP status code between 400 and 699",
            );
        }
        if !(1..=86_400).contains(&a.min_se) {
            self.err(format!("{p}.min_se"), "must be between 1 and 86400");
        }
        if !(1..=86_400).contains(&a.session_expires) || a.session_expires < a.min_se {
            self.err(
                format!("{p}.session_expires"),
                "must be between 1 and 86400 and at least min_se",
            );
        }
        if a.max_calls == 0 {
            self.err(format!("{p}.max_calls"), "must be at least 1");
        }
        if a.answer_delay_ms > 60_000 {
            self.err(format!("{p}.answer_delay_ms"), "must be at most 60000");
        }
        if !(10..=86_400).contains(&a.blf_expires) {
            self.err(format!("{p}.blf_expires"), "must be between 10 and 86400");
        }

        const RESERVED: [&str; 7] = [
            "via",
            "from",
            "to",
            "call-id",
            "cseq",
            "contact",
            "content-length",
        ];
        for (name, value) in &a.extra_headers {
            let hp = format!("{p}.extra_headers.{name}");
            let token = !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c));
            if !token {
                self.err(&hp, "invalid header name");
            } else if RESERVED.contains(&name.to_ascii_lowercase().as_str()) {
                self.err(&hp, "header is managed by Siple and cannot be overridden");
            }
            no_newlines(self, &hp, value);
        }

        let mut seen = BTreeSet::new();
        for (j, blf) in a.blf.iter().enumerate() {
            let bp = format!("{p}.blf[{j}]");
            if blf.uri.trim().is_empty() {
                self.err(format!("{bp}.uri"), "must not be empty");
            } else if blf.uri.contains(char::is_whitespace) {
                self.err(format!("{bp}.uri"), "must not contain whitespace");
            } else if !seen.insert(blf.uri.as_str()) {
                self.err(
                    format!("{bp}.uri"),
                    format!("duplicate BLF uri {:?}", blf.uri),
                );
            }
            no_newlines(self, &format!("{bp}.group"), &blf.group);
            for (name, v) in [
                ("pickup_prefix", &blf.pickup_prefix),
                ("pickup_target", &blf.pickup_target),
            ] {
                if v.contains(char::is_whitespace) {
                    self.err(format!("{bp}.{name}"), "must not contain whitespace");
                }
            }
            let pk = blf.pickup_for(a);
            if pk.mode == PickupMode::Prefix && pk.target.is_empty() && pk.prefix.is_empty() {
                self.err(
                    format!("{bp}.pickup_prefix"),
                    "pickup = \"prefix\" needs a pickup_prefix or a pickup_target",
                );
            }
        }
        if a.pickup_prefix.contains(char::is_whitespace) {
            self.err(format!("{p}.pickup_prefix"), "must not contain whitespace");
        }
    }
}

fn no_newlines(v: &mut Validator, path: &str, value: &str) {
    if value.contains(['\r', '\n']) {
        v.err(path, "must not contain line breaks");
    }
}

fn duplicates<T: PartialEq + fmt::Debug>(v: &mut Validator, path: &str, items: &[T]) {
    for (i, item) in items.iter().enumerate() {
        if items[..i].contains(item) {
            v.err(path, format!("duplicate entry {item:?}"));
        }
    }
}

/// Parses a SHA-256 fingerprint: 64 hex digits, optionally `:`-separated, any case.
pub fn parse_fingerprint(s: &str) -> Option<[u8; 32]> {
    let hex: Vec<u8> = s.trim().bytes().filter(|b| *b != b':').collect();
    if hex.len() != 64 {
        return None;
    }
    let nibble = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let mut out = [0u8; 32];
    for (o, pair) in out.iter_mut().zip(hex.chunks(2)) {
        *o = nibble(pair[0])? << 4 | nibble(pair[1])?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn echo_truncates_long_user_values() {
        assert_eq!(echo("abc"), "\"abc\"");
        let long = "A".repeat(5000);
        let e = echo(&long);
        assert!(e.len() < 100, "{e}");
        assert!(e.contains("5000 chars"));
        let err = "UklGRg".repeat(1000).parse::<AudioSource>().unwrap_err();
        assert!(err.to_string().len() < 200, "{err}");
        assert!(echo("héllo wörld").starts_with('"'));
    }

    fn account(id: &str) -> AccountConfig {
        AccountConfig {
            id: id.into(),
            username: "1001".into(),
            domain: "pbx.example.com".into(),
            ..Default::default()
        }
    }

    fn issues(c: &Config) -> Vec<String> {
        c.validate()
            .err()
            .unwrap_or_default()
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    fn has(c: &Config, needle: &str) -> bool {
        issues(c).iter().any(|i| i.contains(needle))
    }

    #[test]
    fn empty_text_gives_defaults() {
        let c = Config::parse("").unwrap();
        assert_eq!(c, Config::default());
        assert!(c.validate().is_ok());
    }

    #[test]
    fn documented_defaults() {
        let c = Config::default();
        assert_eq!(
            c.general.user_agent,
            concat!("Siple/", env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(c.general.log_level, "info");
        assert_eq!(c.general.log_retention_days, 14);
        assert_eq!(c.control.tcp_listen, "");
        assert_eq!(c.media.rtp_port_range, [16384, 32767]);
        assert_eq!(
            c.media.codecs,
            [Codec::Opus, Codec::G722, Codec::Pcmu, Codec::Pcma]
        );
        assert_eq!((c.media.jitter_min_ms, c.media.jitter_max_ms), (40, 200));
        assert!(!c.media.rtcp_mux && !c.media.symmetric_rtp);
        assert_eq!(c.media.codec_answer_order, CodecAnswerOrder::Offer);
        let a = AccountConfig::default();
        assert!(a.enabled && a.tls_verify && a.register && a.auto_hold && a.sip_trace);
        assert!(a.follow_redirects && a.accept_refer);
        // Digest challenges of foreign hosts are refused unless opted in.
        assert!(!a.auth_foreign);
        assert_eq!(
            (a.expires, a.max_calls, a.dnd_reject_code, a.blf_expires),
            (300, 4, 486, 600)
        );
        assert_eq!(a.keepalive_secs, 30);
        assert_eq!(a.srtp, SrtpPolicy::Optional);
        assert_eq!(a.hold_style, HoldStyle::Sendonly);
        assert_eq!(a.mwi, MwiMode::Subscribe);
    }

    #[test]
    fn unknown_fields_rejected() {
        assert!(Config::parse("[general]\nbogus = 1\n").is_err());
        assert!(Config::parse("[[account]]\nid = \"a\"\nbogus = 1\n").is_err());
        assert!(Config::parse("[[account]]\n[[account.blf]]\nuri = \"1\"\nnope = 1\n").is_err());
        assert!(Config::parse("bogus = 1\n").is_err());
    }

    #[test]
    fn enum_values_parse() {
        let c = Config::parse(
            r#"
            [media]
            codecs = ["pcma", "opus"]
            dtmf_mode = "info"
            codec_answer_order = "local"
            [[account]]
            id = "a"
            transport = "tls"
            srtp = "mandatory"
            hold_style = "inactive"
            mwi = "unsolicited"
            dtmf_mode = "inband"
            attended_refer_target = "contact"
            attended_refer_on = "consult"
            "#,
        )
        .unwrap();
        let a = &c.accounts[0];
        assert_eq!(c.media.codecs, [Codec::Pcma, Codec::Opus]);
        assert_eq!(a.transport, Transport::Tls);
        assert_eq!(a.srtp, SrtpPolicy::Mandatory);
        assert_eq!(a.effective_dtmf_mode(&c.media), DtmfMode::Inband);
        assert_eq!(a.attended_refer_on, AttendedReferOn::Consult);
        assert!(Config::parse("[[account]]\ntransport = \"sctp\"\n").is_err());
        assert!(Config::parse("[[account]]\ndtmf_mode = \"morse\"\n").is_err());
    }

    #[test]
    fn suite_aliases_and_canonical_names() {
        let c = Config::parse(
            r#"
            [[account]]
            srtp_suites = ["AES_CM_256_HMAC_SHA1_80", "AES_256_CM_HMAC_SHA1_32", "AES_CM_256_HMAC_SHA1_32", "AES_CM_128_HMAC_SHA1_32"]
            "#,
        )
        .unwrap();
        assert_eq!(
            c.accounts[0].srtp_suites,
            [
                SrtpSuite::Aes256Cm80,
                SrtpSuite::Aes256Cm32,
                SrtpSuite::Aes256Cm32,
                SrtpSuite::Aes128Cm32
            ]
        );
        assert_eq!(SrtpSuite::Aes256Cm80.sdp_name(), "AES_256_CM_HMAC_SHA1_80");
        assert_eq!(
            (
                SrtpSuite::Aes256Cm32.master_key_len(),
                SrtpSuite::Aes256Cm32.auth_tag_len()
            ),
            (32, 4)
        );
        let out = Config {
            accounts: vec![account("a")],
            ..Default::default()
        }
        .to_toml();
        assert!(out.contains("AES_CM_128_HMAC_SHA1_80"));
        assert!(Config::parse("[[account]]\nsrtp_suites = [\"AES_GCM_128\"]\n").is_err());
    }

    #[test]
    fn audio_source_parser() {
        assert_eq!("device".parse(), Ok(AudioSource::Device));
        assert_eq!("silence".parse(), Ok(AudioSource::Silence));
        assert_eq!("tone:440".parse(), Ok(AudioSource::Tone(440.0)));
        assert_eq!("tone:1004.5".parse(), Ok(AudioSource::Tone(1004.5)));
        assert_eq!(
            "file:/tmp/a.wav".parse(),
            Ok(AudioSource::File("/tmp/a.wav".into()))
        );
        for bad in [
            "",
            "mic",
            "tone:",
            "tone:abc",
            "tone:0",
            "tone:-5",
            "tone:48000",
            "tone:NaN",
            "file:",
            "file:  ",
            "record:x",
        ] {
            assert!(
                bad.parse::<AudioSource>().is_err(),
                "{bad:?} should be rejected"
            );
        }
        assert_eq!(AudioSource::Tone(440.0).to_string(), "tone:440");
    }

    #[test]
    fn audio_sink_parser() {
        assert_eq!("device".parse(), Ok(AudioSink::Device));
        assert_eq!("null".parse(), Ok(AudioSink::Null));
        assert_eq!(
            "record:out.wav".parse(),
            Ok(AudioSink::Record("out.wav".into()))
        );
        for bad in ["", "silence", "record:", "record: ", "file:x.wav"] {
            assert!(
                bad.parse::<AudioSink>().is_err(),
                "{bad:?} should be rejected"
            );
        }
        let c = Config::parse(
            "[[account]]\naudio_source = \"tone:880\"\naudio_sink = \"record:/tmp/r.wav\"\n",
        )
        .unwrap();
        assert_eq!(c.accounts[0].audio_source, AudioSource::Tone(880.0));
        assert_eq!(
            c.accounts[0].audio_sink,
            AudioSink::Record("/tmp/r.wav".into())
        );
        assert!(Config::parse("[[account]]\naudio_source = \"tone:x\"\n").is_err());
        let out = Config {
            accounts: vec![account("a")],
            ..Default::default()
        }
        .to_toml();
        assert!(out.contains("audio_source = \"device\""));
    }

    #[test]
    fn inheritance() {
        let media = MediaConfig::default();
        let mut a = account("a");
        assert_eq!(a.effective_codecs(&media), media.codecs.as_slice());
        assert_eq!(a.effective_dtmf_mode(&media), DtmfMode::Auto);
        assert_eq!(a.effective_registrar(), "pbx.example.com");
        assert_eq!(a.effective_auth_username(), "1001");
        assert_eq!(
            a.effective_user_agent(&GeneralConfig::default()),
            concat!("Siple/", env!("CARGO_PKG_VERSION"))
        );
        a.codecs = vec![Codec::Pcmu];
        a.dtmf_mode = OrInherit::Value(DtmfMode::Info);
        a.registrar = "edge.example.com:5061".into();
        a.auth_username = "auth1001".into();
        a.user_agent = "Yealink".into();
        assert_eq!(a.effective_codecs(&media), [Codec::Pcmu]);
        assert_eq!(a.effective_dtmf_mode(&media), DtmfMode::Info);
        assert_eq!(a.effective_registrar(), "edge.example.com:5061");
        assert_eq!(a.effective_auth_username(), "auth1001");
        assert_eq!(a.effective_user_agent(&GeneralConfig::default()), "Yealink");
        assert_eq!(a.aor(), "sip:1001@pbx.example.com");
    }

    #[test]
    fn valid_config_passes() {
        let c = Config {
            accounts: vec![account("a"), account("b")],
            ..Default::default()
        };
        assert_eq!(c.validate(), Ok(()));
    }

    #[test]
    fn account_id_rules() {
        let mut c = Config {
            accounts: vec![account("a"), account("a"), account(""), account("bad/id")],
            ..Default::default()
        };
        assert!(has(
            &c,
            "account[1].id: duplicate id \"a\" (also account[0])"
        ));
        assert!(has(&c, "account[2].id: must not be empty"));
        assert!(has(&c, "account[3].id"));
        c.accounts.truncate(1);
        assert!(c.validate().is_ok());
    }

    #[test]
    fn enabled_accounts_need_identity_disabled_do_not() {
        let mut a = AccountConfig {
            id: "x".into(),
            ..Default::default()
        };
        let c = Config {
            accounts: vec![a.clone()],
            ..Default::default()
        };
        assert!(has(&c, "account[0].username: required"));
        assert!(has(&c, "account[0].domain: required"));
        a.enabled = false;
        assert!(
            Config {
                accounts: vec![a],
                ..Default::default()
            }
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn range_checks() {
        let mut c = Config::default();
        c.media.rtp_port_range = [16385, 16390];
        assert!(has(&c, "start must be even"));
        c.media.rtp_port_range = [16384, 16384];
        assert!(has(&c, "at least 2 ports"));
        c.media.rtp_port_range = [16384, 16000];
        assert!(has(&c, "at least 2 ports"));
        c.media.rtp_port_range = [100, 200];
        assert!(has(&c, "at least 1024"));
        c.media.rtp_port_range = [16384, 16385];
        assert!(c.validate().is_ok());

        let mut c = Config::default();
        c.media.jitter_min_ms = 300;
        assert!(has(&c, "media.jitter_min_ms"));
        c.media.codecs.clear();
        assert!(has(&c, "media.codecs: must list"));

        let mut a = account("a");
        a.dnd_reject_code = 200;
        a.max_calls = 0;
        a.expires = 1;
        a.keepalive_secs = 2;
        let c = Config {
            accounts: vec![a],
            ..Default::default()
        };
        for p in ["dnd_reject_code", "max_calls", "expires", "keepalive_secs"] {
            assert!(has(&c, &format!("account[0].{p}")), "{p}");
        }
        let mut a = account("a");
        a.dnd_reject_code = 699;
        assert!(
            Config {
                accounts: vec![a],
                ..Default::default()
            }
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn control_endpoints() {
        let mut c = Config::default();
        c.control.tcp_listen = "localhost".into();
        c.control.named_pipe = "siple".into();
        assert!(has(&c, "control.tcp_listen"));
        assert!(has(&c, "control.named_pipe"));
        c.control.tcp_listen = "127.0.0.1:5079".into();
        c.control.named_pipe = r"\\.\pipe\zp".into();
        assert!(c.validate().is_ok());
    }

    #[test]
    fn pickup_settings_resolve_and_validate() {
        let mut a = account("a");
        assert_eq!(
            (a.pickup, a.pickup_prefix.as_str()),
            (PickupMode::Off, "**")
        );
        a.blf = vec![
            BlfConfig {
                uri: "1004".into(),
                ..Default::default()
            },
            BlfConfig {
                uri: "1005".into(),
                pickup: OrInherit::Value(PickupMode::Replaces),
                pickup_prefix: "*97".into(),
                pickup_target: "8005".into(),
                group: "Support".into(),
                ..Default::default()
            },
        ];
        // Account Off, per-entry override wins.
        assert_eq!(a.blf[0].pickup_for(&a).mode, PickupMode::Off);
        let k = a.blf[1].pickup_for(&a);
        assert_eq!(
            (k.mode, k.prefix.as_str(), k.target.as_str()),
            (PickupMode::Replaces, "*97", "8005")
        );
        // Account Prefix is inherited, the prefix comes from the account.
        a.pickup = PickupMode::Prefix;
        let k = a.blf[0].pickup_for(&a);
        assert_eq!((k.mode, k.prefix.as_str()), (PickupMode::Prefix, "**"));
        let c = Config {
            accounts: vec![a.clone()],
            ..Default::default()
        };
        assert!(c.validate().is_ok(), "{:?}", issues(&c));
        // Prefix mode without prefix and target is refused; a target alone is fine.
        a.pickup_prefix.clear();
        let c = Config {
            accounts: vec![a.clone()],
            ..Default::default()
        };
        assert!(has(&c, "blf[0].pickup_prefix"), "{:?}", issues(&c));
        a.blf[0].pickup_target = "8004".into();
        a.blf[1].pickup = OrInherit::Inherit;
        a.blf[1].pickup_prefix.clear();
        a.blf[1].pickup_target.clear();
        let c = Config {
            accounts: vec![a.clone()],
            ..Default::default()
        };
        assert!(has(&c, "blf[1].pickup_prefix"), "{:?}", issues(&c));
        assert!(!has(&c, "blf[0].pickup_prefix"));
        a.blf[1].pickup_target = "bad target".into();
        a.pickup_prefix = "* *".into();
        let c = Config {
            accounts: vec![a],
            ..Default::default()
        };
        assert!(has(&c, "blf[1].pickup_target"));
        assert!(has(&c, ".pickup_prefix: must not contain whitespace"));
    }

    #[test]
    fn pickup_keys_parse_from_toml() {
        let c = Config::parse(
            "[[account]]\nid = \"a\"\npickup = \"prefix\"\npickup_prefix = \"*8\"\n[[account.blf]]\nuri = \"1\"\npickup = \"\"\n[[account.blf]]\nuri = \"2\"\npickup = \"replaces\"\ngroup = \"G\"\npickup_target = \"7\"\n",
        )
        .unwrap();
        let a = &c.accounts[0];
        assert_eq!(
            (a.pickup, a.pickup_prefix.as_str()),
            (PickupMode::Prefix, "*8")
        );
        assert_eq!(a.blf[0].pickup, OrInherit::Inherit);
        assert_eq!(a.blf[1].pickup, OrInherit::Value(PickupMode::Replaces));
        assert_eq!(
            (a.blf[1].group.as_str(), a.blf[1].pickup_target.as_str()),
            ("G", "7")
        );
        assert!(Config::parse("[[account]]\nid = \"a\"\npickup = \"sometimes\"\n").is_err());
        assert_eq!(a.pickup_replaces_target, PickupReplacesTarget::Caller);
        let m = Config::parse("[[account]]\nid = \"a\"\npickup_replaces_target = \"monitored\"\n")
            .unwrap();
        assert_eq!(
            m.accounts[0].pickup_replaces_target,
            PickupReplacesTarget::Monitored
        );
        assert!(
            Config::parse("[[account]]\nid = \"a\"\npickup_replaces_target = \"x\"\n").is_err()
        );
    }

    #[test]
    fn blf_extension_is_the_user_part() {
        let ext = |u: &str| {
            BlfConfig {
                uri: u.into(),
                ..Default::default()
            }
            .extension()
        };
        assert_eq!(ext("1004"), "1004");
        assert_eq!(ext("sip:1004@pbx.example"), "1004");
        assert_eq!(ext("SIPS:1004@pbx.example:5061"), "1004");
        assert_eq!(ext("1004@pbx.example"), "1004");
        assert_eq!(ext("sip:1004;phone-context=x@pbx"), "1004");
        assert_eq!(ext("*97"), "*97");
        assert_eq!(ext("sip:queue"), "sip:queue");
    }

    #[test]
    fn misc_account_rules() {
        let mut a = account("a");
        a.bind_address = "not-an-ip".into();
        a.tls_cert = "cert.pem".into();
        a.srtp_suites.clear();
        a.codecs = vec![Codec::Opus, Codec::Opus];
        a.extra_headers.insert("Call-ID".into(), "x".into());
        a.extra_headers.insert("X-Ok".into(), "bad\r\nvalue".into());
        a.extra_headers.insert("bad name".into(), "v".into());
        a.blf = vec![
            BlfConfig {
                uri: "".into(),
                label: String::new(),
                ..Default::default()
            },
            BlfConfig {
                uri: "1002".into(),
                label: String::new(),
                ..Default::default()
            },
            BlfConfig {
                uri: "1002".into(),
                label: String::new(),
                ..Default::default()
            },
        ];
        let c = Config {
            accounts: vec![a],
            ..Default::default()
        };
        for needle in [
            "bind_address",
            "tls_key",
            "srtp_suites",
            "codecs: duplicate",
            "extra_headers.Call-ID",
            "extra_headers.X-Ok",
            "extra_headers.bad name",
            "blf[0].uri: must not be empty",
            "blf[2].uri: duplicate",
        ] {
            assert!(
                has(&c, needle),
                "missing issue for {needle}: {:?}",
                issues(&c)
            );
        }
    }

    #[test]
    fn duplicate_local_ports() {
        let mut a = account("a");
        a.local_port = 5060;
        let mut b = account("b");
        b.local_port = 5060;
        let mut c = Config {
            accounts: vec![a, b],
            ..Default::default()
        };
        assert!(has(&c, "account[1].local_port"));
        c.accounts[1].transport = Transport::Tcp; // different port space
        assert!(c.validate().is_ok());
    }

    #[test]
    fn invalid_log_level() {
        let mut c = Config::default();
        c.general.log_level = "info,=bad[".into();
        assert!(has(&c, "general.log_level"));
        c.general.log_level = "debug,siple_core=trace".into();
        assert!(c.validate().is_ok());
    }

    #[test]
    fn log_level_typos_rejected() {
        let mut c = Config::default();
        c.general.log_level = "inf".into();
        assert!(has(&c, "general.log_level"));
        for ok in ["info", "debug,rsipstack=trace", "siple_core::sip=debug"] {
            c.general.log_level = ok.into();
            assert!(c.validate().is_ok(), "{ok}");
        }
    }

    #[test]
    fn parse_errors_never_leak_secrets() {
        for text in [
            "[[account]]\nid = \"a\"\npassword = hunter2secret\n",
            "[[account]]\nid = \"a\"\n  Password   =  hunter2secret # x\n",
            "[[account]]\nid = \"a\"\npassword = 12345678secret\n", // gitleaks:allow (test fixture)
            "[[account]]\nid = \"a\"\npassword = [\"hunter2secret\"]\n",
        ] {
            let msg = Config::parse(text).unwrap_err().to_string();
            assert!(
                !msg.contains("hunter2secret") && !msg.contains("12345678secret"),
                "{msg}"
            );
            assert!(msg.contains("line 3"), "{msg}");
        }
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.toml");
        std::fs::write(&p, "[[account]]\npassword = hunter2secret\n").unwrap();
        let msg = Config::load(&p).unwrap_err().to_string();
        assert!(
            msg.contains("c.toml") && msg.contains("line 2, column"),
            "{msg}"
        );
        assert!(!msg.contains("hunter2secret"), "{msg}");
        // Non-secret errors keep position and snippet.
        let msg = Config::parse("[general]\nuser_agent = 5\n")
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("line 2") && msg.contains("user_agent = 5"),
            "{msg}"
        );
    }

    #[test]
    fn tcp_listen_must_be_loopback() {
        let mut c = Config::default();
        for bad in [
            "0.0.0.0:5079",
            "192.168.1.5:5079",
            "[::]:5079",
            "[2001:db8::1]:5079",
        ] {
            c.control.tcp_listen = bad.into();
            assert!(has(&c, "loopback"), "{bad}");
        }
        for ok in ["127.0.0.1:5079", "127.5.5.5:1", "[::1]:5079"] {
            c.control.tcp_listen = ok.into();
            assert!(c.validate().is_ok(), "{ok}");
        }
    }

    #[test]
    fn account_ids_unique_case_insensitively() {
        let c = Config {
            accounts: vec![account("Office"), account("office")],
            ..Default::default()
        };
        assert!(has(
            &c,
            "account[1].id: duplicate id \"office\" (also account[0])"
        ));
    }

    #[test]
    fn tls_fingerprint_format() {
        let hex = "AB".repeat(32);
        let colons = vec!["ab"; 32].join(":");
        assert_eq!(parse_fingerprint(&hex), Some([0xab; 32]));
        assert_eq!(parse_fingerprint(&colons), Some([0xab; 32]));
        assert_eq!(parse_fingerprint("abcd"), None);
        assert_eq!(parse_fingerprint(&"zz".repeat(32)), None);
        let mut c = Config {
            accounts: vec![account("a")],
            ..Default::default()
        };
        c.accounts[0].tls_fingerprint = "nope".into();
        assert!(has(&c, "account[0].tls_fingerprint"));
        c.accounts[0].tls_fingerprint = colons;
        assert!(!has(&c, "tls_fingerprint"));
    }

    #[test]
    fn jitter_upper_bound() {
        let mut c = Config::default();
        c.media.jitter_max_ms = 1001;
        assert!(has(&c, "media.jitter_max_ms: must be at most 1000"));
        c.media.jitter_max_ms = 1000;
        assert!(c.validate().is_ok());
    }

    #[test]
    fn dtmf_gap_upper_bound_matches_media_engine() {
        let mut c = Config::default();
        c.media.dtmf_gap_ms = 5001;
        assert!(has(&c, "media.dtmf_gap_ms: must be at most 5000"));
        c.media.dtmf_gap_ms = 5000;
        assert!(c.validate().is_ok());
    }

    #[test]
    fn resolve_paths_and_warnings() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ca.pem"), "x").unwrap();
        let mut a = account("a");
        a.tls_ca_file = "ca.pem".into();
        a.tls_cert = "missing.pem".into();
        // Absolute on every platform ("/abs/..." has no drive letter, so it is relative on Windows).
        let abs_key = std::env::temp_dir().join("abs").join("key.pem");
        a.tls_key = abs_key.to_string_lossy().into_owned();
        a.audio_source = AudioSource::File("in.wav".into());
        a.audio_sink = AudioSink::Record("out.wav".into());
        let mut c = Config {
            accounts: vec![a],
            ..Default::default()
        };
        c.resolve_paths(dir.path());
        let a = &c.accounts[0];
        assert_eq!(Path::new(&a.tls_ca_file), dir.path().join("ca.pem"));
        assert_eq!(Path::new(&a.tls_key), abs_key);
        assert_eq!(a.audio_source, AudioSource::File(dir.path().join("in.wav")));
        assert_eq!(a.audio_sink, AudioSink::Record(dir.path().join("out.wav")));
        assert!(c.validate().is_ok() || !has(&c, "tls_ca_file"));
        let w: Vec<_> = c.warnings().iter().map(ToString::to_string).collect();
        assert_eq!(w.len(), 2, "{w:?}");
        assert!(w[0].starts_with("account[0].tls_cert") && w[1].starts_with("account[0].tls_key"));
    }

    #[test]
    fn redaction() {
        let mut a = account("a");
        a.password = Secret::new("hunter2");
        let c = Config {
            accounts: vec![a],
            ..Default::default()
        };
        assert!(!format!("{c:?}").contains("hunter2"));
        let r = c.redacted();
        assert_eq!(r.accounts[0].password.expose(), "***");
        assert!(!r.to_toml().contains("hunter2"));
        assert_eq!(c.accounts[0].password.expose(), "hunter2");
    }

    #[test]
    fn load_reports_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing = Config::load(dir.path().join("nope.toml")).unwrap_err();
        assert!(missing.to_string().contains("nope.toml"));
        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, "[general]\nuser_agent = 5\n").unwrap();
        let e = Config::load(&bad).unwrap_err();
        assert!(e.to_string().contains("bad.toml"), "{e}");
        let good = dir.path().join("good.toml");
        std::fs::write(&good, "[general]\nlog_level = \"debug\"\n").unwrap();
        assert_eq!(Config::load(&good).unwrap().general.log_level, "debug");
    }

    // --- example file drift -------------------------------------------------

    fn key_paths(v: &toml::Value, prefix: &str, out: &mut BTreeSet<String>) {
        match v {
            toml::Value::Table(t) => {
                for (k, v) in t {
                    let p = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    };
                    out.insert(p.clone());
                    key_paths(v, &p, out);
                }
            }
            toml::Value::Array(a) if !a.is_empty() && a.iter().all(toml::Value::is_table) => {
                for item in a {
                    key_paths(item, &format!("{prefix}[]"), out);
                }
            }
            _ => {}
        }
    }

    fn paths_of(v: &toml::Value) -> BTreeSet<String> {
        let mut s = BTreeSet::new();
        key_paths(v, "", &mut s);
        s
    }

    #[test]
    fn profile_fills_only_unset_options() {
        let c = Config::parse(
            r#"
[[account]]
id = "y"
profile = "yealink"
hold_style = "inactive"
user_agent = ""

[[account]]
id = "g"

[[account]]
id = "c"
profile = "cisco"
session_timers = false
"#,
        )
        .unwrap();
        let (y, g, ci) = (&c.accounts[0], &c.accounts[1], &c.accounts[2]);
        // Profile value where unset (empty inherit-string counts as unset) ...
        assert_eq!(y.user_agent, "Yealink SIP-T54W 96.86.0.70");
        assert_eq!(y.dnd_reject_code, 480);
        assert_eq!(y.prack, PrackMode::Disabled);
        assert!(!y.session_timers);
        assert_eq!(y.dtmf_mode, OrInherit::Value(DtmfMode::Rfc4733));
        // ... explicit config wins ...
        assert_eq!(y.hold_style, HoldStyle::Inactive);
        assert!(!ci.session_timers);
        assert_eq!(ci.user_agent, "Cisco-CP-8845-3PCC/12.0.1");
        assert_eq!(ci.hold_style, HoldStyle::Inactive);
        // ... generic keeps the built-in defaults.
        assert_eq!(
            *g,
            AccountConfig {
                id: "g".into(),
                ..Default::default()
            }
        );
    }

    #[test]
    fn profile_errors_keep_positions_and_unknown_profile_is_rejected() {
        let e = Config::parse("[[account]]\nprofile = \"nokia\"\n")
            .unwrap_err()
            .to_string();
        assert!(e.contains("line 2"), "{e}");
    }

    #[test]
    fn supported_tokens_follow_prack_and_timers() {
        let mut a = AccountConfig::default();
        assert_eq!(a.supported_tokens(), "replaces, 100rel, timer");
        a.prack = PrackMode::Disabled;
        a.session_timers = false;
        assert_eq!(a.supported_tokens(), "replaces");
    }

    #[test]
    fn session_timer_ranges_validated() {
        let mut c = Config::default();
        let mut a = account("a");
        a.min_se = 100;
        a.session_expires = 90;
        c.accounts.push(a);
        assert!(has(&c, "session_expires"));
    }

    #[test]
    fn speech_and_agent_audio_defaults() {
        let c = Config::default();
        assert_eq!(c.audio.backend, AudioBackend::Auto);
        assert_eq!(c.media.rx_history_secs, 60);
        assert!(c.speech.tts_command.is_empty() && c.speech.stt_command.is_empty());
        assert_eq!(
            (c.speech.tts_timeout_ms, c.speech.stt_timeout_ms),
            (30_000, 60_000)
        );
        assert_eq!(
            (c.speech.tts_voice.as_str(), c.speech.stt_language.as_str()),
            ("", "en")
        );
        let c = Config::parse("[audio]\nbackend = \"none\"\n").unwrap();
        assert_eq!(c.audio.backend, AudioBackend::None);
        let c = Config::parse("[audio]\nbackend = \"always\"\n").unwrap();
        assert_eq!(c.audio.backend, AudioBackend::Always);
        assert!(Config::parse("[audio]\nbackend = \"pulse\"\n").is_err());
        assert!(Config::parse("[speech]\nbogus = 1\n").is_err());
    }

    #[test]
    fn network_defaults_and_ranges() {
        let c = Config::default();
        assert_eq!(
            c.network,
            NetworkConfig {
                watch: true,
                probe_secs: 30,
                debounce_ms: 1500,
                move_calls: true
            }
        );
        assert_eq!(c.accounts.first().map(|a| a.keepalive_fail_threshold), None);
        assert_eq!(AccountConfig::default().keepalive_fail_threshold, 2);
        let mut c = Config::default();
        for ok in [0, 5, 30, 3600] {
            c.network.probe_secs = ok;
            assert_eq!(c.validate(), Ok(()), "{ok}");
        }
        for bad in [1, 4, 3601] {
            c.network.probe_secs = bad;
            assert!(has(&c, "network.probe_secs"), "{bad}");
        }
        c.network.probe_secs = 30;
        for bad in [0, 99, 60_001] {
            c.network.debounce_ms = bad;
            assert!(has(&c, "network.debounce_ms"), "{bad}");
        }
        c.network.debounce_ms = 1500;
        let mut a = account("a");
        for ok in [0, 1, 20] {
            a.keepalive_fail_threshold = ok;
            c.accounts = vec![a.clone()];
            assert_eq!(c.validate(), Ok(()), "{ok}");
        }
        a.keepalive_fail_threshold = 21;
        c.accounts = vec![a];
        assert!(has(&c, "keepalive_fail_threshold"));
        assert!(Config::parse("[network]\nbogus = 1\n").is_err());
        let c = Config::parse("[network]\nwatch = false\nprobe_secs = 0\n").unwrap();
        assert!(!c.network.watch && c.network.probe_secs == 0 && c.network.debounce_ms == 1500);
        assert!(c.network.move_calls, "move_calls defaults to true");
        let c = Config::parse("[network]\nmove_calls = false\n").unwrap();
        assert!(!c.network.move_calls);
    }

    #[test]
    fn calls_hold_scope() {
        assert_eq!(Config::default().calls.hold_scope, HoldScope::All);
        let c = Config::parse("[calls]\nhold_scope = \"account\"\n").unwrap();
        assert_eq!(c.calls.hold_scope, HoldScope::Account);
        assert!(Config::parse("[calls]\nhold_scope = \"world\"\n").is_err());
        assert!(Config::parse("[calls]\nbogus = 1\n").is_err());
        assert!(Config::default().calls.resume_after_failed_transfer);
        let c = Config::parse("[calls]\nresume_after_failed_transfer = false\n").unwrap();
        assert!(!c.calls.resume_after_failed_transfer);
    }

    #[test]
    fn rx_history_range() {
        let mut c = Config::default();
        for ok in [5, 60, 600] {
            c.media.rx_history_secs = ok;
            assert_eq!(c.validate(), Ok(()), "{ok}");
        }
        for bad in [0, 4, 601] {
            c.media.rx_history_secs = bad;
            assert!(has(&c, "media.rx_history_secs"), "{bad}");
        }
    }

    #[test]
    fn speech_placeholders_validated() {
        let argv = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let mut c = Config::default();
        c.speech.tts_command = argv(&[
            "piper",
            "--output_file",
            "{out}",
            "-f",
            "{text_file}",
            "{voice}",
            "{{x}}",
        ]);
        c.speech.stt_command = argv(&[
            "whisper-cli",
            "-f",
            "{in}",
            "-l",
            "{lang}",
            "{out_base}",
            "{out_txt}",
        ]);
        assert_eq!(c.validate(), Ok(()));
        c.speech.tts_command = argv(&["tts", "{in}"]);
        assert!(has(
            &c,
            "speech.tts_command: element 1: unknown placeholder `{in}`"
        ));
        c.speech.tts_command = argv(&["tts", "{out"]);
        assert!(has(&c, "speech.tts_command: element 1: unclosed"));
        c.speech.tts_command = argv(&["tts", "a}b"]);
        assert!(has(&c, "unmatched `}`"));
        c.speech.tts_command = argv(&["tts", "{}"]);
        assert!(has(&c, "invalid placeholder"));
        c.speech.tts_command = argv(&[" "]);
        assert!(has(&c, "program (first element) must not be empty"));
        c.speech.tts_command = vec![];
        c.speech.stt_command = argv(&["stt", "{text_file}"]);
        assert!(has(
            &c,
            "speech.stt_command: element 1: unknown placeholder `{text_file}`"
        ));
    }

    #[test]
    fn speech_timeouts_and_tokens() {
        let mut c = Config::default();
        for bad in [999, SPEECH_TIMEOUT_MAX_MS + 1] {
            c.speech.tts_timeout_ms = bad;
            c.speech.stt_timeout_ms = bad;
            assert!(has(&c, "speech.tts_timeout_ms") && has(&c, "speech.stt_timeout_ms"));
        }
        c.speech.tts_timeout_ms = 1000;
        c.speech.stt_timeout_ms = SPEECH_TIMEOUT_MAX_MS;
        assert_eq!(c.validate(), Ok(()));
        for ok in ["", "en", "en_US-lessac-medium", "a.b+c:d", "Z"] {
            c.speech.tts_voice = ok.into();
            c.speech.stt_language = ok.into();
            assert_eq!(c.validate(), Ok(()), "{ok:?}");
        }
        for bad in ["-v", "a b", "x;y", "en/US", "a\n", "é", &"a".repeat(65)] {
            c.speech.tts_voice = bad.into();
            c.speech.stt_language = "en".into();
            assert!(has(&c, "speech.tts_voice"), "{bad:?}");
            c.speech.tts_voice.clear();
            c.speech.stt_language = bad.into();
            assert!(has(&c, "speech.stt_language"), "{bad:?}");
        }
        assert!(is_speech_token(&"a".repeat(64)));
    }

    #[test]
    fn contact_rewrite_defaults_on_and_preserves_explicit_false() {
        let defaults = Config::parse("[[account]]\nid = \"a\"\n").unwrap();
        assert!(defaults.accounts[0].rewrite_contact);
        let disabled = Config::parse("[[account]]\nid = \"a\"\nrewrite_contact = false\n").unwrap();
        assert!(!disabled.accounts[0].rewrite_contact);
        let round_trip = Config::parse(&disabled.to_toml()).unwrap();
        assert!(!round_trip.accounts[0].rewrite_contact);
    }

    #[test]
    fn ui_calls_and_logging_keys_parse_and_validate() {
        let c = Config::parse(
            "[ui]\ntheme = \"dark\"\nblf_group_by = \"none\"\nreduced_motion = \"on\"\n\
             [ui.shortcuts]\nanswer = \"Mod+Shift+A\"\n[calls]\ndecline_code = 603\n\
             [logging]\nredact_credentials = true\n",
        )
        .unwrap();
        assert_eq!(c.validate(), Ok(()));
        assert_eq!(c.ui.theme, Theme::Dark);
        assert_eq!(c.ui.blf_group_by, BlfGroupBy::None);
        assert_eq!(c.ui.shortcuts["answer"], "Mod+Shift+A");
        assert_eq!(c.calls.decline_code, 603);
        assert!(c.logging.redact_credentials);
        // Defaults per plan 4.5.
        let d = UiConfig::default();
        assert_eq!(d.blf_click, BlfClick::Dial);
        assert_eq!(d.blf_click_in_call, BlfClickInCall::Menu);
        assert_eq!(d.blf_click_ringing, BlfClickRinging::Pickup);
        assert!(d.tray && d.close_to_tray && !d.developer_tools);
        assert_eq!(CallsConfig::default().decline_code, 486);
        assert!(!LoggingConfig::default().redact_credentials);

        assert!(Config::parse("[ui]\ntheme = \"blue\"\n").is_err());
        assert!(Config::parse("[ui]\nunknown = 1\n").is_err());
        let mut c = Config::default();
        c.calls.decline_code = 200;
        assert!(has(&c, "calls.decline_code"));
        let mut c = Config::default();
        c.ui.shortcuts.insert("bad name".into(), "x".into());
        c.ui.shortcuts.insert("answer".into(), " ".into());
        assert!(has(&c, "ui.shortcuts.bad name") && has(&c, "ui.shortcuts.answer"));
        // Known actions with real chords pass; unknown names and malformed chords do not.
        let mut c = Config::default();
        c.ui.shortcuts.insert("line_9".into(), "Alt+9".into());
        c.ui.shortcuts
            .insert("filter_extensions".into(), "/".into());
        assert_eq!(c.validate(), Ok(()));
        c.ui.shortcuts.insert("teleport".into(), "Mod+T".into());
        c.ui.shortcuts.insert("mute".into(), "Mod+Hyper+M".into());
        assert!(has(&c, "ui.shortcuts.teleport") && has(&c, "ui.shortcuts.mute"));
        // Chords that cannot work, and chords that collide with another action.
        for (name, chord) in [
            ("hangup", "Escape"),
            ("line_1", "1"),
            ("line_2", "Shift+2"),
            ("mute", "Mod+C"),
            ("mute", "Mod+Shift+E"),
        ] {
            let mut c = Config::default();
            c.ui.shortcuts.insert(name.into(), chord.into());
            assert!(has(&c, &format!("ui.shortcuts.{name}")), "{name} = {chord}");
        }
        let mut c = Config::default();
        c.ui.shortcuts.insert("mute".into(), "Mod+Shift+E".into());
        let msg = c.validate().unwrap_err()[0].message.clone();
        assert!(msg.contains("hangup"), "{msg}");
    }

    #[test]
    fn example_parses_and_validates() {
        let c = Config::parse(EXAMPLE_CONFIG).expect("example must parse");
        assert_eq!(c.validate(), Ok(()));
        assert_eq!(c.accounts.len(), 1);
        assert_eq!(c.accounts[0].blf.len(), 1);
    }

    #[test]
    fn example_matches_schema_keys() {
        let mut full = Config::default();
        full.accounts.push(AccountConfig::default());
        full.accounts[0].blf.push(BlfConfig::default());
        let schema = paths_of(&toml::Value::try_from(&full).unwrap());
        let example = paths_of(&toml::from_str(EXAMPLE_CONFIG).unwrap());
        // Keys deliberately shown as a commented example (`# key = ...`), e.g. so the versioned
        // default is picked up instead of being pinned by a copy of the example.
        let commented = [
            "general.user_agent",
            "account[].pickup",
            "account[].pickup_prefix",
            "account[].pickup_replaces_target",
            "account[].blf[].group",
            "account[].blf[].pickup",
            "account[].blf[].pickup_prefix",
            "account[].blf[].pickup_target",
        ];
        let missing: Vec<_> = schema
            .difference(&example)
            .filter(|k| {
                !commented.contains(&k.as_str())
                    || !EXAMPLE_CONFIG
                        .lines()
                        .any(|l| l.starts_with(&format!("# {} =", k.rsplit('.').next().unwrap())))
            })
            .collect();
        let stale: Vec<_> = example.difference(&schema).collect();
        assert!(
            missing.is_empty(),
            "in schema but not in config.example.toml: {missing:?}"
        );
        assert!(
            stale.is_empty(),
            "in config.example.toml but not in schema: {stale:?}"
        );
    }

    #[test]
    fn example_values_match_defaults() {
        let ex = Config::parse(EXAMPLE_CONFIG).unwrap();
        let d = Config::default();
        assert_eq!(ex.general, d.general);
        assert_eq!(ex.logging, d.logging);
        assert_eq!(ex.control, d.control);
        assert_eq!(ex.network, d.network);
        assert_eq!(ex.media, d.media);
        assert_eq!(ex.audio, d.audio);
        assert_eq!(ex.speech, d.speech);
        assert_eq!(ex.updates, d.updates);
        assert_eq!(ex.calls, d.calls);
        assert_eq!(ex.ui, d.ui);
        // The sample account differs from defaults only in identity and `enabled`.
        let a = AccountConfig {
            id: "example".into(),
            enabled: false,
            display_name: "Example".into(),
            username: "1001".into(),
            password: Secret::new("secret"),
            domain: "pbx.example.com".into(),
            blf: vec![BlfConfig {
                uri: "1002".into(),
                label: "Bob".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(ex.accounts[0], a);
    }
}

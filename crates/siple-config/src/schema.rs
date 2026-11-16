//! Configuration schema: the single source of truth for every option, its type and default.
//!
//! `config.example.toml` mirrors this file; a unit test fails if the two drift apart.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use serde::de::{self, IntoDeserializer};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

// ---------------------------------------------------------------------------
// Value types
// ---------------------------------------------------------------------------

/// A string that never appears in `Debug` output. Serializes as the plain string so the
/// config round-trips; use [`Config::redacted`] before emitting it anywhere visible.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    /// Wraps a secret value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The underlying secret. Handle with care.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// True when no secret is set.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Constant-time comparison against a candidate (e.g. the control-socket `hello` token).
    /// Length differences are not hidden; only the byte comparison is constant-time.
    pub fn ct_eq(&self, other: &str) -> bool {
        use subtle::ConstantTimeEq;
        self.0.as_bytes().ct_eq(other.as_bytes()).into()
    }
}

impl PartialEq for Secret {
    fn eq(&self, other: &Self) -> bool {
        self.ct_eq(&other.0)
    }
}

impl Eq for Secret {}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.0.is_empty() {
            "<empty>"
        } else {
            "<redacted>"
        })
    }
}

/// Account-level override of a global enum setting; the empty string means "inherit".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OrInherit<T> {
    /// Use the global value.
    #[default]
    Inherit,
    /// Use this value for the account.
    Value(T),
}

impl<T: Copy> OrInherit<T> {
    /// The override if set, otherwise `global`.
    pub fn resolve(self, global: T) -> T {
        match self {
            Self::Inherit => global,
            Self::Value(v) => v,
        }
    }
}

impl<T: Serialize> Serialize for OrInherit<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Inherit => serializer.serialize_str(""),
            Self::Value(v) => v.serialize(serializer),
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for OrInherit<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        if s.is_empty() {
            return Ok(Self::Inherit);
        }
        let de: de::value::StrDeserializer<'_, D::Error> = s.as_str().into_deserializer();
        T::deserialize(de).map(Self::Value)
    }
}

macro_rules! string_enum {
    ($(#[$meta:meta])* $name:ident, { $($(#[$vmeta:meta])* $variant:ident),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "lowercase")]
        pub enum $name {
            $($(#[$vmeta])* $variant),+
        }
    };
}

string_enum!(
    /// SIP signalling transport.
    Transport, {
        /// UDP.
        Udp,
        /// TCP.
        Tcp,
        /// TLS over TCP.
        Tls,
        /// Pick from DNS SRV records of the next hop: `_sips._tcp` (TLS), then `_sip._tcp` (TCP),
        /// then `_sip._udp` (UDP); without any SRV record, UDP on port 5060. No NAPTR. The
        /// transport that was picked is reported as `transport_effective` in the account status.
        Auto,
    }
);

impl Transport {
    /// Whether the transport is chosen at connect time (see [`Transport::Auto`]).
    pub fn is_auto(self) -> bool {
        self == Transport::Auto
    }
}

string_enum!(
    /// Where an account's password lives.
    PasswordStore, {
        /// In `config.toml` (`password`), mode 0600. The default.
        File,
        /// In the operating system's credential store (macOS Keychain, Windows Credential
        /// Manager, Linux Secret Service). `password` must then be empty in the file.
        Keyring,
    }
);

string_enum!(
    /// SRTP (SDES) policy for an account.
    SrtpPolicy, {
        /// Plain RTP only; SAVP-only offers are rejected with 488.
        Off,
        /// Offer RTP/AVP plus `a=crypto` (OSRTP); use SRTP whenever the peer supports it.
        Optional,
        /// Offer RTP/SAVP; offers without acceptable crypto are rejected with 488.
        Mandatory,
    }
);

string_enum!(
    /// DTMF transmission mode.
    DtmfMode, {
        /// RFC 4733 telephone-event packets.
        Rfc4733,
        /// SIP INFO (`application/dtmf-relay`).
        Info,
        /// In-band audio tones.
        Inband,
        /// RFC 4733 if negotiated, otherwise SIP INFO.
        Auto,
    }
);

string_enum!(
    /// Message-waiting-indication behaviour.
    MwiMode, {
        /// Subscribe to `message-summary`; unsolicited NOTIFYs are also accepted.
        Subscribe,
        /// Only accept unsolicited NOTIFYs.
        Unsolicited,
        /// Ignore MWI entirely.
        Off,
    }
);

/// How a ringing BLF extension is picked up (directed call pickup).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PickupMode {
    /// Pickup is not offered.
    #[default]
    Off,
    /// Dial `<pickup_prefix><extension>` (feature code) or `pickup_target`.
    Prefix,
    /// INVITE with `Replaces` for the ringing dialog (experimental; PBX support varies). Sent
    /// to the caller by default, see [`PickupReplacesTarget`].
    Replaces,
}

/// Who receives the INVITE of `pickup = "replaces"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PickupReplacesTarget {
    /// The caller (RFC 5359 §2.16): the dialog-info `<remote>` target. RFC 3891 §4 forbids
    /// replacing an early dialog on a UA that did not originate it, so the caller is the only
    /// party that may answer.
    #[default]
    Caller,
    /// The monitored phone's URI. For PBX / B2BUA setups where the callee leg is a dialog the
    /// PBX originated (its local tag is what dialog-info reports as `remote-tag`). Only the
    /// Request-URI differs: the tags are the same as for `caller`.
    Monitored,
}

string_enum!(
    /// SDP direction used when this side puts a call on hold.
    HoldStyle, {
        /// `a=sendonly` (`inactive` if the remote already holds us).
        Sendonly,
        /// `a=inactive`.
        Inactive,
    }
);

/// Which calls an auto-hold (answering, dialing, `call.activate`) puts on hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HoldScope {
    /// Every account's device-bound calls: at most one un-held device call per process.
    #[default]
    All,
    /// Only the calls of the account that answers / dials (two local accounts calling each
    /// other in one process keep both legs up).
    Account,
}

string_enum!(
    /// Which identity goes into the `Refer-To` URI of an attended transfer.
    AttendedReferTarget, {
        /// The consult party's address of record.
        Aor,
        /// The consult party's Contact.
        Contact,
    }
);

string_enum!(
    /// Dialog on which an attended-transfer REFER is sent.
    AttendedReferOn, {
        /// REFER on the original call, Replaces pointing at the consult call.
        Original,
        /// REFER on the consult call, Replaces pointing at the original call.
        Consult,
    }
);

string_enum!(
    /// How the answerer picks the codec from an offer.
    CodecAnswerOrder, {
        /// First codec in the offer that we support (desk-phone behaviour).
        Offer,
        /// First codec in our own list that is present in the offer.
        Local,
    }
);

string_enum!(
    /// Keep-alive flavour sent every `keepalive_secs`.
    KeepaliveMethod, {
        /// Double-CRLF ping (CRLFCRLF).
        Crlf,
        /// SIP OPTIONS request.
        Options,
    }
);

string_enum!(
    /// Reliable provisional responses (RFC 3262, 100rel/PRACK) when answering as the UAS.
    PrackMode, {
        /// Never: `Require: 100rel` gets `420 Bad Extension`; `100rel` is not advertised.
        Disabled,
        /// Reliable 18x only when the caller sends `Require: 100rel`; `100rel` is advertised.
        Supported,
        /// Reliable 18x whenever the caller lists `100rel` in `Supported` or `Require`.
        Required,
    }
);

string_enum!(
    /// Vendor preset: bundles defaults for a handful of account options so the softphone
    /// behaves like a given desk-phone family. Applied only to options the config file does
    /// not set explicitly (explicit config > profile > built-in default).
    Profile, {
        /// No preset: built-in defaults.
        Generic,
        /// Yealink SIP-Txx.
        Yealink,
        /// Polycom VVX.
        Polycom,
        /// Cisco MPP (78xx/88xx).
        Cisco,
        /// Grandstream GXP.
        Grandstream,
    }
);

string_enum!(
    /// `[ui] theme`.
    Theme, {
        /// Follow the operating system.
        System,
        /// Light palette.
        Light,
        /// Dark palette.
        Dark,
    }
);

string_enum!(
    /// `[ui] density`.
    Density, {
        /// Roomy rows.
        Comfortable,
        /// Tighter rows.
        Compact,
    }
);

string_enum!(
    /// `[ui] reduced_motion`.
    ReducedMotion, {
        /// Follow the operating system.
        System,
        /// Always reduce motion.
        On,
        /// Never reduce motion.
        Off,
    }
);

string_enum!(
    /// `[ui] blf_view`: how monitored extensions are laid out.
    BlfView, {
        /// Phone-style keys.
        Keys,
        /// Small lamps.
        Lamps,
        /// A plain list.
        List,
    }
);

string_enum!(
    /// `[ui] blf_click`: what clicking an idle BLF does.
    BlfClick, {
        /// Place a call at once.
        Dial,
        /// Put the number in the dial field.
        Prefill,
    }
);

string_enum!(
    /// `[ui] blf_click_in_call`: what clicking a BLF during a call does.
    BlfClickInCall, {
        /// Ask: consult or transfer now.
        Menu,
        /// Start a consult call.
        Consult,
        /// Blind transfer.
        Blind,
    }
);

string_enum!(
    /// `[ui] blf_click_ringing`: what clicking a ringing BLF does.
    BlfClickRinging, {
        /// Pick the call up (only when a pickup method is configured).
        Pickup,
        /// Call the extension.
        Call,
    }
);

string_enum!(
    /// `[ui] blf_group_by`.
    BlfGroupBy, {
        /// One group per account.
        Account,
        /// The BLF's own `group`.
        Group,
        /// No grouping.
        None,
    }
);

string_enum!(
    /// `[ui] transfer_enter`: what Enter does in the transfer composer.
    TransferEnter, {
        /// Start a consult call.
        Consult,
        /// Blind transfer.
        Blind,
    }
);

string_enum!(
    /// Audio codec.
    Codec, {
        /// Opus (dynamic payload type, 48 kHz).
        Opus,
        /// G.722 (PT 9).
        G722,
        /// G.711 mu-law (PT 0).
        Pcmu,
        /// G.711 A-law (PT 8).
        Pcma,
    }
);

/// SDES-SRTP crypto suite.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SrtpSuite {
    /// `AES_CM_128_HMAC_SHA1_80`
    #[serde(rename = "AES_CM_128_HMAC_SHA1_80")]
    Aes128Cm80,
    /// `AES_CM_128_HMAC_SHA1_32`
    #[serde(rename = "AES_CM_128_HMAC_SHA1_32")]
    Aes128Cm32,
    /// `AES_256_CM_HMAC_SHA1_80` (also spelled `AES_CM_256_HMAC_SHA1_80`).
    #[serde(rename = "AES_256_CM_HMAC_SHA1_80", alias = "AES_CM_256_HMAC_SHA1_80")]
    Aes256Cm80,
    /// `AES_256_CM_HMAC_SHA1_32` (also spelled `AES_CM_256_HMAC_SHA1_32`).
    #[serde(rename = "AES_256_CM_HMAC_SHA1_32", alias = "AES_CM_256_HMAC_SHA1_32")]
    Aes256Cm32,
}

impl SrtpSuite {
    /// Canonical SDP name of the suite (RFC 4568 / RFC 6188 spelling).
    pub fn sdp_name(self) -> &'static str {
        match self {
            Self::Aes128Cm80 => "AES_CM_128_HMAC_SHA1_80",
            Self::Aes128Cm32 => "AES_CM_128_HMAC_SHA1_32",
            Self::Aes256Cm80 => "AES_256_CM_HMAC_SHA1_80",
            Self::Aes256Cm32 => "AES_256_CM_HMAC_SHA1_32",
        }
    }

    /// Master key length in bytes.
    pub fn master_key_len(self) -> usize {
        match self {
            Self::Aes128Cm80 | Self::Aes128Cm32 => 16,
            Self::Aes256Cm80 | Self::Aes256Cm32 => 32,
        }
    }

    /// Master salt length in bytes (14 for every supported suite).
    pub fn master_salt_len(self) -> usize {
        14
    }

    /// SRTP authentication tag length in bytes (SRTCP always uses 10).
    pub fn auth_tag_len(self) -> usize {
        match self {
            Self::Aes128Cm80 | Self::Aes256Cm80 => 10,
            Self::Aes128Cm32 | Self::Aes256Cm32 => 4,
        }
    }
}

/// Error from parsing an `audio_source` / `audio_sink` string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct AudioSpecError(String);

/// Where a call's transmitted audio comes from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum AudioSource {
    /// The selected microphone.
    Device,
    /// Digital silence.
    Silence,
    /// A sine tone at the given frequency in Hz.
    Tone(f32),
    /// A WAV file, looped by the media engine.
    File(PathBuf),
}

impl FromStr for AudioSource {
    type Err = AudioSpecError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "device" => Ok(Self::Device),
            "silence" => Ok(Self::Silence),
            _ => {
                if let Some(raw) = s.strip_prefix("tone:") {
                    let hz: f32 = raw.trim().parse().map_err(|_| {
                        AudioSpecError(format!("invalid tone frequency {}", crate::echo(raw)))
                    })?;
                    if !(hz.is_finite() && hz > 0.0 && hz < 24_000.0) {
                        return Err(AudioSpecError(format!(
                            "tone frequency {hz} Hz out of range (0, 24000)"
                        )));
                    }
                    Ok(Self::Tone(hz))
                } else if let Some(path) = s.strip_prefix("file:") {
                    non_empty_path(path, "file").map(Self::File)
                } else {
                    Err(AudioSpecError(format!(
                        "invalid audio source {} (expected device | silence | tone:<hz> | file:<path>)",
                        crate::echo(s)
                    )))
                }
            }
        }
    }
}

impl fmt::Display for AudioSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Device => f.write_str("device"),
            Self::Silence => f.write_str("silence"),
            Self::Tone(hz) => write!(f, "tone:{hz}"),
            Self::File(p) => write!(f, "file:{}", p.display()),
        }
    }
}

impl TryFrom<String> for AudioSource {
    type Error = AudioSpecError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<AudioSource> for String {
    fn from(v: AudioSource) -> Self {
        v.to_string()
    }
}

/// Where a call's received audio goes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum AudioSink {
    /// The selected speaker.
    Device,
    /// Discarded.
    Null,
    /// Recorded to a WAV file.
    Record(PathBuf),
}

impl FromStr for AudioSink {
    type Err = AudioSpecError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "device" => Ok(Self::Device),
            "null" => Ok(Self::Null),
            _ => match s.strip_prefix("record:") {
                Some(path) => non_empty_path(path, "record").map(Self::Record),
                None => Err(AudioSpecError(format!(
                    "invalid audio sink {} (expected device | null | record:<path>)",
                    crate::echo(s)
                ))),
            },
        }
    }
}

impl fmt::Display for AudioSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Device => f.write_str("device"),
            Self::Null => f.write_str("null"),
            Self::Record(p) => write!(f, "record:{}", p.display()),
        }
    }
}

impl TryFrom<String> for AudioSink {
    type Error = AudioSpecError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<AudioSink> for String {
    fn from(v: AudioSink) -> Self {
        v.to_string()
    }
}

fn non_empty_path(path: &str, kind: &str) -> Result<PathBuf, AudioSpecError> {
    if path.trim().is_empty() {
        Err(AudioSpecError(format!("`{kind}:` requires a path")))
    } else {
        Ok(PathBuf::from(path))
    }
}

// ---------------------------------------------------------------------------
// Sections
// ---------------------------------------------------------------------------

/// Root of `config.toml`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// `[general]` process-wide settings.
    pub general: GeneralConfig,
    /// `[logging]` logging details.
    pub logging: LoggingConfig,
    /// `[control]` control-API endpoints.
    pub control: ControlConfig,
    /// `[network]` network-change detection and fast re-registration.
    pub network: NetworkConfig,
    /// `[media]` RTP and codec settings shared by all accounts.
    pub media: MediaConfig,
    /// `[audio]` default audio devices.
    pub audio: AudioConfig,
    /// `[calls]` multi-line behaviour shared by all accounts.
    pub calls: CallsConfig,
    /// `[ui]` desktop UI settings (hot; the GUI reads them as it learns to).
    pub ui: UiConfig,
    /// `[speech]` external TTS / STT commands.
    pub speech: SpeechConfig,
    /// `[[account]]` SIP accounts.
    #[serde(rename = "account")]
    pub accounts: Vec<AccountConfig>,
}

/// `[general]`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GeneralConfig {
    /// Default `User-Agent` header; accounts may override it.
    pub user_agent: String,
    /// A level (`trace|debug|info|warn|error`) or full `EnvFilter` syntax. A bare level is
    /// expanded to `<level>,rsipstack=warn`.
    pub log_level: String,
    /// Directory for log files. Empty = `<config dir>/logs`; relative paths are relative to
    /// the config dir.
    pub log_dir: String,
    /// Delete `siple.log.*` files older than this many days at startup (0 = keep forever).
    pub log_retention_days: u32,
    /// Rebuild an account's transport on config change even while it has active calls.
    pub reconfig_force: bool,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            user_agent: concat!("Siple/", env!("CARGO_PKG_VERSION")).into(),
            log_level: "info".into(),
            log_dir: String::new(),
            log_retention_days: 14,
            reconfig_force: false,
        }
    }
}

/// `[calls]`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CallsConfig {
    /// Which calls answering / dialing / `call.activate` put on hold. Per-account `auto_hold =
    /// false` exempts that account's calls from automatic holds only; an explicit
    /// `call.activate` ignores it.
    pub hold_scope: HoldScope,
    /// A transfer that failed for good (a definitive refusal or a local error) takes the
    /// original call off hold again when we were the one holding it and nothing else needs it.
    pub resume_after_failed_transfer: bool,
    /// SIP status code sent when a call-waiting call is declined (400-699).
    pub decline_code: u16,
}

impl Default for CallsConfig {
    fn default() -> Self {
        Self {
            hold_scope: HoldScope::default(),
            resume_after_failed_transfer: true,
            decline_code: 486,
        }
    }
}

/// `[ui]`: desktop UI settings. Hot; no account side effects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct UiConfig {
    /// Colour theme.
    pub theme: Theme,
    /// Row density.
    pub density: Density,
    /// Reduce animation.
    pub reduced_motion: ReducedMotion,
    /// BLF layout.
    pub blf_view: BlfView,
    /// Click on an idle BLF.
    pub blf_click: BlfClick,
    /// Click on a BLF during a call.
    pub blf_click_in_call: BlfClickInCall,
    /// Click on a ringing BLF.
    pub blf_click_ringing: BlfClickRinging,
    /// BLF grouping.
    pub blf_group_by: BlfGroupBy,
    /// Enter in the transfer composer.
    pub transfer_enter: TransferEnter,
    /// Tray / menu-bar mode.
    pub tray: bool,
    /// Closing the window hides it to the tray.
    pub close_to_tray: bool,
    /// No taskbar button / Dock icon while hidden.
    pub hide_from_taskbar: bool,
    /// Start with the window hidden.
    pub start_hidden: bool,
    /// Raise the window on an incoming call.
    pub show_on_incoming: bool,
    /// Raise the window when a call is answered.
    pub show_on_answer: bool,
    /// Desktop notifications.
    pub notifications: bool,
    /// Register as handler for `sip:` / `tel:` links.
    pub handle_links: bool,
    /// Start Siple when the user logs in.
    pub launch_at_login: bool,
    /// Show developer tools in the GUI (does not affect the control API).
    pub developer_tools: bool,
    /// Keyboard shortcuts: action name to key chord, e.g. `answer = "Mod+Shift+A"`.
    pub shortcuts: BTreeMap<String, String>,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: Theme::System,
            density: Density::Comfortable,
            reduced_motion: ReducedMotion::System,
            blf_view: BlfView::Keys,
            blf_click: BlfClick::Dial,
            blf_click_in_call: BlfClickInCall::Menu,
            blf_click_ringing: BlfClickRinging::Pickup,
            blf_group_by: BlfGroupBy::Account,
            transfer_enter: TransferEnter::Consult,
            tray: true,
            close_to_tray: true,
            hide_from_taskbar: true,
            start_hidden: false,
            show_on_incoming: true,
            show_on_answer: false,
            notifications: true,
            handle_links: true,
            launch_at_login: false,
            developer_tools: false,
            shortcuts: BTreeMap::new(),
        }
    }
}

/// `[logging]`
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LoggingConfig {
    /// Mask SRTP master keys (`a=crypto` lines) in logs and SIP traces.
    pub redact_srtp_keys: bool,
    /// Mask digest `Authorization` / `Proxy-Authorization` secrets (`response`, `nonce`,
    /// `cnonce`), the control token and configured account passwords in logs and SIP traces.
    pub redact_credentials: bool,
}

/// `[network]`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct NetworkConfig {
    /// Watch OS network events and sleep/wake and re-register accounts when the network changes.
    pub watch: bool,
    /// Re-probe the route and DNS of every account every N seconds (0 = events only).
    pub probe_secs: u32,
    /// Coalesce network-change triggers for this many milliseconds before acting.
    pub debounce_ms: u32,
    /// After a network change, move established calls onto the new path with a re-INVITE. `false`
    /// ends them with cause `network_lost` instead.
    pub move_calls: bool,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            watch: true,
            probe_secs: 30,
            debounce_ms: 1500,
            move_calls: true,
        }
    }
}

/// `[control]`
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ControlConfig {
    /// Unix domain socket path (macOS/Linux). Empty = `<config dir>/control.sock`
    /// (falls back to `$XDG_RUNTIME_DIR` or `/tmp/siple-<euid>` on Linux, `$TMPDIR` on macOS,
    /// if the path is too long for `sun_path`).
    pub unix_socket: String,
    /// Windows named pipe. Empty = `\\.\pipe\siple-<user>`.
    pub named_pipe: String,
    /// Loopback TCP listener such as `127.0.0.1:5079`. Empty = off. The token is always
    /// required on TCP.
    pub tcp_listen: String,
}

/// `[media]`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MediaConfig {
    /// Inclusive `[start, end]` range for RTP/RTCP port pairs; start must be even.
    pub rtp_port_range: [u16; 2],
    /// Codec preference order, used by accounts that do not set their own list.
    pub codecs: Vec<Codec>,
    /// Lower bound of the adaptive jitter buffer, in milliseconds.
    pub jitter_min_ms: u32,
    /// Upper bound of the adaptive jitter buffer, in milliseconds.
    pub jitter_max_ms: u32,
    /// Accept comfort noise (CN) when offered by the peer.
    pub comfort_noise: bool,
    /// Latch the RTP destination to the source of authenticated incoming packets.
    pub symmetric_rtp: bool,
    /// Offer and use `a=rtcp-mux`.
    pub rtcp_mux: bool,
    /// Seconds of received audio (16 kHz) kept per call for `call.listen` / `call.rx_audio`.
    pub rx_history_secs: u32,
    /// How the codec is chosen when answering an offer.
    pub codec_answer_order: CodecAnswerOrder,
    /// Default DTMF transmission mode.
    pub dtmf_mode: DtmfMode,
    /// Default duration of a transmitted digit, in milliseconds.
    pub dtmf_duration_ms: u32,
    /// Silence between consecutive transmitted digits, in milliseconds.
    pub dtmf_gap_ms: u32,
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            rtp_port_range: [16384, 32767],
            codecs: vec![Codec::Opus, Codec::G722, Codec::Pcmu, Codec::Pcma],
            jitter_min_ms: 40,
            jitter_max_ms: 200,
            comfort_noise: false,
            symmetric_rtp: false,
            rtcp_mux: false,
            rx_history_secs: 60,
            codec_answer_order: CodecAnswerOrder::Offer,
            dtmf_mode: DtmfMode::Auto,
            dtmf_duration_ms: 100,
            dtmf_gap_ms: 70,
        }
    }
}

/// Audio device backend selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioBackend {
    /// Use sound devices; on Linux, probe once at startup and fall back to no audio when no
    /// hardware or sound server is found.
    #[default]
    Auto,
    /// Never touch sound devices (headless servers, containers).
    None,
    /// Always start the device thread, skipping the Linux startup probe (PulseAudio via
    /// `client.conf`, a USB headset plugged in after boot).
    Always,
}

/// `[audio]`
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AudioConfig {
    /// `auto`, `none` or `always`. Changing it needs a restart.
    pub backend: AudioBackend,
    /// Microphone device name; empty = system default.
    pub input: String,
    /// Speaker device name; empty = system default.
    pub output: String,
    /// Ringer device name; empty = same as `output`.
    pub ringer: String,
}

/// `[speech]`: external text-to-speech / speech-to-text commands (off while the argv is empty).
/// Placeholders (`{name}`, `{{`/`}}` for literal braces) are expanded inside argv elements only;
/// no shell is involved. See `docs/CONFIG.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SpeechConfig {
    /// TTS argv; placeholders `{text_file}`, `{out}`, `{voice}`. Empty = TTS off.
    pub tts_command: Vec<String>,
    /// Kill the TTS process after this many milliseconds (1000..=120000).
    pub tts_timeout_ms: u32,
    /// Default voice substituted for `{voice}`; empty = none.
    pub tts_voice: String,
    /// STT argv; placeholders `{in}`, `{lang}`, `{out_base}`, `{out_txt}`. Empty = STT off.
    pub stt_command: Vec<String>,
    /// Kill the STT process after this many milliseconds (1000..=120000).
    pub stt_timeout_ms: u32,
    /// Default language substituted for `{lang}`.
    pub stt_language: String,
}

impl Default for SpeechConfig {
    fn default() -> Self {
        Self {
            tts_command: Vec::new(),
            tts_timeout_ms: 30_000,
            tts_voice: String::new(),
            stt_command: Vec::new(),
            stt_timeout_ms: 60_000,
            stt_language: "en".into(),
        }
    }
}

/// Placeholders allowed in `speech.tts_command`.
pub const TTS_PLACEHOLDERS: &[&str] = &["text_file", "out", "voice"];
/// Placeholders allowed in `speech.stt_command`.
pub const STT_PLACEHOLDERS: &[&str] = &["in", "lang", "out_base", "out_txt"];

/// Splits an argv template into the placeholder names it references (`{name}`; `{{` and `}}`
/// are literal braces). Errors on unbalanced or empty braces and on names that are not
/// `[a-z_]+`; checking against an allowed set is the caller's job.
pub fn template_placeholders(arg: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut chars = arg.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
            }
            '{' => {
                let mut name = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some(ch) => name.push(ch),
                        None => return Err("unclosed `{` (use `{{` for a literal brace)".into()),
                    }
                }
                if name.is_empty() || !name.chars().all(|ch| ch.is_ascii_lowercase() || ch == '_') {
                    return Err(format!("invalid placeholder `{{{name}}}`"));
                }
                out.push(name);
            }
            '}' => return Err("unmatched `}` (use `}}` for a literal brace)".into()),
            _ => {}
        }
    }
    Ok(out)
}

/// Whether `s` is acceptable as a TTS voice / STT language token: `^[A-Za-z0-9][A-Za-z0-9_.+:-]{0,63}$`
/// (no leading `-`, so it can never be read as an option).
pub fn is_speech_token(s: &str) -> bool {
    let mut it = s.chars();
    matches!(it.next(), Some(c) if c.is_ascii_alphanumeric())
        && s.len() <= 64
        && it.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '+' | ':' | '-'))
}

/// `[[account]]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AccountConfig {
    /// Unique account id; used in file names and API calls. Allowed: `[A-Za-z0-9_.-]`.
    pub id: String,
    /// Disabled accounts are loaded but never started.
    pub enabled: bool,
    /// Vendor preset (`generic|yealink|polycom|cisco|grandstream`). Fills in the options the
    /// config file leaves out (`user_agent`, `hold_style`, `dtmf_mode`, `dnd_reject_code`,
    /// `attended_refer_target`, `prack`, `session_timers`); explicit options always win.
    pub profile: Profile,
    /// Display name used in the From header.
    pub display_name: String,
    /// SIP user part of the address of record.
    pub username: String,
    /// Digest auth username; empty = `username`.
    pub auth_username: String,
    /// Digest auth password. With `password_store = "keyring"` it is empty in the file and filled
    /// in memory from the credential store by the loader.
    pub password: Secret,
    /// Where the password lives: `file` (the `password` key) or `keyring` (the OS credential
    /// store: service `io.zyno.siple`, entry `account:<id>`).
    pub password_store: PasswordStore,
    /// SIP domain of the address of record.
    pub domain: String,
    /// Registrar / request-URI host and next hop; empty = `domain`.
    pub registrar: String,
    /// Forced next hop for all requests (`host[:port]`); empty = none.
    pub outbound_proxy: String,
    /// Additionally add `Route: <proxy;lr>` to out-of-dialog requests and INVITE route sets.
    pub outbound_proxy_route: bool,
    /// Signalling transport.
    pub transport: Transport,
    /// Local SIP port; 0 = ephemeral.
    pub local_port: u16,
    /// Local IP to bind and advertise; empty = auto (interface toward the next hop).
    pub bind_address: String,
    /// IP or host to advertise in Via/Contact/SDP without binding to it (NAT); empty = none.
    pub public_address: String,
    /// Learn Via `received`/`rport` from REGISTER responses and re-register with that Contact.
    /// An explicit `public_address` takes precedence.
    pub rewrite_contact: bool,
    /// Keep-alive interval in seconds (0 = off).
    pub keepalive_secs: u32,
    /// What to send as keep-alive.
    pub keepalive_method: KeepaliveMethod,
    /// With `keepalive_method = "options"`: consecutive unanswered keep-alives after which the
    /// flow is considered dead and the account re-registers (0 = never).
    pub keepalive_fail_threshold: u32,
    /// Use a `sips:` Contact on TLS instead of `sip:...;transport=tls`.
    pub use_sips_uri: bool,
    /// Verify the server certificate (OS roots + `tls_ca_file`); false ignores errors.
    pub tls_verify: bool,
    /// PEM file with extra trusted CA certificates.
    pub tls_ca_file: String,
    /// TLS server name override for SNI and verification.
    pub tls_server_name: String,
    /// Pin the server's leaf certificate: SHA-256 of its DER as hex (colons optional). When set,
    /// exactly that certificate is accepted regardless of chain, expiry or name (handshake
    /// signatures are still checked); `tls_verify` / `tls_ca_file` are not consulted.
    pub tls_fingerprint: String,
    /// PEM certificate for the inbound TLS listener and client auth; empty = self-signed.
    pub tls_cert: String,
    /// PEM private key matching `tls_cert`.
    pub tls_key: String,
    /// Send REGISTER. With false the account still places and receives calls (peer mode).
    pub register: bool,
    /// Requested registration lifetime in seconds.
    pub expires: u32,
    /// SRTP policy.
    pub srtp: SrtpPolicy,
    /// Offered/accepted SRTP suites in preference order.
    pub srtp_suites: Vec<SrtpSuite>,
    /// Codec preference override; empty = inherit `[media].codecs`.
    pub codecs: Vec<Codec>,
    /// DTMF mode override; empty string = inherit `[media].dtmf_mode`.
    pub dtmf_mode: OrInherit<DtmfMode>,
    /// `User-Agent` override; empty = inherit `[general].user_agent`.
    pub user_agent: String,
    /// Do not disturb: reject incoming calls.
    pub dnd: bool,
    /// SIP status code used when DND rejects a call (400-699).
    pub dnd_reject_code: u16,
    /// Answer incoming calls automatically.
    pub auto_answer: bool,
    /// Delay before auto-answering, in milliseconds.
    pub answer_delay_ms: u32,
    /// Auto-answer on `Call-Info: answer-after` / `Alert-Info` intercom requests.
    pub allow_intercom: bool,
    /// Answering or dialing a device-bound call puts the other live device-bound calls on hold
    /// (`[calls] hold_scope`). `false` exempts this account's calls from automatic holds and
    /// stops it triggering them; an explicit `call.activate` or `hold_others` ignores it.
    pub auto_hold: bool,
    /// SDP direction used when holding.
    pub hold_style: HoldStyle,
    /// Send periodic comfort noise while holding (keeps PBX media timeouts quiet).
    pub hold_send_cn: bool,
    /// Follow 3xx redirects on outgoing calls (at most 3 hops).
    pub follow_redirects: bool,
    /// Answer digest challenges from servers that are not this account's provider. `false`
    /// (default): a 401/407 is only answered when the address the request was actually sent to
    /// is the account's `outbound_proxy` / registrar / `domain` or an address the outbound
    /// proxy / registrar resolved to (request-URI, Contact and Record-Route are never
    /// trusted), so a call to an arbitrary host never yields a digest response for the SIP
    /// password. `true` restores answering every challenge. Hot-reloadable.
    pub auth_foreign: bool,
    /// Maximum concurrent calls; further INVITEs get 486.
    pub max_calls: u32,
    /// Reliable provisional responses (100rel/PRACK, RFC 3262) as the UAS.
    pub prack: PrackMode,
    /// Session timers (RFC 4028): advertise `Supported: timer`, honour `Session-Expires` /
    /// `Min-SE`, refresh the session when we are the refresher, hang up when the peer does
    /// not refresh.
    pub session_timers: bool,
    /// `Session-Expires` we request on outgoing calls, in seconds.
    pub session_expires: u32,
    /// `Min-SE`: shortest session interval we accept, in seconds (RFC 4028 says at least 90;
    /// lower values are allowed for testing).
    pub min_se: u32,
    /// Accept incoming REFER (act as transferee).
    pub accept_refer: bool,
    /// Identity used in the `Refer-To` URI of attended transfers.
    pub attended_refer_target: AttendedReferTarget,
    /// Which dialog carries the attended-transfer REFER.
    pub attended_refer_on: AttendedReferOn,
    /// Message-waiting handling.
    pub mwi: MwiMode,
    /// URI to subscribe `message-summary` on; empty = own AOR.
    pub mwi_uri: String,
    /// Requested expiry of BLF subscriptions, in seconds.
    pub blf_expires: u32,
    /// Directed pickup of ringing BLF extensions: `off`, `prefix` (dial a feature code) or
    /// `replaces` (experimental). Per-entry override: `[[account.blf]] pickup`.
    pub pickup: PickupMode,
    /// Feature-code prefix of `pickup = "prefix"`, dialled before the extension.
    pub pickup_prefix: String,
    /// With `pickup = "replaces"` (experimental): `caller` (default, RFC 5359) or `monitored`.
    pub pickup_replaces_target: PickupReplacesTarget,
    /// Write a wire-level SIP trace for this account.
    pub sip_trace: bool,
    /// Default audio source for calls: `device|silence|tone:<hz>|file:<path>`.
    pub audio_source: AudioSource,
    /// Default audio sink for calls: `device|null|record:<path>`.
    pub audio_sink: AudioSink,
    /// Extra headers added to outgoing requests (`name = "value"`).
    pub extra_headers: BTreeMap<String, String>,
    /// `[[account.blf]]` busy-lamp-field subscriptions.
    pub blf: Vec<BlfConfig>,
}

impl Default for AccountConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            enabled: true,
            profile: Profile::Generic,
            display_name: String::new(),
            username: String::new(),
            auth_username: String::new(),
            password: Secret::default(),
            password_store: PasswordStore::File,
            domain: String::new(),
            registrar: String::new(),
            outbound_proxy: String::new(),
            outbound_proxy_route: false,
            transport: Transport::Udp,
            local_port: 0,
            bind_address: String::new(),
            public_address: String::new(),
            rewrite_contact: true,
            keepalive_secs: 30,
            keepalive_method: KeepaliveMethod::Crlf,
            keepalive_fail_threshold: 2,
            use_sips_uri: false,
            tls_verify: true,
            tls_ca_file: String::new(),
            tls_server_name: String::new(),
            tls_fingerprint: String::new(),
            tls_cert: String::new(),
            tls_key: String::new(),
            register: true,
            expires: 300,
            srtp: SrtpPolicy::Optional,
            srtp_suites: vec![SrtpSuite::Aes128Cm80, SrtpSuite::Aes128Cm32],
            codecs: Vec::new(),
            dtmf_mode: OrInherit::Inherit,
            user_agent: String::new(),
            dnd: false,
            dnd_reject_code: 486,
            auto_answer: false,
            answer_delay_ms: 0,
            allow_intercom: false,
            auto_hold: true,
            hold_style: HoldStyle::Sendonly,
            hold_send_cn: false,
            follow_redirects: true,
            auth_foreign: false,
            max_calls: 4,
            prack: PrackMode::Supported,
            session_timers: true,
            session_expires: 1800,
            min_se: 90,
            accept_refer: true,
            attended_refer_target: AttendedReferTarget::Aor,
            attended_refer_on: AttendedReferOn::Original,
            mwi: MwiMode::Subscribe,
            mwi_uri: String::new(),
            blf_expires: 600,
            pickup: PickupMode::Off,
            pickup_prefix: "**".into(),
            pickup_replaces_target: PickupReplacesTarget::Caller,
            sip_trace: true,
            audio_source: AudioSource::Device,
            audio_sink: AudioSink::Device,
            extra_headers: BTreeMap::new(),
            blf: Vec::new(),
        }
    }
}

/// `[[account.blf]]`
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct BlfConfig {
    /// Monitored extension or full SIP URI.
    pub uri: String,
    /// Human-readable label.
    pub label: String,
    /// Group the entry belongs to in the GUI; empty = grouped by account.
    pub group: String,
    /// Pickup method for this entry; empty = inherit the account's `pickup`.
    pub pickup: OrInherit<PickupMode>,
    /// Pickup prefix override; empty = the account's `pickup_prefix`.
    pub pickup_prefix: String,
    /// Dial this instead of `<pickup_prefix><extension>` (queues, park slots); empty = unset.
    pub pickup_target: String,
}

/// Effective directed-pickup settings of one BLF entry ([`BlfConfig::pickup_for`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickupSetting {
    /// The method.
    pub mode: PickupMode,
    /// Feature-code prefix (entry override, else the account's).
    pub prefix: String,
    /// Explicit dial target; empty = `<prefix><extension>`.
    pub target: String,
    /// Who receives the INVITE of `Replaces` pickups (account setting).
    pub replaces_target: PickupReplacesTarget,
}

impl BlfConfig {
    /// Pickup settings: the entry's override, else the account's.
    pub fn pickup_for(&self, account: &AccountConfig) -> PickupSetting {
        PickupSetting {
            mode: self.pickup.resolve(account.pickup),
            prefix: if self.pickup_prefix.is_empty() {
                account.pickup_prefix.clone()
            } else {
                self.pickup_prefix.clone()
            },
            target: self.pickup_target.clone(),
            replaces_target: account.pickup_replaces_target,
        }
    }

    /// The extension of the monitored URI: the user part of a SIP URI (`sip:1004@pbx`,
    /// `1004@pbx`), otherwise the configured text as is.
    pub fn extension(&self) -> String {
        let u = self.uri.trim();
        let lower = u.to_ascii_lowercase();
        let rest = lower
            .strip_prefix("sip:")
            .or_else(|| lower.strip_prefix("sips:"))
            .map(|r| &u[u.len() - r.len()..]);
        let is_uri = rest.is_some() || u.contains('@');
        let body = rest.unwrap_or(u);
        match body.split_once('@') {
            Some((user, _)) if is_uri => user.split(';').next().unwrap_or(user).to_string(),
            _ => u.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Effective values
// ---------------------------------------------------------------------------

impl AccountConfig {
    /// Codec preference list: the account's own, or the global one when empty.
    pub fn effective_codecs<'a>(&'a self, media: &'a MediaConfig) -> &'a [Codec] {
        if self.codecs.is_empty() {
            &media.codecs
        } else {
            &self.codecs
        }
    }

    /// DTMF mode: the account override, or the global default.
    pub fn effective_dtmf_mode(&self, media: &MediaConfig) -> DtmfMode {
        self.dtmf_mode.resolve(media.dtmf_mode)
    }

    /// Request-URI host / next hop for REGISTER: `registrar`, or `domain` when empty.
    pub fn effective_registrar(&self) -> &str {
        if self.registrar.is_empty() {
            &self.domain
        } else {
            &self.registrar
        }
    }

    /// Digest username: `auth_username`, or `username` when empty.
    pub fn effective_auth_username(&self) -> &str {
        if self.auth_username.is_empty() {
            &self.username
        } else {
            &self.auth_username
        }
    }

    /// `User-Agent`: the account override, or the global one.
    pub fn effective_user_agent<'a>(&'a self, general: &'a GeneralConfig) -> &'a str {
        if self.user_agent.is_empty() {
            &general.user_agent
        } else {
            &self.user_agent
        }
    }

    /// `Supported` header value this account advertises: `replaces`, plus `100rel` unless
    /// `prack = "disabled"`, plus `timer` when `session_timers` is on.
    pub fn supported_tokens(&self) -> String {
        let mut v = String::from("replaces");
        if self.prack != PrackMode::Disabled {
            v.push_str(", 100rel");
        }
        if self.session_timers {
            v.push_str(", timer");
        }
        v
    }

    /// Address of record, `sip:username@domain`.
    pub fn aor(&self) -> String {
        format!("sip:{}@{}", self.username, self.domain)
    }
}

impl Config {
    /// A copy with every non-empty account password replaced by `***`.
    pub fn redacted(&self) -> Config {
        let mut c = self.clone();
        for a in &mut c.accounts {
            if !a.password.is_empty() {
                a.password = Secret::new("***");
            }
        }
        c
    }
}

//! Busy lamp field: `application/dialog-info+xml` (RFC 4235) parser, the per-entity
//! dialog table (full / partial updates, stale-version filter) and the derived lamp state.

use chrono::{DateTime, Utc};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use serde::Serialize;

/// Derived state of one monitored extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BlfState {
    /// No non-terminated dialogs.
    Idle,
    /// An incoming call is ringing the monitored phone.
    Ringing,
    /// The monitored phone is placing a call that is not answered yet.
    Dialing,
    /// A confirmed dialog (call in progress).
    Busy,
    /// Every confirmed dialog is held by the monitored phone (`+sip.rendering="no"`).
    Held,
    /// No NOTIFY received yet (or the subscription is not established).
    Unknown,
    /// The subscription failed (rejected / not found / gave up).
    Failed,
}

impl BlfState {
    /// Lower-case name (`idle`, `ringing`, ...).
    pub fn name(self) -> &'static str {
        match self {
            BlfState::Idle => "idle",
            BlfState::Ringing => "ringing",
            BlfState::Dialing => "dialing",
            BlfState::Busy => "busy",
            BlfState::Held => "held",
            BlfState::Unknown => "unknown",
            BlfState::Failed => "failed",
        }
    }
}

/// RFC 4235 dialog state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DialogPhase {
    /// `trying`.
    Trying,
    /// `proceeding`.
    Proceeding,
    /// `early`.
    Early,
    /// `confirmed`.
    Confirmed,
    /// `terminated`.
    Terminated,
}

impl DialogPhase {
    fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "trying" => Self::Trying,
            "proceeding" => Self::Proceeding,
            "early" => Self::Early,
            "confirmed" => Self::Confirmed,
            "terminated" => Self::Terminated,
            _ => return None,
        })
    }
}

/// Who started the dialog (from the monitored entity's point of view).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DialogDirection {
    /// The monitored phone called out.
    Initiator,
    /// The monitored phone was called.
    Recipient,
}

/// One dialog of the monitored entity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BlfDialog {
    /// `id` attribute (key for partial updates). Falls back to the Call-ID.
    pub id: String,
    /// SIP Call-ID, if the notifier sent one.
    pub call_id: Option<String>,
    /// Tag of the monitored entity.
    pub local_tag: Option<String>,
    /// Tag of the other side.
    pub remote_tag: Option<String>,
    /// `direction` attribute.
    pub direction: Option<DialogDirection>,
    /// Dialog state.
    pub state: DialogPhase,
    /// Other party's identity URI.
    pub remote_identity: Option<String>,
    /// Other party's display name.
    pub remote_display: Option<String>,
    /// The monitored entity's identity in this dialog.
    pub local_identity: Option<String>,
    /// The monitored entity's target (Contact) URI.
    pub local_target: Option<String>,
    /// The other party's target (Contact) URI (`<remote><target uri>`).
    pub remote_target: Option<String>,
    /// `+sip.rendering` of the local target: `Some(false)` = not rendering media (held).
    pub rendering: Option<bool>,
}

/// A parsed `dialog-info` document.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DialogInfo {
    /// `version` attribute.
    pub version: Option<u64>,
    /// `state="full"` (replace) vs `"partial"` (merge). Missing counts as full.
    pub full: bool,
    /// `entity` attribute.
    pub entity: Option<String>,
    /// The `<dialog>` elements.
    pub dialogs: Vec<BlfDialog>,
}

fn attr(e: &BytesStart<'_>, name: &str) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        (a.key.local_name().into_inner() == name)
            .then(|| {
                a.normalized_value(quick_xml::XmlVersion::Implicit1_0)
                    .ok()
                    .map(|v| v.into_owned())
            })
            .flatten()
    })
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// Characters that must not reach a header or the screen: controls (CR LF ...), line / paragraph
/// separators and the Unicode "format" characters (category Cf: bidi overrides, zero-width
/// characters, tag characters).
fn is_unsafe_char(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{ad}' | '\u{600}'..='\u{605}' | '\u{61c}' | '\u{6dd}' | '\u{70f}' | '\u{890}'..='\u{891}'
            | '\u{8e2}' | '\u{180e}' | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206f}' | '\u{feff}'
            | '\u{fff9}'..='\u{fffb}' | '\u{110bd}' | '\u{110cd}' | '\u{13430}'..='\u{1343f}'
            | '\u{1bca0}'..='\u{1bca3}' | '\u{1d173}'..='\u{1d17a}' | '\u{e0001}'
            | '\u{e0020}'..='\u{e007f}')
}

/// Text that is shown or kept: NOTIFY content is untrusted (the XML parser decodes `&#13;&#10;`),
/// so control and format characters are dropped.
fn plain(s: Option<String>) -> Option<String> {
    non_empty(s.map(|v| v.chars().filter(|c| !is_unsafe_char(*c)).collect()))
}

/// Text that may end up in a SIP header (call-id, tags): kept only when it has no control
/// characters at all, never repaired (a repaired value could still be a forged one).
fn strict(s: Option<String>) -> Option<String> {
    non_empty(s).filter(|v| !v.chars().any(is_unsafe_char))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Local,
    Remote,
}

/// Parses a dialog-info body. Prefix- and namespace-agnostic (`<d:dialog-info xmlns:d=...>`
/// works), unknown elements are skipped. Returns `None` when the body is not XML with a
/// `dialog-info` root.
pub fn parse_dialog_info(xml: &str) -> Option<DialogInfo> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().check_end_names = false;
    let mut info = DialogInfo::default();
    let mut root_seen = false;
    let mut stack: Vec<String> = Vec::new();
    let mut cur: Option<BlfDialog> = None;
    let mut side: Option<Side> = None;
    let mut text = String::new();
    // Pending identity display for the current side element.
    let mut ident_display: Option<String> = None;

    loop {
        let ev = reader.read_event();
        let (start, empty, end) = match &ev {
            Ok(Event::Start(e)) => (Some(e.clone()), false, false),
            Ok(Event::Empty(e)) => (Some(e.clone()), true, false),
            Ok(Event::End(_)) => (None, false, true),
            _ => (None, false, false),
        };
        match ev {
            Ok(Event::Eof) => break,
            Err(_) => {
                // Truncated / malformed: keep what was parsed only if the root was seen.
                break;
            }
            Ok(Event::Text(t)) => {
                text.push_str(&t.xml10_content());
            }
            Ok(Event::CData(c)) => {
                text.push_str(&c.xml10_content());
            }
            Ok(Event::GeneralRef(r)) => {
                if let Ok(Some(c)) = r.resolve_char_ref() {
                    text.push(c);
                } else {
                    let n: &str = &r;
                    match n {
                        "amp" => text.push('&'),
                        "lt" => text.push('<'),
                        "gt" => text.push('>'),
                        "quot" => text.push('"'),
                        "apos" => text.push('\''),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        if let Some(e) = start {
            let name = e.local_name().into_inner().to_ascii_lowercase();
            text.clear();
            match name.as_str() {
                "dialog-info" if !root_seen => {
                    root_seen = true;
                    info.version = attr(&e, "version").and_then(|v| v.trim().parse().ok());
                    info.full =
                        attr(&e, "state").is_none_or(|s| !s.trim().eq_ignore_ascii_case("partial"));
                    info.entity = non_empty(attr(&e, "entity"));
                }
                "dialog" if root_seen => {
                    let call_id = strict(attr(&e, "call-id"));
                    cur = Some(BlfDialog {
                        id: plain(attr(&e, "id"))
                            .or_else(|| call_id.clone())
                            .unwrap_or_default(),
                        call_id,
                        local_tag: strict(attr(&e, "local-tag")),
                        remote_tag: strict(attr(&e, "remote-tag")),
                        direction: attr(&e, "direction").and_then(|d| {
                            match d.trim().to_ascii_lowercase().as_str() {
                                "initiator" => Some(DialogDirection::Initiator),
                                "recipient" => Some(DialogDirection::Recipient),
                                _ => None,
                            }
                        }),
                        state: DialogPhase::Trying,
                        remote_identity: None,
                        remote_display: None,
                        local_identity: None,
                        local_target: None,
                        remote_target: None,
                        rendering: None,
                    });
                    if empty {
                        finish_dialog(&mut info, &mut cur);
                    }
                }
                "local" if cur.is_some() => side = Some(Side::Local),
                "remote" if cur.is_some() => side = Some(Side::Remote),
                "identity" if cur.is_some() => ident_display = plain(attr(&e, "display")),
                "target" if cur.is_some() => {
                    if let (Some(d), Some(sd), Some(u)) =
                        (cur.as_mut(), side, strict(attr(&e, "uri")))
                    {
                        match sd {
                            Side::Local => d.local_target = Some(u),
                            Side::Remote => d.remote_target = Some(u),
                        }
                    }
                }
                "param" if cur.is_some() && side == Some(Side::Local) => {
                    if attr(&e, "pname").is_some_and(|n| n.trim() == "+sip.rendering")
                        && let (Some(d), Some(v)) = (cur.as_mut(), attr(&e, "pvalue"))
                    {
                        d.rendering = match v.trim().to_ascii_lowercase().as_str() {
                            "yes" | "true" => Some(true),
                            "no" | "false" => Some(false),
                            _ => d.rendering,
                        };
                    }
                }
                _ => {}
            }
            if !empty {
                stack.push(name);
            } else if cur.is_some() && stack.last().is_some_and(|p| p == "dialog") {
                // empty elements directly under <dialog> carry nothing we need
            }
        } else if end {
            let name = stack.pop().unwrap_or_default();
            match name.as_str() {
                "state" if stack.last().is_some_and(|p| p == "dialog") => {
                    if let (Some(d), Some(p)) = (cur.as_mut(), DialogPhase::parse(&text)) {
                        d.state = p;
                    }
                }
                "identity" => {
                    if let Some(d) = cur.as_mut() {
                        let uri = plain(Some(std::mem::take(&mut text)));
                        match side {
                            Some(Side::Remote) => {
                                d.remote_identity = uri;
                                d.remote_display = ident_display.take();
                            }
                            Some(Side::Local) => {
                                d.local_identity = uri;
                                ident_display = None;
                            }
                            None => {}
                        }
                    }
                }
                "local" | "remote" => side = None,
                "dialog" => finish_dialog(&mut info, &mut cur),
                _ => {}
            }
            text.clear();
        }
    }
    finish_dialog(&mut info, &mut cur);
    root_seen.then_some(info)
}

fn finish_dialog(info: &mut DialogInfo, cur: &mut Option<BlfDialog>) {
    if let Some(d) = cur.take() {
        info.dialogs.push(d);
    }
}

/// Dialog table of one monitored entity.
#[derive(Debug, Clone, Default)]
pub struct BlfTracker {
    last_version: Option<u64>,
    dialogs: Vec<BlfDialog>,
    entity: Option<String>,
    got_notify: bool,
}

impl BlfTracker {
    /// Applies an update. Returns whether the visible content (dialog list) changed. Updates
    /// with `version` lower than the last one seen are ignored.
    pub fn apply(&mut self, doc: DialogInfo) -> bool {
        if let (Some(last), Some(v)) = (self.last_version, doc.version)
            && v < last
        {
            return false;
        }
        let first = !self.got_notify;
        self.got_notify = true;
        if doc.version.is_some() {
            self.last_version = doc.version;
        }
        if doc.entity.is_some() {
            self.entity = doc.entity.clone();
        }
        let before = self.dialogs.clone();
        let listed: Vec<String> = doc.dialogs.iter().map(|d| d.id.clone()).collect();
        if doc.full {
            self.dialogs = doc.dialogs;
        } else {
            for d in doc.dialogs {
                match self.dialogs.iter_mut().find(|x| x.id == d.id) {
                    Some(slot) => *slot = d,
                    None => self.dialogs.push(d),
                }
            }
        }
        // A terminated dialog stays visible until the next update that does not mention it, so
        // partial updates cannot make history pile up.
        self.dialogs
            .retain(|d| d.state != DialogPhase::Terminated || listed.contains(&d.id));
        first || before != self.dialogs
    }

    /// Forgets the version counter (a new subscription dialog restarts it).
    pub fn new_dialog(&mut self) {
        self.last_version = None;
    }

    /// Whether any NOTIFY was applied.
    pub fn has_notify(&self) -> bool {
        self.got_notify
    }

    /// `entity` of the last document.
    pub fn entity(&self) -> Option<&str> {
        self.entity.as_deref()
    }

    /// Current dialogs (terminated ones included while the notifier lists them).
    pub fn dialogs(&self) -> &[BlfDialog] {
        &self.dialogs
    }

    /// Derived lamp state.
    pub fn state(&self) -> BlfState {
        if !self.got_notify {
            return BlfState::Unknown;
        }
        derive_state(&self.dialogs)
    }
}

/// Whether a dialog is an incoming call alerting the monitored phone (early / proceeding with
/// direction recipient; a missing direction counts for early / proceeding only).
pub fn is_ringing(d: &BlfDialog) -> bool {
    match d.direction {
        Some(DialogDirection::Initiator) => false,
        Some(DialogDirection::Recipient) => matches!(
            d.state,
            DialogPhase::Trying | DialogPhase::Early | DialogPhase::Proceeding
        ),
        None => matches!(d.state, DialogPhase::Early | DialogPhase::Proceeding),
    }
}

fn is_held(d: &BlfDialog) -> bool {
    d.rendering == Some(false)
        && matches!(
            d.state,
            DialogPhase::Early | DialogPhase::Proceeding | DialogPhase::Confirmed
        )
}

/// The dialog a pickup would take over: a live, rendering (not held) ringing dialog. `Some`
/// exactly when [`derive_state`] says [`BlfState::Ringing`].
pub fn ringing_dialog(dialogs: &[BlfDialog]) -> Option<&BlfDialog> {
    dialogs
        .iter()
        .find(|d| d.state != DialogPhase::Terminated && !is_held(d) && is_ringing(d))
}

/// Lamp state of a set of dialogs. Precedence: ringing, busy, dialing, held, idle. A dialog that
/// is not rendering media (`+sip.rendering="no"` on the local target) counts as held whatever
/// its phase: FreeSWITCH reports a held call as `early` + `rendering="no"` rather than
/// `confirmed` (observed against 1.10.12).
pub fn derive_state(dialogs: &[BlfDialog]) -> BlfState {
    let held = is_held;
    let live = || {
        dialogs
            .iter()
            .filter(|d| d.state != DialogPhase::Terminated)
    };
    let ringing = is_ringing;
    let dialing = |d: &BlfDialog| match d.direction {
        Some(DialogDirection::Initiator) | None => matches!(
            d.state,
            DialogPhase::Trying | DialogPhase::Proceeding | DialogPhase::Early
        ),
        Some(DialogDirection::Recipient) => false,
    };
    if live().any(|d| !held(d) && ringing(d)) {
        BlfState::Ringing
    } else if live().any(|d| !held(d) && d.state == DialogPhase::Confirmed) {
        BlfState::Busy
    } else if live().any(|d| !held(d) && dialing(d)) {
        BlfState::Dialing
    } else if live().any(held) {
        BlfState::Held
    } else {
        BlfState::Idle
    }
}

impl BlfStatus {
    /// Whether a directed pickup is offered: the entry is ringing, fresh (not stale, the
    /// subscription active) and a pickup method is configured for it (`call.pickup` refuses
    /// otherwise).
    pub fn pickup_offered(&self) -> bool {
        self.state == BlfState::Ringing
            && !self.stale
            && self.subscription.phase == SubscriptionPhase::Active
            && self.pickup != siple_config::PickupMode::Off
    }
}

/// Seconds past `expires_at` after which a silent subscription counts as stale.
pub const STALE_GRACE_SECS: u64 = 30;

/// Lifecycle phase of a BLF subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionPhase {
    /// SUBSCRIBE sent / waiting for the first NOTIFY (or for the transport).
    Pending,
    /// Confirmed by the notifier.
    Active,
    /// A refresh or resubscribe failed or was terminated; another attempt is scheduled
    /// (`retry_at`).
    Retrying,
    /// Refused for good (no further attempt until the config changes or the account restarts).
    Failed,
}

/// Health of the subscription behind a BLF entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BlfSubscription {
    /// Lifecycle phase.
    pub phase: SubscriptionPhase,
    /// SIP status code of the last failure (`None` for local / NOTIFY-reported failures and
    /// while healthy).
    pub last_code: Option<u16>,
    /// Reason phrase or error text of the last failure.
    pub last_reason: Option<String>,
    /// When the subscription lapses unless refreshed.
    pub expires_at: Option<DateTime<Utc>>,
    /// When the current subscription was first confirmed, i.e. the first 2xx to SUBSCRIBE of its
    /// dialog. In-dialog refreshes keep it; a failure, a termination, a rebuild or a new dialog
    /// (resubscribe, including after a 481 to a refresh) resets it, and the next 2xx sets it
    /// again. `None` while nothing is confirmed.
    pub subscribed_at: Option<DateTime<Utc>>,
    /// When a SUBSCRIBE (initial or refresh) last succeeded (2xx) for the current subscription.
    /// Reset together with `subscribed_at`, so it is `None` after a failure too.
    pub last_refreshed_at: Option<DateTime<Utc>>,
    /// When the next attempt happens (`retrying` only).
    pub retry_at: Option<DateTime<Utc>>,
}

impl BlfSubscription {
    /// Whether the shown state may be out of date: the subscription is retrying, or neither a
    /// NOTIFY nor a refresh extended it past `expires_at + grace`.
    pub fn is_stale(&self, now: DateTime<Utc>, grace: std::time::Duration) -> bool {
        self.phase == SubscriptionPhase::Retrying
            || self.expires_at.is_some_and(|e| {
                now > e + chrono::Duration::from_std(grace).unwrap_or(chrono::Duration::MAX)
            })
    }
}

/// Status of one BLF entry as exposed by the stack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BlfStatus {
    /// Account id.
    pub account: String,
    /// Monitored URI as configured.
    pub uri: String,
    /// Label from the config.
    pub label: String,
    /// Derived state.
    pub state: BlfState,
    /// Dialogs behind the state.
    pub dialogs: Vec<BlfDialog>,
    /// When the state last changed or a NOTIFY last arrived.
    pub updated_at: DateTime<Utc>,
    /// `[[account.blf]] group` (empty = by account).
    pub group: String,
    /// Effective directed-pickup method of this entry (`off|prefix|replaces`). Pickup is
    /// offered while the state is `ringing` and this is not `off`.
    pub pickup: siple_config::PickupMode,
    /// Health of the subscription behind the entry.
    pub subscription: BlfSubscription,
    /// The state shown may be out of date (see [`BlfSubscription::is_stale`]); the last known
    /// state stays.
    pub stale: bool,
}

#[cfg(test)]
pub(crate) mod fixtures {
    /// FreeSWITCH 1.10: idle, version 0.
    pub const FS_EMPTY: &str = r#"<?xml version="1.0"?>
<dialog-info xmlns="urn:ietf:params:xml:ns:dialog-info" version="0" state="full" entity="sip:1018@pbx.example.test">
</dialog-info>
"#;

    /// FreeSWITCH 1.10: callee early (recipient).
    pub const FS_EARLY: &str = r#"<?xml version="1.0"?>
<dialog-info xmlns="urn:ietf:params:xml:ns:dialog-info" version="1" state="full" entity="sip:1018@pbx.example.test">
  <dialog id="a1b2c3d4-0000-1111-2222-333344445555" call-id="a1b2c3d4-0000-1111-2222-333344445555" local-tag="Xp4Zg" remote-tag="9tQ1r" direction="recipient">
    <state>early</state>
    <local>
      <identity display="1018">sip:1018@pbx.example.test</identity>
      <target uri="sip:1018@pbx.example.test">
        <param pname="+sip.rendering" pvalue="yes"/>
      </target>
    </local>
    <remote>
      <identity display="Alice">sip:1019@pbx.example.test</identity>
      <target uri="sip:1019@pbx.example.test"/>
    </remote>
  </dialog>
</dialog-info>
"#;

    /// FreeSWITCH 1.10: confirmed, held (`+sip.rendering="no"`).
    pub const FS_CONFIRMED_HELD: &str = r#"<?xml version="1.0"?>
<dialog-info xmlns="urn:ietf:params:xml:ns:dialog-info" version="2" state="full" entity="sip:1018@pbx.example.test">
  <dialog id="a1b2c3d4" call-id="a1b2c3d4" local-tag="Xp4Zg" remote-tag="9tQ1r" direction="recipient">
    <state>confirmed</state>
    <local>
      <identity>sip:1018@pbx.example.test</identity>
      <target uri="sip:1018@pbx.example.test">
        <param pname="+sip.rendering" pvalue="no"/>
      </target>
    </local>
    <remote>
      <identity>sip:1019@pbx.example.test</identity>
      <target uri="sip:1019@pbx.example.test"/>
    </remote>
  </dialog>
</dialog-info>
"#;

    /// FreeSWITCH: initiator confirmed immediately (caller side).
    pub const FS_CALLER_CONFIRMED: &str = r#"<?xml version="1.0"?>
<dialog-info xmlns="urn:ietf:params:xml:ns:dialog-info" version="3" state="full" entity="sip:1018@pbx.example.test">
  <dialog id="ff00" call-id="ff00" local-tag="aa" remote-tag="bb" direction="initiator">
    <state>confirmed</state>
    <local><identity>sip:1018@pbx.example.test</identity><target uri="sip:1018@pbx.example.test"/></local>
    <remote><identity>sip:9999@pbx.example.test</identity><target uri="sip:**1018@pbx.example.test"/></remote>
  </dialog>
</dialog-info>
"#;

    /// FreeSWITCH 1.10.12 as observed: a held call is `early` + recipient + `rendering="no"`.
    pub const FS_HOLD_AS_EARLY: &str = r#"<?xml version="1.0"?>
<dialog-info xmlns="urn:ietf:params:xml:ns:dialog-info" version="5" state="full" entity="sip:1018@pbx.example.test">
  <dialog id="46dbaddc" direction="recipient">
    <state>early</state>
    <local><identity>sip:1018@pbx.example.test</identity><target uri="sip:1018@pbx.example.test"><param pname="+sip.rendering" pvalue="no"/></target></local>
    <remote><identity display="Extension 1019">sip:1019@pbx.example.test</identity><target uri="sip:1019@pbx.example.test"/></remote>
  </dialog>
</dialog-info>
"#;

    pub const FS_TERMINATED: &str = r#"<?xml version="1.0"?>
<dialog-info xmlns="urn:ietf:params:xml:ns:dialog-info" version="4" state="full" entity="sip:1018@pbx.example.test">
  <dialog id="ff00" call-id="ff00" local-tag="aa" remote-tag="bb" direction="initiator">
    <state>terminated</state>
  </dialog>
</dialog-info>
"#;

    /// Prefixed, partial, Yealink / Polycom flavoured.
    pub const PREFIXED_PARTIAL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<d:dialog-info xmlns:d="urn:ietf:params:xml:ns:dialog-info" xmlns:sa="urn:ietf:params:xml:ns:sa" version="9" state="partial" entity="sip:2001@pbx.example.com">
  <d:dialog id="call-7" call-id="7@10.1.1.5" local-tag="lt" remote-tag="rt" direction="initiator">
    <d:state code="180">early</d:state>
    <d:local><d:identity display="Desk &amp; Co">sip:2001@pbx.example.com</d:identity><d:target uri="sip:2001@10.1.1.9:5060"/></d:local>
    <d:remote><d:identity display="Bob">sip:2002@pbx.example.com</d:identity></d:remote>
  </d:dialog>
</d:dialog-info>
"#;

    pub const POLYCOM_TRYING: &str = r#"<dialog-info xmlns="urn:ietf:params:xml:ns:dialog-info" version="5" state="partial" entity="sip:3000@x">
<dialog id="p1" direction="initiator"><state>trying</state></dialog></dialog-info>"#;
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    #[test]
    fn parses_freeswitch_early() {
        let d = parse_dialog_info(FS_EARLY).unwrap();
        assert_eq!(d.version, Some(1));
        assert!(d.full);
        assert_eq!(d.entity.as_deref(), Some("sip:1018@pbx.example.test"));
        assert_eq!(d.dialogs.len(), 1);
        let x = &d.dialogs[0];
        assert_eq!(x.id, "a1b2c3d4-0000-1111-2222-333344445555");
        assert_eq!(x.direction, Some(DialogDirection::Recipient));
        assert_eq!(x.state, DialogPhase::Early);
        assert_eq!(x.local_tag.as_deref(), Some("Xp4Zg"));
        assert_eq!(x.remote_tag.as_deref(), Some("9tQ1r"));
        assert_eq!(
            x.remote_identity.as_deref(),
            Some("sip:1019@pbx.example.test")
        );
        assert_eq!(x.remote_display.as_deref(), Some("Alice"));
        assert_eq!(
            x.local_identity.as_deref(),
            Some("sip:1018@pbx.example.test")
        );
        assert_eq!(x.local_target.as_deref(), Some("sip:1018@pbx.example.test"));
        assert_eq!(x.rendering, Some(true));
    }

    #[test]
    fn parses_empty_held_and_caller() {
        let d = parse_dialog_info(FS_EMPTY).unwrap();
        assert!(d.dialogs.is_empty() && d.full && d.version == Some(0));
        let d = parse_dialog_info(FS_CONFIRMED_HELD).unwrap();
        assert_eq!(d.dialogs[0].rendering, Some(false));
        assert_eq!(derive_state(&d.dialogs), BlfState::Held);
        let d = parse_dialog_info(FS_CALLER_CONFIRMED).unwrap();
        assert_eq!(d.dialogs[0].direction, Some(DialogDirection::Initiator));
        assert_eq!(
            d.dialogs[0].remote_identity.as_deref(),
            Some("sip:9999@pbx.example.test")
        );
        assert_eq!(derive_state(&d.dialogs), BlfState::Busy);
    }

    #[test]
    fn freeswitch_hold_reported_as_early_is_held() {
        let d = parse_dialog_info(FS_HOLD_AS_EARLY).unwrap();
        assert_eq!(d.dialogs[0].state, DialogPhase::Early);
        assert_eq!(d.dialogs[0].rendering, Some(false));
        assert_eq!(derive_state(&d.dialogs), BlfState::Held);
        // The same dialog while ringing renders (`yes`): ringing.
        let r = parse_dialog_info(FS_EARLY).unwrap();
        assert_eq!(derive_state(&r.dialogs), BlfState::Ringing);
    }

    #[test]
    fn parses_prefixed_partial() {
        let d = parse_dialog_info(PREFIXED_PARTIAL).unwrap();
        assert!(!d.full);
        assert_eq!(d.version, Some(9));
        let x = &d.dialogs[0];
        assert_eq!(x.state, DialogPhase::Early);
        assert_eq!(x.direction, Some(DialogDirection::Initiator));
        assert_eq!(x.local_target.as_deref(), Some("sip:2001@10.1.1.9:5060"));
        assert_eq!(x.remote_display.as_deref(), Some("Bob"));
        assert_eq!(derive_state(&d.dialogs), BlfState::Dialing);
        let d = parse_dialog_info(POLYCOM_TRYING).unwrap();
        assert_eq!(d.dialogs[0].state, DialogPhase::Trying);
        assert_eq!(derive_state(&d.dialogs), BlfState::Dialing);
    }

    #[test]
    fn rejects_non_dialog_info() {
        assert!(parse_dialog_info("").is_none());
        assert!(parse_dialog_info("hello").is_none());
        assert!(parse_dialog_info("<presence/>").is_none());
        // Truncated body keeps what was parsed.
        let d = parse_dialog_info("<dialog-info version=\"1\" state=\"full\"><dialog id=\"x\"><state>confirmed</state></dialog>").unwrap();
        assert_eq!(d.dialogs.len(), 1);
    }

    fn dialog(id: &str, st: DialogPhase, dir: Option<DialogDirection>) -> BlfDialog {
        BlfDialog {
            id: id.into(),
            call_id: None,
            local_tag: None,
            remote_tag: None,
            direction: dir,
            state: st,
            remote_identity: None,
            remote_display: None,
            local_identity: None,
            local_target: None,
            remote_target: None,
            rendering: None,
        }
    }

    #[test]
    fn derived_state_table() {
        use DialogDirection::*;
        use DialogPhase::*;
        assert_eq!(derive_state(&[]), BlfState::Idle);
        assert_eq!(
            derive_state(&[dialog("a", Terminated, None)]),
            BlfState::Idle
        );
        assert_eq!(
            derive_state(&[dialog("a", Early, Some(Recipient))]),
            BlfState::Ringing
        );
        assert_eq!(
            derive_state(&[dialog("a", Proceeding, Some(Recipient))]),
            BlfState::Ringing
        );
        assert_eq!(derive_state(&[dialog("a", Early, None)]), BlfState::Ringing);
        assert_eq!(
            derive_state(&[dialog("a", Early, Some(Initiator))]),
            BlfState::Dialing
        );
        assert_eq!(
            derive_state(&[dialog("a", Trying, Some(Initiator))]),
            BlfState::Dialing
        );
        assert_eq!(
            derive_state(&[dialog("a", Trying, None)]),
            BlfState::Dialing
        );
        assert_eq!(
            derive_state(&[dialog("a", Confirmed, Some(Recipient))]),
            BlfState::Busy
        );
        let mut held = dialog("a", Confirmed, None);
        held.rendering = Some(false);
        assert_eq!(derive_state(&[held.clone()]), BlfState::Held);
        // Held call + second one dialing: the consult call wins.
        assert_eq!(
            derive_state(&[held.clone(), dialog("b", Trying, Some(Initiator))]),
            BlfState::Dialing
        );
        // Held + active call: busy.
        assert_eq!(
            derive_state(&[held.clone(), dialog("b", Confirmed, None)]),
            BlfState::Busy
        );
        // Anything ringing wins.
        assert_eq!(
            derive_state(&[
                dialog("b", Confirmed, None),
                dialog("c", Early, Some(Recipient))
            ]),
            BlfState::Ringing
        );
    }

    #[test]
    fn tracker_transitions_full_updates() {
        let mut t = BlfTracker::default();
        assert_eq!(t.state(), BlfState::Unknown);
        assert!(
            t.apply(parse_dialog_info(FS_EMPTY).unwrap()),
            "first notify is a change"
        );
        assert_eq!(t.state(), BlfState::Idle);
        assert!(t.apply(parse_dialog_info(FS_EARLY).unwrap()));
        assert_eq!(t.state(), BlfState::Ringing);
        assert!(t.apply(parse_dialog_info(FS_CONFIRMED_HELD).unwrap()));
        assert_eq!(t.state(), BlfState::Held);
        assert!(t.apply(parse_dialog_info(FS_CALLER_CONFIRMED).unwrap()));
        assert_eq!(t.state(), BlfState::Busy);
        // Identical resend: no change.
        let mut same = parse_dialog_info(FS_CALLER_CONFIRMED).unwrap();
        same.version = Some(4);
        assert!(!t.apply(same));
        assert!(t.apply(parse_dialog_info(FS_TERMINATED).unwrap()));
        assert_eq!(t.state(), BlfState::Idle);
        assert_eq!(t.entity(), Some("sip:1018@pbx.example.test"));
    }

    #[test]
    fn stale_versions_are_ignored() {
        let mut t = BlfTracker::default();
        t.apply(parse_dialog_info(FS_CALLER_CONFIRMED).unwrap()); // v3
        let mut old = parse_dialog_info(FS_EMPTY).unwrap(); // v0
        old.version = Some(2);
        assert!(!t.apply(old));
        assert_eq!(t.state(), BlfState::Busy);
        // A new subscription dialog restarts numbering.
        t.new_dialog();
        let mut fresh = parse_dialog_info(FS_EMPTY).unwrap();
        fresh.version = Some(0);
        assert!(t.apply(fresh));
        assert_eq!(t.state(), BlfState::Idle);
    }

    #[test]
    fn partial_updates_merge_by_dialog_id() {
        let mut t = BlfTracker::default();
        let mut full = parse_dialog_info(FS_EARLY).unwrap();
        full.version = Some(1);
        t.apply(full);
        // Partial: second dialog appears.
        let p = parse_dialog_info(
            r#"<dialog-info version="2" state="partial" entity="e"><dialog id="other" direction="initiator"><state>confirmed</state></dialog></dialog-info>"#,
        )
        .unwrap();
        assert!(t.apply(p));
        assert_eq!(t.dialogs().len(), 2);
        assert_eq!(t.state(), BlfState::Ringing);
        // Partial: first dialog confirmed (replaced by id), still two.
        let p = parse_dialog_info(
            r#"<dialog-info version="3" state="partial" entity="e"><dialog id="a1b2c3d4-0000-1111-2222-333344445555" direction="recipient"><state>confirmed</state></dialog></dialog-info>"#,
        )
        .unwrap();
        assert!(t.apply(p));
        assert_eq!(t.dialogs().len(), 2);
        assert_eq!(t.state(), BlfState::Busy);
        // Partial: both terminated -> idle.
        let p = parse_dialog_info(
            r#"<dialog-info version="4" state="partial" entity="e"><dialog id="a1b2c3d4-0000-1111-2222-333344445555"><state>terminated</state></dialog><dialog id="other"><state>terminated</state></dialog></dialog-info>"#,
        )
        .unwrap();
        assert!(t.apply(p));
        assert_eq!(t.state(), BlfState::Idle);
    }

    fn sub(phase: SubscriptionPhase, expires_in: Option<i64>) -> BlfSubscription {
        BlfSubscription {
            phase,
            last_code: None,
            last_reason: None,
            expires_at: expires_in.map(|s| Utc::now() + chrono::Duration::seconds(s)),
            subscribed_at: None,
            last_refreshed_at: None,
            retry_at: None,
        }
    }

    #[test]
    fn stale_transitions() {
        use SubscriptionPhase::*;
        use std::time::Duration;
        let now = Utc::now();
        let g = Duration::from_secs(STALE_GRACE_SECS);
        // Fresh, never notified: not stale.
        assert!(!sub(Pending, None).is_stale(now, g));
        assert!(!sub(Active, Some(100)).is_stale(now, g));
        // Past expires_at but inside the grace: not stale; beyond it: stale.
        assert!(!sub(Active, Some(-10)).is_stale(now, g));
        assert!(sub(Active, Some(-31)).is_stale(now, g));
        // A refresh moves expires_at forward: fresh again.
        assert!(!sub(Active, Some(500)).is_stale(now, g));
        // Retrying is stale straight away, failed alone is not (the key shows Failed).
        assert!(sub(Retrying, Some(100)).is_stale(now, g));
        assert!(sub(Retrying, None).is_stale(now, g));
        assert!(!sub(Failed, None).is_stale(now, g));
        // The grace is a parameter.
        assert!(sub(Active, Some(-2)).is_stale(now, Duration::from_secs(1)));
        assert!(!sub(Active, Some(-2)).is_stale(now, Duration::from_secs(5)));
    }

    #[test]
    fn ringing_dialog_matches_ringing_state() {
        use DialogDirection::*;
        use DialogPhase::*;
        let r = dialog("r", Early, Some(Recipient));
        let c = dialog("c", Confirmed, Some(Recipient));
        let mut held = dialog("h", Early, Some(Recipient));
        held.rendering = Some(false);
        assert_eq!(
            ringing_dialog(&[c.clone(), r.clone()]).map(|d| d.id.as_str()),
            Some("r")
        );
        assert!(ringing_dialog(std::slice::from_ref(&c)).is_none());
        assert!(ringing_dialog(std::slice::from_ref(&held)).is_none());
        assert!(ringing_dialog(&[dialog("i", Early, Some(Initiator))]).is_none());
        for set in [vec![r], vec![c], vec![held], vec![]] {
            assert_eq!(
                ringing_dialog(&set).is_some(),
                derive_state(&set) == BlfState::Ringing
            );
        }
    }

    #[test]
    fn bidi_and_zero_width_characters_are_dropped() {
        let xml = "<dialog-info version=\"1\" state=\"full\"><dialog id=\"d\" call-id=\"c&#x200B;1\" local-tag=\"l\" remote-tag=\"r\" direction=\"recipient\"><state>early</state><remote><identity display=\"A&#x202E;lice&#x200B;&#xFEFF;&#x2066;\">sip:2@x</identity></remote></dialog></dialog-info>";
        let d = parse_dialog_info(xml).unwrap();
        let x = &d.dialogs[0];
        assert_eq!(x.remote_display.as_deref(), Some("Alice"));
        assert!(
            x.call_id.is_none(),
            "format characters are not repaired in header data"
        );
    }

    #[test]
    fn remote_target_is_parsed() {
        let d = parse_dialog_info(FS_EARLY).unwrap();
        assert_eq!(
            d.dialogs[0].remote_target.as_deref(),
            Some("sip:1019@pbx.example.test")
        );
        let d = parse_dialog_info(FS_CALLER_CONFIRMED).unwrap();
        assert_eq!(
            d.dialogs[0].remote_target.as_deref(),
            Some("sip:**1018@pbx.example.test")
        );
    }

    #[test]
    fn control_characters_never_survive_parsing() {
        let xml = "<dialog-info version=\"1\" state=\"full\"><dialog id=\"a&#13;&#10;b\" call-id=\"c&#13;&#10;Require: x\" local-tag=\"t&#10;1\" remote-tag=\"ok\" direction=\"recipient\"><state>early</state><local><identity display=\"Ev&#13;&#10;il\">sip:1@x&#13;&#10;</identity></local><remote><identity display=\"Bo&#10;b\">sip:2@x&#13;&#10;Y: 1</identity><target uri=\"sip:2@x&#13;&#10;Y: 1\"/></remote></dialog></dialog-info>";
        let d = parse_dialog_info(xml).unwrap();
        let x = &d.dialogs[0];
        // Header material is dropped, not repaired.
        assert!(x.call_id.is_none() && x.local_tag.is_none() && x.remote_target.is_none());
        assert_eq!(x.remote_tag.as_deref(), Some("ok"));
        // Display material loses the control characters.
        assert_eq!(x.id, "ab");
        assert_eq!(x.remote_display.as_deref(), Some("Bob"));
        assert_eq!(x.remote_identity.as_deref(), Some("sip:2@xY: 1"));
        assert_eq!(x.local_identity.as_deref(), Some("sip:1@x"));
        for s in [
            &x.id,
            x.remote_display.as_ref().unwrap(),
            x.remote_identity.as_ref().unwrap(),
        ] {
            assert!(!s.chars().any(char::is_control), "{s:?}");
        }
    }
}

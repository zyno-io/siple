//! Subscription client: SUBSCRIBE dialogs over raw transactions for BLF
//! (`dialog`), MWI (`message-summary`) and generic event packages.
//!
//! One [`SubManager`] per account actor, one task per subscription. State shared between the
//! task (which sends SUBSCRIBEs) and the router (which delivers NOTIFYs) lives in a mutex per
//! subscription, so a NOTIFY that beats the 2xx establishes the dialog synchronously and the
//! task only reconciles afterwards. NOTIFYs are matched by Call-ID + our From tag + Event
//! *before* the dialog layer sees them (see [`crate::router`]).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use chrono::{DateTime, Utc};
use rsipstack::sip::prelude::*;
use rsipstack::sip::{self, Header, Method, Param, Request, Uri};
use rsipstack::transaction::{make_call_id, make_tag};
use rsipstack::transport::SipAddr;
use serde::Serialize;
use siple_config::{AccountConfig, MwiMode, PickupMode, Transport};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;
use tokio::time::Instant;

use crate::account::{CALLID_SUFFIX, sip_transport, uri_destination};
use crate::blf::{
    BlfDialog, BlfState, BlfStatus, BlfSubscription, BlfTracker, SubscriptionPhase,
    parse_dialog_info,
};
use crate::calls::AccountCalls;
use crate::client::{Client, build_contact};
use crate::dns::{self, SystemSrvSource};
use crate::error::{Result, SipError};
use crate::mwi::{MwiStatus, status_from_body};
use crate::types::SipEvent;

/// Fraction of the granted lifetime after which a subscription is refreshed.
pub const REFRESH_FRACTION: f64 = 0.85;
/// Default lifetime of `message-summary` subscriptions (no config knob).
const MWI_EXPIRES: u32 = 3600;
/// Default lifetime of generic subscriptions.
const GENERIC_EXPIRES: u32 = 600;
/// Bound on the best-effort `Expires: 0` of a release / shutdown.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(2);
/// Overall bound on [`SubManager::release_all`].
const RELEASE_ALL_TIMEOUT: Duration = Duration::from_secs(3);
/// Count cap on the dead Call-IDs remembered to answer late NOTIFYs with 481; entries also
/// expire by time (`max(blf_expires, 3600)` s).
const RECENT_DEAD: usize = 4096;
/// Minimum lifetime of a dead-dialog entry.
const DEAD_MIN_TTL: Duration = Duration::from_secs(3600);
/// A subscription that stayed active this long resets the resubscribe back-off.
const STABLE_AFTER: Duration = Duration::from_secs(60);
/// Longest wait between resubscribe attempts (also caps `Retry-After`).
const MAX_RETRY_DELAY: Duration = Duration::from_secs(300);

static NEXT_SUB: AtomicU64 = AtomicU64::new(1);

/// Hot BLF attributes of a subscription: `(group, effective pickup method)`.
type Meta = (String, PickupMode);

// --- public types -------------------------------------------------------------------------------

/// What a subscription is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubKind {
    /// `[[account.blf]]` entry.
    Blf,
    /// The account's `message-summary` subscription.
    Mwi,
    /// Created through the API.
    Generic,
}

/// State of a subscription.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SubState {
    /// SUBSCRIBE sent / not yet confirmed by a NOTIFY (or waiting for the transport).
    Pending,
    /// The notifier confirmed the subscription.
    Active,
    /// Ended by the notifier or by us; may be resubscribed (see `Subscription-State` reasons).
    Terminated,
    /// Refused or unreachable.
    Failed {
        /// SIP status code, 0 for local / NOTIFY-reported failures.
        code: u16,
        /// Reason phrase or error text.
        reason: String,
    },
}

/// Status of one subscription.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SubscriptionInfo {
    /// Process-unique id (`s1`, `s2`, ...).
    pub id: String,
    /// Account id.
    pub account: String,
    /// Subscribed URI.
    pub uri: String,
    /// Event package.
    pub event: String,
    /// `Accept` value sent.
    pub accept: String,
    /// What it is for.
    pub kind: SubKind,
    /// Current state.
    pub state: SubState,
    /// When the subscription lapses unless refreshed.
    pub expires_at: Option<DateTime<Utc>>,
    /// When the last NOTIFY arrived.
    pub last_notify_at: Option<DateTime<Utc>>,
}

/// Parsed `Subscription-State` header.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubStateHeader {
    /// `active`, `pending`, `terminated` (lower-case; other values kept as is). Empty if the
    /// header is missing.
    pub state: String,
    /// `expires` parameter.
    pub expires: Option<u32>,
    /// `reason` parameter (terminated).
    pub reason: Option<String>,
    /// `retry-after` parameter (terminated).
    pub retry_after: Option<u32>,
}

/// Parses `active;expires=120`, `terminated;reason=deactivated;retry-after=5`, ...
pub fn parse_subscription_state(v: &str) -> SubStateHeader {
    let mut parts = v.split(';');
    let mut out = SubStateHeader {
        state: parts.next().unwrap_or("").trim().to_ascii_lowercase(),
        ..Default::default()
    };
    for p in parts {
        let (k, val) = p.split_once('=').unwrap_or((p, ""));
        let val = val.trim().trim_matches('"');
        match k.trim().to_ascii_lowercase().as_str() {
            "expires" => out.expires = val.parse().ok(),
            "reason" => out.reason = Some(val.to_ascii_lowercase()).filter(|r| !r.is_empty()),
            "retry-after" => out.retry_after = val.parse().ok(),
            _ => {}
        }
    }
    out
}

/// What to do after `Subscription-State: terminated`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermAction {
    /// Subscribe again (new dialog) after this delay.
    Resubscribe(Duration),
    /// Give up; the subscription is `failed`.
    Stop,
}

/// Exponential back-off for repeated resubscribes: 1, 2, 4, ... 60 s.
pub fn backoff(fail_count: u32) -> Duration {
    Duration::from_secs(1u64 << fail_count.min(6)).min(Duration::from_secs(60))
}

/// Resubscribe delay: `max(retry_after, backoff(n))`, never below 1 s, at most 300 s.
pub fn retry_delay(retry_after: Option<u32>, fail_count: u32) -> Duration {
    let ra = Duration::from_secs(u64::from(retry_after.unwrap_or(0)));
    ra.max(backoff(fail_count))
        .max(Duration::from_secs(1))
        .min(MAX_RETRY_DELAY)
}

/// RFC 6665 §4.1.3 termination handling.
pub fn termination_action(
    reason: Option<&str>,
    retry_after: Option<u32>,
    fail_count: u32,
) -> TermAction {
    match reason.map(str::to_ascii_lowercase).as_deref() {
        Some("rejected" | "noresource" | "invariant") => TermAction::Stop,
        // `giveup`: retry only when the notifier says when.
        Some("giveup") if retry_after.is_none() => TermAction::Stop,
        // `deactivated`, `timeout`, `probation`, `giveup;retry-after`, no / unknown reason.
        _ => TermAction::Resubscribe(retry_delay(retry_after, fail_count)),
    }
}

/// Delay before refreshing a subscription granted `expires` seconds.
pub fn refresh_delay(expires: u32) -> Duration {
    Duration::from_secs_f64((f64::from(expires) * REFRESH_FRACTION).max(1.0))
}

/// Whether a failed SUBSCRIBE is worth retrying with back-off.
pub fn retryable(code: u16) -> bool {
    matches!(code, 408 | 480 | 486 | 491) || (500..600).contains(&code)
}

/// Turns the configured target (`1018`, `1018@host`, `sip:1018@host`, `sips:...`) into a URI;
/// bare extensions get the account's domain.
pub fn resolve_target(spec: &str, aor: &Uri) -> Result<Uri> {
    let s = spec.trim();
    if s.is_empty() {
        return Err(SipError::InvalidArgument("empty subscription URI".into()));
    }
    let full = if s.starts_with("sip:") || s.starts_with("sips:") {
        s.to_string()
    } else if s.contains('@') {
        format!("sip:{s}")
    } else {
        format!("sip:{s}@{}", aor.host_with_port)
    };
    Uri::try_from(full.as_str())
        .map_err(|e| SipError::InvalidArgument(format!("bad subscription URI {spec:?}: {e}")))
}

fn default_accept(event: &str) -> &'static str {
    match event.split(';').next().unwrap_or("").trim() {
        "dialog" => "application/dialog-info+xml",
        "message-summary" => "application/simple-message-summary",
        "presence" => "application/pidf+xml",
        _ => "*/*",
    }
}

// --- header helpers -------------------------------------------------------------------------------

fn header_values(headers: &sip::Headers, name: &str) -> Vec<String> {
    headers
        .iter()
        .filter_map(|h| {
            let s = h.to_string();
            let (n, v) = s.split_once(':')?;
            n.trim()
                .eq_ignore_ascii_case(name)
                .then(|| v.trim().to_string())
        })
        .collect()
}

fn header_value(headers: &sip::Headers, name: &str) -> Option<String> {
    header_values(headers, name).into_iter().next()
}

/// Splits a comma-joined list of name-addrs, ignoring commas inside `<...>`.
fn split_list(v: &str) -> Vec<String> {
    let (mut out, mut depth, mut cur) = (Vec::new(), 0i32, String::new());
    for c in v.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth -= 1,
            ',' if depth <= 0 => {
                if !cur.trim().is_empty() {
                    out.push(cur.trim().to_string());
                }
                cur.clear();
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

fn uri_of_name_addr(v: &str) -> Option<Uri> {
    let s = match (v.find('<'), v.find('>')) {
        (Some(a), Some(b)) if b > a => &v[a + 1..b],
        _ => v.split(';').next().unwrap_or(v).trim(),
    };
    Some(s.trim())
        .filter(|u| u.contains(':'))
        .and_then(|u| Uri::try_from(u).ok())
}

fn tag_of(h: Option<&sip::headers::To>) -> Option<String> {
    h.and_then(|t| t.tag().ok().flatten())
        .map(|t| t.to_string())
}

/// `(package, id)` of an `Event` header value (`presence;id=5`), for RFC 6665 §8.2.1 matching.
fn event_key(v: &str) -> (String, Option<String>) {
    let mut parts = v.split(';');
    let pkg = parts.next().unwrap_or("").trim().to_ascii_lowercase();
    let id = parts.find_map(|p| {
        let (k, val) = p.split_once('=')?;
        k.trim()
            .eq_ignore_ascii_case("id")
            .then(|| val.trim().trim_matches('"').to_string())
    });
    (pkg, id)
}

// --- shared state -------------------------------------------------------------------------------

/// Everything a subscription task needs from the account runtime.
pub(crate) struct SubEnv {
    pub client: Client,
    /// Registered Contact; `None` derives one from the connection (peer mode).
    pub contact: Option<Uri>,
    /// Changes when subscriptions must start over (new runtime, new flow / Contact, new AOR).
    pub key: String,
    pub dns: Arc<SystemSrvSource>,
    pub transport: Transport,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Action {
    Refresh,
    NewDialog,
}

struct SubShared {
    info: SubscriptionInfo,
    /// As configured / requested (BLF: `[[account.blf]].uri`).
    spec: String,
    target: Uri,
    event: String,
    /// Explicit lifetime (generic); BLF / MWI use the manager's values.
    expires_req: Option<u32>,
    expires_override: Option<u32>,
    label: String,
    /// `[[account.blf]] group` / effective pickup method (hot: no resubscribe).
    group: String,
    pickup: PickupMode,
    /// Last failure `(code, reason)`; cleared when the subscription becomes active.
    last_error: Option<(u16, String)>,
    /// Another attempt is scheduled or in flight after a failure / termination: shown as
    /// `retrying` (stale, last known state kept) until a 2xx, an active NOTIFY or a permanent
    /// failure ends it.
    retrying: bool,
    /// When the retry was scheduled to run (kept while the attempt is in flight).
    retry_at: Option<DateTime<Utc>>,
    // dialog
    call_id: String,
    from_tag: String,
    to_tag: Option<String>,
    remote_target: Option<Uri>,
    route_set: Vec<String>,
    cseq: Arc<AtomicU32>,
    /// The dialog ended (terminated NOTIFY / unsubscribe / failure); NOTIFYs get 481.
    ended: bool,
    // timers / counters
    next: Option<(Instant, Action)>,
    fail_count: u32,
    /// A NOTIFY with a lifetime arrived since the last SUBSCRIBE was sent.
    notify_since_send: bool,
    /// Since when the subscription has been active (NOTIFY-confirmed) in this dialog.
    stable_since: Option<Instant>,
    /// CSeq of the last NOTIFY applied in this dialog (RFC 6665 ordering).
    remote_cseq: Option<u32>,
    // BLF
    tracker: BlfTracker,
    last_blf: Option<(BlfState, Vec<BlfDialog>, String)>,
    blf_updated: DateTime<Utc>,
}

impl SubShared {
    fn blf_view(&self) -> (BlfState, Vec<BlfDialog>) {
        match self.info.state {
            // Retrying: the last known picture stays (flagged stale by the health).
            _ if self.retrying => (self.tracker.state(), self.tracker.dialogs().to_vec()),
            SubState::Failed { .. } => (BlfState::Failed, self.tracker.dialogs().to_vec()),
            // Without a live subscription dialog the last picture is stale.
            _ if self.ended => (BlfState::Unknown, Vec::new()),
            _ => (self.tracker.state(), self.tracker.dialogs().to_vec()),
        }
    }

    fn emit_blf(&mut self, ev: &UnboundedSender<SipEvent>) {
        if self.info.kind != SubKind::Blf {
            return;
        }
        let (state, dialogs) = self.blf_view();
        let view = (state, dialogs.clone(), self.label.clone());
        if self.last_blf.as_ref() == Some(&view) {
            return;
        }
        // The very first view (Unknown, nothing known yet) is not news.
        if self.last_blf.is_none() && state == BlfState::Unknown {
            self.last_blf = Some(view);
            return;
        }
        self.last_blf = Some(view);
        self.blf_updated = Utc::now();
        let _ = ev.send(SipEvent::BlfState {
            account: self.info.account.clone(),
            uri: self.spec.clone(),
            label: self.label.clone(),
            state,
            dialogs,
        });
    }

    /// Health of a BLF subscription as exposed in [`BlfStatus`].
    fn blf_health(&self) -> BlfSubscription {
        let phase = if self.retrying {
            SubscriptionPhase::Retrying
        } else {
            match &self.info.state {
                SubState::Pending | SubState::Terminated => SubscriptionPhase::Pending,
                SubState::Active => SubscriptionPhase::Active,
                SubState::Failed { .. } => SubscriptionPhase::Failed,
            }
        };
        BlfSubscription {
            phase,
            last_code: self.last_error.as_ref().map(|e| e.0).filter(|c| *c != 0),
            last_reason: self.last_error.as_ref().map(|e| e.1.clone()),
            expires_at: self.info.expires_at,
            retry_at: self.retry_at.filter(|_| self.retrying),
        }
    }

    /// Marks a retry as scheduled after `delay`.
    fn schedule_retry(&mut self, delay: Duration, action: Action) {
        self.retrying = true;
        self.retry_at = Some(Utc::now() + chrono::Duration::from_std(delay).unwrap_or_default());
        self.next = Some((Instant::now() + delay, action));
    }

    fn set_state(&mut self, new: SubState, ev: &UnboundedSender<SipEvent>) {
        match &new {
            SubState::Failed { code, reason } => self.last_error = Some((*code, reason.clone())),
            SubState::Active => {
                self.last_error = None;
                self.retrying = false;
            }
            _ => {}
        }
        if self.info.state != new {
            tracing::info!(account = %self.info.account, sub = %self.info.id, uri = %self.info.uri,
                event = %self.info.event, state = ?new, "subscription state");
            self.info.state = new.clone();
            let _ = ev.send(SipEvent::SubscriptionState {
                account: self.info.account.clone(),
                sub_id: self.info.id.clone(),
                uri: self.info.uri.clone(),
                event: self.info.event.clone(),
                kind: self.info.kind,
                state: new,
            });
        }
        self.emit_blf(ev);
    }

    fn set_expires(&mut self, secs: u32) {
        self.info.expires_at = Some(Utc::now() + chrono::Duration::seconds(i64::from(secs)));
    }
}

enum SubCmd {
    /// Something in the shared state changed (timers); re-evaluate.
    Poke,
    SetEnv(Option<Arc<SubEnv>>),
    /// Start over with a new dialog now.
    Restart,
    /// Retry a permanently failed subscription.
    Kick,
    /// Unsubscribe (best effort); with `exit` the task ends.
    Release {
        exit: bool,
        reply: oneshot::Sender<()>,
    },
}

struct SubHandle {
    id: String,
    key: String,
    shared: Arc<Mutex<SubShared>>,
    tx: UnboundedSender<SubCmd>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

struct MgrInner {
    subs: HashMap<String, Arc<SubHandle>>,
    by_call: HashMap<String, String>,
    dead: VecDeque<(String, Instant)>,
    /// Keys (`blf:<uri>`) in configured order.
    blf_order: Vec<String>,
    env: Option<Arc<SubEnv>>,
    mwi_mode: MwiMode,
    mwi: Option<MwiStatus>,
    blf_expires: u32,
    stagger: u32,
}

/// Result of offering a NOTIFY to the manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotifyOutcome {
    /// Matched and processed: reply 200.
    Accepted,
    /// Matched a dead / mismatching subscription: reply 481.
    NoSubscription,
    /// Not ours: continue with the normal routing.
    NotOurs,
}

/// Subscriptions of one account.
pub(crate) struct SubManager {
    account: String,
    events: UnboundedSender<SipEvent>,
    calls: Arc<AccountCalls>,
    inner: Mutex<MgrInner>,
}

impl SubManager {
    pub(crate) fn new(
        account: String,
        events: UnboundedSender<SipEvent>,
        calls: Arc<AccountCalls>,
    ) -> Arc<Self> {
        Arc::new(Self {
            account,
            events,
            calls,
            inner: Mutex::new(MgrInner {
                subs: HashMap::new(),
                by_call: HashMap::new(),
                dead: VecDeque::new(),
                blf_order: Vec::new(),
                env: None,
                mwi_mode: MwiMode::Subscribe,
                mwi: None,
                blf_expires: 600,
                stagger: 0,
            }),
        })
    }

    // --- queries ---------------------------------------------------------------------------

    pub(crate) fn list(&self) -> Vec<SubscriptionInfo> {
        let handles: Vec<_> = lock(&self.inner).subs.values().cloned().collect();
        let mut v: Vec<_> = handles
            .iter()
            .map(|h| lock(&h.shared).info.clone())
            .collect();
        v.sort_by_key(|i| i.id.trim_start_matches('s').parse::<u64>().unwrap_or(0));
        v
    }

    pub(crate) fn owns(&self, sub_id: &str) -> bool {
        lock(&self.inner).subs.contains_key(sub_id)
    }

    pub(crate) fn blf_statuses(&self, stale_grace: Duration) -> Vec<BlfStatus> {
        let handles: Vec<_> = lock(&self.inner).subs.values().cloned().collect();
        let order = lock(&self.inner).blf_order.clone();
        let mut v: Vec<((usize, u64), BlfStatus)> = handles
            .iter()
            .filter_map(|h| {
                let s = lock(&h.shared);
                (s.info.kind == SubKind::Blf).then(|| {
                    let (state, dialogs) = s.blf_view();
                    let subscription = s.blf_health();
                    let stale = subscription.is_stale(Utc::now(), stale_grace);
                    (
                        (
                            order.iter().position(|k| *k == h.key).unwrap_or(usize::MAX),
                            s.info.id.trim_start_matches('s').parse().unwrap_or(0),
                        ),
                        BlfStatus {
                            account: self.account.clone(),
                            uri: s.spec.clone(),
                            label: s.label.clone(),
                            state,
                            dialogs,
                            updated_at: s.blf_updated,
                            group: s.group.clone(),
                            pickup: s.pickup,
                            subscription,
                            stale,
                        },
                    )
                })
            })
            .collect();
        v.sort_by_key(|(n, _)| *n);
        v.into_iter().map(|(_, b)| b).collect()
    }

    pub(crate) fn mwi(&self) -> Option<MwiStatus> {
        lock(&self.inner).mwi.clone()
    }

    // --- MWI -------------------------------------------------------------------------------

    fn set_mwi(&self, status: MwiStatus) {
        let mut g = lock(&self.inner);
        let changed = g.mwi.as_ref().is_none_or(|o| !o.same_content(&status));
        g.mwi = Some(status.clone());
        if changed {
            let _ = self.events.send(SipEvent::Mwi {
                account: self.account.clone(),
                status,
            });
        }
    }

    /// An out-of-dialog `message-summary` NOTIFY. Returns whether it fed the MWI status.
    pub(crate) fn on_unsolicited_mwi(&self, body: &str) -> bool {
        if lock(&self.inner).mwi_mode == MwiMode::Off {
            return false;
        }
        self.set_mwi(status_from_body(&self.account, body, Utc::now()));
        true
    }

    // --- configuration -----------------------------------------------------------------------

    /// Makes the BLF / MWI subscriptions match the account config: diff by URI,
    /// labels hot, expiry at the next refresh.
    pub(crate) fn configure(self: &Arc<Self>, cfg: &AccountConfig) {
        let Ok(aor) = Uri::try_from(cfg.aor().as_str()) else {
            return;
        };
        let mut gone = Vec::new();
        {
            let mut g = lock(&self.inner);
            g.blf_expires = cfg.blf_expires;
            g.blf_order = cfg.blf.iter().map(|b| format!("blf:{}", b.uri)).collect();
            g.mwi_mode = cfg.mwi;
            if cfg.mwi == MwiMode::Off {
                g.mwi = None;
            }
        }
        // key, kind, spec, label, event, (group, pickup)
        let mut want: Vec<(String, SubKind, String, String, String, Meta)> = Vec::new();
        for b in &cfg.blf {
            want.push((
                format!("blf:{}", b.uri),
                SubKind::Blf,
                b.uri.clone(),
                b.label.clone(),
                "dialog".into(),
                (b.group.clone(), b.pickup_for(cfg).mode),
            ));
        }
        if cfg.mwi == MwiMode::Subscribe {
            let spec = if cfg.mwi_uri.is_empty() {
                cfg.aor()
            } else {
                cfg.mwi_uri.clone()
            };
            want.push((
                "mwi".into(),
                SubKind::Mwi,
                spec,
                String::new(),
                "message-summary".into(),
                Meta::default(),
            ));
        }
        let mut to_start = Vec::new();
        {
            let mut g = lock(&self.inner);
            // Remove what is no longer wanted (or whose MWI target changed).
            let existing: Vec<Arc<SubHandle>> = g
                .subs
                .values()
                .filter(|h| h.key.starts_with("blf:") || h.key == "mwi")
                .cloned()
                .collect();
            for h in existing {
                let keep = want.iter().find(|w| w.0 == h.key);
                let drop_it = match keep {
                    None => true,
                    Some(w) => h.key == "mwi" && lock(&h.shared).spec != w.2,
                };
                if drop_it {
                    g.subs.remove(&h.id);
                    gone.push(h);
                }
            }
            for (key, kind, spec, label, event, meta) in &want {
                let existing = g.subs.values().find(|h| &h.key == key).cloned();
                match existing {
                    Some(h) => {
                        let mut s = lock(&h.shared);
                        let new_target = resolve_target(spec, &aor);
                        let mut restart = false;
                        if let Ok(t) = new_target
                            && t.to_string() != s.target.to_string()
                        {
                            s.target = t;
                            s.info.uri = s.target.to_string();
                            restart = true;
                        }
                        s.label.clone_from(label);
                        s.group.clone_from(&meta.0);
                        s.pickup = meta.1;
                        s.emit_blf(&self.events);
                        drop(s);
                        if restart {
                            let _ = h.tx.send(SubCmd::Restart);
                        }
                    }
                    None => {
                        let Ok(target) = resolve_target(spec, &aor) else {
                            tracing::warn!(account = %self.account, %spec, "invalid BLF / MWI URI; skipped");
                            continue;
                        };
                        to_start.push((
                            key.clone(),
                            *kind,
                            spec.clone(),
                            label.clone(),
                            event.clone(),
                            meta.clone(),
                            target,
                        ));
                    }
                }
            }
        }
        for h in gone {
            self.spawn_release(h);
        }
        for (key, kind, spec, label, event, meta, target) in to_start {
            let accept = default_accept(&event).to_string();
            self.create(key, kind, spec, label, event, accept, None, meta, target);
        }
    }

    // --- creation / removal ----------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn create(
        self: &Arc<Self>,
        key: String,
        kind: SubKind,
        spec: String,
        label: String,
        event: String,
        accept: String,
        expires_req: Option<u32>,
        meta: Meta,
        target: Uri,
    ) -> String {
        let n = NEXT_SUB.fetch_add(1, Ordering::SeqCst);
        let id = format!("s{n}");
        let (tx, rx) = unbounded_channel();
        let shared = Arc::new(Mutex::new(SubShared {
            info: SubscriptionInfo {
                id: id.clone(),
                account: self.account.clone(),
                uri: target.to_string(),
                event: event.clone(),
                accept,
                kind,
                state: SubState::Pending,
                expires_at: None,
                last_notify_at: None,
            },
            spec,
            target,
            event,
            expires_req,
            expires_override: None,
            label,
            group: meta.0,
            pickup: meta.1,
            last_error: None,
            retrying: false,
            retry_at: None,
            call_id: String::new(),
            from_tag: String::new(),
            to_tag: None,
            remote_target: None,
            route_set: Vec::new(),
            cseq: Arc::new(AtomicU32::new(0)),
            ended: true,
            next: None,
            fail_count: 0,
            notify_since_send: false,
            stable_since: None,
            remote_cseq: None,
            tracker: BlfTracker::default(),
            last_blf: None,
            blf_updated: Utc::now(),
        }));
        let handle = Arc::new(SubHandle {
            id: id.clone(),
            key,
            shared,
            tx,
        });
        // The environment is read under the same lock that publishes the handle, so a
        // concurrent `set_env` either is seen here or sends its command to the new handle.
        let (stagger, env) = {
            let mut g = lock(&self.inner);
            g.subs.insert(id.clone(), handle.clone());
            g.stagger = (g.stagger + 1) % 16;
            (g.stagger, g.env.clone())
        };
        let task = SubTask {
            mgr: self.clone(),
            handle,
            env,
            env_key: None,
            stagger: Duration::from_millis(u64::from(stagger) * 25),
        };
        tokio::spawn(task.run(rx));
        id
    }

    fn spawn_release(self: &Arc<Self>, h: Arc<SubHandle>) {
        tokio::spawn(async move {
            let (tx, rx) = oneshot::channel();
            if h.tx
                .send(SubCmd::Release {
                    exit: true,
                    reply: tx,
                })
                .is_ok()
            {
                let _ = tokio::time::timeout(RELEASE_ALL_TIMEOUT, rx).await;
            }
        });
    }

    /// Creates a generic subscription.
    pub(crate) fn subscribe(
        self: &Arc<Self>,
        uri: &str,
        event: &str,
        accept: Option<&str>,
        expires: Option<u32>,
        aor: &Uri,
    ) -> Result<String> {
        let event = event.trim();
        if event.is_empty() || event.contains(char::is_whitespace) {
            return Err(SipError::InvalidArgument(format!(
                "bad event package {event:?}"
            )));
        }
        let target = resolve_target(uri, aor)?;
        let accept = accept
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map_or_else(|| default_accept(event).to_string(), str::to_string);
        Ok(self.create(
            String::new(),
            SubKind::Generic,
            uri.to_string(),
            String::new(),
            event.to_string(),
            accept,
            expires.or(Some(GENERIC_EXPIRES)),
            Meta::default(),
            target,
        ))
    }

    /// Unsubscribes (Expires: 0, bounded) and forgets a generic subscription.
    pub(crate) async fn unsubscribe(&self, sub_id: &str) -> Result<()> {
        let h = {
            let g = lock(&self.inner);
            let h = g
                .subs
                .get(sub_id)
                .cloned()
                .ok_or_else(|| SipError::UnknownSubscription(sub_id.to_string()))?;
            if lock(&h.shared).info.kind != SubKind::Generic {
                return Err(SipError::InvalidArgument(format!(
                    "{sub_id} is managed by the account config (blf / mwi)"
                )));
            }
            h
        };
        lock(&self.inner).subs.remove(sub_id);
        let (tx, rx) = oneshot::channel();
        if h.tx
            .send(SubCmd::Release {
                exit: true,
                reply: tx,
            })
            .is_ok()
        {
            let _ = tokio::time::timeout(RELEASE_ALL_TIMEOUT, rx).await;
        }
        Ok(())
    }

    // --- environment -----------------------------------------------------------------------

    /// New (or no) runtime environment: every subscription is told; a different key restarts
    /// them with new dialogs.
    pub(crate) fn set_env(&self, env: Option<Arc<SubEnv>>) {
        let handles: Vec<_> = {
            let mut g = lock(&self.inner);
            g.env.clone_from(&env);
            g.subs.values().cloned().collect()
        };
        for h in handles {
            let _ = h.tx.send(SubCmd::SetEnv(env.clone()));
        }
    }

    /// Retries subscriptions that failed for good (credentials may have changed).
    pub(crate) fn kick_failed(&self) {
        let handles: Vec<_> = lock(&self.inner).subs.values().cloned().collect();
        for h in handles {
            let _ = h.tx.send(SubCmd::Kick);
        }
    }

    /// Sends `Expires: 0` for every established subscription (bounded overall). With `exit` the
    /// tasks end and the subscriptions are forgotten (account shutdown / removal); otherwise
    /// they stay configured and restart with the next environment (transport rebuild).
    pub(crate) async fn release_all(&self, exit: bool) {
        let handles: Vec<_> = {
            let mut g = lock(&self.inner);
            let hs: Vec<_> = g.subs.values().cloned().collect();
            if exit {
                g.subs.clear();
            }
            hs
        };
        let mut waits = Vec::new();
        for h in handles {
            let (tx, rx) = oneshot::channel();
            if h.tx.send(SubCmd::Release { exit, reply: tx }).is_ok() {
                waits.push(rx);
            }
        }
        let _ = tokio::time::timeout(RELEASE_ALL_TIMEOUT, async {
            for w in waits {
                let _ = w.await;
            }
        })
        .await;
    }

    // --- NOTIFY ----------------------------------------------------------------------------

    fn forget_call(g: &mut MgrInner, call_id: &str) {
        g.by_call.remove(call_id);
        let now = Instant::now();
        let ttl = DEAD_MIN_TTL.max(Duration::from_secs(u64::from(g.blf_expires)));
        while g
            .dead
            .front()
            .is_some_and(|(_, at)| now.duration_since(*at) > ttl)
        {
            g.dead.pop_front();
        }
        if !g.dead.iter().any(|(c, _)| c == call_id) {
            g.dead.push_back((call_id.to_string(), now));
            if g.dead.len() > RECENT_DEAD {
                g.dead.pop_front();
            }
        }
    }

    /// Matches an incoming NOTIFY against our subscriptions (Call-ID + To tag == our From tag +
    /// Event) and applies it. Synchronous so ordering follows arrival order.
    pub(crate) fn on_notify(&self, req: &Request) -> NotifyOutcome {
        let call_id = req
            .call_id_header()
            .map(|c| c.value().to_string())
            .unwrap_or_default();
        let handle = {
            let g = lock(&self.inner);
            match g.by_call.get(&call_id).and_then(|id| g.subs.get(id)) {
                Some(h) => h.clone(),
                None if g.dead.iter().any(|(c, at)| {
                    *c == call_id
                        && at.elapsed()
                            <= DEAD_MIN_TTL.max(Duration::from_secs(u64::from(g.blf_expires)))
                }) =>
                {
                    // The final NOTIFY after our own unsubscribe is legitimate.
                    let fin = header_value(&req.headers, "Subscription-State")
                        .is_some_and(|v| parse_subscription_state(&v).state == "terminated");
                    return if fin {
                        NotifyOutcome::Accepted
                    } else {
                        NotifyOutcome::NoSubscription
                    };
                }
                None => return NotifyOutcome::NotOurs,
            }
        };
        let to_tag = tag_of(req.to_header().ok());
        let from_tag = req
            .from_header()
            .ok()
            .and_then(|f| f.tag().ok().flatten())
            .map(|t| t.to_string());
        let event_hdr = header_value(&req.headers, "Event").unwrap_or_default();
        let ev_key = event_key(&event_hdr);

        let mut s = lock(&handle.shared);
        if s.ended || s.call_id != call_id || to_tag.as_deref() != Some(s.from_tag.as_str()) {
            return NotifyOutcome::NoSubscription;
        }
        if ev_key != event_key(&s.event) {
            return NotifyOutcome::NoSubscription;
        }
        // The remote tag: from the 2xx, or from this NOTIFY when it came first.
        match (&s.to_tag, &from_tag) {
            (None, Some(t)) => s.to_tag = Some(t.clone()),
            (Some(a), Some(b)) if a != b => {
                tracing::debug!(account = %self.account, sub = %s.info.id, "NOTIFY from another dialog (fork); refused");
                return NotifyOutcome::NoSubscription;
            }
            _ => {}
        }
        // Out-of-order NOTIFYs (reordered on the wire) are acknowledged but not applied.
        let cseq = req.cseq_header().ok().and_then(|c| c.seq().ok());
        if let Some(n) = cseq {
            if s.remote_cseq.is_some_and(|last| n <= last) {
                return NotifyOutcome::Accepted;
            }
            s.remote_cseq = Some(n);
        }
        if let Some(u) = header_values(&req.headers, "Contact")
            .first()
            .and_then(|c| uri_of_name_addr(c))
        {
            s.remote_target = Some(u);
        }
        if s.route_set.is_empty() {
            // UAS-style route set: Record-Route in the order received.
            s.route_set = header_values(&req.headers, "Record-Route")
                .iter()
                .flat_map(|v| split_list(v))
                .collect();
        }
        s.info.last_notify_at = Some(Utc::now());

        let sub_state = header_value(&req.headers, "Subscription-State")
            .map(|v| parse_subscription_state(&v))
            .unwrap_or_default();
        let raw_state = header_value(&req.headers, "Subscription-State").unwrap_or_default();
        let content_type = header_value(&req.headers, "Content-Type").unwrap_or_default();
        let body = String::from_utf8_lossy(&req.body).into_owned();

        // Body first, so the state event that follows already carries the new picture.
        let mut mwi = None;
        match s.info.kind {
            SubKind::Blf => {
                if !body.trim().is_empty() {
                    match parse_dialog_info(&body) {
                        Some(doc) => {
                            s.tracker.apply(doc);
                        }
                        None => {
                            tracing::warn!(account = %self.account, sub = %s.info.id, "unparsable dialog-info body")
                        }
                    }
                }
            }
            SubKind::Mwi => {
                if !body.trim().is_empty() {
                    mwi = Some(status_from_body(&self.account, &body, Utc::now()));
                }
            }
            SubKind::Generic => {
                let _ = self.events.send(SipEvent::SubscriptionNotify {
                    account: self.account.clone(),
                    sub_id: s.info.id.clone(),
                    event: event_hdr.clone(),
                    content_type,
                    body,
                    subscription_state: raw_state,
                });
            }
        }

        let ev = &self.events;
        let mut forget = false;
        match sub_state.state.as_str() {
            "terminated" => {
                s.notify_since_send = true;
                s.ended = true;
                s.info.expires_at = None;
                // Only a subscription that stayed up for a while resets the back-off.
                if s.stable_since
                    .take()
                    .is_some_and(|t| t.elapsed() >= STABLE_AFTER)
                {
                    s.fail_count = 0;
                }
                let action = termination_action(
                    sub_state.reason.as_deref(),
                    sub_state.retry_after,
                    s.fail_count,
                );
                forget = true;
                match action {
                    TermAction::Stop => {
                        s.next = None;
                        s.retrying = false;
                        let reason = format!(
                            "subscription terminated: {}",
                            sub_state.reason.as_deref().unwrap_or("no reason")
                        );
                        s.set_state(SubState::Failed { code: 0, reason }, ev);
                    }
                    TermAction::Resubscribe(d) => {
                        s.last_error = Some((
                            0,
                            format!(
                                "subscription terminated: {}",
                                sub_state.reason.as_deref().unwrap_or("no reason")
                            ),
                        ));
                        s.fail_count += 1;
                        s.schedule_retry(d, Action::NewDialog);
                        s.set_state(SubState::Terminated, ev);
                    }
                }
            }
            other => {
                if s.stable_since.is_none() {
                    s.stable_since = Some(Instant::now());
                }
                let granted = sub_state.expires;
                if let Some(e) = granted {
                    s.notify_since_send = true;
                    s.set_expires(e);
                    s.next = (e > 0).then(|| (Instant::now() + refresh_delay(e), Action::Refresh));
                }
                let new = if other == "pending" {
                    SubState::Pending
                } else {
                    SubState::Active
                };
                s.set_state(new, ev);
            }
        }
        drop(s);
        if forget {
            let mut g = lock(&self.inner);
            Self::forget_call(&mut g, &call_id);
        }
        let _ = handle.tx.send(SubCmd::Poke);
        if let Some(m) = mwi {
            self.set_mwi(m);
        }
        NotifyOutcome::Accepted
    }
}

// --- the per-subscription task ------------------------------------------------------------------

struct SubTask {
    mgr: Arc<SubManager>,
    handle: Arc<SubHandle>,
    env: Option<Arc<SubEnv>>,
    env_key: Option<String>,
    stagger: Duration,
}

struct SubResp {
    code: u16,
    reason: String,
    to_tag: Option<String>,
    contact: Option<Uri>,
    record_route: Vec<String>,
    expires: Option<u32>,
    retry_after: Option<u32>,
    min_expires: Option<u32>,
}

async fn sleep_until_opt(t: Option<Instant>) {
    match t {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

impl SubTask {
    fn sh(&self) -> MutexGuard<'_, SubShared> {
        lock(&self.handle.shared)
    }

    async fn run(mut self, mut rx: UnboundedReceiver<SubCmd>) {
        // Start with whatever environment existed at creation.
        if let Some(e) = self.env.take() {
            self.apply_env(Some(e));
        }
        loop {
            let deadline = if self.env.is_some() {
                self.sh().next.map(|(t, _)| t)
            } else {
                None
            };
            tokio::select! {
                cmd = rx.recv() => {
                    let Some(cmd) = cmd else { break };
                    match cmd {
                        SubCmd::Poke => {}
                        SubCmd::SetEnv(e) => self.apply_env(e),
                        SubCmd::Restart => {
                            if self.env.is_some() {
                                self.sh().next = Some((Instant::now(), Action::NewDialog));
                            }
                        }
                        SubCmd::Kick => {
                            let failed = matches!(self.sh().info.state, SubState::Failed { .. })
                                && self.sh().next.is_none();
                            if failed && self.env.is_some() {
                                self.sh().next = Some((Instant::now(), Action::NewDialog));
                            }
                        }
                        SubCmd::Release { exit, reply } => {
                            self.release(exit).await;
                            let _ = reply.send(());
                            if exit {
                                break;
                            }
                        }
                    }
                }
                () = sleep_until_opt(deadline) => {
                    let act = self.sh().next.take().map(|(_, a)| a);
                    if let Some(a) = act {
                        self.cycle(a).await;
                    }
                }
            }
        }
        // Task over: make sure NOTIFYs for it stop matching.
        let call_id = {
            let mut s = self.sh();
            s.ended = true;
            s.call_id.clone()
        };
        if !call_id.is_empty() {
            let mut g = lock(&self.mgr.inner);
            SubManager::forget_call(&mut g, &call_id);
            g.subs.remove(&self.handle.id);
        }
    }

    fn apply_env(&mut self, env: Option<Arc<SubEnv>>) {
        match env {
            None => {
                self.env = None;
                self.env_key = None;
                self.end_dialog(SubState::Pending);
                self.sh().next = None;
            }
            Some(e) => {
                let changed = self.env_key.as_deref() != Some(e.key.as_str());
                self.env_key = Some(e.key.clone());
                self.env = Some(e);
                if changed {
                    self.end_dialog(SubState::Pending);
                    self.sh().next = Some((Instant::now() + self.stagger, Action::NewDialog));
                }
            }
        }
    }

    /// Forgets the current dialog (no signalling) and moves to `state`.
    fn end_dialog(&mut self, state: SubState) {
        let call_id = {
            let mut s = self.sh();
            s.ended = true;
            s.to_tag = None;
            s.remote_target = None;
            s.route_set.clear();
            s.fail_count = 0;
            s.stable_since = None;
            s.tracker = BlfTracker::default();
            s.info.expires_at = None;
            s.retrying = false;
            let ev = self.mgr.events.clone();
            s.set_state(state, &ev);
            s.call_id.clone()
        };
        if !call_id.is_empty() {
            let mut g = lock(&self.mgr.inner);
            SubManager::forget_call(&mut g, &call_id);
        }
    }

    /// New dialog identifiers, registered with the manager before anything is sent.
    fn new_dialog(&mut self) {
        let call_id = make_call_id(Some(CALLID_SUFFIX), Default::default())
            .value()
            .to_string();
        let old = {
            let mut s = self.sh();
            let old = std::mem::replace(&mut s.call_id, call_id.clone());
            s.from_tag = make_tag().to_string();
            s.to_tag = None;
            s.remote_target = None;
            s.route_set.clear();
            s.cseq.store(0, Ordering::SeqCst);
            s.ended = false;
            s.stable_since = None;
            s.remote_cseq = None;
            s.tracker.new_dialog();
            old
        };
        let mut g = lock(&self.mgr.inner);
        if !old.is_empty() {
            SubManager::forget_call(&mut g, &old);
        }
        g.by_call.insert(call_id, self.handle.id.clone());
    }

    fn requested_expires(&self) -> u32 {
        let blf_expires = lock(&self.mgr.inner).blf_expires;
        let s = self.sh();
        let base = match s.info.kind {
            SubKind::Blf => blf_expires,
            SubKind::Mwi => MWI_EXPIRES,
            SubKind::Generic => s.expires_req.unwrap_or(GENERIC_EXPIRES),
        };
        s.expires_override.map_or(base, |o| o.max(base))
    }

    /// One SUBSCRIBE exchange (new dialog or in-dialog refresh) and its consequences.
    async fn cycle(&mut self, action: Action) {
        let Some(env) = self.env.clone() else { return };
        let mut new_dialog = action == Action::NewDialog || {
            let s = self.sh();
            s.ended || s.to_tag.is_none()
        };
        let mut retries_423 = 0;
        loop {
            if new_dialog {
                self.new_dialog();
            }
            let expires = self.requested_expires();
            let res = self.exchange(&env, expires).await;
            let resp = match res {
                Ok(r) => r,
                Err(e) => {
                    let (code, reason) = match &e {
                        SipError::Timeout => (408, "Request Timeout (no response)".to_string()),
                        other => (0, other.to_string()),
                    };
                    self.on_failure(code, reason, None);
                    return;
                }
            };
            match resp.code {
                200..=299 => {
                    self.on_success(&resp, expires, !new_dialog);
                    return;
                }
                481 if !new_dialog => {
                    tracing::info!(account = %self.mgr.account, sub = %self.handle.id, "481 on refresh; subscribing anew");
                    new_dialog = true;
                    continue;
                }
                423 if retries_423 < 2 => {
                    retries_423 += 1;
                    if let Some(m) = resp.min_expires {
                        self.sh().expires_override = Some(m);
                    }
                    continue;
                }
                _ => {
                    self.on_failure(resp.code, resp.reason.clone(), resp.retry_after);
                    return;
                }
            }
        }
    }

    fn on_success(&mut self, resp: &SubResp, sent_expires: u32, refresh: bool) {
        let ev = self.mgr.events.clone();
        let mut s = self.sh();
        if s.ended {
            // A terminating NOTIFY beat the 2xx: do not resurrect the dialog.
            return;
        }
        s.retrying = false;
        if refresh {
            // The subscription survived a refresh: it is healthy again.
            s.fail_count = 0;
        }
        if s.to_tag.is_none() {
            s.to_tag.clone_from(&resp.to_tag);
        }
        if let Some(c) = &resp.contact {
            s.remote_target = Some(c.clone());
        }
        if s.route_set.is_empty() && !resp.record_route.is_empty() {
            // UAC route set: Record-Route of the 2xx in reverse order.
            s.route_set = resp.record_route.iter().rev().cloned().collect();
        }
        if sent_expires == 0 {
            s.ended = true;
            s.next = None;
            s.info.expires_at = None;
            let call_id = s.call_id.clone();
            s.set_state(SubState::Terminated, &ev);
            drop(s);
            let mut g = lock(&self.mgr.inner);
            SubManager::forget_call(&mut g, &call_id);
            return;
        }
        let granted = resp.expires.unwrap_or(sent_expires);
        if !s.notify_since_send {
            s.set_expires(granted);
            s.next =
                (granted > 0).then(|| (Instant::now() + refresh_delay(granted), Action::Refresh));
        }
        if matches!(s.info.state, SubState::Failed { .. } | SubState::Terminated) {
            s.set_state(SubState::Pending, &ev);
        }
    }

    fn on_failure(&mut self, code: u16, reason: String, retry_after: Option<u32>) {
        let ev = self.mgr.events.clone();
        let mut s = self.sh();
        tracing::warn!(account = %self.mgr.account, sub = %s.info.id, uri = %s.info.uri, code, %reason, "SUBSCRIBE failed");
        let established = !s.ended && s.to_tag.is_some();
        if code == 0 || retryable(code) {
            s.fail_count += 1;
            let d = retry_delay(retry_after, s.fail_count);
            s.stable_since = None;
            let act = if established {
                Action::Refresh
            } else {
                Action::NewDialog
            };
            s.schedule_retry(d, act);
        } else {
            s.next = None;
            s.retrying = false;
            s.ended = true;
            let call_id = s.call_id.clone();
            drop(s);
            {
                let mut g = lock(&self.mgr.inner);
                SubManager::forget_call(&mut g, &call_id);
            }
            s = self.sh();
        }
        s.set_state(SubState::Failed { code, reason }, &ev);
    }

    async fn dest_for(&self, env: &SubEnv, target: &Uri) -> Result<SipAddr> {
        let c = &env.client;
        let same = |u: &Uri| u.host_with_port == target.host_with_port;
        if (same(&c.aor) || c.registrar_uri.as_ref().is_some_and(same))
            && let Some(t) = &c.target
        {
            return Ok(t.clone());
        }
        if let Some(t) = &c.target
            && c.route.is_some()
        {
            return Ok(t.clone());
        }
        let (host, port, transport) = uri_destination(target, env.transport);
        let addrs = dns::resolve(env.dns.as_ref(), &host, port, transport).await?;
        Ok(SipAddr::new(sip_transport(transport), addrs[0].into()))
    }

    async fn exchange(&mut self, env: &Arc<SubEnv>, expires: u32) -> Result<SubResp> {
        struct Snap {
            call_id: String,
            from_tag: String,
            to_tag: Option<String>,
            target: Uri,
            remote_target: Option<Uri>,
            route_set: Vec<String>,
            cseq: Arc<AtomicU32>,
            event: String,
            accept: String,
        }
        let snap = {
            let mut s = self.sh();
            s.notify_since_send = false;
            Snap {
                call_id: s.call_id.clone(),
                from_tag: s.from_tag.clone(),
                to_tag: s.to_tag.clone(),
                target: s.target.clone(),
                remote_target: s.remote_target.clone(),
                route_set: s.route_set.clone(),
                cseq: s.cseq.clone(),
                event: s.event.clone(),
                accept: s.info.accept.clone(),
            }
        };
        let mut client = env.client.clone();
        client.cred = self.mgr.calls.credential();
        let dest = self.dest_for(env, &snap.target).await?;
        let (_conn, adv) = client.flow(&dest).await?;
        let contact = env.contact.clone().unwrap_or_else(|| {
            build_contact(
                &client.user,
                adv.addr.clone(),
                client.transport,
                client.use_sips,
            )
        });
        let via = client.endpoint.get_via(Some(adv), None)?;
        let from = sip::typed::From {
            display_name: (!client.display_name.is_empty()).then(|| client.display_name.clone()),
            uri: client.aor.clone(),
            params: vec![],
        }
        .with_tag(snap.from_tag.clone().into());
        let mut to = sip::typed::To {
            display_name: None,
            uri: snap.target.clone(),
            params: vec![],
        };
        if let Some(t) = &snap.to_tag {
            to.params.push(Param::Tag(t.clone().into()));
        }
        let seq = snap.cseq.fetch_add(1, Ordering::SeqCst) + 1;
        let req_uri = snap
            .remote_target
            .clone()
            .unwrap_or_else(|| snap.target.clone());
        let mut req = client.endpoint.make_request(
            Method::Subscribe,
            req_uri,
            via,
            from,
            to,
            seq,
            Some(snap.call_id.clone().into()),
        );
        req.headers.unique_push(
            sip::typed::Contact {
                display_name: None,
                uri: contact,
                params: vec![],
            }
            .into(),
        );
        req.headers
            .unique_push(Header::Event(snap.event.clone().into()));
        req.headers
            .unique_push(Header::Accept(snap.accept.clone().into()));
        req.headers
            .unique_push(sip::headers::Expires::from(expires).into());
        client.decorate(&mut req);
        if !snap.route_set.is_empty() {
            req.headers.retain(|h| !matches!(h, Header::Route(_)));
            for r in &snap.route_set {
                req.headers.push(Header::Route(r.clone().into()));
            }
        }
        let resp = client.send_with_cseq(req, &dest, &snap.cseq).await?;
        let num = |name: &str| {
            header_value(&resp.headers, name)
                .and_then(|v| v.split(';').next().and_then(|x| x.trim().parse().ok()))
        };
        Ok(SubResp {
            code: resp.status_code.code(),
            reason: resp.status_code.text().to_string(),
            to_tag: tag_of(resp.to_header().ok()),
            contact: header_values(&resp.headers, "Contact")
                .first()
                .and_then(|c| uri_of_name_addr(c)),
            record_route: header_values(&resp.headers, "Record-Route")
                .iter()
                .flat_map(|v| split_list(v))
                .collect(),
            expires: num("Expires"),
            retry_after: num("Retry-After"),
            min_expires: num("Min-Expires"),
        })
    }

    /// `Expires: 0` for an established subscription (bounded), then forget the dialog.
    async fn release(&mut self, exit: bool) {
        let established = {
            let s = self.sh();
            !s.ended && s.to_tag.is_some() && !matches!(s.info.state, SubState::Terminated)
        };
        if established && let Some(env) = self.env.clone() {
            self.sh().next = None;
            let r = tokio::time::timeout(RELEASE_TIMEOUT, self.exchange(&env, 0)).await;
            match r {
                Ok(Ok(resp)) => {
                    tracing::debug!(account = %self.mgr.account, sub = %self.handle.id, code = resp.code, "unsubscribed")
                }
                Ok(Err(e)) => {
                    tracing::debug!(account = %self.mgr.account, sub = %self.handle.id, error = %e, "unsubscribe failed")
                }
                Err(_) => {
                    tracing::debug!(account = %self.mgr.account, sub = %self.handle.id, "unsubscribe timed out")
                }
            }
        }
        self.sh().next = None;
        self.env = None;
        self.env_key = None;
        self.end_dialog(if exit {
            SubState::Terminated
        } else {
            SubState::Pending
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscription_state_header() {
        let h = parse_subscription_state("active;expires=120");
        assert_eq!((h.state.as_str(), h.expires), ("active", Some(120)));
        let h = parse_subscription_state("Terminated ; Reason=Deactivated ; retry-after=7");
        assert_eq!(h.state, "terminated");
        assert_eq!(h.reason.as_deref(), Some("deactivated"));
        assert_eq!(h.retry_after, Some(7));
        let h = parse_subscription_state("pending");
        assert_eq!(
            (h.state.as_str(), h.expires, h.reason),
            ("pending", None, None)
        );
        let h = parse_subscription_state("active;expires=\"90\";foo");
        assert_eq!(h.expires, Some(90));
        assert_eq!(parse_subscription_state("").state, "");
    }

    #[test]
    fn termination_reasons() {
        use TermAction::*;
        let sec = Duration::from_secs;
        assert_eq!(termination_action(Some("rejected"), None, 0), Stop);
        assert_eq!(termination_action(Some("noresource"), Some(5), 0), Stop);
        assert_eq!(termination_action(Some("invariant"), Some(5), 0), Stop);
        assert_eq!(termination_action(Some("giveup"), None, 3), Stop);
        assert_eq!(
            termination_action(Some("giveup"), Some(30), 0),
            Resubscribe(sec(30))
        );
        assert_eq!(
            termination_action(Some("deactivated"), None, 0),
            Resubscribe(sec(1)),
            "never faster than 1 s"
        );
        assert_eq!(
            termination_action(Some("timeout"), None, 0),
            Resubscribe(sec(1))
        );
        assert_eq!(
            termination_action(Some("deactivated"), None, 2),
            Resubscribe(sec(4))
        );
        assert_eq!(
            termination_action(Some("deactivated"), Some(0), 3),
            Resubscribe(sec(8)),
            "retry-after 0 does not beat the back-off"
        );
        assert_eq!(
            termination_action(Some("probation"), Some(30), 0),
            Resubscribe(sec(30))
        );
        assert_eq!(
            termination_action(Some("probation"), Some(30), 6),
            Resubscribe(sec(60)),
            "the larger of retry-after and back-off"
        );
        assert_eq!(
            termination_action(Some("probation"), Some(9999), 0),
            Resubscribe(sec(300)),
            "capped"
        );
        assert_eq!(termination_action(None, None, 1), Resubscribe(sec(2)));
        assert_eq!(
            termination_action(Some("whatever"), None, 9),
            Resubscribe(sec(60))
        );
        assert_eq!(
            termination_action(Some("DEACTIVATED"), Some(3), 0),
            Resubscribe(sec(3))
        );
    }

    #[test]
    fn event_keys() {
        assert_eq!(
            event_key("Presence ; ID=5"),
            ("presence".into(), Some("5".into()))
        );
        assert_eq!(event_key("dialog"), ("dialog".into(), None));
        assert_ne!(event_key("presence;id=5"), event_key("presence"));
    }

    #[test]
    fn backoff_and_refresh_timing() {
        let s = Duration::from_secs;
        assert_eq!(
            [
                backoff(0),
                backoff(1),
                backoff(2),
                backoff(5),
                backoff(6),
                backoff(30)
            ],
            [s(1), s(2), s(4), s(32), s(60), s(60)]
        );
        assert_eq!(refresh_delay(600), s(510));
        assert_eq!(refresh_delay(120), s(102));
        assert_eq!(refresh_delay(1), s(1), "never below one second");
        assert_eq!(refresh_delay(100), Duration::from_secs_f64(85.0));
    }

    #[test]
    fn retry_classes() {
        for c in [408, 480, 486, 491, 500, 503, 504] {
            assert!(retryable(c), "{c}");
        }
        for c in [400, 401, 403, 404, 405, 407, 489, 603] {
            assert!(!retryable(c), "{c}");
        }
    }

    #[test]
    fn target_resolution() {
        let aor = Uri::try_from("sip:1017@pbx.example:5060").unwrap();
        assert_eq!(
            resolve_target("1018", &aor).unwrap().to_string(),
            "sip:1018@pbx.example:5060"
        );
        assert_eq!(
            resolve_target("*97", &aor).unwrap().to_string(),
            "sip:*97@pbx.example:5060"
        );
        assert_eq!(
            resolve_target("1018@other.host", &aor).unwrap().to_string(),
            "sip:1018@other.host"
        );
        assert_eq!(
            resolve_target("sip:1018@x.y:5070", &aor)
                .unwrap()
                .to_string(),
            "sip:1018@x.y:5070"
        );
        assert!(resolve_target("  ", &aor).is_err());
    }

    #[test]
    fn list_splitting_and_uris() {
        assert_eq!(
            split_list("<sip:a@b;lr>, <sip:c@d,e;lr>,<sip:f@g>"),
            ["<sip:a@b;lr>", "<sip:c@d,e;lr>", "<sip:f@g>"]
        );
        assert_eq!(
            uri_of_name_addr("\"X\" <sip:1018@h:5060;transport=udp>;expires=5")
                .unwrap()
                .to_string()
                .to_ascii_lowercase(),
            "sip:1018@h:5060;transport=udp"
        );
        assert_eq!(
            uri_of_name_addr("sip:1018@h;q=1").unwrap().to_string(),
            "sip:1018@h"
        );
        assert!(uri_of_name_addr("garbage <>").is_none());
    }

    #[test]
    fn default_accepts() {
        assert_eq!(default_accept("dialog"), "application/dialog-info+xml");
        assert_eq!(
            default_accept("message-summary;id=1"),
            "application/simple-message-summary"
        );
        assert_eq!(default_accept("presence"), "application/pidf+xml");
        assert_eq!(default_accept("x-custom"), "*/*");
    }
}

//! Calls: call table, per-call state, event emission and the public
//! operations behind the `SipStack` call API. Inbound INVITEs are in `inbound`, outbound ones in
//! `outbound`, in-dialog requests and dialog state handling in `indialog`.

mod handler;
mod inbound;
mod indialog;
mod model;
mod network;
mod outbound;
mod reliable;
mod session;
mod timer;
mod transfer;
pub(crate) mod util;

use std::collections::VecDeque;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::Duration;

use chrono::Utc;
use rsipstack::dialog::authenticate::Credential;
use rsipstack::dialog::dialog::{DialogState, DialogStateSender};
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::dialog::invite_dialog::InviteDialog;
use rsipstack::sip::{Header, Host, Request, Transport as SipTransport, Uri};
use rsipstack::transaction::endpoint::EndpointInner;
use rsipstack::transport::SipAddr;
use siple_config::{
    AccountConfig, AudioSink, AudioSource, DtmfMode, HoldScope, HoldStyle, MediaConfig, SrtpPolicy,
};
use siple_media::sdp::SessionDescription;
use siple_media::sdp_neg::{LocalHold, MediaSession, Negotiated};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::{AbortHandle, JoinHandle};

use crate::account::Cmd;
use crate::dns::SystemSrvSource;
use crate::error::{Result, SipError};
use crate::media::{CallMedia, CallMediaRequest, MediaProvider, NullMediaProvider};
use crate::types::SipEvent;

pub(crate) use handler::CallsHandler;

/// Test hook: overrides the 32 s wait for `refer` NOTIFYs. Process-wide.
#[doc(hidden)]
pub fn set_transfer_timeout_ms(ms: u64) {
    transfer::TRANSFER_TIMEOUT_MS.store(ms, Ordering::Relaxed);
}

/// Test hook: overrides how long `AnswerMode::EndOthers` waits for the ACK (default 32 s, the
/// INVITE server transaction's Timer H). Process-wide.
#[doc(hidden)]
pub fn set_end_others_ack_wait_ms(ms: u64) {
    END_OTHERS_ACK_WAIT_MS.store(ms, Ordering::Relaxed);
}

static END_OTHERS_ACK_WAIT_MS: AtomicU64 = AtomicU64::new(32_000);

/// Test hook: delays every 3xx redirect hop by `ms` (emulates a slow DNS lookup). Process-wide.
#[doc(hidden)]
pub fn set_redirect_hop_delay_ms(ms: u64) {
    outbound::NEXT_HOP_DELAY_MS.store(ms, Ordering::Relaxed);
}
/// Test hook: overrides the 10 s wait for the answer to the re-INVITE that moves a call onto a new
/// network path (0 = default). Process-wide.
#[doc(hidden)]
pub fn set_move_attempt_ms(ms: u64) {
    network::MOVE_ATTEMPT_MS.store(ms, Ordering::Relaxed);
}
pub use model::{
    AnswerMode, AnswerOptions, AnswerOutcome, CallDirection, CallEnd, CallId, CallInfo, CallState,
    DialOptions, EndCause, EndedBy, HoldFlags, NegotiatedSummary, PendingOp, TransferRef,
    TransferRole,
};
#[doc(hidden)]
pub use reliable::set_prack_t1_ms;
pub use session::{
    CompleteMode, TransferError, TransferKind, TransferNotify, TransferPhase, TransferSession,
};

/// Cause of an INVITE that ended with a final non-2xx `code` (see [`EndCause::Rejected`]).
pub(crate) fn cause_for_status(code: u16) -> EndCause {
    match code {
        487 => EndCause::Cancelled,
        408 | 0 | 500..=599 => EndCause::Failed,
        400..=499 | 600..=699 => EndCause::Rejected,
        _ => EndCause::Failed,
    }
}

/// Process-wide call counter (ids are unique across accounts).
static NEXT_CALL: AtomicU64 = AtomicU64::new(1);

/// Ended calls kept for `calls()` / `call()`.
const KEEP_ENDED: usize = 64;

/// Everything of a running transport generation that calls need.
pub(crate) struct RtEnv {
    pub endpoint: Arc<EndpointInner>,
    pub dialogs: Arc<DialogLayer>,
    pub state_tx: DialogStateSender,
    pub contact: Arc<Mutex<Option<Uri>>>,
    /// Address RTP sockets bind to.
    pub bind_ip: IpAddr,
    /// Address written into SDP.
    pub advertise_ip: IpAddr,
    /// Registrar / outbound proxy next hop (resolved).
    pub target: Option<SipAddr>,
    pub transport: SipTransport,
    pub use_sips: bool,
    pub public_host: Option<Host>,
    /// Bumped whenever the account retires its stream connections (a calls' flows die with
    /// them): a call on an older value has no connection any more, even on the same address.
    pub stream_gen: u64,
    /// Address of the SIP listener of this generation (`None` for client-only TCP / TLS). With
    /// the bind address it identifies the generation: a flap back to an earlier address can
    /// bring a different socket (another ephemeral port).
    pub listener: Option<std::net::SocketAddr>,
}

impl RtEnv {
    /// Same network generation (listener and addresses): a call on it needs no move.
    pub(crate) fn same_generation(&self, other: &RtEnv) -> bool {
        self.listener == other.listener
            && self.stream_gen == other.stream_gen
            && self.bind_ip == other.bind_ip
            && self.advertise_ip == other.advertise_ip
    }
}

/// Configuration + runtime snapshot published by the account actor.
#[derive(Clone)]
pub(crate) struct CallEnv {
    pub cfg: Arc<AccountConfig>,
    pub media: Arc<MediaConfig>,
    pub rt: Option<Arc<RtEnv>>,
    /// OSRTP-memory generation read together with the rest of this snapshot (in
    /// `publish_env`), so a call that uses this snapshot commits with the generation that
    /// matches its config. A later clear leaves it older, so its commits are rejected.
    pub srtp_gen: u64,
}

/// Upper bound of the OSRTP-refusal memory (destinations) per account.
const SRTP_MEMORY_CAP: usize = 64;

/// What a final 2xx of the retry INVITE means for the OSRTP refusal memory. Only a final 2xx
/// commits a decision: provisionals prove nothing (a late-negotiating B2BUA rings, then refuses
/// both offers), and a call canceled while ringing teaches nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum SrtpProbe {
    #[default]
    None,
    /// The plain retry after a refused OSRTP offer: remember the destination on its 2xx.
    Remember,
    /// The OSRTP retry after a refused plain offer: forget the destination on its 2xx.
    Forget,
}

/// The OSRTP refusal memory of an account plus its generation. The generation is bumped on
/// every clear, so a call that started (and captured the generation) before a config change or
/// a rebuild cannot commit a stale decision afterwards.
#[derive(Default)]
struct SrtpMemory {
    dests: Vec<String>,
    generation: u64,
}

/// Per-account shared cell between the actor (writer) and the call code (reader).
pub(crate) struct AccountCalls {
    pub account: String,
    /// Effective DND (config value or runtime override); written by the actor.
    pub dnd: AtomicBool,
    pub cmd: UnboundedSender<Cmd>,
    env: RwLock<Option<CallEnv>>,
    credential: RwLock<Credential>,
    /// Which digest challenges the credential may answer (shared with its `guard`).
    pub(crate) auth: Arc<crate::authz::AuthState>,
    /// Destinations (next hop `host:port[:transport]`) that refused an `srtp = optional` (OSRTP)
    /// offer with 488 / 415 / 606 and then answered the plain retry with a final 2xx: calls to
    /// them offer plain RTP/AVP first. Least recently remembered first, capped at
    /// [`SRTP_MEMORY_CAP`]. Cleared (generation bumped) when the account's SRTP / routing /
    /// media config changes, on a rebuild and on a re-probe rebuild.
    srtp_downgraded: Mutex<SrtpMemory>,
}

impl AccountCalls {
    pub(crate) fn new(
        account: String,
        cmd: UnboundedSender<Cmd>,
        dnd: bool,
        credential: Credential,
        auth: Arc<crate::authz::AuthState>,
    ) -> Arc<Self> {
        Arc::new(Self {
            account,
            dnd: AtomicBool::new(dnd),
            cmd,
            env: RwLock::new(None),
            credential: RwLock::new(credential),
            auth,
            srtp_downgraded: Mutex::new(SrtpMemory::default()),
        })
    }

    fn srtp_mem(&self) -> std::sync::MutexGuard<'_, SrtpMemory> {
        self.srtp_downgraded
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Remembered OSRTP-refusing destinations.
    pub(crate) fn srtp_downgraded(&self) -> Vec<String> {
        self.srtp_mem().dests.clone()
    }

    /// Current generation of the memory; a call captures it when it starts.
    pub(crate) fn srtp_generation(&self) -> u64 {
        self.srtp_mem().generation
    }

    /// Whether `dest` is remembered as refusing OSRTP offers.
    pub(crate) fn srtp_remembered(&self, dest: &str) -> bool {
        self.srtp_mem().dests.iter().any(|d| d == dest)
    }

    /// Remembers `dest` unless `generation` is stale; true when it was not remembered yet.
    pub(crate) fn remember_srtp_dest(&self, dest: &str, generation: u64) -> bool {
        let mut m = self.srtp_mem();
        if m.generation != generation {
            return false;
        }
        let had = m
            .dests
            .iter()
            .position(|d| d == dest)
            .map(|i| m.dests.remove(i));
        m.dests.push(dest.to_string());
        if m.dests.len() > SRTP_MEMORY_CAP {
            m.dests.remove(0);
        }
        had.is_none()
    }

    /// Forgets `dest` unless `generation` is stale; true when it was remembered.
    pub(crate) fn forget_srtp_dest(&self, dest: &str, generation: u64) -> bool {
        let mut m = self.srtp_mem();
        if m.generation != generation {
            return false;
        }
        let n = m.dests.len();
        m.dests.retain(|d| d != dest);
        m.dests.len() != n
    }

    /// Forgets every destination and bumps the generation; true when there was any.
    pub(crate) fn clear_srtp_downgraded(&self) -> bool {
        let mut m = self.srtp_mem();
        m.generation += 1;
        let had = !m.dests.is_empty();
        m.dests.clear();
        had
    }

    /// Digest credential, refreshed by config updates.
    pub(crate) fn credential(&self) -> Credential {
        self.credential
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn set_credential(&self, c: Credential) {
        *self
            .credential
            .write()
            .unwrap_or_else(PoisonError::into_inner) = c;
    }

    pub(crate) fn set_env(&self, env: Option<CallEnv>) {
        *self.env.write().unwrap_or_else(PoisonError::into_inner) = env;
    }

    pub(crate) fn env(&self) -> Option<CallEnv> {
        self.env
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Mutable part of a call. Locked briefly; never held across an `.await`.
pub(crate) struct CallInner {
    pub info: CallInfo,
    pub local_tag: String,
    /// Runtime of the transport generation the call runs on. Dropped when the call ends so an
    /// ended call (kept for `calls()`) never pins sockets of a removed / rebuilt account.
    pub rt: Option<Arc<RtEnv>>,
    pub dialog: Option<InviteDialog>,
    /// Our dialog Contact (for responses sent straight on a transaction).
    pub local_contact: Option<Uri>,
    pub media: Option<Arc<dyn CallMedia>>,
    pub session: Option<MediaSession>,
    pub neg: Option<Negotiated>,
    pub source: AudioSource,
    pub sink: AudioSink,
    /// Remote offer of an inbound INVITE (kept to re-answer if the media is re-created).
    pub offer: Option<SessionDescription>,
    /// Answer SDP + negotiation prepared for the 200 (inbound call with an offer).
    pub answer: Option<(String, Negotiated)>,
    /// Offer-less inbound INVITE: our offer goes into the 200, the answer arrives in the ACK.
    pub offerless: bool,
    /// The SDP answer is expected in the next ACK (offer-less INVITE / re-INVITE).
    pub awaiting_ack_sdp: bool,
    /// CSeq of the INVITE whose ACK carries that answer.
    pub awaiting_ack_cseq: Option<u32>,
    /// Last SDP applied from a provisional response.
    pub early_sdp: Option<String>,
    pub local_hangup: bool,
    /// Outbound INVITE still in flight after a local hang-up: the dialog the background task
    /// (CANCEL, or ACK + BYE for a late 2xx) works on. Cleared when the INVITE is over.
    pub late_dialog: Option<InviteDialog>,
    /// We have a re-INVITE / UPDATE of our own outstanding (glare -> 491).
    pub reinvite_out: bool,
    /// Generation of the latest move onto a new network path (`calls/network.rs`): a move that
    /// is overtaken by a newer one does not end the call when it fails.
    pub move_gen: u64,
    /// The account moved its transport and this call's move is still to come (or running): a
    /// failing re-INVITE of another kind (hold, session refresh) must not end the dialog, the
    /// move decides.
    pub move_pending: bool,
    /// Rebind sequence of the account the pending move belongs to (see `mark_moving`): a move
    /// of an older rebind never clears the flag of a newer one.
    pub move_seq: u64,
    /// The Contact announced by the last move is provisional (no port; the re-REGISTER had not
    /// returned): refreshed when the registration tells the real one.
    pub fallback_contact: bool,
    /// Status for the end of a call whose answer SDP was refused.
    pub pending_negotiation_error: Option<u16>,
    pub bye_reason: Option<util::Reason>,
    pub invite_abort: Option<AbortHandle>,
    pub answer_timer: Option<AbortHandle>,
    /// Initial INVITE (inbound) for header lookups.
    pub invite: Option<Request>,
    /// SRTP policy of the outstanding outbound offer.
    pub srtp: SrtpPolicy,
    /// The plain-RTP retry after a refused OSRTP offer was used.
    pub srtp_retried: bool,
    /// The first offer was plain RTP/AVP only because the destination is remembered as
    /// refusing OSRTP.
    pub srtp_plain_first: bool,
    /// What a final 2xx of the retry INVITE means for the refusal memory.
    pub srtp_probe: SrtpProbe,
    /// Generation of the refusal memory when the call started; older commits are ignored.
    pub srtp_gen: u64,
    /// Memory key (next hop) of the current INVITE for `srtp = optional` outbound calls.
    pub srtp_key: Option<String>,
    /// A REFER of ours is outstanding on this call.
    pub transfer: Option<transfer::TransferOut>,
    /// Reliable provisional responses of an inbound call (RFC 3262).
    pub reliable: reliable::Reliable,
    /// Session interval and refresher negotiated for an inbound INVITE, installed with the 2xx.
    pub timer_plan: Option<(u32, util::Refresher)>,
    /// Running session timer (RFC 4028).
    pub timer: Option<timer::SessionTimer>,
    /// Cause a plain hang-up (either side's BYE) is reported with: set when the call is known to
    /// be handed over (final 2xx `refer` NOTIFY), before the BYE race.
    pub cause_hint: Option<EndCause>,
}

/// One call.
pub(crate) struct Call {
    pub id: CallId,
    pub account: String,
    pub direction: CallDirection,
    pub mgr: Arc<CallManager>,
    pub env: Arc<AccountCalls>,
    pub cfg: Arc<AccountConfig>,
    pub media_cfg: Arc<MediaConfig>,
    pub inner: Mutex<CallInner>,
    /// Becomes true when the call ends; lets waiters (hold re-INVITEs, coordinator steps) give
    /// up at once.
    pub ended: tokio::sync::watch::Sender<bool>,
}

struct Table {
    calls: Vec<Arc<Call>>,
    orphans: VecDeque<DialogState>,
}

/// The call table of a [`crate::SipStack`].
pub(crate) struct CallManager {
    pub events: UnboundedSender<SipEvent>,
    pub dns: Arc<SystemSrvSource>,
    provider: RwLock<Arc<dyn MediaProvider>>,
    /// `[calls] hold_scope`: which calls an auto-hold considers.
    hold_scope: Mutex<HoldScope>,
    /// The line coordinator: line operations (activate, answer, dial's auto-hold, hold / resume
    /// requested through the stack) hold this for their whole duration. tokio's mutex is fair,
    /// so waiting operations run in the order they were issued.
    pub(crate) lines: tokio::sync::Mutex<()>,
    /// The intended active line (see [`CallManager::active_line`]).
    active: Mutex<Option<CallId>>,
    table: Mutex<Table>,
    /// Transfer sessions (see `session`).
    sessions: Mutex<session::Sessions>,
    /// `[calls] resume_after_failed_transfer`.
    resume_failed: AtomicBool,
    /// Background BYE / CANCEL tasks of calls that already ended locally, by account.
    bg: Mutex<Vec<(String, JoinHandle<()>)>>,
}

/// How long a locally ended call's BYE may take.
const BYE_TIMEOUT: Duration = Duration::from_secs(6);
/// How long account removal / rebuild / shutdown wait for BYE / CANCEL before aborting them.
const HANGUP_DRAIN: Duration = Duration::from_secs(2);
/// How long an in-dialog request waits for the dialog to be confirmed (ACK).
const CONFIRM_WAIT: Duration = Duration::from_secs(4);
/// Bound for holding (or resuming) one other line inside a locked line operation. On a timeout
/// the operation aborts and releases the coordinator; the re-INVITE may finish in the background.
const LINE_HOLD_TIMEOUT: Duration = Duration::from_secs(8);
/// Message of the error a hold gets while another re-INVITE of the call is outstanding.
const REINVITE_BUSY: &str = "a re-INVITE is already in progress";

impl CallManager {
    pub(crate) fn new(events: UnboundedSender<SipEvent>, dns: Arc<SystemSrvSource>) -> Arc<Self> {
        Arc::new(Self {
            events,
            dns,
            provider: RwLock::new(NullMediaProvider::new()),
            hold_scope: Mutex::new(HoldScope::default()),
            lines: tokio::sync::Mutex::new(()),
            active: Mutex::new(None),
            table: Mutex::new(Table {
                calls: Vec::new(),
                orphans: VecDeque::new(),
            }),
            bg: Mutex::new(Vec::new()),
            sessions: Mutex::new(session::Sessions::default()),
            resume_failed: AtomicBool::new(true),
        })
    }

    pub(crate) fn set_provider(&self, p: Arc<dyn MediaProvider>) {
        *self
            .provider
            .write()
            .unwrap_or_else(PoisonError::into_inner) = p;
    }

    pub(crate) fn set_hold_scope(&self, scope: HoldScope) {
        *self
            .hold_scope
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = scope;
    }

    fn hold_scope(&self) -> HoldScope {
        *self
            .hold_scope
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn provider(&self) -> Arc<dyn MediaProvider> {
        self.provider
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn table(&self) -> MutexGuard<'_, Table> {
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn next_id() -> CallId {
        format!("c{}", NEXT_CALL.fetch_add(1, Ordering::Relaxed))
    }

    /// Runs `fut` (the network part of a hang-up) in the background; account removal, rebuild
    /// and shutdown wait for it ([`CallManager::drain_bg`]).
    pub(crate) fn spawn_bg(
        &self,
        account: &str,
        fut: impl std::future::Future<Output = ()> + Send + 'static,
    ) {
        let h = tokio::spawn(fut);
        let mut bg = self.bg.lock().unwrap_or_else(PoisonError::into_inner);
        bg.retain(|(_, h)| !h.is_finished());
        bg.push((account.to_string(), h));
    }

    /// Waits up to `wait` for the background hang-up tasks of `account` (all when `None`), then
    /// aborts what is left so no dialog (and no socket) outlives its runtime.
    pub(crate) async fn drain_bg(&self, account: Option<&str>, wait: Duration) {
        let taken: Vec<JoinHandle<()>> = {
            let mut bg = self.bg.lock().unwrap_or_else(PoisonError::into_inner);
            let (take, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut *bg)
                .into_iter()
                .partition(|(a, _)| account.is_none_or(|x| x == a));
            *bg = keep;
            take.into_iter().map(|(_, h)| h).collect()
        };
        let deadline = tokio::time::Instant::now() + wait;
        for mut h in taken {
            if tokio::time::timeout_at(deadline, &mut h).await.is_err() {
                h.abort();
            }
        }
    }

    /// Adds a call and replays dialog states that arrived before it existed.
    pub(crate) async fn register(self: &Arc<Self>, call: Arc<Call>) {
        let replay: Vec<DialogState> = {
            let mut t = self.table();
            t.calls.push(call.clone());
            let (call_id, tag) = call.dialog_key();
            let mut keep = VecDeque::new();
            let mut replay = Vec::new();
            for s in t.orphans.drain(..) {
                let id = s.id();
                if id.call_id == call_id && id.local_tag == tag {
                    replay.push(s);
                } else {
                    keep.push_back(s);
                }
            }
            t.orphans = keep;
            // Prune old ended calls.
            let ended: Vec<usize> = t
                .calls
                .iter()
                .enumerate()
                .filter(|(_, c)| c.is_ended())
                .map(|(i, _)| i)
                .collect();
            if ended.len() > KEEP_ENDED {
                let drop_n = ended.len() - KEEP_ENDED;
                let to_drop: Vec<usize> = ended.into_iter().take(drop_n).collect();
                let mut i = 0;
                t.calls.retain(|_| {
                    let keep = !to_drop.contains(&i);
                    i += 1;
                    keep
                });
            }
            replay
        };
        for s in replay {
            self.on_dialog_state_for(&call, s).await;
        }
    }

    /// Call by id.
    pub(crate) fn get(&self, id: &str) -> Result<Arc<Call>> {
        self.table()
            .calls
            .iter()
            .find(|c| c.id == id)
            .cloned()
            .ok_or_else(|| SipError::UnknownCall(id.to_string()))
    }

    /// Call owning the dialog `(sip call-id, our tag)`.
    pub(crate) fn find_dialog(&self, call_id: &str, local_tag: &str) -> Option<Arc<Call>> {
        self.table()
            .calls
            .iter()
            .find(|c| {
                let (cid, tag) = c.dialog_key();
                cid == call_id && tag == local_tag
            })
            .cloned()
    }

    /// Looks up the call of a dialog state or parks the state until the call is registered.
    pub(crate) fn call_for_state(&self, state: &DialogState) -> Option<Arc<Call>> {
        let id = state.id();
        let mut t = self.table();
        let found = t
            .calls
            .iter()
            .find(|c| {
                let (cid, tag) = c.dialog_key();
                cid == id.call_id && tag == id.local_tag
            })
            .cloned();
        if found.is_none()
            && matches!(
                state,
                DialogState::Calling(_)
                    | DialogState::Trying(_)
                    | DialogState::Early(..)
                    | DialogState::Confirmed(..)
                    | DialogState::Terminated(..)
            )
        {
            t.orphans.push_back(state.clone());
            while t.orphans.len() > 32 {
                t.orphans.pop_front();
            }
        }
        found
    }

    /// Snapshots of all calls (ended ones included), oldest first.
    pub(crate) fn infos(&self) -> Vec<CallInfo> {
        self.table().calls.iter().map(|c| c.snapshot()).collect()
    }

    /// Calls of `account` that are not over.
    pub(crate) fn live_count(&self, account: &str) -> usize {
        self.table()
            .calls
            .iter()
            .filter(|c| c.account == account && !c.is_ended())
            .count()
    }

    /// Tells the account actor how many calls are live (defers transport rebuilds). Counted and
    /// sent under the table lock so concurrent notifications arrive in order.
    pub(crate) fn notify_count(&self, env: &AccountCalls) {
        let t = self.table();
        let n = t
            .calls
            .iter()
            .filter(|c| c.account == env.account && !c.is_ended())
            .count();
        let _ = env.cmd.send(Cmd::CallsChanged(n));
    }

    /// Moves the established calls of `account` onto the transport generation `rt` (the account
    /// rebound its transport after a network change). Each move runs in the background; calls
    /// that are not established yet are left alone.
    pub(crate) fn move_live(self: &Arc<Self>, account: &str, rt: &Arc<RtEnv>, seq: u64) {
        for c in self.live_calls_of(account) {
            // A call set up after the rebind (or one the network flapped back under) already
            // runs on this generation; one whose move is in progress is always moved again.
            let on_new = !c.is_reconnecting() && c.rt().is_some_and(|r| r.same_generation(rt));
            if c.is_established() && !on_new {
                let rt = rt.clone();
                self.spawn_bg(account, async move { c.move_to(rt, seq, true).await });
            } else {
                let mut g = c.lock();
                if g.move_seq == seq {
                    g.move_pending = false;
                }
                let has_timer = g.timer.is_some();
                drop(g);
                if has_timer {
                    // A refresh skipped during the pending move: arm the timer again.
                    c.touch_timer();
                }
            }
        }
    }

    /// The re-REGISTER told the real Contact: calls that moved with a provisional one announce
    /// it now (a re-INVITE with the current SDP, no media rebind).
    pub(crate) fn refresh_fallback_contacts(self: &Arc<Self>, account: &str, rt: &Arc<RtEnv>) {
        for c in self.live_calls_of(account) {
            let due = {
                let g = c.lock();
                g.fallback_contact && !g.info.reconnecting
            };
            if due
                && c.is_established()
                && rt
                    .contact
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .is_some()
            {
                let rt = rt.clone();
                self.spawn_bg(account, async move { c.move_to(rt, 0, false).await });
            }
        }
    }

    /// The account rebound its transport: the established calls will be moved shortly. Until
    /// then (and while moving) a failing hold / refresh re-INVITE does not end the dialog.
    pub(crate) fn mark_moving(&self, account: &str, seq: u64) {
        for c in self.live_calls_of(account) {
            if c.is_established() {
                let mut g = c.lock();
                g.move_pending = true;
                g.move_seq = seq;
            }
        }
    }

    /// A live call of `account` still runs on the transport generation whose listener is
    /// `listener` (a call that was ringing when the account moved keeps its old Contact and media
    /// address).
    pub(crate) fn any_live_on(&self, account: &str, listener: std::net::SocketAddr) -> bool {
        self.live_calls_of(account)
            .iter()
            .any(|c| c.rt().is_some_and(|r| r.listener == Some(listener)))
    }

    /// `[network] move_calls = false` (or a failed rebind): ends the established calls of
    /// `account` with cause `network_lost`. Calls that are not established yet are left alone.
    pub(crate) fn end_live_network_lost(self: &Arc<Self>, account: &str, why: &str) {
        for c in self.live_calls_of(account) {
            if c.is_established() {
                c.end_network_lost(why);
            }
        }
    }

    fn live_calls_of(&self, account: &str) -> Vec<Arc<Call>> {
        self.table()
            .calls
            .iter()
            .filter(|c| !c.is_ended() && c.account == account)
            .cloned()
            .collect()
    }

    /// Hangs up the live calls of `account` (all accounts when `None`): every call ends locally
    /// at once; their BYE / CANCEL get a bounded moment to go out before the leftovers are
    /// aborted.
    pub(crate) async fn hangup_live(self: &Arc<Self>, account: Option<&str>) {
        let live: Vec<Arc<Call>> = self
            .table()
            .calls
            .iter()
            .filter(|c| !c.is_ended() && account.is_none_or(|a| c.account == a))
            .cloned()
            .collect();
        for c in live {
            if let Err(e) = c.hangup_now() {
                tracing::debug!(call = %c.id, error = %e, "hang-up failed; ending the call locally");
                c.local_end(0, "hang-up failed", EndCause::Hangup);
            }
            // Whatever the plan was, the call must not stay up.
            c.local_end(200, "BYE", EndCause::Hangup);
        }
        self.drain_bg(account, HANGUP_DRAIN).await;
    }

    /// Hangs up every live call and forgets all calls.
    pub(crate) async fn shutdown(self: &Arc<Self>) {
        self.hangup_live(None).await;
        let calls: Vec<Arc<Call>> = std::mem::take(&mut self.table().calls);
        for c in calls {
            c.end_quiet();
        }
    }

    pub(crate) fn emit(&self, ev: SipEvent) {
        let _ = self.events.send(ev);
    }

    /// The calls an auto-hold on behalf of `initiator` (an account id) puts on hold: every
    /// auto-holdable call (established, device-bound, not held by us; `RemoteHeld` counts) within
    /// `[calls] hold_scope`, except
    /// - `except` (the call being answered / activated) and its same-process peer leg,
    /// - one leg of every self-call (two calls of this process with the same SIP Call-ID): the
    ///   leg of `initiator` is held (of a self-call between two other accounts, the dialing
    ///   leg), the other leg then sits in `RemoteHeld` by itself, and
    /// - unless `explicit`, calls of accounts with `auto_hold = false`.
    ///
    /// Calls that use no audio device are never auto-held (`auto_holdable`).
    fn hold_targets(
        &self,
        initiator: &str,
        except: Option<&str>,
        explicit: bool,
    ) -> Vec<Arc<Call>> {
        let scope = self.hold_scope();
        let live: Vec<Arc<Call>> = self
            .table()
            .calls
            .iter()
            .filter(|c| !c.is_ended())
            .cloned()
            .collect();
        let mut cands: Vec<(Arc<Call>, CallInfo)> = live
            .iter()
            .filter(|c| Some(c.id.as_str()) != except && c.auto_holdable())
            // The call carrying an outstanding REFER is not touched.
            .filter(|c| !self.session_refer_call_busy(&c.id))
            .filter(|c| scope == HoldScope::All || c.account == initiator)
            .filter(|c| explicit || c.live_cfg().auto_hold)
            .map(|c| (c.clone(), c.snapshot()))
            .collect();
        // The initiator's own calls first, so it is its leg of a self-call that gets held. Of a
        // self-call between two other accounts the outbound (dialing) leg comes first: table
        // order would leave it to a race, since the dialler registers its call only after
        // sending the INVITE and the callee's leg may be in the table before it.
        cands.sort_by_key(|(c, _)| {
            (
                c.account != initiator,
                c.direction != CallDirection::Outbound,
            )
        });
        let except_cid = except
            .and_then(|e| live.iter().find(|c| c.id == e))
            .map(|c| c.snapshot().sip_call_id);
        let mut picked: Vec<(Arc<Call>, String)> = Vec::new();
        for (c, info) in cands {
            let cid = info.sip_call_id;
            if !cid.is_empty() {
                // Peer leg of the call being answered / activated, or of a call already
                // held (or about to be): leave this one alone.
                let peer_is_except = except_cid.as_deref() == Some(cid.as_str());
                let peer_held = live
                    .iter()
                    .filter(|d| d.id != c.id)
                    .map(|d| d.snapshot())
                    .any(|d| d.sip_call_id == cid && d.hold.local);
                let peer_picked = picked.iter().any(|(_, pc)| *pc == cid);
                if peer_is_except || peer_held || peer_picked {
                    continue;
                }
            }
            picked.push((c, cid));
        }
        picked.into_iter().map(|(c, _)| c).collect()
    }

    /// Holds one other line, bounded by [`LINE_HOLD_TIMEOUT`] in total (on a timeout the
    /// re-INVITE may still finish in the background). A line that ends meanwhile counts as held;
    /// "a re-INVITE is already in progress" (e.g. a session refresh) waits for that exchange and
    /// retries once when `wait_busy` (explicit operations); an automatic hold just fails fast, so
    /// one stuck line never delays an unrelated answer or dial.
    async fn hold_line(c: &Arc<Call>, held: bool, wait_busy: bool) -> Result<()> {
        if held && !wait_busy && c.is_reconnecting() {
            // A move onto a new network path is running: an automatic hold fails fast.
            return Err(c.state_err(REINVITE_BUSY));
        }
        let deadline = tokio::time::Instant::now() + LINE_HOLD_TIMEOUT;
        let mut retried = false;
        loop {
            match tokio::time::timeout_at(deadline, c.clone().hold(held)).await {
                Ok(Ok(())) => return Ok(()),
                Ok(Err(_)) if held && c.is_ended() => return Ok(()),
                Ok(Err(SipError::CallState { msg, .. }))
                    if msg == REINVITE_BUSY && wait_busy && !retried =>
                {
                    retried = true;
                    if tokio::time::timeout_at(deadline, c.wait_reinvite_idle())
                        .await
                        .is_err()
                    {
                        return Err(SipError::Timeout);
                    }
                }
                Ok(Err(e)) => return Err(e),
                Err(_) => return Err(SipError::Timeout),
            }
        }
    }

    /// Holds `targets` one by one (each at most [`LINE_HOLD_TIMEOUT`], see `hold_line`); the first failure stops
    /// the sequence and is returned. The caller holds the line coordinator.
    async fn hold_all(targets: Vec<Arc<Call>>, wait_busy: bool) -> Result<()> {
        for c in targets {
            tracing::info!(call = %c.id, "holding the other line's call");
            let _pending = c.set_pending(PendingOp::Hold);
            Self::hold_line(&c, true, wait_busy).await?;
        }
        Ok(())
    }

    /// `auto_hold`: puts the other device-bound calls within `hold_scope` on hold, awaiting each
    /// re-INVITE (a failure is logged and ignored). The caller holds the line coordinator and
    /// has checked that the new call uses an audio device.
    pub(crate) async fn auto_hold_others(self: &Arc<Self>, account: &str, except: Option<&str>) {
        for c in self.hold_targets(account, except, false) {
            if let Err(e) = Self::hold_all(vec![c.clone()], false).await {
                tracing::warn!(call = %c.id, error = %e, "auto_hold: could not hold the other call");
            }
        }
    }

    /// [`CallManager::auto_hold_others`] for an explicit request (`AnswerMode::HoldOthers`): the
    /// accounts' `auto_hold` flags are not consulted. A failure is logged and ignored.
    pub(crate) async fn hold_others_explicit(
        self: &Arc<Self>,
        account: &str,
        except: Option<&str>,
    ) {
        for c in self.hold_targets(account, except, true) {
            if let Err(e) = Self::hold_all(vec![c.clone()], false).await {
                tracing::warn!(call = %c.id, error = %e, "could not hold the other call");
            }
        }
    }

    /// Records `id` as the intended active line (set synchronously by activate, answer, resume
    /// and the outbound-answer rule; the media focus follows).
    pub(crate) fn set_active(&self, id: &str) {
        *self.active.lock().unwrap_or_else(PoisonError::into_inner) = Some(id.to_string());
    }

    /// The intended active line while it is still an un-held established device call.
    pub(crate) fn active_line(&self) -> Option<CallId> {
        let id = self
            .active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()?;
        if self.get(&id).is_ok_and(|c| c.auto_holdable()) {
            return Some(id);
        }
        // The recorded line is gone or held: another un-held device call may still own the
        // microphone (the engine re-picks the focus).
        let focus = self.provider().focus()?;
        self.get(&focus)
            .ok()
            .filter(|c| c.auto_holdable())
            .map(|_| focus)
    }

    /// A device-bound outbound call became active: the user dialed this party, so the other
    /// un-held device lines in scope go on hold (through the coordinator; same exemptions as
    /// auto-hold, honoring the account's `auto_hold`).
    pub(crate) async fn outbound_answered(self: &Arc<Self>, call: &Arc<Call>) {
        if !call.live_cfg().auto_hold {
            return;
        }
        let _line = self.lines.lock().await;
        // The wait for the coordinator may have been long: only an established, un-held device
        // call still gets to hold the others (a line the user resumed meanwhile is left alone,
        // and so is a call the user held).
        if !call.auto_holdable() {
            return;
        }
        self.auto_hold_others(&call.account, Some(&call.id)).await;
        self.set_active(&call.id);
    }

    /// Resume after a failed transfer: only when no other device line is live in scope (the
    /// user may have moved on to another call); otherwise the call stays held.
    pub(crate) async fn resume_if_free(self: &Arc<Self>, call: &Arc<Call>) {
        let device = call.device_bound();
        let _line = if device {
            Some(self.lines.lock().await)
        } else {
            None
        };
        if device
            && !self
                .hold_targets(&call.account, Some(&call.id), true)
                .is_empty()
        {
            tracing::debug!(call = %call.id, "another line is live: the original call stays held");
            return;
        }
        if let Err(e) = Self::hold_line(call, false, true).await {
            tracing::debug!(call = %call.id, error = %e, "cannot resume the original call");
        }
    }

    /// Makes `id` the only live device-bound line: the other un-held device calls within
    /// `hold_scope` are held (explicit request: `auto_hold` is ignored; non-device calls and the
    /// peer leg of a self-call are left alone), the target is resumed when we hold it, and the
    /// media focus moves to it. A failed hold aborts with its error before anything is resumed.
    /// Idempotent; serialized with the other multi-call line operations.
    pub(crate) async fn activate(self: &Arc<Self>, id: &str) -> Result<()> {
        let target = self.get(id)?;
        if let Some(s) = self.session_blocks_hold(id) {
            return Err(target.state_err(&format!(
                "transfer {} is {}: the call cannot be activated until it settles",
                s.id,
                s.phase.as_str()
            )));
        }
        let _line = self.lines.lock().await;
        // The wait for the coordinator may have been long: look again.
        if let Some(s) = self.session_blocks_hold(id) {
            return Err(target.state_err(&format!(
                "transfer {} is {}: the call cannot be activated until it settles",
                s.id,
                s.phase.as_str()
            )));
        }
        let locally_held = {
            let g = target.lock();
            if !matches!(
                g.info.state,
                CallState::Active | CallState::Held | CallState::RemoteHeld
            ) {
                return Err(target.state_err("call is not established"));
            }
            g.info.hold.local
        };
        if !target.device_bound() {
            return Err(target.state_err("call does not use an audio device"));
        }
        let others = self.hold_targets(&target.account, Some(id), true);
        if others.is_empty() && !locally_held && self.active_line().as_deref() == Some(id) {
            return Ok(());
        }
        let _pending = target.set_pending(PendingOp::Activate);
        Self::hold_all(others, true).await?;
        if locally_held {
            Self::hold_line(&target, false, true).await?;
        }
        if target.is_ended() {
            return Err(target.state_err("call is over"));
        }
        target.focus_media();
        Ok(())
    }

    // --- operations behind the SipStack API ----------------------------------------------

    /// Answers a ringing call; see [`AnswerMode`]. Only a device-bound call that holds other
    /// lines takes the line coordinator; calls without an audio device (agents, automation)
    /// neither hold anything nor wait for it.
    pub(crate) async fn answer(
        self: &Arc<Self>,
        id: &str,
        opts: AnswerOptions,
    ) -> Result<AnswerOutcome> {
        let call = self.get(id)?;
        let device = call.answer_is_device_bound(&opts);
        let ringing = call.is_ringing_inbound();
        let hold = match &opts.mode {
            AnswerMode::EndOthers(other_id) => {
                if other_id == id {
                    return Err(call.state_err("cannot end the call being answered"));
                }
                let other = self.get(other_id)?;
                // Answer first; the other call is only hung up once the caller's ACK confirms
                // the new one (background task). The answer itself succeeded once sent.
                call.answer(opts)?;
                Self::spawn_end_others(call, other);
                return Ok(AnswerOutcome::EndOthersPendingAck);
            }
            AnswerMode::Default => call.live_cfg().auto_hold,
            AnswerMode::HoldOthers => true,
        };
        if !(hold && ringing) {
            return call.answer(opts).map(|()| AnswerOutcome::Answered);
        }
        if !device {
            if opts.mode == AnswerMode::HoldOthers {
                tracing::debug!(call = %id, "hold_others ignored: the call uses no audio device");
            }
            return call.answer(opts).map(|()| AnswerOutcome::Answered);
        }
        let _line = self.lines.lock().await;
        if opts.mode == AnswerMode::HoldOthers {
            self.hold_others_explicit(&call.account, Some(id)).await;
        } else {
            self.auto_hold_others(&call.account, Some(id)).await;
        }
        call.answer(opts).map(|()| AnswerOutcome::Answered)
    }

    /// Background half of `end_others`: waits for the ACK of `call` (until its dialog is
    /// confirmed, it ends, or Timer H runs out), then hangs `other` up. Without an ACK `other`
    /// is untouched and becomes the active line (and media focus) again.
    fn spawn_end_others(call: Arc<Call>, other: Arc<Call>) {
        tokio::spawn(async move {
            let wait = Duration::from_millis(END_OTHERS_ACK_WAIT_MS.load(Ordering::Relaxed));
            match call.wait_confirmed(wait).await {
                Ok(()) => {
                    let _ = other.hangup_now();
                }
                Err(e) => {
                    tracing::warn!(call = %call.id, other = %other.id, error = %e,
                        "end_others: no ACK; the other call stays");
                    if call.lock().info.state == CallState::Ringing {
                        // No 200 was sent (reliable 180 never PRACKed): drop the queued answer.
                        call.cancel_deferred_answer();
                    }
                    if other.auto_holdable() {
                        other.focus_media();
                    }
                }
            }
        });
    }

    pub(crate) async fn reject(
        self: &Arc<Self>,
        id: &str,
        code: Option<u16>,
        reason: Option<String>,
    ) -> Result<()> {
        self.get(id)?.reject(code, reason, true)
    }

    pub(crate) async fn hangup(self: &Arc<Self>, id: &str) -> Result<()> {
        self.get(id)?.hangup_now()
    }

    pub(crate) async fn mute(self: &Arc<Self>, id: &str, muted: bool) -> Result<()> {
        self.get(id)?.mute(muted)
    }

    pub(crate) async fn send_dtmf(
        self: &Arc<Self>,
        id: &str,
        digits: &str,
        mode: Option<DtmfMode>,
        duration_ms: Option<u32>,
    ) -> Result<()> {
        self.get(id)?.send_dtmf(digits, mode, duration_ms).await
    }

    /// Hold / resume requested through the stack. Single-call: it does not take the line
    /// coordinator (the per-call `reinvite_out` guard serializes re-INVITEs of one call), so a
    /// slow peer never blocks other lines.
    pub(crate) async fn hold(self: &Arc<Self>, id: &str, held: bool) -> Result<()> {
        let call = self.get(id)?;
        if let Some(s) = self.session_blocks_hold(id) {
            return Err(call.state_err(&format!(
                "transfer {} is {}: hold and resume are not allowed until it settles",
                s.id,
                s.phase.as_str()
            )));
        }
        // A re-INVITE of ours in flight (an automatic resume after a failed transfer, a session
        // refresh) is waited for once, so a redundant request becomes a no-op instead of an error.
        let started = tokio::time::Instant::now();
        if call.lock().reinvite_out {
            let _ = tokio::time::timeout(LINE_HOLD_TIMEOUT, call.wait_reinvite_idle()).await;
            // A transfer may have started meanwhile.
            if let Some(s) = self.session_blocks_hold(id) {
                return Err(call.state_err(&format!(
                    "transfer {} is {}: hold and resume are not allowed until it settles",
                    s.id,
                    s.phase.as_str()
                )));
            }
        }
        let _pending = call.set_pending(if held {
            PendingOp::Hold
        } else {
            PendingOp::Resume
        });
        // The wait above counts against the budget, so the whole request stays within it.
        let left = HOLD_BUDGET.saturating_sub(started.elapsed());
        tokio::time::timeout(left, call.hold(held))
            .await
            .unwrap_or(Err(SipError::Timeout))
    }
}

/// Longest wait for the final response to a re-INVITE of ours: Timer B (64*T1 = 32 s) plus a
/// margin. rsipstack stops Timer B at a 100 Trying and ignores Timer C, so a peer that goes
/// silent after a 100 would otherwise leave the request (and `reinvite_out`) pending forever.
const REINVITE_BOUND: Duration = Duration::from_secs(34);
/// How long a re-INVITE waits for the dialog's other INVITE transaction (rsipstack refuses to start
/// one meanwhile, RFC 3261 §14.1), polling every [`INVITE_BUSY_POLL`]. That is mostly the ACK of a
/// re-INVITE we just answered, which follows within a round trip; past this, callers get a 491.
const INVITE_BUSY_WAIT: Duration = Duration::from_secs(1);
const INVITE_BUSY_POLL: Duration = Duration::from_millis(25);
/// Overall budget of one public `hold()` / `unhold()`: the wait for an in-flight re-INVITE,
/// the dialog-confirm wait, every attempt and the 491 backoff. The control client allows more
/// (`request_timeout`, 50 s).
const HOLD_BUDGET: Duration = Duration::from_secs(44);
/// A 491 retry that cannot get at least this much of the budget is not made.
const HOLD_MIN_ATTEMPT: Duration = Duration::from_secs(5);

/// Clears `CallInfo::pending` when an operation finishes or is dropped.
pub(crate) struct PendingGuard {
    call: Arc<Call>,
    op: PendingOp,
    /// False when an outstanding REFER kept the marker: this guard then never touches it.
    armed: bool,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        let mut g = self.call.lock();
        if self.armed && g.info.pending == Some(self.op) {
            g.info.pending = None;
            if g.info.state != CallState::Ended {
                self.call.emit(SipEvent::CallUpdated {
                    call: g.info.clone(),
                });
            }
        }
    }
}

/// Clears `reinvite_out` when a hold future finishes or is dropped.
struct ReinviteGuard(Arc<Call>);

impl Drop for ReinviteGuard {
    fn drop(&mut self) {
        self.0.lock().reinvite_out = false;
    }
}

/// Hold the session should *offer* now: our wish, except that a remote that holds us
/// (`remote_holds`) turns a `sendonly` offer into `inactive` (RFC 3264 §8.4).
fn offer_hold(desired: LocalHold, remote_holds: bool) -> LocalHold {
    match desired {
        LocalHold::SendOnly if remote_holds => LocalHold::Inactive,
        d => d,
    }
}

/// Our current SDP as an offer, with the direction derived from the desired hold and the
/// remote's current state. The session keeps the desired hold.
pub(crate) fn offer_sdp(session: &mut MediaSession, remote_holds: bool) -> String {
    let desired = session.local_hold();
    session.set_local_hold(offer_hold(desired, remote_holds));
    let sdp = session.current_sdp();
    session.set_local_hold(desired);
    sdp
}

/// Applies the answer to an offer made by [`offer_sdp`].
pub(crate) fn answer_sdp(
    session: &mut MediaSession,
    remote_holds: bool,
    answer: &SessionDescription,
) -> std::result::Result<Negotiated, siple_media::sdp_neg::NegError> {
    let desired = session.local_hold();
    session.set_local_hold(offer_hold(desired, remote_holds));
    let res = session.apply_answer(answer);
    session.set_local_hold(desired);
    res
}

/// Outcome of one hold attempt.
enum HoldTry {
    /// 491 Request Pending.
    Glare,
    Failed(SipError),
}

impl Call {
    pub(crate) fn lock(&self) -> MutexGuard<'_, CallInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn snapshot(&self) -> CallInfo {
        self.lock().info.clone()
    }

    pub(crate) fn is_ended(&self) -> bool {
        self.lock().info.state == CallState::Ended
    }

    /// Runtime of the call; `None` once it is over.
    pub(crate) fn rt(&self) -> Option<Arc<RtEnv>> {
        self.lock().rt.clone()
    }

    /// `(SIP Call-ID, our tag)` identifying the current dialog.
    pub(crate) fn dialog_key(&self) -> (String, String) {
        let g = self.lock();
        (g.info.sip_call_id.clone(), g.local_tag.clone())
    }

    pub(crate) fn emit(&self, ev: SipEvent) {
        self.mgr.emit(ev);
    }

    /// An established call on a device that `auto_hold` may put on hold: active, or held by the
    /// remote only (which would otherwise leave two live lines after a remote resume).
    fn auto_holdable(&self) -> bool {
        let g = self.lock();
        matches!(g.info.state, CallState::Active | CallState::RemoteHeld)
            && !g.info.hold.local
            && (g.source == AudioSource::Device || g.sink == AudioSink::Device)
    }

    /// Marks `op` as in flight on this call until the guard drops (`CallInfo::pending`).
    pub(crate) fn set_pending(self: &Arc<Self>, op: PendingOp) -> PendingGuard {
        let armed = {
            let mut g = self.lock();
            if g.info.pending == Some(PendingOp::Refer) && op != PendingOp::Refer {
                // An outstanding REFER keeps its marker.
                false
            } else {
                if g.info.state != CallState::Ended && g.info.pending != Some(op) {
                    g.info.pending = Some(op);
                    self.emit(SipEvent::CallUpdated {
                        call: g.info.clone(),
                    });
                }
                true
            }
        };
        PendingGuard {
            call: self.clone(),
            op,
            armed,
        }
    }

    /// Gives this device-bound call the media focus (and makes it the active line).
    pub(crate) fn focus_media(&self) {
        let g = self.lock();
        if let Some(m) = &g.media
            && (g.source == AudioSource::Device || g.sink == AudioSink::Device)
        {
            m.set_focus();
            self.mgr.set_active(&self.id);
        }
    }

    /// Whether the call uses an audio device.
    pub(crate) fn device_bound(&self) -> bool {
        let g = self.lock();
        g.source == AudioSource::Device || g.sink == AudioSink::Device
    }

    /// Whether answering with `opts` would use an audio device (options fall back to the
    /// call's own source / sink).
    pub(crate) fn answer_is_device_bound(&self, opts: &AnswerOptions) -> bool {
        let g = self.lock();
        opts.source.as_ref().unwrap_or(&g.source) == &AudioSource::Device
            || opts.sink.as_ref().unwrap_or(&g.sink) == &AudioSink::Device
    }

    pub(crate) fn is_ringing_inbound(&self) -> bool {
        self.direction == CallDirection::Inbound && self.lock().info.state == CallState::Ringing
    }

    /// Resolves when the call is over.
    pub(crate) async fn wait_ended(&self) {
        let mut rx = self.ended.subscribe();
        let _ = rx.wait_for(|v| *v).await;
    }

    /// Resolves when no re-INVITE of ours is outstanding (or the call is over).
    pub(crate) async fn wait_reinvite_idle(&self) {
        loop {
            {
                let g = self.lock();
                if !g.reinvite_out || g.info.state == CallState::Ended {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Waits up to `wait` for the dialog of an answered inbound call to be confirmed (the ACK).
    pub(crate) async fn wait_confirmed(&self, wait: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let d = {
                let g = self.lock();
                if g.info.state == CallState::Ended {
                    return Err(self.state_err("call ended before it was confirmed"));
                }
                g.dialog.clone()
            };
            if d.is_some_and(|d| d.state().is_confirmed()) {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(self.state_err("answer not confirmed (no ACK received)"));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Forgets an answer that waits for the PRACK of a reliable 180 (the caller was told the
    /// answer failed).
    pub(crate) fn cancel_deferred_answer(&self) {
        self.lock().reliable.deferred = None;
    }

    /// Changes the state (no-op when equal or after the end) and emits `CallState`. The event is
    /// sent under the lock so events keep the order of the transitions.
    pub(crate) fn set_state(&self, g: &mut CallInner, state: CallState) {
        if g.info.state == state || g.info.state == CallState::Ended {
            return;
        }
        tracing::debug!(call = %self.id, from = ?g.info.state, to = ?state, "call state");
        g.info.state = state;
        self.emit(SipEvent::CallState {
            call: g.info.clone(),
        });
        self.mgr.session_call_state(&self.id, state, None);
    }

    /// Re-derives Active / Held / RemoteHeld from the hold flags.
    pub(crate) fn sync_hold_state(&self, g: &mut CallInner) {
        if !matches!(
            g.info.state,
            CallState::Active | CallState::Held | CallState::RemoteHeld
        ) {
            return;
        }
        let st = if g.info.hold.local {
            CallState::Held
        } else if g.info.hold.remote {
            CallState::RemoteHeld
        } else {
            CallState::Active
        };
        self.set_state(g, st);
    }

    /// Emits `CallUpdated` when the hold flags of an established call changed.
    fn announce_hold(&self, g: &mut CallInner, before: HoldFlags) {
        if g.info.hold != before
            && matches!(
                g.info.state,
                CallState::Active | CallState::Held | CallState::RemoteHeld
            )
        {
            g.info.hold_changed_at = Some(Utc::now());
            self.emit(SipEvent::CallUpdated {
                call: g.info.clone(),
            });
        }
    }

    /// Records a finished negotiation: media, summary, remote hold, `CallMedia` event.
    pub(crate) fn apply_negotiated(&self, g: &mut CallInner, neg: Negotiated) {
        let before = g.info.hold;
        self.apply_negotiated_quiet(g, neg);
        self.announce_hold(g, before);
    }

    fn apply_negotiated_quiet(&self, g: &mut CallInner, neg: Negotiated) {
        if let Some(m) = &g.media {
            m.apply(&neg);
        }
        let summary = NegotiatedSummary::from_negotiated(&neg);
        let changed = g.info.negotiated.as_ref() != Some(&summary);
        // An answer to an offer of ours that could not receive says nothing about the remote
        // holding us: keep what we knew.
        if neg.remote_hold_known {
            g.info.hold.remote = neg.remote_hold;
        }
        g.info.negotiated = Some(summary.clone());
        g.neg = Some(neg);
        if changed {
            self.emit(SipEvent::CallMedia {
                call_id: self.id.clone(),
                negotiated: summary,
            });
        }
        self.sync_hold_state(g);
    }

    /// Ends the call (first caller wins). Returns whether this call did it. An ended call keeps
    /// only its [`CallInfo`]: runtime, dialog, media session and INVITE are released.
    pub(crate) fn end(&self, end: CallEnd) -> bool {
        let (media, dialog, rt) = {
            let mut g = self.lock();
            if g.info.state == CallState::Ended {
                return false;
            }
            tracing::info!(call = %self.id, code = end.code, reason = %end.reason, by = ?end.by, "call ended");
            let mut end = end;
            // A successful transfer outcome decides the cause, whichever side's BYE won.
            if matches!(end.cause, EndCause::Hangup | EndCause::RemoteHangup)
                && let Some(h) = g.cause_hint.take()
            {
                end.cause = h;
            }
            g.info.state = CallState::Ended;
            g.info.reconnecting = false;
            self.ended.send_replace(true);
            g.info.ended_at = Some(Utc::now());
            g.info.pending = None;
            self.mgr
                .session_call_state(&self.id, CallState::Ended, Some(&end));
            g.info.end = Some(end);
            if let Some(a) = g.answer_timer.take() {
                a.abort();
            }
            g.reliable.stop();
            if let Some(t) = g.timer.as_mut() {
                t.stop();
            }
            let media = g.media.take();
            let dialog = g.dialog.take();
            // A late INVITE is still being finished in the background: its dialog stays in the
            // layer until then.
            let dialog = if g.late_dialog.is_some() {
                None
            } else {
                dialog
            };
            let rt = g.rt.take();
            g.session = None;
            g.invite = None;
            g.offer = None;
            g.answer = None;
            self.emit(SipEvent::CallState {
                call: g.info.clone(),
            });
            (media, dialog, rt)
        };
        if let Some(m) = media {
            m.close();
        }
        if let (Some(d), Some(rt)) = (dialog, rt) {
            rt.dialogs.remove_dialog(&d.id());
        }
        self.mgr.notify_count(&self.env);
        self.transfer_call_ended();
        true
    }

    /// Forgets the call without events (stack shutdown).
    fn end_quiet(&self) {
        let media = {
            let mut g = self.lock();
            if let Some(a) = g.answer_timer.take() {
                a.abort();
            }
            if let Some(a) = g.invite_abort.take() {
                a.abort();
            }
            g.reliable.stop();
            if let Some(t) = g.timer.as_mut() {
                t.stop();
            }
            g.dialog = None;
            g.late_dialog = None;
            g.rt = None;
            g.media.take()
        };
        if let Some(m) = media {
            m.close();
        }
    }

    pub(crate) fn remote_end(&self, code: u16, reason: impl Into<String>, cause: EndCause) -> bool {
        self.end(CallEnd {
            code,
            reason: reason.into(),
            by: EndedBy::Remote,
            missed: cause == EndCause::Missed,
            q850_cause: None,
            cause,
        })
    }

    pub(crate) fn local_end(&self, code: u16, reason: impl Into<String>, cause: EndCause) -> bool {
        self.end(CallEnd {
            code,
            reason: reason.into(),
            by: EndedBy::Local,
            missed: false,
            q850_cause: None,
            cause,
        })
    }

    // --- operations --------------------------------------------------------------------

    /// Local mute (no signaling).
    pub(crate) fn mute(&self, muted: bool) -> Result<()> {
        let mut g = self.lock();
        if g.info.state == CallState::Ended {
            return Err(self.state_err("call is over"));
        }
        if g.info.muted == muted {
            return Ok(());
        }
        g.info.muted = muted;
        if let Some(m) = &g.media {
            m.set_mute(muted);
        }
        self.emit(SipEvent::CallUpdated {
            call: g.info.clone(),
        });
        Ok(())
    }

    pub(crate) fn state_err(&self, msg: &str) -> SipError {
        SipError::CallState {
            call: self.id.clone(),
            msg: msg.to_string(),
        }
    }

    /// Hangs up whatever state the call is in. The call ends locally at once (state `Ended`,
    /// media closed); the network part (CANCEL, BYE, or ACK + BYE for a late 2xx) finishes in
    /// the background with its own timeout. Ringing inbound calls are rejected with 603.
    pub(crate) fn hangup_now(self: &Arc<Self>) -> Result<()> {
        self.hangup_with(None)
    }

    /// [`Call::hangup_now`] with an end reason for the BYE case (`transferred`, `replaced`).
    pub(crate) fn hangup_with(self: &Arc<Self>, why: Option<&str>) -> Result<()> {
        enum Plan {
            Done,
            Reject,
            Cancel(InviteDialog),
            Bye(InviteDialog),
        }
        let plan = {
            let mut g = self.lock();
            match g.info.state {
                CallState::Ended => Plan::Done,
                CallState::Ringing => Plan::Reject,
                CallState::Dialing | CallState::Early => {
                    g.local_hangup = true;
                    match g.dialog.clone() {
                        Some(d) => {
                            g.late_dialog = Some(d.clone());
                            Plan::Cancel(d)
                        }
                        None => Plan::Done,
                    }
                }
                _ => {
                    g.local_hangup = true;
                    match g.dialog.clone() {
                        Some(d) => Plan::Bye(d),
                        None => Plan::Done,
                    }
                }
            }
        };
        match plan {
            Plan::Done => {
                self.local_end(200, "hang-up", EndCause::Hangup);
                Ok(())
            }
            Plan::Reject => self.reject(Some(603), None, true),
            Plan::Cancel(d) => {
                let rt = self.rt();
                self.local_end(487, "Request Terminated", EndCause::Cancelled);
                let c = self.clone();
                self.mgr
                    .spawn_bg(&self.account, async move { c.finish_cancel(d, rt).await });
                Ok(())
            }
            Plan::Bye(d) => {
                match why {
                    Some("session_expired") => {
                        self.local_end(408, "session expired", EndCause::SessionExpired)
                    }
                    Some("transferred") => {
                        self.local_end(200, "transferred", EndCause::Transferred)
                    }
                    Some("replaced") => self.local_end(200, "replaced", EndCause::Replaced),
                    other => self.local_end(200, other.unwrap_or("BYE"), EndCause::Hangup),
                };
                let id = self.id.clone();
                let (cause, text) = match why {
                    Some("transferred") => (200, "Call transferred"),
                    Some("replaced") => (200, "Call replaced"),
                    Some("session_expired") => (408, "Session expired"),
                    _ => (200, "Call completed"),
                };
                self.mgr.spawn_bg(&self.account, async move {
                    let res = tokio::time::timeout(
                        BYE_TIMEOUT,
                        d.bye_with_headers(Some(vec![Header::Other(
                            "Reason".into(),
                            format!("SIP;cause={cause};text=\"{text}\""),
                        )])),
                    )
                    .await;
                    match res {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => tracing::warn!(call = %id, error = %e, "BYE failed"),
                        Err(_) => tracing::warn!(call = %id, "BYE timed out"),
                    }
                });
                Ok(())
            }
        }
    }

    /// Creates the media of a call (called before any SDP is generated).
    pub(crate) fn make_media_request(
        &self,
        rt: &RtEnv,
        source: &AudioSource,
        sink: &AudioSink,
    ) -> CallMediaRequest {
        CallMediaRequest {
            call_id: self.id.clone(),
            account_id: self.account.clone(),
            bind_ip: rt.bind_ip,
            advertise_ip: rt.advertise_ip,
            source: source.clone(),
            sink: sink.clone(),
            device_bound: *source == AudioSource::Device || *sink == AudioSink::Device,
        }
    }

    /// The dialog once it is confirmed (ACK received / 2xx processed); waits up to
    /// [`CONFIRM_WAIT`] for it, so a request issued right after `answer()` / `Active` does not
    /// get lost.
    async fn confirmed_dialog(&self) -> Result<InviteDialog> {
        let start = tokio::time::Instant::now();
        loop {
            let d = {
                let g = self.lock();
                if g.info.state == CallState::Ended {
                    return Err(self.state_err("call is over"));
                }
                g.dialog.clone()
            };
            let d = d.ok_or_else(|| self.state_err("no dialog"))?;
            if d.state().is_confirmed() {
                return Ok(d);
            }
            if start.elapsed() >= CONFIRM_WAIT {
                return Err(self.state_err("dialog is not confirmed yet (no ACK received)"));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// RFC 3261 §12.2.1.2: a 481 / 408 / no answer to an in-dialog request ends the dialog.
    /// Unless the peer said it has no such dialog (481), a best-effort BYE tells it so; the
    /// call ends at once, without waiting for that BYE.
    /// A network move of this call is pending or running (see `CallInner::move_pending`).
    pub(crate) fn move_pending(&self) -> bool {
        self.lock().move_pending
    }

    /// An in-dialog request could not be sent / completed: the dialog is gone, unless the
    /// request only lacked permission to answer a challenge (`auth_foreign`): that leaves the
    /// call up and just fails the request.
    fn request_failed(&self, e: &rsipstack::Error, what: &str) {
        if crate::authz::refusal(e).is_some() {
            tracing::info!(call = %self.id, "{what} refused: the challenge is not from the account's provider");
        } else {
            self.dialog_lost(0, what);
        }
    }

    fn dialog_lost(&self, code: u16, what: &str) {
        if code != 481 && self.lock().move_pending {
            // The network just changed under this call: a timeout or send error of another
            // re-INVITE says nothing about the new path. The move decides.
            tracing::info!(call = %self.id, code, "{what} failed during a network move; leaving it to the move");
            return;
        }
        tracing::warn!(call = %self.id, code, "{what}: dialog is gone; ending the call");
        let text = if code == 481 {
            "Call/Transaction Does Not Exist".to_string()
        } else if code == 408 {
            "Request Timeout".to_string()
        } else {
            format!("no response to {what}")
        };
        let dialog = self.lock().dialog.clone();
        if self.remote_end(code, text, EndCause::Failed)
            && code != 481
            && let Some(d) = dialog
        {
            self.mgr.spawn_bg(&self.account, async move {
                let _ = tokio::time::timeout(BYE_TIMEOUT, d.bye()).await;
            });
        }
    }

    /// Sends a re-INVITE and waits for its final response, at most `bound` (capped at
    /// [`REINVITE_BOUND`]). The bound turns into a synthetic 408 (as for Timer B), so every
    /// caller takes its usual timeout path. While another INVITE transaction of the dialog is in
    /// progress rsipstack refuses to start one; after [`INVITE_BUSY_WAIT`] of retries that turns
    /// into a synthetic 491, so callers apply their glare back-off.
    #[allow(clippy::result_large_err)] // rsipstack's own Result type, as returned by `reinvite`
    pub(crate) async fn reinvite_bounded(
        &self,
        dialog: &InviteDialog,
        headers: Vec<Header>,
        body: Option<Vec<u8>>,
        bound: Duration,
    ) -> rsipstack::Result<Option<rsipstack::sip::Response>> {
        let synthetic = |status_code| {
            Ok(Some(rsipstack::sip::Response {
                status_code,
                version: rsipstack::sip::Version::V2,
                headers: Default::default(),
                body: Vec::new(),
                wire_reason: None,
            }))
        };
        let start = tokio::time::Instant::now();
        let deadline = start + bound.min(REINVITE_BOUND);
        let busy_until = (start + INVITE_BUSY_WAIT).min(deadline);
        loop {
            let attempt = dialog.reinvite(Some(headers.clone()), body.clone());
            match tokio::time::timeout_at(deadline, attempt).await {
                Ok(Err(rsipstack::Error::DialogError(
                    _,
                    _,
                    rsipstack::sip::StatusCode::RequestPending,
                ))) => {
                    if tokio::time::Instant::now() + INVITE_BUSY_POLL >= busy_until {
                        return synthetic(rsipstack::sip::StatusCode::RequestPending);
                    }
                    tokio::time::sleep(INVITE_BUSY_POLL).await;
                }
                Ok(res) => return res,
                Err(_) => return synthetic(rsipstack::sip::StatusCode::RequestTimeout),
            }
        }
    }

    /// Local hold / unhold through a re-INVITE. `hold_style` picks `sendonly` or
    /// `inactive`; a remote that already holds us makes it `inactive`. A 491 is retried per
    /// RFC 3261 §14.1 (2.1-4 s when we sent the initial INVITE, else 0-2 s; two retries). On
    /// failure the previous state and media stay (§14.1).
    pub(crate) async fn hold(self: Arc<Self>, held: bool) -> Result<()> {
        // Nothing waits on a dead call: the end of the call aborts the exchange at once.
        tokio::select! {
            r = async {
                // A move onto a new network path owns the re-INVITE slot: wait for it (bounded;
                // part of this cancellable future, so a timeout around `hold` abandons the wait).
                self.wait_not_reconnecting(tokio::time::Instant::now() + HOLD_BUDGET).await;
                self.clone().hold_loop(held).await
            } => r,
            () = self.wait_ended() => Err(self.state_err("call is over")),
        }
    }

    async fn hold_loop(self: Arc<Self>, held: bool) -> Result<()> {
        let deadline = tokio::time::Instant::now() + HOLD_BUDGET;
        let mut retries = 0u8;
        loop {
            match self.hold_once(held, deadline).await {
                Ok(()) => return Ok(()),
                Err(HoldTry::Failed(e)) => return Err(e),
                Err(HoldTry::Glare) => {
                    if retries >= 2 {
                        return Err(SipError::Status {
                            code: 491,
                            reason: "Request Pending".into(),
                        });
                    }
                    retries += 1;
                    let ms: u64 = if self.direction == CallDirection::Outbound {
                        rand::random_range(2100..=4000)
                    } else {
                        rand::random_range(0..=2000)
                    };
                    if tokio::time::Instant::now() + Duration::from_millis(ms) + HOLD_MIN_ATTEMPT
                        > deadline
                    {
                        return Err(SipError::Status {
                            code: 491,
                            reason: "Request Pending".into(),
                        });
                    }
                    tracing::debug!(call = %self.id, ms, retries, "re-INVITE glare; retrying");
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                }
            }
        }
    }

    /// One hold / unhold attempt. The re-INVITE runs in its own task, which always finishes
    /// (ACKs the 2xx, applies or rolls back the session / media / call state) even when the
    /// caller's future is dropped (timeouts around `hold`), so an abandoned attempt can never
    /// leave the SDP direction and the call state out of step.
    async fn hold_once(
        self: &Arc<Self>,
        held: bool,
        deadline: tokio::time::Instant,
    ) -> std::result::Result<(), HoldTry> {
        let call = self.clone();
        tokio::spawn(async move { call.hold_exchange(held, deadline).await })
            .await
            .unwrap_or_else(|e| {
                Err(HoldTry::Failed(SipError::Protocol(format!(
                    "hold task failed: {e}"
                ))))
            })
    }

    async fn hold_exchange(
        self: &Arc<Self>,
        held: bool,
        deadline: tokio::time::Instant,
    ) -> std::result::Result<(), HoldTry> {
        let fail = HoldTry::Failed;
        let dialog = self.confirmed_dialog().await.map_err(fail)?;
        let (sdp, prev, _guard) = {
            let mut g = self.lock();
            if !matches!(
                g.info.state,
                CallState::Active | CallState::Held | CallState::RemoteHeld
            ) {
                return Err(HoldTry::Failed(self.state_err("call is not established")));
            }
            if g.info.hold.local == held {
                return Ok(());
            }
            if g.reinvite_out {
                return Err(HoldTry::Failed(self.state_err(REINVITE_BUSY)));
            }
            let remote = g.info.hold.remote;
            let Some(session) = g.session.as_mut() else {
                return Err(HoldTry::Failed(self.state_err("no media session")));
            };
            let prev = session.local_hold();
            // The session keeps what we *want* (RFC 3264 §8.4: the direction actually offered
            // or answered also depends on what the remote currently says).
            session.set_local_hold(if !held {
                LocalHold::None
            } else if self.cfg.hold_style == HoldStyle::Inactive {
                LocalHold::Inactive
            } else {
                LocalHold::SendOnly
            });
            let sdp = offer_sdp(session, remote);
            g.reinvite_out = true;
            (sdp, prev, ReinviteGuard(self.clone()))
        };
        let mut req_headers = util::sdp_headers(&self.cfg);
        req_headers.extend(self.timer_request_headers(&self.lock()));
        let res = tokio::select! {
            r = self.reinvite_bounded(
                &dialog,
                req_headers,
                Some(sdp.into_bytes()),
                deadline.saturating_duration_since(tokio::time::Instant::now()),
            ) => r,
            () = self.wait_ended() => {
                return Err(HoldTry::Failed(self.state_err("call is over")));
            }
        };
        let mut g = self.lock();
        let revert = |g: &mut CallInner| {
            if let Some(s) = g.session.as_mut() {
                s.set_local_hold(prev);
            }
        };
        if g.info.state == CallState::Ended {
            return Err(HoldTry::Failed(self.state_err("call is over")));
        }
        let resp = match res {
            Ok(Some(r)) => r,
            Ok(None) => {
                revert(&mut g);
                return Err(HoldTry::Failed(
                    self.state_err("re-INVITE was not sent: dialog is not confirmed"),
                ));
            }
            Err(e) => {
                revert(&mut g);
                drop(g);
                self.request_failed(&e, "re-INVITE");
                return Err(HoldTry::Failed(e.into()));
            }
        };
        let code = resp.status_code.code();
        if !(200..300).contains(&code) {
            revert(&mut g);
            drop(g);
            if code == 491 {
                return Err(HoldTry::Glare);
            }
            if code == 481 || code == 408 {
                self.dialog_lost(code, "re-INVITE");
            }
            return Err(HoldTry::Failed(SipError::Status {
                code,
                reason: resp.status_code.text().to_string(),
            }));
        }
        match util::sdp_body(&resp.headers, &resp.body) {
            util::SdpBody::Sdp(_, sd) => {
                let remote = g.info.hold.remote;
                let Some(session) = g.session.as_mut() else {
                    return Err(HoldTry::Failed(self.state_err("no media session")));
                };
                match answer_sdp(session, remote, &sd) {
                    Ok(neg) => {
                        let before = g.info.hold;
                        g.info.hold.local = held;
                        if let Some(m) = &g.media {
                            m.set_hold(held);
                            // A resumed device call takes the microphone back (the media
                            // engine moved it off the call when it was held).
                            if !held
                                && (g.source == AudioSource::Device || g.sink == AudioSink::Device)
                            {
                                m.set_focus();
                                self.mgr.set_active(&self.id);
                            }
                        }
                        self.apply_negotiated_quiet(&mut g, neg);
                        self.announce_hold(&mut g, before);
                        // Any successful re-INVITE refreshes the session.
                        if g.timer.is_some() {
                            drop(g);
                            self.touch_timer();
                        }
                        Ok(())
                    }
                    Err(e) => {
                        revert(&mut g);
                        Err(HoldTry::Failed(SipError::Protocol(e.to_string())))
                    }
                }
            }
            _ => {
                revert(&mut g);
                Err(HoldTry::Failed(SipError::Protocol(
                    "re-INVITE answer without SDP".into(),
                )))
            }
        }
    }

    /// Sends DTMF digits.
    pub(crate) async fn send_dtmf(
        self: Arc<Self>,
        digits: &str,
        mode: Option<DtmfMode>,
        duration_ms: Option<u32>,
    ) -> Result<()> {
        if digits.is_empty()
            || digits.chars().count() > MAX_DTMF_DIGITS
            || !digits
                .chars()
                .all(|c| matches!(c, '0'..='9' | '*' | '#' | 'A'..='D' | 'a'..='d'))
        {
            return Err(SipError::InvalidArgument(format!(
                "invalid DTMF digits {digits:?} (1-{MAX_DTMF_DIGITS} of 0-9 * # A-D)"
            )));
        }
        if let Some(d) = duration_ms
            && !(DTMF_DURATION_MIN..=DTMF_DURATION_MAX).contains(&d)
        {
            return Err(SipError::InvalidArgument(format!(
                "duration_ms {d} is outside {DTMF_DURATION_MIN}..={DTMF_DURATION_MAX}"
            )));
        }
        let (media, rfc4733) = {
            let g = self.lock();
            if !matches!(
                g.info.state,
                CallState::Active | CallState::Held | CallState::RemoteHeld
            ) {
                return Err(self.state_err("call is not established"));
            }
            (
                g.media.clone(),
                g.neg.as_ref().is_some_and(|n| n.te.is_some()),
            )
        };
        // The live [media] section (hot-updated through `SipStack::set_media`).
        let media_cfg = self
            .env
            .env()
            .map_or_else(|| self.media_cfg.clone(), |e| e.media);
        let mode = mode.unwrap_or_else(|| self.cfg.effective_dtmf_mode(&media_cfg));
        let duration = duration_ms
            .unwrap_or(media_cfg.dtmf_duration_ms)
            .clamp(DTMF_DURATION_MIN, DTMF_DURATION_MAX);
        let gap = media_cfg.dtmf_gap_ms.min(DTMF_GAP_MAX);
        let mode = match mode {
            DtmfMode::Auto if rfc4733 => DtmfMode::Rfc4733,
            DtmfMode::Auto => DtmfMode::Info,
            m => m,
        };
        match mode {
            DtmfMode::Rfc4733 => {
                if !rfc4733 {
                    return Err(self.state_err("telephone-event was not negotiated"));
                }
                let m = media.ok_or_else(|| self.state_err("no media"))?;
                m.send_dtmf_rfc4733(digits, duration, gap)
                    .map_err(|e| SipError::Media(e.to_string()))
            }
            DtmfMode::Inband => {
                let m = media.ok_or_else(|| self.state_err("no media"))?;
                m.send_dtmf_inband(digits, duration, gap)
                    .map_err(|e| SipError::Media(e.to_string()))
            }
            DtmfMode::Info | DtmfMode::Auto => {
                let dialog = self.confirmed_dialog().await?;
                let n = digits.chars().count();
                for (i, d) in digits.chars().enumerate() {
                    let body = format!(
                        "Signal={}\r\nDuration={}\r\n",
                        d.to_ascii_uppercase(),
                        duration
                    );
                    let headers = vec![Header::ContentType("application/dtmf-relay".into())];
                    match dialog.info(Some(headers), Some(body.into_bytes())).await {
                        Ok(Some(r))
                            if r.status_code.kind()
                                == rsipstack::sip::StatusCodeKind::Successful => {}
                        Ok(Some(r)) => {
                            let code = r.status_code.code();
                            if code == 481 || code == 408 {
                                self.dialog_lost(code, "INFO");
                            }
                            return Err(SipError::Status {
                                code,
                                reason: r.status_code.text().to_string(),
                            });
                        }
                        Ok(None) => {
                            return Err(
                                self.state_err("INFO was not sent: dialog is not confirmed")
                            );
                        }
                        Err(e) => {
                            self.request_failed(&e, "INFO");
                            return Err(e.into());
                        }
                    }
                    if i + 1 < n {
                        tokio::time::sleep(Duration::from_millis(u64::from(
                            duration.saturating_add(gap),
                        )))
                        .await;
                    }
                }
                Ok(())
            }
        }
    }
}

/// Most digits per `send_dtmf`.
const MAX_DTMF_DIGITS: usize = 32;
/// Tone duration range (same as the config range).
const DTMF_DURATION_MIN: u32 = 10;
const DTMF_DURATION_MAX: u32 = 10_000;
/// Longest gap between digits.
const DTMF_GAP_MAX: u32 = 5_000;

#[cfg(test)]
mod srtp_memory_tests {
    use super::*;

    fn cell() -> Arc<AccountCalls> {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        AccountCalls::new(
            "a".into(),
            tx,
            false,
            Credential::default(),
            crate::authz::AuthState::new(),
        )
    }

    #[test]
    fn stale_generation_commits_are_ignored() {
        let c = cell();
        let g0 = c.srtp_generation();
        assert!(c.remember_srtp_dest("h:5060", g0));
        // A clear (config change / rebuild / re-probe rebuild) bumps the generation, even when
        // the memory was already empty.
        assert!(c.clear_srtp_downgraded());
        assert!(
            !c.remember_srtp_dest("h:5060", g0),
            "stale remember re-added"
        );
        assert!(c.srtp_downgraded().is_empty());
        let g1 = c.srtp_generation();
        assert_ne!(g0, g1);
        assert!(c.remember_srtp_dest("h:5060", g1));
        assert!(!c.forget_srtp_dest("h:5060", g0), "stale forget applied");
        assert!(c.srtp_remembered("h:5060"));
        assert!(c.forget_srtp_dest("h:5060", g1));
        assert!(!c.clear_srtp_downgraded());
        assert_ne!(c.srtp_generation(), g1);
    }
}

#[cfg(test)]
mod generation_tests {
    use super::*;
    use rsipstack::transaction::EndpointBuilder;

    fn env(stream_gen: u64, listener: Option<std::net::SocketAddr>) -> RtEnv {
        let endpoint = EndpointBuilder::new().build();
        let inner = endpoint.inner.clone();
        let (state_tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        RtEnv {
            endpoint: inner.clone(),
            dialogs: Arc::new(DialogLayer::new(inner)),
            state_tx,
            contact: Arc::default(),
            bind_ip: "10.0.0.1".parse().unwrap(),
            advertise_ip: "10.0.0.1".parse().unwrap(),
            target: None,
            transport: SipTransport::Tcp,
            use_sips: false,
            public_host: None,
            stream_gen,
            listener,
        }
    }

    /// A client-only stream account has no listener: after its connections were retired (every
    /// address-changing rebind), a call on the same address with an older stream generation has
    /// no flow left and must be moved.
    #[tokio::test]
    async fn retired_stream_connections_make_a_new_generation_even_on_the_same_address() {
        let a = env(0, None);
        let same = env(0, None);
        let after_retire = env(1, None);
        assert!(a.same_generation(&same));
        assert!(!a.same_generation(&after_retire));
        let l1: std::net::SocketAddr = "10.0.0.1:5060".parse().unwrap();
        let l2: std::net::SocketAddr = "10.0.0.1:5062".parse().unwrap();
        assert!(!env(0, Some(l1)).same_generation(&env(0, Some(l2))));
    }
}

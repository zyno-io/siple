//! Transfer sessions: one record pairing the original call A and its consult
//! call B (or just A for a blind transfer) through consult, REFER and result, owned by the
//! engine so every client (GUI, CLI, MCP) sees the same state, auto-resume and idempotency.
//!
//! Locking: the session table has its own std mutex that is only ever taken *after* (never
//! while taking) a call lock, and it never locks a call itself; transitions that depend on the
//! state of a call read it first. Operations that touch more than one call take the line
//! coordinator (`CallManager::lines`) when a device-bound call is involved, like `activate`;
//! non-device calls (agents, automation) never take it.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, MutexGuard, PoisonError};

use chrono::{DateTime, Utc};
use serde::Serialize;
use siple_config::{AttendedReferOn, AudioSink, AudioSource};

use super::outbound::target_uri;
use super::{
    Call, CallEnd, CallId, CallManager, CallState, DialOptions, PendingOp, TransferRef,
    TransferRole,
};
use crate::error::{Result, SipError};
use crate::types::SipEvent;

/// Terminal sessions kept for display.
const KEEP_DONE: usize = 16;

/// How a transfer session came about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferKind {
    /// No consult call: the original is referred straight to the target.
    Blind,
    /// The original is replaced by the consult call's dialog (REFER with Replaces).
    Attended,
    /// The consult call was cancelled while ringing and the original was referred to the party
    /// it rang.
    Semi,
}

/// Where a transfer session stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferPhase {
    /// The consult INVITE was sent.
    ConsultDialing,
    /// The consult call is ringing.
    ConsultRinging,
    /// The consult call is answered.
    ConsultConnected,
    /// The consult call ended before the transfer (the original stays held).
    ConsultFailed,
    /// The REFER is on its way.
    Referring,
    /// The REFER was accepted; waiting for the transfer's outcome.
    Transferring,
    /// No outcome within 32 s; the transfer may or may not have happened.
    Unconfirmed,
    /// Terminal: the transfer succeeded.
    Transferred,
    /// Terminal: the transfer failed for good.
    Failed,
    /// Terminal: cancelled before the REFER.
    Cancelled,
    /// Terminal: the original call ended before there was an outcome.
    Abandoned,
}

impl TransferPhase {
    /// No further transitions.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Transferred | Self::Failed | Self::Cancelled | Self::Abandoned
        )
    }

    /// Before the REFER: the session is still a consult.
    pub fn is_consult(self) -> bool {
        matches!(
            self,
            Self::ConsultDialing
                | Self::ConsultRinging
                | Self::ConsultConnected
                | Self::ConsultFailed
        )
    }

    /// A REFER of ours is outstanding (hold / resume of either leg is refused).
    pub fn refer_outstanding(self) -> bool {
        matches!(self, Self::Referring | Self::Transferring)
    }

    /// The wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ConsultDialing => "consult_dialing",
            Self::ConsultRinging => "consult_ringing",
            Self::ConsultConnected => "consult_connected",
            Self::ConsultFailed => "consult_failed",
            Self::Referring => "referring",
            Self::Transferring => "transferring",
            Self::Unconfirmed => "unconfirmed",
            Self::Transferred => "transferred",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Abandoned => "abandoned",
        }
    }
}

/// The last `refer` NOTIFY of the transfer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TransferNotify {
    /// Status of the sipfrag.
    pub code: u16,
    /// Its reason phrase.
    pub reason: String,
    /// When it arrived.
    pub at: DateTime<Utc>,
}

/// What went wrong last.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TransferError {
    /// `consult`, `refer`, `notify`, `swap`, `resume` or `superseded`.
    pub stage: String,
    /// SIP status code; 0 = local failure.
    pub code: u16,
    /// Reason phrase or local error text.
    pub reason: String,
}

/// A transfer session (`snapshot.transfers`, `transfer.get`, event `transfer.session`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TransferSession {
    /// Session id (`t1`, `t2`, ...).
    pub id: String,
    /// Blind, attended or semi-attended.
    pub kind: TransferKind,
    /// Where it stands.
    pub phase: TransferPhase,
    /// Account of both calls.
    pub account: String,
    /// The call being transferred (A).
    pub original: CallId,
    /// The consult call (B); none for a blind transfer.
    pub consult: Option<CallId>,
    /// Where the original is sent: the consult party's dialled URI, or the blind target.
    pub target_uri: String,
    /// The call whose dialog carries the REFER (once it was sent).
    pub refer_call: Option<CallId>,
    /// The leg the user is talking to during the consult.
    pub talking_to: Option<CallId>,
    /// The session put the original on hold (basis of the auto-resume).
    pub we_held_original: bool,
    /// Last `refer` NOTIFY.
    pub last_notify: Option<TransferNotify>,
    /// Last error.
    pub last_error: Option<TransferError>,
    /// When the session started.
    pub started_at: DateTime<Utc>,
    /// When it reached a terminal phase.
    pub ended_at: Option<DateTime<Utc>>,
}

/// How [`crate::SipStack::transfer_complete`] finishes a consult.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompleteMode {
    /// REFER with Replaces; needs an answered consult call.
    Attended,
    /// End the consult call and refer the original to the party it was dialling.
    BlindNow,
}

/// The session table.
#[derive(Default)]
pub(crate) struct Sessions {
    live: Vec<TransferSession>,
    /// Terminal sessions, oldest first (at most [`KEEP_DONE`]).
    done: VecDeque<TransferSession>,
    /// Calls an operation is about to put in a session (closes the window between the checks
    /// and the insertion while a consult is dialled).
    reserved: Vec<CallId>,
    next: u64,
}

/// Releases the reservation of [`CallManager::reserve`] when dropped.
pub(crate) struct Reservation {
    mgr: Arc<CallManager>,
    ids: Vec<CallId>,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut st = self.mgr.sess();
        st.reserved.retain(|r| !self.ids.contains(r));
    }
}

/// Fails the session if the task that claimed REFERRING ends before the REFER exists (a panic or
/// an early return), so a call is never locked out by a session that can no longer progress.
struct ReferGuard {
    mgr: Arc<CallManager>,
    sid: String,
    armed: bool,
}

impl ReferGuard {
    fn new(mgr: &Arc<CallManager>, sid: &str) -> Self {
        Self {
            mgr: mgr.clone(),
            sid: sid.to_string(),
            armed: true,
        }
    }
}

impl Drop for ReferGuard {
    fn drop(&mut self) {
        if self.armed
            && self
                .mgr
                .session_refer_failed(&self.sid, "refer", 0, "the transfer task ended early")
        {
            let (mgr, sid) = (self.mgr.clone(), self.sid.clone());
            tokio::spawn(async move { mgr.auto_resume(&sid).await });
        }
    }
}

/// Runs `fut` in its own task: a caller that is dropped (control connection closed, UI timeout)
/// cannot leave a half-done session operation behind.
async fn detached<T: Send + 'static>(
    fut: impl std::future::Future<Output = Result<T>> + Send + 'static,
) -> Result<T> {
    tokio::spawn(fut)
        .await
        .unwrap_or_else(|e| Err(SipError::Protocol(format!("transfer task failed: {e}"))))
}

fn conflict(s: &TransferSession, call: &str) -> SipError {
    SipError::CallState {
        call: call.to_string(),
        msg: format!(
            "already in transfer session {} ({}); finish or cancel it first",
            s.id,
            s.phase.as_str()
        ),
    }
}

fn err(stage: &str, code: u16, reason: impl Into<String>) -> TransferError {
    TransferError {
        stage: stage.to_string(),
        code,
        reason: reason.into(),
    }
}

impl CallManager {
    fn sess(&self) -> MutexGuard<'_, Sessions> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `[calls] resume_after_failed_transfer` (hot).
    pub(crate) fn set_resume_after_failed(&self, on: bool) {
        self.resume_failed.store(on, Ordering::Relaxed);
    }

    /// Every session: the last 16 terminal ones, then the live ones, oldest first.
    pub(crate) fn transfers(&self) -> Vec<TransferSession> {
        let st = self.sess();
        st.done.iter().chain(st.live.iter()).cloned().collect()
    }

    /// One session (live or among the last 16 terminal ones).
    pub(crate) fn transfer(&self, id: &str) -> Result<TransferSession> {
        let st = self.sess();
        st.live
            .iter()
            .chain(st.done.iter())
            .find(|s| s.id == id)
            .cloned()
            .ok_or_else(|| SipError::UnknownTransfer(id.to_string()))
    }

    fn live_session(&self, id: &str) -> Option<TransferSession> {
        self.sess().live.iter().find(|s| s.id == id).cloned()
    }

    /// The non-terminal session `call` is part of.
    fn live_for_call(&self, call: &str) -> Option<TransferSession> {
        self.sess()
            .live
            .iter()
            .find(|s| s.original == call || s.consult.as_deref() == Some(call))
            .cloned()
    }

    /// Applies `f` to a live session. A change is announced (`SipEvent::TransferSession`) under
    /// the table lock, so events keep the order of the transitions; a session that became
    /// terminal moves to the bounded ring of finished ones. `None` if there is no such live
    /// session.
    fn update_session<R>(&self, id: &str, f: impl FnOnce(&mut TransferSession) -> R) -> Option<R> {
        let mut st = self.sess();
        let i = st.live.iter().position(|s| s.id == id)?;
        let before = st.live[i].clone();
        let r = f(&mut st.live[i]);
        if st.live[i] != before {
            if st.live[i].phase.is_terminal() {
                st.live[i].ended_at.get_or_insert_with(Utc::now);
                let s = st.live.remove(i);
                self.emit(SipEvent::TransferSession { session: s.clone() });
                st.done.push_back(s);
                while st.done.len() > KEEP_DONE {
                    st.done.pop_front();
                }
            } else {
                self.emit(SipEvent::TransferSession {
                    session: st.live[i].clone(),
                });
            }
        }
        Some(r)
    }

    /// Reserves `ids` for a session about to be created; fails when one of them is in a
    /// session or reserved already.
    fn reserve(self: &Arc<Self>, ids: &[&str]) -> Result<Reservation> {
        let mut st = self.sess();
        for id in ids {
            if let Some(s) = st
                .live
                .iter()
                .find(|s| s.original == *id || s.consult.as_deref() == Some(*id))
            {
                return Err(conflict(s, id));
            }
            if st.reserved.iter().any(|r| r == id) {
                return Err(SipError::CallState {
                    call: (*id).to_string(),
                    msg: "another transfer is being started on this call".into(),
                });
            }
        }
        st.reserved.extend(ids.iter().map(|s| (*s).to_string()));
        Ok(Reservation {
            mgr: self.clone(),
            ids: ids.iter().map(|s| (*s).to_string()).collect(),
        })
    }

    /// Inserts a new session (id assigned here), announces it and links its calls.
    /// `check_reserved`: refuse calls somebody reserved (false for the reservation holder).
    fn create_session(
        &self,
        mut s: TransferSession,
        check_reserved: bool,
    ) -> Result<TransferSession> {
        {
            let mut st = self.sess();
            let ids = [Some(s.original.clone()), s.consult.clone()];
            for id in ids.iter().flatten() {
                if let Some(x) = st
                    .live
                    .iter()
                    .find(|x| x.original == *id || x.consult.as_deref() == Some(id.as_str()))
                {
                    return Err(conflict(x, id));
                }
                if check_reserved && st.reserved.contains(id) {
                    return Err(SipError::CallState {
                        call: id.clone(),
                        msg: "another transfer is being started on this call".into(),
                    });
                }
            }
            st.next += 1;
            s.id = format!("t{}", st.next);
            st.live.push(s.clone());
            self.emit(SipEvent::TransferSession { session: s.clone() });
        }
        for (id, role) in [
            (Some(s.original.clone()), TransferRole::Original),
            (s.consult.clone(), TransferRole::Consult),
        ] {
            if let Some(call) = id.and_then(|i| self.get(&i).ok()) {
                let mut g = call.lock();
                if g.info.state != CallState::Ended {
                    g.info.transfer = Some(TransferRef {
                        session_id: s.id.clone(),
                        role,
                    });
                    call.emit(SipEvent::CallUpdated {
                        call: g.info.clone(),
                    });
                }
            }
        }
        Ok(s)
    }

    fn new_session(
        kind: TransferKind,
        phase: TransferPhase,
        a: &Call,
        consult: Option<CallId>,
        target: String,
        we_held: bool,
    ) -> TransferSession {
        TransferSession {
            id: String::new(),
            kind,
            phase,
            account: a.account.clone(),
            original: a.id.clone(),
            consult,
            target_uri: target,
            refer_call: None,
            talking_to: None,
            we_held_original: we_held,
            last_notify: None,
            last_error: None,
            started_at: Utc::now(),
            ended_at: None,
        }
    }

    // --- hooks from the calls ----------------------------------------------------------------

    /// A call changed state (`end` is set for `Ended`): drives the consult phases. Callable
    /// under a call lock.
    pub(crate) fn session_call_state(&self, call: &str, state: CallState, end: Option<&CallEnd>) {
        let Some((sid, is_consult)) = self
            .sess()
            .live
            .iter()
            .find(|s| s.original == call || s.consult.as_deref() == Some(call))
            .map(|s| (s.id.clone(), s.consult.as_deref() == Some(call)))
        else {
            return;
        };
        let call = call.to_string();
        let end = end.map(|e| (e.code, e.reason.clone()));
        self.update_session(&sid, |s| {
            use TransferPhase::*;
            if is_consult {
                match state {
                    CallState::Early if s.phase == ConsultDialing => s.phase = ConsultRinging,
                    CallState::Active | CallState::Held | CallState::RemoteHeld
                        if matches!(s.phase, ConsultDialing | ConsultRinging) =>
                    {
                        s.phase = ConsultConnected;
                        s.talking_to = Some(call);
                    }
                    CallState::Ended
                        if matches!(
                            s.phase,
                            ConsultDialing | ConsultRinging | ConsultConnected
                        ) =>
                    {
                        s.phase = ConsultFailed;
                        if s.talking_to.as_deref() == Some(call.as_str()) {
                            s.talking_to = None;
                        }
                        let (code, reason) = end.unwrap_or((0, "consult call ended".into()));
                        s.last_error = Some(err("consult", code, reason));
                    }
                    _ => {}
                }
            } else if state == CallState::Ended {
                // Past the REFER the outcome decides (the transferee may BYE first), except
                // that no REFER may exist yet, or that it travels on the consult call (A's end
                // then says nothing about its outcome and must not keep the session alive).
                let no_refer = s.phase == Referring && s.refer_call.is_none();
                let refer_elsewhere =
                    s.phase == Unconfirmed && s.refer_call.as_deref() != Some(s.original.as_str());
                if s.phase.is_consult() || no_refer || refer_elsewhere {
                    s.phase = Abandoned;
                }
            }
        });
    }

    /// The REFER of session `sid` was created on `refer_call`.
    pub(crate) fn session_refer_started(&self, sid: &str, refer_call: &str) {
        self.update_session(sid, |s| s.refer_call = Some(refer_call.to_string()));
    }

    /// The REFER was accepted (2xx).
    pub(crate) fn session_refer_accepted(&self, sid: &str) {
        self.update_session(sid, |s| {
            if s.phase == TransferPhase::Referring {
                s.phase = TransferPhase::Transferring;
            }
        });
    }

    /// A `refer` NOTIFY arrived.
    pub(crate) fn session_refer_notify(&self, sid: &str, code: u16, reason: &str) {
        self.update_session(sid, |s| {
            if matches!(
                s.phase,
                TransferPhase::Referring | TransferPhase::Transferring | TransferPhase::Unconfirmed
            ) {
                s.last_notify = Some(TransferNotify {
                    code,
                    reason: reason.to_string(),
                    at: Utc::now(),
                });
                if s.phase == TransferPhase::Referring {
                    s.phase = TransferPhase::Transferring;
                }
            }
        });
    }

    /// The final 2xx NOTIFY.
    pub(crate) fn session_refer_succeeded(&self, sid: &str) {
        self.update_session(sid, |s| {
            if matches!(
                s.phase,
                TransferPhase::Referring | TransferPhase::Transferring | TransferPhase::Unconfirmed
            ) {
                s.phase = TransferPhase::Transferred;
                s.talking_to = None;
                s.last_error = None;
            }
        });
    }

    /// No final NOTIFY in time: nothing is done automatically.
    /// Must not be called under a call lock (it reads A's state).
    pub(crate) fn session_refer_timed_out(&self, sid: &str) {
        let a_ended = self
            .live_session(sid)
            .and_then(|s| self.get(&s.original).ok())
            .is_none_or(|a| a.is_ended());
        self.update_session(sid, |s| {
            if s.phase.refer_outstanding() {
                // The original is gone and the REFER is on the consult call: nothing is left
                // to keep the session for.
                s.phase = if a_ended && s.refer_call.as_deref() != Some(s.original.as_str()) {
                    TransferPhase::Abandoned
                } else {
                    TransferPhase::Unconfirmed
                };
            }
        });
    }

    /// The call carrying the REFER ended without a result.
    pub(crate) fn session_refer_ended(&self, sid: &str) {
        self.update_session(sid, |s| {
            if s.phase.refer_outstanding() || s.phase == TransferPhase::Unconfirmed {
                s.phase = TransferPhase::Abandoned;
                s.last_error
                    .get_or_insert_with(|| err("refer", 0, "call ended before the result"));
            }
        });
    }

    /// The REFER failed (refused, local error or final NOTIFY >= 300). Returns whether the
    /// original should be resumed. Reads call states, so it must
    /// not be called under a call lock.
    pub(crate) fn session_refer_failed(
        &self,
        sid: &str,
        stage: &str,
        code: u16,
        reason: &str,
    ) -> bool {
        let Some(s) = self.live_session(sid) else {
            return false;
        };
        let alive = |id: Option<&str>| {
            id.and_then(|i| self.get(i).ok())
                .is_some_and(|c| !c.is_ended())
        };
        let a_alive = alive(Some(&s.original));
        let b_alive = alive(s.consult.as_deref());
        let auto = self.resume_failed.load(Ordering::Relaxed);
        self.update_session(sid, |s| {
            use TransferPhase::*;
            if !matches!(s.phase, Referring | Transferring | Unconfirmed) {
                return false;
            }
            let late = s.phase == Unconfirmed;
            s.last_error = Some(err(stage, code, reason));
            if !a_alive {
                s.phase = Abandoned;
                return false;
            }
            if b_alive && s.kind == TransferKind::Attended && !late {
                // Back to the consult: the user can try again, swap or cancel.
                s.phase = ConsultConnected;
                return false;
            }
            s.phase = Failed;
            auto && s.we_held_original && (!b_alive || s.kind != TransferKind::Attended)
        })
        .unwrap_or(false)
    }

    /// Takes the original off hold after a failed transfer: only when it is
    /// alive and still held by us, and no other device line is live (`resume_if_free`).
    pub(crate) async fn auto_resume(self: &Arc<Self>, sid: &str) {
        let Ok(s) = self.transfer(sid) else { return };
        let Ok(a) = self.get(&s.original) else { return };
        let held = {
            let g = a.lock();
            g.info.state != CallState::Ended && g.info.hold.local
        };
        if !held {
            return;
        }
        self.resume_if_free(&a).await;
        if a.auto_holdable() {
            a.focus_media();
        }
    }

    /// [`CallManager::session_refer_failed`] plus the auto-resume it asks for.
    pub(crate) async fn refer_failed(
        self: &Arc<Self>,
        sid: &str,
        stage: &str,
        code: u16,
        reason: &str,
    ) {
        if self.session_refer_failed(sid, stage, code, reason) {
            self.auto_resume(sid).await;
        }
    }

    /// Whether a hold / resume of `call` is refused: a REFER of its session is outstanding.
    pub(crate) fn session_blocks_hold(&self, call: &str) -> Option<TransferSession> {
        self.sess()
            .live
            .iter()
            .find(|s| {
                s.phase.refer_outstanding()
                    && (s.original == call || s.consult.as_deref() == Some(call))
            })
            .cloned()
    }

    /// The call carries the REFER of a session that is still outstanding: the only call that
    /// automatic holds leave alone (the consult leg may be held like any other line).
    pub(crate) fn session_refer_call_busy(&self, call: &str) -> bool {
        self.sess()
            .live
            .iter()
            .any(|s| s.phase.refer_outstanding() && s.refer_call.as_deref() == Some(call))
    }

    fn note_we_held(&self, sid: &str, held: bool) {
        if held {
            self.update_session(sid, |s| s.we_held_original = true);
        }
    }

    /// Feeds the consult call's current state into its (new) session.
    fn sync_consult(&self, s: &TransferSession) -> TransferSession {
        if let Some(b) = s.consult.as_deref().and_then(|b| self.get(b).ok()) {
            let (state, end) = {
                let g = b.lock();
                (g.info.state, g.info.end.clone())
            };
            self.session_call_state(&b.id, state, end.as_ref());
        }
        if let Ok(a) = self.get(&s.original) {
            let (state, end) = {
                let g = a.lock();
                (g.info.state, g.info.end.clone())
            };
            if state == CallState::Ended {
                self.session_call_state(&a.id, state, end.as_ref());
            }
        }
        self.transfer(&s.id).unwrap_or_else(|_| s.clone())
    }

    /// Legacy entry points (`transfer_blind` / `transfer_attended`) keep their "last writer
    /// wins" behaviour: sessions of the calls that are still consulting are dropped (calls
    /// untouched); one with a REFER outstanding refuses, as before.
    fn legacy_supersede(&self, ids: &[&str]) -> Result<()> {
        let hits: Vec<TransferSession> = self
            .sess()
            .live
            .iter()
            .filter(|s| {
                ids.iter()
                    .any(|i| s.original == *i || s.consult.as_deref() == Some(*i))
            })
            .cloned()
            .collect();
        if let Some(s) = hits.iter().find(|s| s.phase.refer_outstanding()) {
            return Err(SipError::CallState {
                call: s.original.clone(),
                msg: "a transfer is already in progress".into(),
            });
        }
        for s in hits {
            self.update_session(&s.id, |s| {
                // Re-checked under the lock: a concurrent complete may have claimed it.
                let next = if s.phase.is_consult() {
                    TransferPhase::Cancelled
                } else if s.phase == TransferPhase::Unconfirmed {
                    TransferPhase::Abandoned
                } else {
                    return;
                };
                s.last_error = Some(err("superseded", 0, "replaced by a newer transfer"));
                s.phase = next;
            });
        }
        Ok(())
    }

    // --- operations ----------------------------------------------------------------------------

    /// Blind transfer of `id` to `target`: a blind session (REFERRING right away).
    pub(crate) async fn transfer_blind(
        self: &Arc<Self>,
        id: &str,
        target: &str,
    ) -> Result<TransferSession> {
        let call = self.get(id)?;
        let uri = target_uri(&call.live_cfg(), target)?;
        call.ensure_established()?;
        self.legacy_supersede(&[id])?;
        let s = Self::new_session(
            TransferKind::Blind,
            TransferPhase::Referring,
            &call,
            None,
            uri.to_string(),
            false,
        );
        let s = self.create_session(s, true)?;
        let (this, sid, target) = (self.clone(), s.id.clone(), format!("<{uri}>"));
        detached(async move {
            let mut guard = ReferGuard::new(&this, &sid);
            let held = call.hold_for_transfer().await;
            this.note_we_held(&sid, held);
            guard.armed = false;
            call.start_refer(target, None, sid.clone()).await
        })
        .await?;
        Ok(self.transfer(&s.id).unwrap_or(s))
    }

    /// Legacy attended transfer of `id` to the party of `consult_id`: an implicit session that
    /// is completed at once (blind-now while the consult rings).
    pub(crate) async fn transfer_attended(
        self: &Arc<Self>,
        id: &str,
        consult_id: &str,
    ) -> Result<TransferSession> {
        let existing = self
            .live_for_call(id)
            .filter(|s| s.original == id && s.consult.as_deref() == Some(consult_id))
            // Unconfirmed: a retry starts a new session (the old one is superseded below).
            .filter(|s| s.phase != TransferPhase::Unconfirmed);
        let sid = match existing {
            Some(s) => s.id,
            None => {
                let a = self.get(id)?;
                let b = self.get(consult_id)?;
                if a.id != b.id && a.account == b.account {
                    self.legacy_supersede(&[id, consult_id])?;
                }
                self.consult_link(id, consult_id).await?.id
            }
        };
        let mode = match self.get(consult_id)?.snapshot().state {
            CallState::Dialing | CallState::Early => CompleteMode::BlindNow,
            _ => CompleteMode::Attended,
        };
        self.transfer_complete(&sid, mode).await
    }

    /// Links two existing calls of one account into a consult session (A original, B consult).
    pub(crate) async fn consult_link(
        self: &Arc<Self>,
        id: &str,
        consult_id: &str,
    ) -> Result<TransferSession> {
        let a = self.get(id)?;
        let b = self.get(consult_id)?;
        if a.id == b.id {
            return Err(SipError::InvalidArgument(
                "the consult call must differ from the original call".into(),
            ));
        }
        if a.account != b.account {
            return Err(SipError::InvalidArgument(
                "both calls must belong to the same account".into(),
            ));
        }
        a.ensure_established()?;
        if !matches!(
            b.snapshot().state,
            CallState::Dialing
                | CallState::Early
                | CallState::Active
                | CallState::Held
                | CallState::RemoteHeld
        ) {
            return Err(b.state_err("the consult call is not established or dialing"));
        }
        let _resv = self.reserve(&[id, consult_id])?;
        let target = b.dialed_uri();
        let s = Self::new_session(
            TransferKind::Attended,
            TransferPhase::ConsultDialing,
            &a,
            Some(b.id.clone()),
            target,
            false,
        );
        let s = self.create_session(s, false)?;
        let s = self.sync_consult(&s);
        if s.phase == TransferPhase::ConsultConnected {
            // The leg the user is on is the un-held one; none when both are held.
            let on = |c: &Arc<Call>| !c.lock().info.hold.local;
            let tt = if on(&b) {
                Some(b.id.clone())
            } else if on(&a) {
                Some(a.id.clone())
            } else {
                None
            };
            self.update_session(&s.id, |s| s.talking_to = tt);
        }
        Ok(self.transfer(&s.id).unwrap_or(s))
    }

    /// Holds `call_id` and dials `target` on its account (source / sink default to the
    /// original's); both calls are linked in a session.
    pub(crate) async fn consult_start(
        self: &Arc<Self>,
        call_id: &str,
        target: &str,
        opts: DialOptions,
    ) -> Result<TransferSession> {
        let (this, id, target) = (self.clone(), call_id.to_string(), target.to_string());
        detached(async move { this.consult_start_inner(&id, &target, opts).await }).await
    }

    async fn consult_start_inner(
        self: &Arc<Self>,
        call_id: &str,
        target: &str,
        opts: DialOptions,
    ) -> Result<TransferSession> {
        let a = self.get(call_id)?;
        a.ensure_established()?;
        let uri = target_uri(&a.live_cfg(), target)?;
        let _resv = self.reserve(&[call_id])?;
        let (src, snk) = {
            let g = a.lock();
            (g.source.clone(), g.sink.clone())
        };
        let mut opts = opts;
        let source = opts.source.get_or_insert(src).clone();
        let sink = opts.sink.get_or_insert(snk).clone();
        // Only device-bound calls take the coordinator; agents and automation never wait for it.
        let device = a.device_bound() || source == AudioSource::Device || sink == AudioSink::Device;
        let _line = if device {
            Some(self.lines.lock().await)
        } else {
            None
        };
        a.ensure_established()?;
        // A phone holds the original before the consult, whatever it is connected to.
        let held = a.hold_for_transfer().await;
        let b_id = match self.dial_inner(&a.env, target, opts, None, false).await {
            Ok(b) => b,
            Err(e) => {
                if held && !a.is_ended() {
                    let _ = Self::hold_line(&a, false, true).await;
                    if a.auto_holdable() {
                        a.focus_media();
                    }
                }
                return Err(e);
            }
        };
        if a.is_ended() {
            // The original ended while we held it and dialled: no session for a dead call.
            if let Ok(b) = self.get(&b_id) {
                let _ = b.hangup_now();
            }
            return Err(a.state_err("the original call ended during the consult"));
        }
        let s = Self::new_session(
            TransferKind::Attended,
            TransferPhase::ConsultDialing,
            &a,
            Some(b_id.clone()),
            uri.to_string(),
            held,
        );
        let s = match self.create_session(s, false) {
            Ok(s) => s,
            Err(e) => {
                if let Ok(b) = self.get(&b_id) {
                    let _ = b.hangup_now();
                }
                return Err(e);
            }
        };
        Ok(self.sync_consult(&s))
    }

    /// Completes the consult: REFER with Replaces (`Attended`) or end the consult call and
    /// refer the original to its target (`BlindNow`). Idempotent once the REFER was claimed.
    pub(crate) async fn transfer_complete(
        self: &Arc<Self>,
        sid: &str,
        mode: CompleteMode,
    ) -> Result<TransferSession> {
        use TransferPhase::*;
        let claim = self.update_session(sid, |s| -> Result<bool> {
            let orig = s.original.clone();
            let bad = |msg: &str| SipError::CallState {
                call: orig.clone(),
                msg: msg.to_string(),
            };
            match (s.phase, mode) {
                (Referring | Transferring | Unconfirmed, _) => Ok(false),
                (ConsultDialing | ConsultRinging, CompleteMode::Attended) => {
                    Err(bad("consult not answered; use blind_now"))
                }
                (ConsultFailed, _) => Err(bad(
                    "the consult call has ended; cancel the transfer and start again",
                )),
                (ConsultDialing | ConsultRinging | ConsultConnected, m) => {
                    if m == CompleteMode::BlindNow {
                        s.kind = TransferKind::Semi;
                    }
                    s.phase = Referring;
                    s.last_error = None;
                    Ok(true)
                }
                _ => Err(bad("transfer is over")),
            }
        });
        let Some(claim) = claim else {
            // Not live: a finished session answers with its result.
            let s = self.transfer(sid)?;
            return if s.phase == Transferred {
                Ok(s)
            } else {
                Err(SipError::CallState {
                    call: s.original,
                    msg: format!("transfer is over ({})", s.phase.as_str()),
                })
            };
        };
        if !claim? {
            return self.transfer(sid);
        }
        let (this, id) = (self.clone(), sid.to_string());
        detached(async move { this.run_complete(&id, mode).await }).await?;
        self.transfer(sid)
    }

    async fn run_complete(self: &Arc<Self>, sid: &str, mode: CompleteMode) -> Result<()> {
        let mut guard = ReferGuard::new(self, sid);
        let s = self.transfer(sid)?;
        let gone = |what: &str| SipError::CallState {
            call: s.original.clone(),
            msg: what.to_string(),
        };
        let a = match self.get(&s.original) {
            Ok(a) if !a.is_ended() => a,
            _ => {
                self.refer_failed(sid, "refer", 0, "the original call is over")
                    .await;
                return Err(gone("the original call is over"));
            }
        };
        let b = s.consult.as_deref().and_then(|b| self.get(b).ok());
        let Some(b) = b else {
            self.refer_failed(sid, "refer", 0, "the consult call is gone")
                .await;
            return Err(gone("the consult call is gone"));
        };
        let device = a.device_bound() || b.device_bound();
        match mode {
            CompleteMode::BlindNow => {
                // The dialled target (the dialog's To), not the identity the far end asserted.
                let target = b.dialed_uri();
                let held = {
                    let _line = if device {
                        Some(self.lines.lock().await)
                    } else {
                        None
                    };
                    let _ = b.hangup_now();
                    a.hold_for_transfer().await
                };
                self.note_we_held(sid, held);
                guard.armed = false;
                a.start_refer(format!("<{target}>"), None, sid.to_string())
                    .await
            }
            CompleteMode::Attended => {
                let cfg = a.live_cfg();
                let (refer_call, target_call) = match cfg.attended_refer_on {
                    AttendedReferOn::Original => (a.clone(), b.clone()),
                    AttendedReferOn::Consult => (b.clone(), a.clone()),
                };
                let held = {
                    let _line = if device {
                        Some(self.lines.lock().await)
                    } else {
                        None
                    };
                    a.hold_for_transfer().await
                };
                self.note_we_held(sid, held);
                let refer_to = match target_call.attended_refer_to(cfg.attended_refer_target) {
                    Ok(r) => r,
                    Err(e) => {
                        self.refer_failed(sid, "refer", 0, &e.to_string()).await;
                        return Err(e);
                    }
                };
                guard.armed = false;
                refer_call
                    .start_refer(refer_to, Some(target_call), sid.to_string())
                    .await
            }
        }
    }

    /// Switches the user between A and B during the consult: holds the current leg, resumes the
    /// other and gives it the media focus. A failed hold aborts with `last_error` set.
    pub(crate) async fn transfer_swap(self: &Arc<Self>, sid: &str) -> Result<TransferSession> {
        let (this, id) = (self.clone(), sid.to_string());
        detached(async move { this.transfer_swap_inner(&id).await }).await
    }

    /// Resumes `leg`; a device-bound leg first puts the other un-held device lines on hold (the
    /// way `activate` does), so a resume never leaves two live lines. The caller holds the line
    /// coordinator.
    async fn resume_leg(self: &Arc<Self>, leg: &Arc<Call>) -> Result<()> {
        if leg.device_bound() {
            let others = self.hold_targets(&leg.account, Some(&leg.id), true);
            Self::hold_all(others, true).await?;
        }
        Self::hold_line(leg, false, true).await
    }

    async fn transfer_swap_inner(self: &Arc<Self>, sid: &str) -> Result<TransferSession> {
        let s = self.transfer(sid)?;
        let bad = |msg: String| SipError::CallState {
            call: s.original.clone(),
            msg,
        };
        if s.phase != TransferPhase::ConsultConnected {
            return Err(bad(format!(
                "swap needs an answered consult (phase {})",
                s.phase.as_str()
            )));
        }
        let (Ok(a), Some(Ok(b))) = (
            self.get(&s.original),
            s.consult.as_deref().map(|b| self.get(b)),
        ) else {
            return Err(bad("a call of the transfer is gone".into()));
        };
        let device = a.device_bound() || b.device_bound();
        let _line = if device {
            Some(self.lines.lock().await)
        } else {
            None
        };
        // The wait may have been long: decide on the state as it is now.
        let Some(s) = self
            .live_session(sid)
            .filter(|s| s.phase == TransferPhase::ConsultConnected)
        else {
            return Err(bad("the transfer moved on".into()));
        };
        let on_b = s.talking_to.as_deref() != Some(a.id.as_str());
        let (cur, other) = if on_b { (&b, &a) } else { (&a, &b) };
        let fail = |e: SipError, what: &str| {
            self.update_session(sid, |s| {
                s.last_error = Some(err("swap", 0, format!("{what}: {e}")));
            });
            e
        };
        let _pending = cur.set_pending(PendingOp::Hold);
        if !cur.lock().info.hold.local {
            Self::hold_line(cur, true, true)
                .await
                .map_err(|e| fail(e, "cannot hold the current line"))?;
        }
        if other.lock().info.hold.local {
            let _p = other.set_pending(PendingOp::Resume);
            if let Err(e) = self.resume_leg(other).await {
                // Do not leave both lines held: put the user back where they were.
                let _ = self.resume_leg(cur).await;
                return Err(fail(e, "cannot resume the other line"));
            }
        }
        if other.is_ended() {
            return Err(fail(other.state_err("call is over"), "swap"));
        }
        other.focus_media();
        let other_id = other.id.clone();
        self.update_session(sid, |s| {
            s.talking_to = Some(other_id);
            s.last_error = None;
        });
        self.transfer(sid)
    }

    /// Abandons the consult: hangs up B, resumes A when the session held it, focuses A. A
    /// no-op for a finished session; refused once the REFER was handed over.
    pub(crate) async fn transfer_cancel(self: &Arc<Self>, sid: &str) -> Result<TransferSession> {
        let (this, id) = (self.clone(), sid.to_string());
        detached(async move { this.transfer_cancel_inner(&id).await }).await
    }

    async fn transfer_cancel_inner(self: &Arc<Self>, sid: &str) -> Result<TransferSession> {
        use TransferPhase::*;
        let claim = self.update_session(sid, |s| -> Result<()> {
            match s.phase {
                Referring | Transferring | Unconfirmed => Err(SipError::CallState {
                    call: s.original.clone(),
                    msg: "transfer already handed to the PBX".into(),
                }),
                _ => {
                    s.phase = Cancelled;
                    Ok(())
                }
            }
        });
        match claim {
            None => return self.transfer(sid),
            Some(Err(e)) => return Err(e),
            Some(Ok(())) => {}
        }
        let s = self.transfer(sid)?;
        let a = self.get(&s.original).ok();
        let b = s.consult.as_deref().and_then(|b| self.get(b).ok());
        let device = a.as_ref().is_some_and(|a| a.device_bound())
            || b.as_ref().is_some_and(|b| b.device_bound());
        let _line = if device {
            Some(self.lines.lock().await)
        } else {
            None
        };
        if let Some(b) = b.filter(|b| !b.is_ended()) {
            let _ = b.hangup_now();
        }
        if let Some(a) = a {
            let held = {
                let g = a.lock();
                g.info.state != CallState::Ended && g.info.hold.local
            };
            if s.we_held_original && held {
                let _pending = a.set_pending(PendingOp::Resume);
                if let Err(e) = self.resume_leg(&a).await {
                    tracing::warn!(call = %a.id, error = %e, "cannot resume the original call");
                    // The session is over already: only the ring entry can carry the note.
                    let mut st = self.sess();
                    if let Some(d) = st.done.iter_mut().find(|d| d.id == sid) {
                        d.last_error = Some(err("resume", 0, e.to_string()));
                    }
                }
            }
            if a.auto_holdable() {
                a.focus_media();
            }
        }
        self.transfer(sid)
    }
}

//! Local call tones, driven by call events.
//!
//! * ringtone on the ringer while at least one device-bound inbound call is `Ringing` and no
//!   device-bound call is `Active`;
//! * call-waiting beep on the speaker (repeated every 3 s) instead, when a device-bound call is
//!   `Active`;
//! * ringback on the speaker for a device-bound outbound call in `Early` without early media;
//! * busy tone (2 s) when a device-bound outbound call fails with 486 / 600, reorder (2 s) for
//!   other failures (>= 400, not 487).
//!
//! Calls that are not device-bound (test calls with `tone:` / `file:` sources and `null` /
//! `record:` sinks) are silent locally.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use siple_sip::{CallDirection, CallInfo, CallState};
use tokio_util::sync::CancellationToken;

use crate::media::{LocalTone, LocalToneKind, MediaEngine, ToneId, ToneTarget};

/// Repeat period of the call-waiting beep.
pub const CALL_WAITING_PERIOD: Duration = Duration::from_secs(3);
/// How long busy / reorder play.
pub const FAILURE_TONE_DURATION: Duration = Duration::from_secs(2);

/// Where tones go. Implemented by the [`MediaEngine`]; tests use a recorder.
pub trait TonePlayer: Send + Sync + 'static {
    /// Starts a tone and returns its id.
    fn start(&self, tone: LocalTone) -> ToneId;
    /// Stops a tone (no-op if it already ended).
    fn stop(&self, id: ToneId);
}

impl TonePlayer for MediaEngine {
    fn start(&self, tone: LocalTone) -> ToneId {
        self.start_local_tone(tone)
    }
    fn stop(&self, id: ToneId) {
        self.stop_local_tone(id);
    }
}

struct Tracked {
    direction: CallDirection,
    state: CallState,
    early_media: bool,
    device_bound: bool,
}

#[derive(Default)]
struct State {
    calls: HashMap<String, Tracked>,
    ringtone: Option<ToneId>,
    ringback: Option<ToneId>,
    /// Cancels the call-waiting repeat task.
    call_waiting: Option<CancellationToken>,
}

/// Turns call state changes into local tones. Cheap to clone.
#[derive(Clone)]
pub struct ToneController {
    player: Arc<dyn TonePlayer>,
    state: Arc<Mutex<State>>,
}

impl ToneController {
    /// New controller playing on `player`. Must be used inside a tokio runtime (timed tones
    /// spawn tasks).
    pub fn new(player: Arc<dyn TonePlayer>) -> Self {
        Self {
            player,
            state: Arc::default(),
        }
    }

    /// Feeds a call snapshot (incoming / state / updated events). `device_bound` is whether the
    /// call's source or sink is the device.
    pub fn on_call(&self, info: &CallInfo, device_bound: bool) {
        let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let prev = st.calls.get(&info.id).map(|t| t.state);
        if info.state == CallState::Ended {
            st.calls.remove(&info.id);
            if let Some(kind) = failure_tone(info, prev, device_bound) {
                self.play_timed(kind);
            }
        } else {
            st.calls.insert(
                info.id.clone(),
                Tracked {
                    direction: info.direction,
                    state: info.state,
                    early_media: info.early_media,
                    device_bound,
                },
            );
        }
        self.reconcile(&mut st);
    }

    /// Stops every tone and forgets all calls (shutdown).
    pub fn stop_all(&self) {
        let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        st.calls.clear();
        self.reconcile(&mut st);
    }

    fn reconcile(&self, st: &mut State) {
        let device = |t: &&Tracked| t.device_bound;
        let ringing = st
            .calls
            .values()
            .filter(device)
            .any(|t| t.direction == CallDirection::Inbound && t.state == CallState::Ringing);
        let active = st
            .calls
            .values()
            .filter(device)
            .any(|t| t.state == CallState::Active);
        let ringback = st.calls.values().filter(device).any(|t| {
            t.direction == CallDirection::Outbound && t.state == CallState::Early && !t.early_media
        });

        set_loop(
            &mut st.ringtone,
            ringing && !active,
            &*self.player,
            LocalTone {
                kind: LocalToneKind::Ringtone,
                target: ToneTarget::Ringer,
            },
        );
        set_loop(
            &mut st.ringback,
            ringback,
            &*self.player,
            LocalTone {
                kind: LocalToneKind::Ringback,
                target: ToneTarget::Speaker,
            },
        );

        let want_cw = ringing && active;
        match (&st.call_waiting, want_cw) {
            (None, true) => {
                let token = CancellationToken::new();
                st.call_waiting = Some(token.clone());
                let player = self.player.clone();
                tokio::spawn(async move {
                    loop {
                        player.start(LocalTone {
                            kind: LocalToneKind::CallWaiting,
                            target: ToneTarget::Speaker,
                        });
                        tokio::select! {
                            _ = token.cancelled() => return,
                            _ = tokio::time::sleep(CALL_WAITING_PERIOD) => {}
                        }
                    }
                });
            }
            (Some(token), false) => {
                token.cancel();
                st.call_waiting = None;
            }
            _ => {}
        }
    }

    /// Plays a repeating tone on the speaker for [`FAILURE_TONE_DURATION`].
    fn play_timed(&self, kind: LocalToneKind) {
        let id = self.player.start(LocalTone {
            kind,
            target: ToneTarget::Speaker,
        });
        let player = self.player.clone();
        tokio::spawn(async move {
            tokio::time::sleep(FAILURE_TONE_DURATION).await;
            player.stop(id);
        });
    }
}

fn set_loop(slot: &mut Option<ToneId>, want: bool, player: &dyn TonePlayer, tone: LocalTone) {
    match (*slot, want) {
        (None, true) => *slot = Some(player.start(tone)),
        (Some(id), false) => {
            player.stop(id);
            *slot = None;
        }
        _ => {}
    }
}

/// Busy / reorder for a failed device-bound outbound call.
fn failure_tone(
    info: &CallInfo,
    prev: Option<CallState>,
    device_bound: bool,
) -> Option<LocalToneKind> {
    if !device_bound || info.direction != CallDirection::Outbound || info.answered_at.is_some() {
        return None;
    }
    // Only calls we saw alive; a call that was never tracked as pending cannot have failed.
    prev?;
    let code = info.end.as_ref()?.code;
    match code {
        486 | 600 => Some(LocalToneKind::Busy),
        c if c >= 400 && c != 487 => Some(LocalToneKind::Reorder),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    use chrono::Utc;
    use siple_sip::{CallEnd, EndedBy, HoldFlags};

    #[derive(Default)]
    struct Rec {
        next: AtomicU64,
        log: Mutex<Vec<String>>,
    }

    impl Rec {
        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }

    impl TonePlayer for Rec {
        fn start(&self, tone: LocalTone) -> ToneId {
            let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
            self.log
                .lock()
                .unwrap()
                .push(format!("start {:?} {:?} #{id}", tone.kind, tone.target));
            ToneId(id)
        }
        fn stop(&self, id: ToneId) {
            self.log.lock().unwrap().push(format!("stop #{}", id.0));
        }
    }

    fn info(id: &str, dir: CallDirection, state: CallState) -> CallInfo {
        CallInfo {
            id: id.into(),
            account: "a".into(),
            direction: dir,
            state,
            remote_uri: "sip:x@y".into(),
            remote_display: None,
            local_uri: "sip:a@y".into(),
            sip_call_id: "sip".into(),
            created_at: Utc::now(),
            answered_at: None,
            ended_at: None,
            end: None,
            hold: HoldFlags::default(),
            hold_changed_at: None,
            pending: None,
            muted: false,
            negotiated: None,
            early_media: false,
            alert_info: None,
            pai: None,
            rpid: None,
            srtp_fallback: false,
            transferred_from: None,
            replaces: None,
            transfer: None,
            reconnecting: false,
        }
    }

    fn ended(mut i: CallInfo, code: u16) -> CallInfo {
        i.state = CallState::Ended;
        i.end = Some(CallEnd {
            code,
            reason: "x".into(),
            by: EndedBy::Remote,
            missed: false,
            q850_cause: None,
            cause: siple_sip::EndCause::RemoteHangup,
        });
        i
    }

    fn rig() -> (Arc<Rec>, ToneController) {
        let rec = Arc::new(Rec::default());
        (rec.clone(), ToneController::new(rec))
    }

    #[tokio::test(start_paused = true)]
    async fn ringtone_while_ringing_only() {
        let (rec, tc) = rig();
        let ring = info("c1", CallDirection::Inbound, CallState::Ringing);
        tc.on_call(&ring, true);
        tc.on_call(&ring, true); // repeated snapshot: no second start
        assert_eq!(rec.log(), ["start Ringtone Ringer #1"]);
        tc.on_call(&info("c1", CallDirection::Inbound, CallState::Active), true);
        assert_eq!(rec.log(), ["start Ringtone Ringer #1", "stop #1"]);
    }

    #[tokio::test(start_paused = true)]
    async fn non_device_calls_are_silent() {
        let (rec, tc) = rig();
        tc.on_call(
            &info("c1", CallDirection::Inbound, CallState::Ringing),
            false,
        );
        let mut early = info("c2", CallDirection::Outbound, CallState::Early);
        tc.on_call(&early, false);
        early = ended(early, 486);
        tc.on_call(&early, false);
        assert!(rec.log().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn call_waiting_repeats_until_answered() {
        let (rec, tc) = rig();
        tc.on_call(
            &info("c1", CallDirection::Outbound, CallState::Active),
            true,
        );
        tc.on_call(
            &info("c2", CallDirection::Inbound, CallState::Ringing),
            true,
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(rec.log(), ["start CallWaiting Speaker #1"]);
        tokio::time::sleep(Duration::from_secs(6)).await;
        assert_eq!(rec.log().len(), 3, "{:?}", rec.log());
        tc.on_call(&info("c2", CallDirection::Inbound, CallState::Active), true);
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert_eq!(rec.log().len(), 3, "stopped after answer: {:?}", rec.log());
    }

    #[tokio::test(start_paused = true)]
    async fn ringback_until_active_or_early_media() {
        let (rec, tc) = rig();
        tc.on_call(
            &info("c1", CallDirection::Outbound, CallState::Dialing),
            true,
        );
        assert!(rec.log().is_empty());
        let early = info("c1", CallDirection::Outbound, CallState::Early);
        tc.on_call(&early, true);
        assert_eq!(rec.log(), ["start Ringback Speaker #1"]);
        let mut em = early.clone();
        em.early_media = true;
        tc.on_call(&em, true);
        assert_eq!(rec.log().last().unwrap(), "stop #1");
        // Early media then active: no new ringback.
        tc.on_call(
            &info("c1", CallDirection::Outbound, CallState::Active),
            true,
        );
        assert_eq!(rec.log().len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn failure_tones() {
        let (rec, tc) = rig();
        for (id, code) in [("c1", 486), ("c2", 603), ("c3", 487), ("c4", 600)] {
            let c = info(id, CallDirection::Outbound, CallState::Dialing);
            tc.on_call(&c, true);
            tc.on_call(&ended(c, code), true);
        }
        assert_eq!(
            rec.log(),
            [
                "start Busy Speaker #1",
                "start Reorder Speaker #2",
                "start Busy Speaker #3"
            ]
        );
        tokio::time::sleep(Duration::from_millis(2100)).await;
        let l = rec.log();
        assert!(l.contains(&"stop #1".to_string()) && l.contains(&"stop #2".to_string()));
        assert!(l.contains(&"stop #3".to_string()));
    }
}

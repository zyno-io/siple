//! Moving established calls onto a new network path.
//!
//! After the account actor rebound its transport in place (new local address), every
//! established call of the account is moved: its RTP sockets are rebound to the new address and a
//! re-INVITE announces the new Contact and SDP `c=` / `m=` port. The hold direction and the SDES
//! keys are kept (`MediaSession::rebind`). The move uses the per-call re-INVITE slot
//! (`reinvite_out`) and the 491 glare rules of hold and session refresh; it never takes the
//! coordinator's `lines` lock, so it cannot deadlock against a line operation.
//!
//! A call that cannot be moved ends with [`EndCause::NetworkLost`]: the peer answered 481 / 408
//! or refused the re-INVITE, gave no final answer within [`MOVE_ATTEMPT`], or the media cannot be
//! rebound. Calls that are not established yet are left alone.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use super::{Call, CallState, EndCause, ReinviteGuard, RtEnv, answer_sdp, offer_sdp, util};
use crate::types::SipEvent;

/// Test hook: overrides [`MOVE_ATTEMPT`] (milliseconds, 0 = default). Process-wide.
pub(crate) static MOVE_ATTEMPT_MS: AtomicU64 = AtomicU64::new(0);
/// Longest wait for the final answer to one move re-INVITE (plan: "no answer within 10 s").
const MOVE_ATTEMPT: Duration = Duration::from_secs(10);

fn move_attempt() -> Duration {
    match MOVE_ATTEMPT_MS.load(Ordering::Relaxed) {
        0 => MOVE_ATTEMPT,
        ms => Duration::from_millis(ms),
    }
}
/// Overall budget of a move: waiting for a re-INVITE of the call that is in flight, the attempts
/// and the 491 back-off.
const MOVE_BUDGET: Duration = Duration::from_secs(40);
/// A 491 retry that cannot get at least this much of the budget is not made.
const MOVE_MIN_ATTEMPT: Duration = Duration::from_secs(3);

/// How a move ended unsuccessfully.
enum MoveEnd {
    Lost(String),
    Stale,
}

/// Outcome of one move attempt.
enum Try {
    /// A newer network generation exists: this move is obsolete and must not touch the call.
    Stale,
    /// 491 Request Pending: back off and try again.
    Glare,
    /// The call cannot be moved.
    Lost(String),
}

impl Call {
    /// The call is established (answered, not ended): the only calls a network change moves.
    pub(crate) fn is_established(&self) -> bool {
        matches!(
            self.lock().info.state,
            CallState::Active | CallState::Held | CallState::RemoteHeld
        )
    }

    /// A move of this call is in progress.
    pub(crate) fn is_reconnecting(&self) -> bool {
        self.lock().info.reconnecting
    }

    /// Waits until no move is in progress (or the call is over, or `deadline` passes). Hold /
    /// resume wait for a move instead of failing on the busy re-INVITE slot.
    pub(crate) async fn wait_not_reconnecting(&self, deadline: tokio::time::Instant) {
        while self.is_reconnecting() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Ends the call because the network path is gone (cause `network_lost`). A best-effort BYE
    /// goes out in the background (it may well not arrive). Returns whether this call ended it.
    pub(crate) fn end_network_lost(self: &Arc<Self>, why: &str) -> bool {
        tracing::warn!(call = %self.id, %why, "call lost with the network");
        let dialog = {
            let mut g = self.lock();
            g.local_hangup = true;
            g.dialog.clone()
        };
        let ended = self.local_end(0, format!("network lost: {why}"), EndCause::NetworkLost);
        if ended && let Some(d) = dialog {
            self.mgr.spawn_bg(&self.account, async move {
                let _ = tokio::time::timeout(
                    Duration::from_secs(3),
                    d.bye_with_headers(Some(vec![rsipstack::sip::Header::Other(
                        "Reason".into(),
                        "SIP;cause=408;text=\"Network lost\"".into(),
                    )])),
                )
                .await;
            });
        }
        ended
    }

    /// Moves this call onto the transport generation `rt` (the account's new local address):
    /// rebinds the media, re-INVITEs with the new Contact and `c=`, keeps hold and SRTP. Ends the
    /// call with `network_lost` when that fails. Never moves a call that is not established.
    ///
    /// `seq` is the account's rebind sequence the move belongs to; `rebind = false` only
    /// re-announces the Contact (current SDP, media untouched).
    pub(crate) async fn move_to(self: Arc<Self>, rt: Arc<RtEnv>, seq: u64, rebind: bool) {
        if !self.is_established() {
            return;
        }
        let deadline = tokio::time::Instant::now() + MOVE_BUDGET;
        // A move that is still running (an earlier network change) finishes first: this one
        // moves the call again, onto the newest `rt`. The generation tells the older one that it
        // was overtaken, so its failure (it may sit on a path that is gone) does not end the call.
        let my_gen = {
            let mut g = self.lock();
            g.move_gen += 1;
            if rebind {
                g.move_pending = true;
            }
            g.move_gen
        };
        loop {
            {
                let mut g = self.lock();
                if g.info.state == CallState::Ended {
                    return;
                }
                if g.move_gen != my_gen {
                    return; // a newer move took over while this one waited
                }
                if !g.info.reconnecting {
                    g.info.reconnecting = true;
                    // Under the lock, like `call.reconnected`: never after `Ended`.
                    self.emit(SipEvent::CallUpdated {
                        call: g.info.clone(),
                    });
                    self.emit(SipEvent::CallReconnecting {
                        call_id: self.id.clone(),
                        account: self.account.clone(),
                    });
                    break;
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tracing::info!(call = %self.id, bind = %rt.bind_ip, advertise = %rt.advertise_ip, "moving the call onto the new network path");
        let outcome = tokio::select! {
            r = self.clone().move_loop(rt, deadline, my_gen, rebind) => r,
            () = self.wait_ended() => return,
        };
        match outcome {
            Ok(()) => {
                let mut g = self.lock();
                if g.info.state == CallState::Ended {
                    return;
                }
                g.info.reconnecting = false;
                if g.move_seq == seq {
                    g.move_pending = false;
                }
                // Both events go out under the lock: `end()` cannot slip in between, so
                // `call.reconnected` never follows the `Ended` state.
                self.emit(SipEvent::CallUpdated {
                    call: g.info.clone(),
                });
                self.emit(SipEvent::CallReconnected {
                    call_id: self.id.clone(),
                    account: self.account.clone(),
                });
                drop(g);
                tracing::info!(call = %self.id, "call moved");
            }
            Err(end) => {
                let (superseded, stale) = {
                    let mut g = self.lock();
                    let stale = matches!(end, MoveEnd::Stale);
                    let superseded = g.move_gen != my_gen;
                    if superseded || stale {
                        // A newer move (waiting for this one) takes over, or the call already
                        // runs on the newest generation.
                        g.info.reconnecting = false;
                    }
                    if stale && g.move_seq == seq {
                        g.move_pending = false;
                    }
                    (superseded, stale)
                };
                match end {
                    MoveEnd::Stale => {
                        tracing::info!(call = %self.id, "move obsolete: a newer network change followed");
                    }
                    MoveEnd::Lost(why) if superseded => {
                        tracing::info!(call = %self.id, %why, "move overtaken by a newer network change");
                    }
                    MoveEnd::Lost(why) => {
                        let _ = stale;
                        // The call ends: nothing left to move.
                        self.end_network_lost(&why);
                    }
                }
            }
        }
    }

    async fn move_loop(
        self: Arc<Self>,
        rt: Arc<RtEnv>,
        deadline: tokio::time::Instant,
        my_gen: u64,
        rebind: bool,
    ) -> Result<(), MoveEnd> {
        let mut rebound = !rebind;
        let mut retries = 0u8;
        loop {
            if self.lock().move_gen != my_gen {
                return Err(MoveEnd::Lost("overtaken by a newer move".into()));
            }
            match self.move_once(&rt, deadline, &mut rebound).await {
                Ok(()) => return Ok(()),
                Err(Try::Lost(why)) => return Err(MoveEnd::Lost(why)),
                Err(Try::Stale) => return Err(MoveEnd::Stale),
                Err(Try::Glare) => {
                    if retries >= 2 {
                        return Err(MoveEnd::Lost(
                            "re-INVITE glare (491) did not resolve".into(),
                        ));
                    }
                    retries += 1;
                    let ms: u64 = if self.direction == super::CallDirection::Outbound {
                        rand::random_range(2100..=4000)
                    } else {
                        rand::random_range(0..=2000)
                    };
                    if tokio::time::Instant::now() + Duration::from_millis(ms) + MOVE_MIN_ATTEMPT
                        > deadline
                    {
                        return Err(MoveEnd::Lost("re-INVITE glare (491); out of time".into()));
                    }
                    tracing::debug!(call = %self.id, ms, retries, "move re-INVITE glare; retrying");
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                }
            }
        }
    }

    /// One attempt, in its own task: it always finishes (applies the answer or reports the
    /// failure) even when the caller's future is dropped, like `hold_once`.
    async fn move_once(
        self: &Arc<Self>,
        rt: &Arc<RtEnv>,
        deadline: tokio::time::Instant,
        rebound: &mut bool,
    ) -> Result<(), Try> {
        let call = self.clone();
        let rt = rt.clone();
        let was_rebound = *rebound;
        let (res, did_rebind) =
            tokio::spawn(async move { call.move_exchange(rt, deadline, was_rebound).await })
                .await
                .unwrap_or_else(|e| (Err(Try::Lost(format!("move task failed: {e}"))), false));
        *rebound |= did_rebind;
        res
    }

    async fn move_exchange(
        self: &Arc<Self>,
        rt: Arc<RtEnv>,
        deadline: tokio::time::Instant,
        already_rebound: bool,
    ) -> (Result<(), Try>, bool) {
        let mut did_rebind = false;
        let res = self
            .move_exchange_inner(&rt, deadline, already_rebound, &mut did_rebind)
            .await;
        (res, did_rebind)
    }

    async fn move_exchange_inner(
        self: &Arc<Self>,
        rt: &Arc<RtEnv>,
        deadline: tokio::time::Instant,
        already_rebound: bool,
        did_rebind: &mut bool,
    ) -> Result<(), Try> {
        let lost = |m: &str| Try::Lost(m.to_string());
        // The re-INVITE slot: another re-INVITE of this call (hold, session refresh) finishes
        // first. Taken atomically, so a hold that starts meanwhile waits for us instead.
        let dialog = loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(lost("a re-INVITE of the call never finished"));
            }
            let d = self
                .confirmed_dialog()
                .await
                .map_err(|e| Try::Lost(e.to_string()))?;
            let taken = {
                let mut g = self.lock();
                if g.info.state == CallState::Ended {
                    return Err(lost("call is over"));
                }
                if g.reinvite_out {
                    false
                } else {
                    g.reinvite_out = true;
                    true
                }
            };
            if taken {
                break d;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let _guard = ReinviteGuard(self.clone());
        // A newer network generation may have appeared while this move waited for the slot:
        // then this one is obsolete (it would move the call back onto a path that is gone).
        if let Some(latest) = self.env.env().and_then(|e| e.rt)
            && !latest.same_generation(rt)
        {
            return Err(Try::Stale);
        }
        let (sdp, headers) = {
            let mut g = self.lock();
            if !matches!(
                g.info.state,
                CallState::Active | CallState::Held | CallState::RemoteHeld
            ) {
                return Err(lost("call is not established"));
            }
            let Some(media) = g.media.clone() else {
                return Err(lost("no media"));
            };
            if !already_rebound {
                match media.rebind(rt.bind_ip, rt.advertise_ip) {
                    Ok(port) => {
                        *did_rebind = true;
                        let Some(session) = g.session.as_mut() else {
                            return Err(lost("no media session"));
                        };
                        session.rebind(rt.advertise_ip, port);
                    }
                    Err(e) => return Err(Try::Lost(format!("media cannot be rebound: {e}"))),
                }
            }
            // The call now lives on the new generation: its Contact, and (TCP / TLS) a fresh flow.
            g.rt = Some(rt.clone());
            let registered = rt
                .contact
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            g.fallback_contact = registered.is_none();
            let c = registered.unwrap_or_else(|| fallback_contact(&self.cfg.username, rt));
            dialog.set_local_contact(c.clone());
            g.local_contact = Some(c);
            dialog.clear_flow_affinity();
            let remote_holds = g.info.hold.remote;
            let Some(session) = g.session.as_mut() else {
                return Err(lost("no media session"));
            };
            let sdp = offer_sdp(session, remote_holds);
            let mut headers = util::sdp_headers(&self.cfg);
            headers.extend(self.timer_request_headers(&g));
            (sdp, headers)
        };
        let bound = deadline
            .saturating_duration_since(tokio::time::Instant::now())
            .min(move_attempt());
        let res = tokio::select! {
            r = self.reinvite_bounded(&dialog, headers, Some(sdp.into_bytes()), bound) => r,
            () = self.wait_ended() => return Err(lost("call is over")),
        };
        let resp = match res {
            Ok(Some(r)) => r,
            Ok(None) => return Err(lost("re-INVITE was not sent: dialog is not confirmed")),
            Err(e) => return Err(Try::Lost(format!("re-INVITE failed: {e}"))),
        };
        let code = resp.status_code.code();
        if code == 491 {
            return Err(Try::Glare);
        }
        if !(200..300).contains(&code) {
            // 481 / 408 (also the synthetic 408 of an unanswered re-INVITE) and any refusal: the
            // peer does not follow us to the new path.
            return Err(Try::Lost(format!(
                "re-INVITE answered {code} {}",
                resp.status_code.text()
            )));
        }
        let mut g = self.lock();
        if g.info.state == CallState::Ended {
            return Err(lost("call is over"));
        }
        let sd = match util::sdp_body(&resp.headers, &resp.body) {
            util::SdpBody::Sdp(_, sd) => sd,
            _ => return Err(lost("re-INVITE answer without SDP")),
        };
        let remote_holds = g.info.hold.remote;
        let Some(session) = g.session.as_mut() else {
            return Err(lost("no media session"));
        };
        let neg = answer_sdp(session, remote_holds, &sd)
            .map_err(|e| Try::Lost(format!("unusable answer SDP: {e}")))?;
        let before = g.info.hold;
        self.apply_negotiated_quiet(&mut g, neg);
        self.announce_hold(&mut g, before);
        // A re-INVITE refreshes the session.
        if g.timer.is_some() {
            drop(g);
            self.touch_timer();
        }
        Ok(())
    }
}

/// Contact for a call that moves before the re-REGISTER told us the flow's address (client-only
/// TCP / TLS): the new local address, so the peer is at least not told the old one. The port is
/// unknown until the connection exists; the Via `alias` keeps the peer on our connection.
fn fallback_contact(user: &str, rt: &RtEnv) -> rsipstack::sip::Uri {
    contact_for(
        user,
        rt.public_host.as_ref(),
        rt.bind_ip,
        rt.transport,
        rt.use_sips,
    )
}

fn contact_for(
    user: &str,
    public_host: Option<&rsipstack::sip::Host>,
    bind_ip: std::net::IpAddr,
    transport: rsipstack::sip::Transport,
    use_sips: bool,
) -> rsipstack::sip::Uri {
    let host = public_host
        .cloned()
        .unwrap_or(rsipstack::sip::Host::IpAddr(bind_ip));
    crate::client::build_contact(
        user,
        rsipstack::sip::HostWithPort { host, port: None },
        transport,
        use_sips,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_contact_names_the_new_local_address_not_the_old_flow() {
        let ip: std::net::IpAddr = "10.9.8.7".parse().unwrap();
        let c = contact_for("bob", None, ip, rsipstack::sip::Transport::Tcp, false);
        let s = c.to_string();
        assert!(
            s.contains("bob@10.9.8.7") && s.to_lowercase().contains("transport=tcp"),
            "{s}"
        );
        let pub_host = rsipstack::sip::Host::Domain("pbx-nat.example".into());
        let c = contact_for(
            "bob",
            Some(&pub_host),
            ip,
            rsipstack::sip::Transport::Tls,
            false,
        );
        assert!(c.to_string().contains("pbx-nat.example"), "{c}");
    }
}

//! Session timers (RFC 4028): the negotiated interval and refresher of a call, the
//! refresh we send when we are the refresher (UPDATE when the peer allows it, else a re-INVITE
//! with an unchanged SDP), and the BYE when the session lapses.

use std::sync::Arc;
use std::time::Duration;

use rsipstack::sip::{Header, Headers};
use siple_config::AccountConfig;
use tokio::task::AbortHandle;
use tokio::time::Instant;

use super::util::{self, Refresher, SdpBody};
use super::{Call, CallDirection, CallInner, CallState, ReinviteGuard, answer_sdp, offer_sdp};

/// Negotiated session timer of an established call.
pub(crate) struct SessionTimer {
    /// Session interval in seconds.
    pub interval: u32,
    /// We send the refreshes.
    pub local_refresher: bool,
    /// The peer's `Allow` lists UPDATE (refresh with UPDATE instead of a re-INVITE).
    pub peer_update: bool,
    /// Start of the current interval (last successful refresh, sent or received).
    pub last_refresh: Instant,
    epoch: u64,
    task: Option<AbortHandle>,
}

impl SessionTimer {
    pub(crate) fn stop(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }
}

/// How a refresh attempt ended.
enum RefreshErr {
    /// The call or dialog is gone.
    Gone,
    /// Try again after the delay.
    Retry(Duration),
    /// The peer refused (or something else went wrong); keep trying until the session lapses.
    Failed,
}

/// UAS side of RFC 4028 §9 for an initial INVITE: `Ok(Some((interval, refresher)))` when a
/// timer is negotiated, `Ok(None)` when there is none, `Err(min_se)` for a 422.
pub(crate) fn plan_inbound(
    cfg: &AccountConfig,
    headers: &Headers,
) -> Result<Option<(u32, Refresher)>, u32> {
    if !cfg.session_timers {
        return Ok(None);
    }
    let Some((delta, wanted)) = util::parse_session_expires(headers) else {
        return Ok(None);
    };
    if delta < cfg.min_se {
        return Err(cfg.min_se);
    }
    let uac_timer = util::has_token(headers, "supported", "timer")
        || util::has_token(headers, "require", "timer");
    // The UAC names the refresher, or we pick: the UAC when it supports timers, else us.
    let refresher = match wanted {
        Some(Refresher::Uac) if uac_timer => Refresher::Uac,
        Some(_) => Refresher::Uas,
        None if uac_timer => Refresher::Uac,
        None => Refresher::Uas,
    };
    Ok(Some((delta, refresher)))
}

/// `Session-Expires` (+ `Require: timer` when the UAC must refresh) for a 2xx.
pub(crate) fn answer_headers(interval: u32, refresher: Refresher) -> Vec<Header> {
    let mut h = vec![Header::Other(
        "Session-Expires".into(),
        format!("{interval};refresher={}", refresher.as_str()),
    )];
    if refresher == Refresher::Uac {
        h.push(Header::Other("Require".into(), "timer".into()));
    }
    h
}

impl Call {
    /// Maps "we refresh" to the `refresher` parameter (roles of the dialog-creating INVITE).
    fn refresher_of(&self, local: bool) -> Refresher {
        if local == (self.direction == CallDirection::Outbound) {
            Refresher::Uac
        } else {
            Refresher::Uas
        }
    }

    fn local_is(&self, r: Refresher) -> bool {
        (r == Refresher::Uac) == (self.direction == CallDirection::Outbound)
    }

    /// `Session-Expires` for a request of ours inside the dialog (empty without a timer).
    pub(crate) fn timer_request_headers(&self, g: &CallInner) -> Vec<Header> {
        match &g.timer {
            None => vec![],
            Some(t) => vec![Header::Other(
                "Session-Expires".into(),
                format!(
                    "{};refresher={}",
                    t.interval,
                    self.refresher_of(t.local_refresher).as_str()
                ),
            )],
        }
    }

    /// Installs the negotiated timer and starts it (the dialog is established).
    pub(crate) fn install_timer(
        self: &Arc<Self>,
        g: &mut CallInner,
        interval: u32,
        refresher: Refresher,
        peer_update: bool,
    ) {
        if let Some(t) = g.timer.as_mut() {
            t.stop();
        }
        tracing::debug!(call = %self.id, interval, ?refresher, "session timer");
        g.timer = Some(SessionTimer {
            interval,
            local_refresher: self.local_is(refresher),
            peer_update,
            last_refresh: Instant::now(),
            epoch: 0,
            task: None,
        });
        self.arm_timer(g);
    }

    /// (Re)starts the timer task for the current `last_refresh`.
    fn arm_timer(self: &Arc<Self>, g: &mut CallInner) {
        let Some(t) = g.timer.as_mut() else { return };
        t.stop();
        t.epoch += 1;
        t.last_refresh = Instant::now();
        let epoch = t.epoch;
        let call = self.clone();
        let task = tokio::spawn(async move { call.run_timer(epoch).await });
        t.task = Some(task.abort_handle());
    }

    /// A refresh happened (one we sent through hold/unhold, or one we received): new interval.
    pub(crate) fn touch_timer(self: &Arc<Self>) {
        let mut g = self.lock();
        if g.info.state != CallState::Ended {
            self.arm_timer(&mut g);
        }
    }

    /// `Some(our Min-SE)` when a request carries a `Session-Expires` below it (answer 422).
    pub(crate) fn refresh_too_small(&self, req: &rsipstack::sip::Request) -> Option<u32> {
        if !self.cfg.session_timers {
            return None;
        }
        let (delta, _) = util::parse_session_expires(&req.headers)?;
        (delta < self.cfg.min_se).then_some(self.cfg.min_se)
    }

    /// `Session-Expires` for our 2xx to a refresh request of the peer (re-INVITE / UPDATE in an
    /// established dialog; empty without a timer). Adopts a new interval / refresher from the
    /// request and restarts the timer.
    pub(crate) fn timer_on_remote_refresh(
        self: &Arc<Self>,
        req: &rsipstack::sip::Request,
    ) -> Vec<Header> {
        if !self.cfg.session_timers {
            return vec![];
        }
        let mut g = self.lock();
        let se = util::parse_session_expires(&req.headers);
        let has_timer = g.timer.is_some();
        if let Some((delta, r)) = se {
            if has_timer {
                if let Some(t) = g.timer.as_mut() {
                    t.interval = delta;
                    if let Some(r) = r {
                        t.local_refresher =
                            (r == Refresher::Uac) == (self.direction == CallDirection::Outbound);
                    }
                }
            } else {
                // The peer starts a timer mid-call; it refreshes unless it says otherwise.
                let r = r.unwrap_or(if self.direction == CallDirection::Outbound {
                    Refresher::Uas
                } else {
                    Refresher::Uac
                });
                let pu = util::allows(&req.headers, "UPDATE");
                self.install_timer(&mut g, delta, r, pu);
            }
        }
        let h = self.timer_request_headers(&g);
        if g.timer.is_some() {
            self.arm_timer(&mut g);
        }
        h
    }

    fn timer_snapshot(&self, epoch: u64) -> Option<(u32, bool, Instant)> {
        let g = self.lock();
        if g.info.state == CallState::Ended {
            return None;
        }
        match &g.timer {
            Some(t) if t.epoch == epoch => Some((t.interval, t.local_refresher, t.last_refresh)),
            _ => None,
        }
    }

    async fn run_timer(self: Arc<Self>, epoch: u64) {
        let mut retry_at: Option<Instant> = None;
        loop {
            let Some((interval, local, last_ok)) = self.timer_snapshot(epoch) else {
                return;
            };
            let ival = Duration::from_secs(u64::from(interval));
            if !local {
                // RFC 4028 §10: the non-refresher ends the session shortly before it expires.
                let margin = Duration::from_secs(u64::from((interval / 3).min(32)));
                tokio::time::sleep_until(last_ok + ival - margin).await;
                if self.timer_snapshot(epoch).is_some() {
                    tracing::warn!(call = %self.id, interval, "session expired: the peer did not refresh");
                    self.expire_session();
                }
                return;
            }
            if Instant::now() >= last_ok + ival {
                tracing::warn!(call = %self.id, interval, "session expired: could not refresh");
                self.expire_session();
                return;
            }
            tokio::time::sleep_until(retry_at.take().unwrap_or(last_ok + ival / 2)).await;
            if self.timer_snapshot(epoch).is_none() {
                return;
            }
            let call = self.clone();
            let res = tokio::spawn(async move { call.refresh_exchange().await })
                .await
                .unwrap_or(Err(RefreshErr::Failed));
            match res {
                Ok(()) => {
                    let mut g = self.lock();
                    if let Some(t) = g.timer.as_mut()
                        && t.epoch == epoch
                    {
                        t.last_refresh = Instant::now();
                    }
                }
                Err(RefreshErr::Gone) => return,
                Err(RefreshErr::Retry(d)) => retry_at = Some(Instant::now() + d),
                Err(RefreshErr::Failed) => {
                    retry_at = Some(Instant::now() + (ival / 8).max(Duration::from_secs(1)));
                }
            }
        }
    }

    fn expire_session(self: &Arc<Self>) {
        let _ = self.hangup_with(Some("session_expired"));
    }

    /// One refresh: UPDATE (peer allows it) or a re-INVITE with the current SDP, so `o=` stays.
    async fn refresh_exchange(self: &Arc<Self>) -> Result<(), RefreshErr> {
        let dialog = match self.confirmed_dialog().await {
            Ok(d) => d,
            Err(_) if self.is_ended() => return Err(RefreshErr::Gone),
            Err(_) => return Err(RefreshErr::Retry(Duration::from_secs(1))),
        };
        let (use_update, headers, sdp, _guard) = {
            let mut g = self.lock();
            if !matches!(
                g.info.state,
                CallState::Active | CallState::Held | CallState::RemoteHeld
            ) {
                return Err(RefreshErr::Retry(Duration::from_secs(1)));
            }
            if g.reinvite_out {
                return Err(RefreshErr::Retry(Duration::from_millis(300)));
            }
            let Some(t) = g.timer.as_ref() else {
                return Err(RefreshErr::Gone);
            };
            let update = t.peer_update;
            let mut headers = self.timer_request_headers(&g);
            let sdp = if update {
                headers.push(Header::Supported(self.cfg.supported_tokens().into()));
                None
            } else {
                headers.extend(util::sdp_headers(&self.cfg));
                let remote = g.info.hold.remote;
                let Some(session) = g.session.as_mut() else {
                    return Err(RefreshErr::Gone);
                };
                Some(offer_sdp(session, remote))
            };
            g.reinvite_out = true;
            (update, headers, sdp, ReinviteGuard(self.clone()))
        };
        tracing::debug!(call = %self.id, update = use_update, "refreshing the session");
        let res = if use_update {
            dialog.update(Some(headers), None).await
        } else {
            self.reinvite_bounded(
                &dialog,
                headers,
                sdp.map(String::into_bytes),
                super::REINVITE_BOUND,
            )
            .await
        };
        let resp = match res {
            Ok(Some(r)) => r,
            Ok(None) => return Err(RefreshErr::Retry(Duration::from_secs(1))),
            Err(e) => {
                if self.move_pending() {
                    return Err(RefreshErr::Retry(Duration::from_secs(1)));
                }
                if crate::authz::refusal(&e).is_some() {
                    // Not allowed to answer the peer's challenge: like a failed refresh, retry
                    // and finally let the session expire.
                    self.request_failed(&e, "session refresh");
                    return Err(RefreshErr::Failed);
                }
                self.request_failed(&e, "session refresh");
                return Err(RefreshErr::Gone);
            }
        };
        let code = resp.status_code.code();
        let mut g = self.lock();
        if g.info.state == CallState::Ended {
            return Err(RefreshErr::Gone);
        }
        match code {
            200..=299 => {
                if let Some((delta, r)) = util::parse_session_expires(&resp.headers)
                    && let Some(t) = g.timer.as_mut()
                {
                    t.interval = delta;
                    if let Some(r) = r {
                        t.local_refresher =
                            (r == Refresher::Uac) == (self.direction == CallDirection::Outbound);
                    }
                }
                if !use_update
                    && let SdpBody::Sdp(_, sd) = util::sdp_body(&resp.headers, &resp.body)
                {
                    let remote = g.info.hold.remote;
                    if let Some(session) = g.session.as_mut()
                        && let Ok(neg) = answer_sdp(session, remote, &sd)
                        && g.neg.as_ref() != Some(&neg)
                    {
                        self.apply_negotiated(&mut g, neg);
                    }
                }
                Ok(())
            }
            422 => {
                let min = util::parse_min_se(&resp.headers).unwrap_or(self.cfg.min_se);
                if let Some(t) = g.timer.as_mut() {
                    t.interval = t.interval.max(min);
                }
                Err(RefreshErr::Retry(Duration::ZERO))
            }
            491 => {
                let ms: u64 = if self.direction == CallDirection::Outbound {
                    rand::random_range(2100..=4000)
                } else {
                    rand::random_range(0..=2000)
                };
                Err(RefreshErr::Retry(Duration::from_millis(ms)))
            }
            408 if g.move_pending => {
                // The network just changed: the move decides, the timer keeps running.
                Err(RefreshErr::Retry(Duration::from_secs(1)))
            }
            481 | 408 => {
                drop(g);
                self.dialog_lost(code, "session refresh");
                Err(RefreshErr::Gone)
            }
            _ => {
                tracing::warn!(call = %self.id, code, "session refresh refused");
                Err(RefreshErr::Failed)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(items: &[(&str, &str)]) -> Headers {
        items
            .iter()
            .map(|(k, v)| Header::Other((*k).into(), (*v).into()))
            .collect::<Vec<_>>()
            .into()
    }

    #[test]
    fn inbound_plan() {
        let cfg = AccountConfig::default(); // min_se 90, timers on
        // No Session-Expires: no timer.
        assert_eq!(plan_inbound(&cfg, &h(&[])), Ok(None));
        // Too small: 422 with our Min-SE.
        assert_eq!(
            plan_inbound(&cfg, &h(&[("Session-Expires", "60")])),
            Err(90)
        );
        // UAC supports timer, names nobody: UAC refreshes.
        assert_eq!(
            plan_inbound(
                &cfg,
                &h(&[("Session-Expires", "1800"), ("Supported", "timer")])
            ),
            Ok(Some((1800, Refresher::Uac)))
        );
        // UAC does not support timer: we must refresh.
        assert_eq!(
            plan_inbound(&cfg, &h(&[("Session-Expires", "1800")])),
            Ok(Some((1800, Refresher::Uas)))
        );
        // Named refresher honoured.
        assert_eq!(
            plan_inbound(
                &cfg,
                &h(&[
                    ("Session-Expires", "900;refresher=uas"),
                    ("Supported", "timer")
                ])
            ),
            Ok(Some((900, Refresher::Uas)))
        );
        // Disabled: ignored.
        let off = AccountConfig {
            session_timers: false,
            ..Default::default()
        };
        assert_eq!(
            plan_inbound(&off, &h(&[("Session-Expires", "10")])),
            Ok(None)
        );
    }

    #[test]
    fn answer_header_requires_timer_only_for_uac_refresher() {
        let h1 = answer_headers(1800, Refresher::Uac);
        assert_eq!(h1[0].value(), "1800;refresher=uac");
        assert_eq!(h1[1].value(), "timer");
        assert_eq!(answer_headers(90, Refresher::Uas).len(), 1);
    }
}

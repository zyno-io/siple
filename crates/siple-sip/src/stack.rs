//! [`SipStack`]: the registry of account actors.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use siple_config::{AccountConfig, DtmfMode, GeneralConfig, MediaConfig, NetworkConfig};
use tokio::sync::oneshot;

use crate::account::{AccountHandle, Cmd, Shared, spawn_account};
use crate::blf::{BlfState, BlfStatus, STALE_GRACE_SECS, SubscriptionPhase, ringing_dialog};
use crate::calls::{
    AnswerOptions, AnswerOutcome, CallId, CallInfo, CallManager, CallsHandler, CompleteMode,
    DialOptions, TransferSession,
};
use crate::dns::SystemSrvSource;
use crate::error::{Result, SipError};
use crate::media::MediaProvider;
use crate::mwi::MwiStatus;
use crate::net::{
    NetEvent, NetProbe, NetReason, NetService, NetSetup, SystemClock, SystemProbe, os::OsMonitor,
};
use crate::router::CallHandler;
use crate::subscribe::SubscriptionInfo;
use crate::transport::init_crypto;
use crate::types::{AccountStatus, OptionsResponse, SipStackOptions, UpdateOutcome};

/// Id of a subscription (`s1`, `s2`, ...), unique per process.
pub type SubscriptionId = String;

/// How long `shutdown` / `remove_account` wait for an account to unregister.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// All SIP accounts of the process. Cheap to clone.
#[derive(Clone)]
pub struct SipStack {
    shared: Arc<Shared>,
    accounts: Arc<Mutex<HashMap<String, AccountHandle>>>,
    /// Display order of the accounts (config file order); unlisted ids sort last, by id.
    order: Arc<Mutex<Vec<String>>>,
    general: Arc<Mutex<GeneralConfig>>,
    /// Milliseconds past `expires_at` before a silent BLF subscription is stale.
    blf_stale_grace_ms: Arc<AtomicU64>,
    /// Dials that have not reached the call table yet (DNS, connect, audio setup).
    dialing: Arc<AtomicU64>,
}

impl SipStack {
    /// Creates the stack and installs the rustls crypto provider. Must be called inside a tokio
    /// runtime (the DNS resolver is created here).
    ///
    /// Network-change watching is **off** until [`SipStack::set_network_config`] (the engine
    /// calls it with `[network]`); [`SipStack::network_refresh`] and
    /// [`SipStack::reregister`] work either way.
    pub fn new(opts: SipStackOptions) -> SipStack {
        Self::with_net(
            opts,
            NetSetup {
                config: Some(NetworkConfig {
                    watch: false,
                    probe_secs: 0,
                    ..NetworkConfig::default()
                }),
                ..NetSetup::default()
            },
        )
    }

    /// [`SipStack::new`] with the network monitor, probe, clock and settings supplied (fakes in
    /// tests).
    pub fn with_net(opts: SipStackOptions, net: NetSetup) -> SipStack {
        init_crypto();
        let dns = Arc::new(SystemSrvSource::new());
        let calls = CallManager::new(opts.events.clone(), dns.clone());
        let handler: Arc<dyn CallHandler> = Arc::new(CallsHandler { mgr: calls.clone() });
        let probe: Arc<dyn NetProbe> = net
            .probe
            .unwrap_or_else(|| Arc::new(SystemProbe::new(dns.clone())));
        let service = NetService::spawn(
            net.config.unwrap_or_default(),
            net.monitor.unwrap_or_else(|| Arc::new(OsMonitor)),
            probe.clone(),
            net.clock.unwrap_or_else(|| Arc::new(SystemClock)),
        );
        let stack = SipStack {
            shared: Arc::new(Shared {
                events: opts.events,
                wire_sink: opts.wire_sink,
                default_ua: opts.user_agent_default,
                handler: Arc::new(RwLock::new(handler)),
                dns,
                calls,
                net: service,
                probe,
            }),
            accounts: Arc::default(),
            general: Arc::default(),
            order: Arc::new(Mutex::new(Vec::new())),
            blf_stale_grace_ms: Arc::new(AtomicU64::new(STALE_GRACE_SECS * 1000)),
            dialing: Arc::new(AtomicU64::new(0)),
        };
        stack.spawn_net_forwarder();
        stack
    }

    /// Fans network events out to every account (and to the event channel).
    fn spawn_net_forwarder(&self) {
        let mut rx = self.shared.net.subscribe();
        let accounts = self.accounts.clone();
        let events = self.shared.events.clone();
        tokio::spawn(async move {
            loop {
                let ev = match rx.recv().await {
                    Ok(ev) => ev,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                let map = accounts.lock().unwrap_or_else(PoisonError::into_inner);
                match ev {
                    NetEvent::Changed(c) => {
                        let _ = events.send(crate::SipEvent::NetworkChanged {
                            generation: c.generation,
                            reasons: c.reasons.clone(),
                            old_local_ip: c.old_local_ip.clone(),
                            new_local_ip: c.new_local_ip.clone(),
                        });
                        for h in map.values() {
                            let _ = h.tx.send(Cmd::NetworkChanged {
                                generation: c.generation,
                                reasons: c.reasons.clone(),
                            });
                        }
                    }
                    NetEvent::Probe => {
                        for h in map.values() {
                            let _ = h.tx.send(Cmd::NetProbeTick);
                        }
                    }
                }
            }
        });
    }

    /// Forces a network-change signal for every account now (no debounce): each re-probes and
    /// re-registers. Returns the new network generation.
    pub async fn network_refresh(&self) -> u64 {
        self.shared.net.refresh().await
    }

    /// Current process-wide network generation (0 until the first change).
    pub fn network_generation(&self) -> u64 {
        self.shared.net.generation()
    }

    /// Applies `[network]` settings to the running monitor (hot).
    pub fn set_network_config(&self, cfg: &NetworkConfig) {
        self.shared.net.set_config(cfg);
    }

    /// Raises a network trigger (debounced like an OS event). For embedders that learn about
    /// network changes elsewhere (mobile shells) and for tests.
    pub fn network_trigger(&self, reason: NetReason) {
        self.shared.net.trigger(reason);
    }

    /// Immediate re-register of one account: re-probe, redial a stream connection, reset the
    /// back-off and send REGISTER now (also re-enables registration after
    /// [`SipStack::unregister`]).
    pub fn reregister(&self, id: &str) -> Result<()> {
        self.send(id, Cmd::Reregister)
    }

    /// Replaces the INVITE / dialog handler of every account, call manager included. Normally
    /// not needed; the stack installs its own.
    pub fn set_call_handler(&self, handler: Arc<dyn CallHandler>) {
        *self
            .shared
            .handler
            .write()
            .unwrap_or_else(PoisonError::into_inner) = handler;
    }

    /// Sets where call media comes from (the media engine). Defaults to a
    /// [`crate::NullMediaProvider`], which transmits nothing.
    pub fn set_media_provider(&self, provider: Arc<dyn MediaProvider>) {
        self.shared.calls.set_provider(provider);
    }

    /// Sets `[calls] hold_scope`: which accounts' calls an auto-hold puts on hold. Takes effect
    /// for the next answer / dial / activation.
    pub fn set_hold_scope(&self, scope: siple_config::HoldScope) {
        self.shared.calls.set_hold_scope(scope);
    }

    fn call_cell(&self, account: &str) -> Result<Arc<crate::calls::AccountCalls>> {
        self.handle(account, |h| h.calls.clone())
    }

    /// Places a call. `target` is digits / an extension (`sip:<target>@<domain>`) or a full
    /// `sip:` / `sips:` URI. Returns once the INVITE is on its way; progress comes as events.
    pub async fn dial(&self, account: &str, target: &str, opts: DialOptions) -> Result<CallId> {
        let cell = self.call_cell(account)?;
        self.dialing.fetch_add(1, Ordering::SeqCst);
        let r = self.shared.calls.dial(&cell, target, opts).await;
        self.dialing.fetch_sub(1, Ordering::SeqCst);
        r
    }

    /// Number of dials still being set up (not yet in [`SipStack::calls`]).
    pub fn dials_in_progress(&self) -> u64 {
        self.dialing.load(Ordering::SeqCst)
    }

    /// Answers a ringing inbound call; `opts.mode` says what happens to the other lines.
    pub async fn answer(&self, call_id: &str, opts: AnswerOptions) -> Result<AnswerOutcome> {
        self.shared.calls.answer(call_id, opts).await
    }

    /// Makes `call_id` the only live device-bound line: the other un-held
    /// device calls within `[calls] hold_scope` are put on hold, the call is resumed when we hold
    /// it, and the media focus moves to it. If any hold fails the whole operation aborts with
    /// that error and nothing is resumed. Idempotent (an already active line is a no-op), and
    /// serialised with the other line operations (answer, hold / resume, auto-hold of a dial) in
    /// the order they were issued.
    pub async fn activate(&self, call_id: &str) -> Result<()> {
        self.shared.calls.activate(call_id).await
    }

    /// The `active_line`: the un-held device-bound call that owns the media focus.
    pub fn active_line(&self) -> Option<CallId> {
        self.shared.calls.active_line()
    }

    /// Rejects a ringing inbound call (`code` defaults to 486).
    pub async fn reject(
        &self,
        call_id: &str,
        code: Option<u16>,
        reason: Option<String>,
    ) -> Result<()> {
        self.shared.calls.reject(call_id, code, reason).await
    }

    /// CANCEL (outbound, unanswered), 603 (inbound, ringing) or BYE.
    pub async fn hangup(&self, call_id: &str) -> Result<()> {
        self.shared.calls.hangup(call_id).await
    }

    /// Sends DTMF. `mode` defaults to the effective `dtmf_mode`; `auto` picks RFC 4733 when
    /// negotiated, else SIP INFO.
    pub async fn send_dtmf(
        &self,
        call_id: &str,
        digits: &str,
        mode: Option<DtmfMode>,
        duration_ms: Option<u32>,
    ) -> Result<()> {
        self.shared
            .calls
            .send_dtmf(call_id, digits, mode, duration_ms)
            .await
    }

    /// Local mute (no signalling).
    pub async fn mute(&self, call_id: &str, muted: bool) -> Result<()> {
        self.shared.calls.mute(call_id, muted).await
    }

    /// All calls, ended ones (the last 64) included, oldest first.
    pub fn calls(&self) -> Vec<CallInfo> {
        self.shared.calls.infos()
    }

    /// One call.
    pub fn call(&self, call_id: &str) -> Option<CallInfo> {
        self.shared.calls.get(call_id).ok().map(|c| c.snapshot())
    }

    /// Runtime DND override; it lasts until the account's config `dnd` changes.
    pub async fn set_dnd(&self, account: &str, enabled: bool) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.send(account, Cmd::SetDnd { enabled, reply: tx })?;
        rx.await.map_err(|_| SipError::Closed)
    }

    /// Effective DND of an account.
    pub fn dnd(&self, account: &str) -> Result<bool> {
        self.handle(account, |h| {
            h.calls.dnd.load(std::sync::atomic::Ordering::SeqCst)
        })
    }

    /// Puts a call on hold: re-INVITE with `a=sendonly` (`inactive` with `hold_style =
    /// "inactive"` or when the remote already holds us). On success the call is `Held` and
    /// transmission stops; on failure (non-2xx) the previous state stays and the error carries
    /// the status code. A 491 is retried per RFC 3261 §14.1. Waits (up to 4 s) for the ACK of
    /// a call that was just answered.
    pub async fn hold(&self, call_id: &str) -> Result<()> {
        self.shared.calls.hold(call_id, true).await
    }

    /// Takes a call off hold (`a=sendrecv`). The call becomes `Active`, or `RemoteHeld` while the
    /// remote still holds us.
    pub async fn unhold(&self, call_id: &str) -> Result<()> {
        self.shared.calls.hold(call_id, false).await
    }

    /// Blind transfer: holds the call, sends `REFER` with `Refer-To: <target>`
    /// (digits / extension become `sip:<target>@<domain>`) and `Referred-By: <our AOR>`.
    /// Returns once the REFER was accepted (2xx); a refusal is an error (and a final
    /// [`crate::SipEvent::TransferProgress`]). The sipfrag NOTIFYs arrive as `TransferProgress`
    /// events; a 2xx final one ends the call (end reason `transferred`). A final failure takes
    /// the call off hold again (`[calls] resume_after_failed_transfer`); no final NOTIFY within
    /// 32 s reports 408 and leaves the call held. Creates a blind [`TransferSession`] (returned).
    pub async fn transfer_blind(&self, call_id: &str, target: &str) -> Result<TransferSession> {
        self.shared.calls.transfer_blind(call_id, target).await
    }

    /// Attended transfer: both calls belong to the same account; `call_id` is the original
    /// (held if it is not), `consult_call_id` the consult call. REFER with
    /// `Refer-To: <consult party?Replaces=...>` (on the original dialog, or on the consult one
    /// with `attended_refer_on = "consult"`). A consult call that is still dialing / early is
    /// CANCELed and the original is transferred blind to the party it rang. Legacy entry point:
    /// it creates (or drives) an implicit [`TransferSession`], returned.
    pub async fn transfer_attended(
        &self,
        call_id: &str,
        consult_call_id: &str,
    ) -> Result<TransferSession> {
        self.shared
            .calls
            .transfer_attended(call_id, consult_call_id)
            .await
    }

    /// Starts a consult: holds `call_id` (as a phone does, whatever it is connected to), dials
    /// `target` on the same account (audio source / sink default to the original's) and links
    /// both calls in a [`TransferSession`].
    pub async fn consult_start(
        &self,
        call_id: &str,
        target: &str,
        opts: DialOptions,
    ) -> Result<TransferSession> {
        self.shared.calls.consult_start(call_id, target, opts).await
    }

    /// Links two existing calls of one account into a consult session (`call_id` is the
    /// original).
    pub async fn consult_link(
        &self,
        call_id: &str,
        consult_call_id: &str,
    ) -> Result<TransferSession> {
        self.shared
            .calls
            .consult_link(call_id, consult_call_id)
            .await
    }

    /// Completes a consult session ([`CompleteMode`]). Idempotent once the REFER was sent.
    pub async fn transfer_complete(
        &self,
        session_id: &str,
        mode: CompleteMode,
    ) -> Result<TransferSession> {
        self.shared.calls.transfer_complete(session_id, mode).await
    }

    /// Swaps the user between the original and the consult call.
    pub async fn transfer_swap(&self, session_id: &str) -> Result<TransferSession> {
        self.shared.calls.transfer_swap(session_id).await
    }

    /// Cancels a consult: hangs up the consult call and resumes the original.
    pub async fn transfer_cancel(&self, session_id: &str) -> Result<TransferSession> {
        self.shared.calls.transfer_cancel(session_id).await
    }

    /// One transfer session (live, or among the last 16 finished ones).
    pub fn transfer(&self, session_id: &str) -> Result<TransferSession> {
        self.shared.calls.transfer(session_id)
    }

    /// The last 16 finished transfer sessions followed by the live ones, oldest first.
    pub fn transfers(&self) -> Vec<TransferSession> {
        self.shared.calls.transfers()
    }

    /// Sets `[calls] resume_after_failed_transfer`.
    pub fn set_resume_after_failed_transfer(&self, on: bool) {
        self.shared.calls.set_resume_after_failed(on);
    }

    /// Old name of [`SipStack::hold`] / [`SipStack::unhold`].
    #[doc(hidden)]
    #[deprecated(note = "use SipStack::hold / SipStack::unhold")]
    pub async fn hold_for_test(&self, call_id: &str, held: bool) -> Result<()> {
        self.shared.calls.hold(call_id, held).await
    }

    fn handle<T>(&self, id: &str, f: impl FnOnce(&AccountHandle) -> T) -> Result<T> {
        let map = self.accounts.lock().unwrap_or_else(PoisonError::into_inner);
        map.get(id)
            .map(f)
            .ok_or_else(|| SipError::UnknownAccount(id.to_string()))
    }

    fn send(&self, id: &str, cmd: Cmd) -> Result<()> {
        self.handle(id, |h| h.tx.send(cmd).map_err(|_| SipError::Closed))?
    }

    /// Adds an account and starts it. Returns once the first transport build attempt finished;
    /// a failed build is reported through the account's status (`Failed`) and retried, not as an
    /// error here.
    pub async fn add_account(
        &self,
        cfg: AccountConfig,
        general: &GeneralConfig,
        media: &MediaConfig,
    ) -> Result<()> {
        if cfg.id.is_empty() {
            return Err(SipError::Config("account id is empty".into()));
        }
        let ready = {
            let mut map = self.accounts.lock().unwrap_or_else(PoisonError::into_inner);
            if map.contains_key(&cfg.id) {
                return Err(SipError::DuplicateAccount(cfg.id));
            }
            *self.general.lock().unwrap_or_else(PoisonError::into_inner) = general.clone();
            let id = cfg.id.clone();
            let (handle, ready) =
                spawn_account(cfg, general.clone(), media.clone(), self.shared.clone());
            map.insert(id, handle);
            ready
        };
        let _ = ready.await;
        Ok(())
    }

    /// Unregisters (best effort) and removes an account.
    pub async fn remove_account(&self, id: &str) -> Result<()> {
        self.shared.calls.hangup_live(Some(id)).await;
        let handle = self
            .accounts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id)
            .ok_or_else(|| SipError::UnknownAccount(id.to_string()))?;
        stop(handle).await;
        Ok(())
    }

    /// Applies a changed account config and reports what it took.
    pub async fn update_account(&self, cfg: AccountConfig) -> Result<UpdateOutcome> {
        let (tx, rx) = oneshot::channel();
        self.send(
            &cfg.id.clone(),
            Cmd::Update {
                cfg: Box::new(cfg),
                reply: tx,
            },
        )?;
        rx.await.map_err(|_| SipError::Closed)
    }

    /// Pushes new `[general]` values (user agent default, `reconfig_force`) to all accounts.
    pub fn set_general(&self, general: &GeneralConfig) {
        *self.general.lock().unwrap_or_else(PoisonError::into_inner) = general.clone();
        for h in self
            .accounts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
        {
            let _ = h.tx.send(Cmd::SetGeneral(Box::new(general.clone())));
        }
    }

    /// Pushes new `[media]` values (codecs, rtcp_mux, comfort_noise, codec_answer_order, DTMF
    /// mode / timing, ptime, ...) to every account. They apply to calls created afterwards (SDP
    /// capabilities) and to DTMF sent on existing calls; running media is not renegotiated.
    pub fn set_media(&self, media: &MediaConfig) {
        for h in self
            .accounts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
        {
            let _ = h.tx.send(Cmd::SetMedia(Box::new(media.clone())));
        }
    }

    /// Current status of one account.
    pub fn account_status(&self, id: &str) -> Result<AccountStatus> {
        self.handle(id, |h| {
            let mut st = h.status.borrow().clone();
            st.mwi = h.subs.mwi().map(|m| m.summary());
            st
        })
    }

    /// Subscribes `account` to `event` at `uri` (a bare extension, `user@host` or a full
    /// `sip:` URI; extensions get the account's domain). `accept` defaults per package
    /// (`dialog`, `message-summary`, `presence`), `expires` to 600 s. The subscription is kept
    /// alive (refresh at 85 %, resubscribe after `Subscription-State: terminated` reasons that
    /// allow it) until [`SipStack::unsubscribe`]. Every NOTIFY is delivered as
    /// [`crate::SipEvent::SubscriptionNotify`]. Works before the account is registered: the
    /// SUBSCRIBE goes out once it is.
    pub fn subscribe(
        &self,
        account: &str,
        uri: &str,
        event: &str,
        accept: Option<&str>,
        expires: Option<u32>,
    ) -> Result<SubscriptionId> {
        let (subs, aor) =
            self.handle(account, |h| (h.subs.clone(), h.status.borrow().aor.clone()))?;
        let aor = rsipstack::sip::Uri::try_from(aor.as_str())
            .map_err(|e| SipError::Config(format!("bad AOR {aor:?}: {e}")))?;
        subs.subscribe(uri, event, accept, expires, &aor)
    }

    /// Unsubscribes (`Expires: 0`, bounded) and forgets a subscription made with
    /// [`SipStack::subscribe`]. Config-driven ones (BLF / MWI) are changed in the config.
    pub async fn unsubscribe(&self, sub_id: &str) -> Result<()> {
        let subs = {
            let map = self.accounts.lock().unwrap_or_else(PoisonError::into_inner);
            map.values()
                .find(|h| h.subs.owns(sub_id))
                .map(|h| h.subs.clone())
        };
        subs.ok_or_else(|| SipError::UnknownSubscription(sub_id.to_string()))?
            .unsubscribe(sub_id)
            .await
    }

    /// All subscriptions (BLF and MWI included) of one account or of every account, ordered by
    /// id.
    pub fn subscriptions(&self, account: Option<&str>) -> Vec<SubscriptionInfo> {
        let mut v: Vec<SubscriptionInfo> = self
            .accounts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .filter(|h| account.is_none_or(|a| a == h.cfg_id))
            .flat_map(|h| h.subs.list())
            .collect();
        v.sort_by_key(|i| i.id.trim_start_matches('s').parse::<u64>().unwrap_or(0));
        v
    }

    /// BLF state of the monitored extensions of one account (or all), in config order per
    /// account.
    pub fn blf_status(&self, account: Option<&str>) -> Vec<BlfStatus> {
        let map = self.accounts.lock().unwrap_or_else(PoisonError::into_inner);
        let mut ids: Vec<&String> = map
            .keys()
            .filter(|k| account.is_none_or(|a| a == k.as_str()))
            .collect();
        let order = self.order.lock().unwrap_or_else(PoisonError::into_inner);
        let rank = |id: &str| order.iter().position(|o| o == id).unwrap_or(usize::MAX);
        ids.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.cmp(b)));
        ids.into_iter()
            .flat_map(|k| map[k].subs.blf_statuses(self.stale_grace()))
            .collect()
    }

    fn stale_grace(&self) -> Duration {
        Duration::from_millis(self.blf_stale_grace_ms.load(Ordering::Relaxed))
    }

    /// Test hook: the time past a BLF subscription's `expires_at` after which it counts as
    /// stale (default 30 s).
    #[doc(hidden)]
    pub fn set_blf_stale_grace(&self, grace: Duration) {
        self.blf_stale_grace_ms.store(
            grace.as_millis().min(u128::from(u64::MAX)) as u64,
            Ordering::Relaxed,
        );
    }

    /// The account's config entry for a monitored URI.
    fn blf_entry(
        &self,
        account: &str,
        blf_uri: &str,
    ) -> Result<(
        Arc<crate::calls::AccountCalls>,
        siple_config::BlfConfig,
        AccountConfig,
    )> {
        let cell = self.call_cell(account)?;
        let cfg = cell
            .env()
            .ok_or_else(|| SipError::Transport("account is not running".into()))?
            .cfg;
        let blf = cfg
            .blf
            .iter()
            .find(|b| b.uri == blf_uri)
            .cloned()
            .ok_or_else(|| SipError::UnknownBlf(blf_uri.to_string()))?;
        Ok((cell, blf, (*cfg).clone()))
    }

    /// Dials a monitored BLF entry on its account (what a click on the key does). Same as
    /// [`SipStack::dial`] but the entry must exist (`UnknownBlf` otherwise).
    pub async fn blf_dial(&self, account: &str, blf_uri: &str) -> Result<CallId> {
        self.blf_dial_with(account, blf_uri, DialOptions::default())
            .await
    }

    /// [`SipStack::blf_dial`] with dial options (audio source / sink, headers, ...).
    pub async fn blf_dial_with(
        &self,
        account: &str,
        blf_uri: &str,
        opts: DialOptions,
    ) -> Result<CallId> {
        let (cell, blf, _) = self.blf_entry(account, blf_uri)?;
        self.shared.calls.dial(&cell, &blf.uri, opts).await
    }

    /// Directed pickup of the ringing monitored extension `blf_uri`.
    ///
    /// The method is the entry's `pickup` override, else the account's `pickup`: `prefix` dials
    /// `pickup_target` or `<pickup_prefix><extension>`; `replaces` (experimental) INVITEs the
    /// caller (RFC 5359 §2.16; account `pickup_replaces_target = "monitored"`: the monitored
    /// phone) with `Require: replaces` and `Replaces` built from the ringing dialog. Refused
    /// while the entry is stale or its subscription is not active. Errors: `UnknownBlf`; a
    /// `CallState` error (`invalid_state`) when pickup is not configured or the entry is not
    /// ringing. The call goes through the normal dial path (line coordinator rules apply); a
    /// refusal (404 / 480 / 486, ...) ends the call with that code.
    pub async fn pickup(&self, account: &str, blf_uri: &str) -> Result<CallId> {
        self.pickup_with(account, blf_uri, DialOptions::default())
            .await
    }

    /// [`SipStack::pickup`] with dial options (a non-device source / sink never holds device
    /// calls).
    pub async fn pickup_with(
        &self,
        account: &str,
        blf_uri: &str,
        mut opts: DialOptions,
    ) -> Result<CallId> {
        use crate::pickup::{PickupPlan, not_configured, plan};
        let (cell, blf, cfg) = self.blf_entry(account, blf_uri)?;
        let setting = blf.pickup_for(&cfg);
        if setting.mode == siple_config::PickupMode::Off {
            return Err(not_configured(&blf));
        }
        let grace = self.stale_grace();
        let status = self
            .handle(account, |h| h.subs.blf_statuses(grace))?
            .into_iter()
            .find(|s| s.uri == blf.uri);
        // A silent / retrying subscription may show a call that is long gone.
        if let Some(s) = status
            .as_ref()
            .filter(|s| s.stale || s.subscription.phase != SubscriptionPhase::Active)
        {
            return Err(SipError::CallState {
                call: blf.uri.clone(),
                msg: format!(
                    "BLF state is stale (subscription {}); not picking up",
                    format!("{:?}", s.subscription.phase).to_lowercase()
                ),
            });
        }
        let ringing = status
            .as_ref()
            .filter(|s| s.state == BlfState::Ringing)
            .and_then(|s| ringing_dialog(&s.dialogs));
        let Some(ringing) = ringing else {
            let st = status.as_ref().map_or("unknown", |s| s.state.name());
            return Err(SipError::CallState {
                call: blf.uri.clone(),
                msg: format!(
                    "{} is not ringing (state {st}); nothing to pick up",
                    blf.uri
                ),
            });
        };
        // Pickup targets can come from a NOTIFY: they never choose the next hop (and with it
        // where the digest credential goes); the account's registrar / proxy gets every pickup.
        opts.via_registrar = true;
        let target = match plan(&blf, &setting, ringing)? {
            PickupPlan::Dial(t) => t,
            PickupPlan::Replaces { target, replaces } => {
                opts.headers.push(("Require".into(), "replaces".into()));
                opts.headers.push(("Replaces".into(), replaces));
                target
            }
        };
        self.shared.calls.dial(&cell, &target, opts).await
    }

    /// Message-waiting status of an account (`None` until a `message-summary` NOTIFY arrived,
    /// or with `mwi = "off"`).
    pub fn mwi_status(&self, account: &str) -> Option<MwiStatus> {
        self.handle(account, |h| h.subs.mwi()).ok().flatten()
    }

    /// Wait handle on the status of one account (changes whenever anything visible changes).
    pub fn watch_status(&self, id: &str) -> Result<tokio::sync::watch::Receiver<AccountStatus>> {
        self.handle(id, |h| h.status.clone())
    }

    /// Sets the display order used by [`SipStack::accounts_status`] (the config file order).
    pub fn set_account_order(&self, ids: &[String]) {
        *self.order.lock().unwrap_or_else(PoisonError::into_inner) = ids.to_vec();
    }

    /// Status of all accounts, in the configured order (see [`SipStack::set_account_order`]),
    /// accounts not in it last, by id.
    pub fn accounts_status(&self) -> Vec<AccountStatus> {
        let mut v: Vec<_> = self
            .accounts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .map(|h| {
                let mut st = h.status.borrow().clone();
                st.mwi = h.subs.mwi().map(|m| m.summary());
                st
            })
            .collect();
        let order = self.order.lock().unwrap_or_else(PoisonError::into_inner);
        let rank = |id: &str| order.iter().position(|o| o == id).unwrap_or(usize::MAX);
        v.sort_by(|a, b| rank(&a.id).cmp(&rank(&b.id)).then_with(|| a.id.cmp(&b.id)));
        v
    }

    /// Registers now (also re-enables registration after [`SipStack::unregister`]).
    pub fn register_now(&self, id: &str) -> Result<()> {
        self.send(id, Cmd::RegisterNow)
    }

    /// Removes our binding (`Expires: 0` for our Contact only) and stops refreshing until
    /// [`SipStack::register_now`].
    pub fn unregister(&self, id: &str) -> Result<()> {
        self.send(id, Cmd::Unregister)
    }

    /// Runtime SIP-trace override (a config change of `sip_trace` replaces it).
    pub fn set_sip_trace(&self, id: &str, on: bool) -> Result<()> {
        self.send(id, Cmd::SetTrace(on))
    }

    /// Tells the account how many calls are active: rebuilds deferred by
    /// [`UpdateOutcome::RebuildDeferred`] run when this drops to zero.
    pub fn set_active_calls(&self, id: &str, calls: usize) -> Result<()> {
        self.send(id, Cmd::CallsChanged(calls))
    }

    /// Sends an OPTIONS request from the account (`sip:` URI, `user@host[:port]` or
    /// `host[:port]`; the account's transport unless the URI says otherwise) and returns the
    /// final response.
    pub async fn send_options(&self, id: &str, target: &str) -> Result<OptionsResponse> {
        let (tx, rx) = oneshot::channel();
        self.send(
            id,
            Cmd::SendOptions {
                target: target.to_string(),
                reply: tx,
            },
        )?;
        rx.await.map_err(|_| SipError::Closed)?
    }

    /// `account.test`: one REGISTER (and unREGISTER) with `cfg` on a private socket, within
    /// `timeout` (at most [`crate::TEST_TIMEOUT`] by convention), without touching any running
    /// account. See [`Diagnosis`](crate::Diagnosis).
    pub async fn test_account(
        &self,
        cfg: siple_config::AccountConfig,
        timeout: std::time::Duration,
    ) -> crate::Diagnosis {
        crate::diag::run(
            cfg,
            self.shared.probe.clone(),
            &self.shared.default_ua,
            timeout,
        )
        .await
    }

    /// Test hook: closes the registrar TCP/TLS connection as if the network dropped it, to
    /// exercise the redial + re-REGISTER path. Returns whether a connection was closed.
    #[doc(hidden)]
    pub async fn simulate_flow_loss(&self, id: &str) -> Result<bool> {
        let (tx, rx) = oneshot::channel();
        self.send(id, Cmd::DropFlow(tx))?;
        rx.await.map_err(|_| SipError::Closed)
    }

    /// Test hook: makes the account's UDP SIP socket fail like one the OS has torn down (every
    /// send `EPIPE`, every receive `ENOTCONN`, as macOS "defunct" sockets do), to exercise the
    /// dead-socket replacement. Returns whether the account had a UDP socket.
    #[doc(hidden)]
    pub async fn simulate_udp_socket_death(&self, id: &str) -> Result<bool> {
        let (tx, rx) = oneshot::channel();
        self.send(id, Cmd::KillUdp(tx))?;
        rx.await.map_err(|_| SipError::Closed)
    }

    /// Test hook: whether the account waits for a delayed replacement of a dead UDP socket,
    /// and how many replaced sockets died right away in a row.
    #[doc(hidden)]
    pub async fn socket_replacement_state(&self, id: &str) -> Result<(bool, u32)> {
        let (tx, rx) = oneshot::channel();
        self.send(id, Cmd::SocketReplacement(tx))?;
        rx.await.map_err(|_| SipError::Closed)
    }

    /// Unregisters every account (best effort, bounded) and stops them.
    pub async fn shutdown(&self) {
        self.shared.net.shutdown();
        self.shared.calls.shutdown().await;
        let handles: Vec<AccountHandle> = self
            .accounts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain()
            .map(|(_, h)| h)
            .collect();
        let stops = handles
            .into_iter()
            .map(|h| tokio::spawn(stop(h)))
            .collect::<Vec<_>>();
        for s in stops {
            let _ = s.await;
        }
    }
}

async fn stop(handle: AccountHandle) {
    let (tx, rx) = oneshot::channel();
    if handle.tx.send(Cmd::Shutdown(tx)).is_ok()
        && tokio::time::timeout(STOP_TIMEOUT, rx).await.is_err()
    {
        tracing::warn!(account = %handle.cfg_id, "account did not stop in time");
    }
}

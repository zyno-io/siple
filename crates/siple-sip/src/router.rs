//! Per-account incoming-request router (never block the router, spawn
//! `dialog.handle()`, remove terminated dialogs).
//!
//! Order of dispatch for every incoming server transaction:
//! 1. in-dialog request (To-tag): [`CallHandler::on_in_dialog`] first (early dialogs, registration
//!    races), then `dialog.handle(tx)` for a known dialog;
//! 2. new INVITE -> [`CallHandler::on_invite`];
//! 3. out-of-dialog OPTIONS / MESSAGE / NOTIFY / SUBSCRIBE -> answered here + [`SipEvent`];
//! 4. anything with a To-tag that matched no dialog -> 481; everything else -> 405.

use std::sync::{Arc, Mutex, PoisonError, RwLock};

use async_trait::async_trait;
use rsipstack::dialog::authenticate::Credential;
use rsipstack::dialog::dialog::{DialogState, DialogStateReceiver, DialogStateSender};
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::sip::prelude::*;
use rsipstack::sip::{Header, Method, StatusCode, Uri};
use rsipstack::transaction::Transaction;
use rsipstack::transaction::TransactionReceiver;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::calls::AccountCalls;
use crate::subscribe::{NotifyOutcome, SubManager};
use crate::types::{ACCEPT, ALLOW, SUPPORTED, SipEvent};

/// Per-account handles a [`CallHandler`] needs to create and drive dialogs.
#[derive(Clone)]
pub struct AccountCtx {
    /// Account id.
    pub account: String,
    /// The account's dialog layer.
    pub dialogs: Arc<DialogLayer>,
    /// Sender for `get_or_create_server_invite` / `do_invite`; its receiver is drained into
    /// [`CallHandler::on_dialog_state`].
    pub state_tx: DialogStateSender,
    contact: Arc<Mutex<Option<Uri>>>,
    /// Call-related state shared with the account actor.
    pub(crate) calls: Arc<AccountCalls>,
}

impl AccountCtx {
    /// Current digest credential (follows config updates).
    pub fn credential(&self) -> Credential {
        self.calls.credential()
    }

    /// The Contact URI currently registered (or that will be) for this account; use it as the
    /// local contact of dialogs so it matches the registered flow.
    pub fn contact(&self) -> Option<Uri> {
        self.contact
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Extension point for calls. The stack installs its call manager; [`RejectCalls`] answers 480.
#[async_trait]
pub trait CallHandler: Send + Sync {
    /// A new out-of-dialog INVITE arrived. The handler owns the transaction and must answer it
    /// (typically `ctx.dialogs.get_or_create_server_invite(..)` then `dialog.handle(&mut tx)`).
    async fn on_invite(&self, ctx: AccountCtx, tx: Transaction);

    /// A dialog state change from any dialog of the account. Implementations must answer
    /// `Notify`/`Info`/`Options`/`Updated`/... through the handle and call
    /// `ctx.dialogs.remove_dialog(&id)` on `Terminated`; the default does both.
    async fn on_dialog_state(&self, ctx: &AccountCtx, state: DialogState) {
        default_dialog_state(ctx, state).await;
    }

    /// An in-dialog request (To-tag present) arrived. Return `None` when it was answered here;
    /// return the transaction to let the dialog layer handle it. Called before the dialog-layer
    /// lookup so early-dialog requests and requests racing the dialog registration can be
    /// matched by Call-ID + our tag.
    async fn on_in_dialog(&self, _ctx: &AccountCtx, tx: Transaction) -> Option<Transaction> {
        Some(tx)
    }
}

/// Default dialog-state handling: answer in-dialog requests, drop terminated dialogs.
pub async fn default_dialog_state(ctx: &AccountCtx, state: DialogState) {
    match state {
        DialogState::Terminated(id, reason) => {
            tracing::debug!(account = %ctx.account, ?reason, "dialog terminated");
            ctx.dialogs.remove_dialog(&id);
        }
        DialogState::Notify(_, _, h)
        | DialogState::Info(_, _, h)
        | DialogState::Options(_, _, h)
        | DialogState::Updated(_, _, h)
        | DialogState::Message(_, _, h) => {
            let _ = h.reply(StatusCode::OK).await;
        }
        DialogState::Refer(_, _, h) | DialogState::Publish(_, _, h) => {
            let _ = h.reply(StatusCode::NotImplemented).await;
        }
        _ => {}
    }
}

/// Handler that refuses every call with `480 Temporarily Unavailable`.
pub struct RejectCalls;

#[async_trait]
impl CallHandler for RejectCalls {
    async fn on_invite(&self, ctx: AccountCtx, mut tx: Transaction) {
        tracing::info!(account = %ctx.account, "rejecting incoming INVITE (calls not implemented)");
        let _ = tx.reply(StatusCode::TemporarilyUnavailable).await;
    }
}

/// Shared slot holding the active [`CallHandler`].
pub type HandlerSlot = Arc<RwLock<Arc<dyn CallHandler>>>;

/// Everything the router task needs.
pub struct Router {
    pub account: String,
    pub events: UnboundedSender<SipEvent>,
    pub dialogs: Arc<DialogLayer>,
    pub handler: HandlerSlot,
    pub ctx: AccountCtx,
    pub(crate) subs: Arc<SubManager>,
}

impl Router {
    /// Builds the router and its context; returns the dialog-state receiver to drain.
    pub(crate) fn new(
        account: String,
        events: UnboundedSender<SipEvent>,
        dialogs: Arc<DialogLayer>,
        handler: HandlerSlot,
        contact: Arc<Mutex<Option<Uri>>>,
        calls: Arc<AccountCalls>,
        subs: Arc<SubManager>,
    ) -> (Arc<Router>, DialogStateReceiver) {
        let (state_tx, state_rx) = dialogs.new_dialog_state_channel();
        let ctx = AccountCtx {
            account: account.clone(),
            dialogs: dialogs.clone(),
            state_tx,
            contact,
            calls,
        };
        (
            Arc::new(Router {
                account,
                events,
                dialogs,
                handler,
                ctx,
                subs,
            }),
            state_rx,
        )
    }

    fn handler(&self) -> Arc<dyn CallHandler> {
        self.handler
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Spawns the dialog-state drain and the transaction loop. Both stop when `token` (the
    /// account runtime's token) is canceled: the channels they drain are fed by senders owned
    /// by the endpoint, which this router itself keeps alive (router -> dialog layer ->
    /// endpoint), so closure alone would never end them and the whole runtime (UDP socket
    /// included) would leak.
    pub fn spawn(
        self: &Arc<Self>,
        token: CancellationToken,
        mut incoming: TransactionReceiver,
        mut states: DialogStateReceiver,
    ) {
        let this = self.clone();
        let t = token.clone();
        tokio::spawn(async move {
            loop {
                let state = tokio::select! {
                    () = t.cancelled() => break,
                    s = states.recv() => match s {
                        Some(s) => s,
                        None => break,
                    },
                };
                let handler = this.handler();
                tokio::select! {
                    () = t.cancelled() => break,
                    () = handler.on_dialog_state(&this.ctx, state) => {}
                }
            }
        });
        let this = self.clone();
        tokio::spawn(async move {
            loop {
                let tx = tokio::select! {
                    () = token.cancelled() => break,
                    tx = incoming.recv() => match tx {
                        Some(tx) => tx,
                        None => break,
                    },
                };
                this.dispatch(tx, token.clone());
            }
        });
    }

    fn dispatch(self: &Arc<Self>, tx: Transaction, token: CancellationToken) {
        let this = self.clone();
        tokio::spawn(async move {
            tokio::select! {
                () = token.cancelled() => {}
                () = this.route(tx) => {}
            }
        });
    }

    async fn route(self: Arc<Self>, mut tx: Transaction) {
        let has_to_tag = tx
            .original
            .to_header()
            .ok()
            .and_then(|t| t.tag().ok().flatten())
            .is_some();
        // 0. NOTIFYs of our own subscriptions (Call-ID + our tag + Event) before anything else:
        // one may even arrive before the SUBSCRIBE's 2xx.
        if tx.original.method == Method::Notify && has_to_tag {
            match self.subs.on_notify(&tx.original) {
                NotifyOutcome::Accepted => {
                    let _ = tx.reply(StatusCode::OK).await;
                    return;
                }
                NotifyOutcome::NoSubscription => {
                    let _ = tx.reply(StatusCode::CallTransactionDoesNotExist).await;
                    return;
                }
                NotifyOutcome::NotOurs => {}
            }
        }
        // 1. Calls get the first look at in-dialog requests (early dialogs, registration races).
        if has_to_tag {
            let handler = self.handler();
            match handler.on_in_dialog(&self.ctx, tx).await {
                Some(t) => tx = t,
                None => return,
            }
        }
        // 2. Known dialog (also ACKs and in-dialog NOTIFY/BYE/...).
        if let Some(mut dialog) = self.dialogs.match_dialog(&tx) {
            if let Err(e) = dialog.handle(&mut tx).await {
                tracing::warn!(account = %self.account, error = %e, "dialog handler error");
            }
            return;
        }
        self.out_of_dialog(tx).await;
    }

    async fn out_of_dialog(self: Arc<Self>, mut tx: Transaction) {
        let req = tx.original.clone();
        let has_to_tag = req
            .to_header()
            .ok()
            .and_then(|t| t.tag().ok().flatten())
            .is_some();
        let from = req
            .from_header()
            .map(|f| f.value().to_string())
            .unwrap_or_default();
        let from_uri = req
            .from_header()
            .ok()
            .and_then(|f| f.typed().ok())
            .map(|t| t.uri.to_string())
            .unwrap_or(from);
        let account = self.account.clone();
        match req.method {
            Method::Invite if has_to_tag => {
                let _ = tx.reply(StatusCode::CallTransactionDoesNotExist).await;
            }
            Method::Invite => {
                let handler = self.handler();
                handler.on_invite(self.ctx.clone(), tx).await;
            }
            Method::Options => {
                let supported = self
                    .ctx
                    .calls
                    .env()
                    .map_or_else(|| SUPPORTED.to_string(), |e| e.cfg.supported_tokens());
                let headers = vec![
                    Header::Allow(ALLOW.into()),
                    Header::Accept(ACCEPT.into()),
                    Header::Supported(supported.into()),
                ];
                let _ = tx.reply_with(StatusCode::OK, headers, None).await;
                let _ = self.events.send(SipEvent::OptionsReceived {
                    account,
                    from: from_uri,
                });
            }
            Method::Message => {
                let _ = tx.reply(StatusCode::OK).await;
                let content_type = header_text(&req, |h| matches!(h, Header::ContentType(_)));
                let body = String::from_utf8_lossy(&req.body).into_owned();
                let _ = self.events.send(SipEvent::MessageReceived {
                    account,
                    from: from_uri,
                    content_type,
                    body,
                });
            }
            Method::Notify => {
                // Unsolicited (e.g. FreeSWITCH message-summary after REGISTER) or for a
                // subscription nobody registered yet: always 200. NOTIFYs of a subscription
                // are intercepted by Call-ID before they get here.
                let _ = tx.reply(StatusCode::OK).await;
                let event = header_text(&req, |h| matches!(h, Header::Event(_)));
                let body = String::from_utf8_lossy(&req.body).into_owned();
                if event
                    .split(';')
                    .next()
                    .is_some_and(|e| e.trim().eq_ignore_ascii_case("message-summary"))
                {
                    // Unsolicited message-summary (FreeSWITCH pushes one after REGISTER).
                    self.subs.on_unsolicited_mwi(&body);
                }
                let _ = self.events.send(SipEvent::UnhandledNotify {
                    account,
                    event,
                    from: from_uri,
                    body,
                });
            }
            Method::Subscribe if !has_to_tag => {
                // 489 Bad Event: we accept no subscriptions.
                let _ = tx.reply(StatusCode::BadEvent).await;
            }
            Method::Ack | Method::Cancel => {} // stray; the stack answers CANCEL itself
            // Dialog-only methods (we list them in Allow) without a dialog: 481, not 405.
            Method::Bye | Method::PRack | Method::Update | Method::Info | Method::Refer => {
                let _ = tx.reply(StatusCode::CallTransactionDoesNotExist).await;
            }
            _ if has_to_tag => {
                let _ = tx.reply(StatusCode::CallTransactionDoesNotExist).await;
            }
            _ => {
                let _ = tx
                    .reply_with(
                        StatusCode::MethodNotAllowed,
                        vec![Header::Allow(ALLOW.into())],
                        None,
                    )
                    .await;
            }
        }
    }
}

fn header_text(req: &rsipstack::sip::Request, pick: fn(&Header) -> bool) -> String {
    req.headers
        .iter()
        .find(|h| pick(h))
        .and_then(|h| {
            h.to_string()
                .split_once(':')
                .map(|(_, v)| v.trim().to_string())
        })
        .unwrap_or_default()
}

//! [`CallHandler`] implementation that plugs the call manager into the account routers.

use std::sync::Arc;

use async_trait::async_trait;
use rsipstack::dialog::dialog::DialogState;
use rsipstack::transaction::Transaction;

use super::CallManager;
use crate::router::{AccountCtx, CallHandler};

/// Routes INVITEs, dialog states and in-dialog requests to the [`CallManager`].
pub(crate) struct CallsHandler {
    pub(crate) mgr: Arc<CallManager>,
}

#[async_trait]
impl CallHandler for CallsHandler {
    async fn on_invite(&self, ctx: AccountCtx, tx: Transaction) {
        self.mgr.on_invite(ctx, tx).await;
    }

    async fn on_dialog_state(&self, ctx: &AccountCtx, state: DialogState) {
        self.mgr.on_dialog_state(ctx, state).await;
    }

    async fn on_in_dialog(&self, ctx: &AccountCtx, tx: Transaction) -> Option<Transaction> {
        self.mgr.on_in_dialog(ctx, tx).await
    }
}

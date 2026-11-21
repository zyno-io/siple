//! Siple SIP foundation (Phase 3): one actor per account owning its rsipstack endpoint,
//! registration state machine, keep-alives and incoming-request router.
//!
//! Entry point: [`SipStack`]. Events come out of the channel given in [`SipStackOptions`];
//! status is polled with [`SipStack::account_status`]. Calls (Phase 5) are driven through
//! `SipStack::{dial, answer, reject, hangup, send_dtmf, mute, calls}`; media comes from the
//! [`MediaProvider`] given to [`SipStack::set_media_provider`].

mod account;
mod authz;
pub mod blf;
mod calls;
pub mod client;
mod diag;
pub mod dns;
mod error;
pub mod media;
pub mod mwi;
pub mod net;
pub mod pickup;
pub mod problem;
pub mod reg_logic;
pub mod router;
mod stack;
pub mod subscribe;
pub mod transport;
mod types;
pub mod update;

#[doc(hidden)]
pub use account::set_register_timeout_ms;
pub use blf::{
    BlfDialog, BlfState, BlfStatus, BlfSubscription, DialogDirection, DialogPhase,
    SubscriptionPhase,
};
pub use calls::{
    AnswerMode, AnswerOptions, AnswerOutcome, CallDirection, CallEnd, CallId, CallInfo, CallState,
    CompleteMode, DialOptions, EndCause, EndedBy, HoldFlags, NegotiatedSummary, PendingOp,
    TransferError, TransferKind, TransferNotify, TransferPhase, TransferRef, TransferRole,
    TransferSession,
};
#[doc(hidden)]
pub use calls::{
    set_end_others_ack_wait_ms, set_move_attempt_ms, set_prack_t1_ms, set_redirect_hop_delay_ms,
    set_transfer_timeout_ms,
};
pub use diag::{Diagnosis, TEST_TIMEOUT};
pub use error::{Result, SipError};
pub use media::{CallMedia, CallMediaRequest, MediaProvider, NullMediaProvider, NullMediaStats};
pub use mwi::{MessageCounts, MwiStatus, MwiSummary};
pub use problem::Problem;
pub use rsipstack::sip::Transport as WireTransport;
pub use rsipstack::transport::{WireDirection, WireEvent};
pub use siple_config::PickupMode;
pub use stack::{SipStack, SubscriptionId};
pub use subscribe::{SubKind, SubState, SubscriptionInfo};
pub use transport::init_crypto;
pub use types::{
    ACCEPT, ALLOW, AccountStatus, NullWireSink, OptionsResponse, RegState, SUPPORTED, SipEvent,
    SipStackOptions, UpdateOutcome, WireSink,
};

//! Error type for the SIP layer.

/// Failure in the SIP layer.
#[derive(Debug, thiserror::Error)]
pub enum SipError {
    /// An account with this id already exists.
    #[error("account {0:?} already exists")]
    DuplicateAccount(String),
    /// No account with this id.
    #[error("unknown account {0:?}")]
    UnknownAccount(String),
    /// The account configuration cannot be used.
    #[error("invalid account config: {0}")]
    Config(String),
    /// Socket / connection problem.
    #[error("transport: {0}")]
    Transport(String),
    /// The OS has no route toward the destination (network down, no default route, address
    /// gone): the account waits for the network instead of retrying.
    #[error("no route: {0}")]
    NoRoute(String),
    /// Name resolution failed.
    #[error("DNS: {0}")]
    Dns(String),
    /// TLS configuration or handshake problem.
    #[error("TLS: {0}")]
    Tls(String),
    /// No final response in time.
    #[error("request timed out")]
    Timeout,
    /// The account actor has stopped.
    #[error("account task is gone")]
    Closed,
    /// No call with this id.
    #[error("unknown call {0:?}")]
    UnknownCall(String),
    /// No transfer session with this id.
    #[error("unknown transfer session {0:?}")]
    UnknownTransfer(String),
    /// No subscription with this id.
    #[error("unknown subscription {0:?}")]
    UnknownSubscription(String),
    /// No BLF entry with this URI on the account.
    #[error("unknown BLF entry {0:?}")]
    UnknownBlf(String),
    /// The call is in a state that does not allow the operation.
    #[error("call {call}: {msg}")]
    CallState {
        /// Call id.
        call: String,
        /// What is wrong.
        msg: String,
    },
    /// The media provider failed.
    #[error("media: {0}")]
    Media(String),
    /// An argument is out of range or malformed (maps to `invalid_params`).
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    /// The request was answered with a failure and cannot continue.
    #[error("{0}")]
    Protocol(String),
    /// A request (re-INVITE, INFO, REFER, ...) was answered with a final SIP failure response.
    #[error("{code} {reason}")]
    Status {
        /// SIP status code of the final response.
        code: u16,
        /// Reason phrase of the response.
        reason: String,
    },
}

impl From<rsipstack::Error> for SipError {
    fn from(e: rsipstack::Error) -> Self {
        // A refused digest challenge reads as is: "Authentication refused: <host> ...".
        match crate::authz::refusal(&e) {
            Some(msg) => SipError::Protocol(msg),
            None => SipError::Protocol(e.to_string()),
        }
    }
}

impl From<std::io::Error> for SipError {
    fn from(e: std::io::Error) -> Self {
        SipError::Transport(e.to_string())
    }
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, SipError>;

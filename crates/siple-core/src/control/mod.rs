//! Control plane: JSON-RPC 2.0 over newline-delimited JSON on a local socket.
//!
//! - [`protocol`]: wire types and [`ApiError`].
//! - [`methods`]: the [`ControlBackend`] trait the engine implements, built-in methods
//!   (`hello`, `ping`, `methods.list`, `events.*`, `logs.tail`) and the [`wait_for`] helper.
//! - [`server`]: unix socket / named pipe / loopback TCP listeners.
//! - [`client`]: a small async client used by `siple-cli ctl|events` (and later the MCP bridge).

pub mod client;
pub mod methods;
pub mod protocol;
pub mod server;

pub use methods::{
    ControlBackend, ControlContext, MethodInfo, StubBackend, release_big_request_permit, wait_for,
};
pub use protocol::{ApiError, ErrorKind};
pub use server::{ControlServer, ServerError};
pub use tokio_util::sync::CancellationToken;

#[cfg(all(test, unix))]
mod tests;

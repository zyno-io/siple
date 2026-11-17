//! `siple-cli mcp`: a hand-rolled MCP server (JSON-RPC 2.0, newline-delimited, stdio) that
//! proxies to a running instance's control socket.
//!
//! Hand-rolled instead of `rmcp`: the surface is five methods, and the tool list comes from
//! `methods.list` at runtime, which an SDK's static tool macros would not help with.
//!
//! - Requests run concurrently (each `tools/call` / `tools/list` in its own task with its own
//!   control connection), so `ping` and short calls are never stuck behind a long
//!   `wait_for_event`. Responses may therefore arrive out of order (they carry their ids).
//! - Every control request has a time limit (see [`request_timeout`]); a stuck request fails
//!   instead of wedging the session.
//! - If the phone is not running, `tools/list` serves the static catalog from
//!   [`siple_core::backend::method_catalog`]; once a connection succeeds and the live list
//!   differs, `notifications/tools/list_changed` is sent.
//! - A lost connection is retried only for idempotent read methods; state-changing calls
//!   report the failure (they may have run).
//! - If the instance restarted since the last connection (`instance_id` changed), the next tool
//!   result says so (`restarted: true`), because event seqs and call ids reset.
//!
//! stdout carries only protocol messages; diagnostics go to stderr.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};
use siple_core::backend::method_catalog;
use siple_core::control::client::{ClientError, ControlClient, is_idempotent};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;

use crate::{ConnectArgs, connect};

const SUPPORTED_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
const WAIT_TOOL: &str = "wait_for_event";
const DEFAULT_WAIT_MS: u64 = 10_000;
const MAX_WAIT_MS: u64 = 60_000;
/// Methods that only make sense on a streaming connection (or are the handshake).
const EXCLUDED: [&str; 4] = [
    "hello",
    "events.subscribe",
    "events.unsubscribe",
    "call.stream",
];
/// First protocol revision with audio content blocks.
const AUDIO_MIN_VERSION: &str = "2025-03-26";

/// Concurrent tool calls served at once.
const MAX_CONCURRENT: usize = 16;
/// How long to wait for in-flight requests after stdin closed.
const DRAIN: Duration = Duration::from_secs(10);

/// Prefix of the first tool result after the instance restarted.
const RESTART_NOTICE: &str = "Siple restarted: event sequence and call ids reset. Any event_seq / since / next_seq values and call ids (c1, c2, ...) from before are no longer valid; call `status` to see the current state and `events_current_seq` for a fresh baseline.";

const NOT_RUNNING: &str =
    "Siple is not running — start it with `siple-cli run --headless` or the GUI";

const INSTRUCTIONS: &str = "Siple is a SIP test softphone with multiple accounts; these tools drive a running instance \
(register accounts, place/answer calls, send and detect DTMF/tones, transfer, hold, inspect SIP traces). \
Typical flow: `status` to see accounts and calls, `call_dial` {account, target}, then `wait_for_event` with \
kinds [\"call.state\"] and the call_id until the call is active (or ended), then `call_dtmf` / `call_detect_tone` / \
`call_expect_dtmf`, finally `call_hangup`. Every state-changing result includes `event_seq`; pass it as `since` to \
`wait_for_event` (and chain `next_seq` from one wait to the next) so no event is missed between actions. \
Voice loop (needs [speech] tts/stt configured): `call_say` {call_id, text, wait:true} returns finished.rx_ms; \
then `call_listen` {call_id, since_rx_ms: that rx_ms, transcribe:true} returns the caller's transcript and end_rx_ms; \
answer with `call_say` {wait:true} and pass THAT say's finished.barge_in_rx_ms if present (the caller interrupted: \
where they began) else finished.rx_ms (end of your own audio) as the next since_rx_ms; only when listening again without speaking in between, pass the previous end_rx_ms. The start timeout \
counts from the moment you call listen, and history after since_rx_ms is replayed, so thinking time loses nothing. Without speech hooks, use `call_play` (wav_base64 / tone:) and `call_listen` with audio:\"file\" \
or the audio block returned to you. Barge-in is unreliable on echo paths. \
Call ids look like c1, c2. Tool names use underscores for method dots (call.dial -> call_dial). If a result says Siple restarted, event seqs and call ids from before are invalid: re-check `status`.";

type BoxFut<T> = Pin<Box<dyn Future<Output = T> + Send>>;
/// Opens a fresh, authenticated control connection.
pub type Connector = Arc<dyn Fn() -> BoxFut<Result<ControlClient, ClientError>> + Send + Sync>;

/// Entry point for the CLI: serves stdin/stdout until EOF.
pub async fn run(args: ConnectArgs) -> anyhow::Result<()> {
    let args = Arc::new(args);
    let connector: Connector = Arc::new(move || {
        let args = args.clone();
        Box::pin(async move { connect(&args).await })
    });
    let server = McpServer::new(connector);
    server
        .serve(BufReader::new(tokio::io::stdin()), tokio::io::stdout())
        .await?;
    Ok(())
}

#[derive(Clone, PartialEq)]
struct ToolEntry {
    /// MCP-safe name.
    name: String,
    /// Control API method (`None` for the synthetic `wait_for_event`).
    method: Option<String>,
    description: String,
    schema: Value,
}

/// `call.dial` -> `call_dial`: anything outside `[A-Za-z0-9_-]` becomes `_`, capped at 64.
pub fn tool_name(method: &str) -> String {
    method
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

fn wait_tool() -> ToolEntry {
    ToolEntry {
        name: WAIT_TOOL.into(),
        method: None,
        description: "Wait (long-poll up to timeout_ms, default 10000, max 60000) for Siple events and return \
those matching the filters, e.g. kinds [\"call.state\"] with a call_id to wait for a call to become active or ended. \
Returns {events, next_seq, truncated}. Race-free usage: pass as `since` the `event_seq` from the result of the action \
you just performed (e.g. call_dial), or `next_seq` from your previous wait_for_event; events already logged after that \
seq are returned immediately. Without `since` only events newer than the call are returned. An empty `events` list \
means the timeout elapsed. Kinds are exact (`call.state`) or end with `*` (`call.*`). Event kinds include \
account.registration, account.reconnecting, network.changed, call.incoming, call.state, call.media, call.dtmf, \
call.reconnecting, call.reconnected, call.transfer_progress, transfer.session, message.received."
            .into(),
        schema: json!({
            "type": "object",
            "properties": {
                "kinds": {"type": "array", "items": {"type": "string"},
                    "description": "Event kinds; exact or trailing `*` prefix (e.g. `call.*`)."},
                "account": {"type": "string", "description": "Only events of this account id."},
                "call_id": {"type": "string", "description": "Only events of this call id (e.g. c1)."},
                "since": {"type": "integer", "minimum": 0,
                    "description": "Return events with seq greater than this (event_seq from a prior action, or next_seq from a prior wait)."},
                "timeout_ms": {"type": "integer", "minimum": 0, "maximum": MAX_WAIT_MS,
                    "description": "Max time to wait for a matching event (default 10000, max 60000)."}
            },
            "additionalProperties": false
        }),
    }
}

fn build_tools(methods: &Value) -> Vec<ToolEntry> {
    let mut out: Vec<ToolEntry> = Vec::new();
    let mut used: HashMap<String, ()> = HashMap::new();
    used.insert(WAIT_TOOL.into(), ());
    for m in methods["methods"].as_array().into_iter().flatten() {
        let Some(method) = m["name"].as_str() else {
            continue;
        };
        if EXCLUDED.contains(&method) {
            continue;
        }
        let mut name = tool_name(method);
        // Disambiguate collisions (e.g. `a.b` vs `a_b`).
        let mut n = 2;
        while used.contains_key(&name) {
            let suffix = format!("_{n}");
            name = format!(
                "{}{suffix}",
                tool_name(method)
                    .chars()
                    .take(64 - suffix.len())
                    .collect::<String>()
            );
            n += 1;
        }
        used.insert(name.clone(), ());
        let mut schema = m["params_schema"].clone();
        if !schema.is_object() {
            schema = json!({"type": "object"});
        }
        if schema.get("type").is_none() {
            schema["type"] = "object".into();
        }
        out.push(ToolEntry {
            name,
            method: Some(method.to_string()),
            description: m["description"].as_str().unwrap_or("").to_string(),
            schema,
        });
    }
    out.push(wait_tool());
    out
}

fn rpc_ok(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_err(id: &Value, code: i64, message: impl Into<String>) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.into()}})
}

fn tool_text(text: String, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

/// Compact JSON: error text stays short (the message is already capped).
fn compact(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}

/// The connection broke (as opposed to the server answering with an error or timing out).
fn is_transport(e: &ClientError) -> bool {
    matches!(
        e,
        ClientError::Io(_) | ClientError::Closed | ClientError::Protocol(_)
    )
}

fn unreachable_msg(e: &ClientError) -> String {
    format!("{NOT_RUNNING}. ({e})")
}

/// Why a tool call produced no result.
enum Failure {
    /// Could not connect (nothing was sent).
    Unreachable(ClientError),
    /// Failed after the request went out.
    Call(ClientError),
}

/// What the bridge knows about the instance and its tools.
#[derive(Default)]
struct State {
    tools: Vec<ToolEntry>,
    by_name: HashMap<String, usize>,
    /// `tools` is the static catalog, not the live list.
    static_list: bool,
    /// The client has asked for the list at least once.
    listed: bool,
    /// `instance_id` of the last instance we talked to.
    instance_id: Option<String>,
    /// The next tool result must announce a restart.
    restart_pending: bool,
}

impl State {
    /// Installs `tools`; returns whether the list changed.
    fn set_tools(&mut self, tools: Vec<ToolEntry>, is_static: bool) -> bool {
        let changed = self.tools != tools;
        self.by_name = tools
            .iter()
            .enumerate()
            .map(|(i, t)| (t.name.clone(), i))
            .collect();
        self.tools = tools;
        self.static_list = is_static;
        changed
    }

    /// Makes sure there is some list to answer from.
    fn ensure_static(&mut self) {
        if self.tools.is_empty() {
            let list = json!({ "methods": method_catalog() });
            self.set_tools(build_tools(&list), true);
        }
    }
}

struct Core {
    connector: Connector,
    state: Mutex<State>,
    slots: Semaphore,
    /// Protocol version negotiated by `initialize` (empty before).
    protocol: Mutex<String>,
}

type Out = mpsc::UnboundedSender<Value>;

pub struct McpServer {
    core: Arc<Core>,
}

impl Core {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Opens a control connection and notes whether the instance changed since the last one.
    /// Returns the client and whether a restart was just detected.
    async fn connect(&self) -> Result<(ControlClient, bool), ClientError> {
        let client = (self.connector)().await?;
        let mut restarted = false;
        if let Some(id) = client.instance_id() {
            let mut st = self.state();
            if st.instance_id.as_deref().is_some_and(|old| old != id) {
                st.restart_pending = true;
                restarted = true;
            }
            st.instance_id = Some(id.to_string());
        }
        Ok((client, restarted))
    }

    /// Loads the live tool list over `client`. Tells the MCP client (`out`) if it replaces a
    /// list that was already served and differs.
    async fn load_live_tools(
        &self,
        client: &mut ControlClient,
        out: Option<&Out>,
    ) -> Result<(), ClientError> {
        let list = client.call("methods.list", Value::Null).await?;
        let tools = build_tools(&list);
        let (changed, listed) = {
            let mut st = self.state();
            let changed = st.set_tools(tools, false);
            (changed, st.listed)
        };
        if changed
            && listed
            && let Some(out) = out
        {
            let _ =
                out.send(json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}));
        }
        Ok(())
    }

    /// After a successful connect: replace the static list or a list from before a restart.
    async fn sync_tools(&self, client: &mut ControlClient, restarted: bool, out: &Out) {
        let need = {
            let st = self.state();
            st.static_list || st.tools.is_empty() || restarted
        };
        if need && let Err(e) = self.load_live_tools(client, Some(out)).await {
            eprintln!("siple mcp: cannot load the tool list: {e}");
        }
    }

    async fn tools_list(&self) -> Result<Value, (i64, String)> {
        match self.connect().await {
            Ok((mut client, _)) => {
                if let Err(e) = self.load_live_tools(&mut client, None).await {
                    match e {
                        ClientError::Api(a) => return Err((-32603, a.to_string())),
                        // The instance went away mid-request: serve what we have.
                        e => eprintln!("siple mcp: cannot load the tool list: {e}"),
                    }
                }
            }
            Err(e) => {
                eprintln!(
                    "siple mcp: {} — serving the static tool list",
                    unreachable_msg(&e)
                );
            }
        }
        let mut st = self.state();
        st.ensure_static();
        st.listed = true;
        let tools: Vec<Value> = st
            .tools
            .iter()
            .map(|t| json!({"name": t.name, "description": t.description, "inputSchema": t.schema}))
            .collect();
        Ok(json!({"tools": tools}))
    }

    /// The tool called `name`, if listed.
    fn find(&self, name: &str) -> Option<ToolEntry> {
        let st = self.state();
        st.by_name.get(name).map(|&i| st.tools[i].clone())
    }

    async fn tools_call(&self, params: Value, out: &Out) -> Result<Value, (i64, String)> {
        let name = params["name"]
            .as_str()
            .ok_or((-32602, "missing tool name".to_string()))?
            .to_string();
        let args = match params.get("arguments") {
            None | Some(Value::Null) => json!({}),
            Some(a) if a.is_object() => a.clone(),
            Some(_) => return Err((-32602, "arguments must be an object".into())),
        };
        self.state().ensure_static();

        let mut client: Option<ControlClient> = None;
        let mut connect_err: Option<ClientError> = None;
        let mut entry = self.find(&name);
        if entry.is_none() {
            // Not in the static catalog / not listed yet (or the instance runs an older or
            // different build): ask the instance.
            match self.connect().await {
                Ok((mut c, _)) => {
                    if let Err(e) = self.load_live_tools(&mut c, Some(out)).await {
                        connect_err = Some(e);
                    }
                    client = Some(c);
                    entry = self.find(&name);
                }
                Err(e) => connect_err = Some(e),
            }
        }
        let Some(entry) = entry else {
            return match connect_err {
                Some(e) => Ok(self.finish(tool_text(unreachable_msg(&e), true))),
                None => Err((-32602, format!("unknown tool: {name}"))),
            };
        };
        let (method, params) = match &entry.method {
            Some(m) => (m.clone(), args),
            None => ("events.poll".to_string(), wait_params(&args)),
        };

        let _slot = self.slots.acquire().await;
        let mut attempts = 0;
        let result = loop {
            attempts += 1;
            let mut c = match client.take() {
                Some(c) => c,
                None => match self.connect().await {
                    Ok((mut c, r)) => {
                        self.sync_tools(&mut c, r, out).await;
                        c
                    }
                    Err(e) => break Err(Failure::Unreachable(e)),
                },
            };
            match c.call(&method, params.clone()).await {
                Err(e) if is_transport(&e) && is_idempotent(&method) && attempts < 2 => {
                    eprintln!("siple mcp: connection lost ({e}); retrying {method}");
                }
                other => break other.map_err(Failure::Call),
            }
        };
        let r = match result {
            Ok(value) => {
                let structured = if value.is_object() {
                    value.clone()
                } else {
                    json!({"result": value})
                };
                let (value, structured, audio) = strip_audio(value, structured);
                let supports_audio = self.supports_audio();
                let mut text = pretty(&value);
                if audio.is_some() && !supports_audio {
                    text.push_str(AUDIO_NOTE);
                }
                let mut r = tool_text(text, false);
                r["structuredContent"] = structured;
                if let (Some(b64), true) = (audio, supports_audio) {
                    r["content"]
                        .as_array_mut()
                        .expect("content is an array")
                        .push(json!({"type": "audio", "data": b64, "mimeType": "audio/wav"}));
                }
                r
            }
            Err(Failure::Call(ClientError::Api(e))) => tool_text(compact(&e.to_wire()), true),
            Err(Failure::Call(e @ ClientError::Timeout { .. })) => tool_text(e.to_string(), true),
            Err(Failure::Call(e)) => tool_text(
                format!(
                    "Connection to Siple lost during `{method}` ({e}).{}",
                    if is_idempotent(&method) {
                        ""
                    } else {
                        " The request may or may not have been executed; check `status` before repeating it."
                    }
                ),
                true,
            ),
            Err(Failure::Unreachable(e)) => tool_text(unreachable_msg(&e), true),
        };
        Ok(self.finish(r))
    }

    /// Whether the negotiated protocol version has audio content blocks.
    fn supports_audio(&self) -> bool {
        let v = self.protocol.lock().unwrap_or_else(PoisonError::into_inner);
        version_at_least(&v, AUDIO_MIN_VERSION)
    }

    /// Adds the restart notice to the result if one is pending.
    fn finish(&self, mut r: Value) -> Value {
        let restarted = std::mem::take(&mut self.state().restart_pending);
        if restarted {
            let text = r["content"][0]["text"].as_str().unwrap_or("").to_string();
            r["content"][0]["text"] = format!("{RESTART_NOTICE}\n\n{text}").into();
            match r.get_mut("structuredContent") {
                Some(Value::Object(m)) => {
                    m.insert("restarted".into(), true.into());
                }
                _ => r["structuredContent"] = json!({"restarted": true}),
            }
        }
        r
    }
}

impl McpServer {
    pub fn new(connector: Connector) -> Self {
        Self {
            core: Arc::new(Core {
                connector,
                state: Mutex::new(State::default()),
                slots: Semaphore::new(MAX_CONCURRENT),
                protocol: Mutex::new(String::new()),
            }),
        }
    }

    /// Reads newline-delimited messages from `input` until EOF, writing responses to `output`.
    /// Slow requests run in their own tasks, so responses may be out of order.
    pub async fn serve<R, W>(&self, mut input: R, mut output: W) -> std::io::Result<()>
    where
        R: AsyncBufReadExt + Unpin,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
        let writer = tokio::spawn(async move {
            while let Some(v) = rx.recv().await {
                let mut s = serde_json::to_string(&v).expect("serializes");
                s.push('\n');
                if output.write_all(s.as_bytes()).await.is_err() || output.flush().await.is_err() {
                    break;
                }
            }
        });
        let mut tasks: JoinSet<()> = JoinSet::new();
        let mut line = String::new();
        let outcome = loop {
            line.clear();
            match input.read_line(&mut line).await {
                Ok(0) => break Ok(()),
                Err(e) => break Err(e),
                Ok(_) => {}
            }
            let text = line.trim();
            if text.is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(text) {
                Err(e) => {
                    let _ = tx.send(rpc_err(&Value::Null, -32700, format!("parse error: {e}")));
                }
                Ok(v) => self.route(v, &tx, &mut tasks),
            }
            // Reap finished tasks so the set does not grow without bound.
            while tasks.try_join_next().is_some() {}
        };
        // Input closed: let in-flight requests answer (bounded), then stop.
        let _ =
            tokio::time::timeout(DRAIN, async { while tasks.join_next().await.is_some() {} }).await;
        tasks.abort_all();
        drop(tx);
        let _ = writer.await;
        outcome
    }

    /// Handles cheap messages inline and spawns the ones that talk to the instance.
    fn route(&self, msg: Value, tx: &Out, tasks: &mut JoinSet<()>) {
        if !msg.is_object() {
            let _ = tx.send(rpc_err(
                &Value::Null,
                -32600,
                "batches and non-object messages are not supported",
            ));
            return;
        }
        let id = msg.get("id").cloned();
        let Some(method) = msg["method"].as_str().map(str::to_owned) else {
            // A response to something we never sent, or garbage.
            if let Some(id) = id {
                let _ = tx.send(rpc_err(&id, -32600, "invalid request: missing method"));
            }
            return;
        };
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let Some(id) = id else {
            // Notification (initialized, cancelled, ...): nothing to do.
            return;
        };
        match method.as_str() {
            "tools/list" | "tools/call" => {
                let core = self.core.clone();
                let tx = tx.clone();
                tasks.spawn(async move {
                    let r = if method == "tools/list" {
                        core.tools_list().await
                    } else {
                        core.tools_call(params, &tx).await
                    };
                    let _ = tx.send(match r {
                        Ok(r) => rpc_ok(&id, r),
                        Err((code, m)) => rpc_err(&id, code, m),
                    });
                });
            }
            _ => {
                let _ = tx.send(match self.dispatch_inline(&method, &params) {
                    Ok(r) => rpc_ok(&id, r),
                    Err((code, m)) => rpc_err(&id, code, m),
                });
            }
        }
    }

    fn dispatch_inline(&self, method: &str, params: &Value) -> Result<Value, (i64, String)> {
        match method {
            "initialize" => {
                let want = params["protocolVersion"].as_str().unwrap_or("");
                let version = if SUPPORTED_VERSIONS.contains(&want) {
                    want
                } else {
                    SUPPORTED_VERSIONS[0]
                };
                *self
                    .core
                    .protocol
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = version.to_string();
                Ok(json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": true}},
                    "serverInfo": {"name": "siple", "version": siple_core::update::full_version()},
                    "instructions": INSTRUCTIONS,
                }))
            }
            "ping" => Ok(json!({})),
            other => Err((-32601, format!("method not found: {other}"))),
        }
    }
}

/// Appended to the text when the client's protocol version cannot carry audio blocks.
const AUDIO_NOTE: &str = "\n\nNote: the audio was omitted (this MCP session cannot carry audio content). Use audio:\"file\" with a path, or transcribe:true.";

/// Date-style protocol versions compare lexicographically.
fn version_at_least(v: &str, min: &str) -> bool {
    !v.is_empty() && v >= min
}

/// Removes `audio.base64` from the result (text source and structured copy), marking
/// `audio.base64_omitted`; returns the base64 so it can travel as an audio content block.
/// Base64 must never reach the model as text.
fn strip_audio(mut value: Value, mut structured: Value) -> (Value, Value, Option<String>) {
    let mut found = None;
    for v in [&mut value, &mut structured] {
        if let Some(Value::Object(a)) = v.get_mut("audio")
            && let Some(b) = a.remove("base64")
        {
            a.insert("base64_omitted".into(), true.into());
            if let Value::String(s) = b {
                found.get_or_insert(s);
            }
        }
    }
    (value, structured, found)
}

/// Maps `wait_for_event` arguments to `events.poll` params (clamping the timeout).
fn wait_params(args: &Value) -> Value {
    let mut p = serde_json::Map::new();
    for k in ["kinds", "account", "call_id", "since"] {
        if let Some(v) = args.get(k).filter(|v| !v.is_null()) {
            p.insert(k.into(), v.clone());
        }
    }
    let t = args["timeout_ms"]
        .as_u64()
        .unwrap_or(DEFAULT_WAIT_MS)
        .min(MAX_WAIT_MS);
    p.insert("timeout_ms".into(), t.into());
    Value::Object(p)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use siple_core::config::Secret;
    use siple_core::control::{CancellationToken, ControlContext, ControlServer, StubBackend};
    use siple_core::events::EventLog;
    use siple_core::instance::ControlEndpoints;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, duplex};

    struct Fx {
        server: Option<ControlServer>,
        log: Arc<EventLog>,
        sock: String,
        _dir: tempfile::TempDir,
    }

    async fn start(dir: tempfile::TempDir, log: Arc<EventLog>) -> Fx {
        let ctx = ControlContext::new(log.clone(), Arc::new(StubBackend), None, Secret::new("tok"));
        let req = ControlEndpoints {
            unix_socket: Some(dir.path().join("c.sock").to_string_lossy().into_owned()),
            ..Default::default()
        };
        let server = ControlServer::start(&req, ctx, CancellationToken::new())
            .await
            .unwrap();
        let sock = server.endpoints().unix_socket.clone().unwrap();
        Fx {
            server: Some(server),
            log,
            sock,
            _dir: dir,
        }
    }

    fn connector(sock: String) -> Connector {
        Arc::new(move || {
            let sock = sock.clone();
            Box::pin(async move {
                let mut c = ControlClient::connect_unix(&sock).await?;
                c.hello(None, "mcp-test").await?;
                Ok(c)
            })
        })
    }

    /// Sends `lines` through the server over in-memory pipes, waits for all answers and returns
    /// every message the server wrote (responses in completion order, then notifications).
    async fn drive(server: &McpServer, lines: &[Value]) -> Vec<Value> {
        let (mut cin, sin) = duplex(1 << 16);
        let (sout, mut cout) = duplex(1 << 20);
        for l in lines {
            cin.write_all(format!("{l}\n").as_bytes()).await.unwrap();
        }
        drop(cin);
        server.serve(BufReader::new(sin), sout).await.unwrap();
        let mut s = String::new();
        cout.read_to_string(&mut s).await.unwrap();
        s.lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// The response to request `id`.
    fn by_id(out: &[Value], id: u64) -> &Value {
        out.iter()
            .find(|m| m["id"] == id)
            .unwrap_or_else(|| panic!("no response with id {id} in {out:?}"))
    }

    fn notifications(out: &[Value]) -> Vec<&str> {
        out.iter()
            .filter(|m| m.get("id").is_none())
            .filter_map(|m| m["method"].as_str())
            .collect()
    }

    fn req(id: u64, method: &str, params: Value) -> Value {
        json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
    }

    fn tool_names(resp: &Value) -> Vec<&str> {
        resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn name_mapping() {
        assert_eq!(tool_name("call.dial"), "call_dial");
        assert_eq!(tool_name("a-b_c"), "a-b_c");
        assert_eq!(tool_name(&"x".repeat(100)).len(), 64);
        let tools = build_tools(&json!({"methods":[
            {"name":"a.b","description":"","params_schema":{}},
            {"name":"a_b","description":"","params_schema":{"type":"object"}},
            {"name":"hello","description":"","params_schema":{}},
            {"name":"events.subscribe","description":"","params_schema":{}}]}));
        let names: Vec<_> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["a_b", "a_b_2", WAIT_TOOL]);
        assert_eq!(tools[0].method.as_deref(), Some("a.b"));
        assert_eq!(tools[0].schema["type"], "object");
    }

    #[test]
    fn audio_stripping_and_version_gate() {
        let v = json!({"speech": true, "audio": {"format": "wav", "base64": "UklGRg=="}});
        let (v2, st, b64) = strip_audio(v.clone(), v);
        assert_eq!(b64.as_deref(), Some("UklGRg=="));
        for x in [&v2, &st] {
            assert!(x["audio"].get("base64").is_none());
            assert_eq!(x["audio"]["base64_omitted"], true);
            assert!(!x.to_string().contains("UklGRg"));
        }
        let (_, _, none) = strip_audio(json!({"audio": {"path": "/x.wav"}}), json!({}));
        assert!(none.is_none());
        assert!(version_at_least("2025-06-18", AUDIO_MIN_VERSION));
        assert!(version_at_least("2025-03-26", AUDIO_MIN_VERSION));
        assert!(!version_at_least("2024-11-05", AUDIO_MIN_VERSION));
        assert!(!version_at_least("", AUDIO_MIN_VERSION));
    }

    #[test]
    fn call_stream_is_excluded() {
        let tools = build_tools(&json!({"methods":[
            {"name":"call.stream","description":"","params_schema":{}},
            {"name":"call.listen","description":"","params_schema":{}}]}));
        let names: Vec<_> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["call_listen", WAIT_TOOL]);
        let cat: Vec<_> = method_catalog().into_iter().map(|m| m.name).collect();
        assert!(
            cat.iter().any(|n| n == "call.stream"),
            "documented in methods.list"
        );
        assert!(
            !build_tools(&json!({"methods": method_catalog()}))
                .iter()
                .any(|t| t.name == "call_stream")
        );
    }

    #[test]
    fn call_activate_and_answer_modes_are_tools() {
        let tools = build_tools(&json!({"methods": method_catalog()}));
        assert!(tools.iter().any(|t| t.name == "call_activate"));
        let answer = tools.iter().find(|t| t.name == "call_answer").unwrap();
        let props = &answer.schema["properties"];
        assert!(props["mode"].is_object() && props["end_call_id"].is_object());
    }

    #[tokio::test]
    async fn audio_result_gated_by_protocol_version() {
        let fx = start(tempfile::tempdir().unwrap(), Arc::new(EventLog::new(100))).await;
        let args = json!({"audio": {"format": "wav", "base64": "UklGRg=="}, "speech": true});
        for (ver, with_block) in [
            ("2025-06-18", true),
            ("2025-03-26", true),
            ("2024-11-05", false),
        ] {
            let s = McpServer::new(connector(fx.sock.clone()));
            let out = drive(
                &s,
                &[
                    req(1, "initialize", json!({"protocolVersion": ver})),
                    req(2, "tools/call", json!({"name":"echo","arguments":args})),
                ],
            )
            .await;
            let r = &by_id(&out, 2)["result"];
            let content = r["content"].as_array().unwrap();
            let text = content[0]["text"].as_str().unwrap();
            assert!(!text.contains("UklGRg"), "base64 leaked into text ({ver})");
            assert!(text.contains("base64_omitted"));
            assert_eq!(r["structuredContent"]["audio"]["base64_omitted"], true);
            assert!(r["structuredContent"]["audio"].get("base64").is_none());
            if with_block {
                assert_eq!(content.len(), 2, "{ver}");
                assert_eq!(content[1]["type"], "audio");
                assert_eq!(content[1]["data"], "UklGRg==");
                assert_eq!(content[1]["mimeType"], "audio/wav");
            } else {
                assert_eq!(content.len(), 1, "{ver}");
                assert!(text.contains("audio:\"file\""));
            }
        }
    }

    #[test]
    fn wait_param_clamping() {
        let p = wait_params(&json!({"kinds":["call.*"],"since":3,"timeout_ms":999999,"x":1}));
        assert_eq!(p, json!({"kinds":["call.*"],"since":3,"timeout_ms":60000}));
        assert_eq!(wait_params(&json!({}))["timeout_ms"], 10000);
    }

    #[tokio::test]
    async fn initialize_list_call() {
        let fx = start(tempfile::tempdir().unwrap(), Arc::new(EventLog::new(100))).await;
        let s = McpServer::new(connector(fx.sock.clone()));
        let out = drive(
            &s,
            &[
                req(
                    1,
                    "initialize",
                    json!({"protocolVersion":"2025-03-26","capabilities":{}}),
                ),
                json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
                json!({"jsonrpc":"2.0","method":"notifications/whatever"}),
                req(2, "ping", json!({})),
                req(3, "tools/list", json!({})),
                req(5, "tools/call", json!({"name":"echo","arguments":{"a":1}})),
                req(6, "tools/call", json!({"name":"nope"})),
                req(7, "bogus/method", json!({})),
                req(
                    8,
                    "tools/call",
                    json!({"name":"wait_for_event","arguments":{"kinds":["x.*"],"timeout_ms":50}}),
                ),
            ],
        )
        .await;
        assert_eq!(out.len(), 7, "notifications get no reply: {out:?}");
        let init = by_id(&out, 1);
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(init["result"]["serverInfo"]["name"], "siple");
        assert_eq!(
            init["result"]["capabilities"]["tools"]["listChanged"], true,
            "the static list can be replaced by the live one"
        );
        assert!(
            init["result"]["instructions"]
                .as_str()
                .unwrap()
                .contains("wait_for_event")
        );
        assert_eq!(by_id(&out, 2)["result"], json!({}));
        let names = tool_names(by_id(&out, 3));
        for n in [
            "status",
            "echo",
            "ping",
            "events_poll",
            "logs_tail",
            WAIT_TOOL,
        ] {
            assert!(names.contains(&n), "{n} missing in {names:?}");
        }
        for n in ["hello", "events_subscribe", "events_unsubscribe"] {
            assert!(!names.contains(&n), "{n} should be excluded");
        }
        assert_eq!(
            by_id(&out, 5)["result"]["structuredContent"],
            json!({"a":1})
        );
        assert_eq!(by_id(&out, 6)["error"]["code"], -32602);
        assert_eq!(by_id(&out, 7)["error"]["code"], -32601);
        assert_eq!(
            by_id(&out, 8)["result"]["structuredContent"]["events"],
            json!([])
        );

        // A status call returns the stub's object, as text and as structured content.
        let out = drive(
            &s,
            &[req(
                1,
                "tools/call",
                json!({"name":"status","arguments":{}}),
            )],
        )
        .await;
        let r = &by_id(&out, 1)["result"];
        assert_eq!(r["isError"], false);
        assert_eq!(r["structuredContent"]["stub"], true);
        let text = r["content"][0]["text"].as_str().unwrap();
        assert_eq!(serde_json::from_str::<Value>(text).unwrap()["stub"], true);
    }

    #[tokio::test]
    async fn unknown_version_falls_back_and_api_error() {
        let fx = start(tempfile::tempdir().unwrap(), Arc::new(EventLog::new(100))).await;
        let s = McpServer::new(connector(fx.sock.clone()));
        let out = drive(
            &s,
            &[
                req(1, "initialize", json!({"protocolVersion":"1999-01-01"})),
                // No logging handle -> internal error.
                req(
                    2,
                    "tools/call",
                    json!({"name":"logs_tail","arguments":{"lines":5}}),
                ),
                req(
                    3,
                    "tools/call",
                    json!({"name":"events_poll","arguments":{"bogus":1}}),
                ),
            ],
        )
        .await;
        assert_eq!(by_id(&out, 1)["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(by_id(&out, 2)["result"]["isError"], true);
        assert_eq!(by_id(&out, 3)["result"]["isError"], true);
        let text = by_id(&out, 3)["result"]["content"][0]["text"]
            .as_str()
            .unwrap();
        assert!(text.contains("invalid_params"), "{text}");
    }

    #[tokio::test]
    async fn wait_for_event_since_and_stale_since() {
        let fx = start(tempfile::tempdir().unwrap(), Arc::new(EventLog::new(100))).await;
        let before = fx.log.current_seq();
        fx.log
            .emit("call.state", None, Some("c1"), json!({"state":"active"}));
        let s = McpServer::new(connector(fx.sock.clone()));
        let out = drive(
            &s,
            &[
                req(
                    1,
                    "tools/call",
                    json!({"name":"wait_for_event","arguments":
                    {"kinds":["call.state"],"call_id":"c1","since":before,"timeout_ms":2000}}),
                ),
                req(
                    2,
                    "tools/call",
                    json!({"name":"wait_for_event","arguments":{"since":before + 50,"timeout_ms":2000}}),
                ),
            ],
        )
        .await;
        let r = &by_id(&out, 1)["result"]["structuredContent"];
        assert_eq!(r["events"].as_array().unwrap().len(), 1);
        assert_eq!(r["next_seq"], before + 1);
        // A `since` from a previous run fails at once with a pointer to events.current_seq.
        let stale = &by_id(&out, 2)["result"];
        assert_eq!(stale["isError"], true);
        let text = stale["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("invalid_params") && text.contains("events.current_seq"),
            "{text}"
        );
    }

    /// Item 4: the tool list does not depend on the phone running.
    #[tokio::test]
    async fn static_tool_list_without_instance_then_list_changed() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("c.sock").to_string_lossy().into_owned();
        let s = McpServer::new(connector(sock));
        let out = drive(
            &s,
            &[
                req(1, "initialize", json!({"protocolVersion":"2025-06-18"})),
                req(2, "tools/call", json!({"name":"status","arguments":{}})),
                req(3, "tools/list", json!({})),
            ],
        )
        .await;
        assert!(
            by_id(&out, 1)["result"].is_object(),
            "initialize works without the phone"
        );
        let r = &by_id(&out, 2)["result"];
        assert_eq!(r["isError"], true);
        let t = r["content"][0]["text"].as_str().unwrap();
        assert!(t.contains("Siple is not running"), "{t}");
        let names = tool_names(by_id(&out, 3));
        for n in [
            "status",
            "call_dial",
            "call_dtmf",
            "config_reload",
            "events_poll",
            WAIT_TOOL,
        ] {
            assert!(
                names.contains(&n),
                "{n} missing from the static list {names:?}"
            );
        }
        assert!(notifications(&out).is_empty());

        // The phone appears (a stub with a different method set): the first call that connects
        // swaps in the live list and tells the client.
        let fx = start(dir, Arc::new(EventLog::new(100))).await;
        let out = drive(&s, &[req(4, "tools/call", json!({"name":"status"}))]).await;
        assert_eq!(by_id(&out, 4)["result"]["isError"], false);
        assert_eq!(notifications(&out), ["notifications/tools/list_changed"]);
        let out = drive(&s, &[req(5, "tools/list", json!({}))]).await;
        let names = tool_names(by_id(&out, 5));
        assert!(
            names.contains(&"echo") && !names.contains(&"call_dial"),
            "{names:?}"
        );
        assert!(notifications(&out).is_empty(), "no change, no notification");
        drop(fx);
    }

    /// Item 3: a restart is reported on the next result.
    #[tokio::test]
    async fn restart_is_announced_once() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("c.sock").to_string_lossy().into_owned();
        let fx = start(dir, Arc::new(EventLog::new(100))).await;
        let s = McpServer::new(connector(sock));
        let out = drive(&s, &[req(1, "tools/call", json!({"name":"status"}))]).await;
        let r = &by_id(&out, 1)["result"];
        assert_eq!(r["isError"], false);
        assert!(r["structuredContent"].get("restarted").is_none());
        assert!(
            !r["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("restarted")
        );

        // Same instance, new connection: nothing to report.
        let out = drive(&s, &[req(2, "tools/call", json!({"name":"status"}))]).await;
        assert!(
            by_id(&out, 2)["result"]["structuredContent"]
                .get("restarted")
                .is_none()
        );

        // The instance restarts (fresh event log => new instance_id).
        let Fx { server, _dir, .. } = fx;
        server.unwrap().shutdown().await;
        let fx = start(_dir, Arc::new(EventLog::new(100))).await;
        let out = drive(&s, &[req(3, "tools/call", json!({"name":"status"}))]).await;
        let r = &by_id(&out, 3)["result"];
        assert_eq!(r["isError"], false, "{out:?}");
        assert_eq!(r["structuredContent"]["restarted"], true);
        assert_eq!(
            r["structuredContent"]["stub"], true,
            "the payload is still there"
        );
        let text = r["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("Siple restarted"), "{text}");
        assert!(text.contains("\"stub\""), "{text}");

        // Reported once.
        let out = drive(&s, &[req(4, "tools/call", json!({"name":"status"}))]).await;
        assert!(
            by_id(&out, 4)["result"]["structuredContent"]
                .get("restarted")
                .is_none()
        );
        drop(fx);
    }

    /// Item 5: `ping` and short calls are not blocked behind a long wait.
    #[tokio::test]
    async fn ping_is_not_blocked_by_a_long_wait() {
        let fx = start(tempfile::tempdir().unwrap(), Arc::new(EventLog::new(100))).await;
        let s = Arc::new(McpServer::new(connector(fx.sock.clone())));
        let (mut cin, sin) = duplex(1 << 16);
        let (sout, cout) = duplex(1 << 16);
        let srv = {
            let s = s.clone();
            tokio::spawn(async move { s.serve(BufReader::new(sin), sout).await })
        };
        let long = req(
            1,
            "tools/call",
            json!({"name":"wait_for_event","arguments":{"timeout_ms":1500}}),
        );
        cin.write_all(format!("{long}\n{}\n", req(2, "ping", json!({}))).as_bytes())
            .await
            .unwrap();
        let mut lines = BufReader::new(cout).lines();
        let t = std::time::Instant::now();
        let first: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(first["id"], 2, "ping answered first: {first}");
        assert!(t.elapsed() < Duration::from_millis(1000));
        // A short call also completes while the long one is still waiting.
        cin.write_all(format!("{}\n", req(3, "tools/call", json!({"name":"status"}))).as_bytes())
            .await
            .unwrap();
        let second: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(second["id"], 3, "{second}");
        assert!(t.elapsed() < Duration::from_millis(1000));
        let third: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(third["id"], 1);
        drop(cin);
        srv.await.unwrap().unwrap();
    }

    /// A server that completes `hello` and then drops every connection on its first request.
    /// Returns the socket path and the number of connections accepted.
    async fn flaky_server(dir: &std::path::Path) -> (String, Arc<AtomicUsize>) {
        let path = dir.join("flaky.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                c.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let (rd, mut wr) = stream.into_split();
                    let mut rd = BufReader::new(rd);
                    let mut line = String::new();
                    // hello
                    if rd.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let id = serde_json::from_str::<Value>(&line).unwrap()["id"].clone();
                    let reply = json!({"jsonrpc":"2.0","id":id,"result":{"instance_id":"flaky"}});
                    let _ = wr.write_all(format!("{reply}\n").as_bytes()).await;
                    line.clear();
                    // The first real request: read it, then hang up without answering.
                    let _ = rd.read_line(&mut line).await;
                });
            }
        });
        (path.to_string_lossy().into_owned(), count)
    }

    /// Item 9: only idempotent reads are retried after a lost connection.
    #[tokio::test]
    async fn only_idempotent_methods_are_retried() {
        let dir = tempfile::tempdir().unwrap();
        let (sock, count) = flaky_server(dir.path()).await;
        let s = McpServer::new(connector(sock));
        // Prime the tool list from the static catalog; the flaky server never answers
        // methods.list, so sync attempts only add connections we account for below.
        let out = drive(
            &s,
            &[req(
                1,
                "tools/call",
                json!({"name":"call_dial","arguments":{"account":"a","target":"b"}}),
            )],
        )
        .await;
        let r = &by_id(&out, 1)["result"];
        assert_eq!(r["isError"], true);
        let text = r["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("may or may not have been executed"), "{text}");
        let after_dial = count.load(Ordering::SeqCst);
        assert!(after_dial >= 1);

        let before = count.load(Ordering::SeqCst);
        let out = drive(
            &s,
            &[req(
                2,
                "tools/call",
                json!({"name":"call_dial","arguments":{"account":"a","target":"b"}}),
            )],
        )
        .await;
        assert_eq!(by_id(&out, 2)["result"]["isError"], true);
        let dial_conns = count.load(Ordering::SeqCst) - before;

        let before = count.load(Ordering::SeqCst);
        let out = drive(
            &s,
            &[req(
                3,
                "tools/call",
                json!({"name":"status","arguments":{}}),
            )],
        )
        .await;
        assert_eq!(by_id(&out, 3)["result"]["isError"], true);
        let status_conns = count.load(Ordering::SeqCst) - before;
        assert!(
            status_conns > dial_conns,
            "status (read) is retried on a fresh connection, call_dial is not: {status_conns} vs {dial_conns}"
        );
    }
}

//! `siple-cli`: headless runner, control client and MCP bridge.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use serde_json::{Value, json};
use siple_core::config;
use siple_core::control::client::{ClientError, ControlClient};
use siple_core::instance::RuntimeInfo;
use siple_core::{App, AppError, AppOptions};

mod mcp;

#[derive(Parser)]
#[command(
    name = "siple-cli",
    version = siple_core::update::full_version(),
    about = "Siple SIP test softphone (console)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the engine in this terminal.
    Run(RunArgs),
    /// Call a control API method on a running instance.
    Ctl(CtlArgs),
    /// Stream events from a running instance (one JSON event per line).
    Events(EventsArgs),
    /// Stream a call's audio over a binary upgrade of a control connection (raw PCM on
    /// stdin/stdout, JSON events on stderr).
    Stream(StreamArgs),
    /// Serve MCP over stdio, proxying to a running instance.
    Mcp(ConnectArgs),
    /// Print the control API method catalog (offline; no instance needed).
    Methods(MethodsArgs),
    /// Print the commented example config (the one written on first run).
    ConfigExample,
    /// Print the agent skill (SKILL.md) for this version: how an LLM agent drives Siple with
    /// `siple-cli`. Save it as `<skills dir>/siple/SKILL.md`.
    Skill,
}

/// The agent skill, kept in the repository at `skills/siple/SKILL.md`.
const SKILL_MD: &str = include_str!("../../../skills/siple/SKILL.md");

#[derive(Args)]
struct MethodsArgs {
    /// Markdown reference (used to generate docs/API.md).
    #[arg(long, conflicts_with = "json")]
    markdown: bool,
    /// JSON array of {name, description, params_schema}.
    #[arg(long)]
    json: bool,
}

/// How to reach a running instance. Default: discover via `runtime.json` in the config dir.
#[derive(Args)]
struct ConnectArgs {
    /// Config file whose directory holds `runtime.json`; defaults to the platform config dir.
    #[arg(long, env = "SIPLE_CONFIG")]
    config: Option<PathBuf>,
    /// Unix socket path (overrides discovery).
    #[arg(long, conflicts_with_all = ["pipe", "tcp"])]
    socket: Option<PathBuf>,
    /// Windows named pipe name (overrides discovery).
    #[arg(long, conflicts_with_all = ["socket", "tcp"])]
    pipe: Option<String>,
    /// TCP address (overrides discovery); requires --token.
    #[arg(long, conflicts_with_all = ["socket", "pipe"], requires = "token")]
    tcp: Option<String>,
    /// Auth token (default: from runtime.json).
    #[arg(long)]
    token: Option<String>,
}

#[derive(Args)]
struct CtlArgs {
    /// Method name, e.g. `status` or `call.dial`.
    method: String,
    /// JSON params object; `@file.json` reads it from a file, `-` from stdin (inline params
    /// over 128 KiB do not fit in a Linux command line).
    json: Option<String>,
    #[command(flatten)]
    connect: ConnectArgs,
}

#[derive(Args)]
struct StreamArgs {
    /// Call id, e.g. `c1`.
    call_id: String,
    /// Sample rate: 8000, 16000, 24000 or 48000.
    #[arg(long, default_value_t = 16000)]
    rate: u32,
    /// Sample encoding: s16le, f32le, mulaw or alaw.
    #[arg(long, default_value = "s16le")]
    encoding: String,
    /// Frame length in ms: 10, 20, 40 or 60.
    #[arg(long, default_value_t = 20)]
    frame_ms: u32,
    /// Audio to send into the call: a file, or `-` for stdin (default). Raw PCM in --encoding
    /// at --rate, except that a file starting with a RIFF/WAVE header is decoded and converted
    /// to --rate / --encoding (mono). When the audio ends the CLI sends a `mark` named
    /// `__tx_done`; the server answers it with a `mark` event once everything before it has
    /// been played, and with --no-rx that event ends the session.
    #[arg(long, conflicts_with = "no_tx")]
    tx: Option<String>,
    /// Where to write the call's raw PCM: a file, or `-` for stdout (default).
    #[arg(long, conflicts_with = "no_rx")]
    rx: Option<String>,
    /// Do not send audio.
    #[arg(long)]
    no_tx: bool,
    /// Do not receive audio.
    #[arg(long)]
    no_rx: bool,
    /// Hang the call up when the stream closes by the client side.
    #[arg(long)]
    hangup_on_close: bool,
    /// Send a TX file as fast as the server accepts it instead of in real time.
    #[arg(long)]
    no_pace: bool,
    #[command(flatten)]
    connect: ConnectArgs,
}

#[derive(Args)]
struct EventsArgs {
    /// Comma-separated kinds; exact or prefix with `*` (e.g. `call.*,account.*`).
    #[arg(long, value_delimiter = ',')]
    kinds: Option<Vec<String>>,
    /// Only events of this account id.
    #[arg(long)]
    account: Option<String>,
    /// Only events of this call id.
    #[arg(long)]
    call_id: Option<String>,
    /// Replay events after this seq (long-polls) instead of streaming only new ones.
    #[arg(long)]
    since: Option<u64>,
    #[command(flatten)]
    connect: ConnectArgs,
}

#[derive(Args)]
struct RunArgs {
    /// Config file; defaults to the platform config directory.
    #[arg(long, env = "SIPLE_CONFIG")]
    config: Option<PathBuf>,
    /// Run without a GUI (required; the GUI is the `siple` binary).
    #[arg(long)]
    headless: bool,
    /// Never open sound devices (same as `[audio] backend = "none"`).
    #[arg(long)]
    no_audio: bool,
}

#[tokio::main]
async fn main() -> Result<ExitCode> {
    match Cli::parse().command {
        Command::Run(args) => run(args).await,
        Command::Ctl(args) => Ok(report(ctl(args).await)),
        Command::Events(args) => Ok(report(events(args).await)),
        Command::Stream(args) => Ok(report(stream(args).await)),
        Command::Methods(args) => {
            methods(&args);
            Ok(ExitCode::SUCCESS)
        }
        Command::ConfigExample => {
            print!("{}", config::EXAMPLE_CONFIG);
            Ok(ExitCode::SUCCESS)
        }
        Command::Skill => {
            print!("{SKILL_MD}");
            Ok(ExitCode::SUCCESS)
        }
        Command::Mcp(args) => {
            mcp::run(args).await?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// Prints the offline method catalog as a plain list (default), JSON or Markdown.
fn methods(args: &MethodsArgs) {
    let catalog = siple_core::backend::method_catalog();
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&catalog).unwrap_or_default()
        );
    } else if args.markdown {
        print!("{}", methods_markdown(&catalog));
    } else {
        for m in &catalog {
            println!("{:<26} {}", m.name, m.description);
        }
    }
}

/// Escapes `<` / `>` outside backtick code spans, so placeholders such as `sip:<target>@<domain>`
/// in descriptions render as text instead of being swallowed as HTML tags (GitHub, VitePress).
fn md_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_code = false;
    for ch in s.chars() {
        match ch {
            '`' => {
                in_code = !in_code;
                out.push(ch);
            }
            '<' if !in_code => out.push_str("&lt;"),
            '>' if !in_code => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
    out
}

/// One `###` section per method: description, a parameter table, and the full JSON Schema.
fn methods_markdown(catalog: &[siple_core::control::MethodInfo]) -> String {
    use std::fmt::Write;
    let cell = |s: &str| md_text(s).replace('|', "\\|").replace('\n', " ");
    let mut out = String::new();
    for m in catalog {
        let _ = writeln!(out, "### `{}`\n\n{}\n", m.name, md_text(&m.description));
        let props = m.params_schema["properties"].as_object();
        if props.is_none_or(|p| p.is_empty()) {
            let _ = writeln!(out, "Params: none.\n");
        } else if let Some(props) = props {
            let required: Vec<&str> = m.params_schema["required"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let _ = writeln!(
                out,
                "| Param | Type | Required | Description |\n|---|---|---|---|"
            );
            for (k, v) in props {
                let ty = match (&v["type"], v["enum"].as_array()) {
                    (_, Some(e)) => e
                        .iter()
                        .map(|x| x.as_str().map_or(x.to_string(), str::to_string))
                        .collect::<Vec<_>>()
                        .join(" | "),
                    (Value::String(t), _) => t.clone(),
                    (Value::Array(a), _) => a
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" | "),
                    _ => v.to_string(),
                };
                let req = if required.contains(&k.as_str()) {
                    "yes"
                } else {
                    "no"
                };
                let desc = v["description"].as_str().unwrap_or("");
                let _ = writeln!(out, "| `{k}` | {} | {req} | {} |", cell(&ty), cell(desc));
            }
            out.push('\n');
        }
        let _ = writeln!(
            out,
            "<details><summary>JSON Schema</summary>\n\n```json\n{}\n```\n\n</details>\n",
            serde_json::to_string_pretty(&m.params_schema).unwrap_or_default()
        );
    }
    out
}

/// Prints a failure (API errors as JSON on stderr) and maps to the exit code.
fn report(r: std::result::Result<(), ClientError>) -> ExitCode {
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(ClientError::Api(e)) => {
            eprintln!(
                "{}",
                serde_json::to_string_pretty(&e.to_wire()).unwrap_or_default()
            );
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("{}", json!({ "error": e.to_string() }));
            ExitCode::from(1)
        }
    }
}

/// Connects and authenticates according to `args`.
async fn connect(args: &ConnectArgs) -> std::result::Result<ControlClient, ClientError> {
    let proto = |m: String| ClientError::Protocol(m);
    let mut token = args.token.clone();
    let mut client = if let Some(p) = &args.socket {
        #[cfg(unix)]
        {
            ControlClient::connect_unix(p).await?
        }
        #[cfg(not(unix))]
        {
            let _ = p;
            return Err(proto("--socket is not supported on this platform".into()));
        }
    } else if let Some(n) = &args.pipe {
        #[cfg(windows)]
        {
            ControlClient::connect_pipe(n).await?
        }
        #[cfg(not(windows))]
        {
            let _ = n;
            return Err(proto("--pipe is only supported on Windows".into()));
        }
    } else if let Some(a) = &args.tcp {
        ControlClient::connect_tcp(a).await?
    } else {
        let path = config::resolve_config_path(args.config.clone())
            .ok_or_else(|| proto("cannot determine the default config directory".into()))?
            .path;
        let dir = config::config_dir_for(&config::absolute_path(&path));
        let info = RuntimeInfo::read(&dir).map_err(|e| {
            proto(format!(
                "no running instance found ({e}); is `siple-cli run --headless` running?"
            ))
        })?;
        token.get_or_insert_with(|| info.token.expose().to_string());
        ControlClient::connect_endpoints(&info.control).await?
    };
    client.hello(token.as_deref(), "siple-cli").await?;
    Ok(client)
}

/// Params argument: inline JSON, `@path` (file) or `-` (stdin).
fn parse_json_arg(s: Option<&str>) -> std::result::Result<Value, ClientError> {
    parse_json_arg_from(s, &mut std::io::stdin().lock())
}

fn parse_json_arg_from(
    s: Option<&str>,
    stdin: &mut dyn std::io::Read,
) -> std::result::Result<Value, ClientError> {
    let proto = |m: String| ClientError::Protocol(m);
    let text = match s {
        None => return Ok(Value::Null),
        Some("-") => {
            let mut buf = String::new();
            stdin
                .read_to_string(&mut buf)
                .map_err(|e| proto(format!("cannot read params from stdin: {e}")))?;
            buf
        }
        Some(p) if p.starts_with('@') => std::fs::read_to_string(&p[1..])
            .map_err(|e| proto(format!("cannot read params file {}: {e}", &p[1..])))?,
        Some(inline) => inline.to_string(),
    };
    serde_json::from_str(&text).map_err(|e| proto(format!("params is not valid JSON: {e}")))
}

async fn ctl(args: CtlArgs) -> std::result::Result<(), ClientError> {
    if args.method == "call.stream" {
        return Err(ClientError::Protocol(
            "`call.stream` upgrades the connection to binary; use `siple-cli stream <call_id>` instead"
                .into(),
        ));
    }
    let params = parse_json_arg(args.json.as_deref())?;
    let mut c = connect(&args.connect).await?;
    let result = c.call(&args.method, params).await?;
    println!(
        "{}",
        serde_json::to_string_pretty(&result).unwrap_or_default()
    );
    Ok(())
}

/// Reads until `buf` is full or the input ends; returns the byte count.
async fn read_up_to<R: tokio::io::AsyncRead + Unpin>(r: &mut R, buf: &mut [u8]) -> usize {
    use tokio::io::AsyncReadExt;
    let mut got = 0;
    while got < buf.len() {
        match r.read(&mut buf[got..]).await {
            Ok(0) | Err(_) => break,
            Ok(k) => got += k,
        }
    }
    got
}

/// Whether `head` (the first bytes of a file) is a RIFF/WAVE header.
fn is_wav_header(head: &[u8]) -> bool {
    head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WAVE"
}

/// Sniffs the head of a `--tx` source without seeking (it may be a pipe): a WAV is decoded to
/// raw PCM; anything else is streamed as is, the sniffed bytes chained in front of the reader.
async fn sniff_tx<R>(
    mut f: R,
    rate: u32,
    encoding: &str,
) -> Result<Box<dyn tokio::io::AsyncRead + Send + Unpin>, String>
where
    R: tokio::io::AsyncRead + Send + Unpin + 'static,
{
    use tokio::io::AsyncReadExt;
    let mut head = [0u8; 12];
    let n = read_up_to(&mut f, &mut head).await;
    if is_wav_header(&head[..n]) {
        let mut bytes = head[..n].to_vec();
        f.read_to_end(&mut bytes)
            .await
            .map_err(|e| format!("cannot read: {e}"))?;
        Ok(Box::new(std::io::Cursor::new(wav_to_pcm(
            &bytes, rate, encoding,
        )?)))
    } else {
        Ok(Box::new(std::io::Cursor::new(head[..n].to_vec()).chain(f)))
    }
}

/// Longest WAV accepted by `stream --tx`.
const MAX_TX_WAV_SECS: u32 = 3600;

/// Decodes a WAV to mono raw PCM at `rate` in `encoding`.
fn wav_to_pcm(bytes: &[u8], rate: u32, encoding: &str) -> Result<Vec<u8>, String> {
    use siple_core::agent::{stream::encode_f32, wav};
    let enc = wav::PcmEncoding::parse(encoding)
        .ok_or_else(|| format!("unknown encoding {encoding:?}"))?;
    let mono48 = wav::decode_wav_48k(std::io::Cursor::new(bytes), MAX_TX_WAV_SECS)
        .map_err(|e| format!("bad WAV: {e}"))?;
    let mono = wav::resample(&mono48, 48_000, rate).map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(mono.len() * enc.sample_bytes());
    encode_f32(enc, &mono, &mut out);
    Ok(out)
}

/// `stream`: pumps raw PCM between stdin/stdout (or files) and a `call.stream` connection.
async fn stream(args: StreamArgs) -> std::result::Result<(), ClientError> {
    use siple_core::agent::stream::{FRAME_AUDIO, FRAME_JSON, encode_frame, read_client_frame};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let proto = |m: String| ClientError::Protocol(m);
    if args.no_tx && args.no_rx {
        return Err(proto(
            "--no-tx and --no-rx together leave nothing to do".into(),
        ));
    }
    let tx_src = (!args.no_tx).then(|| args.tx.clone().unwrap_or_else(|| "-".into()));
    let rx_dst = (!args.no_rx).then(|| args.rx.clone().unwrap_or_else(|| "-".into()));
    // Open files before connecting so a bad path fails without touching the call.
    let tx_file: Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>> = match tx_src.as_deref() {
        Some(p) if p != "-" => {
            let f = tokio::fs::File::open(p)
                .await
                .map_err(|e| proto(format!("cannot open --tx {p}: {e}")))?;
            Some(
                sniff_tx(f, args.rate, &args.encoding)
                    .await
                    .map_err(|e| proto(format!("--tx {p}: {e}")))?,
            )
        }
        _ => None,
    };
    let mut rx_out: Option<Box<dyn tokio::io::AsyncWrite + Send + Unpin>> = match rx_dst.as_deref()
    {
        Some("-") => Some(Box::new(tokio::io::stdout())),
        Some(p) => {
            Some(Box::new(tokio::fs::File::create(p).await.map_err(|e| {
                proto(format!("cannot create --rx {p}: {e}"))
            })?))
        }
        None => None,
    };

    let client = connect(&args.connect).await?;
    let st = client
        .into_stream(json!({
            "call_id": args.call_id,
            "rx": !args.no_rx,
            "tx": !args.no_tx,
            "sample_rate": args.rate,
            "encoding": args.encoding,
            "frame_ms": args.frame_ms,
            "hangup_on_close": args.hangup_on_close,
        }))
        .await?;
    eprintln!("{}", json!({"event": "stream", "info": st.info}));
    let frame_bytes = st.info["frame_bytes"].as_u64().unwrap_or(640) as usize;
    let mut rd = st.rd;
    let wr = Arc::new(tokio::sync::Mutex::new(st.wr));
    let close_frame = encode_frame(FRAME_JSON, br#"{"cmd":"close"}"#);

    // TX pump.
    let paced = tx_file.is_some() && !args.no_pace;
    let tx_task = tx_src.map(|_| {
        let wr = wr.clone();
        let frame_ms = u64::from(args.frame_ms);
        let rx_enabled = !args.no_rx;
        tokio::spawn(async move {
            let mut src: Box<dyn tokio::io::AsyncRead + Send + Unpin> = match tx_file {
                Some(f) => f,
                None => Box::new(tokio::io::stdin()),
            };
            let start = tokio::time::Instant::now();
            let mut n = 0u64;
            let mut buf = vec![0u8; frame_bytes];
            loop {
                let mut got = 0;
                while got < buf.len() {
                    match src.read(&mut buf[got..]).await {
                        Ok(0) | Err(_) => break,
                        Ok(k) => got += k,
                    }
                }
                if got == 0 {
                    break;
                }
                if paced {
                    tokio::time::sleep_until(start + Duration::from_millis(n * frame_ms)).await;
                }
                n += 1;
                let mut w = wr.lock().await;
                if w.write_all(&encode_frame(FRAME_AUDIO, &buf[..got]))
                    .await
                    .is_err()
                {
                    return;
                }
                if got < buf.len() {
                    break;
                }
            }
            // Mark the end of the audio; without RX the mark event ends the session.
            let mut w = wr.lock().await;
            let _ = w
                .write_all(&encode_frame(
                    FRAME_JSON,
                    br#"{"cmd":"mark","name":"__tx_done"}"#,
                ))
                .await;
            let _ = w.flush().await;
            let _ = rx_enabled;
        })
    });

    let no_rx = args.no_rx;
    let pump = async {
        loop {
            match read_client_frame(&mut rd).await {
                Ok(Some((FRAME_AUDIO, b))) => {
                    if let Some(o) = rx_out.as_mut() {
                        o.write_all(&b).await.map_err(ClientError::Io)?;
                        o.flush().await.map_err(ClientError::Io)?;
                    }
                }
                Ok(Some((_, b))) => {
                    let text = String::from_utf8_lossy(&b).into_owned();
                    eprintln!("{text}");
                    if no_rx
                        && serde_json::from_str::<Value>(&text)
                            .is_ok_and(|v| v["event"] == "mark" && v["name"] == "__tx_done")
                    {
                        return Ok(true);
                    }
                }
                Ok(None) => return Ok(false),
                Err(m) => return Err(ClientError::Protocol(m)),
            }
        }
    };
    let r: std::result::Result<bool, ClientError> = tokio::select! {
        r = pump => r,
        _ = tokio::signal::ctrl_c() => Ok(true),
    };
    if let Some(t) = tx_task {
        t.abort();
    }
    if matches!(r, Ok(true)) {
        let mut w = wr.lock().await;
        let _ = w.write_all(&close_frame).await;
        let _ = w.flush().await;
        let _ = w.shutdown().await;
    }
    if let Some(o) = rx_out.as_mut() {
        let _ = o.flush().await;
    }
    r.map(|_| ())
}

async fn events(args: EventsArgs) -> std::result::Result<(), ClientError> {
    let mut filter = serde_json::Map::new();
    if let Some(k) = &args.kinds {
        filter.insert("kinds".into(), json!(k));
    }
    if let Some(a) = &args.account {
        filter.insert("account".into(), json!(a));
    }
    if let Some(c) = &args.call_id {
        filter.insert("call_id".into(), json!(c));
    }
    let mut c = connect(&args.connect).await?;
    let work = async {
        if let Some(mut since) = args.since {
            loop {
                let mut p = filter.clone();
                p.insert("since".into(), json!(since));
                p.insert("timeout_ms".into(), json!(30_000));
                let r = c.call("events.poll", Value::Object(p)).await?;
                if r["truncated"] == true {
                    eprintln!(
                        "{}",
                        json!({ "warning": "events were dropped from the ring before they could be read" })
                    );
                }
                for e in r["events"].as_array().into_iter().flatten() {
                    println!("{e}");
                }
                since = r["next_seq"].as_u64().unwrap_or(since);
            }
        } else {
            c.call("events.subscribe", Value::Object(filter)).await?;
            loop {
                let e = c.next_event().await?;
                println!("{}", serde_json::to_string(&e).unwrap_or_default());
            }
        }
    };
    // A server that goes away is a failure (exit 1 with a message), not a clean end of stream;
    // only Ctrl-C ends normally.
    tokio::select! {
        r = work => match r {
            Err::<(), _>(ClientError::Closed) => Err(ClientError::Protocol(
                "the server closed the connection (Siple stopped or restarted?)".into(),
            )),
            other => other,
        },
        _ = tokio::signal::ctrl_c() => Ok(()),
    }
}

/// Why `run` must not start an engine in `channel`: the Mac App Store sandbox only allows this
/// CLI to talk to the app (a headless engine there is broken: no external commands, no
/// reachable audio or sockets for other tools).
fn run_refusal(channel: &config::channel::Channel) -> Option<&'static str> {
    (channel.kind == config::channel::ChannelKind::MacAppStore).then_some(
        "siple-cli run is not available in the Mac App Store edition: this copy only controls the \
         running Siple app (ctl, mcp, stream). For a headless engine install the Developer ID \
         build (also on Homebrew) and run its siple-cli.",
    )
}

async fn run(args: RunArgs) -> Result<ExitCode> {
    if let Some(msg) = run_refusal(config::channel::current()) {
        eprintln!("{msg}");
        return Ok(ExitCode::from(2));
    }
    if !args.headless {
        eprintln!("siple-cli run requires --headless; the GUI is the `siple` binary");
        return Ok(ExitCode::from(2));
    }

    let resolved = config::resolve_config_path(args.config)
        .context("cannot determine the default config directory")?;
    let mut opts = AppOptions::new(resolved.path);
    #[cfg(not(feature = "store"))]
    opts.use_github_updates();
    opts.migrate_legacy_config = resolved.is_default;
    opts.startup_warnings = resolved.warnings;
    opts.headless = true;
    if args.no_audio {
        opts.audio_backend = Some(config::AudioBackend::None);
    }
    let app = match App::start(opts).await {
        Ok(app) => app,
        Err(e @ (AppError::Invalid { .. } | AppError::Config(_))) => {
            eprintln!("{e}");
            return Ok(ExitCode::from(1));
        }
        Err(e) => return Err(e.into()),
    };
    if app.created_config() {
        eprintln!("created example config at {}", app.config_path().display());
    }

    wait_for_shutdown(&app).await?;
    app.shutdown().await;
    Ok(ExitCode::SUCCESS)
}

/// Resolves on the first termination request, so `run` returns and the app shuts down cleanly
/// (removing `runtime.json`). SIGHUP reloads the config instead of stopping. (After a hard kill
/// a stale file stays behind; that is harmless because the instance lock is free and the next
/// start overwrites it.)
async fn wait_for_shutdown(app: &App) -> Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate())?;
        let mut hup = signal(SignalKind::hangup())?;
        loop {
            tokio::select! {
                r = tokio::signal::ctrl_c() => { r?; return Ok(()); }
                _ = term.recv() => return Ok(()),
                _ = hup.recv() => {
                    tracing::info!("SIGHUP: reloading config");
                    let report = app.reload().await;
                    if !report.errors.is_empty() {
                        tracing::warn!("reload finished with errors: {}", report.errors.join("; "));
                    }
                }
            }
        }
    }
    #[cfg(windows)]
    {
        use tokio::signal::windows;
        let _ = app;
        let mut close = windows::ctrl_close()?;
        let mut shutdown = windows::ctrl_shutdown()?;
        let mut logoff = windows::ctrl_logoff()?;
        tokio::select! {
            r = tokio::signal::ctrl_c() => r?,
            _ = close.recv() => {}
            _ = shutdown.recv() => {}
            _ = logoff.recv() => {}
        }
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = app;
        tokio::signal::ctrl_c().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    /// A reader that cannot seek and returns data in tiny chunks (like a pipe).
    struct Trickle(std::io::Cursor<Vec<u8>>);
    impl tokio::io::AsyncRead for Trickle {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            use std::io::Read;
            let mut one = [0u8; 5];
            let max = one.len().min(buf.remaining());
            let n = self.0.read(&mut one[..max]).unwrap();
            buf.put_slice(&one[..n]);
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn sniff_tx_works_on_non_seekable_readers() {
        use siple_core::agent::wav::encode_wav_i16;
        use tokio::io::AsyncReadExt;
        // Raw PCM: every byte comes through, including the sniffed head.
        let raw: Vec<u8> = (0..100u8).collect();
        let mut r = super::sniff_tx(Trickle(std::io::Cursor::new(raw.clone())), 8000, "s16le")
            .await
            .unwrap();
        let mut got = Vec::new();
        r.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, raw);
        // Shorter than the sniff window.
        let mut r = super::sniff_tx(Trickle(std::io::Cursor::new(vec![1, 2, 3])), 8000, "s16le")
            .await
            .unwrap();
        let mut got = Vec::new();
        r.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, [1, 2, 3]);
        // WAV: decoded, header gone.
        let wav = encode_wav_i16(&vec![1000i16; 8000], 8000);
        let mut r = super::sniff_tx(Trickle(std::io::Cursor::new(wav)), 16000, "s16le")
            .await
            .unwrap();
        let mut got = Vec::new();
        r.read_to_end(&mut got).await.unwrap();
        assert_eq!(got.len(), 16000 * 2);
    }

    #[test]
    fn wav_header_is_detected_and_converted() {
        use siple_core::agent::wav::encode_wav_i16;
        let wav = encode_wav_i16(&vec![1000i16; 8000], 8000); // 1 s at 8 kHz
        assert!(super::is_wav_header(&wav[..12]));
        assert!(!super::is_wav_header(&[0u8; 12]));
        let pcm = super::wav_to_pcm(&wav, 16000, "s16le").unwrap();
        assert_eq!(pcm.len(), 16000 * 2); // 1 s at 16 kHz s16le, header gone
        let ulaw = super::wav_to_pcm(&wav, 8000, "mulaw").unwrap();
        assert_eq!(ulaw.len(), 8000);
        assert!(super::wav_to_pcm(&wav, 8000, "bogus").is_err());
    }

    use super::*;

    #[test]
    fn run_is_refused_only_in_the_mac_app_store_edition() {
        use siple_core::config::channel::Channel;
        let msg = run_refusal(&Channel::MAC_APP_STORE).expect("refused");
        assert!(msg.contains("Developer ID") && msg.contains("Homebrew"));
        assert!(run_refusal(&Channel::DIRECT).is_none());
        assert!(run_refusal(&Channel::msix("a_b", None)).is_none());
    }
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn md_text_escapes_placeholders_outside_code() {
        assert_eq!(
            md_text("sip:<target>@<domain> or `tone:<hz>`"),
            "sip:&lt;target&gt;@&lt;domain&gt; or `tone:<hz>`"
        );
    }

    #[test]
    fn methods_markdown_covers_catalog() {
        let catalog = siple_core::backend::method_catalog();
        let md = methods_markdown(&catalog);
        for m in &catalog {
            assert!(md.contains(&format!("### `{}`", m.name)), "{}", m.name);
        }
    }

    #[test]
    fn parses_methods_and_config_example() {
        assert!(Cli::try_parse_from(["siple-cli", "methods", "--markdown"]).is_ok());
        assert!(Cli::try_parse_from(["siple-cli", "methods", "--markdown", "--json"]).is_err());
        assert!(Cli::try_parse_from(["siple-cli", "config-example"]).is_ok());
        assert!(Cli::try_parse_from(["siple-cli", "skill"]).is_ok());
    }

    #[test]
    fn skill_has_frontmatter_and_names_only_real_methods() {
        let skill = SKILL_MD.replace("\r\n", "\n");
        assert!(skill.starts_with("---\nname: siple\ndescription: "));
        // Every dotted `group.method` the skill mentions, for any group the catalog has, must
        // exist. File names such as `config.toml` are not methods.
        let catalog = siple_core::backend::method_catalog();
        let known: std::collections::HashSet<&str> =
            catalog.iter().map(|m| m.name.as_str()).collect();
        let groups: std::collections::HashSet<&str> = catalog
            .iter()
            .filter_map(|m| m.name.split_once('.').map(|(g, _)| g))
            .collect();
        let mut checked = 0;
        for raw in skill.split(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '_')) {
            let word = raw.trim_end_matches('.');
            let Some((group, rest)) = word.split_once('.') else {
                continue;
            };
            if !groups.contains(group) || rest.is_empty() || rest.contains('.') || rest == "toml" {
                continue;
            }
            assert!(
                known.contains(word),
                "SKILL.md mentions unknown method `{word}`"
            );
            checked += 1;
        }
        assert!(
            checked > 10,
            "only {checked} method names found in SKILL.md"
        );
    }

    #[test]
    fn parses_run_flags() {
        let cli = Cli::try_parse_from(["siple-cli", "run", "--headless", "--config", "/x/c.toml"])
            .unwrap();
        let Command::Run(a) = cli.command else {
            panic!("expected run")
        };
        assert!(a.headless);
        assert_eq!(a.config.unwrap(), PathBuf::from("/x/c.toml"));
    }

    #[test]
    fn parses_ctl() {
        let cli = Cli::try_parse_from(["siple-cli", "ctl", "status", "{}"]).unwrap();
        assert!(
            matches!(cli.command, Command::Ctl(ref a) if a.method == "status" && a.json.is_some())
        );
        let cli = Cli::try_parse_from([
            "siple-cli",
            "ctl",
            "ping",
            "--tcp",
            "127.0.0.1:5079",
            "--token",
            "t",
        ])
        .unwrap();
        assert!(matches!(cli.command, Command::Ctl(ref a) if a.connect.tcp.is_some()));
        // --tcp without --token, and conflicting endpoint flags, are rejected.
        assert!(Cli::try_parse_from(["siple-cli", "ctl", "ping", "--tcp", "127.0.0.1:1"]).is_err());
        assert!(
            Cli::try_parse_from(["siple-cli", "ctl", "ping", "--socket", "/a", "--pipe", "b"])
                .is_err()
        );
    }

    #[test]
    fn params_from_inline_file_and_stdin() {
        let mut empty = std::io::empty();
        assert_eq!(parse_json_arg_from(None, &mut empty).unwrap(), Value::Null);
        assert_eq!(
            parse_json_arg_from(Some(r#"{"a":1}"#), &mut empty).unwrap(),
            json!({"a": 1})
        );
        let mut stdin: &[u8] = br#"{"from":"stdin"}"#;
        assert_eq!(
            parse_json_arg_from(Some("-"), &mut stdin).unwrap(),
            json!({"from": "stdin"})
        );
        let dir = tempfile::tempdir().unwrap();
        let big = "x".repeat(300_000); // larger than Linux MAX_ARG_STRLEN
        let path = dir.path().join("p.json");
        std::fs::write(&path, json!({ "audio": big }).to_string()).unwrap();
        let arg = format!("@{}", path.display());
        let v = parse_json_arg_from(Some(&arg), &mut empty).unwrap();
        assert_eq!(v["audio"].as_str().map(str::len), Some(300_000));
        // Errors: missing file, bad JSON.
        assert!(parse_json_arg_from(Some("@/nonexistent/zp.json"), &mut empty).is_err());
        let mut bad: &[u8] = b"{";
        assert!(parse_json_arg_from(Some("-"), &mut bad).is_err());
    }

    #[test]
    fn parses_no_audio_and_env_config() {
        let cli = Cli::try_parse_from(["siple-cli", "run", "--headless", "--no-audio"]).unwrap();
        let Command::Run(a) = cli.command else {
            panic!("expected run")
        };
        assert!(a.no_audio);
    }

    #[test]
    fn parses_stream() {
        let cli = Cli::try_parse_from([
            "siple-cli",
            "stream",
            "c1",
            "--rate",
            "8000",
            "--encoding",
            "mulaw",
            "--tx",
            "in.raw",
            "--no-rx",
            "--hangup-on-close",
            "--no-pace",
        ])
        .unwrap();
        let Command::Stream(a) = cli.command else {
            panic!("expected stream")
        };
        assert_eq!(
            (a.call_id.as_str(), a.rate, a.no_rx, a.no_pace),
            ("c1", 8000, true, true)
        );
        assert!(
            Cli::try_parse_from(["siple-cli", "stream", "c1", "--tx", "x", "--no-tx"]).is_err()
        );
    }

    #[tokio::test]
    async fn ctl_refuses_call_stream() {
        let args = CtlArgs {
            method: "call.stream".into(),
            json: None,
            connect: ConnectArgs {
                config: None,
                socket: None,
                pipe: None,
                tcp: None,
                token: None,
            },
        };
        let e = ctl(args).await.unwrap_err();
        assert!(e.to_string().contains("siple-cli stream"), "{e}");
    }

    #[test]
    fn parses_events() {
        let cli = Cli::try_parse_from([
            "siple-cli",
            "events",
            "--kinds",
            "call.*,account.*",
            "--account",
            "a1",
            "--since",
            "5",
        ])
        .unwrap();
        let Command::Events(a) = cli.command else {
            panic!("expected events")
        };
        assert_eq!(a.kinds.unwrap(), ["call.*", "account.*"]);
        assert_eq!((a.account.as_deref(), a.since), (Some("a1"), Some(5)));
    }
}

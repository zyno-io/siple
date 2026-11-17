# AI agents

Siple is built to be driven by AI agents as much as by hand. Whatever the GUI can do, an agent can do, through the same
engine and the same state.

<div class="tip custom-block" style="padding-top: 8px">

Everything below works against a running Siple, either the GUI or `siple-cli run --headless` (no window, for servers, CI
and SSH sessions).

</div>

## Layers

**Control API.** Newline-delimited JSON-RPC 2.0 over a local Unix socket (macOS, Linux) or named pipe (Windows), with an
optional loopback TCP listener that always requires a token. It exposes every state the GUI shows and every action: accounts,
calls, transfers, BLF, config edits, events. From a shell: `siple-cli ctl status`, `siple-cli ctl call.dial '{...}'`,
`siple-cli events` for a live stream.

**Agent skill.** A short `SKILL.md` (print it with `siple-cli skill`) teaches an agent to drive Siple with one
`siple-cli ctl` command per action and to look methods up with `siple-cli methods`. No extra process runs.
[Install it](/agents/skill).

**MCP server.** `siple-cli mcp` is a stdio MCP server that proxies to a running instance. There is one tool per API method,
generated from the same catalog as the reference, so the tool list and [the API reference](/agents/api-reference) cannot
drift. A `wait_for_event` tool is the primary way to wait for something to happen.
[Set it up in a minute](/agents/mcp-quickstart).

**Agent audio.** An agent can speak into a call and hear it without a sound card: `call.play` (tones, files, inline audio),
`call.listen` (voice-activity detection, optional transcript), `call.rx_audio`, `call.say` (text to speech) and a binary
duplex `call.stream`. Turn-taking is race-free: `call.say` with `wait` returns an `rx_ms` position and `call.listen` with
`since_rx_ms` hears everything after it. Text to speech and transcription run your own programs configured in `[speech]`
(piper, espeak-ng, `say`, SAPI, whisper.cpp; nothing is bundled, and it never runs a shell). Over MCP, audio comes back as an
audio content block, never as base64 text.

## Designed for automation

- **Race-free waiting.** Every state-changing result carries `event_seq`; wait with `since = event_seq` and nothing is
  missed. See [JSON-RPC basics](/agents/json-rpc).
- **Automation calls need no hardware.** Dial with `source` and `sink` of `silence`, `tone:<hz>` or `null` so no microphone is
  involved. Calls like these are exempt from auto-hold.
- **Diagnostics.** SIP traces per call (`call.sip_trace`), tone and DTMF detection, account tests (`account.test`), and vendor
  profile emulation (Yealink, Polycom, Cisco, Grandstream).
- **Same config file.** `config.edit` changes `config.toml` and keeps its comments; invalid edits never replace the last good
  config.
- **Local and private.** The control endpoint is restricted to the current user. There is no telemetry; the only request Siple makes on its own is the optional, anonymous [update check](/guide/updates).

## Where next

1. [Agent skill](/agents/skill): give an agent Siple through `siple-cli`, with no helper process.
2. [Quick start with MCP](/agents/mcp-quickstart): register `siple-cli mcp` with Claude Code or Claude Desktop.
3. [JSON-RPC basics](/agents/json-rpc): connect, authenticate, wait without races.
4. [API reference](/agents/api-reference): every method, event and recipe.

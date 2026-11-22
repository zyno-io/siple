# Quick start with MCP

Connect an MCP client such as Claude Code or Claude Desktop to a running Siple in a few minutes.

`siple-cli mcp` is a stdio MCP server. It does not run the phone itself: it connects to a running Siple (the GUI or
`siple-cli run --headless`) and exposes its control API as tools. Start Siple first.

## Claude Code

```sh
claude mcp add siple -- /path/to/siple-cli mcp
```

On a macOS app bundle the binary is inside the bundle:

```sh
claude mcp add siple -- "/Applications/Siple.app/Contents/MacOS/siple-cli" mcp
```

## Claude Desktop

Add the server to `claude_desktop_config.json` (macOS: `~/Library/Application Support/Claude/`, Windows: `%APPDATA%\Claude\`),
then restart Claude Desktop:

```json
{
  "mcpServers": {
    "siple": {
      "command": "/Applications/Siple.app/Contents/MacOS/siple-cli",
      "args": ["mcp"]
    }
  }
}
```

## Other MCP clients

Any client that can launch a stdio command works: the command is `siple-cli` and the argument is `mcp`.

The Flatpak edition has no `siple-cli` on your `PATH`; the command is `flatpak run` with `--command=siple-cli io.zyno.siple mcp`
as its arguments:

```sh
claude mcp add siple -- flatpak run --command=siple-cli io.zyno.siple mcp
```

The AppImage edition: run the file itself with `--cli`, or use the `siple-cli` wrapper script from [Install](/guide/install):

```sh
claude mcp add siple -- /path/to/Siple.AppImage --cli mcp
claude mcp add siple -- ~/.local/bin/siple-cli mcp
```

Without FUSE, put `--appimage-extract-and-run` first: `/path/to/Siple.AppImage --appimage-extract-and-run --cli mcp`.

## A non-default instance

`siple-cli mcp` finds the instance through `runtime.json` in the config directory. For an instance started with another config,
add `--config`:

```sh
claude mcp add siple -- /path/to/siple-cli mcp --config /path/to/config.toml
```

You can also point at an endpoint directly with `--socket`, `--pipe`, or `--tcp ADDR --token T`.

## Docker or a remote host

Because the transport is stdio, any command that reaches `siple-cli mcp` works:

```sh
claude mcp add siple -- docker exec -i siple siple-cli mcp
# from another machine:
claude mcp add siple -- ssh <host> docker exec -i siple siple-cli mcp
```

## What the agent gets

- One tool per control method, named with underscores (`call.dial` becomes `call_dial`), with the JSON Schema from the catalog.
- `wait_for_event`, a wrapper over `events.poll` (default timeout 10 s, maximum 60 s), the primary way to wait.
- Results in `content[0].text` and `structuredContent`; failures set `isError: true` with the error kind and message.
- The tool list is served even when Siple is not running; calls then fail with a clear "Siple is not running" message.
- Audio tools (`call_listen`, `call_rx_audio`, `speech_synthesize`) return audio as an MCP audio content block, and omit base64
  from the text.
- `call.stream` is not exposed over MCP (it is a binary upgrade). Lost connections are retried only for idempotent reads; if
  Siple restarted, the next result carries `restarted: true` and all remembered call ids are invalid.

## Try it

Ask your agent to run `status`, then to dial a test number with silent audio. A first manual check from a shell:

```sh
siple-cli ctl status
```

More recipes (place a call and verify audio, voicemail, BLF, transfers) are in the
[API reference](/agents/api-reference#llm-recipes).

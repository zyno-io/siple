# Agent skill (CLI)

Give an AI agent control of Siple with a skill that needs no helper process.

The simplest way to give an agent Siple: a **skill** that teaches it to run `siple-cli`. Each action is one command that
connects to the running Siple, does one thing and exits. There is no MCP server or other helper process to keep running;
only Siple itself runs, because it has to stay registered to receive calls.

## Install

`siple-cli skill` prints the skill for the version you have installed, so it always matches the binary.

::: code-group

```sh [Claude Code (macOS)]
mkdir -p ~/.claude/skills/siple
/Applications/Siple.app/Contents/MacOS/siple-cli skill > ~/.claude/skills/siple/SKILL.md
```

```sh [Claude Code (Linux)]
# from the extracted siple-<version> folder
mkdir -p ~/.claude/skills/siple
./bin/siple-cli skill > ~/.claude/skills/siple/SKILL.md
```

```powershell [Claude Code (Windows)]
# from the folder you unzipped Siple into
New-Item -ItemType Directory -Force "$HOME\.claude\skills\siple" | Out-Null
.\siple-cli.exe skill | Set-Content -Encoding ascii "$HOME\.claude\skills\siple\SKILL.md"
```

:::

For a single project, put it in `.claude/skills/siple/SKILL.md` in that project instead. Other agents that read
[Agent Skills](https://agentskills.io) use the same file in their own skills folder; agents without skill support can be
given its text as instructions.

Reinstall after updating Siple so new methods are covered.

## What the skill teaches

- Find `siple-cli` and check that Siple is running (`siple-cli ctl status`).
- Call any method with `siple-cli ctl <method> '<json>'`: the result is JSON on stdout, an error is JSON on stderr with
  exit code 1.
- Look methods up instead of guessing: `siple-cli methods` and `siple-cli methods --json` work offline.
- Wait without a background process: `call.wait_for` and `events.poll` with `since` set to the `event_seq` of the
  action, so nothing is missed between commands.
- Common tasks (dial, answer, transfer, DTMF, speak and listen, extensions, settings) and safety rules: confirm before
  calling or transferring anyone the user did not name, and never print passwords or the control token.

The full text is in the repository at
[`skills/siple/SKILL.md`](https://github.com/zyno-io/siple/blob/main/skills/siple/SKILL.md).

## Skill or MCP?

| | Skill + CLI | MCP (`siple-cli mcp`) |
| --- | --- | --- |
| Extra process | none | one stdio server per agent session |
| Tools in the agent's context | one short skill; methods looked up on demand | one tool per method (about 75) |
| Agent audio (`call.listen` audio, `call.rx_audio`) | WAV as base64 in JSON, or written to a file | native audio content blocks |
| Live event stream | `events.poll` long-poll | `wait_for_event` tool |

Both reach exactly the same methods. Use the skill unless your agent cannot run shell commands or you need audio
returned as audio content.

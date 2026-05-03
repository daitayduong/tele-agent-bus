# tele-agent-bus

Multi-agent orchestration daemon (Claude Code / Gemini CLI / Codex CLI) over Telegram.

**Status:** v0.1.0.

## Overview

A global daemon at `~/.agent-bus/` that:
- Serves multiple projects (repos) from one always-on process
- Routes Telegram commands to the right agent in the right repo
- Intercepts Bash, Write, Edit, and MultiEdit commands from Claude Code and routes them through an approval gate + Telegram approval UI
- Bridges Codex App Server turns in isolated mode (`codex_mode: app_server`)
- Bridges Gemini CLI headless sessions
- Runs independently of any IDE

## Workspace layout

```
crates/
├── agent-bus-proto/    Wire types only. No runtime deps. Embedded by hook binary.
├── agent-bus-core/     Shared logic: config, state actor, approval gate, redaction, path validation.
├── agent-bus-hook/     Minimal hook binary (cold-start <50ms, size <2MB).
└── agent-bus/          Main daemon + CLI.
```

## Requirements

- Linux with systemd user services, or Windows for the experimental Telegram daemon MVP
- Rust 1.75+
- A dedicated Telegram bot token from `@BotFather`
- Claude Code, Gemini CLI, or Codex CLI installed if you want to route work to those agents

## Platform Support

- Linux is the primary supported platform. It includes the daemon, Telegram
  commands, systemd service install, Unix-socket permission approvals, and
  Claude Code hook integration.
- Windows support is experimental. The daemon and Telegram repo/agent routing
  can be built and run manually, but permission hook IPC and service
  installation are not ported yet.
- macOS is not officially supported yet, but should be closer to Linux than
  Windows once launchd/service packaging and peer-credential checks are added.

## Install from source on Linux

Clone and build:

```bash
git clone https://github.com/daitayduong/tele-agent-bus.git
cd tele-agent-bus
cargo build --release --workspace
```

Install the two binaries:

```bash
sudo install -m 0755 target/release/agent-bus /usr/local/bin/agent-bus
sudo install -m 0755 target/release/agent-bus-hook /usr/local/bin/agent-bus-hook
```

Initialize OS-level files and the systemd user unit:

```bash
sudo scripts/install.sh
```

Log out and back in once so your user picks up the `agent-bus` group membership.

Initialize your per-user config:

```bash
agent-bus init
```

This creates:

```text
~/.agent-bus/config.yaml
~/.agent-bus/repos.yaml
```

## Telegram Setup

Create a dedicated bot:

1. Open Telegram and message `@BotFather`.
2. Send `/newbot`.
3. Follow the prompts and copy the bot token.
4. Message your new bot once with `/start`.

Find your chat ID:

```bash
curl "https://api.telegram.org/bot<YOUR_BOT_TOKEN>/getUpdates"
```

Look for `chat":{"id":...}` in the response. That number is your chat ID.

Export the values used by the default config:

```bash
export TELE_BUS_BOT_TOKEN='123456:ABCdef...'
export TELE_BUS_CHAT_ID='123456789'
```

For a systemd user service, put the variables somewhere your user service can read them. One persistent option is `~/.config/environment.d/agent-bus.conf`:

```ini
TELE_BUS_BOT_TOKEN=123456:ABCdef...
TELE_BUS_CHAT_ID=123456789
```

Then log out and back in, or import them into the current user manager before starting the daemon:

```bash
systemctl --user import-environment TELE_BUS_BOT_TOKEN TELE_BUS_CHAT_ID
```

Validate config:

```bash
agent-bus config validate
```

Start the daemon:

```bash
systemctl --user daemon-reload
systemctl --user enable --now agent-bus
```

For foreground debugging:

```bash
agent-bus daemon
```

Do not run two `agent-bus daemon` processes with the same bot token. Telegram allows only one `getUpdates` consumer per bot.

## Register Repositories

Add a repository:

```bash
agent-bus repo add /path/to/project
agent-bus repo list
```

`agent-bus repo add` defaults Codex to `live_bridge`. If you want a repo to use
the App Server-owned Codex flow instead, edit `~/.agent-bus/repos.yaml` and set:

```yaml
repos:
  - id: tele-agent-bus_5979e1d0
    display: tele-agent-bus
    path: /home/you/Projects/tele-agent-bus
    agents: [claude, gemini, antigravity, codex]
    codex_mode: app_server
```

Available values:
- `live_bridge`: follow the currently open desktop Codex session. This is the default.
- `app_server`: let `agent-bus` own the Codex turn through `codex app-server`.

To route Claude Code Bash commands through the approval gate, install the hook in that project:

```bash
agent-bus repo install-hook /path/to/project
```

After adding or removing repos, restart the daemon so Telegram sees the updated registry:

```bash
systemctl --user restart agent-bus
```

## Approval Gate

The approval gate intercepts Bash, Write, Edit, and MultiEdit commands from Claude Code before they execute.
When a command matches a pattern in the gate, the daemon sends a Telegram
message with **Approve** and **Deny** buttons. Commands that do not match any
pattern are silently approved.

Gate rules live in two places:
- **`/etc/agent-bus/approval-gate.conf`** — global, requires `sudo`
- **`~/.agent-bus/repos/<id>/approval-gate.conf`** — per-repo, no `sudo`

Both files are HMAC-signed — tampering causes fail-closed denial of all commands.

### Adding rules

```bash
# --destructive: deny when daemon is unreachable (fail-closed)
sudo agent-bus gate add '(^|\s)rm\s+-[rRfF]' --destructive
sudo agent-bus gate add 'git\s+reset\s+--hard' --destructive
sudo agent-bus gate add 'git\s+push\s+.*--force' --destructive
sudo agent-bus gate add '(^|\s)dd\s+if=' --destructive

# Without --destructive: approve silently when daemon is unreachable
sudo agent-bus gate add 'npm\s+run\s+deploy'

# Per-repo rule (no sudo)
agent-bus gate add --repo my-project '^make\s+deploy' --destructive
```

### Managing rules

```bash
sudo agent-bus gate list
sudo agent-bus gate remove '<pattern>'
sudo agent-bus gate verify        # check HMAC integrity
```

### Installing the hook

The gate only applies to projects with the hook installed:

```bash
agent-bus repo install-hook /path/to/project
```

This adds a `PreToolUse` entry to `.claude/settings.json`. Restart Claude Code
in that project after installing. Other agents (Gemini, Codex, Antigravity)
use their own internal approval mechanisms and do not route through this gate.

See `docs/per-repo-approval-gate.md` for file format, merge semantics, and
fail behavior details.

## Telegram Commands

- `/help` lists all available commands as inline buttons you can tap to run.
- `/switch_rp` shows repository buttons and sets the current chat's default repo.
- `/switch_rp <repo_id>` switches directly to a repo without showing the picker.
- `/current` shows the current default repo.
- `/list_claude` lists Claude desktop sessions for the current repo.
- `/list_codex` lists Codex desktop sessions for the current repo.
- `/list_gemini` lists Gemini sessions for the current repo.
- `@claude <message>` sends a message to the selected Claude session.
- `@codex <message>` sends a message to the selected Codex session.
  With `codex_mode: live_bridge` (default), agent-bus follows the live desktop-owned bridge.
  With `codex_mode: app_server`, agent-bus owns the Codex turn via `codex app-server` with `sandbox=false`, isolated per turn.
- `@gemini <message>` resumes the selected Gemini session. If none is selected yet, it falls back to headless Gemini CLI in the current default repo. Gemini uses `--approval-mode plan` by default.
- `@agent:<repo_id> <message>` routes explicitly to a repo and bypasses the default repo selection.

See `docs/setup-telegram.md` for the detailed Telegram setup flow.
See `docs/setup-windows.md` for the experimental Windows setup flow.

## Security disclaimer

The approval-gate is a **heuristic UX guardrail**, not a sandbox. It is trivially bypassable by shell quoting, command substitution, aliases, interpreters, or encoded payloads. Do not rely on it as a security boundary against adversarial code. See spec §10.1.

## Build

```bash
# Requires Rust 1.75+
cargo build --workspace
cargo test --workspace
```

## License

MIT. You may use, modify, distribute, and commercialize this project, provided that the copyright and license notice are preserved.

Copyright (c) 2026 John Chuong.

## About the Author

tele-agent-bus is created by John Chuong, an amateur programmer pursuing a few
free projects, including [docprivy.com](https://docprivy.com) and
tele-agent-bus.

If this project is useful to you, you can support the author with a
"Buy me a coffee" contribution through PayPal: johnchuong5@gmail.com.

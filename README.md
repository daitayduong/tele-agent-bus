# tele-agent-bus

Multi-agent orchestration daemon (Claude Code / Gemini CLI / Codex CLI) over Telegram.

**Status:** v0.1.0. See `docs/spec.md` for the full specification.

## Overview

A global daemon at `~/.agent-bus/` that:
- Serves multiple projects (repos) from one always-on process
- Routes Telegram commands to the right agent in the right repo
- Gates dangerous Bash commands via a blacklist + Telegram approval UI
- Runs independently of any IDE (systemd user service)

## Workspace layout

```
crates/
├── agent-bus-proto/    Wire types only. No runtime deps. Embedded by hook binary.
├── agent-bus-core/     Shared logic: config, state actor, blacklist, redaction, path validation.
├── agent-bus-hook/     Minimal hook binary (cold-start <50ms, size <2MB).
└── agent-bus/          Main daemon + CLI.
```

## Requirements

- Linux with systemd user services
- Rust 1.75+
- A dedicated Telegram bot token from `@BotFather`
- Claude Code, Gemini CLI, or Codex CLI installed if you want to route work to those agents

## Install from source

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

If you want Claude Code Bash permission approvals to go through Telegram, install the hook in that project:

```bash
agent-bus repo install-hook /path/to/project
```

After adding or removing repos, restart the daemon so Telegram sees the updated registry:

```bash
systemctl --user restart agent-bus
```

Hot-reloading the repo registry is tracked in `TASKS.md`.

## Telegram Commands

- `/list_rp` lists registered repositories and shows buttons to select the default repo for the current chat.
- `/switch_rp <repo_id>` sets the current chat's default repo.
- `/current` shows the current default repo.
- `@claude <message>` sends a message to the selected Claude session.
- `@codex <message>` sends a message to the selected Codex session.
- `@agent:<repo_id> <message>` routes explicitly to a repo and bypasses the default repo selection.

See `docs/setup-telegram.md` for the detailed Telegram setup flow.

## Security disclaimer

The permission-gate blacklist is a **heuristic UX guardrail**, not a sandbox. It is trivially bypassable by shell quoting, command substitution, aliases, interpreters, or encoded payloads. Do not rely on it as a security boundary against adversarial code. See spec §10.1.

## Build

```bash
# Requires Rust 1.75+
cargo build --workspace
cargo test --workspace
```

## License

MIT. You may use, modify, distribute, and commercialize this project, provided that the copyright and license notice are preserved.

Copyright (c) 2026 John Chuong.

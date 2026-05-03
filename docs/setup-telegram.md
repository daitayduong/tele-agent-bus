# Telegram Setup for agent-bus

## Prerequisites

- Telegram account
- Rust 1.75+
- `agent-bus` and `agent-bus-hook` installed at `/usr/local/bin/`
- A dedicated Telegram bot for agent-bus

## Step 1 - Build and install agent-bus

From the repository root:

```bash
cargo build --release --workspace

sudo install -m 0755 target/release/agent-bus /usr/local/bin/agent-bus
sudo install -m 0755 target/release/agent-bus-hook /usr/local/bin/agent-bus-hook
sudo scripts/install.sh
```

Log out and back in once after `scripts/install.sh` so your user picks up the
`agent-bus` group membership.

Initialize per-user config:

```bash
agent-bus init
```

This creates `~/.agent-bus/config.yaml` and `~/.agent-bus/repos.yaml`.

## Step 2 - Create a dedicated bot

**Important:** agent-bus must have its OWN bot. Do not reuse a bot used by
other tools (Claude MCP notifications, home automation, etc.). Telegram
only allows one `getUpdates` consumer per bot, so sharing causes lost
messages and timeouts.

1. Open Telegram, search for `@BotFather`
2. Send `/newbot`
3. Follow prompts for name and username (e.g. `YourName Agent Bus`, `yourname_agentbus_bot`)
4. Copy the token. It looks like `123456:ABCdef...`

## Step 3 - Get your chat_id

1. Search for your new bot in Telegram, send `/start`
2. Open `https://api.telegram.org/bot<TOKEN>/getUpdates` in your browser (replace `<TOKEN>`)
3. Find `chat":{"id":NNNN`. That number is your chat_id

Alternative: message `@userinfobot` and it replies with your user ID.

## Step 4 - Export env vars

```bash
export TELE_BUS_BOT_TOKEN='123456:ABCdef...'
export TELE_BUS_CHAT_ID='YOUR_CHAT_ID'
```

Add them to `~/.bashrc` or `~/.zshrc` for foreground debugging.

For persistent systemd user services, create `~/.config/environment.d/agent-bus.conf`:

```ini
TELE_BUS_BOT_TOKEN=123456:ABCdef...
TELE_BUS_CHAT_ID=YOUR_CHAT_ID
```

Then log out and back in.

For the provided systemd user service, import the environment before starting
the daemon in the current login session:

```bash
systemctl --user import-environment TELE_BUS_BOT_TOKEN TELE_BUS_CHAT_ID
```

## Step 5 - Register a repository

```bash
agent-bus repo add /path/to/project
agent-bus repo list
```

Optional, for Claude Code Bash permission approvals through Telegram:

```bash
agent-bus repo install-hook /path/to/project
```

Codex defaults to `live_bridge`, which follows the currently open desktop session.
To switch a repo to the isolated App Server-owned Codex flow instead, edit
`~/.agent-bus/repos.yaml` and set `codex_mode: app_server` under that repo,
then restart the daemon.

### Codex App Server Mode

The `app_server` mode spawns `codex app-server --listen stdio:// -c sandbox=false`
in a dedicated process per turn, rather than following a live desktop session.
This is useful when the desktop session is unavailable or you want isolated,
turn-scoped execution. The daemon owns the turn lifecycle: `initialize` →
`resume thread` → `start turn` → poll for output/approvals → `turn/completed`.

## Step 6 - Verify and start

```bash
agent-bus config validate
systemctl --user daemon-reload
systemctl --user enable --now agent-bus
```

For foreground debugging:

```bash
agent-bus daemon
```

The daemon should not log `TerminatedByOtherGetUpdates`. If it does, another
client is polling the same bot. Stop the other client or create a separate bot.

## Step 7 - Test in Telegram

Send these messages to your bot:

```text
/current
/switch_rp
/switch_rp <repo_id>
/list_claude
/list_codex
/list_gemini
/flush_gemini
@gemini hello from Telegram
```

Use `/switch_rp` without arguments to choose from Telegram buttons. Use
`/switch_rp <repo_id>` when you already know the repo ID. Run
`agent-bus repo list` locally to see repo IDs.

Use `/list_gemini` to pick a Gemini session for the current repo. After that,
`@gemini <message>` resumes that selected Gemini session. If no Gemini session
has been selected yet, `@gemini <message>` falls back to headless Gemini CLI in
the current default repo.

For Codex, `@codex <message>` follows the selected session using the repo's
`codex_mode`:
- `live_bridge` keeps the existing desktop-owned live bridge.
- `app_server` resumes the selected Codex thread through `codex app-server`.

Gemini uses `--approval-mode plan` by default; set
`AGENT_BUS_GEMINI_APPROVAL_MODE` only if you want to test a less restrictive
mode. `@flush_gemini` is a no-op informational command because the Gemini
bridge is resume-based and does not maintain transcript sync files.

After adding or removing repos, restart the daemon so Telegram sees the updated
registry:

```bash
systemctl --user restart agent-bus
```

## Step 8 - Set up the approval gate (optional)

The approval gate intercepts Bash, Write, Edit, and MultiEdit commands from Claude Code
and sends a Telegram message with **Approve** / **Deny** buttons before execution.
Commands that do not match any pattern are silently approved.

The hook waits up to 180 seconds for the daemon (or longer if configured via
`~/.agent-bus/config.yaml` → `permissions.timeout_seconds`). The daemon default
is 30s. Effective timeout = min(hook_timeout_ms, daemon_timeout_seconds * 1000).

Initialize the gate and add your first rules (requires `sudo`):

```bash
# Add destructive patterns — denied when daemon is unreachable
sudo agent-bus gate add '(^|\s)rm\s+-[rRfF]' --destructive
sudo agent-bus gate add 'git\s+reset\s+--hard' --destructive
sudo agent-bus gate add 'git\s+push\s+.*--force' --destructive
sudo agent-bus gate add '(^|\s)dd\s+if=' --destructive
sudo agent-bus gate add 'DROP\s+TABLE' --destructive
sudo agent-bus gate add 'prisma\s+migrate\s+reset' --destructive

# Add non-destructive patterns — approved silently when daemon is unreachable
sudo agent-bus gate add 'npm\s+run\s+deploy'

# Review what you have
sudo agent-bus gate list
```

Install the hook in each project that should go through the gate:

```bash
agent-bus repo install-hook /path/to/project
```

After installing the hook, restart Claude Code in that project. The next time
Claude runs a Bash command matching a pattern, you will receive a Telegram
message with buttons to approve or deny it.

Send `/help` to your bot at any time to see all available commands as buttons.

## Deprecated: TELE_BUS_TOKEN

Earlier versions used `TELE_BUS_TOKEN`. It still works but emits a
deprecation warning. Switch to `TELE_BUS_BOT_TOKEN` when convenient.

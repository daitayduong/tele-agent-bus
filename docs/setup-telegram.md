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
/list_rp
/current
/switch_rp <repo_id>
```

Use `agent-bus repo list` locally to see repo IDs.

After adding or removing repos, restart the daemon so Telegram sees the updated
registry:

```bash
systemctl --user restart agent-bus
```

## Deprecated: TELE_BUS_TOKEN

Earlier versions used `TELE_BUS_TOKEN`. It still works but emits a
deprecation warning. Switch to `TELE_BUS_BOT_TOKEN` when convenient.

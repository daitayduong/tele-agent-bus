# Telegram Setup for agent-bus

## Prerequisites

- Telegram account
- `agent-bus` binary installed at `/usr/local/bin/agent-bus`

## Step 1 — Create a dedicated bot

**Important:** agent-bus must have its OWN bot. Do not reuse a bot used by
other tools (Claude MCP notifications, home automation, etc.) — Telegram
only allows one `getUpdates` consumer per bot, so sharing causes lost
messages and timeouts.

1. Open Telegram, search for `@BotFather`
2. Send `/newbot`
3. Follow prompts for name and username (e.g. `YourName Agent Bus`, `yourname_agentbus_bot`)
4. Copy the token — format `123456:ABCdef...`

## Step 2 — Get your chat_id

1. Search for your new bot in Telegram, send `/start`
2. Open `https://api.telegram.org/bot<TOKEN>/getUpdates` in your browser (replace `<TOKEN>`)
3. Find `chat":{"id":NNNN` — that's your chat_id

Alternative: message `@userinfobot` and it replies with your user ID.

## Step 3 — Export env vars

```bash
export TELE_BUS_BOT_TOKEN='123456:ABCdef...'
export TELE_BUS_CHAT_ID='YOUR_CHAT_ID'
```

Add to `~/.bashrc` or `~/.zshrc` for persistence. Or put them in
`/etc/agent-bus/env` (chmod 0640, owned root:agent-bus) and reference from
your systemd unit.

## Step 4 — Verify

```bash
agent-bus config validate
agent-bus daemon       # should NOT log "TerminatedByOtherGetUpdates"
```

If you see that error, another client is polling the same bot. Either
stop the other client or create a separate bot.

## Deprecated: TELE_BUS_TOKEN

Earlier versions used `TELE_BUS_TOKEN`. It still works but emits a
deprecation warning. Switch to `TELE_BUS_BOT_TOKEN` when convenient.

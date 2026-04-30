# Experimental Windows Setup

Windows support is an experimental MVP. It is intended for testing the
Telegram daemon, repository selection, and agent message routing from a normal
PowerShell session.

Not supported yet on Windows:

- `agent-bus-hook` live permission approvals through daemon IPC.
- Windows Service installation.
- Codex desktop live IPC bridge.

## Prerequisites

- Windows 10/11.
- Rust 1.75+ from <https://rustup.rs/>.
- A dedicated Telegram bot token from `@BotFather`.
- Claude Code, Gemini CLI, or Codex CLI installed if you want to route work to
  those agents.

## Build

Open PowerShell in the repository root:

```powershell
cargo build --release -p agent-bus
```

The binary will be created at:

```text
target\release\agent-bus.exe
```

Optional hook binary build:

```powershell
cargo build --release -p agent-bus-hook
```

The hook currently falls back locally on Windows because daemon IPC is not
ported yet.

## Configure

Use a Windows-friendly agent-bus home:

```powershell
$env:AGENT_BUS_HOME = "$env:USERPROFILE\.agent-bus"
target\release\agent-bus.exe init
```

Set your Telegram values for the current PowerShell session:

```powershell
$env:TELE_BUS_BOT_TOKEN = "123456:ABCdef..."
$env:TELE_BUS_CHAT_ID = "123456789"
```

Register a repo:

```powershell
target\release\agent-bus.exe repo add C:\path\to\project
target\release\agent-bus.exe repo list
```

Validate config:

```powershell
target\release\agent-bus.exe config validate
```

## Run

Run the daemon in the foreground:

```powershell
target\release\agent-bus.exe daemon
```

Keep this PowerShell window open while testing. Do not run two daemon processes
with the same bot token because Telegram allows only one `getUpdates` consumer
per bot.

## Test From Telegram

Send these commands to your bot:

```text
/switch_rp
/current
/list_claude
/list_codex
@claude hello from Windows
@codex hello from Windows
```

If `@claude` or `@codex` fails, confirm the matching CLI is installed and
available in the same PowerShell `PATH` used to start the daemon.

## Persistent Environment

For repeated manual testing, set user-level environment variables:

```powershell
[Environment]::SetEnvironmentVariable("AGENT_BUS_HOME", "$env:USERPROFILE\.agent-bus", "User")
[Environment]::SetEnvironmentVariable("TELE_BUS_BOT_TOKEN", "123456:ABCdef...", "User")
[Environment]::SetEnvironmentVariable("TELE_BUS_CHAT_ID", "123456789", "User")
```

Open a new PowerShell window after setting them.

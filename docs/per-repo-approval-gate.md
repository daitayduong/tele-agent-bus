# Per-Repo Approval Gates

The agent-bus supports per-repository approval gates, which allow for more granular control over command execution. This document explains how they work and how to use them.

## How It Works

When an agent (currently Claude Code) runs a Bash, Write, Edit, or MultiEdit command,
the `agent-bus-hook` binary intercepts it and sends it to the daemon via a Unix socket.
The daemon first checks if the command is suspicious (contains `base64`, `eval`, `$()`,
backticks, `|sh`, `|bash`, `|python`, or `exec`); if so, it treats it as destructive
and always requires approval. Otherwise, the daemon checks whether the command matches
any pattern in the approval gate (global + per-repo union). If it matches, the daemon
sends a Telegram message with **Approve** and **Deny** buttons; if no pattern matches,
the command is silently approved.

Approved/denied commands are recorded in the Telegram chat with full original context
appended: `✅ Approved by @user` or `❌ Denied by @user`. This creates an audit trail.

The hook is installed per-project via `agent-bus repo install-hook <path>`, which writes
a `PreToolUse` entry into `.claude/settings.json`. Other agents (Gemini, Codex,
Antigravity) use their own internal approval systems and do not route through the
agent-bus gate today.

## Suspicious Command Heuristic

Before checking against any approval gate patterns, the daemon runs a **suspicious pattern check**:
any command containing the following substrings is automatically flagged as destructive and requires approval:

```
base64
eval
$(         (command substitution)
`          (backtick substitution)
|sh        (pipe to shell)
|bash      (pipe to bash)
|python    (pipe to python)
exec       (exec invocation)
```

This heuristic bypasses all gate patterns and runs BEFORE per-repo/global gate regex matching.
It catches encoded and indirect invocations that the gate patterns might miss.

## File Format

Each line in an `approval-gate.conf` file is either:

```
<regex>
<regex>\tdestructive
```

Where `\t` is a literal tab character. The `destructive` flag changes how the rule behaves when the daemon is unreachable (see Fail Behavior below).

Example `approval-gate.conf`:
```
(^|\s)rm\s+-[rRfF].*	destructive
git\s+reset\s+--hard	destructive
git\s+push\s+.*--force	destructive
(^|\s)dd\s+if=	destructive
DROP\s+TABLE	destructive
prisma\s+migrate\s+reset	destructive
npm\s+run\s+deploy
```

The last rule (`npm run deploy`) has no flag — it still prompts for approval but is treated as non-destructive when the daemon is down.

## The `--destructive` Flag

When adding a rule with `--destructive`, the agent marks it as fail-closed:

```bash
# Without flag — approve silently when daemon is unreachable
agent-bus gate add 'npm\s+run\s+deploy'

# With flag — deny when daemon is unreachable (fail-closed)
sudo agent-bus gate add 'git\s+reset\s+--hard' --destructive
sudo agent-bus gate add '(^|\s)rm\s+-[rRfF]' --destructive
```

Use `--destructive` for any command that could cause irreversible data loss. When the daemon is down (network issue, service stopped) and a destructive-flagged command is triggered, the hook exits with code 2 (deny) and logs `daemon_unreachable verdict=deny_destructive`.

## Fail Behavior (Hybrid Mode)

The default `fail_mode: hybrid` in `config.yaml`:

| Daemon status | Rule flag | Result |
|---|---|---|
| Running | any | Telegram approval prompt sent |
| Down | no flag | Auto-approve (silent) |
| Down | `destructive` | Deny immediately (fail-closed) |

## Merge Semantics (UNION)

Per-repo approval gates are merged with the global gate using UNION semantics. This means that a command is blocked if it matches a pattern in *either* the global gate or the per-repo gate.

**Per-repo gates can only *add* restrictions; they can never relax rules defined in the global gate.**

For example:
- If `^rm -rf` is in the global gate...
- And the per-repo gate for `my-project` does *not* contain `^rm -rf`...
- The `rm -rf` command will still be blocked in `my-project` because of the global rule.

If a per-repo gate file is missing, it is treated as an empty list of patterns. If either the global or per-repo gate file is tampered with (i.e., the HMAC signature does not match), the system will fail-closed, denying all command executions.

## CLI Usage

The `agent-bus gate` subcommands (`add`, `remove`, `list`, `verify`) all accept a `--repo <repo_id>` flag to operate on a per-repo gate.

**Global gate (requires sudo):**

```bash
# Add a destructive rule to the global gate
sudo agent-bus gate add '(^|\s)rm\s+-[rRfF]' --destructive

# Add a non-destructive rule
sudo agent-bus gate add 'npm\s+run\s+deploy'

# List global rules
sudo agent-bus gate list

# Remove a global rule
sudo agent-bus gate remove '(^|\s)rm\s+-[rRfF]'

# Verify HMAC integrity
sudo agent-bus gate verify
```

**Per-repo gate (no sudo needed):**

```bash
# Add a repo-specific rule
agent-bus gate add --repo my-project '^git push --force' --destructive

# List per-repo rules
agent-bus gate list --repo my-project

# Remove a per-repo rule
agent-bus gate remove --repo my-project '^git push --force'

# Verify per-repo integrity
agent-bus gate verify --repo my-project
```

When the `--repo` flag is used, these commands do **not** require `sudo`.

## Storage Location

Per-repo approval gates are stored in your agent-bus home directory, typically `~/.agent-bus/`. Each repository has a dedicated subdirectory named after its ID.

- **Path:** `~/.agent-bus/repos/<repo_id>/approval-gate.conf`
- **HMAC:** `~/.agent-bus/repos/<repo_id>/approval-gate.conf.hmac`

These files are owned by the user who runs the `agent-bus gate` commands.

## Key Sharing and Security

Per-repo approval gates are signed using the same shared HMAC key as the global gate.

- **Key Location:** `/etc/agent-bus/approval-gate.key`

This key is owned by `root:agent-bus` with `0640` permissions. To read this key for per-repo operations, your user account must be a member of the `agent-bus` group.

**To add your user to the group:**

```bash
sudo usermod -aG agent-bus $USER
```

After running this command, you must **log out and log back in** for the group change to take effect.

The rationale for sharing the key is:
1.  **Simpler Key Management:** There is only one key to manage and rotate.
2.  **Limited Risk:** Since per-repo approval gates can only add new restrictions, the risk of a compromised user account tampering with a gate is limited to a denial-of-service (by adding overly restrictive rules), not a security breach (by removing rules). Write access to the key file itself is still protected by `sudo`.

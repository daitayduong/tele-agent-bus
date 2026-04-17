# tele-agent-bus

Multi-agent orchestration daemon (Claude Code / Gemini CLI / Codex CLI) over Telegram.

**Status:** v0.1.0 — scaffolding only. See `docs/spec.md` for the full specification.

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

## Security disclaimer

The permission-gate blacklist is a **heuristic UX guardrail**, not a sandbox. It is trivially bypassable by shell quoting, command substitution, aliases, interpreters, or encoded payloads. Do not rely on it as a security boundary against adversarial code. See spec §10.1.

## Build

```bash
# Requires Rust 1.75+
cargo build --workspace
cargo test --workspace
```

## License

MIT

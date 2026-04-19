# Fake CLI Fixtures for Agent Bus Testing

These scripts are used to simulate the behavior of external CLI tools (like `claude` or `codex`) during Rust integration tests.

## Purpose
By setting `AGENT_BUS_CLAUDE_BIN` or `AGENT_BUS_CODEX_BIN` to the absolute path of these scripts, you can test how `agent-bus` handles different scenarios without requiring the actual external tools or active sessions.

## Available Scripts

| Script | Exit Code | Simulation |
|---|---|---|
| `claude_ok.sh` | 0 | Normal Claude execution. |
| `claude_quota.sh` | 1 | Claude usage limit reached (quota hit). |
| `claude_rate_limit.sh` | 1 | Claude rate limit exceeded. |
| `claude_auth_expired.sh` | 1 | Claude session expired or user not logged in. |
| `codex_ok.sh` | 0 | Normal Codex execution. |
| `codex_auth_expired.sh` | 1 | Codex session expired or user not signed in. |

## Usage in Rust Tests
Example of setting up a test:

```rust
let test_bin = std::env::current_dir()?.join("crates/agent-bus/tests/fixtures/fake-cli/claude_ok.sh");
std::env::set_var("AGENT_BUS_CLAUDE_BIN", test_bin);
```

## Behavior
Each script:
1. Reads from `stdin` (simulating the prompt sent to the agent).
2. For Claude scripts, if `--resume <uuid>` is passed, it outputs `[resumed uuid=<uuid>]` to `stdout`.
3. If `CLAUDE_CONFIG_DIR` or `CODEX_HOME` is set, it outputs `[config=$VAR]` to `stdout`.
4. Outputs either the result (to `stdout`) or an error message (to `stderr`) and exits with the corresponding code.

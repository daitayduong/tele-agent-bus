//! Core logic for tele-agent-bus.
//!
//! Modules planned per spec §3–§10:
//!   - `config`     — YAML loader with `env:` prefix resolution
//!   - `state`      — State actor (tokio mpsc), atomic write (`state.json.tmp` → fsync → rename)
//!   - `blacklist`  — regex matcher with `destructive` flag awareness + "suspicious" heuristic
//!   - `redact`     — logging redaction (secrets, chat_ids, command hashes)
//!   - `path_validate` — canonicalize + traversal + forbidden-root check
//!   - `repo_id`    — `<slug>_<hash8(abs_path)>` collision-free internal IDs

#![deny(unsafe_code)]

// TODO: declare modules once contributors start filling them in.
pub mod config;
// pub mod state;
pub mod blacklist;
// pub mod redact;
// pub mod path_validate;
// pub mod repo_id;

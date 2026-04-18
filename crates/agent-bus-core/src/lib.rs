//! Core logic for tele-agent-bus.
//!
//! Modules per spec §3–§10:
//!   - `config`     — YAML loader with `env:` prefix resolution
//!   - `state`      — State actor (tokio mpsc), atomic write (`state.json.tmp` → fsync → rename)
//!   - `blacklist`  — regex matcher with `destructive` flag awareness + "suspicious" heuristic
//!   - `redact`     — logging redaction (secrets, chat_ids, command hashes)
//!   - `path_validate` — canonicalize + traversal + forbidden-root check
//!   - `repo_id`    — `<slug>_<hash8(abs_path)>` collision-free internal IDs
//!   - `peer_uid`   — `SO_PEERCRED` verification for UDS connections

#![deny(unsafe_code)]

pub mod auth_context;
pub mod blacklist;
pub mod blacklist_integrity;
pub mod classifier;
pub mod config;
pub mod path_validate;
pub mod peer_uid;
pub mod redact;
pub mod repo_id;
pub mod state;

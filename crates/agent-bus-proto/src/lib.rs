//! Wire protocol types for tele-agent-bus.
//!
//! This crate is embedded by the `agent-bus-hook` binary — must stay minimal
//! (no async runtime, no heavy deps). Only `serde` + `thiserror`.
//!
//! See spec §7 (API Contract) and §3.2 (cargo workspace layout).

#![deny(unsafe_code)]

pub const PROTOCOL_VERSION: u32 = 1;

// TODO: define request/response types per spec §7
//   - PermCheckRequest / PermCheckResponse
//   - StateResponse (redacted)
//   - RepoListResponse
//   - InboxSendRequest
//   - SetDefaultRepoRequest
//   - ProtocolError enum with thiserror

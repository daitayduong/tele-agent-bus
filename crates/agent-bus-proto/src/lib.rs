//! Wire protocol types for tele-agent-bus.
//!
//! This crate is embedded by the `agent-bus-hook` binary — must stay minimal
//! (no async runtime, no heavy deps). Only `serde` + `thiserror`.
//!
//! See spec §7 (API Contract) and §3.2 (cargo workspace layout).

#![deny(unsafe_code)]

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct PermCheckRequest { }

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct PermCheckResponse { }

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("Unknown error")]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_perm_check_request_serialization() {
        let req = PermCheckRequest { };
        let j = serde_json::to_value(&req).unwrap();
        assert_eq!(j["protocol_version"], 1);
        assert_eq!(j["tool"], "Bash");
    }

    #[test]
    fn test_perm_check_response_serialization() {
        let resp = PermCheckResponse { };
        let j = serde_json::to_value(&resp).unwrap();
        assert_eq!(j["verdict"], "approve");
    }
}

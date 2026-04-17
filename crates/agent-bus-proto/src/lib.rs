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
pub struct PermCheckRequest {
    pub protocol_version: u32,
    pub request_id: String,
    pub session_id: String,
    pub tool: String,
    pub command: String,
    pub repo_hint: Option<String>,
    pub timeout_ms: u64,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct PermCheckResponse {
    pub protocol_version: u32,
    pub request_id: String,
    pub req_id: String,
    pub verdict: Decision,
    pub reason: String,
    pub matched_pattern: Option<String>,
    pub destructive: bool,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Approve,
    Deny,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct RepoInfo {
    pub id: String,
    pub display: String,
    pub path: String,
    pub agents: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct RepoListResponse {
    pub protocol_version: u32,
    pub repos: Vec<RepoInfo>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct InboxSendRequest {
    pub protocol_version: u32,
    pub repo_id: String,
    pub agent: String,
    pub task: String,
    pub content: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct SetDefaultRepoRequest {
    pub protocol_version: u32,
    pub chat_id: String,
    pub repo_id: String,
}

#[derive(Debug, Error, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolError {
    #[error("Protocol version mismatch: expected {expected}, got {got}")]
    VersionMismatch { expected: u32, got: u32 },
    #[error("Internal server error: {0}")]
    Internal(String),
    #[error("Forbidden: {0}")]
    Forbidden(String),
    #[error("Bad request: {0}")]
    BadRequest(String),
    #[error("Not found: {0}")]
    NotFound(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_perm_check_request_serialization() {
        let req = PermCheckRequest {
            protocol_version: PROTOCOL_VERSION,
            request_id: "req-123".to_string(),
            session_id: "sess-456".to_string(),
            tool: "Bash".to_string(),
            command: "ls".to_string(),
            repo_hint: Some("rallyup".to_string()),
            timeout_ms: 10000,
        };
        let j = serde_json::to_value(&req).unwrap();
        assert_eq!(j["protocol_version"], 1);
        assert_eq!(j["tool"], "Bash");
        assert_eq!(j["command"], "ls");

        let de: PermCheckRequest = serde_json::from_value(j).unwrap();
        assert_eq!(de, req);
    }

    #[test]
    fn test_perm_check_response_serialization() {
        let resp = PermCheckResponse {
            protocol_version: PROTOCOL_VERSION,
            request_id: "req-123".to_string(),
            req_id: "req-server-123".to_string(),
            verdict: Decision::Approve,
            reason: "not blacklisted".to_string(),
            matched_pattern: None,
            destructive: false,
        };
        let j = serde_json::to_value(&resp).unwrap();
        assert_eq!(j["verdict"], "approve");
        assert_eq!(j["destructive"], false);

        let de: PermCheckResponse = serde_json::from_value(j).unwrap();
        assert_eq!(de, resp);
    }

    #[test]
    fn test_decision_serialization() {
        assert_eq!(serde_json::to_value(Decision::Approve).unwrap(), json!("approve"));
        assert_eq!(serde_json::to_value(Decision::Deny).unwrap(), json!("deny"));
    }

    #[test]
    fn test_forward_compat() {
        let j = json!({
            "protocol_version": 1,
            "request_id": "id",
            "session_id": "sid",
            "tool": "Bash",
            "command": "ls",
            "timeout_ms": 1000,
            "extra_field": "should be ignored"
        });
        let de: bool = serde_json::from_value::<PermCheckRequest>(j).is_ok_and(|_| true);
        assert!(de);
    }
}

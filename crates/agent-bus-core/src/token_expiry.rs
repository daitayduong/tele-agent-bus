//! Phase 4a.12 — read provider credential files to surface token TTL.
//!
//! We intentionally do NOT touch browser profile dirs, system keyrings,
//! or any path outside `~/.agent-bus/auth/<agent>/<id>/` (enforced by
//! the caller passing us the profile_dir from a validated config).

use serde::Deserialize;
use std::path::Path;
use time::OffsetDateTime;
use tracing::warn;

pub const WARNING_WINDOW_SECS: i64 = 72 * 60 * 60; // 72 hours

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpiryStatus {
    /// No credential file found, or format unrecognized. Treat as unknown.
    Unknown,
    /// Expiry > now + 72h.
    Healthy { expires_at: OffsetDateTime },
    /// 0 < (expires_at - now) <= 72h.
    ExpiringSoon { expires_at: OffsetDateTime },
    /// expires_at <= now.
    Expired { expires_at: OffsetDateTime },
}

#[derive(Deserialize)]
struct ClaudeCredentials {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: ClaudeOauth,
}

#[derive(Deserialize)]
struct ClaudeOauth {
    #[serde(rename = "expiresAt")]
    expires_at: i64, // unix epoch in milliseconds
}

#[derive(Deserialize)]
struct CodexAuth {
    tokens: CodexTokens,
}

#[derive(Deserialize)]
struct CodexTokens {
    #[serde(with = "time::serde::rfc3339")]
    expires_at: OffsetDateTime,
}

/// Read `<profile_dir>/.credentials.json`. Returns Unknown on missing file
/// or parse error (never an Err).
pub fn read_claude(profile_dir: &Path) -> ExpiryStatus {
    read_claude_with_now(profile_dir, OffsetDateTime::now_utc())
}

pub(crate) fn read_claude_with_now(profile_dir: &Path, now: OffsetDateTime) -> ExpiryStatus {
    let path = profile_dir.join(".credentials.json");
    if !path.exists() {
        return ExpiryStatus::Unknown;
    }

    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            warn!("failed to read claude credentials at {:?}: {}", path, e);
            return ExpiryStatus::Unknown;
        }
    };

    let creds: ClaudeCredentials = match serde_json::from_str(&content) {
        Ok(c) => c,
        Err(e) => {
            warn!("failed to parse claude credentials at {:?}: {}", path, e);
            return ExpiryStatus::Unknown;
        }
    };

    let expires_at_ms = creds.claude_ai_oauth.expires_at;
    let expires_at =
        match OffsetDateTime::from_unix_timestamp_nanos((expires_at_ms as i128) * 1_000_000) {
            Ok(t) => t,
            Err(e) => {
                warn!(
                    "invalid expiresAt in claude credentials at {:?}: {}",
                    path, e
                );
                return ExpiryStatus::Unknown;
            }
        };

    to_status(expires_at, now)
}

/// Read `<profile_dir>/auth.json`. Returns Unknown on missing file
/// or parse error.
pub fn read_codex(profile_dir: &Path) -> ExpiryStatus {
    read_codex_with_now(profile_dir, OffsetDateTime::now_utc())
}

pub(crate) fn read_codex_with_now(profile_dir: &Path, now: OffsetDateTime) -> ExpiryStatus {
    let path = profile_dir.join("auth.json");
    if !path.exists() {
        return ExpiryStatus::Unknown;
    }

    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            warn!("failed to read codex auth at {:?}: {}", path, e);
            return ExpiryStatus::Unknown;
        }
    };

    let auth: CodexAuth = match serde_json::from_str(&content) {
        Ok(a) => a,
        Err(e) => {
            warn!("failed to parse codex auth at {:?}: {}", path, e);
            return ExpiryStatus::Unknown;
        }
    };

    to_status(auth.tokens.expires_at, now)
}

/// Dispatcher by agent name ("claude" | "codex" | anything else → Unknown).
pub fn read_for_agent(agent: &str, profile_dir: &Path) -> ExpiryStatus {
    match agent {
        "claude" => read_claude(profile_dir),
        "codex" => read_codex(profile_dir),
        _ => ExpiryStatus::Unknown,
    }
}

fn to_status(expires_at: OffsetDateTime, now: OffsetDateTime) -> ExpiryStatus {
    if expires_at <= now {
        ExpiryStatus::Expired { expires_at }
    } else if (expires_at - now).whole_seconds() <= WARNING_WINDOW_SECS {
        ExpiryStatus::ExpiringSoon { expires_at }
    } else {
        ExpiryStatus::Healthy { expires_at }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn mock_now() -> OffsetDateTime {
        // 2026-04-18 12:00:00 UTC
        OffsetDateTime::from_unix_timestamp(1776513600).unwrap()
    }

    #[test]
    fn test_to_status() {
        let now = mock_now();

        // Healthy: > 72h
        let expires_at = now + time::Duration::hours(73);
        assert_eq!(
            to_status(expires_at, now),
            ExpiryStatus::Healthy { expires_at }
        );

        // ExpiringSoon: <= 72h
        let expires_at = now + time::Duration::hours(72);
        assert_eq!(
            to_status(expires_at, now),
            ExpiryStatus::ExpiringSoon { expires_at }
        );

        // ExpiringSoon: > 0h
        let expires_at = now + time::Duration::seconds(1);
        assert_eq!(
            to_status(expires_at, now),
            ExpiryStatus::ExpiringSoon { expires_at }
        );

        // Expired: <= now
        let expires_at = now;
        assert_eq!(
            to_status(expires_at, now),
            ExpiryStatus::Expired { expires_at }
        );

        let expires_at = now - time::Duration::seconds(1);
        assert_eq!(
            to_status(expires_at, now),
            ExpiryStatus::Expired { expires_at }
        );
    }

    #[test]
    fn test_read_claude() {
        let dir = TempDir::new().unwrap();
        let now = mock_now();
        let path = dir.path().join(".credentials.json");

        // Missing file
        assert_eq!(read_claude_with_now(dir.path(), now), ExpiryStatus::Unknown);

        // Malformed JSON
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(read_claude_with_now(dir.path(), now), ExpiryStatus::Unknown);

        // Missing expiresAt
        std::fs::write(&path, r#"{"claudeAiOauth": {}}"#).unwrap();
        assert_eq!(read_claude_with_now(dir.path(), now), ExpiryStatus::Unknown);

        // Healthy
        let expires_at_ms = (now + time::Duration::hours(100)).unix_timestamp_nanos() / 1_000_000;
        std::fs::write(
            &path,
            format!(
                r#"{{"claudeAiOauth": {{"expiresAt": {}, "accessToken": "..."}}}}"#,
                expires_at_ms
            ),
        )
        .unwrap();
        let status = read_claude_with_now(dir.path(), now);
        if let ExpiryStatus::Healthy { expires_at } = status {
            assert_eq!(expires_at.unix_timestamp_nanos() / 1_000_000, expires_at_ms);
        } else {
            panic!("expected Healthy, got {:?}", status);
        }

        // ExpiringSoon
        let expires_at_ms = (now + time::Duration::hours(2)).unix_timestamp_nanos() / 1_000_000;
        std::fs::write(
            &path,
            format!(r#"{{"claudeAiOauth": {{"expiresAt": {}}}}}"#, expires_at_ms),
        )
        .unwrap();
        let status = read_claude_with_now(dir.path(), now);
        assert!(matches!(status, ExpiryStatus::ExpiringSoon { .. }));

        // Expired
        let expires_at_ms = (now - time::Duration::hours(1)).unix_timestamp_nanos() / 1_000_000;
        std::fs::write(
            &path,
            format!(r#"{{"claudeAiOauth": {{"expiresAt": {}}}}}"#, expires_at_ms),
        )
        .unwrap();
        let status = read_claude_with_now(dir.path(), now);
        assert!(matches!(status, ExpiryStatus::Expired { .. }));
    }

    #[test]
    fn test_read_codex() {
        let dir = TempDir::new().unwrap();
        let now = mock_now();
        let path = dir.path().join("auth.json");

        // Missing file
        assert_eq!(read_codex_with_now(dir.path(), now), ExpiryStatus::Unknown);

        // Malformed JSON
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(read_codex_with_now(dir.path(), now), ExpiryStatus::Unknown);

        // Unparseable ISO
        std::fs::write(&path, r#"{"tokens": {"expires_at": "invalid"}}"#).unwrap();
        assert_eq!(read_codex_with_now(dir.path(), now), ExpiryStatus::Unknown);

        // Healthy
        std::fs::write(
            &path,
            r#"{"tokens": {"expires_at": "2026-07-01T12:00:00Z"}}"#,
        )
        .unwrap();
        let status = read_codex_with_now(dir.path(), now);
        assert!(matches!(status, ExpiryStatus::Healthy { .. }));

        // ExpiringSoon
        let expires_at = now + time::Duration::hours(48);
        let expires_at_str = expires_at
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        std::fs::write(
            &path,
            format!(r#"{{"tokens": {{"expires_at": "{}"}}}}"#, expires_at_str),
        )
        .unwrap();
        let status = read_codex_with_now(dir.path(), now);
        assert!(matches!(status, ExpiryStatus::ExpiringSoon { .. }));

        // Expired
        let expires_at = now - time::Duration::hours(48);
        let expires_at_str = expires_at
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        std::fs::write(
            &path,
            format!(r#"{{"tokens": {{"expires_at": "{}"}}}}"#, expires_at_str),
        )
        .unwrap();
        let status = read_codex_with_now(dir.path(), now);
        assert!(matches!(status, ExpiryStatus::Expired { .. }));
    }

    #[test]
    fn test_read_for_agent() {
        let dir = TempDir::new().unwrap();
        assert_eq!(read_for_agent("gemini", dir.path()), ExpiryStatus::Unknown);
    }
}

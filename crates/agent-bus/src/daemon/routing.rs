use regex::Regex;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Routed {
    pub agent: String,
    pub repo: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RoutingError {
    #[error("no routing prefix")]
    NoMatch,
    #[error("no default repo")]
    NoDefaultRepo,
    #[error("invalid agent name")]
    InvalidAgentName,
    #[error("invalid repo name")]
    InvalidRepoName,
    #[error("empty message body")]
    EmptyBody,
    #[error("message too long")]
    MessageTooLong,
}

impl RoutingError {
    pub fn reason(&self) -> &'static str {
        match self {
            Self::NoMatch => "no_match",
            Self::NoDefaultRepo => "no_default_repo",
            Self::InvalidAgentName => "invalid_agent_name",
            Self::InvalidRepoName => "invalid_repo_name",
            Self::EmptyBody => "empty_body",
            Self::MessageTooLong => "message_too_long",
        }
    }
}

pub struct RoutingParser;

impl RoutingParser {
    pub fn parse(input: &str, default_repo: Option<&str>) -> Result<Routed, RoutingError> {
        let trimmed = input.trim_start();
        let Some(after_at) = trimmed.strip_prefix('@') else {
            return Err(RoutingError::NoMatch);
        };

        let (target, body) = match after_at.find(char::is_whitespace) {
            Some(index) => (&after_at[..index], after_at[index..].trim()),
            None => (after_at, ""),
        };
        if body.is_empty() {
            return Err(RoutingError::EmptyBody);
        }
        if body.chars().count() > 4096 {
            return Err(RoutingError::MessageTooLong);
        }

        let (agent, repo) = match target.split_once(':') {
            Some((agent, repo)) => {
                validate_agent(agent)?;
                validate_repo(repo)?;
                (agent, repo)
            }
            None => {
                validate_agent(target)?;
                let repo = default_repo.ok_or(RoutingError::NoDefaultRepo)?;
                validate_repo(repo)?;
                (target, repo)
            }
        };

        Ok(Routed {
            agent: agent.to_string(),
            repo: repo.to_string(),
            body: body.to_string(),
        })
    }
}

fn validate_agent(agent: &str) -> Result<(), RoutingError> {
    let re = Regex::new(r"^[a-z][a-z0-9_-]{0,31}$").expect("valid agent regex");
    if re.is_match(agent) {
        Ok(())
    } else {
        Err(RoutingError::InvalidAgentName)
    }
}

fn validate_repo(repo: &str) -> Result<(), RoutingError> {
    let re = Regex::new(r"^[a-z0-9][a-z0-9_-]{0,63}$").expect("valid repo regex");
    if re.is_match(repo) {
        Ok(())
    } else {
        Err(RoutingError::InvalidRepoName)
    }
}

#[cfg(test)]
mod tests {
    use super::{Routed, RoutingError, RoutingParser};

    #[test]
    fn test_parse_explicit_repo() {
        let routed = RoutingParser::parse("@codex:sample_repo fix the lint warning", None).unwrap();

        assert_eq!(
            routed,
            Routed {
                agent: "codex".to_string(),
                repo: "sample_repo".to_string(),
                body: "fix the lint warning".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_default_repo_fallback() {
        let routed = RoutingParser::parse("  @codex fix the bug", Some("sample_repo")).unwrap();

        assert_eq!(routed.agent, "codex");
        assert_eq!(routed.repo, "sample_repo");
        assert_eq!(routed.body, "fix the bug");
    }

    #[test]
    fn test_parse_rejects_traversal() {
        assert_eq!(
            RoutingParser::parse("@../etc:sample_repo pwn", Some("sample_repo")).unwrap_err(),
            RoutingError::InvalidAgentName
        );
        assert_eq!(
            RoutingParser::parse("@codex:../../etc/passwd msg", Some("sample_repo")).unwrap_err(),
            RoutingError::InvalidRepoName
        );
    }

    #[test]
    fn test_parse_rejects_empty_body() {
        assert_eq!(
            RoutingParser::parse("@codex:sample_repo     ", None).unwrap_err(),
            RoutingError::EmptyBody
        );
    }

    #[test]
    fn test_parse_rejects_spaces_in_repo_id() {
        assert_eq!(
            RoutingParser::parse("@codex:Rally Up fix it", None).unwrap_err(),
            RoutingError::InvalidRepoName
        );
    }

    #[test]
    fn test_parse_ignores_at_mid_message() {
        assert_eq!(
            RoutingParser::parse("hello @codex foo", Some("sample_repo")).unwrap_err(),
            RoutingError::NoMatch
        );
    }

    #[test]
    fn test_parse_rejects_body_over_4096() {
        let body = "a".repeat(4097);
        let input = format!("@codex:sample_repo {body}");

        assert_eq!(
            RoutingParser::parse(&input, None).unwrap_err(),
            RoutingError::MessageTooLong
        );
    }
}

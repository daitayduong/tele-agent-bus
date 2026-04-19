//! Phase 4a — Failure classifier (§6 of `docs/specs/phase4-quota-rotation-lead.md`).
//!
//! Maps CLI run output `(exit_code, stdout, stderr, timed_out)` to a
//! [`ResultKind`] used by `AgentRunner` for rotation decisions and audit logs.
//!
//! Design:
//! - Rules are compiled regex patterns per agent/provider.
//! - First-match-wins within an agent's rule set; rules are scanned in
//!   declaration order, stderr first then stdout.
//! - `timed_out=true` always classifies as [`ResultKind::Timeout`] in v1.
//!   Pattern-aware timeout (e.g. "timed out while showing rate-limit text")
//!   is left for a future stretch (spec §6.2).
//! - Unknown non-zero exit → [`ResultKind::UnknownFailure`] (never rotates
//!   automatically unless explicitly configured by policy).

use std::sync::Arc;

use regex::Regex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultKind {
    Success,
    QuotaExhausted,
    RateLimited,
    AuthExpired,
    ManualReauthRequired,
    Timeout,
    UnknownFailure,
}

/// Snapshot of a single CLI invocation's output, fed to the classifier.
#[derive(Debug, Clone, Copy)]
pub struct RunOutput<'a> {
    /// Process exit code, or `None` if the process was killed (e.g. timeout).
    pub exit_code: Option<i32>,
    pub stdout: &'a str,
    pub stderr: &'a str,
    /// `true` if the outer timeout fired before the process exited.
    pub timed_out: bool,
}

/// Result of classification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Classification {
    pub kind: ResultKind,
    /// Identifier of the rule that matched, or `None` for terminal branches
    /// (`success`, `timeout`, `unknown_failure`). Format: `"<stream>_regex:<name>"`.
    pub classifier: Option<String>,
}

impl Classification {
    pub fn plain(kind: ResultKind) -> Self {
        Self {
            kind,
            classifier: None,
        }
    }

    pub fn matched(kind: ResultKind, classifier: String) -> Self {
        Self {
            kind,
            classifier: Some(classifier),
        }
    }
}

/// A single regex rule tied to a `ResultKind`.
#[derive(Debug, Clone)]
pub struct ClassifierRule {
    pub kind: ResultKind,
    /// Short stable name used in audit logs. Format guideline: `<agent>_<reason>`
    /// e.g. `claude_usage_limit`.
    pub name: String,
    pub pattern: Regex,
}

#[derive(Debug, Error)]
pub enum ClassifierError {
    #[error("invalid regex in rule {name}: {source}")]
    BadRegex {
        name: String,
        #[source]
        source: regex::Error,
    },
}

/// Collection of rules for a single agent/provider.
#[derive(Debug, Clone)]
pub struct ProviderClassifier {
    agent: String,
    rules: Arc<Vec<ClassifierRule>>,
}

impl ProviderClassifier {
    pub fn new(agent: impl Into<String>, rules: Vec<ClassifierRule>) -> Self {
        Self {
            agent: agent.into(),
            rules: Arc::new(rules),
        }
    }

    /// Compile a `(kind, name, pattern_str)` list into a classifier.
    pub fn from_specs(
        agent: impl Into<String>,
        specs: &[(ResultKind, &str, &str)],
    ) -> Result<Self, ClassifierError> {
        let mut rules = Vec::with_capacity(specs.len());
        for (kind, name, pat) in specs {
            let pattern = Regex::new(pat).map_err(|source| ClassifierError::BadRegex {
                name: (*name).to_string(),
                source,
            })?;
            rules.push(ClassifierRule {
                kind: *kind,
                name: (*name).to_string(),
                pattern,
            });
        }
        Ok(Self::new(agent, rules))
    }

    pub fn agent(&self) -> &str {
        &self.agent
    }

    pub fn rules(&self) -> &[ClassifierRule] {
        &self.rules
    }

    /// Classify a single run. See module docs for priority rules.
    pub fn classify(&self, out: &RunOutput<'_>) -> Classification {
        if out.timed_out {
            return Classification::plain(ResultKind::Timeout);
        }

        // Success path: exit code 0 AND no auth/quota rule matched stderr.
        // We still scan stderr for auth-expired rules even on exit 0, because
        // some CLIs warn on stderr while exiting 0 after a retry; but quota
        // rules only trigger on non-zero exit (conservative).
        let exit_ok = matches!(out.exit_code, Some(0));

        for stream_label in ["stderr", "stdout"] {
            let text = match stream_label {
                "stderr" => out.stderr,
                _ => out.stdout,
            };
            if text.is_empty() {
                continue;
            }
            for rule in self.rules.iter() {
                if exit_ok && is_failure_kind(rule.kind) {
                    // Don't flag quota/rate-limit on a successful exit.
                    continue;
                }
                if rule.pattern.is_match(text) {
                    let classifier = format!("{stream_label}_regex:{}", rule.name);
                    return Classification::matched(rule.kind, classifier);
                }
            }
        }

        match out.exit_code {
            Some(0) => Classification::plain(ResultKind::Success),
            _ => Classification::plain(ResultKind::UnknownFailure),
        }
    }
}

fn is_failure_kind(kind: ResultKind) -> bool {
    matches!(
        kind,
        ResultKind::QuotaExhausted
            | ResultKind::RateLimited
            | ResultKind::AuthExpired
            | ResultKind::ManualReauthRequired
    )
}

/// Built-in default rule specs per spec §6.2. Conservative set —
/// only high-confidence phrases observed in provider CLIs.
pub fn default_specs(agent: &str) -> &'static [(ResultKind, &'static str, &'static str)] {
    match agent {
        "claude" => CLAUDE_SPECS,
        "codex" => CODEX_SPECS,
        "gemini" => GEMINI_SPECS,
        _ => &[],
    }
}

const CLAUDE_SPECS: &[(ResultKind, &str, &str)] = &[
    (
        ResultKind::QuotaExhausted,
        "claude_usage_limit",
        r"(?i)usage\s+limit",
    ),
    (
        ResultKind::QuotaExhausted,
        "claude_quota_exceeded",
        r"(?i)quota.*exceeded",
    ),
    (
        ResultKind::RateLimited,
        "claude_try_again_later",
        r"(?i)try\s+again\s+later",
    ),
    (
        ResultKind::AuthExpired,
        "claude_not_logged_in",
        r"(?i)not\s+logged\s+in",
    ),
    (
        ResultKind::AuthExpired,
        "claude_please_log_in",
        r"(?i)please\s+log\s+in",
    ),
    (
        ResultKind::AuthExpired,
        "claude_please_sign_in",
        r"(?i)please\s+sign\s+in",
    ),
    (
        ResultKind::AuthExpired,
        "claude_invalid_api_key",
        r"(?i)invalid\s+(or\s+expired\s+)?api\s+key",
    ),
];

const CODEX_SPECS: &[(ResultKind, &str, &str)] = &[
    (
        ResultKind::QuotaExhausted,
        "codex_usage_limit",
        r"(?i)usage\s+limit",
    ),
    (
        ResultKind::RateLimited,
        "codex_rate_limit",
        r"(?i)rate\s+limit",
    ),
    (
        ResultKind::QuotaExhausted,
        "codex_quota",
        r"(?i)quota",
    ),
    (
        ResultKind::AuthExpired,
        "codex_not_authenticated",
        r"(?i)not\s+authenticated",
    ),
    (
        ResultKind::AuthExpired,
        "codex_please_sign_in",
        r"(?i)please\s+sign\s+in",
    ),
];

const GEMINI_SPECS: &[(ResultKind, &str, &str)] = &[
    (
        ResultKind::QuotaExhausted,
        "gemini_quota_exceeded",
        r"(?i)quota.*exceeded",
    ),
    (
        ResultKind::QuotaExhausted,
        "gemini_resource_exhausted",
        r"(?i)resource[\s_]+exhausted",
    ),
    (
        ResultKind::RateLimited,
        "gemini_rate_limit",
        r"(?i)rate\s+limit",
    ),
    (
        ResultKind::AuthExpired,
        "gemini_authentication_required",
        r"(?i)authentication\s+required",
    ),
    (
        ResultKind::AuthExpired,
        "gemini_login_required",
        r"(?i)login\s+required",
    ),
];

/// Convenience: build the default classifier for a supported agent.
pub fn default_classifier(agent: &str) -> Option<ProviderClassifier> {
    let specs = default_specs(agent);
    if specs.is_empty() {
        return None;
    }
    ProviderClassifier::from_specs(agent, specs).ok()
}

/// Classify arbitrary text using the default rules for an agent. Returns
/// `(kind, rule_name)` if a match is found.
pub fn classify_text(agent: &str, text: &str) -> Option<(ResultKind, String)> {
    let classifier = default_classifier(agent)?;
    for rule in classifier.rules() {
        if rule.pattern.is_match(text) {
            return Some((rule.kind, rule.name.clone()));
        }
    }
    None
}


#[cfg(test)]
mod tests {
    use super::*;

    fn out<'a>(exit: Option<i32>, stderr: &'a str, stdout: &'a str, timed_out: bool) -> RunOutput<'a> {
        RunOutput {
            exit_code: exit,
            stdout,
            stderr,
            timed_out,
        }
    }

    #[test]
    fn success_when_exit_zero_no_match() {
        let c = default_classifier("claude").unwrap();
        let r = c.classify(&out(Some(0), "", "hello world", false));
        assert_eq!(r, Classification::plain(ResultKind::Success));
    }

    #[test]
    fn timeout_overrides_all_else() {
        let c = default_classifier("claude").unwrap();
        // Even if stderr looks like quota, timed_out=true wins in v1.
        let r = c.classify(&out(None, "usage limit reached", "", true));
        assert_eq!(r.kind, ResultKind::Timeout);
        assert!(r.classifier.is_none());
    }

    #[test]
    fn claude_quota_exhausted_matched_by_stderr() {
        let c = default_classifier("claude").unwrap();
        let r = c.classify(&out(
            Some(1),
            "Claude usage limit reached. Try again later.",
            "",
            false,
        ));
        assert_eq!(r.kind, ResultKind::QuotaExhausted);
        assert_eq!(r.classifier.as_deref(), Some("stderr_regex:claude_usage_limit"));
    }

    #[test]
    fn claude_auth_expired_matched() {
        let c = default_classifier("claude").unwrap();
        let r = c.classify(&out(Some(1), "Please log in to continue.", "", false));
        assert_eq!(r.kind, ResultKind::AuthExpired);
        assert!(r.classifier.as_deref().unwrap().contains("please_log_in"));
    }

    #[test]
    fn codex_rate_limit_matched() {
        let c = default_classifier("codex").unwrap();
        let r = c.classify(&out(Some(1), "rate limit exceeded", "", false));
        assert_eq!(r.kind, ResultKind::RateLimited);
        assert_eq!(r.classifier.as_deref(), Some("stderr_regex:codex_rate_limit"));
    }

    #[test]
    fn codex_auth_expired_please_sign_in() {
        let c = default_classifier("codex").unwrap();
        let r = c.classify(&out(Some(1), "please sign in first", "", false));
        assert_eq!(r.kind, ResultKind::AuthExpired);
    }

    #[test]
    fn gemini_resource_exhausted() {
        let c = default_classifier("gemini").unwrap();
        let r = c.classify(&out(Some(1), "RESOURCE_EXHAUSTED: quota", "", false));
        assert_eq!(r.kind, ResultKind::QuotaExhausted);
    }

    #[test]
    fn unknown_failure_when_exit_nonzero_no_match() {
        let c = default_classifier("claude").unwrap();
        let r = c.classify(&out(Some(42), "segmentation fault", "", false));
        assert_eq!(r, Classification::plain(ResultKind::UnknownFailure));
    }

    #[test]
    fn first_match_wins_in_declaration_order() {
        // Pattern A matches before B — both match the text, A wins.
        let rules = vec![
            ClassifierRule {
                kind: ResultKind::QuotaExhausted,
                name: "A".to_string(),
                pattern: Regex::new(r"(?i)foo").unwrap(),
            },
            ClassifierRule {
                kind: ResultKind::RateLimited,
                name: "B".to_string(),
                pattern: Regex::new(r"(?i)foo").unwrap(),
            },
        ];
        let c = ProviderClassifier::new("test", rules);
        let r = c.classify(&out(Some(1), "foo bar", "", false));
        assert_eq!(r.kind, ResultKind::QuotaExhausted);
        assert_eq!(r.classifier.as_deref(), Some("stderr_regex:A"));
    }

    #[test]
    fn case_insensitive_default_patterns() {
        let c = default_classifier("claude").unwrap();
        let r = c.classify(&out(Some(1), "USAGE LIMIT REACHED", "", false));
        assert_eq!(r.kind, ResultKind::QuotaExhausted);
    }

    #[test]
    fn stderr_scanned_before_stdout() {
        let rules = vec![
            ClassifierRule {
                kind: ResultKind::QuotaExhausted,
                name: "quota".to_string(),
                pattern: Regex::new(r"(?i)quota").unwrap(),
            },
            ClassifierRule {
                kind: ResultKind::AuthExpired,
                name: "auth".to_string(),
                pattern: Regex::new(r"(?i)auth").unwrap(),
            },
        ];
        let c = ProviderClassifier::new("test", rules);
        let r = c.classify(&out(
            Some(1),
            "auth failed",        // stderr matches 'auth' rule
            "quota exceeded",     // stdout matches 'quota' rule
            false,
        ));
        // stderr first → auth wins even though quota is declared earlier,
        // because we scan stderr fully before stdout.
        assert_eq!(r.kind, ResultKind::AuthExpired);
        assert_eq!(r.classifier.as_deref(), Some("stderr_regex:auth"));
    }

    #[test]
    fn exit_zero_ignores_failure_rules_but_accepts_success() {
        let c = default_classifier("claude").unwrap();
        // Exit 0 with "usage limit" text on stderr (e.g. warning) → still success.
        let r = c.classify(&out(Some(0), "usage limit warning", "done", false));
        assert_eq!(r.kind, ResultKind::Success);
    }

    #[test]
    fn bad_regex_returns_error() {
        let err = ProviderClassifier::from_specs(
            "test",
            &[(ResultKind::QuotaExhausted, "bad", r"(?P<")],
        )
        .unwrap_err();
        assert!(matches!(err, ClassifierError::BadRegex { .. }));
    }

    #[test]
    fn unknown_agent_has_no_default_classifier() {
        assert!(default_classifier("llama").is_none());
        assert!(default_specs("llama").is_empty());
    }

    #[test]
    fn empty_streams_with_nonzero_exit_is_unknown_failure() {
        let c = default_classifier("claude").unwrap();
        let r = c.classify(&out(Some(1), "", "", false));
        assert_eq!(r.kind, ResultKind::UnknownFailure);
    }
}

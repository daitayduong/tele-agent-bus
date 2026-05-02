use regex::{Regex, RegexSet};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum GateError {
    #[error("Regex error: {0}")]
    Regex(#[from] regex::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, PartialEq)]
pub struct GateRule {
    pub pattern: String,
    pub destructive: bool,
}

pub struct ApprovalGate {
    rules: Vec<GateRule>,
    regex_set: Option<RegexSet>,
}

impl Default for ApprovalGate {
    fn default() -> Self {
        Self::new()
    }
}

impl ApprovalGate {
    pub fn new() -> Self {
        Self {
            rules: vec![],
            regex_set: None,
        }
    }

    pub fn add_rule(&mut self, line: &str) -> Result<(), GateError> {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return Ok(());
        }

        let parts: Vec<&str> = line.split('\t').collect();
        let pattern = parts[0].trim().to_string();
        let mut destructive = false;

        if parts.len() > 1 {
            let flags = parts[1].split(',').map(|s| s.trim());
            for flag in flags {
                if flag == "destructive" {
                    destructive = true;
                }
            }
        }

        // Validate regex
        Regex::new(&pattern)?;

        self.rules.push(GateRule {
            pattern,
            destructive,
        });

        // Invalidate compiled set
        self.regex_set = None;

        Ok(())
    }

    pub fn load<P: AsRef<std::path::Path>>(&mut self, path: P) -> Result<(), GateError> {
        let content = std::fs::read_to_string(path)?;
        for line in content.lines() {
            self.add_rule(line)?;
        }
        self.compile()?;
        Ok(())
    }

    pub fn compile(&mut self) -> Result<(), GateError> {
        if self.rules.is_empty() {
            return Ok(());
        }
        let patterns: Vec<&String> = self.rules.iter().map(|r| &r.pattern).collect();
        self.regex_set = Some(RegexSet::new(patterns)?);
        Ok(())
    }

    pub fn check(&self, command: &str) -> Option<GateRule> {
        // First check hardcoded suspicious patterns (§10.1)
        if is_suspicious(command) {
            return Some(GateRule {
                pattern: "heuristic:suspicious".to_string(),
                destructive: true,
            });
        }

        if let Some(ref set) = self.regex_set {
            let matches: Vec<usize> = set.matches(command).into_iter().collect();
            if !matches.is_empty() {
                // Return the first match
                return Some(self.rules[matches[0]].clone());
            }
        } else {
            // Fallback if not compiled
            for rule in &self.rules {
                if let Ok(re) = Regex::new(&rule.pattern) {
                    if re.is_match(command) {
                        return Some(rule.clone());
                    }
                }
            }
        }
        None
    }
}

fn is_suspicious(command: &str) -> bool {
    // Spec §10.1: base64, eval, $(), ``, |sh, |bash, |python, exec
    let patterns = [
        "base64", "eval", "$(", "`", "|sh", "|bash", "|python", "exec",
    ];
    for p in patterns {
        if command.contains(p) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_match() {
        let mut bl = ApprovalGate::new();
        bl.add_rule("rm\\s+-rf").unwrap();
        bl.compile().unwrap();
        let matched = bl.check("rm -rf /").unwrap();
        assert_eq!(matched.pattern, "rm\\s+-rf");
    }

    #[test]
    fn test_flag_parsing() {
        let mut bl = ApprovalGate::new();
        bl.add_rule("rm\\s+-rf\tdestructive").unwrap();
        bl.compile().unwrap();
        let matched = bl.check("rm -rf /").unwrap();
        assert!(matched.destructive);
    }

    #[test]
    fn test_suspicious_heuristic() {
        let bl = ApprovalGate::new();
        let matched = bl.check("echo cm0gLXJmCg==|base64 -d|sh").unwrap();
        assert!(matched.destructive);
        assert_eq!(matched.pattern, "heuristic:suspicious");
    }

    #[test]
    fn test_performance_large_set() {
        let mut bl = ApprovalGate::new();
        for i in 0..1000 {
            bl.add_rule(&format!("^pattern_{}$", i)).unwrap();
        }
        bl.add_rule("final_pattern").unwrap();
        bl.compile().unwrap();

        let start = std::time::Instant::now();
        let matched = bl.check("final_pattern").unwrap();
        let duration = start.elapsed();

        assert_eq!(matched.pattern, "final_pattern");
        // Should be fast
        assert!(duration.as_millis() < 100);
    }

    #[test]
    fn test_malformed_regex() {
        let mut bl = ApprovalGate::new();
        let res = bl.add_rule("[a-z");
        assert!(res.is_err());
    }
}

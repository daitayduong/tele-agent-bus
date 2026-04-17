use regex::Regex;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BlacklistError {
    #[error("Regex error: {0}")]
    Regex(#[from] regex::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, PartialEq)]
pub struct BlacklistRule {
    pub pattern: String,
    pub destructive: bool,
}

pub struct Blacklist {
    rules: Vec<BlacklistRule>,
}

impl Blacklist {
    pub fn new() -> Self {
        Self { rules: vec![] }
    }

    pub fn add_rule(&mut self, _line: &str) -> Result<(), BlacklistError> {
        // RED: Does nothing
        Ok(())
    }

    pub fn compile(&mut self) -> Result<(), BlacklistError> {
        Ok(())
    }

    pub fn check(&self, _command: &str) -> Option<BlacklistRule> {
        // RED: Always returns None
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_match() {
        let mut bl = Blacklist::new();
        bl.add_rule("rm\\s+-rf").unwrap();
        bl.compile().unwrap();
        let matched = bl.check("rm -rf /");
        assert!(matched.is_some());
    }
}

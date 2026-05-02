//! Auth context registry for per-agent OAuth/profile rotation.
//!
//! Loads `~/.agent-bus/auth-contexts.yaml` with strict path/id validation.
//! Pure parsing + string-level safety. Filesystem-level checks (mode 0700,
//! existence) are deferred to the CLI register/login path and `AgentRunner`.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

use regex::Regex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const CONFIG_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum AuthConfigError {
    #[error("io error reading {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("yaml parse error in {path}: {source}")]
    Yaml {
        path: String,
        #[source]
        source: serde_yaml::Error,
    },
    #[error("unsupported version: expected {expected}, got {actual}")]
    Version { expected: u32, actual: u32 },
    #[error("invalid context id '{id}' for agent {agent}: must match ^[a-z][a-z0-9_-]{{0,31}}$")]
    BadId { agent: String, id: String },
    #[error("duplicate context id '{id}' for agent {agent}")]
    DuplicateId { agent: String, id: String },
    #[error("agent '{agent}' not supported (must be claude|codex|gemini|antigravity)")]
    UnsupportedAgent { agent: String },
    #[error("invalid lead agent '{agent}': must be claude|codex|gemini|antigravity")]
    BadLeadAgent { agent: String },
    #[error("profile_dir for {agent}/{id} must be under {expected}: got {actual}")]
    BadProfileDir {
        agent: String,
        id: String,
        expected: String,
        actual: String,
    },
    #[error("profile_dir for {agent}/{id} contains traversal (..): {path}")]
    ProfileDirTraversal {
        agent: String,
        id: String,
        path: String,
    },
    #[error("agent '{agent}' has active='{id}' but no such context is declared")]
    UnknownActiveId { agent: String, id: String },
}

pub const SUPPORTED_AGENTS: &[&str] = &["claude", "codex", "gemini", "antigravity"];

fn id_regex() -> Regex {
    Regex::new(r"^[a-z][a-z0-9_-]{0,31}$").expect("static regex")
}

// ── File-shape (as serialized in YAML) ──────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AuthContextsFile {
    pub version: u32,
    #[serde(default)]
    pub defaults: RawDefaults,
    #[serde(default)]
    pub agents: BTreeMap<String, RawAgentBlock>,
    #[serde(default)]
    pub lead: Option<RawLead>,
    #[serde(default)]
    pub mobile_context: Option<RawMobileContext>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawDefaults {
    #[serde(default)]
    pub auto_rotate: bool,
    #[serde(default = "default_true")]
    pub require_owner_approval: bool,
}

impl Default for RawDefaults {
    fn default() -> Self {
        Self {
            auto_rotate: false,
            require_owner_approval: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RawAgentBlock {
    /// Persistent "active" context id for this agent. When set, `AgentRunner`
    /// seeds `state.active_auth_context[agent]` at daemon startup. Written by
    /// the `agent-bus auth use` CLI. Optional; missing means "use first usable".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    #[serde(default)]
    pub contexts: Vec<RawContext>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawContext {
    pub id: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub owner: Option<String>,
    pub profile_dir: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub auto_rotate: Option<bool>,
    #[serde(default)]
    pub require_owner_approval: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawLead {
    #[serde(default = "default_lead_agent")]
    pub default: String,
    #[serde(default)]
    pub per_chat: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawMobileContext {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_mobile_max_bytes")]
    pub max_bytes: usize,
    #[serde(default = "default_mobile_max_messages")]
    pub max_messages: usize,
    #[serde(default)]
    pub include_tool_use: bool,
}

fn default_true() -> bool {
    true
}
fn default_lead_agent() -> String {
    "claude".to_string()
}
fn default_mobile_max_bytes() -> usize {
    32 * 1024
}
fn default_mobile_max_messages() -> usize {
    40
}

// ── Validated, ready-to-use shape ────────────────────────────────────────

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AuthContextsConfig {
    pub defaults: Defaults,
    pub agents: BTreeMap<String, Vec<AuthContext>>,
    /// Persistent "active" context id per agent (from `agents.<agent>.active`).
    /// Consumed by AgentRunner at startup to seed `active_auth_context`.
    pub active: BTreeMap<String, String>,
    pub lead: Option<LeadConfig>,
    pub mobile_context: MobileContextConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Defaults {
    pub auto_rotate: bool,
    pub require_owner_approval: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthContext {
    pub agent: String,
    pub id: String,
    pub label: Option<String>,
    pub owner: Option<String>,
    pub profile_dir: PathBuf, // absolute, expanded, validated against auth root
    pub enabled: bool,
    pub auto_rotate: bool,
    pub require_owner_approval: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeadConfig {
    pub default: AgentKind,
    pub per_chat: BTreeMap<String, AgentKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    Claude,
    Codex,
    Gemini,
    Antigravity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MobileContextConfig {
    pub enabled: bool,
    pub max_bytes: usize,
    pub max_messages: usize,
    pub include_tool_use: bool,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            auto_rotate: false,
            require_owner_approval: true,
        }
    }
}

impl AgentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::Antigravity => "antigravity",
        }
    }
}

impl fmt::Display for AgentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for AgentKind {
    type Err = AuthConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "gemini" => Ok(Self::Gemini),
            "antigravity" => Ok(Self::Antigravity),
            other => Err(AuthConfigError::BadLeadAgent {
                agent: other.to_string(),
            }),
        }
    }
}

impl Default for MobileContextConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_bytes: default_mobile_max_bytes(),
            max_messages: default_mobile_max_messages(),
            include_tool_use: false,
        }
    }
}

impl AuthContextsConfig {
    /// Parse YAML text with `home_dir` used to expand `~` and validate that
    /// `profile_dir` paths sit under `<home>/.agent-bus/auth/<agent>/`.
    pub fn parse(yaml: &str, home_dir: &Path) -> Result<Self, AuthConfigError> {
        let raw: AuthContextsFile =
            serde_yaml::from_str(yaml).map_err(|source| AuthConfigError::Yaml {
                path: "<inline>".to_string(),
                source,
            })?;
        Self::from_raw(raw, home_dir)
    }

    /// Load from a YAML file on disk.
    pub fn load(path: &Path, home_dir: &Path) -> Result<Self, AuthConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| AuthConfigError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let raw: AuthContextsFile =
            serde_yaml::from_str(&text).map_err(|source| AuthConfigError::Yaml {
                path: path.display().to_string(),
                source,
            })?;
        Self::from_raw(raw, home_dir)
    }

    fn from_raw(raw: AuthContextsFile, home_dir: &Path) -> Result<Self, AuthConfigError> {
        if raw.version != CONFIG_VERSION {
            return Err(AuthConfigError::Version {
                expected: CONFIG_VERSION,
                actual: raw.version,
            });
        }

        let defaults = Defaults {
            auto_rotate: raw.defaults.auto_rotate,
            require_owner_approval: raw.defaults.require_owner_approval,
        };

        let id_rx = id_regex();
        let auth_root = home_dir.join(".agent-bus").join("auth");
        let mut agents: BTreeMap<String, Vec<AuthContext>> = BTreeMap::new();
        let mut active: BTreeMap<String, String> = BTreeMap::new();

        for (agent, block) in raw.agents.into_iter() {
            if !SUPPORTED_AGENTS.contains(&agent.as_str()) {
                return Err(AuthConfigError::UnsupportedAgent {
                    agent: agent.clone(),
                });
            }
            let agent_root = auth_root.join(&agent);
            let mut seen_ids: std::collections::HashSet<String> = Default::default();
            let mut contexts = Vec::with_capacity(block.contexts.len());
            let active_id = block.active.clone();

            for ctx in block.contexts {
                if !id_rx.is_match(&ctx.id) {
                    return Err(AuthConfigError::BadId {
                        agent: agent.clone(),
                        id: ctx.id.clone(),
                    });
                }
                if !seen_ids.insert(ctx.id.clone()) {
                    return Err(AuthConfigError::DuplicateId {
                        agent: agent.clone(),
                        id: ctx.id.clone(),
                    });
                }

                let expanded = expand_tilde(&ctx.profile_dir, home_dir);
                if expanded
                    .components()
                    .any(|c| matches!(c, Component::ParentDir))
                {
                    return Err(AuthConfigError::ProfileDirTraversal {
                        agent: agent.clone(),
                        id: ctx.id.clone(),
                        path: ctx.profile_dir.clone(),
                    });
                }
                if !expanded.starts_with(&agent_root) {
                    return Err(AuthConfigError::BadProfileDir {
                        agent: agent.clone(),
                        id: ctx.id.clone(),
                        expected: agent_root.display().to_string(),
                        actual: expanded.display().to_string(),
                    });
                }

                contexts.push(AuthContext {
                    agent: agent.clone(),
                    id: ctx.id,
                    label: ctx.label,
                    owner: ctx.owner,
                    profile_dir: expanded,
                    enabled: ctx.enabled,
                    auto_rotate: ctx.auto_rotate.unwrap_or(defaults.auto_rotate),
                    require_owner_approval: ctx
                        .require_owner_approval
                        .unwrap_or(defaults.require_owner_approval),
                });
            }

            if let Some(id) = active_id {
                if !seen_ids.contains(&id) {
                    return Err(AuthConfigError::UnknownActiveId {
                        agent: agent.clone(),
                        id,
                    });
                }
                active.insert(agent.clone(), id);
            }

            agents.insert(agent, contexts);
        }

        let lead = raw.lead.map(parse_lead).transpose()?;

        let mobile_context = raw
            .mobile_context
            .map(|m| MobileContextConfig {
                enabled: m.enabled,
                max_bytes: m.max_bytes,
                max_messages: m.max_messages,
                include_tool_use: m.include_tool_use,
            })
            .unwrap_or_default();

        Ok(Self {
            defaults,
            agents,
            active,
            lead,
            mobile_context,
        })
    }

    pub fn contexts_for(&self, agent: &str) -> &[AuthContext] {
        self.agents.get(agent).map(|v| v.as_slice()).unwrap_or(&[])
    }

    pub fn context(&self, agent: &str, id: &str) -> Option<&AuthContext> {
        self.contexts_for(agent).iter().find(|c| c.id == id)
    }

    pub fn resolve_lead(&self, chat_id: Option<&str>) -> (AgentKind, LeadSource) {
        let Some(lead) = &self.lead else {
            return (AgentKind::Claude, LeadSource::Legacy);
        };
        if let Some(cid) = chat_id {
            if let Some(agent) = lead.per_chat.get(cid) {
                return (*agent, LeadSource::PerChat);
            }
        }
        (lead.default, LeadSource::Default)
    }

    pub fn resolve_agent_for_text(
        &self,
        text: &str,
        chat_id: Option<&str>,
    ) -> (AgentKind, LeadSource) {
        if let Some(agent) = explicit_agent_prefix(text) {
            return (agent, LeadSource::Explicit);
        }
        self.resolve_lead(chat_id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeadSource {
    Explicit,
    PerChat,
    Default,
    OverridePerChat,
    OverrideDefault,
    Legacy,
}

fn parse_lead(raw: RawLead) -> Result<LeadConfig, AuthConfigError> {
    let default = AgentKind::from_str(&raw.default)?;
    let mut per_chat = BTreeMap::new();
    for (chat_id, agent) in raw.per_chat {
        per_chat.insert(chat_id, AgentKind::from_str(&agent)?);
    }
    Ok(LeadConfig { default, per_chat })
}

pub fn explicit_agent_prefix(text: &str) -> Option<AgentKind> {
    let trimmed = text.trim_start();
    let after_at = trimmed.strip_prefix('@')?;
    let target = after_at
        .split_once(char::is_whitespace)
        .map(|(target, _)| target)
        .unwrap_or(after_at);
    let agent = target
        .split_once(':')
        .map(|(agent, _)| agent)
        .unwrap_or(target);
    AgentKind::from_str(agent).ok()
}

fn expand_tilde(path_str: &str, home_dir: &Path) -> PathBuf {
    if let Some(rest) = path_str.strip_prefix("~/") {
        home_dir.join(rest)
    } else if path_str == "~" {
        home_dir.to_path_buf()
    } else {
        PathBuf::from(path_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        PathBuf::from("/home/alice")
    }

    const MINIMAL: &str = r#"
version: 1
agents:
  claude:
    contexts:
      - id: john
        profile_dir: ~/.agent-bus/auth/claude/john
"#;

    #[test]
    fn parses_minimal_config() {
        let cfg = AuthContextsConfig::parse(MINIMAL, &home()).unwrap();
        assert_eq!(cfg.agents.get("claude").unwrap().len(), 1);
        let ctx = &cfg.agents["claude"][0];
        assert_eq!(ctx.id, "john");
        assert_eq!(
            ctx.profile_dir,
            PathBuf::from("/home/alice/.agent-bus/auth/claude/john")
        );
        assert!(ctx.enabled);
        // Defaults cascade: auto_rotate false, require_owner_approval true
        assert!(!ctx.auto_rotate);
        assert!(ctx.require_owner_approval);
    }

    #[test]
    fn defaults_cascade_to_contexts() {
        let y = r#"
version: 1
defaults:
  auto_rotate: true
  require_owner_approval: false
agents:
  claude:
    contexts:
      - id: a
        profile_dir: ~/.agent-bus/auth/claude/a
      - id: b
        profile_dir: ~/.agent-bus/auth/claude/b
        auto_rotate: false
"#;
        let cfg = AuthContextsConfig::parse(y, &home()).unwrap();
        let ctxs = &cfg.agents["claude"];
        assert!(ctxs[0].auto_rotate);
        assert!(!ctxs[0].require_owner_approval);
        assert!(!ctxs[1].auto_rotate); // explicit override
    }

    #[test]
    fn rejects_bad_id_uppercase() {
        let y = r#"
version: 1
agents:
  claude:
    contexts:
      - id: JOHN
        profile_dir: ~/.agent-bus/auth/claude/JOHN
"#;
        let err = AuthContextsConfig::parse(y, &home()).unwrap_err();
        assert!(matches!(err, AuthConfigError::BadId { .. }));
    }

    #[test]
    fn rejects_bad_id_starting_with_digit() {
        let y = r#"
version: 1
agents:
  claude:
    contexts:
      - id: 1john
        profile_dir: ~/.agent-bus/auth/claude/1john
"#;
        let err = AuthContextsConfig::parse(y, &home()).unwrap_err();
        assert!(matches!(err, AuthConfigError::BadId { .. }));
    }

    #[test]
    fn rejects_duplicate_id() {
        let y = r#"
version: 1
agents:
  claude:
    contexts:
      - id: x
        profile_dir: ~/.agent-bus/auth/claude/x
      - id: x
        profile_dir: ~/.agent-bus/auth/claude/x2
"#;
        let err = AuthContextsConfig::parse(y, &home()).unwrap_err();
        assert!(matches!(err, AuthConfigError::DuplicateId { .. }));
    }

    #[test]
    fn rejects_profile_dir_outside_auth_root() {
        let y = r#"
version: 1
agents:
  claude:
    contexts:
      - id: evil
        profile_dir: ~/.ssh
"#;
        let err = AuthContextsConfig::parse(y, &home()).unwrap_err();
        assert!(matches!(err, AuthConfigError::BadProfileDir { .. }));
    }

    #[test]
    fn rejects_profile_dir_wrong_agent_subfolder() {
        // profile_dir under auth/ but wrong agent folder
        let y = r#"
version: 1
agents:
  claude:
    contexts:
      - id: x
        profile_dir: ~/.agent-bus/auth/codex/x
"#;
        let err = AuthContextsConfig::parse(y, &home()).unwrap_err();
        assert!(matches!(err, AuthConfigError::BadProfileDir { .. }));
    }

    #[test]
    fn rejects_profile_dir_with_traversal() {
        let y = r#"
version: 1
agents:
  claude:
    contexts:
      - id: x
        profile_dir: ~/.agent-bus/auth/claude/../../../etc
"#;
        let err = AuthContextsConfig::parse(y, &home()).unwrap_err();
        assert!(matches!(err, AuthConfigError::ProfileDirTraversal { .. }));
    }

    #[test]
    fn rejects_unsupported_agent() {
        let y = r#"
version: 1
agents:
  llama:
    contexts:
      - id: x
        profile_dir: ~/.agent-bus/auth/llama/x
"#;
        let err = AuthContextsConfig::parse(y, &home()).unwrap_err();
        assert!(matches!(err, AuthConfigError::UnsupportedAgent { .. }));
    }

    #[test]
    fn rejects_unsupported_version() {
        let y = "version: 99\nagents: {}";
        let err = AuthContextsConfig::parse(y, &home()).unwrap_err();
        assert!(matches!(err, AuthConfigError::Version { .. }));
    }

    #[test]
    fn lead_block_missing_is_none() {
        let cfg = AuthContextsConfig::parse(MINIMAL, &home()).unwrap();
        assert_eq!(cfg.lead, None);
    }

    #[test]
    fn parses_valid_lead_block_with_default_and_per_chat() {
        let y = r#"
version: 1
agents: {}
lead:
  default: codex
  per_chat:
    "123": gemini
    "456": gemini
"#;
        let cfg = AuthContextsConfig::parse(y, &home()).unwrap();
        let lead = cfg.lead.as_ref().expect("lead block parsed");
        assert_eq!(lead.default, AgentKind::Codex);
        assert_eq!(lead.per_chat.get("123"), Some(&AgentKind::Gemini));
        assert_eq!(lead.per_chat.get("456"), Some(&AgentKind::Gemini));
    }

    #[test]
    fn resolve_explicit_prefix_wins_over_lead_config() {
        let y = r#"
version: 1
agents: {}
lead:
  default: codex
  per_chat:
    "123": claude
"#;
        let cfg = AuthContextsConfig::parse(y, &home()).unwrap();
        assert_eq!(
            cfg.resolve_agent_for_text("@gemini hello", Some("123")),
            (AgentKind::Gemini, LeadSource::Explicit)
        );
    }

    #[test]
    fn resolve_per_chat_match_returns_per_chat_agent() {
        let y = r#"
version: 1
agents: {}
lead:
  default: codex
  per_chat:
    "123": gemini
"#;
        let cfg = AuthContextsConfig::parse(y, &home()).unwrap();
        assert_eq!(
            cfg.resolve_agent_for_text("hello", Some("123")),
            (AgentKind::Gemini, LeadSource::PerChat)
        );
    }

    #[test]
    fn resolve_falls_back_to_default_without_per_chat_match() {
        let y = r#"
version: 1
agents: {}
lead:
  default: codex
  per_chat:
    "123": gemini
"#;
        let cfg = AuthContextsConfig::parse(y, &home()).unwrap();
        assert_eq!(
            cfg.resolve_agent_for_text("hello", Some("456")),
            (AgentKind::Codex, LeadSource::Default)
        );
    }

    #[test]
    fn resolve_missing_lead_config_returns_legacy_claude() {
        let cfg = AuthContextsConfig::parse(MINIMAL, &home()).unwrap();
        assert_eq!(
            cfg.resolve_agent_for_text("hello", Some("123")),
            (AgentKind::Claude, LeadSource::Legacy)
        );
    }

    #[test]
    fn lead_rejects_unsupported_agent() {
        let y = r#"
version: 1
agents: {}
lead:
  default: llama
"#;
        let err = AuthContextsConfig::parse(y, &home()).unwrap_err();
        assert!(matches!(err, AuthConfigError::BadLeadAgent { .. }));
    }

    #[test]
    fn lead_per_chat_rejects_unsupported_agent() {
        let y = r#"
version: 1
agents: {}
lead:
  default: claude
  per_chat:
    "1": llama
"#;
        let err = AuthContextsConfig::parse(y, &home()).unwrap_err();
        assert!(matches!(err, AuthConfigError::BadLeadAgent { .. }));
    }

    #[test]
    fn mobile_context_defaults_when_missing() {
        let cfg = AuthContextsConfig::parse(MINIMAL, &home()).unwrap();
        assert!(cfg.mobile_context.enabled);
        assert_eq!(cfg.mobile_context.max_bytes, 32 * 1024);
        assert_eq!(cfg.mobile_context.max_messages, 40);
        assert!(!cfg.mobile_context.include_tool_use);
    }

    #[test]
    fn mobile_context_overrides_parsed() {
        let y = r#"
version: 1
agents: {}
mobile_context:
  enabled: false
  max_bytes: 8192
  max_messages: 10
  include_tool_use: true
"#;
        let cfg = AuthContextsConfig::parse(y, &home()).unwrap();
        assert!(!cfg.mobile_context.enabled);
        assert_eq!(cfg.mobile_context.max_bytes, 8192);
        assert_eq!(cfg.mobile_context.max_messages, 10);
        assert!(cfg.mobile_context.include_tool_use);
    }

    #[test]
    fn context_lookup_by_id() {
        let cfg = AuthContextsConfig::parse(MINIMAL, &home()).unwrap();
        assert!(cfg.context("claude", "john").is_some());
        assert!(cfg.context("claude", "missing").is_none());
        assert!(cfg.context("codex", "john").is_none());
    }

    #[test]
    fn empty_agents_block_ok() {
        let y = "version: 1\nagents: {}";
        let cfg = AuthContextsConfig::parse(y, &home()).unwrap();
        assert!(cfg.agents.is_empty());
    }

    #[test]
    fn loads_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth-contexts.yaml");
        std::fs::write(&path, MINIMAL).unwrap();
        let cfg = AuthContextsConfig::load(&path, &home()).unwrap();
        assert_eq!(cfg.agents["claude"].len(), 1);
    }

    #[test]
    fn load_missing_file_is_io_error() {
        let err =
            AuthContextsConfig::load(Path::new("/nonexistent/path/auth-contexts.yaml"), &home())
                .unwrap_err();
        assert!(matches!(err, AuthConfigError::Io { .. }));
    }

    #[test]
    fn reject_unknown_top_level_field() {
        let y = r#"
version: 1
agents: {}
mystery_field: 42
"#;
        let err = AuthContextsConfig::parse(y, &home()).unwrap_err();
        assert!(matches!(err, AuthConfigError::Yaml { .. }));
    }
}

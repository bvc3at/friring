//! Data-driven [`AgentProvider`] built from an [`AgentDef`].
//!
//! Replaces the former hard-coded `ClaudeProvider`: any agent described in
//! `agents.toml` is launched through this single provider, which maps the
//! session's resume/fork/session ids onto the definition's argument templates.

use crate::session::{AgentDef, SessionConfig};

use super::provider::AgentProvider;

/// An [`AgentProvider`] backed by a declarative [`AgentDef`].
pub struct GenericProvider {
    def: AgentDef,
}

impl GenericProvider {
    /// Wrap an agent definition.
    pub fn new(def: AgentDef) -> Self {
        Self { def }
    }

    /// The underlying definition.
    pub fn def(&self) -> &AgentDef {
        &self.def
    }
}

impl AgentProvider for GenericProvider {
    fn command(&self) -> &str {
        &self.def.command
    }

    fn build_args(&self, config: &SessionConfig) -> Vec<String> {
        self.def.build_args(
            config.resume_session_id.as_deref(),
            config.fork_session_id.as_deref(),
            config.agent_session_id.as_deref(),
            config.session_name.as_deref(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::agent_config::builtin_registry;

    #[test]
    fn claude_provider_builds_resume_without_model() {
        let reg = builtin_registry();
        let provider = GenericProvider::new(reg.get("claude").unwrap().clone());
        assert_eq!(provider.command(), "claude");

        let config = SessionConfig {
            resume_session_id: Some("abc-123".into()),
            ..SessionConfig::default()
        };
        let args = provider.build_args(&config);
        assert_eq!(args, vec!["--resume", "abc-123"]);
        assert!(!args.iter().any(|a| a == "--model"));
    }

    #[test]
    fn fresh_session_pins_id_no_model() {
        let reg = builtin_registry();
        let provider = GenericProvider::new(reg.get("claude").unwrap().clone());

        let config = SessionConfig {
            agent_session_id: Some("new-id".into()),
            ..SessionConfig::default()
        };
        // No session name → the seeded `-n {name}` pair vanishes cleanly.
        let args = provider.build_args(&config);
        assert_eq!(args, vec!["--session-id", "new-id"]);
    }

    #[test]
    fn fresh_session_with_name_passes_it_to_claude() {
        let reg = builtin_registry();
        let provider = GenericProvider::new(reg.get("claude").unwrap().clone());

        let config = SessionConfig {
            agent_session_id: Some("new-id".into()),
            session_name: Some("fix auth flow".into()),
            ..SessionConfig::default()
        };
        assert_eq!(
            provider.build_args(&config),
            vec!["--session-id", "new-id", "-n", "fix auth flow"]
        );

        // Resuming the same session never re-pushes the name (the seeded
        // resume group carries no {name} token).
        let config = SessionConfig {
            resume_session_id: Some("new-id".into()),
            session_name: Some("fix auth flow".into()),
            ..SessionConfig::default()
        };
        assert_eq!(provider.build_args(&config), vec!["--resume", "new-id"]);
    }
}

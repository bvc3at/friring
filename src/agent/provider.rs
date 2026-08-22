use crate::session::{AgentDef, SessionConfig};

/// Abstraction over different coding agent CLIs (Claude, opencode, etc.).
///
/// Each provider knows how to build CLI arguments from a `SessionConfig`.
/// `Session` delegates command/arg construction to the provider, keeping
/// session lifecycle code agent-agnostic.
pub trait AgentProvider: Send + Sync {
    /// CLI command name (e.g., "claude", "opencode").
    fn command(&self) -> &str;

    /// Build CLI arguments from session config.
    fn build_args(&self, config: &SessionConfig) -> Vec<String>;

    /// The registry entry behind this provider, when there is one.
    ///
    /// The launch path reads the agent's `[agents.<name>.sandbox]` declaration
    /// and its credential family from it (see [`crate::agent::sandboxing`]).
    /// Defaulted to `None` so a test double stays two methods long: an agent
    /// with no definition is simply one the sandbox cannot help.
    fn agent_def(&self) -> Option<&AgentDef> {
        None
    }
}

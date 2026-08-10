use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
    Frame,
};

use super::theme::Theme;

/// Wider than the other confirmations because two of these lines carry a host
/// name. The static text is kept under fifty columns so it survives an 80-column
/// terminal, where the body is not wrapped but clipped.
const MODAL_WIDTH_PCT: u16 = 72;

pub struct SandboxDomainState<'a> {
    /// Which sandbox asked — the question is meaningless without it, because
    /// several sessions can be blocked on different hosts at once.
    pub session_name: &'a str,
    /// Profile the answer is written back to.
    pub profile: &'a str,
    /// The host as the agent asked for it.
    pub host: &'a str,
    pub port: u16,
    /// The rule an "Allow" stores and applies — shown verbatim, because it is
    /// narrower than the question looks: the host on that port only.
    pub rule: &'a str,
}

/// Render the first-use domain prompt: a sandboxed agent reached for a host its
/// profile does not allow.
///
/// The two things the user cannot guess are spelled out rather than implied —
/// exactly what an "Allow" writes into the profile (a port-scoped rule, not the
/// whole host), and that the filter believes the name the agent supplied. The
/// second is ADR-27's disclosure: without TLS interception an allowed host is
/// an allowed *name*, and this modal is where that grant is made.
pub fn render_sandbox_domain_modal(
    frame: &mut Frame,
    state: &SandboxDomainState<'_>,
) -> super::ModalButtons {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let secondary = Style::default().fg(Theme::text_secondary());
    let muted = Style::default().fg(Theme::text_muted());

    let body: Vec<Line> = vec![
        Line::from(vec![
            Span::styled(format!("'{}'", state.session_name), bold),
            Span::raw(" asked for "),
            Span::styled(format!("{}:{}", state.host, state.port), bold),
        ]),
        Line::from(Span::styled(
            format!("Sandbox profile '{}' does not allow it.", state.profile),
            secondary,
        )),
        Line::from(""),
        Line::from(vec![
            Span::raw("Allow adds "),
            Span::styled(state.rule.to_string(), bold),
            Span::raw(", now and next launch:"),
        ]),
        Line::from(Span::styled("that host on that port only.", secondary)),
        Line::from(""),
        Line::from(Span::styled(
            "The name is trusted; TLS is not inspected.",
            muted,
        )),
    ];

    super::render_confirm_modal(
        frame,
        MODAL_WIDTH_PCT,
        "Sandbox — allow domain?",
        false,
        body,
        (
            "Allow",
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    /// 80 columns: the narrowest terminal this has to stay legible in, and the
    /// one where an over-long line would be clipped rather than wrapped.
    fn rendered_text(state: &SandboxDomainState<'_>) -> String {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_sandbox_domain_modal(frame, state);
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    /// Everything the answer depends on has to be on screen: which session, the
    /// host and port, the profile that gains the rule, and the rule itself.
    #[test]
    fn names_the_session_the_host_the_port_and_the_rule() {
        let out = rendered_text(&SandboxDomainState {
            session_name: "api",
            profile: "dev",
            host: "api.github.com",
            port: 443,
            rule: "api.github.com:443",
        });
        assert!(out.contains("'api'"), "{out}");
        assert!(out.contains("api.github.com:443"), "{out}");
        assert!(out.contains("'dev'"), "{out}");
        assert!(out.contains("Allow"), "{out}");
        assert!(out.contains("Cancel"), "{out}");
    }

    /// The grant is narrower than "allow github.com", and the filter is weaker
    /// than "only that host can be reached". Both are stated where the grant is
    /// made, not only in the docs (ADR-27).
    #[test]
    fn states_the_scope_of_the_grant_and_the_limit_of_the_filter() {
        let out = rendered_text(&SandboxDomainState {
            session_name: "api",
            profile: "dev",
            host: "api.github.com",
            port: 443,
            rule: "api.github.com:443",
        });
        assert!(out.contains("that host on that port only."), "{out}");
        assert!(out.contains("TLS is not inspected"), "{out}");
    }
}

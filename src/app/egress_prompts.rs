//! Bookkeeping behind the first-use domain prompt: which of the egress proxy's
//! refusals reach the user, and which are a retry of something already said.
//!
//! The proxy refuses a request *per request*, and an agent that wanted a domain
//! wants it again a moment later — a failed `curl` in a retry loop, an npm
//! install walking a dependency list, a language server reconnecting. Left
//! ungated that is one modal per attempt, arriving faster than anyone can press
//! a key. So a refusal is turned into something the user sees exactly once per
//! `(session, host)`: the first one is reported, and asked about when the
//! profile says to; every later one is silent.
//!
//! Pure state, deliberately free of [`App`](crate::app::App) — the rule that
//! decides "ask, report, or say nothing" is the part worth testing on its own,
//! the same split [`notify_state`](crate::app::notify_state) makes.
//!
//! Both bounds here exist because the far side of them is an *agent*: a sandbox
//! reaching for a thousand generated names must not grow this map without end,
//! and must not be able to queue a thousand modals the user then has to dismiss
//! one by one. Each bound fails towards saying less, never towards asking more.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

/// How long a question ignores every key after it appears.
///
/// Unlike every other confirmation in friring this modal is not opened by a
/// keypress: it is raised from the background tick, because a *sandboxed agent*
/// made a request — while the user's focus is a terminal pane and they are
/// typing into it. Without a guard, the keystroke already on its way when the
/// modal appears answers a question the user has not read, and an agent can
/// provoke that window whenever it likes by timing a request.
///
/// So the question is inert until it has been on screen long enough to have
/// been seen. Long enough to outlast a keystroke in flight and the burst of a
/// fast typist; short enough that answering deliberately never feels blocked.
pub const PROMPT_ARMING: Duration = Duration::from_millis(600);

/// Distinct hosts tracked per session before friring stops asking about new
/// ones. Reaching it is reported once and then ignored: past this many refused
/// names the agent is not asking for a domain, it is scanning.
const MAX_TRACKED_HOSTS: usize = 256;

/// Questions that may wait for the single modal slot at once. A queue longer
/// than this is worse for the user than a message — they would clear it one
/// keypress at a time — so a burst past it is reported instead of asked.
const MAX_QUEUED_PROMPTS: usize = 8;

/// One "allow this domain?" question, as the modal renders it and the answer
/// handler acts on it.
///
/// Carries the launch key rather than a [`SessionId`](crate::session::SessionId)
/// because that is what the proxy is registered under, and the question can
/// outlive the session it is about — a stale answer must be refused, not
/// applied to whatever now holds that index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainPrompt {
    /// The key the proxy runs under: friring's session id, as a string.
    pub session_key: String,
    /// What to call the session in the question.
    pub session_name: String,
    /// Profile an "allow" is written back to.
    pub profile: String,
    /// The host as the sandbox asked for it. Shown, never stored: it is the
    /// spelling the user has to recognise, and the proxy has already replaced
    /// anything a terminal would act on.
    pub host: String,
    pub port: u16,
    /// The rule an "allow" applies and stores — canonical, and scoped to the
    /// port that was refused, so granting is never wider than the question.
    pub rule: String,
    /// When this question reached the screen, or `None` while it is still
    /// queued. Stamped by the opener, so the [`PROMPT_ARMING`] window measures
    /// time the user could have *read* it rather than time it spent waiting
    /// behind another modal.
    pub shown_at: Option<Instant>,
}

impl DomainPrompt {
    /// Whether this question has been on screen long enough to be answered.
    ///
    /// A question that never reached the screen is never armed: there is no
    /// keypress it could be answering.
    pub fn is_armed(&self) -> bool {
        self.shown_at
            .is_some_and(|shown| shown.elapsed() >= PROMPT_ARMING)
    }
}

/// What a refusal turned out to be worth telling the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observed {
    /// First refusal of this host for this session: report it, and ask if the
    /// profile asked to be asked.
    First,
    /// Queued, on screen, or already answered — the user has seen this host.
    Repeat,
    /// This session has now been refused more distinct hosts than friring
    /// tracks. Returned once, and only once, per session.
    Saturated,
}

/// What friring remembers about one session's refusals.
#[derive(Debug, Default)]
struct SessionRecord {
    /// Hosts already reported: answered, on screen, or waiting to be asked.
    hosts: HashSet<String>,
    /// Whether [`Observed::Saturated`] has been handed out for this session.
    saturated_reported: bool,
}

/// Per-session refusal history plus the queue of questions waiting for the
/// modal slot.
#[derive(Debug, Default)]
pub struct EgressPromptState {
    sessions: HashMap<String, SessionRecord>,
    queue: VecDeque<DomainPrompt>,
}

impl EgressPromptState {
    /// Record that `session_key`'s sandbox was refused `host`, and say what
    /// that is worth.
    ///
    /// `host` is the caller's canonical key, not the spelling the client used:
    /// the same name refused in two spellings is one thing to tell the user
    /// about, and only the caller knows how a host canonicalises. The lowercase
    /// here is the floor under that, not the whole of it.
    pub fn observe(&mut self, session_key: &str, host: &str) -> Observed {
        let record = self.sessions.entry(session_key.to_string()).or_default();
        let host = host.to_ascii_lowercase();
        if record.hosts.contains(&host) {
            return Observed::Repeat;
        }
        if record.hosts.len() >= MAX_TRACKED_HOSTS {
            if record.saturated_reported {
                return Observed::Repeat;
            }
            record.saturated_reported = true;
            return Observed::Saturated;
        }
        record.hosts.insert(host);
        Observed::First
    }

    /// Queue a question, or report that there was no room for it.
    ///
    /// A refusal that does not fit is *not* re-offered later:
    /// [`observe`](Self::observe) already settled its host, so the user has been
    /// told about it and will not be asked twice about the same thing under a
    /// different rule.
    pub fn enqueue(&mut self, prompt: DomainPrompt) -> bool {
        if self.queue.len() >= MAX_QUEUED_PROMPTS {
            return false;
        }
        self.queue.push_back(prompt);
        true
    }

    /// The next question to put in front of the user, oldest first.
    pub fn next_prompt(&mut self) -> Option<DomainPrompt> {
        self.queue.pop_front()
    }

    /// Forget everything about one session.
    ///
    /// Called where the boundary is rebuilt: a relaunch mints a fresh proxy
    /// from a possibly edited profile, so the answers given against the
    /// previous run — a "no" above all — are not the user's standing position
    /// on the new one.
    pub fn forget(&mut self, session_key: &str) {
        self.sessions.remove(session_key);
        self.queue
            .retain(|prompt| prompt.session_key != session_key);
    }

    /// Drop every session not in `live`, so a friring left running for weeks
    /// does not accumulate the history of sessions that are gone.
    pub fn retain_sessions(&mut self, live: &HashSet<String>) {
        self.sessions.retain(|key, _| live.contains(key));
        self.queue
            .retain(|prompt| live.contains(&prompt.session_key));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt(session: &str, host: &str) -> DomainPrompt {
        DomainPrompt {
            session_key: session.to_string(),
            session_name: "api".to_string(),
            profile: "dev".to_string(),
            host: host.to_string(),
            port: 443,
            rule: format!("{host}:443"),
            shown_at: None,
        }
    }

    /// A queued question is not an answerable one: the arming window measures
    /// time on screen, and a question waiting behind another modal has had
    /// none.
    #[test]
    fn a_question_is_inert_until_it_has_been_seen() {
        let mut queued = prompt("s1", "github.com");
        assert!(!queued.is_armed());
        queued.shown_at = Some(Instant::now());
        assert!(!queued.is_armed(), "answerable the instant it appeared");
        queued.shown_at = Instant::now().checked_sub(PROMPT_ARMING);
        assert!(queued.is_armed());
    }

    /// The rule the whole screen rests on: an agent retrying a blocked host
    /// gets one question, not one per attempt.
    #[test]
    fn a_burst_for_one_host_is_a_single_question() {
        let mut state = EgressPromptState::default();
        assert_eq!(state.observe("s1", "github.com"), Observed::First);
        assert!(state.enqueue(prompt("s1", "github.com")));
        for _ in 0..50 {
            assert_eq!(state.observe("s1", "github.com"), Observed::Repeat);
        }
        assert!(state.next_prompt().is_some());
        assert!(state.next_prompt().is_none());
        // And the retries that arrive *after* the question was taken off the
        // queue are still silent — the host is settled, not merely queued.
        assert_eq!(state.observe("s1", "github.com"), Observed::Repeat);
    }

    /// A refusal answered "no" must not come back. Nothing marks the answer:
    /// asking is what settles the host, so a denial is remembered by the same
    /// mechanism that stops the burst.
    #[test]
    fn a_refused_host_never_asks_again() {
        let mut state = EgressPromptState::default();
        state.observe("s1", "tracker.example");
        state.enqueue(prompt("s1", "tracker.example"));
        let asked = state.next_prompt().expect("the question is raised once");
        assert_eq!(asked.host, "tracker.example");
        // The user said no; the agent keeps trying.
        assert_eq!(state.observe("s1", "tracker.example"), Observed::Repeat);
        assert!(state.next_prompt().is_none());
    }

    /// Spelling is presentation. Two cases of one name are one question.
    #[test]
    fn one_host_in_two_spellings_is_one_question() {
        let mut state = EgressPromptState::default();
        assert_eq!(state.observe("s1", "GitHub.com"), Observed::First);
        assert_eq!(state.observe("s1", "github.com"), Observed::Repeat);
    }

    /// Sessions are separate boundaries, so one session's answer must not
    /// silence another's question about the same host.
    #[test]
    fn each_session_is_asked_for_itself() {
        let mut state = EgressPromptState::default();
        assert_eq!(state.observe("s1", "github.com"), Observed::First);
        assert_eq!(state.observe("s2", "github.com"), Observed::First);
    }

    /// A sandbox scanning names cannot grow the map forever, and cannot turn
    /// the status bar into its output stream: the cap is announced once.
    #[test]
    fn a_session_scanning_names_is_capped_and_said_so_once() {
        let mut state = EgressPromptState::default();
        for index in 0..MAX_TRACKED_HOSTS {
            assert_eq!(
                state.observe("s1", &format!("h{index}.example")),
                Observed::First
            );
        }
        assert_eq!(
            state.observe("s1", "one-too-many.example"),
            Observed::Saturated
        );
        assert_eq!(state.observe("s1", "another.example"), Observed::Repeat);
        assert_eq!(state.observe("s1", "and-another.example"), Observed::Repeat);
        // A host from before the cap is still remembered rather than re-asked.
        assert_eq!(state.observe("s1", "h0.example"), Observed::Repeat);
        // The cap is per session: a well-behaved sibling is unaffected.
        assert_eq!(state.observe("s2", "one-too-many.example"), Observed::First);
    }

    /// A burst of *distinct* hosts must not queue a modal the user has to
    /// dismiss dozens of times. Past the cap they are reported, not asked.
    #[test]
    fn the_question_queue_is_bounded() {
        let mut state = EgressPromptState::default();
        for index in 0..MAX_QUEUED_PROMPTS {
            assert!(state.enqueue(prompt("s1", &format!("h{index}.example"))));
        }
        assert!(!state.enqueue(prompt("s1", "overflow.example")));
        // Draining one makes room again.
        assert!(state.next_prompt().is_some());
        assert!(state.enqueue(prompt("s1", "overflow.example")));
    }

    /// A relaunch is a new boundary from a possibly edited profile, so the
    /// previous run's answers are not carried into it.
    #[test]
    fn forgetting_a_session_lets_it_be_asked_again() {
        let mut state = EgressPromptState::default();
        state.observe("s1", "github.com");
        state.enqueue(prompt("s1", "github.com"));
        state.observe("s2", "github.com");
        state.enqueue(prompt("s2", "github.com"));

        state.forget("s1");
        assert_eq!(state.observe("s1", "github.com"), Observed::First);
        // The other session's pending question survives untouched.
        assert_eq!(
            state.next_prompt().map(|p| p.session_key),
            Some("s2".to_string())
        );
    }

    #[test]
    fn pruning_drops_sessions_that_are_gone() {
        let mut state = EgressPromptState::default();
        state.observe("s1", "github.com");
        state.enqueue(prompt("s1", "github.com"));
        state.observe("s2", "github.com");
        state.enqueue(prompt("s2", "github.com"));

        state.retain_sessions(&HashSet::from(["s2".to_string()]));
        assert_eq!(state.observe("s1", "github.com"), Observed::First);
        assert_eq!(state.observe("s2", "github.com"), Observed::Repeat);
        assert_eq!(state.queue.len(), 1);
    }
}

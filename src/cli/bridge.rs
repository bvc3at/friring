//! `friring-cli bridge` — a sandboxed agent's own request queue (ADR-30).
//!
//! The one command a *sandboxed* agent runs to ask friring for something, and
//! like `sandbox relay` and `sandbox launch` it is dispatched **before the
//! database opens**: ADR-29 keeps friring's SQLite file out of every boundary,
//! and this runs inside one.
//!
//! It is a client and nothing more. It writes a JSON file, polls for a JSON
//! file, and prints what it found. Every decision — may this caller do this, to
//! which session, under which grant — is made by the running friring on the
//! other side of the queue, where the authority is.
//!
//! # The channel is the identity
//!
//! Nothing here says who the caller is, and the protocol has no field for it.
//! Authority comes from **which directory the request was written into**:
//! friring minted one bridge directory per session and exposed exactly that one
//! inside that session's boundary, so a request in it is by construction a
//! request from that session. A caller-supplied session id would be a claim, and
//! a claim is not evidence.
//!
//! `FRIRING_BRIDGE_DIR` is how the caller finds its own directory. It is
//! inserted on the sandbox **policy**, which is the launch's last word, so an
//! agent that declares the variable in `agents.toml` cannot point the channel
//! somewhere else — the same rule `FRIRING_SIGNAL_FILE` follows.
//!
//! # Nothing here is a fallback
//!
//! A queue that cannot be found, a friring that never answers, a response that
//! will not parse: each is an error and none is a degraded mode. A client that
//! quietly did nothing and exited zero would let a leader believe it had created
//! a child that does not exist.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::Subcommand;

use crate::session::bridge::{
    CreateBody, EmptyBody, InboxBody, ReportBody, Request, RequestBody, RequestKey, ResumeBody,
    SendBody, StopBody, Verb, BRIDGE_DIR_ENV, BRIDGE_PROTOCOL, MAX_BRIDGE_RESPONSE_BYTES,
};

/// How long a client waits for an answer before giving up, by default.
///
/// Generous, because a `create` is a worktree checkout, a proxy bind and a
/// window spawn on a machine that may be busy. A client that gave up early would
/// leave a child running that its leader believes does not exist.
const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// How often the response directory is polled.
const POLL: Duration = Duration::from_millis(100);

/// What a sandboxed agent is asking friring to do.
///
/// One subcommand per verb, and the set is closed for the reason
/// [`Verb`] is: there is no `exec`, no arbitrary `friring-cli`, no SQL and no
/// way to name another session's anything.
#[derive(Subcommand, Debug, Clone)]
pub enum Action {
    /// Create one child session in a repository this session already works in.
    Create {
        /// Repository root. Must be one this session works in.
        #[arg(long)]
        repo_root: String,
        /// Branch to cut the child's worktree on.
        #[arg(long)]
        branch: String,
        /// Agent to run, from the profile's allowed child agents.
        #[arg(long)]
        agent: String,
        /// What kind of work this is, for the mailbox and the UI.
        #[arg(long, default_value = "task")]
        task_kind: String,
        /// The task itself, delivered as the child's first mail. `-` reads
        /// stdin, which is how a long task body avoids a command line.
        #[arg(long)]
        task_body: String,
        /// A label for the UI. Data, never authority.
        #[arg(long)]
        role_hint: Option<String>,
        /// Child ids that must be `done` first. Repeatable.
        #[arg(long = "depends-on")]
        depends_on: Vec<String>,
        #[command(flatten)]
        common: Common,
    },
    /// Ask a child to finish, then stop and verify it.
    Stop {
        /// The child's session id.
        child: String,
        /// How long the child has to answer before the host stops it anyway.
        #[arg(long)]
        grace_secs: Option<u64>,
        #[command(flatten)]
        common: Common,
    },
    /// Relaunch a dirty, stalled, stopped or unusable child, keeping its
    /// ownership.
    Resume {
        /// The child's session id.
        child: String,
        #[command(flatten)]
        common: Common,
    },
    /// This session's own state, and its children's when it has any.
    Status {
        #[command(flatten)]
        common: Common,
    },
    /// Read this session's own mail.
    Inbox {
        /// Mark what is returned as read. Without this the mail stays unread.
        #[arg(long)]
        claim: bool,
        /// At most this many.
        #[arg(long)]
        limit: Option<usize>,
        #[command(flatten)]
        common: Common,
    },
    /// Send mail to this session's owner, or to a child it owns.
    Send {
        /// A child id this session owns, or `owner`.
        #[arg(long)]
        to: String,
        /// The message kind — `task`, `answer`, `cancel`, `result`, `blocked`
        /// or `report`, whichever the direction allows.
        #[arg(long)]
        kind: String,
        /// The message. `-` reads stdin.
        #[arg(long)]
        body: String,
        #[command(flatten)]
        common: Common,
    },
    /// File a bounded progress report about this session.
    Report {
        /// `planning`, `implementing`, `verifying`, `blocked` or `done`.
        #[arg(long)]
        phase: String,
        /// `0..=100`.
        #[arg(long, default_value_t = 0)]
        progress: u8,
        /// A short note. `-` reads stdin.
        #[arg(long, default_value = "")]
        summary: String,
        /// Raise the attention badge and mail the owner.
        #[arg(long)]
        needs_operator: bool,
        /// A path inside this session's own worktree. Repeatable.
        #[arg(long = "artifact")]
        artifacts: Vec<String>,
        #[command(flatten)]
        common: Common,
    },
}

/// The flags every verb takes.
#[derive(clap::Args, Debug, Clone)]
pub struct Common {
    /// The idempotency key. Omit to mint one.
    ///
    /// Supplying it is what makes a retry safe: the same key with the same body
    /// returns the first attempt's answer instead of doing the work twice, and
    /// the same key with a *different* body is refused outright.
    #[arg(long)]
    pub key: Option<String>,
    /// How long to wait for an answer, in seconds.
    #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECS)]
    pub timeout: u64,
    /// Delete the response file once it has been read.
    #[arg(long)]
    pub ack: bool,
    /// Print a readable summary instead of the raw JSON answer.
    ///
    /// JSON is the default and stays the default: the caller here is almost
    /// always an agent parsing the answer, and a command whose output shape
    /// depended on whether a terminal was attached would break the moment one
    /// was. This is for the operator running the same command by hand.
    #[arg(long)]
    pub human: bool,
    /// Unix millis after which friring must not act on this request.
    #[arg(long)]
    pub deadline: Option<u64>,
}

impl Action {
    /// The flags this verb was given.
    fn common(&self) -> &Common {
        match self {
            Self::Create { common, .. }
            | Self::Stop { common, .. }
            | Self::Resume { common, .. }
            | Self::Status { common }
            | Self::Inbox { common, .. }
            | Self::Send { common, .. }
            | Self::Report { common, .. } => common,
        }
    }

    /// Which verb this is.
    fn verb(&self) -> Verb {
        match self {
            Self::Create { .. } => Verb::Create,
            Self::Stop { .. } => Verb::Stop,
            Self::Resume { .. } => Verb::Resume,
            Self::Status { .. } => Verb::Status,
            Self::Inbox { .. } => Verb::Inbox,
            Self::Send { .. } => Verb::Send,
            Self::Report { .. } => Verb::Report,
        }
    }

    /// The verb's own arguments, with `-` placeholders read from stdin.
    ///
    /// # Errors
    ///
    /// Stdin could not be read.
    fn body(self) -> Result<RequestBody, String> {
        Ok(match self {
            Self::Create {
                repo_root,
                branch,
                agent,
                task_kind,
                task_body,
                role_hint,
                depends_on,
                ..
            } => RequestBody::Create(CreateBody {
                repo_root,
                branch,
                agent,
                task_kind,
                task_body: from_stdin_if_dash(task_body)?,
                role_hint,
                depends_on_children: depends_on,
            }),
            Self::Stop {
                child, grace_secs, ..
            } => RequestBody::Stop(StopBody { child, grace_secs }),
            Self::Resume { child, .. } => RequestBody::Resume(ResumeBody { child }),
            Self::Status { .. } => RequestBody::Empty(EmptyBody {}),
            Self::Inbox { claim, limit, .. } => RequestBody::Inbox(InboxBody { claim, limit }),
            Self::Send { to, kind, body, .. } => RequestBody::Send(SendBody {
                to,
                kind,
                body: from_stdin_if_dash(body)?,
            }),
            Self::Report {
                phase,
                progress,
                summary,
                needs_operator,
                artifacts,
                ..
            } => RequestBody::Report(ReportBody {
                phase,
                progress,
                summary: from_stdin_if_dash(summary)?,
                needs_operator,
                artifact_paths: artifacts,
            }),
        })
    }
}

/// A long value read from stdin rather than from a command line.
///
/// A task body is prose and can be long; argv is world-readable through
/// `/proc/<pid>/cmdline` on Linux, and a shell that has to quote it is a shell
/// that can mangle it. `-` is the usual spelling for "the real value is on
/// stdin".
fn from_stdin_if_dash(value: String) -> Result<String, String> {
    use std::io::Read as _;

    if value != "-" {
        return Ok(value);
    }
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .map_err(|e| format!("cannot read the value from stdin: {e}"))?;
    Ok(text)
}

/// Write one request, wait for its answer, print it.
///
/// # Errors
///
/// The queue could not be found or written, the request key is not one friring
/// accepts, no answer arrived before the timeout, or the answer could not be
/// read. A refusal from friring is **not** an error here: it is a well-formed
/// answer, printed as one, and the exit status says which.
pub fn run(action: Action) -> Result<(), String> {
    let dir = queue_dir()?;
    let common = action.common().clone();
    let key = match &common.key {
        Some(supplied) => RequestKey::new(supplied.clone())?,
        None => mint_key(),
    };
    let request = Request {
        protocol: BRIDGE_PROTOCOL,
        key: key.clone(),
        verb: action.verb(),
        deadline: common.deadline,
        body: action.body()?,
    };
    let encoded = serde_json::to_string(&request)
        .map_err(|e| format!("cannot encode the bridge request: {e}"))?;
    submit(&dir, key.as_str(), &encoded)?;
    let answer = await_response(&dir, key.as_str(), Duration::from_secs(common.timeout))?;
    if common.human {
        println!("{}", render_human(&answer));
    } else {
        println!("{answer}");
    }
    if common.ack {
        let _ = std::fs::remove_file(response_path(&dir, key.as_str()));
    }
    // A refusal is an answer, not a client failure — but a caller scripting
    // this needs the status to say so, or a shell `&&` chain would carry on
    // past a `create` that created nothing.
    if refused(&answer) {
        return Err(String::new());
    }
    Ok(())
}

/// One answer, rendered for a person.
///
/// Deliberately thin: it names the outcome, the refusal code when there is one,
/// and the fields of a `status` an operator actually reads. Anything it cannot
/// recognise is printed as the JSON it was, because a summary that silently
/// dropped part of an answer would be worse than the answer.
///
/// Child-authored text is labelled where it appears, as it is everywhere else.
fn render_human(answer: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(answer) else {
        return answer.to_string();
    };
    if value.get("ok").and_then(serde_json::Value::as_bool) == Some(false) {
        return format!(
            "refused ({}): {}",
            value["error"].as_str().unwrap_or("failed"),
            value["message"].as_str().unwrap_or("no reason given")
        );
    }
    let Some(data) = value.get("data") else {
        return "ok".to_string();
    };
    let Some(children) = data.get("children").and_then(serde_json::Value::as_array) else {
        return format!("ok: {data}");
    };
    let mut out = String::from("ok");
    if let Some(session) = data.get("session") {
        out.push_str(&format!(
            "\n  self: {} ({} unread)",
            session["state"].as_str().unwrap_or("-"),
            session["unread"]
        ));
    }
    for child in children {
        out.push_str(&format!(
            "\n  {} {} {}",
            child["state"].as_str().unwrap_or("-"),
            child["name"].as_str().unwrap_or("-"),
            child["id"].as_str().unwrap_or("-"),
        ));
        if let Some(result) = child.get("result").filter(|r| !r.is_null()) {
            out.push_str(&format!(
                "\n      verified: {} branch={} head={} dirty={} ahead={}",
                result["outcome"].as_str().unwrap_or("-"),
                result["branch"].as_str().unwrap_or("-"),
                result["head"].as_str().unwrap_or("-"),
                result["dirty"],
                result["ahead_of_base"],
            ));
        }
        if let Some(report) = child.get("last_report").filter(|r| !r.is_null()) {
            out.push_str(&format!(
                "\n      child-authored report [{}]: {}",
                report["phase"].as_str().unwrap_or("-"),
                report["summary"].as_str().unwrap_or(""),
            ));
        }
    }
    out
}

/// Whether a well-formed answer is a refusal.
///
/// Read from the `ok` field, never from the presence of `error`: the two are
/// exclusive by construction on the writing side, and a reader that consulted
/// the other one would disagree with friring about the same answer.
fn refused(answer: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(answer)
        .ok()
        .and_then(|value| value.get("ok").and_then(serde_json::Value::as_bool))
        .is_some_and(|ok| !ok)
}

/// This session's own bridge directory, from the environment friring set.
///
/// # Errors
///
/// The variable is unset — this process is not in a boundary friring granted
/// the bridge, and there is no other way to find a queue. Guessing one would be
/// guessing at an authority.
fn queue_dir() -> Result<PathBuf, String> {
    let raw = std::env::var(BRIDGE_DIR_ENV).map_err(|_| {
        format!(
            "{BRIDGE_DIR_ENV} is not set, so this process has no bridge queue. The bridge is \
             granted per session by a sandbox profile; a session whose profile grants none has \
             no channel and no fallback"
        )
    })?;
    if raw.trim().is_empty() {
        return Err(format!("{BRIDGE_DIR_ENV} is set but empty"));
    }
    Ok(PathBuf::from(raw))
}

/// A fresh idempotency key, in the format [`RequestKey`] accepts.
fn mint_key() -> RequestKey {
    let raw: String = uuid::Uuid::new_v4()
        .to_string()
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-')
        .take(RequestKey::MAX_LEN)
        .collect();
    RequestKey::new(raw).expect("a uuid is a valid request key")
}

/// Write the request, staged and renamed.
///
/// The rename is what makes it appear whole: friring reads `.req` files, and a
/// partially written one would be parsed as a malformed request rather than as
/// one that is still being written.
fn submit(dir: &Path, key: &str, body: &str) -> Result<(), String> {
    let requests = dir.join("req");
    std::fs::create_dir_all(&requests).map_err(|e| {
        format!(
            "cannot open the bridge queue at {}: {e}",
            requests.display()
        )
    })?;
    let staged = requests.join(format!("{key}.tmp"));
    let final_path = requests.join(format!("{key}.req"));
    std::fs::write(&staged, body)
        .map_err(|e| format!("cannot write the request to {}: {e}", staged.display()))?;
    std::fs::rename(&staged, &final_path)
        .map_err(|e| format!("cannot submit the request as {}: {e}", final_path.display()))
}

/// Where the answer to `key` appears.
fn response_path(dir: &Path, key: &str) -> PathBuf {
    dir.join("res").join(format!("{key}.res"))
}

/// Poll for the answer.
///
/// # Errors
///
/// Nothing arrived before the timeout — which a caller must treat as "friring
/// may or may not have acted", and which the idempotency key exists to make
/// retryable.
fn await_response(dir: &Path, key: &str, timeout: Duration) -> Result<String, String> {
    use std::io::Read as _;

    let path = response_path(dir, key);
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(file) = std::fs::File::open(&path) {
            let mut text = String::new();
            file.take(MAX_BRIDGE_RESPONSE_BYTES)
                .read_to_string(&mut text)
                .map_err(|e| format!("cannot read the response at {}: {e}", path.display()))?;
            if !text.trim().is_empty() {
                return Ok(text);
            }
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "no answer to bridge request '{key}' within {}s. friring may still act on it — \
                 retry with the same --key, which returns the first attempt's answer rather than \
                 doing the work twice",
                timeout.as_secs()
            ));
        }
        std::thread::sleep(POLL);
    }
}

/// What this friring's bridge offers, as JSON.
///
/// The document behind `friring-cli capabilities`, and the one an extension's
/// `binary-capability` requirement is checked against. Built in
/// [`crate::session::bridge`] from the constants the broker itself uses — so a
/// version this prints is a version it speaks — and re-exported here because
/// `session_ops` reads the same document and may not reach `cli`.
pub fn capabilities_json() -> serde_json::Value {
    crate::session::bridge::capabilities_json()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key names the file the request arrives in, so it is validated before
    /// any path is joined — and a minted one is always valid.
    #[test]
    fn a_minted_key_is_one_path_segment() {
        let key = mint_key();
        assert!(key.as_str().len() >= RequestKey::MIN_LEN);
        assert!(!key.as_str().contains('/'));
        assert!(!key.as_str().contains('.'));
        assert!(RequestKey::new(key.as_str()).is_ok());
    }

    /// A caller-supplied key that would be a path is refused before it becomes
    /// one.
    #[test]
    fn a_key_that_would_be_a_path_is_refused() {
        for hostile in [
            "../../etc/passwd",
            "a/b",
            "..",
            "UPPER-CASE",
            "short",
            &"x".repeat(RequestKey::MAX_LEN + 1),
        ] {
            assert!(RequestKey::new(hostile).is_err(), "{hostile:?}");
        }
        assert!(RequestKey::new("abc-1234").is_ok());
    }

    /// Without the variable there is no queue and no way to guess one: a client
    /// that fell back to a path would be guessing at an authority.
    #[test]
    fn no_bridge_directory_is_an_error_and_never_a_fallback() {
        // Asserted on the function rather than through `run`, which would wait
        // on a queue.
        let saved = std::env::var(BRIDGE_DIR_ENV).ok();
        std::env::remove_var(BRIDGE_DIR_ENV);
        let error = queue_dir().unwrap_err();
        assert!(error.contains(BRIDGE_DIR_ENV), "{error}");
        assert!(error.contains("no fallback"), "{error}");
        if let Some(saved) = saved {
            std::env::set_var(BRIDGE_DIR_ENV, saved);
        }
    }

    /// A refusal is a well-formed answer, and the exit status has to say so or
    /// a shell chain would carry on past a `create` that created nothing.
    #[test]
    fn a_refusal_is_read_from_the_ok_field() {
        assert!(refused(r#"{"ok":false,"error":"not_owner"}"#));
        assert!(!refused(r#"{"ok":true,"data":{}}"#));
        // Neither field present: not a refusal this client can claim.
        assert!(!refused("{}"));
        assert!(!refused("not json"));
    }

    /// A refusal reads as a refusal, and a `status` names each child's
    /// host-verified verdict — with the child's own words labelled as its own.
    #[test]
    fn the_human_rendering_labels_what_a_child_wrote() {
        let refused = render_human(r#"{"ok":false,"error":"not_owner","message":"not yours"}"#);
        assert!(refused.starts_with("refused (not_owner)"), "{refused}");

        let ok = render_human(
            r#"{"ok":true,"data":{"session":{"state":"ready","unread":2},"children":[
                {"id":"c1","name":"lead-abc","state":"dirty",
                 "result":{"outcome":"completed","branch":"feat/x","head":"abc","dirty":true,
                           "ahead_of_base":2},
                 "last_report":{"phase":"blocked","summary":"needs a decision"}}]}}"#,
        );
        assert!(ok.contains("dirty lead-abc c1"), "{ok}");
        assert!(ok.contains("verified: completed"), "{ok}");
        assert!(ok.contains("child-authored report"), "{ok}");
        // The host's verdict and the child's words are never run together.
        assert!(ok.contains("needs a decision"), "{ok}");
    }

    /// An answer this cannot recognise is printed whole rather than summarised
    /// into something shorter and less true.
    #[test]
    fn an_unrecognised_answer_is_printed_as_it_arrived() {
        assert_eq!(render_human("not json"), "not json");
        assert_eq!(render_human(r#"{"ok":true}"#), "ok");
    }

    /// The capability document is built from the same constants the broker
    /// uses, so a version this prints is a version it speaks.
    #[test]
    fn the_capability_document_names_every_verb() {
        let doc = capabilities_json();
        assert_eq!(doc["bridge"]["protocol"], BRIDGE_PROTOCOL);
        let verbs = doc["bridge"]["verbs"].as_array().unwrap();
        assert_eq!(verbs.len(), Verb::ALL.len());
        for verb in Verb::ALL {
            assert!(
                verbs.iter().any(|v| v == verb.as_str()),
                "{verb} is missing"
            );
        }
        let caps = doc["bridge"]["capabilities"].as_array().unwrap();
        assert_eq!(caps.len(), crate::session::BridgeCapability::ALL.len());
    }
}

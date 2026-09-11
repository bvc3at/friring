//! The orchestration bridge's pure data: the states a child moves through, and
//! (from Stage D) the wire protocol a sandboxed agent speaks.
//!
//! Everything here is data with no side effects and no crate-internal
//! references, because `session` is the dependency sink
//! (`tests/architecture_rules.rs`). The broker that acts on these types lives in
//! `app::bridge`; the rows live in `storage::bridge`; the client that writes
//! them from inside a boundary lives in `cli::bridge`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

// ── Wire protocol ────────────────────────────────────────────────────────

/// The protocol version a client and this friring must agree on.
///
/// Bumped only for a change that is not backwards compatible; a new verb or a
/// new optional field is not one. `friring-cli capabilities` prints it, and an
/// extension declares the version it needs with a `binary-capability`
/// requirement, so a mismatch is a refused install rather than a request
/// friring answers wrongly at runtime.
pub const BRIDGE_PROTOCOL: u32 = 1;

/// The largest request friring will read.
///
/// A request is a small JSON object; anything larger is not one, and the file it
/// arrives in is written by a process inside a boundary. Bounded before the
/// parse, not by it.
pub const MAX_BRIDGE_REQUEST_BYTES: u64 = 64 * 1024;

/// The largest response friring will write.
///
/// A `status` over a full fan-out with reports attached is the big one; four
/// times the request cap covers it with room to spare.
pub const MAX_BRIDGE_RESPONSE_BYTES: u64 = 256 * 1024;

/// The largest body a mail message may carry.
pub const MAX_MAIL_BODY_BYTES: usize = 64 * 1024;

/// The largest summary a `result` or a `report` may carry.
pub const MAX_SUMMARY_BYTES: usize = 4 * 1024;

/// How many unread messages a **bridge child** may be holding before its inbox
/// refuses more.
///
/// Tighter than the generic
/// [`MAX_UNREAD_PER_RECIPIENT`](crate::storage::messages::MAX_UNREAD_PER_RECIPIENT)
/// because both sides of a child's mailbox are agent-controlled: its owner
/// chooses how much to send and the child chooses when to drain. At
/// [`MAX_MAIL_BODY_BYTES`] this bounds one child's undrained mail at ~3 MiB
/// rather than ~32.
///
/// A refusal is [`ErrorCode::Quota`] on the sender's own `send`, so it is
/// backpressure the owner can act on rather than a message quietly lost.
pub const MAX_UNREAD_PER_CHILD: usize = 50;

/// How many unread messages a **bridge owner** may be holding before its inbox
/// refuses more.
///
/// Four times [`MAX_UNREAD_PER_CHILD`] because an owner is the recipient of
/// every one of its children plus friring's own host mail, and a leader driving
/// a fan-out legitimately accumulates a deeper backlog than any single worker.
pub const MAX_UNREAD_PER_OWNER: usize = 200;

/// How many unanswered requests friring reads out of one directory before it
/// stops enumerating.
///
/// A client that writes faster than the broker answers is either broken or
/// hostile, and either way the queue is not the place to absorb it. The bound is
/// on the **enumeration**: the directory is agent-writable and the take runs on
/// the render loop, so listing it without limit is the starvation the budget
/// exists to prevent. [`MAX_QUEUE_SCAN`] is the other half — this one bounds the
/// requests, that one bounds the entries, and a client controls both counts.
///
/// The excess is left where it is — not taken, not answered, and not removed:
/// friring has read no request to answer, so it warns the operator instead and
/// reads the rest once capacity frees up.
pub const MAX_UNANSWERED_REQUESTS: usize = 16;

/// How many directory entries friring reads before it stops looking, whatever
/// they are named.
///
/// [`MAX_UNANSWERED_REQUESTS`] bounds the *requests*; this bounds the **scan**,
/// and the two are different numbers because the directory is agent-writable and
/// a name that is not a request still costs a `readdir` entry to skip. Without
/// it, a client that filled its own queue with files ending in anything else
/// would make friring enumerate the lot on every poll — on the render loop.
///
/// Comfortably above the quota, so an ordinary queue is never truncated by it,
/// and small enough that the worst case is a handful of skipped entries rather
/// than a directory listing.
pub const MAX_QUEUE_SCAN: usize = 256;

/// Environment variable naming a sandboxed agent's own bridge directory.
///
/// Inserted on the **policy**, so an agent that declares it in `agents.toml`
/// cannot point the channel somewhere friring does not read — the same rule
/// [`crate::paths::SIGNAL_FILE_ENV`] follows.
pub const BRIDGE_DIR_ENV: &str = "FRIRING_BRIDGE_DIR";

/// What a caller is asking friring to do (ADR-30).
///
/// A **closed** set of fixed verbs, and that is the whole security model of the
/// channel. There is no `exec`, no arbitrary `friring-cli`, no SQL, no pane
/// capture and no way to name another session's anything: every verb acts on the
/// caller's own session or on a child it provably owns, and a verb friring does
/// not know is a refusal rather than a pass-through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verb {
    /// Create one child session. Owner only, and only under
    /// [`BridgeCapability::ChildLifecycle`](crate::session::BridgeCapability::ChildLifecycle).
    Create,
    /// Ask a child to finish, then quiesce it. Owner only.
    Stop,
    /// Relaunch a dirty, stalled, stopped or unusable child, keeping its
    /// ownership.
    /// Owner only.
    Resume,
    /// The caller's own state, and its children's when it has any.
    Status,
    /// Claim the caller's **own** mail. Never another session's.
    Inbox,
    /// Send mail to the caller's owner, or to a child it owns.
    Send,
    /// File a bounded progress report about the caller itself.
    Report,
}

impl Verb {
    /// Every verb, in the order `capabilities` prints them.
    pub const ALL: &'static [Self] = &[
        Self::Create,
        Self::Stop,
        Self::Resume,
        Self::Status,
        Self::Inbox,
        Self::Send,
        Self::Report,
    ];

    /// Wire value, storage value and log label.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Stop => "stop",
            Self::Resume => "resume",
            Self::Status => "status",
            Self::Inbox => "inbox",
            Self::Send => "send",
            Self::Report => "report",
        }
    }

    /// Which capability a caller must hold to use this verb.
    pub fn requires(self) -> crate::session::BridgeCapability {
        use crate::session::BridgeCapability as Cap;
        match self {
            Self::Create | Self::Stop | Self::Resume => Cap::ChildLifecycle,
            // `status` is `mailbox`, not `child-lifecycle`: a child holds
            // `mailbox` and has to be able to ask about *itself*. An owner's
            // `status` widens to its children because it holds the other
            // capability too, never because the verb does.
            Self::Status | Self::Inbox | Self::Send => Cap::Mailbox,
            Self::Report => Cap::Report,
        }
    }
}

impl fmt::Display for Verb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Verb {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let key = s.trim().to_ascii_lowercase();
        Self::ALL
            .iter()
            .copied()
            .find(|verb| verb.as_str() == key)
            .ok_or_else(|| {
                format!(
                    "Unknown bridge verb '{s}' (expected {})",
                    Self::ALL
                        .iter()
                        .map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
    }
}

/// One request's idempotency key.
///
/// `[a-z0-9-]{8,64}`, validated **before** any path is joined: the key names the
/// file the request arrives in, and a value carrying a separator or a `..` would
/// be a path rather than a name. The length floor is not cosmetic either — the
/// key is what makes a replay find the first attempt's answer, and a two-character
/// key collides by accident.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RequestKey(String);

impl RequestKey {
    /// The shortest key friring accepts.
    pub const MIN_LEN: usize = 8;
    /// The longest.
    pub const MAX_LEN: usize = 64;

    /// Validate a key, or say exactly what is wrong with it.
    ///
    /// # Errors
    ///
    /// Too short, too long, or carrying a character outside `[a-z0-9-]`.
    pub fn new(raw: impl Into<String>) -> Result<Self, String> {
        let raw = raw.into();
        if raw.len() < Self::MIN_LEN || raw.len() > Self::MAX_LEN {
            return Err(format!(
                "a bridge request key is {}-{} characters; '{raw}' is {}",
                Self::MIN_LEN,
                Self::MAX_LEN,
                raw.len()
            ));
        }
        if let Some(bad) = raw
            .chars()
            .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-'))
        {
            return Err(format!(
                "a bridge request key is [a-z0-9-] only; '{raw}' carries '{bad}'"
            ));
        }
        Ok(Self(raw))
    }

    /// The key as a string, safe to use as one path segment.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RequestKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for RequestKey {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl TryFrom<String> for RequestKey {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<RequestKey> for String {
    fn from(key: RequestKey) -> Self {
        key.0
    }
}

/// One request, as it arrives in the queue.
///
/// `deny_unknown_fields` on every struct here, and that is deliberate: a field
/// friring does not recognise is either a typo that drops part of what was asked
/// for or a knob a newer friring understands and this one cannot honour. Both
/// read as "this request does not mean here what it means where it was written",
/// and answering it anyway would be answering a different request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Which protocol the client speaks. A mismatch is refused rather than
    /// interpreted.
    pub protocol: u32,
    pub key: RequestKey,
    pub verb: Verb,
    /// Unix millis after which friring will not act on this request. Absent
    /// means no deadline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<u64>,
    pub body: RequestBody,
}

impl Request {
    /// The body, re-read as the type this request's **verb** names.
    ///
    /// [`RequestBody`] is untagged, and two verbs' bodies can be
    /// indistinguishable as JSON: `{"child": "c1"}` is a well-formed `stop`
    /// (whose `grace_secs` is optional) *and* a well-formed `resume`. Serde
    /// resolves an untagged enum by **first match**, so without this the variant
    /// order in the enum would decide what a request meant and the `verb` field
    /// would be a comment.
    ///
    /// The verb is the authority — it is what the journal records and what
    /// authorization is checked against — so the broker re-reads the body
    /// against it rather than trusting which arm serde happened to pick.
    ///
    /// # Errors
    ///
    /// The body is not the shape this verb takes, with the field that is wrong.
    pub fn body_as<T: serde::de::DeserializeOwned>(&self) -> Result<T, String> {
        let value = serde_json::to_value(&self.body)
            .map_err(|e| format!("this request's body could not be re-read: {e}"))?;
        serde_json::from_value(value).map_err(|e| {
            format!(
                "the body of this request is not a '{}' body: {e}",
                self.verb
            )
        })
    }
}

/// The verb-specific half of a request.
///
/// Untagged is deliberate: the `verb` field already says which shape this is, so
/// a tag here would be a second place for the two to disagree. What that costs
/// is that two verbs whose bodies share a shape parse to whichever variant comes
/// first, which is why the broker reads a body through [`Request::body_as`] —
/// against the verb — rather than matching on the arm serde picked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestBody {
    Create(CreateBody),
    Stop(StopBody),
    Resume(ResumeBody),
    Inbox(InboxBody),
    Send(SendBody),
    Report(ReportBody),
    /// `status` and a bodyless verb.
    Empty(EmptyBody),
}

/// The body of a verb that takes no arguments.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmptyBody {}

/// `create` — one child, in a repository the owner already works in.
///
/// Nothing here can widen a boundary: the repository is checked against
/// `session_repos`, the agent against the profile's `child_agents`, and the
/// child's policy is the owner's narrowed. What the request chooses is *which*
/// of the things already permitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateBody {
    /// Must equal a `session_repos.repo_root` of the owner.
    pub repo_root: String,
    /// The branch the child's worktree is cut on.
    pub branch: String,
    /// A registry name the profile's `child_agents` lists.
    pub agent: String,
    /// What kind of work this is, for the mailbox and the UI. Free text,
    /// bounded, and never interpolated into anything.
    pub task_kind: String,
    /// The task itself, delivered as the child's first mail.
    pub task_body: String,
    /// A label for the UI. Data, never authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_hint: Option<String>,
    /// Child ids that must be `done` before this one is created.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on_children: Vec<String>,
}

/// `stop` — ask a child to finish, then quiesce it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StopBody {
    pub child: String,
    /// How long the child has to send a finish intent before the host stops it
    /// anyway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grace_secs: Option<u64>,
}

/// `resume` — relaunch a child, keeping its ownership and its worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeBody {
    pub child: String,
}

/// `inbox` — claim the caller's own mail.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InboxBody {
    /// Mark what is returned as read. `false` peeks.
    #[serde(default)]
    pub claim: bool,
    /// At most this many, capped at [`MAX_INBOX_LIMIT`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

/// The most mail one `inbox` call returns.
pub const MAX_INBOX_LIMIT: usize = 20;

/// `send` — mail to the caller's owner, or to a child it owns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendBody {
    /// A child id the caller owns, or `owner` for its own owner. Never an
    /// arbitrary session: the broker resolves this against the ownership rows
    /// and refuses anything else.
    pub to: String,
    /// One of the kinds the direction table permits — see
    /// [`MailKind`].
    pub kind: String,
    /// The message. Bounded at [`MAX_MAIL_BODY_BYTES`], and data throughout: it
    /// never reaches a command, a query, a path, a nudge or an OS notification.
    pub body: String,
}

/// `report` — a bounded progress note about the caller itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportBody {
    /// One of [`REPORT_PHASES`].
    pub phase: String,
    /// `0..=100`.
    #[serde(default)]
    pub progress: u8,
    /// Bounded at [`MAX_SUMMARY_BYTES`], control characters stripped.
    #[serde(default)]
    pub summary: String,
    /// Raise the attention badge and mail the owner.
    #[serde(default)]
    pub needs_operator: bool,
    /// Relative paths **inside the caller's own worktree**.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifact_paths: Vec<String>,
}

/// The phases a report may claim. Closed, because the UI groups by them.
pub const REPORT_PHASES: &[&str] = &["planning", "implementing", "verifying", "blocked", "done"];

/// The typed body of a `result` mail — the one **finish intent** (ADR-32).
///
/// A `result` is not a terminal state and there is no other finish kind: the
/// host accepts the intent, stops the pane, inspects the worktree, and *then*
/// decides. A `result` whose body is not this shape is refused and starts no
/// quiesce, because "the child asked to finish" is the one message that must not
/// be inferred from free text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultBody {
    pub outcome: Outcome,
    /// Bounded at [`MAX_SUMMARY_BYTES`]. Child-authored, labelled as such
    /// wherever it is shown, and never in a notification.
    #[serde(default)]
    pub summary: String,
}

/// What a mail message is, and which direction it may travel (ADR-30).
///
/// The direction is enforced by the broker, not by convention: a child that
/// could send `task` would be assigning work to its owner, and one that could
/// send `child.done` would be reporting a verdict only the host may reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MailKind {
    /// owner → child: work to do.
    Task,
    /// owner → child: an answer to a `blocked`.
    Answer,
    /// owner → child, or host → child: stop asked for.
    Cancel,
    /// child → owner: the one finish intent, carrying a [`ResultBody`].
    Result,
    /// child → owner: a question it cannot answer itself.
    Blocked,
    /// child → owner: a mirrored progress report.
    Report,
    /// host → child: a finish intent was accepted.
    Ack,
    /// host → owner: the child is running and proved its private state.
    ChildReady,
    /// host → owner: repeated nudges with no bridge call in return.
    ChildStalled,
    /// host → owner: the quiesce found uncommitted work.
    ChildDirty,
    /// host → owner: verified complete.
    ChildDone,
    /// host → owner: verified failed.
    ChildFailed,
    /// host → owner: friring could not finish the quiesce — either the child's
    /// pane refused to die, or its pane died and the verdict could not be
    /// recorded. Both leave a child nothing may integrate and an operator must
    /// look at.
    ChildStopFailed,
    /// host → owner: a committed child with no live pane was relaunched.
    ChildRelaunched,
    /// host → owner: an operator deleted the child.
    ChildRemovedByOperator,
}

/// Who may send a kind of mail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailDirection {
    /// An owner, to a child it owns.
    OwnerToChild,
    /// A child, to its owner.
    ChildToOwner,
    /// friring itself, to a child.
    HostToChild,
    /// friring itself, to an owner.
    HostToOwner,
}

impl MailKind {
    /// Every kind, in direction order.
    pub const ALL: &'static [Self] = &[
        Self::Task,
        Self::Answer,
        Self::Cancel,
        Self::Result,
        Self::Blocked,
        Self::Report,
        Self::Ack,
        Self::ChildReady,
        Self::ChildStalled,
        Self::ChildDirty,
        Self::ChildDone,
        Self::ChildFailed,
        Self::ChildStopFailed,
        Self::ChildRelaunched,
        Self::ChildRemovedByOperator,
    ];

    /// Wire value, storage value and UI label.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Task => "task",
            Self::Answer => "answer",
            Self::Cancel => "cancel",
            Self::Result => "result",
            Self::Blocked => "blocked",
            Self::Report => "report",
            Self::Ack => "ack",
            Self::ChildReady => "child.ready",
            Self::ChildStalled => "child.stalled",
            Self::ChildDirty => "child.dirty",
            Self::ChildDone => "child.done",
            Self::ChildFailed => "child.failed",
            Self::ChildStopFailed => "child.stop_failed",
            Self::ChildRelaunched => "child.relaunched",
            Self::ChildRemovedByOperator => "child.removed_by_operator",
        }
    }

    /// Which direction this kind may travel.
    pub fn direction(self) -> MailDirection {
        match self {
            Self::Task | Self::Answer => MailDirection::OwnerToChild,
            // `cancel` is the one kind two senders share: an owner asks for a
            // stop, and the host repeats it when it accepts one. Both are the
            // same message to the same recipient.
            Self::Cancel => MailDirection::OwnerToChild,
            Self::Result | Self::Blocked | Self::Report => MailDirection::ChildToOwner,
            Self::Ack => MailDirection::HostToChild,
            _ => MailDirection::HostToOwner,
        }
    }

    /// Whether a **caller** may send this at all, as opposed to friring itself.
    ///
    /// The host kinds are the ones that carry a verdict — `child.done` is what
    /// an integration step reads — so a caller that could forge one could claim
    /// its own work verified.
    pub fn is_sendable_by_caller(self) -> bool {
        matches!(
            self.direction(),
            MailDirection::OwnerToChild | MailDirection::ChildToOwner
        )
    }
}

impl fmt::Display for MailKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for MailKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let key = s.trim().to_ascii_lowercase();
        Self::ALL
            .iter()
            .copied()
            .find(|kind| kind.as_str() == key)
            .ok_or_else(|| format!("Unknown mail kind '{s}'"))
    }
}

/// Why friring refused (ADR-30).
///
/// A **closed** set, because a caller has to be able to tell "you may not" from
/// "not yet" from "that does not exist" without parsing prose. The human
/// sentence rides alongside and is supplemental.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The request's deadline had passed.
    Expired,
    /// This key was used for a different request. Nothing was done.
    KeyReused,
    /// No such child.
    UnknownChild,
    /// That child is not the caller's.
    NotOwner,
    /// The profile does not grant the capability this verb needs.
    GrantMissing,
    /// The owner already has as many live children as its profile allows.
    FanoutExhausted,
    /// A child may not create children.
    DepthExceeded,
    /// The owner does not work in that repository.
    RepoNotOwned,
    /// The profile does not list that agent as a child agent.
    AgentNotAllowed,
    /// The child's narrowed policy would not be narrower than its owner's.
    OverlayViolation,
    /// The child's private agent state could not be proven — see ADR-31.
    StateUnrelocatable,
    /// A `depends_on_children` entry has not finished.
    DependencyUnfinished,
    /// The child's own hook never reported, so friring cannot prove it is
    /// running from its private state.
    NotReady,
    /// The persisted egress endpoint could not be rebound.
    EgressNotRestorable,
    /// The multiplexer server or pane is not the one friring recorded.
    IdentityMismatch,
    /// The child's pane could not be stopped, so nothing was verified.
    QuiesceFailed,
    /// A bound was reached: unanswered requests, unread mail, body size.
    Quota,
    /// No running friring is serving this session's bridge.
    BrokerAbsent,
    /// Everything else, with the sentence saying which.
    Failed,
}

impl ErrorCode {
    /// Every code.
    pub const ALL: &'static [Self] = &[
        Self::Expired,
        Self::KeyReused,
        Self::UnknownChild,
        Self::NotOwner,
        Self::GrantMissing,
        Self::FanoutExhausted,
        Self::DepthExceeded,
        Self::RepoNotOwned,
        Self::AgentNotAllowed,
        Self::OverlayViolation,
        Self::StateUnrelocatable,
        Self::DependencyUnfinished,
        Self::NotReady,
        Self::EgressNotRestorable,
        Self::IdentityMismatch,
        Self::QuiesceFailed,
        Self::Quota,
        Self::BrokerAbsent,
        Self::Failed,
    ];

    /// Wire value and log label.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Expired => "expired",
            Self::KeyReused => "key_reused",
            Self::UnknownChild => "unknown_child",
            Self::NotOwner => "not_owner",
            Self::GrantMissing => "grant_missing",
            Self::FanoutExhausted => "fanout_exhausted",
            Self::DepthExceeded => "depth_exceeded",
            Self::RepoNotOwned => "repo_not_owned",
            Self::AgentNotAllowed => "agent_not_allowed",
            Self::OverlayViolation => "overlay_violation",
            Self::StateUnrelocatable => "state_unrelocatable",
            Self::DependencyUnfinished => "dependency_unfinished",
            Self::NotReady => "not_ready",
            Self::EgressNotRestorable => "egress_not_restorable",
            Self::IdentityMismatch => "identity_mismatch",
            Self::QuiesceFailed => "quiesce_failed",
            Self::Quota => "quota",
            Self::BrokerAbsent => "broker_absent",
            Self::Failed => "failed",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What friring answers with.
///
/// `ok` and `error` are exclusive by construction — [`Response::ok`] and
/// [`Response::refused`] are the only ways to build one — so a client that
/// checks `ok` and a client that checks `error` cannot reach different
/// conclusions about the same answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub protocol: u32,
    pub key: RequestKey,
    pub ok: bool,
    /// The refusal's machine-readable code. `None` exactly when `ok`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorCode>,
    /// The refusal's human sentence. Supplemental to `error`, never the
    /// authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The verb's own answer. `null` for a refusal and for a verb with nothing
    /// to say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl Response {
    /// A successful answer.
    pub fn ok(key: RequestKey, data: Option<serde_json::Value>) -> Self {
        Self {
            protocol: BRIDGE_PROTOCOL,
            key,
            ok: true,
            error: None,
            message: None,
            data,
        }
    }

    /// A refusal, with its code and its sentence.
    pub fn refused(key: RequestKey, error: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            protocol: BRIDGE_PROTOCOL,
            key,
            ok: false,
            error: Some(error),
            message: Some(message.into()),
            data: None,
        }
    }
}

/// What this binary's bridge offers, as JSON.
///
/// Built **from the constants this module defines**, so a version friring prints
/// is a version it actually speaks. Two readers, which is why it lives here in
/// the pure-data layer rather than beside either: `friring-cli capabilities`
/// prints it for an extension author, and the extension installer checks a
/// `binary-capability` requirement against it — and `session_ops` may not reach
/// `cli` (`tests/architecture_rules.rs`).
pub fn capabilities_json() -> serde_json::Value {
    use crate::session::BridgeCapability;

    serde_json::json!({
        "bridge": {
            "protocol": BRIDGE_PROTOCOL,
            "capabilities": BridgeCapability::ALL
                .iter()
                .map(|c| c.as_str())
                .collect::<Vec<_>>(),
            "verbs": Verb::ALL.iter().map(|v| v.as_str()).collect::<Vec<_>>(),
            "errors": ErrorCode::ALL.iter().map(|e| e.as_str()).collect::<Vec<_>>(),
        },
        "extension_requires": crate::session::extension_def::REQUIREMENTS_VERSION,
    })
}

/// The version this binary reports for one capability name, or `None` for a
/// name it does not have.
///
/// The one lookup an extension's `binary-capability` requirement is answered
/// from. `None` is a **refusal** at the call site rather than a pass: a name
/// this friring does not know is one an extension meant something by.
pub fn binary_capability(name: &str) -> Option<u32> {
    let doc = capabilities_json();
    match name {
        "bridge" => doc["bridge"]["protocol"].as_u64(),
        "extension_requires" => doc["extension_requires"].as_u64(),
        _ => None,
    }
    .and_then(|v| u32::try_from(v).ok())
}

/// Where a bridge child is in its life (ADR-32).
///
/// Two groups, and the split is what the fan-out cap counts: a **live** child
/// holds one of its owner's slots, a **terminal** one has released it. Nothing
/// else distinguishes them — `dirty` and `stop_failed` are live precisely
/// because they still need an operator or an owner to do something, and a child
/// that quietly released its slot in either state would let a leader create a
/// replacement while the first one still holds a worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChildState {
    /// The saga has committed the child's row and the agent is starting. Not
    /// yet proven to be running from its own private state.
    Starting,
    /// The child's own hook has reported. This is the only proof accepted, and
    /// the reason is Stage G: a hook report is what shows the agent is running
    /// from the relocated private state directory, which a live pane process
    /// does not show.
    Ready,
    /// The child's agent is working on a turn.
    Working,
    /// The child's agent is waiting for input it cannot get from its mailbox.
    Blocked,
    /// The child has been nudged repeatedly with no bridge call in return.
    /// Attention, not a verdict — it may simply be a long turn.
    Stalled,
    /// A finish intent has been accepted and the host is verifying. Transient,
    /// but persisted: a crash here must not lose the fact that the agent was
    /// told to stop.
    Finishing,
    /// The quiesce found uncommitted work in the child's worktree. **Never
    /// integrated**, whatever the intent said, and the slot is held: the owner
    /// may `resume` so the worker can commit, or `stop`.
    Dirty,
    /// The child's pane could not be stopped, or its identity did not match what
    /// friring recorded. Never integrated, never reused, and surfaced for an
    /// operator: friring will not guess which process to kill.
    StopFailed,
    /// Verified complete: a `result` with `outcome = completed`, a clean
    /// worktree, and a stopped pane.
    Done,
    /// Verified failed: a `result` with `outcome = failed` and a clean worktree,
    /// or a saga that could not finish.
    Failed,
    /// Stopped by its owner and verified clean. Its runtime and fan-out slot
    /// are released, while ownership, worktree and agent state stay available
    /// for an explicit owner `resume`.
    Stopped,
    /// Relaunched once and still not usable. The end of the automatic road.
    Unusable,
}

impl ChildState {
    /// Every state, in lifecycle order.
    pub const ALL: &'static [Self] = &[
        Self::Starting,
        Self::Ready,
        Self::Working,
        Self::Blocked,
        Self::Stalled,
        Self::Finishing,
        Self::Dirty,
        Self::StopFailed,
        Self::Done,
        Self::Failed,
        Self::Stopped,
        Self::Unusable,
    ];

    /// The states that hold one of the owner's fan-out slots.
    pub const LIVE: &'static [Self] = &[
        Self::Starting,
        Self::Ready,
        Self::Working,
        Self::Blocked,
        Self::Stalled,
        Self::Finishing,
        Self::Dirty,
        Self::StopFailed,
    ];

    /// Storage value, wire value and UI label.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Stalled => "stalled",
            Self::Finishing => "finishing",
            Self::Dirty => "dirty",
            Self::StopFailed => "stop_failed",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
            Self::Unusable => "unusable",
        }
    }

    /// Whether this state holds a slot.
    pub fn is_live(self) -> bool {
        Self::LIVE.contains(&self)
    }

    /// Whether this state has no live runtime and holds no fan-out slot.
    ///
    /// Most such states are final. `stopped` and `unusable` are deliberately
    /// resumable: an explicit owner action may reacquire a slot and relaunch
    /// the same child from its preserved worktree and agent state.
    pub fn is_terminal(self) -> bool {
        !self.is_live()
    }

    /// Whether an owner may `resume` a child in this state.
    ///
    /// `dirty` and `stalled` already hold a slot. `stopped` and `unusable`
    /// released theirs, so the relaunch path must reacquire capacity before it
    /// changes either state. `starting`, `ready`, `working` and `blocked` are a
    /// child that is already running — relaunching one would kill work in
    /// progress — and `finishing` is the host mid-verification.
    pub fn is_resumable(self) -> bool {
        matches!(
            self,
            Self::Dirty | Self::Stalled | Self::Stopped | Self::Unusable
        )
    }
}

impl fmt::Display for ChildState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ChildState {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let key = s.trim().to_ascii_lowercase();
        Self::ALL
            .iter()
            .copied()
            .find(|state| state.as_str() == key)
            .ok_or_else(|| format!("Unknown bridge child state '{s}'"))
    }
}

/// How far a child-spawn saga has got (ADR-32).
///
/// Persisted on every transition, because recovery acts on **recorded
/// identities only**: the step says which external effects exist, and each one
/// was written down before it was made. A step friring cannot read is
/// [`Failed`](Self::Failed) — the narrow reading, which reconciles nothing it
/// cannot name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SagaStep {
    /// S0: the request is journaled and nothing external exists.
    Accepted,
    /// S1: the child id, name and saga row exist. Still nothing external.
    Named,
    /// S2: a git worktree and branch exist at the recorded paths.
    Worktree,
    /// S3: the child's scratch, signal, bridge, gate and private state
    /// directories exist and the state directory is seeded.
    Dirs,
    /// S4: an egress proxy is prepared (not yet committed) at the recorded
    /// endpoint.
    Egress,
    /// S5: a gated pane exists at the recorded window, pane and pane pid,
    /// running the launch helper and waiting.
    Pane,
    /// S6: the one transaction has committed — session row, ownership row,
    /// starting state and the initial task mail all exist together.
    Committed,
    /// S7: the egress supervisor has acknowledged the commit.
    EgressLive,
    /// S8: the gate has been released and the agent is running.
    Released,
    /// S9: the child's own hook has reported. The saga is complete.
    Done,
    /// The saga will not continue. Its journaled outcome is what a replay
    /// returns.
    Failed,
}

impl SagaStep {
    /// Every step, in the order a saga passes through them.
    pub const ALL: &'static [Self] = &[
        Self::Accepted,
        Self::Named,
        Self::Worktree,
        Self::Dirs,
        Self::Egress,
        Self::Pane,
        Self::Committed,
        Self::EgressLive,
        Self::Released,
        Self::Done,
        Self::Failed,
    ];

    /// Storage value and log label.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Named => "named",
            Self::Worktree => "worktree",
            Self::Dirs => "dirs",
            Self::Egress => "egress",
            Self::Pane => "pane",
            Self::Committed => "committed",
            Self::EgressLive => "egress_live",
            Self::Released => "released",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }

    /// Whether the child's durable rows exist — the line recovery turns on.
    ///
    /// Before it, reconciliation removes what the saga made and fails the
    /// request. After it, the child is a real session: recovery adopts it,
    /// restores its egress and carries on.
    pub fn is_committed(self) -> bool {
        matches!(
            self,
            Self::Committed | Self::EgressLive | Self::Released | Self::Done
        )
    }

    /// Whether the saga has stopped moving.
    pub fn is_final(self) -> bool {
        matches!(self, Self::Done | Self::Failed)
    }
}

impl fmt::Display for SagaStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SagaStep {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let key = s.trim().to_ascii_lowercase();
        Self::ALL
            .iter()
            .copied()
            .find(|step| step.as_str() == key)
            .ok_or_else(|| format!("Unknown child saga step '{s}'"))
    }
}

/// What a child said about its own work when it asked to finish (ADR-32).
///
/// Closed to two values on purpose. The finish *intent* is the child's; the
/// terminal *state* is the host's, decided after the pane is stopped and the
/// worktree inspected. A child claiming `completed` over a dirty worktree lands
/// in [`ChildState::Dirty`], not in `done`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    Completed,
    Failed,
}

impl Outcome {
    /// Both outcomes.
    pub const ALL: &'static [Self] = &[Self::Completed, Self::Failed];

    /// Storage value, wire value and UI label.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    /// The terminal state a **clean, stopped** child with this outcome reaches.
    /// A dirty or unstoppable child never gets here.
    pub fn terminal_state(self) -> ChildState {
        match self {
            Self::Completed => ChildState::Done,
            Self::Failed => ChildState::Failed,
        }
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Outcome {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let key = s.trim().to_ascii_lowercase();
        Self::ALL
            .iter()
            .copied()
            .find(|outcome| outcome.as_str() == key)
            .ok_or_else(|| format!("Unknown result outcome '{s}' (expected completed, failed)"))
    }
}

/// The ownership record a child's authority is derived from (ADR-32).
///
/// Immutable for the life of the database — the table's own triggers refuse an
/// `UPDATE` and a `DELETE` — because every verb's authority reads it. A row that
/// could be re-pointed at another owner would make ownership a thing anything
/// holding a write handle to the file could reassign.
///
/// `parent_session_id` on the session row is **display only**: it is what the
/// UI shows and what a user can change, and it is never asked whether a verb is
/// allowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeChild {
    /// The child's session id.
    pub child_id: String,
    /// The session that created it. Never changes.
    pub owner_id: String,
    /// The `create` request key it was made for, so a replay finds the same
    /// child instead of making a second one.
    pub request_key: String,
    /// Unix millis.
    pub created_at: u64,
}

/// A child's mutable lifecycle state, beside its immutable ownership.
///
/// Separate from [`BridgeChild`] because this is what churns and that is what
/// must not. `archived_at` hides a long-dead child from `status` and the UI
/// without ever deleting the ownership row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeChildState {
    pub child_id: String,
    pub state: ChildState,
    /// When the host's `ack` for a finish intent was sent.
    pub acked_at: Option<u64>,
    /// When the child last claimed mail — the counter the stall rule resets.
    pub claimed_at: Option<u64>,
    /// When a finish intent was accepted.
    pub finishing_at: Option<u64>,
    /// When the child last filed a report.
    pub last_report_at: Option<u64>,
    /// When the child reached a terminal state.
    pub tombstoned_at: Option<u64>,
    /// When an operator force-deleted it.
    pub force_deleted_at: Option<u64>,
    /// When it was hidden from the default views. Never a delete.
    pub archived_at: Option<u64>,
    pub updated_at: u64,
}

/// What the **host** found in a child's worktree after it stopped the pane
/// (ADR-32).
///
/// Never what the child said. The `outcome` is the intent the host accepted, and
/// every other field was read by the host from git after the agent could no
/// longer write. An integration step reads these and nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeResult {
    pub child_id: String,
    /// The mail row the finish intent arrived on, for provenance.
    pub message_id: Option<i64>,
    pub outcome: Outcome,
    pub branch: Option<String>,
    /// `git rev-parse HEAD` in the child's worktree, after the stop.
    pub head: Option<String>,
    /// `git status --porcelain` was non-empty.
    pub dirty: bool,
    /// `git rev-list --count <base>..HEAD`.
    pub ahead_of_base: u32,
    pub verified_at: u64,
}

/// One bounded progress report a child filed about itself.
///
/// Child-authored text, and labelled as such everywhere it is shown. It never
/// reaches a command, a query, a path, a nudge or an OS notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeReport {
    pub child_id: String,
    /// Monotonic per child. The writer caps the table at
    /// [`MAX_REPORTS_PER_CHILD`] by dropping the oldest.
    pub seq: u32,
    pub phase: String,
    pub progress: u8,
    pub summary: String,
    pub needs_operator: bool,
    /// Relative paths inside the child's own worktree, validated on the way in.
    pub artifact_paths: Vec<String>,
    pub created_at: u64,
}

/// How many reports one child keeps. Older ones are dropped by the writer: a
/// report is a progress note, and an unbounded stream of them from inside a
/// boundary is a disk-space channel.
pub const MAX_REPORTS_PER_CHILD: u32 = 20;

/// A running child-spawn saga (ADR-32).
///
/// Every external effect is recorded here **before** it is made, so recovery
/// reconciles by exact identity and never by a prefix scan or by "kill what has
/// no row".
#[derive(Clone, PartialEq, Eq, Default)]
pub struct ChildSaga {
    pub owner_id: String,
    pub key: String,
    pub child_id: Option<String>,
    pub step: Option<SagaStep>,
    /// The task the child is created with, held here from S1 so the S6
    /// transaction can insert the mail in the same commit as the row.
    pub task_kind: Option<String>,
    pub task_body: Option<String>,
    pub worktree_path: Option<String>,
    pub branch: Option<String>,
    /// The commit the branch was cut from, so recovery can ask whether the
    /// branch carries work before deleting it.
    pub base_head: Option<String>,
    /// Whether **this saga's** own `git branch` created [`Self::branch`].
    ///
    /// [`Self::worktree_path`] is recorded before git runs, so it is where this
    /// launch *intended* to put a worktree and not evidence that it did: the
    /// loser of a cross-instance race holds the winner's directory against its
    /// own failed saga. This is the evidence, and only an attempt that
    /// atomically created the ref ever sets it — see
    /// [`crate::git::claim_child_worktree`]. Unset, a reclaim declines and says
    /// so, because a leaked directory can be removed by hand and a wrongly
    /// deleted one cannot be brought back.
    pub branch_claimed: bool,
    /// The child's own finish intent, held because it arrived while this launch
    /// was still running, as [`Outcome::as_str`].
    ///
    /// Persisted rather than kept in the job alone: the launch has not written
    /// [`ChildState::Finishing`] yet, so a crash between S8 and S9 would leave a
    /// child that had already reported to be adopted with no verdict, holding
    /// its owner's fan-out slot — after the `send` that carried the result was
    /// answered `ok`, which a replay returns verbatim rather than re-running.
    pub finish_outcome: Option<String>,
    /// The mail row that intent arrived on, for provenance.
    pub finish_message_id: Option<i64>,
    pub scratch_minted: bool,
    pub gate_dir: Option<String>,
    pub egress_endpoint: Option<String>,
    /// Redacted from every rendering — see `storage::bridge`.
    pub egress_token: Option<String>,
    pub mux_server: Option<String>,
    pub mux_window_id: Option<String>,
    pub mux_pane_id: Option<String>,
    pub mux_pane_pid: Option<u32>,
    /// The `@friring_pane` marker stamped on that pane.
    ///
    /// Without it [`crate::session::MuxIdentity::is_recorded`] is false, so a
    /// recovery that rebuilt an identity from the four columns above could kill
    /// nothing: the guard would return early every time and a crash below the
    /// committed line would always leave its gated pane to the launch helper's
    /// own timeout.
    pub mux_launch_key: Option<String>,
    /// Which TUI instance owns this saga, so two racing instances produce one.
    pub instance_id: Option<String>,
    pub lease_until: Option<u64>,
    pub created_at: u64,
    pub updated_at: u64,
}

impl fmt::Debug for ChildSaga {
    /// Hand-written so [`egress_token`](Self::egress_token) is never rendered.
    ///
    /// The token is the egress proxy's live credential for a running boundary,
    /// and a `{:?}` on a saga row is exactly how one reaches a tracing field, a
    /// log somebody pastes into a bug report, or a failing assertion's own
    /// message. Whether there *is* one is the diagnostic; the value is the
    /// secret.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChildSaga")
            .field("owner_id", &self.owner_id)
            .field("key", &self.key)
            .field("child_id", &self.child_id)
            .field("step", &self.step)
            .field("task_kind", &self.task_kind)
            .field("task_body", &self.task_body)
            .field("worktree_path", &self.worktree_path)
            .field("branch", &self.branch)
            .field("base_head", &self.base_head)
            .field("scratch_minted", &self.scratch_minted)
            .field("gate_dir", &self.gate_dir)
            .field("egress_endpoint", &self.egress_endpoint)
            .field(
                "egress_token",
                &self.egress_token.as_ref().map(|_| "<redacted>"),
            )
            .field("mux_server", &self.mux_server)
            .field("mux_window_id", &self.mux_window_id)
            .field("mux_pane_id", &self.mux_pane_id)
            .field("mux_pane_pid", &self.mux_pane_pid)
            .field("instance_id", &self.instance_id)
            .field("lease_until", &self.lease_until)
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_state_is_live_or_terminal_and_never_both() {
        for state in ChildState::ALL {
            assert_ne!(
                state.is_live(),
                state.is_terminal(),
                "{state} must be exactly one of live and terminal"
            );
        }
        // The four the plan names as terminal, spelled out so a state added
        // later has to decide rather than inherit.
        for state in [
            ChildState::Done,
            ChildState::Failed,
            ChildState::Stopped,
            ChildState::Unusable,
        ] {
            assert!(state.is_terminal(), "{state}");
        }
    }

    /// A dirty or unstoppable child still holds its slot: it has a worktree an
    /// owner has to deal with, and letting a leader create a replacement while
    /// it does is how a fan-out cap stops meaning anything.
    #[test]
    fn dirty_and_stop_failed_hold_their_slot() {
        assert!(ChildState::Dirty.is_live());
        assert!(ChildState::StopFailed.is_live());
    }

    #[test]
    fn a_running_child_is_never_resumable() {
        for state in [
            ChildState::Starting,
            ChildState::Ready,
            ChildState::Working,
            ChildState::Blocked,
            ChildState::Finishing,
        ] {
            assert!(!state.is_resumable(), "{state}");
        }
        for state in [
            ChildState::Dirty,
            ChildState::Stalled,
            ChildState::Stopped,
            ChildState::Unusable,
        ] {
            assert!(state.is_resumable(), "{state}");
        }
    }

    #[test]
    fn every_state_and_step_round_trips_through_its_stored_spelling() {
        for state in ChildState::ALL {
            assert_eq!(ChildState::from_str(state.as_str()).unwrap(), *state);
        }
        for step in SagaStep::ALL {
            assert_eq!(SagaStep::from_str(step.as_str()).unwrap(), *step);
        }
        for outcome in Outcome::ALL {
            assert_eq!(Outcome::from_str(outcome.as_str()).unwrap(), *outcome);
        }
    }

    /// The line recovery turns on: before it the saga's effects are removed,
    /// after it the child is a real session that is adopted and carried on.
    #[test]
    fn the_committed_line_is_where_a_child_becomes_durable() {
        for step in [
            SagaStep::Accepted,
            SagaStep::Named,
            SagaStep::Worktree,
            SagaStep::Dirs,
            SagaStep::Egress,
            SagaStep::Pane,
            SagaStep::Failed,
        ] {
            assert!(!step.is_committed(), "{step}");
        }
        for step in [
            SagaStep::Committed,
            SagaStep::EgressLive,
            SagaStep::Released,
            SagaStep::Done,
        ] {
            assert!(step.is_committed(), "{step}");
        }
    }

    /// The token is the egress proxy's live credential; a `{:?}` on a saga row
    /// must never carry it.
    #[test]
    fn a_saga_debug_never_renders_its_egress_token() {
        let saga = ChildSaga {
            egress_endpoint: Some("tcp:8123".to_string()),
            egress_token: Some("s3cr3t-token-value".to_string()),
            ..ChildSaga::default()
        };
        let rendered = format!("{saga:?}");
        assert!(!rendered.contains("s3cr3t"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
        // The endpoint is not a secret and stays legible.
        assert!(rendered.contains("tcp:8123"), "{rendered}");
    }

    /// Two verbs' bodies can be the same JSON, so the **verb** has to decide
    /// which one a request is — not the order the untagged enum declares them
    /// in.
    ///
    /// `{"child": "c1"}` is a well-formed `stop` (its `grace_secs` is optional)
    /// and a well-formed `resume`. Serde matches untagged by first arm, so a
    /// broker that read the arm would answer every `resume` as a `stop`.
    #[test]
    fn a_body_is_read_against_its_verb_and_not_against_the_arm_serde_picked() {
        let raw = r#"{"protocol":1,"key":"abc-1234","verb":"resume","body":{"child":"c1"}}"#;
        let request: Request = serde_json::from_str(raw).unwrap();
        assert_eq!(request.verb, Verb::Resume);
        // The arm serde picked is the *other* verb's, which is exactly the trap.
        assert!(matches!(request.body, RequestBody::Stop(_)), "{request:?}");
        // Read against the verb, it is what it says it is.
        let body: ResumeBody = request.body_as().unwrap();
        assert_eq!(body.child, "c1");
        // And a body that really is not this verb's is refused rather than
        // reinterpreted.
        let mismatched = Request {
            verb: Verb::Report,
            ..request
        };
        assert!(mismatched.body_as::<ReportBody>().is_err());
    }

    #[test]
    fn an_outcome_decides_only_a_clean_childs_terminal_state() {
        assert_eq!(Outcome::Completed.terminal_state(), ChildState::Done);
        assert_eq!(Outcome::Failed.terminal_state(), ChildState::Failed);
    }
}

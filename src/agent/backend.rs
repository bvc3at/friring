use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex, OnceLock,
};
use std::time::SystemTime;

use anyhow::{bail, Context as _, Result};
use tokio::sync::mpsc;
use tracing::{debug, error, warn};

use crate::agent::osc52::Osc52Scanner;
use crate::agent::provider::AgentProvider;
use crate::sandbox::PendingEgress;
use crate::session::{SandboxState, SessionConfig, SessionInfo};

pub(crate) fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Length of the prefix of `buf` that is safe to feed to the vt100 parser
/// without splitting a UTF-8 character. Returns `buf.len()` unless `buf` ends
/// with the start of a multi-byte character whose continuation bytes have not
/// all arrived yet, in which case it returns the offset of that incomplete
/// lead byte (so the caller can carry the tail to the next read).
///
/// Only a *plausibly-complete-able* truncated tail is held back: a lead byte
/// missing some of its continuations. A malformed tail (continuation bytes with
/// no lead, or a fully-present sequence) is passed through as-is, so garbage is
/// never buffered unboundedly — the carry is at most 3 bytes (a 4-byte char
/// missing its last byte).
fn utf8_ready_prefix_len(buf: &[u8]) -> usize {
    let len = buf.len();
    // A truncated tail is at most 3 bytes, so only the last 3 can matter.
    let start = len.saturating_sub(3);
    let mut i = len;
    while i > start {
        i -= 1;
        let b = buf[i];
        // Anything that is not a UTF-8 continuation byte (0x80..=0xbf) starts a
        // character — ASCII or a multi-byte lead.
        if !(0x80..=0xbf).contains(&b) {
            // A lead byte (or ASCII). Determine the sequence's expected length;
            // if not all of it is present yet, cut before it.
            let expected = match b {
                0x00..=0x7f => 1,
                0xc0..=0xdf => 2,
                0xe0..=0xef => 3,
                _ => 4,
            };
            return if len - i < expected { i } else { len };
        }
        // Continuation byte (0x80..=0xbf): keep scanning back for its lead.
    }
    // No lead byte within the last 3 bytes — malformed tail; don't hold it back.
    len
}

/// Captures terminal signals the agent emits into shared cells read by the app
/// layer (mirrors the `last_output_at` side channel). The parser fires these
/// callbacks while processing the PTY byte stream:
///
/// - **Title** (OSC `0`/`1`/`2`) → live activity text.
/// - **Attention** — a terminal bell (`BEL`) or a desktop-notification escape
///   (OSC `9`, OSC `777`) means the agent finished or needs input. We record
///   the time of the latest such signal, plus its message text when the OSC
///   carries one. This is how we surface a real "needs attention" state instead
///   of timing-only Busy/Waiting.
#[derive(Clone, Default)]
pub struct TermSignals {
    title: Arc<Mutex<Option<String>>>,
    /// `now_millis()` of the most recent attention signal; `0` = none yet.
    attention_at: Arc<AtomicU64>,
    /// Message text from the most recent OSC 9/777 notification, if any.
    notification: Arc<Mutex<Option<String>>>,
    /// Generation counter bumped after every title/notification write, so the
    /// per-tick status refresh can skip the mutex locks + String clones while
    /// nothing changed (ADR-P10; see [`Session::sync_agent_meta`]).
    meta_gen: Arc<AtomicU64>,
}

impl TermSignals {
    fn store_title(&self, raw: &[u8]) {
        let s = String::from_utf8_lossy(raw).trim().to_string();
        if let Ok(mut guard) = self.title.lock() {
            *guard = (!s.is_empty()).then_some(s);
        }
        // After the write, so a reader that observes the new generation also
        // observes the new value.
        self.meta_gen.fetch_add(1, Ordering::Release);
    }

    /// Mark an attention signal, optionally with notification message text.
    fn signal_attention(&self, message: Option<String>) {
        self.attention_at.store(now_millis(), Ordering::Relaxed);
        if let Some(msg) = message {
            let msg = msg.trim().to_string();
            if let Ok(mut guard) = self.notification.lock() {
                *guard = (!msg.is_empty()).then_some(msg);
            }
            self.meta_gen.fetch_add(1, Ordering::Release);
        }
    }
}

impl vt100::Callbacks for TermSignals {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.store_title(title);
    }

    fn set_window_icon_name(&mut self, _: &mut vt100::Screen, icon_name: &[u8]) {
        // Some CLIs emit only OSC `1` (icon name); treat it as the title too.
        self.store_title(icon_name);
    }

    fn audible_bell(&mut self, _: &mut vt100::Screen) {
        // BEL: the cross-agent "done / needs you" signal (e.g. Claude's
        // `preferredNotifChannel terminal_bell`). No message text.
        self.signal_attention(None);
    }

    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        // Desktop-notification escapes carry the agent's status message.
        //   OSC 9 ; <message>
        //   OSC 777 ; notify ; <title> ; <body>
        match params {
            [b"9", msg] => self.signal_attention(Some(String::from_utf8_lossy(msg).into_owned())),
            [b"777", kind, rest @ ..] if kind.eq_ignore_ascii_case(b"notify") => {
                let msg = rest
                    .iter()
                    .map(|p| String::from_utf8_lossy(p))
                    .collect::<Vec<_>>()
                    .join(": ");
                self.signal_attention(Some(msg));
            }
            _ => {}
        }
    }
}

/// Session terminal parser, specialized to capture terminal signals via
/// [`TermSignals`]. The captured `Screen` is callback-independent, so
/// rendering is unaffected.
pub type SessionParser = vt100::Parser<TermSignals>;

/// Process-wide monotonic capture sequence, stamped on every OSC 52 copy so
/// the app can tell which copy drained from which pane was captured last, and
/// give that one the clipboard (pane-by-pane draining alone would let a later
/// pane's older copy win — see
/// [`crate::app::App::drain_pane_clipboard_copies`]).
static OSC52_SEQ: AtomicU64 = AtomicU64::new(0);

/// Clipboard writes captured from a pane's output stream (OSC 52 — see
/// [`crate::agent::osc52`]), queued for the app to route through its clipboard
/// stack on the next tick. Generation-gated like `TermSignals::meta_gen`
/// (ADR-P10): the every-tick, nothing-new poll pays one atomic load per pane,
/// never a lock. Each queued copy carries a global capture sequence
/// (`OSC52_SEQ`) so cross-pane draining can restore capture order.
#[derive(Default)]
pub struct PaneClipboard {
    queue: Mutex<VecDeque<(u64, String)>>,
    /// Bumped **after** each push (`Release`), so an observer that sees the new
    /// generation also sees the queued value.
    gen: AtomicU64,
}

impl PaneClipboard {
    /// Cap on undrained copies. Drop-oldest: with clipboard writes, the
    /// *newest* is the one that must end up winning, so a pane spamming OSC 52
    /// between ticks loses its stale copies, never its latest.
    const CAP: usize = 8;

    fn push(&self, text: String) {
        let seq = OSC52_SEQ.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut q) = self.queue.lock() {
            if q.len() >= Self::CAP {
                q.pop_front();
            }
            q.push_back((seq, text));
        }
        self.gen.fetch_add(1, Ordering::Release);
    }

    /// Drain copies queued since the caller's `last_seen` generation, oldest
    /// first (each paired with its global capture sequence); empty — without
    /// locking — when nothing new arrived.
    fn drain_new(&self, last_seen: &mut u64) -> Vec<(u64, String)> {
        let gen = self.gen.load(Ordering::Acquire);
        if gen == *last_seen {
            return Vec::new();
        }
        *last_seen = gen;
        self.queue
            .lock()
            .map(|mut q| q.drain(..).collect())
            .unwrap_or_default()
    }
}

/// Metadata returned when discovering existing sessions from the backend.
#[derive(Clone)]
pub struct DiscoveredSession {
    /// Backend-specific ID (e.g., tmux pane_id).
    pub backend_id: String,
    /// Window name or label.
    pub name: String,
    /// Whether the process is still running.
    pub is_alive: bool,
}

/// A newly spawned session from the backend.
pub struct SpawnedSession {
    /// Backend-specific session identifier.
    pub backend_id: String,
    /// Streaming output bytes from the session.
    pub output: Box<dyn Read + Send>,
    /// Input write handle to send bytes to the session.
    pub input: Box<dyn Write + Send>,
}

/// A reconnected session from the backend.
pub struct AdoptedSession {
    /// Streaming output bytes from the session.
    pub output: Box<dyn Read + Send>,
    /// Input write handle to send bytes to the session.
    pub input: Box<dyn Write + Send>,
}

/// Trait that all session backends implement. The app layer interacts only through this trait.
pub trait SessionBackend: Send + Sync {
    /// Human-readable name (e.g., "local-tmux", "ssh-remote").
    fn name(&self) -> &str;

    /// Check if the backend is available/healthy.
    fn check_available(&self) -> Result<()>;

    /// Initialize the backend (e.g., start tmux server).
    fn ensure_ready(&self) -> Result<()>;

    /// Spawn a new session running the given command.
    #[allow(clippy::too_many_arguments)]
    fn spawn(
        &self,
        window_name: &str,
        command: &str,
        args: &[String],
        cwd: Option<&Path>,
        env: &HashMap<String, String>,
        rows: u16,
        cols: u16,
    ) -> Result<SpawnedSession>;

    /// Reconnect to an existing session. `seed` is pre-captured scrollback
    /// history to prepend to the live stream (see [`Self::capture_history`]);
    /// `None` makes the backend capture it itself — the two paths produce the
    /// same bytes, `Some` just lets restore overlap the captures (ADR-P9).
    fn adopt(
        &self,
        backend_id: &str,
        rows: u16,
        cols: u16,
        seed: Option<Vec<u8>>,
    ) -> Result<AdoptedSession>;

    /// Capture a session's scrollback history as terminal bytes suitable for
    /// seeding a fresh parser, to pass into [`Self::adopt`]. An independent
    /// subprocess per pane, safe to run concurrently across sessions — unlike
    /// `adopt`'s control-mode connect, which is serialized. Default: no
    /// history (backends without a capture facility adopt with an empty
    /// scrollback, exactly as if the capture had failed).
    fn capture_history(&self, _backend_id: &str) -> Result<Vec<u8>> {
        Ok(Vec::new())
    }

    /// Capture just the pane's **visible screen** as terminal bytes — the
    /// ghost frame taken at unload/shutdown. Scrollback is deliberately not
    /// included: it can only ever hold output that *scrolled out* of the pane,
    /// and a full-screen agent TUI repaints in place, so for the sessions
    /// friring drives there is none (measured: `#{history_size}` is 0 for
    /// claude/codex/opencode/agy). Preferred over serializing the parser
    /// in-memory because the backend joins soft-wrapped rows into logical
    /// lines, which re-wrap cleanly when a ghost is rendered at another width.
    /// Default: an empty seed, like [`Self::capture_history`].
    fn capture_visible(&self, _backend_id: &str) -> Result<Vec<u8>> {
        Ok(Vec::new())
    }

    /// Discover existing sessions managed by this backend.
    fn discover(&self) -> Result<Vec<DiscoveredSession>>;

    /// Resize a session's terminal.
    fn resize(&self, backend_id: &str, rows: u16, cols: u16) -> Result<()>;

    /// Check if a session's process has exited.
    fn is_dead(&self, backend_id: &str) -> Result<bool>;

    /// Kill/destroy a session (for Ctrl+X close).
    fn kill(&self, backend_id: &str) -> Result<()>;

    /// Detach from a session without killing it (for Ctrl+Q quit).
    fn detach(&self, backend_id: &str) -> Result<()>;

    /// Default shell command for companion shell panes.
    ///
    /// Unix uses `$SHELL` (falling back to `/bin/sh`); Windows uses `%COMSPEC%`
    /// (falling back to `cmd.exe`), since `$SHELL`/`/bin/sh` don't exist there.
    fn default_shell(&self) -> String {
        #[cfg(windows)]
        {
            std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string())
        }
        #[cfg(not(windows))]
        {
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
        }
    }

    /// Return the PID of the process running in a backend pane.
    fn pane_pid(&self, backend_id: &str) -> Result<Option<u32>>;

    /// Drain queued `(backend_id, hook-state)` events reported by a remote
    /// agent's hooks (a tmux pane user option pushed over the control-mode
    /// subscription — see [`crate::session::REMOTE_HOOK_STATE_OPTION`]).
    ///
    /// Poll-style shared state (like the `TermSignals` atomics): the app tick
    /// drains this and persists each state exactly as a local
    /// `friring-cli session signal` would have. Default: no events — only the
    /// tmux backend produces them.
    fn take_hook_state_events(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    /// Tear down the backend's own long-lived resources (for a tmux backend,
    /// its control-mode connection: child process + reader thread).
    ///
    /// Distinct from [`Self::detach`], which retires one *session*'s pane. This
    /// retires the *connection*, and is called once per backend at quit.
    ///
    /// Exists as an explicit method rather than relying on `Drop` so quit can
    /// run every backend's teardown **concurrently**: the registry holds each
    /// backend behind an `Arc`, so dropping it is both hard to sequence and
    /// serial by nature, and each connection's teardown blocks on a child exit.
    /// Total quit cost is then the slowest connection rather than their sum —
    /// which matters because the backend count grows with every configured SSH
    /// host and auto-discovered WSL distro. Must be idempotent: a later `Drop`
    /// still runs and has to be a no-op.
    ///
    /// Default: nothing to tear down.
    fn shutdown(&self) {}
}

/// Internal bundle of I/O handles before wiring.
struct SessionIo {
    output: Box<dyn Read + Send>,
    input: Box<dyn Write + Send>,
    backend_id: String,
    /// Whether these handles came from a fresh spawn or an adopt.
    mode: WireMode,
}

/// Whether we are wiring a freshly-spawned process or reconnecting to an
/// already-running one. Controls the initial `last_output_at`: a fresh spawn
/// is legitimately starting up (recent activity → `Busy`), whereas an adopt's
/// first output is the forced SIGWINCH repaint, which must not be mistaken for
/// real agent activity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WireMode {
    Spawn,
    Adopt,
}

/// Initial `last_output_at` for a session being wired in `mode`. `Spawn` uses
/// "now" (fresh process is active); `Adopt` uses `0` (stale) so the post-adopt
/// repaint doesn't read as activity — the first *real* output flips it to busy.
fn initial_output_at(mode: WireMode) -> u64 {
    match mode {
        WireMode::Spawn => now_millis(),
        WireMode::Adopt => 0,
    }
}

/// Max pending input messages per session before sends fail fast. Each
/// message is one key/paste payload; a full queue means the tmux stdin
/// writer has stalled, and dropping with an error beats unbounded growth.
const INPUT_CHANNEL_CAPACITY: usize = 1024;

/// Queue input without ever blocking — `send_input` is called from the
/// render/update path, so a stalled writer must surface as an error, not a
/// hang.
fn send_to_input_channel(tx: &mpsc::Sender<Vec<u8>>, data: Vec<u8>, what: &str) -> Result<()> {
    use tokio::sync::mpsc::error::TrySendError;
    tx.try_send(data).map_err(|e| match e {
        TrySendError::Full(_) => anyhow::anyhow!("{what} input channel full (writer stalled)"),
        TrySendError::Closed(_) => anyhow::anyhow!("{what} input channel closed"),
    })
}

/// Wired-up I/O state: parser, channels, and exit tracking.
struct WiredState {
    parser: Arc<Mutex<SessionParser>>,
    input_tx: mpsc::Sender<Vec<u8>>,
    exited: Arc<AtomicBool>,
    last_output_at: Arc<AtomicU64>,
    last_title: Arc<Mutex<Option<String>>>,
    attention_at: Arc<AtomicU64>,
    notification: Arc<Mutex<Option<String>>>,
    meta_gen: Arc<AtomicU64>,
    osc52: Arc<PaneClipboard>,
}

/// A companion shell pane running alongside an agent session.
pub struct ShellPane {
    pub parser: Arc<Mutex<SessionParser>>,
    input_tx: mpsc::Sender<Vec<u8>>,
    backend_id: String,
    /// Kept alive so the reader loop's Arc clone has a peer.
    #[allow(dead_code)]
    exited: Arc<AtomicBool>,
    last_output_at: Arc<AtomicU64>,
    /// Captured OSC title for the shell pane (unused; kept for symmetry).
    #[allow(dead_code)]
    last_title: Arc<Mutex<Option<String>>>,
    /// OSC 52 clipboard writes from the shell pane, drained through
    /// [`Session::drain_osc52_copies`].
    osc52: Arc<PaneClipboard>,
    /// The [`PaneClipboard`] generation last drained.
    last_drained_osc52_gen: u64,
}

impl ShellPane {
    pub fn send_input(&self, data: Vec<u8>) -> Result<()> {
        send_to_input_channel(&self.input_tx, data, "Shell")
    }

    /// Raw monotonic timestamp (epoch millis) of the pane's last output — the
    /// shell twin of [`Session::last_output_at`]. Read by the render loop's
    /// output-change detector ([`crate::app::App::detect_output_redraw`]) so a
    /// shell keystroke's echo repaints immediately instead of waiting out the
    /// forced-redraw floor.
    pub fn last_output_at(&self) -> u64 {
        self.last_output_at.load(Ordering::Relaxed)
    }

    /// Shell twin of [`Session::feed_output_for_test`]: bump `last_output_at`
    /// and run the bytes through the vt100 parser, driving the same state the
    /// reader loop's `%output` path drives. OSC 52 sequences are captured like
    /// the reader loop does, but per call — a sequence must complete within
    /// one feed (the live scanner persists across reads).
    #[cfg(test)]
    pub fn feed_output_for_test(&self, bytes: &[u8]) {
        // Strictly-increasing bump, unlike the reader loop's plain `now_millis()`
        // store: two feeds within the same millisecond must still read as *new*
        // output to `App::detect_output_redraw`'s signature.
        let prev = self.last_output_at.load(Ordering::Relaxed);
        self.last_output_at
            .store(now_millis().max(prev + 1), Ordering::Relaxed);
        for copy in Osc52Scanner::default().scan(bytes) {
            self.osc52.push(copy);
        }
        if let Ok(mut p) = self.parser.lock() {
            p.process(bytes);
        }
    }

    /// Build a ShellPane from wired-up I/O state.
    fn from_wired(state: WiredState, backend_id: String) -> Self {
        Self {
            parser: state.parser,
            input_tx: state.input_tx,
            backend_id,
            exited: state.exited,
            last_output_at: state.last_output_at,
            last_title: state.last_title,
            osc52: state.osc52,
            last_drained_osc52_gen: 0,
        }
    }
}

/// The bare host name for a session's off-local backend (e.g. `devbox` for an
/// `ssh:devbox` backend, `Ubuntu` for a `wsl:Ubuntu` backend), or `None` for
/// local backends. Drives the session list's remote indicator.
fn remote_host_from_backend(backend: &Arc<dyn SessionBackend>) -> Option<String> {
    let name = backend.name();
    name.strip_prefix(crate::session::SSH_BACKEND_PREFIX)
        .or_else(|| name.strip_prefix(crate::session::WSL_BACKEND_PREFIX))
        .map(str::to_string)
}

/// Everything a `backend.spawn` needs, with the session's sandbox profile
/// already applied. Built by [`sandboxed_invocation`].
struct Sandboxed {
    command: String,
    args: Vec<String>,
    env: HashMap<String, String>,
    /// The egress proxy this invocation was composed against, if the profile
    /// needs one. Provisional: the session keeps the instance it is already
    /// using until [`PendingEgress::commit`], and a launch that fails between
    /// composing and spawning releases this one by dropping it. Every field
    /// above is useless without a pane, and so is this.
    egress: PendingEgress,
    /// The profile the session **asked for**, for
    /// `SessionInfo::sandbox_profile`. Carried through whether or not the
    /// boundary went on: a launch that fell back to the host must not erase the
    /// session's link to its profile, or the next relaunch — once the backend
    /// is available again — would have nothing to rebuild from.
    profile: Option<String>,
    /// What the launch **applied**, for `SessionInfo::sandbox_state`. `None`
    /// only when the session carries no profile at all.
    state: Option<SandboxState>,
    /// The **place** the invocation runs in, when the profile resolved to a
    /// place backend (ADR-26). It is a transport, so the launch spawns through
    /// it rather than through the backend the caller passed in: tmux is inside
    /// the place, and its `sandbox:<profile>` name is what lands in
    /// `backend_type` and drives restore.
    place: Option<crate::agent::transport::Place>,
    /// The place row to record in `sandbox_instances` once the launch has a
    /// pane, so garbage collection can find the container it created.
    instance: Option<crate::sandbox::SandboxInstance>,
}

/// Bring a place's own tmux up before spawning into it.
///
/// Separate from the caller's `ensure_backend_ready` because a place-backed
/// launch resolves its transport *during* the composition — the container has to
/// exist before there is anything to ready — so by the time this backend is
/// known the caller's readiness step is already behind us.
fn ready_place(backend: &Arc<dyn SessionBackend>) -> Result<()> {
    backend
        .ensure_ready()
        .with_context(|| format!("sandbox place '{}' has no reachable tmux", backend.name()))
}

/// What an unreachable placeholder's pane says while friring keeps trying.
///
/// The two off-host shapes fail differently and the pane has to say which
/// (ADR-26's stated consequence). A remote host takes down the sessions it was
/// running; a **place** takes down *every* session using its profile at once,
/// because they share one container — so a user looking at three frozen panes
/// needs to know whether that is three problems or one, and where to look.
fn unreachable_notice(backend: &str, host: Option<&str>) -> String {
    match crate::session::sandbox_backend_profile(backend) {
        Some(profile) => format!(
            "\r\n  \u{2298} Sandbox place '{profile}' is not running \u{2014} \
             retrying\u{2026}\r\n\r\n  \
             Every session using this profile shares one place, so they all stopped\r\n  \
             together. friring starts it again and reattaches by itself.\r\n  \
             Press restart to retry now, or delete to remove this session.\r\n"
        ),
        None => {
            let host = host.unwrap_or("?");
            format!(
                "\r\n  \u{2298} Remote host '{host}' unreachable \u{2014} retrying\u{2026}\r\n\r\n  \
                 This session will reconnect automatically when the host comes back.\r\n  \
                 Press restart to retry now, or delete to remove it.\r\n"
            )
        }
    }
}

/// The transport for a place a launch composed against — **one per place**,
/// shared by every session in it.
///
/// `TmuxBackend::for_place` is the whole of the transport: everything above that
/// seam — control mode, discovery, adoption, input, scrollback — is the SSH
/// path's, unchanged (ADR-26). What is not free is the connection it opens: a
/// backend per session would be one `<engine> exec -i` process, one tmux client
/// and one reader thread each, where a place is created once per profile and
/// shared by that profile's sessions. So is this.
///
/// Keyed on the profile and compared on the whole address, so a rebuilt
/// container (an edited profile asks for a new one) retires the connection into
/// the container it replaced rather than keeping both.
pub(crate) fn place_backend(place: &crate::agent::transport::Place) -> Arc<dyn SessionBackend> {
    /// Open places by their `sandbox:<profile>` name, each with the engine and
    /// container it was built for so a rebuild is noticed.
    type OpenPlaces = Mutex<HashMap<String, (String, Arc<dyn SessionBackend>)>>;
    static PLACES: OnceLock<OpenPlaces> = OnceLock::new();

    let address = format!("{}\u{1}{}", place.engine(), place.container());
    let mut open = PLACES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((held, backend)) = open.get(&place.backend_name()) {
        if *held == address {
            return Arc::clone(backend);
        }
    }
    let backend: Arc<dyn SessionBackend> =
        Arc::new(crate::agent::tmux::TmuxBackend::for_place(place));
    open.insert(place.backend_name(), (address, Arc::clone(&backend)));
    backend
}

/// Compose the invocation for a spawn or a restart, wrapping it in the
/// session's sandbox profile when it has one.
///
/// Both live-session launch paths go through here so a restart re-derives the
/// same boundary a spawn built, from the same [`SessionConfig`] the app
/// reloaded out of the database. Wrapping happens here rather than inside the
/// backend because the backend is where per-transport quoting starts (see
/// [`crate::agent::sandboxing`]).
fn sandboxed_invocation(
    config: &SessionConfig,
    provider: &Arc<dyn AgentProvider>,
) -> Result<Sandboxed> {
    let command = provider.command().to_string();
    let args = provider.build_args(config);
    let decision = crate::agent::sandboxing::apply(provider.agent_def(), config, &command, &args)
        .map_err(anyhow::Error::msg)?;
    let plain = Sandboxed {
        command,
        args,
        env: config.env.clone(),
        // The desired profile, regardless of what the decision turns out to be:
        // it is `None` exactly when the session carries none.
        profile: config.sandbox.as_ref().map(|p| p.name.clone()),
        state: None,
        place: None,
        instance: None,
        // Claimed here rather than left parked so the invocation and the
        // boundary it names travel together: whatever happens to one of them
        // from now on happens to both.
        egress: crate::agent::sandboxing::pending_egress(config),
    };

    match decision {
        crate::agent::sandboxing::SandboxDecision::Unsandboxed => Ok(plain),
        // The escape hatch fired: the agent runs on the host. The link to the
        // profile survives so a later relaunch is sandboxed again, and the
        // reason rides on the session so the UI can say so — a log line is not
        // an indicator.
        crate::agent::sandboxing::SandboxDecision::Skipped { reason } => {
            warn!(agent = %config.agent, "{reason}");
            Ok(Sandboxed {
                state: Some(SandboxState::Unenforced(reason)),
                ..plain
            })
        }
        crate::agent::sandboxing::SandboxDecision::Wrapped(wrapped) => {
            let mut env = plain.env;
            env.extend(wrapped.env);
            debug!(sandbox = %wrapped.label, "Wrapping agent invocation");
            Ok(Sandboxed {
                command: wrapped.command,
                args: wrapped.args,
                env,
                profile: plain.profile,
                state: Some(SandboxState::Applied(wrapped.state)),
                egress: plain.egress,
                place: wrapped.place,
                instance: wrapped.instance,
            })
        }
    }
}

/// A running session connected to a backend.
pub struct Session {
    pub info: SessionInfo,
    pub parser: Arc<Mutex<SessionParser>>,
    input_tx: mpsc::Sender<Vec<u8>>,
    backend_id: String,
    backend: Arc<dyn SessionBackend>,
    provider: Arc<dyn AgentProvider>,
    exited: Arc<AtomicBool>,
    last_output_at: Arc<AtomicU64>,
    /// Latest OSC window title the agent emitted (live activity text).
    last_title: Arc<Mutex<Option<String>>>,
    /// `now_millis()` of the latest attention signal (bell / OSC 9 / OSC 777).
    attention_at: Arc<AtomicU64>,
    /// Message text from the latest OSC 9/777 notification, if any.
    notification: Arc<Mutex<Option<String>>>,
    /// Shared with [`TermSignals::meta_gen`]: bumped by the reader thread on
    /// every title/notification write.
    meta_gen: Arc<AtomicU64>,
    /// The generation last consumed by [`Self::sync_agent_meta`]. Starts at
    /// `u64::MAX` so the first tick always syncs.
    last_synced_meta_gen: u64,
    /// OSC 52 clipboard writes captured from the agent pane's output stream,
    /// awaiting the app's per-tick [`Self::drain_osc52_copies`].
    osc52: Arc<PaneClipboard>,
    /// The [`PaneClipboard`] generation last drained.
    last_drained_osc52_gen: u64,
    /// `now_millis()` of the last attention acknowledgement (set while the
    /// session is the active one). Attention is pending when
    /// `attention_at > attention_ack_at`.
    attention_ack_at: u64,
    pub shell_pane: Option<ShellPane>,
    /// Session environment variables, passed to shell pane spawns.
    env: HashMap<String, String>,
    /// True for a **placeholder** session: no live backend pane / reader /
    /// writer (its `input_tx` is a dead channel), so every pane-touching path
    /// (kill/detach/hook sync/metrics/save_state upsert) skips it. Two kinds
    /// exist, told apart by [`Self::is_ghost`]: a persisted remote session
    /// whose host is unreachable (parser holds a "host unreachable" notice,
    /// replaced in place once the host recovers), and a **ghost** (below).
    placeholder: bool,
    /// True for a **ghost**: a placeholder whose agent process is deliberately
    /// not running (unloaded, or lazily restored after the tmux server died).
    /// Its parser is seeded with the session's saved last frame, rendered
    /// greyed; [`Self::restart`] spawns the agent and clears both flags.
    ghost: bool,
    /// The place this session's last launch created or adopted, when its
    /// profile resolved to a place backend. The launch path records it in
    /// `sandbox_instances` once the pane exists, so garbage collection can find
    /// the container; `None` for every policy-backed and unsandboxed session,
    /// which create nothing that outlives the process.
    place_instance: Option<crate::sandbox::SandboxInstance>,
    /// The bytes a placeholder's parser was seeded with (a ghost's saved frame,
    /// or the unreachable-host notice). Retained so [`Self::resize`] can
    /// **re-render** rather than `set_size`: vt100 resizes by truncating each
    /// row's cells, so narrowing a pane destroys every cell past the new width
    /// and widening back pads with blanks. A live session's agent repaints that
    /// away on SIGWINCH; a placeholder has no process to repaint it, so without
    /// the seed its content would be permanently clipped to the narrowest size
    /// the terminal ever hit. `None` for a live session, whose pane content is
    /// owned by the backend, not by us.
    placeholder_seed: Option<Vec<u8>>,
}

impl Session {
    /// Spawn a new session via the given backend.
    pub fn spawn(
        name: String,
        rows: u16,
        cols: u16,
        config: &SessionConfig,
        backend: &Arc<dyn SessionBackend>,
        provider: &Arc<dyn AgentProvider>,
    ) -> Result<Self> {
        let window_name = crate::agent::tmux::agent_window_name(&name);
        let Sandboxed {
            command,
            args,
            env,
            profile,
            state,
            egress,
            place,
            instance,
        } = sandboxed_invocation(config, provider)?;

        // A place is a transport, so a place-backed launch spawns *into* the
        // place rather than onto the backend the caller resolved from
        // `backend_type` — which for a first spawn does not name it yet, and
        // for a relaunch may name a container an edited profile has replaced.
        let in_place = place.as_ref().map(place_backend);
        let backend = in_place.as_ref().unwrap_or(backend);
        if in_place.is_some() {
            // The caller readied the backend the session's row named, which is
            // not this one: the place was ensured a moment ago and its tmux
            // server is inside it. Idempotent once the connection is up, and
            // this is where the *first* session in a place pays for starting
            // that server.
            ready_place(backend)?;
        }

        let spawned = backend.spawn(
            &window_name,
            &command,
            &args,
            config.cwd.as_deref(),
            &env,
            rows,
            cols,
        )?;
        // There is a pane now, so the boundary composed above has something to
        // be the boundary *of*. A spawn that failed dropped it instead.
        egress.commit();

        let mut info = SessionInfo::new(name);
        // Reuse the caller-supplied id when present (stable identity across a
        // respawn; matches the `FRIRING_SESSION` env injected before launch).
        if let Some(id) = config.session_id {
            info.id = id;
        }
        info.agent_session_id = config.agent_session_id.clone();
        info.cwd = config.cwd.clone();
        if !config.agent.is_empty() {
            info.agent = config.agent.clone();
        }
        info.backend_id = Some(spawned.backend_id.clone());
        info.remote_host = remote_host_from_backend(backend);
        info.sandbox_profile = profile;
        info.sandbox_state = state;
        debug!(session_id = %info.id, backend_id = %spawned.backend_id, "Spawned session via backend");

        let mut session = Self::wire_io(
            info,
            rows,
            cols,
            SessionIo {
                output: spawned.output,
                input: spawned.input,
                backend_id: spawned.backend_id,
                mode: WireMode::Spawn,
            },
            backend,
            provider,
            env,
        );
        session.place_instance = instance;
        Ok(session)
    }

    /// Reconnect to an existing backend session. `seed` is optional
    /// pre-captured scrollback (see [`SessionBackend::capture_history`]);
    /// `None` = the backend captures it during the adopt.
    #[allow(clippy::too_many_arguments)]
    pub fn adopt(
        name: String,
        rows: u16,
        cols: u16,
        backend_id: &str,
        backend: &Arc<dyn SessionBackend>,
        provider: &Arc<dyn AgentProvider>,
        env: HashMap<String, String>,
        seed: Option<Vec<u8>>,
    ) -> Result<Self> {
        let adopted = backend.adopt(backend_id, rows, cols, seed)?;

        debug!(
            backend_id = %backend_id,
            parser_rows = rows,
            parser_cols = cols,
            "Adopting session"
        );

        let mut info = SessionInfo::new(name);
        info.backend_id = Some(backend_id.to_string());
        info.remote_host = remote_host_from_backend(backend);
        debug!(session_id = %info.id, backend_id = %backend_id, "Adopted session via backend");

        Ok(Self::wire_io(
            info,
            rows,
            cols,
            SessionIo {
                output: adopted.output,
                input: adopted.input,
                backend_id: backend_id.to_string(),
                mode: WireMode::Adopt,
            },
            backend,
            provider,
            env,
        ))
    }

    /// Create parser, spawn reader/writer loops for the given I/O handles.
    fn wire_up(rows: u16, cols: u16, io: SessionIo) -> (WiredState, String) {
        let last_title = Arc::new(Mutex::new(None));
        let attention_at = Arc::new(AtomicU64::new(0));
        let notification = Arc::new(Mutex::new(None));
        let meta_gen = Arc::new(AtomicU64::new(0));
        let parser = Arc::new(Mutex::new(vt100::Parser::new_with_callbacks(
            rows,
            cols,
            crate::session::settings::global().scrollback_lines,
            TermSignals {
                title: Arc::clone(&last_title),
                attention_at: Arc::clone(&attention_at),
                notification: Arc::clone(&notification),
                meta_gen: Arc::clone(&meta_gen),
            },
        )));

        let exited = Arc::new(AtomicBool::new(false));
        let last_output_at = Arc::new(AtomicU64::new(initial_output_at(io.mode)));
        let osc52 = Arc::new(PaneClipboard::default());

        let (input_tx, input_rx) = mpsc::channel(INPUT_CHANNEL_CAPACITY);
        tokio::spawn(Self::writer_loop(io.input, input_rx));

        let parser_clone = Arc::clone(&parser);
        let exited_clone = Arc::clone(&exited);
        let last_output_clone = Arc::clone(&last_output_at);
        let osc52_clone = Arc::clone(&osc52);
        tokio::task::spawn_blocking(move || {
            Self::reader_loop(
                io.output,
                parser_clone,
                exited_clone,
                last_output_clone,
                osc52_clone,
            );
        });

        let state = WiredState {
            parser,
            input_tx,
            exited,
            last_output_at,
            last_title,
            attention_at,
            notification,
            meta_gen,
            osc52,
        };
        (state, io.backend_id)
    }

    /// Wire up parser, reader loop, and writer loop for a new session.
    fn wire_io(
        info: SessionInfo,
        rows: u16,
        cols: u16,
        io: SessionIo,
        backend: &Arc<dyn SessionBackend>,
        provider: &Arc<dyn AgentProvider>,
        env: HashMap<String, String>,
    ) -> Self {
        let (state, backend_id) = Self::wire_up(rows, cols, io);
        Self {
            info,
            parser: state.parser,
            input_tx: state.input_tx,
            backend_id,
            backend: Arc::clone(backend),
            provider: Arc::clone(provider),
            exited: state.exited,
            last_output_at: state.last_output_at,
            last_title: state.last_title,
            attention_at: state.attention_at,
            notification: state.notification,
            meta_gen: state.meta_gen,
            last_synced_meta_gen: u64::MAX,
            osc52: state.osc52,
            last_drained_osc52_gen: 0,
            attention_ack_at: 0,
            shell_pane: None,
            env,
            placeholder: false,
            ghost: false,
            place_instance: None,
            placeholder_seed: None,
        }
    }

    /// Build a **placeholder** session for a persisted remote session whose host
    /// is currently unreachable. It carries no live backend pane: the reader /
    /// writer loops are never spawned, `input_tx` is a dead channel (keystrokes
    /// are silently dropped), and the `parser` is seeded with a static notice.
    /// The row renders like any other (grouping/ordering/nesting all key off
    /// `info`) but shows `SessionStatus::Unreachable` until the host recovers and
    /// [`Self::adopt`] replaces it in place. `info.status` is forced to
    /// `Unreachable` here regardless of the caller's value.
    ///
    /// `backend_type` is what the session's row *says* it runs on, which is not
    /// always what `backend` is: a place friring has not opened has no transport
    /// in the registry, so the caller falls back to the local backend to have
    /// something renderable — and the pane would otherwise claim a remote host
    /// went away when a container did.
    #[allow(clippy::too_many_arguments)]
    pub fn placeholder(
        mut info: SessionInfo,
        rows: u16,
        cols: u16,
        backend: &Arc<dyn SessionBackend>,
        backend_type: &str,
        provider: &Arc<dyn AgentProvider>,
        env: HashMap<String, String>,
    ) -> Self {
        info.status = crate::session::SessionStatus::Unreachable;
        info.backend_id = None;

        let last_title = Arc::new(Mutex::new(None));
        let attention_at = Arc::new(AtomicU64::new(0));
        let notification = Arc::new(Mutex::new(None));
        let meta_gen = Arc::new(AtomicU64::new(0));
        let parser = Arc::new(Mutex::new(vt100::Parser::new_with_callbacks(
            rows.max(1),
            cols.max(1),
            crate::session::settings::global().scrollback_lines,
            TermSignals {
                title: Arc::clone(&last_title),
                attention_at: Arc::clone(&attention_at),
                notification: Arc::clone(&notification),
                meta_gen: Arc::clone(&meta_gen),
            },
        )));
        let notice = unreachable_notice(backend_type, info.remote_host.as_deref());
        if let Ok(mut p) = parser.lock() {
            p.process(notice.as_bytes());
        }

        // A dead input channel: the receiver is dropped immediately, so any
        // keystroke `try_send` fails fast and the byte is discarded.
        let (input_tx, _dead_rx) = mpsc::channel(INPUT_CHANNEL_CAPACITY);

        Self {
            info,
            parser,
            input_tx,
            backend_id: String::new(),
            backend: Arc::clone(backend),
            provider: Arc::clone(provider),
            exited: Arc::new(AtomicBool::new(false)),
            last_output_at: Arc::new(AtomicU64::new(0)),
            last_title,
            attention_at,
            notification,
            meta_gen,
            last_synced_meta_gen: u64::MAX,
            osc52: Arc::default(),
            last_drained_osc52_gen: 0,
            attention_ack_at: 0,
            shell_pane: None,
            env,
            placeholder: true,
            ghost: false,
            place_instance: None,
            placeholder_seed: Some(notice.into_bytes()),
        }
    }

    /// Build a **ghost** session: a placeholder whose agent process is
    /// deliberately not running (unloaded, or lazily restored). The parser is
    /// seeded with `frame` — the session's saved last frame, SGR-styled lines
    /// joined with `\r\n` — at `rows`×`cols`, so the frozen pane re-wraps to
    /// the *current* size; `None` (no frame ever captured) seeds a short
    /// notice instead. Keystrokes are dropped like any placeholder; the render
    /// layer greys the pane. `info.status` is forced to `Unloaded`;
    /// [`Self::restart`] turns the ghost back into a live session in place.
    pub fn ghost(
        info: SessionInfo,
        rows: u16,
        cols: u16,
        backend: &Arc<dyn SessionBackend>,
        provider: &Arc<dyn AgentProvider>,
        env: HashMap<String, String>,
        frame: Option<&[u8]>,
    ) -> Self {
        // `placeholder` forces `Unreachable`, so the status is set after it. Its
        // notice is replaced by the saved frame below, so which backend name it
        // was composed from makes no difference here.
        let mut session =
            Self::placeholder(info, rows, cols, backend, backend.name(), provider, env);
        session.info.status = crate::session::SessionStatus::Unloaded;
        session.ghost = true;
        // Re-seed the parser: replace the placeholder's "host unreachable"
        // notice with the saved frame. Frame bytes are SGR-styled lines (tmux
        // `capture-pane -e` / `rows_formatted`) — no OSC/BEL, so replaying
        // them can't fire the title/attention callbacks.
        let seed: Vec<u8> = match frame {
            Some(bytes) => bytes.to_vec(),
            None => "\r\n  \u{25CC} Session unloaded \u{2014} no saved preview.\r\n\r\n  \
                     Press Enter (or restart) to launch the agent and resume.\r\n"
                .as_bytes()
                .to_vec(),
        };
        session.seed_placeholder_parser(rows, cols, &seed);
        session.placeholder_seed = Some(seed);
        session
    }

    /// Rebuild this placeholder's parser at `rows`×`cols` and replay `seed`
    /// into it. Used both at construction and by [`Self::resize`] — a
    /// placeholder re-renders from its seed instead of `set_size`, which would
    /// truncate away every cell past a narrower width with no agent to repaint
    /// it back (see [`Self::placeholder_seed`]).
    ///
    fn seed_placeholder_parser(&self, rows: u16, cols: u16, seed: &[u8]) {
        let Ok(mut p) = self.parser.lock() else {
            return;
        };
        *p = vt100::Parser::new_with_callbacks(
            rows.max(1),
            cols.max(1),
            crate::session::settings::global().scrollback_lines,
            TermSignals {
                title: Arc::clone(&self.last_title),
                attention_at: Arc::clone(&self.attention_at),
                notification: Arc::clone(&self.notification),
                meta_gen: Arc::clone(&self.meta_gen),
            },
        );
        p.process(seed);
    }

    /// Whether this is a placeholder for an unreachable remote session (no live
    /// backend pane). See [`Self::placeholder`].
    pub fn is_placeholder(&self) -> bool {
        self.placeholder
    }

    /// Whether this is a **ghost** (unloaded / lazily-restored placeholder
    /// showing its saved frame). Ghosts are placeholders too — check this
    /// first where the two need different handling (load vs remote retry).
    pub fn is_ghost(&self) -> bool {
        self.ghost
    }

    /// Blocking read loop feeding the vt100 parser. Runs on a
    /// `spawn_blocking` thread and exits only on EOF/error from `reader`.
    /// Lifecycle contract: every path that retires a `Session` must call
    /// `kill()`/`detach()` so the backend unregisters the pane and this
    /// thread sees EOF — a silent drop leaks the thread (blocked in a read)
    /// for the process lifetime.
    fn reader_loop(
        mut reader: Box<dyn Read + Send>,
        parser: Arc<Mutex<SessionParser>>,
        exited: Arc<AtomicBool>,
        last_output_at: Arc<AtomicU64>,
        osc52: Arc<PaneClipboard>,
    ) {
        let mut buf = [0u8; 4096];
        // Bytes of a trailing, not-yet-complete UTF-8 character held back from
        // the previous read. The agent's output is a single byte stream, but
        // the OS read boundary (and tmux's `%output` framing) can fall in the
        // middle of a multi-byte character. vt100 is NOT robust to a `process()`
        // chunk that ends mid-codepoint — it can swallow a following control
        // byte (e.g. a newline), misplacing later output — so we never hand it a
        // truncated tail. `carry` is at most 3 bytes (a 4-byte char missing one).
        let mut carry: Vec<u8> = Vec::new();
        // Clipboard writes (OSC 52) are extracted here, from the same chunks
        // the parser gets — the parser can't surface them itself (its OSC
        // buffer truncates at 1 KiB; see `agent::osc52`). The stream is left
        // untouched: vt100 ignores the sequence harmlessly.
        let mut scanner = Osc52Scanner::default();
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    debug!("Session reader: EOF");
                    break;
                }
                Ok(n) => {
                    let mut data = std::mem::take(&mut carry);
                    data.extend_from_slice(&buf[..n]);
                    let ready = utf8_ready_prefix_len(&data);
                    carry = data.split_off(ready);
                    if !data.is_empty() {
                        for copy in scanner.scan(&data) {
                            osc52.push(copy);
                        }
                        if let Ok(mut p) = parser.lock() {
                            p.process(&data);
                        }
                    }
                    // Stamped *after* the parser saw the bytes: the ghost-frame
                    // debounce treats this as the dirty marker, so advancing it
                    // first would let a saver serialize the previous screen and
                    // then record it as up to date. Unconditional — a read that
                    // only extends `carry` is still output.
                    last_output_at.store(now_millis(), Ordering::Relaxed);
                }
                Err(e) => {
                    debug!("Session reader error: {e}");
                    break;
                }
            }
        }
        // Stream ended (EOF or error): flush any leftover partial UTF-8 sequence,
        // since no more bytes are coming to complete it.
        if !carry.is_empty() {
            for copy in scanner.scan(&carry) {
                osc52.push(copy);
            }
            if let Ok(mut p) = parser.lock() {
                p.process(&carry);
            }
        }
        exited.store(true, Ordering::SeqCst);
    }

    async fn writer_loop(mut writer: Box<dyn Write + Send>, mut input_rx: mpsc::Receiver<Vec<u8>>) {
        while let Some(data) = input_rx.recv().await {
            if let Err(e) = writer.write_all(&data) {
                error!("Session writer error: {e}");
                break;
            }
            if let Err(e) = writer.flush() {
                error!("Session flush error: {e}");
                break;
            }
        }
        debug!("Session writer task exiting");
    }

    pub fn send_input(&self, data: Vec<u8>) -> Result<()> {
        send_to_input_channel(&self.input_tx, data, "Session")
    }

    pub fn resize(&self, rows: u16, cols: u16) {
        // A cramped layout (tiny terminal + open panels/strips) can compute a
        // zero-row/col content area; vt100's `set_size` underflows on 0 and
        // tmux rejects it, so clamp at this boundary for every path below.
        let (rows, cols) = (rows.max(1), cols.max(1));
        // A placeholder has no live pane; only resize its local buffer.
        // Talking to the (possibly-down) backend here would issue a blocking
        // ssh resize on the UI thread — the freeze we're avoiding.
        if self.placeholder {
            match &self.placeholder_seed {
                // Re-render from the seed rather than `set_size`, which
                // truncates each row's cells: with no agent to repaint it, a
                // shrink would clip the frozen content for good, so growing
                // back showed bare background where the pane's text had been.
                Some(seed) => self.seed_placeholder_parser(rows, cols, seed),
                None => {
                    if let Ok(mut parser) = self.parser.lock() {
                        parser.screen_mut().set_size(rows, cols);
                    }
                }
            }
            return;
        }
        if let Err(e) = self.backend.resize(&self.backend_id, rows, cols) {
            tracing::warn!("Failed to resize session: {e}");
            return;
        }
        if let Ok(mut parser) = self.parser.lock() {
            parser.screen_mut().set_size(rows, cols);
        }
        if let Some(shell) = &self.shell_pane {
            if let Err(e) = self.backend.resize(&shell.backend_id, rows, cols) {
                tracing::warn!("Failed to resize shell pane: {e}");
                return;
            }
            if let Ok(mut parser) = shell.parser.lock() {
                parser.screen_mut().set_size(rows, cols);
            }
        }
    }

    pub fn has_exited(&self) -> bool {
        self.exited.load(Ordering::SeqCst)
    }

    /// Force the session into the "process exited" state, for tests that need to
    /// exercise the exited → `Idle` status branch.
    #[cfg(test)]
    pub fn mark_exited_for_test(&self) {
        self.exited.store(true, Ordering::SeqCst);
    }

    /// Backdate the session's last-output timestamp by `ms`, for tests that need
    /// to exercise the output-quiescence fallback (a stuck `working` state going
    /// quiet → `Idle`).
    #[cfg(test)]
    pub fn backdate_output_for_test(&self, ms: u64) {
        let now = now_millis();
        self.last_output_at
            .store(now.saturating_sub(ms), Ordering::Relaxed);
    }

    pub fn millis_since_last_output(&self) -> u64 {
        now_millis().saturating_sub(self.last_output_at.load(Ordering::Relaxed))
    }

    /// Raw monotonic timestamp (epoch millis) of the session's last output.
    /// Monotonic non-decreasing — the reader thread only ever stores `now`.
    /// Used by the render loop's cheap output-change detector
    /// ([`crate::app::App::detect_output_redraw`]) so it can spot new output
    /// without locking the vt100 parser.
    pub fn last_output_at(&self) -> u64 {
        self.last_output_at.load(Ordering::Relaxed)
    }

    /// Latest OSC window title the agent emitted, if any (live activity text).
    pub fn agent_title(&self) -> Option<String> {
        self.last_title.lock().ok().and_then(|t| t.clone())
    }

    /// Read the agent's title + notification **only when they changed** since
    /// the last call: `None` means unchanged (reuse the previously-synced
    /// values), `Some` carries the fresh pair. The reader thread bumps a
    /// generation counter on every write (`TermSignals::meta_gen`), so the
    /// ~100 Hz status refresh pays one atomic load per session instead of two
    /// mutex locks + two `String` clones (ADR-P10). A generation observed
    /// before its write completes only delays the sync by one ~10 ms tick —
    /// the counter is bumped *after* the value write, never before.
    pub fn sync_agent_meta(&mut self) -> Option<(Option<String>, Option<String>)> {
        let gen = self.meta_gen.load(Ordering::Acquire);
        if gen == self.last_synced_meta_gen {
            return None;
        }
        self.last_synced_meta_gen = gen;
        Some((self.agent_title(), self.notification()))
    }

    /// Clipboard writes captured from this session's panes (agent + shell)
    /// since the last drain, each paired with its global capture sequence —
    /// programs inside a pane setting the clipboard via OSC 52 (Claude Code's
    /// `/copy`, nvim's OSC 52 provider, …; see [`crate::agent::osc52`]). The
    /// app sorts by that sequence across sessions and routes each through the
    /// same clipboard stack as every other copy surface. Gen-gated like
    /// [`Self::sync_agent_meta`]: the every-tick, nothing-new case is one
    /// atomic load per pane (ADR-P10).
    pub fn drain_osc52_copies(&mut self) -> Vec<(u64, String)> {
        let mut copies = self.osc52.drain_new(&mut self.last_drained_osc52_gen);
        if let Some(shell) = self.shell_pane.as_mut() {
            copies.extend(shell.osc52.drain_new(&mut shell.last_drained_osc52_gen));
        }
        copies
    }

    /// Simulate a reader-thread title/notification write for the ADR-P10
    /// perf tests (mirrors [`Self::mark_exited_for_test`]).
    #[cfg(test)]
    pub(crate) fn bump_meta_gen_for_test(&self, title: &str) {
        if let Ok(mut guard) = self.last_title.lock() {
            *guard = Some(title.to_string());
        }
        self.meta_gen.fetch_add(1, Ordering::Release);
    }

    /// Whether the agent has signalled for attention (bell / OSC 9 / OSC 777)
    /// since it was last acknowledged. Cleared via [`Self::acknowledge_attention`].
    pub fn needs_attention(&self) -> bool {
        self.attention_at.load(Ordering::Relaxed) > self.attention_ack_at
    }

    /// Message text from the latest attention notification, if any.
    pub fn notification(&self) -> Option<String> {
        self.notification.lock().ok().and_then(|n| n.clone())
    }

    /// Acknowledge any pending attention signal (called while the session is
    /// the active/selected one — the user is already looking at it).
    pub fn acknowledge_attention(&mut self) {
        self.attention_ack_at = now_millis();
    }

    /// Return the backend-specific session identifier.
    pub fn backend_id(&self) -> &str {
        &self.backend_id
    }

    /// Return the backend name.
    pub fn backend_name(&self) -> &str {
        self.backend.name()
    }

    /// The backend this session is wired to, for a caller that has to register
    /// it: a place-backed launch builds its own transport
    /// ([`crate::agent::transport::Place`]) rather than taking one from the
    /// registry, so the registry only learns about it from here.
    pub fn backend_arc(&self) -> &Arc<dyn SessionBackend> {
        &self.backend
    }

    /// The place this session's last launch created or adopted, if any — the
    /// row to record in `sandbox_instances` now that the pane exists.
    pub fn place_instance(&self) -> Option<&crate::sandbox::SandboxInstance> {
        self.place_instance.as_ref()
    }

    /// The session's current environment — the env it was last (re)spawned
    /// with. Used by acceptance tests to assert the identity env (`FRIRING_*`)
    /// is preserved across a restart.
    #[cfg(test)]
    pub(crate) fn env(&self) -> &HashMap<String, String> {
        &self.env
    }

    /// Return the PID of the process running in this session's backend pane.
    pub fn pane_pid(&self) -> Result<Option<u32>> {
        self.backend.pane_pid(&self.backend_id)
    }

    /// Clone the backend handle + id so a background task can query the pane
    /// PID (a control-mode round-trip, slow for remote SSH backends) off the UI
    /// thread. The backend is `Send + Sync`, so the clone is cheap to move.
    pub fn backend_handle(&self) -> (Arc<dyn SessionBackend>, String) {
        (Arc::clone(&self.backend), self.backend_id.clone())
    }

    /// Replace the provider [`Self::restart`] rebuilds its launch args from.
    ///
    /// A session adopted at startup stores a provider built from the plain
    /// registry def; a later restart must use a def resolved (and, for a remote
    /// backend, arg-adapted) *now* — otherwise the relaunch would resurrect
    /// local config paths the host can't see.
    pub fn set_provider(&mut self, provider: Arc<dyn AgentProvider>) {
        self.provider = provider;
    }

    /// Restart the session: kill the old pane, spawn a fresh one with new config.
    ///
    /// Uses the agent's resume args (when defined) so it picks up the
    /// existing conversation instead of starting fresh. On a **ghost** this is
    /// the *load* path: there is no pane to kill, and success clears the
    /// placeholder/ghost flags — the frozen frame is simply replaced by the
    /// live stream, in place.
    pub fn restart(&mut self, config: &SessionConfig, rows: u16, cols: u16) -> Result<()> {
        // Resolve the wrapped invocation *before* tearing the old pane down:
        // applying a sandbox profile can fail (backend unavailable with
        // fallback off, a policy this build can't express, an I/O error writing
        // the profile), and a healthy session must survive that.
        //
        // Which is also why `egress` is a *provisional* boundary held across
        // the kill and the spawn: composing binds a fresh proxy, and until this
        // relaunch has a pane the session's agent — still running if the kill
        // failed — must keep the instance it was launched with.
        let window_name = crate::agent::tmux::agent_window_name(&self.info.name);
        let Sandboxed {
            command,
            args,
            env,
            profile,
            state: sandbox_state,
            egress,
            place,
            instance,
        } = sandboxed_invocation(config, &self.provider)?;

        // A relaunch re-reads the profile, so an edited one asks for a *new*
        // container and this session moves into it. The old pane is killed
        // where it still is, and the new one spawned where the launch says.
        let in_place = place.as_ref().map(place_backend);
        let moved = in_place
            .as_ref()
            .is_some_and(|next| next.name() != self.backend.name());
        // The other direction — a profile edited from a place backend to a
        // policy one, or off a place entirely — has no home to relaunch into:
        // this session's tmux is *inside* the place, and a policy backend's
        // argv (`sandbox-exec …`, `bwrap …`) names host binaries a container
        // image does not have. Refusing says so; relaunching would kill the
        // pane and put a dead one in its place.
        if in_place.is_none() && crate::session::is_sandbox_backend(self.backend.name()) {
            bail!(
                "This session runs inside sandbox place '{}', and its profile no longer resolves \
                 to a place. Create a new session to move it back onto the host.",
                self.backend.name()
            );
        }

        // A placeholder/ghost owns no live pane — killing its empty backend_id
        // would only produce a tmux error.
        if !self.placeholder {
            match self.backend.kill(&self.backend_id) {
                Ok(()) => {}
                // The pane lived in a place this launch is not going back to —
                // a rebuilt container, or one that died and took every session
                // in it. Not reaching it is the outcome, not a failure; a
                // relaunch that refused here would strand the session in a
                // place that no longer exists.
                Err(e) if moved => {
                    warn!(session_id = %self.info.id, "Old sandbox place is gone: {e:#}");
                }
                Err(e) => return Err(e),
            }
        }

        let backend = in_place.as_ref().unwrap_or(&self.backend);
        if in_place.is_some() {
            ready_place(backend)?;
        }
        let spawned = backend.spawn(
            &window_name,
            &command,
            &args,
            config.cwd.as_deref(),
            &env,
            rows,
            cols,
        )?;
        // The relaunch has a pane: the boundary composed for it replaces the
        // one the retired pane was using, and that one is shut down. Every
        // failure above dropped it instead, leaving the session's own alone.
        egress.commit();

        let (state, backend_id) = Self::wire_up(
            rows,
            cols,
            SessionIo {
                output: spawned.output,
                input: spawned.input,
                backend_id: spawned.backend_id,
                mode: WireMode::Spawn,
            },
        );

        self.backend_id = backend_id;
        self.parser = state.parser;
        self.input_tx = state.input_tx;
        self.exited = state.exited;
        self.last_output_at = state.last_output_at;
        // Adopt the fresh reader loop's clipboard queue (and reset the drain
        // gate): keeping the old pane's queue would silently drop every OSC 52
        // copy the restarted pane makes.
        self.osc52 = state.osc52;
        self.last_drained_osc52_gen = 0;
        // Same for the metadata cells: `wire_up` wired the new parser's
        // TermSignals to *its* cells, so the old ones belong to the retired
        // reader — keeping them would strand every OSC title, notification and
        // attention signal the restarted pane emits. Generations reset to the
        // fresh-session values `wire_io` uses.
        self.last_title = state.last_title;
        self.attention_at = state.attention_at;
        self.notification = state.notification;
        self.meta_gen = state.meta_gen;
        self.last_synced_meta_gen = u64::MAX;
        self.attention_ack_at = 0;
        self.env = env;
        // Only now, past every fallible step: a restart that failed leaves the
        // session pointing at the pane it still has.
        if let Some(next) = in_place {
            self.backend = next;
            self.info.remote_host = remote_host_from_backend(&self.backend);
        }
        self.place_instance = instance;
        self.info.backend_id = Some(self.backend_id.clone());
        self.info.sandbox_profile = profile;
        self.info.sandbox_state = sandbox_state;
        if !config.agent.is_empty() {
            self.info.agent = config.agent.clone();
        }
        if self.placeholder {
            // A ghost just became a live session: hand the status back to the
            // hook pipeline (fresh spawns start as Working until a hook says
            // otherwise, matching `SessionInfo::new`).
            self.placeholder = false;
            self.ghost = false;
            self.info.status = crate::session::SessionStatus::Working;
        }

        debug!(session_id = %self.info.id, backend_id = %self.backend_id, "Restarted session");
        Ok(())
    }

    /// Serialize the pane's **visible screen** as a ghost frame: SGR-styled
    /// lines joined with `\r\n` (the tmux-seed shape, so it re-parses at any
    /// pane size), trailing blank rows trimmed, attrs reset per row so one
    /// row's colors can't bleed into the next on replay. Pure in-memory read —
    /// no tmux round-trip — which is what makes it safe on the crash-safety
    /// debounce and for remote sessions at shutdown. Returns
    /// `(rows, cols, bytes)`; `None` only on a poisoned parser lock.
    pub fn serialize_visible_frame(&self) -> Option<(u16, u16, Vec<u8>)> {
        let parser = self.parser.lock().ok()?;
        let screen = parser.screen();
        let (rows, cols) = screen.size();
        let mut lines: Vec<Vec<u8>> = screen.rows_formatted(0, cols).collect();
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        Some((rows, cols, lines.join(&b"\x1b[0m\r\n"[..])))
    }

    /// Capture the ghost frame for an unload/shutdown: the pane's visible
    /// screen via the backend (an independent `capture-pane` subprocess, whose
    /// output keeps logical lines — see [`SessionBackend::capture_visible`]),
    /// falling back to the in-memory serialization when the capture fails.
    /// Returns `(rows, cols, bytes)`.
    pub fn capture_unload_frame(&self) -> Option<(u16, u16, Vec<u8>)> {
        match self.backend.capture_visible(&self.backend_id) {
            Ok(seed) if !seed.is_empty() => {
                let (rows, cols) = self
                    .parser
                    .lock()
                    .ok()
                    .map(|p| p.screen().size())
                    .unwrap_or((0, 0));
                Some((rows, cols, seed))
            }
            Ok(_) => self.serialize_visible_frame(),
            Err(e) => {
                tracing::warn!("Ghost-frame capture failed, saving visible screen only: {e}");
                self.serialize_visible_frame()
            }
        }
    }

    /// Kill/destroy the backend session (for Ctrl+X close).
    pub fn kill(&self) {
        if let Err(e) = self.kill_checked() {
            tracing::warn!("Failed to kill session: {e}");
        }
    }

    /// [`Self::kill`] that **reports** a failed agent-pane teardown instead of
    /// only logging it. Unload needs this: it must not swap in a ghost — a row
    /// that says the agent process is gone — while that process is in fact
    /// still running and still holding its memory. Delete keeps the
    /// fire-and-forget [`Self::kill`], where a stale pane is cosmetic.
    ///
    /// The companion shell pane stays best-effort and is torn down first, as
    /// in `kill`: it is the agent pane whose death the caller gates on.
    pub fn kill_checked(&self) -> Result<()> {
        // A placeholder owns no live backend pane (see `placeholder`).
        if self.placeholder {
            return Ok(());
        }
        self.kill_shell_pane();
        self.backend.kill(&self.backend_id)
    }

    /// Detach from the backend session without killing it (for Ctrl+Q quit).
    pub fn detach(self) {
        // A placeholder owns no live backend pane — detaching would issue a
        // blocking ssh call (possibly to a down host) for nothing.
        if self.placeholder {
            return;
        }
        if let Some(shell) = &self.shell_pane {
            if let Err(e) = self.backend.detach(&shell.backend_id) {
                tracing::warn!("Failed to detach shell pane: {e}");
            }
        }
        if let Err(e) = self.backend.detach(&self.backend_id) {
            tracing::warn!("Failed to detach session: {e}");
        }
        drop(self.input_tx);
        debug!("Session detached");
    }

    /// Lazily spawn a companion shell pane.
    ///
    /// `cwd` is the directory the shell starts in — the caller passes the
    /// session's *launch* cwd (the multi-repo symlink workspace when there is
    /// one, so the shell lands where the agent does), falling back to the
    /// primary repo (`info.cwd`) when `None`. The command is the backend's
    /// [`SessionBackend::default_shell`]. The window name uses the `tbs-` prefix
    /// to distinguish from the agent's `tb-` windows.
    pub fn ensure_shell_pane(
        &mut self,
        rows: u16,
        cols: u16,
        cwd: Option<&std::path::Path>,
    ) -> Result<()> {
        if self.shell_pane.is_some() {
            return Ok(());
        }

        let shell_cmd = self.backend.default_shell();
        let window_name = crate::agent::tmux::shell_window_name(&self.info.name);

        let env = self.env.clone();
        let cwd = cwd.or(self.info.cwd.as_deref());

        let spawned = self
            .backend
            .spawn(&window_name, &shell_cmd, &[], cwd, &env, rows, cols)?;

        let (state, backend_id) = Self::wire_up(
            rows,
            cols,
            SessionIo {
                output: spawned.output,
                input: spawned.input,
                backend_id: spawned.backend_id,
                mode: WireMode::Spawn,
            },
        );

        self.info.shell_backend_id = Some(backend_id.clone());
        self.shell_pane = Some(ShellPane::from_wired(state, backend_id));

        debug!(session_id = %self.info.id, "Spawned shell pane");
        Ok(())
    }

    /// Re-adopt an existing shell pane from a backend_id (for restore on restart).
    pub fn adopt_shell_pane(&mut self, backend_id: &str, rows: u16, cols: u16) -> Result<()> {
        let adopted = self.backend.adopt(backend_id, rows, cols, None)?;

        let (state, bid) = Self::wire_up(
            rows,
            cols,
            SessionIo {
                output: adopted.output,
                input: adopted.input,
                backend_id: backend_id.to_string(),
                mode: WireMode::Adopt,
            },
        );

        self.info.shell_backend_id = Some(bid.clone());
        self.shell_pane = Some(ShellPane::from_wired(state, bid));

        debug!(session_id = %self.info.id, backend_id = %backend_id, "Adopted shell pane");
        Ok(())
    }

    /// Kill the shell pane if it exists.
    fn kill_shell_pane(&self) {
        if let Some(shell) = &self.shell_pane {
            if let Err(e) = self.backend.kill(&shell.backend_id) {
                tracing::warn!("Failed to kill shell pane: {e}");
            }
        }
    }

    /// Create a lightweight stub for unit tests (no real backend process).
    #[cfg(test)]
    pub fn stub(
        name: &str,
        backend: &Arc<dyn SessionBackend>,
        provider: &Arc<dyn AgentProvider>,
    ) -> Self {
        Self::stub_with_input_rx(name, backend, provider).0
    }

    /// Like [`Self::stub`], but also returns the input-channel receiver so a
    /// test can inspect bytes the app sends to the PTY. The caller must keep
    /// the receiver alive for `send_input` to succeed.
    #[cfg(test)]
    pub fn stub_with_input_rx(
        name: &str,
        backend: &Arc<dyn SessionBackend>,
        provider: &Arc<dyn AgentProvider>,
    ) -> (Self, mpsc::Receiver<Vec<u8>>) {
        let (input_tx, input_rx) = mpsc::channel(INPUT_CHANNEL_CAPACITY);
        // Wire TermSignals to the session's accessor cells exactly like
        // `wire_up`, so bytes injected via `feed_output_for_test` drive
        // `agent_title`/`needs_attention` the same way live PTY output does.
        let last_title = Arc::new(Mutex::new(None));
        let attention_at = Arc::new(AtomicU64::new(0));
        let notification = Arc::new(Mutex::new(None));
        let meta_gen = Arc::new(AtomicU64::new(0));
        let session = Self {
            info: SessionInfo::new(name.to_string()),
            parser: Arc::new(Mutex::new(vt100::Parser::new_with_callbacks(
                24,
                80,
                0,
                TermSignals {
                    title: Arc::clone(&last_title),
                    attention_at: Arc::clone(&attention_at),
                    notification: Arc::clone(&notification),
                    meta_gen: Arc::clone(&meta_gen),
                },
            ))),
            input_tx,
            backend_id: String::new(),
            backend: Arc::clone(backend),
            provider: Arc::clone(provider),
            exited: Arc::new(AtomicBool::new(false)),
            last_output_at: Arc::new(AtomicU64::new(now_millis())),
            last_title,
            attention_at,
            notification,
            meta_gen,
            last_synced_meta_gen: u64::MAX,
            osc52: Arc::default(),
            last_drained_osc52_gen: 0,
            attention_ack_at: 0,
            shell_pane: None,
            env: HashMap::new(),
            placeholder: false,
            ghost: false,
            place_instance: None,
            placeholder_seed: None,
        };
        (session, input_rx)
    }

    /// Feed raw agent-output bytes into the session exactly as the reader loop
    /// would: bump `last_output_at` and run the bytes through the vt100 parser
    /// (firing `TermSignals` callbacks). This is the test seam for everything
    /// downstream of PTY output — terminal rendering, the output-change redraw
    /// detector, OSC title/bell signals, buffer-content search, and OSC 52
    /// clipboard capture (per call: a sequence must complete within one feed;
    /// the live reader's scanner persists across reads).
    #[cfg(test)]
    pub fn feed_output_for_test(&self, bytes: &[u8]) {
        // Strictly-increasing bump: two feeds within the same millisecond must
        // still read as *new* output to `App::detect_output_redraw`'s signature.
        let prev = self.last_output_at.load(Ordering::Relaxed);
        self.last_output_at
            .store(now_millis().max(prev + 1), Ordering::Relaxed);
        for copy in Osc52Scanner::default().scan(bytes) {
            self.osc52.push(copy);
        }
        if let Ok(mut p) = self.parser.lock() {
            p.process(bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A backend scripted for one outcome, counting `kill` and recording the
    /// environment a `spawn` was handed.
    ///
    /// The environment is where a launch's boundary names itself — the port is
    /// inside `HTTP_PROXY` — so recording it is what lets a test prove the
    /// listener a failed launch composed is *gone*, rather than merely
    /// uncommitted.
    struct ScriptedBackend {
        kills: Arc<AtomicU64>,
        kill_fails: bool,
        /// A spawn that fails is what the launch-failure tests need; one that
        /// succeeds hands back a pane whose output ends immediately.
        spawn_succeeds: bool,
        spawn_env: Arc<Mutex<HashMap<String, String>>>,
    }

    impl ScriptedBackend {
        /// Kills succeed, spawns fail, nothing recorded yet.
        fn new() -> Self {
            Self {
                kills: Arc::new(AtomicU64::new(0)),
                kill_fails: false,
                spawn_succeeds: false,
                spawn_env: Arc::default(),
            }
        }
    }

    impl SessionBackend for ScriptedBackend {
        fn name(&self) -> &str {
            "scripted"
        }
        fn check_available(&self) -> Result<()> {
            Ok(())
        }
        fn ensure_ready(&self) -> Result<()> {
            Ok(())
        }
        fn spawn(
            &self,
            _: &str,
            _: &str,
            _: &[String],
            _: Option<&Path>,
            env: &HashMap<String, String>,
            _: u16,
            _: u16,
        ) -> Result<SpawnedSession> {
            if let Ok(mut recorded) = self.spawn_env.lock() {
                recorded.clone_from(env);
            }
            if !self.spawn_succeeds {
                anyhow::bail!("stub backend does not spawn");
            }
            Ok(SpawnedSession {
                backend_id: "%scripted".to_string(),
                output: Box::new(std::io::empty()),
                input: Box::new(std::io::sink()),
            })
        }
        fn adopt(&self, _: &str, _: u16, _: u16, _: Option<Vec<u8>>) -> Result<AdoptedSession> {
            anyhow::bail!("stub backend does not adopt")
        }
        fn discover(&self) -> Result<Vec<DiscoveredSession>> {
            Ok(vec![])
        }
        fn resize(&self, _: &str, _: u16, _: u16) -> Result<()> {
            Ok(())
        }
        fn is_dead(&self, _: &str) -> Result<bool> {
            Ok(false)
        }
        fn kill(&self, _: &str) -> Result<()> {
            self.kills.fetch_add(1, Ordering::SeqCst);
            if self.kill_fails {
                anyhow::bail!("stub backend cannot kill that pane");
            }
            Ok(())
        }
        fn detach(&self, _: &str) -> Result<()> {
            Ok(())
        }
        fn pane_pid(&self, _: &str) -> Result<Option<u32>> {
            Ok(None)
        }
    }

    /// Applying a profile is fallible, so it has to happen before the old pane
    /// is killed — otherwise a condition friring can detect up front destroys a
    /// healthy session.
    #[test]
    fn restart_keeps_the_pane_when_the_sandbox_cannot_be_applied() {
        let kills = Arc::new(AtomicU64::new(0));
        let backend: Arc<dyn SessionBackend> = Arc::new(ScriptedBackend {
            kills: Arc::clone(&kills),
            ..ScriptedBackend::new()
        });
        let provider: Arc<dyn AgentProvider> = Arc::new(crate::agent::GenericProvider::new(
            crate::agent::agent_config::builtin_registry()
                .default_agent()
                .unwrap()
                .clone(),
        ));
        let mut session = Session::stub("boxed", &backend, &provider);

        // A place backend is not in this build, and the profile refuses to fall
        // back to an unsandboxed launch — so the wrap fails.
        let mut profile = crate::session::SandboxProfile::new(
            "dev",
            vec![crate::session::SandboxPath::workspace("~/dev/app")],
        );
        profile.backend = crate::session::SandboxBackendKind::Docker;
        let config = SessionConfig {
            sandbox: Some(profile),
            ..SessionConfig::default()
        };

        assert!(session.restart(&config, 24, 80).is_err());
        assert_eq!(
            kills.load(Ordering::SeqCst),
            0,
            "the existing pane must still be alive"
        );
    }

    fn default_provider() -> Arc<dyn AgentProvider> {
        Arc::new(crate::agent::GenericProvider::new(
            crate::agent::agent_config::builtin_registry()
                .default_agent()
                .unwrap()
                .clone(),
        ))
    }

    /// A profile pinned to a place backend: not in this build on any host, so
    /// the decision is the same wherever the suite runs.
    fn unappliable_profile(fallback: bool) -> crate::session::SandboxProfile {
        let mut profile = crate::session::SandboxProfile::new(
            "dev",
            vec![crate::session::SandboxPath::workspace("~/dev/app")],
        );
        profile.backend = crate::session::SandboxBackendKind::WslDistro;
        profile.allow_unsandboxed_fallback = fallback;
        profile
    }

    /// The escape hatch firing must not erase the session's link to its
    /// profile. The profile is the *desired* boundary — persisted, and all a
    /// later relaunch has to rebuild from — while `sandbox_state` is what the
    /// indicators read. Reporting the fallback as "no profile" dropped the link
    /// for good and left every later launch unsandboxed.
    #[test]
    fn a_fallback_launch_keeps_the_desired_profile_and_says_why_it_is_not_applied() {
        let provider = default_provider();
        let config = SessionConfig {
            sandbox: Some(unappliable_profile(true)),
            ..SessionConfig::default()
        };

        let out = sandboxed_invocation(&config, &provider).unwrap();

        assert_eq!(out.profile.as_deref(), Some("dev"));
        let Some(SandboxState::Unenforced(reason)) = out.state.as_ref() else {
            panic!("expected an unenforced boundary, got {:?}", out.state);
        };
        assert!(reason.contains("wsl-distro"), "{reason}");
        // Nothing wrapped the agent, so the indicator must not claim a boundary.
        assert_eq!(out.command, provider.command());
        assert!(!out.state.as_ref().unwrap().is_applied());
    }

    /// A session that asked for no boundary records neither half, so the
    /// indicators stay silent for the overwhelmingly common case.
    #[test]
    fn an_unsandboxed_session_records_no_sandbox_state() {
        let provider = default_provider();
        let out = sandboxed_invocation(&SessionConfig::default(), &provider).unwrap();
        assert!(out.profile.is_none());
        assert!(out.state.is_none());
    }

    /// Because the profile survives a fallback, the next launch tries the
    /// boundary again: on a host with a policy backend it is applied, on one
    /// with none it is refused. What it is never again is silently on the host.
    #[test]
    fn the_launch_after_a_fallback_tries_the_boundary_again() {
        let provider = default_provider();
        let mut profile = unappliable_profile(false);
        // The same session, its profile now resolving down this host's ladder
        // rather than naming a backend that will never exist.
        profile.backend = crate::session::SandboxBackendKind::Auto;
        let config = SessionConfig {
            sandbox: Some(profile),
            // As a real spawn always does. The default profile's `allowlist`
            // needs the egress proxy, and a boundary with no session to be
            // named by is refused rather than sharing the fallback key.
            session_id: Some(crate::session::SessionId::default()),
            ..SessionConfig::default()
        };

        let host_has_a_backend = crate::sandbox::SandboxHost::local_shared()
            .select(crate::session::SandboxBackendKind::Auto)
            .chosen
            .is_some();
        match sandboxed_invocation(&config, &provider) {
            Ok(out) => {
                assert!(host_has_a_backend, "a host with no backend must not wrap");
                assert_eq!(out.profile.as_deref(), Some("dev"));
                assert!(
                    out.state.as_ref().is_some_and(SandboxState::is_applied),
                    "{:?}",
                    out.state
                );
                assert_ne!(out.command, provider.command());
            }
            Err(e) => assert!(
                !host_has_a_backend,
                "this host offers a backend, so the wrap should have applied: {e:#}"
            ),
        }
    }

    /// Everything the egress tests below need to be about the boundary rather
    /// than about the machine they run on: a fabricated data directory, and a
    /// host that offers seatbelt whether or not this one does. Held together
    /// because dropping either mid-test would move the ground under the launch.
    struct EgressFixture {
        _paths: crate::paths::TestPathGuard,
        _host: crate::agent::sandboxing::TestSandboxHost,
        _dir: tempfile::TempDir,
        session_id: crate::session::SessionId,
        /// Where a scripted spawn records the environment it was handed.
        spawn_env: Arc<Mutex<HashMap<String, String>>>,
    }

    impl EgressFixture {
        fn new() -> Self {
            let dir = tempfile::TempDir::new().expect("a private test directory");
            Self {
                _paths: crate::paths::TestPathGuard::new(dir.path()),
                _host: crate::agent::sandboxing::TestSandboxHost::seatbelt(),
                _dir: dir,
                session_id: crate::session::SessionId::default(),
                spawn_env: Arc::default(),
            }
        }

        fn key(&self) -> String {
            self.session_id.to_string()
        }

        /// A session pinned to this fixture's id, filtered to one domain — so
        /// the composition binds a real proxy, and the rule it enforces says
        /// which instance a later assertion is looking at.
        fn config(&self, allow: &str) -> SessionConfig {
            let mut profile = crate::session::SandboxProfile::new(
                "dev",
                vec![crate::session::SandboxPath::workspace(
                    "/fabricated/dev/app",
                )],
            );
            profile.network_allow = vec![allow.to_string()];
            SessionConfig {
                session_id: Some(self.session_id),
                sandbox: Some(profile),
                ..SessionConfig::default()
            }
        }

        /// The proxy already serving this session, as one launched earlier
        /// would have left it: committed, listening, and enforcing `allow`.
        fn running_proxy(&self, allow: &str) -> u16 {
            let config = self.config(allow);
            let policy = config
                .sandbox
                .as_ref()
                .expect("the fixture's profile")
                .resolve(
                    crate::session::SandboxBackendKind::Seatbelt,
                    "/fabricated/home",
                )
                .expect("a valid profile");
            let scratch =
                crate::sandbox::create_session_scratch(&self.key()).expect("a scratch directory");
            let grant = crate::sandbox::egress::establish(
                &self.key(),
                &policy,
                crate::sandbox::ProxyTransport::Loopback,
                &scratch,
            )
            .expect("the session's own proxy binds");
            match grant.endpoint {
                crate::sandbox::ProxyEndpoint::Loopback { port } => port,
                other => panic!("expected a loopback endpoint, got {other:?}"),
            }
        }

        fn backend(&self, scripted: ScriptedBackend) -> Arc<dyn SessionBackend> {
            Arc::new(ScriptedBackend {
                spawn_env: Arc::clone(&self.spawn_env),
                ..scripted
            })
        }

        /// The port the launch was composed against, read out of the
        /// environment the spawn was handed.
        fn composed_proxy_port(&self) -> u16 {
            let env = self.spawn_env.lock().expect("the recorded environment");
            let url = env
                .get("HTTP_PROXY")
                .unwrap_or_else(|| panic!("no HTTP_PROXY in the spawn environment: {env:?}"));
            url.rsplit(':')
                .next()
                .and_then(|port| port.parse().ok())
                .unwrap_or_else(|| panic!("no port in {url}"))
        }
    }

    /// Wait for every egress command queued so far to have been handled, and
    /// answer with the rules the session's own instance is enforcing: the
    /// supervisor takes one command at a time, so its reply is proof the
    /// earlier ones are done.
    fn settled_rules(session_key: &str) -> Option<Vec<String>> {
        crate::sandbox::egress::running_allow_rules(session_key)
    }

    fn listening(port: u16) -> bool {
        std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
    }

    /// A launch binds its proxy before it has a pane, so a spawn that fails
    /// must take the proxy with it. What it left behind before was a listener
    /// and a live credential belonging to a session that never existed.
    #[test]
    fn a_spawn_that_fails_leaves_no_listener_behind() {
        let fixture = EgressFixture::new();
        let backend = fixture.backend(ScriptedBackend::new());
        let config = fixture.config("api.anthropic.com");

        let Err(err) = Session::spawn(
            "boxed".to_string(),
            24,
            80,
            &config,
            &backend,
            &default_provider(),
        ) else {
            panic!("the scripted backend refuses to spawn");
        };
        assert!(err.to_string().contains("does not spawn"), "{err:#}");

        assert_eq!(
            settled_rules(&fixture.key()),
            None,
            "a session that never existed was given a boundary"
        );
        let port = fixture.composed_proxy_port();
        assert!(
            !listening(port),
            "the proxy composed for a session that never existed is still listening"
        );
    }

    /// The restart half, and the worse one: composing happens *before* the kill
    /// precisely so a healthy session survives a composition failure — which
    /// means a kill that fails leaves the old agent running. Its way out must
    /// still be there.
    #[test]
    fn a_restart_whose_kill_fails_leaves_the_running_agent_its_egress() {
        let fixture = EgressFixture::new();
        let old_port = fixture.running_proxy("old.example");
        let backend = fixture.backend(ScriptedBackend {
            kill_fails: true,
            ..ScriptedBackend::new()
        });
        let mut session = Session::stub("boxed", &backend, &default_provider());

        let err = session
            .restart(&fixture.config("new.example"), 24, 80)
            .expect_err("the scripted backend cannot kill the pane");
        assert!(err.to_string().contains("cannot kill"), "{err:#}");

        assert_eq!(
            settled_rules(&fixture.key()),
            Some(vec!["old.example".to_string()]),
            "the still-running agent's boundary was replaced by a relaunch that never happened"
        );
        assert!(
            listening(old_port),
            "the still-running agent lost its way out"
        );
        crate::sandbox::egress::stop(&fixture.key());
    }

    /// And when the kill succeeds but the spawn does not: the session keeps the
    /// instance it had — the next relaunch replaces it — and the one composed
    /// for the launch that failed is gone.
    #[test]
    fn a_restart_whose_spawn_fails_keeps_the_boundary_it_had() {
        let fixture = EgressFixture::new();
        let old_port = fixture.running_proxy("old.example");
        let backend = fixture.backend(ScriptedBackend::new());
        let mut session = Session::stub("boxed", &backend, &default_provider());

        session
            .restart(&fixture.config("new.example"), 24, 80)
            .expect_err("the scripted backend refuses to spawn");

        assert_eq!(
            settled_rules(&fixture.key()),
            Some(vec!["old.example".to_string()])
        );
        assert!(listening(old_port));
        assert!(
            !listening(fixture.composed_proxy_port()),
            "the proxy composed for a pane that never spawned is still listening"
        );
        crate::sandbox::egress::stop(&fixture.key());
    }

    /// The success path, which is what makes the two above more than "never
    /// commit anything": a relaunch that reaches its pane takes over, exactly
    /// once, and the instance it replaced is shut down.
    #[tokio::test]
    async fn a_restart_that_succeeds_replaces_the_boundary_exactly_once() {
        let fixture = EgressFixture::new();
        let old_port = fixture.running_proxy("old.example");
        let backend = fixture.backend(ScriptedBackend {
            spawn_succeeds: true,
            ..ScriptedBackend::new()
        });
        let mut session = Session::stub("boxed", &backend, &default_provider());
        let config = fixture.config("new.example");

        session
            .restart(&config, 24, 80)
            .expect("the relaunch spawns");

        assert_eq!(
            settled_rules(&fixture.key()),
            Some(vec!["new.example".to_string()]),
            "the session is still on the retired agent's boundary"
        );
        assert!(listening(fixture.composed_proxy_port()));
        assert!(!listening(old_port), "the replaced instance kept running");
        assert!(
            !crate::agent::sandboxing::pending_egress(&config).is_pending(),
            "the relaunch left a second instance behind it"
        );
        crate::sandbox::egress::stop(&fixture.key());
    }

    #[test]
    fn pane_clipboard_drops_oldest_and_drains_gen_gated() {
        let pc = PaneClipboard::default();
        let mut seen = 0;
        assert!(pc.drain_new(&mut seen).is_empty());
        for i in 0..PaneClipboard::CAP + 3 {
            pc.push(format!("c{i}"));
        }
        let copies = pc.drain_new(&mut seen);
        // Drop-oldest under the cap: the newest write must survive to win the
        // clipboard.
        assert_eq!(copies.len(), PaneClipboard::CAP);
        assert_eq!(
            copies.last().unwrap().1,
            format!("c{}", PaneClipboard::CAP + 2)
        );
        assert!(pc.drain_new(&mut seen).is_empty());
    }

    #[test]
    fn input_channel_overflow_fails_fast_without_blocking() {
        let (tx, _rx) = mpsc::channel(INPUT_CHANNEL_CAPACITY);
        for _ in 0..INPUT_CHANNEL_CAPACITY {
            send_to_input_channel(&tx, vec![b'x'], "Session").unwrap();
        }
        let err = send_to_input_channel(&tx, vec![b'x'], "Session").unwrap_err();
        assert!(err.to_string().contains("full"), "got: {err}");
    }

    #[test]
    fn input_channel_closed_reports_closed() {
        let (tx, rx) = mpsc::channel::<Vec<u8>>(INPUT_CHANNEL_CAPACITY);
        drop(rx);
        let err = send_to_input_channel(&tx, vec![b'x'], "Session").unwrap_err();
        assert!(err.to_string().contains("closed"), "got: {err}");
    }

    #[test]
    fn now_millis_returns_reasonable_value() {
        let ms = now_millis();
        // Should be after 2024-01-01 (1704067200000 ms since epoch).
        assert!(ms > 1_704_067_200_000);
    }

    #[test]
    fn utf8_ready_prefix_passes_complete_input() {
        assert_eq!(utf8_ready_prefix_len(b""), 0);
        assert_eq!(utf8_ready_prefix_len(b"hello"), 5);
        // "é" = c3 a9, complete.
        assert_eq!(utf8_ready_prefix_len(&[b'a', 0xc3, 0xa9]), 3);
        // "你好" complete (two 3-byte chars).
        assert_eq!(utf8_ready_prefix_len("你好".as_bytes()), 6);
    }

    #[test]
    fn utf8_ready_prefix_holds_back_truncated_tail() {
        // Lone 2-byte lead → hold all of it.
        assert_eq!(utf8_ready_prefix_len(&[b'a', 0xc3]), 1);
        // 3-byte lead with one continuation, missing one → hold the two.
        assert_eq!(utf8_ready_prefix_len(&[b'x', 0xe4, 0xbd]), 1);
        // 4-byte lead alone, and with 1 and 2 continuations → all held.
        assert_eq!(utf8_ready_prefix_len(&[b'x', 0xf0]), 1);
        assert_eq!(utf8_ready_prefix_len(&[b'x', 0xf0, 0x9f]), 1);
        assert_eq!(utf8_ready_prefix_len(&[b'x', 0xf0, 0x9f, 0x8e]), 1);
        // Same 4-byte char, fully present → nothing held.
        assert_eq!(utf8_ready_prefix_len(&[b'x', 0xf0, 0x9f, 0x8e, 0x89]), 5);
        // The realistic read-boundary case: a complete "é" (c3 a9) followed by
        // the lead byte of the next char → hold only that fresh lead.
        assert_eq!(utf8_ready_prefix_len(&[0xc3, 0xa9, 0xe6]), 2);
    }

    #[test]
    fn utf8_ready_prefix_does_not_buffer_garbage() {
        // Continuation bytes with no lead in the last 3 → pass through (no
        // unbounded carry).
        assert_eq!(utf8_ready_prefix_len(&[0x80, 0x80, 0x80, 0x80]), 4);
    }

    /// Property/regression tests for the reader-loop UTF-8 carry: feeding the
    /// vt100 parser through `utf8_ready_prefix_len`-bounded chunks must render
    /// identically to feeding the whole stream, for any chunking — proving
    /// friring's read boundaries can never glitch valid agent output. vt100 on
    /// its own does NOT have this property (it can swallow a newline that
    /// follows a mid-codepoint chunk boundary); the carry is what restores it.
    mod utf8_chunking {
        use proptest::prelude::*;

        use super::utf8_ready_prefix_len;

        /// vt100 screen as normalized, right-trimmed visible rows.
        fn rows(p: &vt100::Parser) -> Vec<String> {
            let s = p.screen();
            let (r, c) = s.size();
            (0..r)
                .map(|y| {
                    let mut t = String::new();
                    for x in 0..c {
                        let sym = s.cell(y, x).map(|cl| cl.contents()).unwrap_or_default();
                        t.push_str(if sym.is_empty() { " " } else { sym });
                    }
                    t.trim_end().to_string()
                })
                .collect()
        }

        fn whole(bytes: &[u8]) -> Vec<String> {
            let mut p = vt100::Parser::new(10, 38, 0);
            p.process(bytes);
            rows(&p)
        }

        /// Replays the reader-loop carry logic over `bytes` cut into `sizes`.
        fn carry_chunked(bytes: &[u8], sizes: &[usize]) -> Vec<String> {
            let mut p = vt100::Parser::new(10, 38, 0);
            let mut carry: Vec<u8> = Vec::new();
            let (mut pos, mut i) = (0usize, 0usize);
            while pos < bytes.len() {
                let sz = sizes.get(i % sizes.len()).copied().unwrap_or(1).max(1);
                let end = (pos + sz).min(bytes.len());
                let mut data = std::mem::take(&mut carry);
                data.extend_from_slice(&bytes[pos..end]);
                let ready = utf8_ready_prefix_len(&data);
                carry = data.split_off(ready);
                assert!(carry.len() <= 3, "carry must stay bounded");
                if !data.is_empty() {
                    p.process(&data);
                }
                pos = end;
                i += 1;
            }
            if !carry.is_empty() {
                p.process(&carry);
            }
            rows(&p)
        }

        /// The exact minimal case that exposed the vt100 mid-codepoint bug:
        /// "f" + lead of "é" delivered in one read, then "é"-tail + "\n日本語"
        /// in the next. Without the carry the newline is swallowed and 日本語
        /// lands on the wrong row.
        #[test]
        fn regression_midcodepoint_newline_widechars() {
            // f é \n 日本語. Chunked [2, 100] so the first read ends on "f" plus
            // the lead byte of "é" and the rest arrives next.
            let bytes = b"f\xc3\xa9\n\xe6\x97\xa5\xe6\x9c\xac\xe8\xaa\x9e";
            assert_eq!(carry_chunked(bytes, &[2, 100]), whole(bytes));
        }

        /// Strategy producing valid-UTF-8 agent output: text, CSI/OSC escapes,
        /// wide/combining chars, newlines.
        fn valid_utf8_output() -> impl Strategy<Value = Vec<u8>> {
            let token = prop_oneof![
                proptest::string::string_regex("[ -~]{0,8}")
                    .unwrap()
                    .prop_map(String::into_bytes),
                (
                    proptest::string::string_regex("[0-9;]{0,6}").unwrap(),
                    prop::sample::select(vec![b'm', b'H', b'J', b'K', b'A', b'B']),
                )
                    .prop_map(|(params, fin)| {
                        let mut v = vec![0x1b, b'['];
                        v.extend(params.bytes());
                        v.push(fin);
                        v
                    }),
                proptest::string::string_regex("[ -~]{0,8}")
                    .unwrap()
                    .prop_map(|s| {
                        let mut v = vec![0x1b, b']'];
                        v.extend(s.bytes());
                        v.push(0x07);
                        v
                    }),
                prop::sample::select(vec!["你好", "🎉", "café", "日本語", "→★", "a\u{0301}"])
                    .prop_map(|s| s.as_bytes().to_vec()),
                prop::sample::select(vec![b'\n', b'\r', b'\t', 0x08]).prop_map(|b| vec![b]),
            ];
            prop::collection::vec(token, 0..40).prop_map(|tokens| tokens.concat())
        }

        proptest! {
            /// For valid UTF-8, the carry makes vt100 rendering independent of how
            /// the byte stream is chunked across reads — the core guarantee that
            /// friring's transport/read boundaries never corrupt agent output.
            #[test]
            fn carry_makes_chunking_invariant(
                bytes in valid_utf8_output(),
                sizes in prop::collection::vec(1usize..40, 1..16),
            ) {
                prop_assert_eq!(carry_chunked(&bytes, &sizes), whole(&bytes));
            }
        }
    }

    /// Where a scrolled-off line goes when a scrolling region is set — the
    /// contract that decides whether an agent pane can be scrolled at all.
    ///
    /// Agents that run on the alternate screen (Claude Code) handle the wheel
    /// themselves, but one built on ratatui's *inline* viewport (Codex CLI)
    /// grows its transcript on the normal screen by pinning a `DECSTBM` region
    /// and scrolling inside it. Stock vt100 0.16.2 drops every line that leaves
    /// such a region, so those panes had permanently empty scrollback and
    /// Shift+Up, the wheel and the scrollbar all did nothing. Friring builds
    /// against a fork that restores the real-terminal rule instead — see
    /// `[patch.crates-io]` in Cargo.toml. These tests pin that rule, so a
    /// dependency bump back onto stock vt100 fails here rather than silently
    /// un-scrolling every Codex session.
    mod inline_viewport_scrollback {
        /// How many lines of history the parser actually holds. `set_scrollback`
        /// clamps to what exists, so asking for more than any buffer could hold
        /// reads its true depth back (same trick as `ui::terminal_view`).
        fn depth(rows: u16, cols: u16, bytes: &[u8]) -> usize {
            let mut p = vt100::Parser::new(rows, cols, 100);
            p.process(bytes);
            p.screen_mut().set_scrollback(usize::MAX);
            p.screen().scrollback()
        }

        /// The oldest `n` lines of history, as text.
        fn oldest(rows: u16, cols: u16, bytes: &[u8], n: usize) -> Vec<String> {
            let mut p = vt100::Parser::new(rows, cols, 100);
            p.process(bytes);
            p.screen_mut().set_scrollback(usize::MAX);
            p.screen()
                .contents()
                .lines()
                .take(n)
                .map(str::to_string)
                .collect()
        }

        /// A screen whose every row is labelled, so a line that reaches
        /// scrollback can be told apart from a blank one.
        fn labelled(rows: u16) -> Vec<u8> {
            (1..=rows)
                .flat_map(|r| format!("\x1b[{r};1Hrow{r:02}").into_bytes())
                .collect()
        }

        /// The shape recorded from codex-cli 0.146.0: a region anchored at row
        /// 1 and ending above the composer, scrolled up with `SU`. The lines
        /// that leave it left the top of the screen, so they are history.
        #[test]
        fn top_anchored_region_keeps_what_leaves_the_screen() {
            let mut stream = labelled(10);
            stream.extend_from_slice(b"\x1b[1;8r\x1b[3S\x1b[r");
            assert_eq!(depth(10, 40, &stream), 3);
            assert_eq!(oldest(10, 40, &stream, 3), ["row01", "row02", "row03"]);
        }

        /// Same region, scrolled by writing at its last row — how the viewport
        /// grows one transcript line at a time.
        #[test]
        fn top_anchored_region_keeps_history_on_linefeed() {
            let mut stream = labelled(10);
            stream.extend_from_slice(b"\x1b[1;8r\x1b[8;1H\n\n\n\x1b[r");
            assert_eq!(depth(10, 40, &stream), 3);
            assert_eq!(oldest(10, 40, &stream, 3), ["row01", "row02", "row03"]);
        }

        /// A region that starts *below* row 1 scrolls mid-screen: those lines
        /// never crossed the top edge, so they are not history and must still
        /// be discarded. (tmux keeps them; xterm doesn't, and neither do we —
        /// the fix is deliberately the narrow one.)
        #[test]
        fn region_below_the_top_row_still_discards() {
            let mut stream = labelled(10);
            stream.extend_from_slice(b"\x1b[4;10r\x1b[3S\x1b[r");
            assert_eq!(depth(10, 40, &stream), 0);
        }

        /// The ordinary case every other agent and the shell pane rely on:
        /// no region at all, newlines at the bottom row.
        #[test]
        fn plain_screen_scroll_still_keeps_history() {
            let mut stream = labelled(10);
            stream.extend_from_slice(b"\x1b[10;1H\n\n\n");
            assert_eq!(depth(10, 40, &stream), 3);
            assert_eq!(oldest(10, 40, &stream, 3), ["row01", "row02", "row03"]);
        }
    }

    /// The ghost-frame capture path: the backend's capture must win over the
    /// in-memory screen (it keeps logical lines, which re-wrap better), and
    /// every failure mode must still yield the visible screen, never nothing.
    mod ghost_frame_capture {
        use std::sync::atomic::AtomicUsize;

        use super::*;

        /// Capture outcome scripted per test case.
        enum Capture {
            Seed(&'static [u8]),
            Empty,
            Fail,
        }

        /// Backend that records how many times it was asked to capture.
        struct CaptureBackend {
            calls: Arc<AtomicUsize>,
            capture: Capture,
        }

        impl SessionBackend for CaptureBackend {
            fn name(&self) -> &str {
                "capture-stub"
            }
            fn check_available(&self) -> Result<()> {
                Ok(())
            }
            fn ensure_ready(&self) -> Result<()> {
                Ok(())
            }
            fn spawn(
                &self,
                _: &str,
                _: &str,
                _: &[String],
                _: Option<&Path>,
                _: &HashMap<String, String>,
                _: u16,
                _: u16,
            ) -> Result<SpawnedSession> {
                anyhow::bail!("capture stub does not spawn")
            }
            fn adopt(&self, _: &str, _: u16, _: u16, _: Option<Vec<u8>>) -> Result<AdoptedSession> {
                anyhow::bail!("capture stub does not adopt")
            }
            fn capture_visible(&self, _: &str) -> Result<Vec<u8>> {
                self.calls.fetch_add(1, Ordering::Relaxed);
                match self.capture {
                    Capture::Seed(bytes) => Ok(bytes.to_vec()),
                    Capture::Empty => Ok(Vec::new()),
                    Capture::Fail => anyhow::bail!("capture failed"),
                }
            }
            fn discover(&self) -> Result<Vec<DiscoveredSession>> {
                Ok(Vec::new())
            }
            fn resize(&self, _: &str, _: u16, _: u16) -> Result<()> {
                Ok(())
            }
            fn is_dead(&self, _: &str) -> Result<bool> {
                Ok(false)
            }
            fn kill(&self, _: &str) -> Result<()> {
                Ok(())
            }
            fn detach(&self, _: &str) -> Result<()> {
                Ok(())
            }
            fn pane_pid(&self, _: &str) -> Result<Option<u32>> {
                Ok(None)
            }
        }

        /// Session on a `CaptureBackend`, with `on screen` in its parser, plus
        /// the cell counting capture calls.
        fn session_with(capture: Capture) -> (Session, Arc<AtomicUsize>) {
            let calls = Arc::new(AtomicUsize::new(0));
            let backend: Arc<dyn SessionBackend> = Arc::new(CaptureBackend {
                calls: Arc::clone(&calls),
                capture,
            });
            let provider: Arc<dyn AgentProvider> = Arc::new(crate::agent::GenericProvider::new(
                crate::agent::agent_config::builtin_registry()
                    .default_agent()
                    .unwrap()
                    .clone(),
            ));
            let session = Session::stub("cap", &backend, &provider);
            session.feed_output_for_test(b"on screen\r\n");
            (session, calls)
        }

        #[test]
        fn prefers_the_backend_capture_over_the_in_memory_screen() {
            let (session, calls) = session_with(Capture::Seed(b"captured screen"));

            let (_, _, bytes) = session.capture_unload_frame().expect("frame");

            assert_eq!(calls.load(Ordering::Relaxed), 1);
            assert_eq!(bytes, b"captured screen");
            assert!(
                !String::from_utf8_lossy(&bytes).contains("on screen"),
                "the capture seed replaces the visible screen, it is not appended"
            );
        }

        #[test]
        fn falls_back_to_the_visible_screen_when_the_capture_yields_nothing() {
            for capture in [Capture::Empty, Capture::Fail] {
                let (session, _) = session_with(capture);

                let (_, _, bytes) = session.capture_unload_frame().expect("frame");

                assert!(
                    String::from_utf8_lossy(&bytes).contains("on screen"),
                    "expected the serialized visible screen, got {:?}",
                    String::from_utf8_lossy(&bytes)
                );
                let (_, _, visible) = session.serialize_visible_frame().expect("visible frame");
                assert_eq!(bytes, visible);
            }
        }
    }

    /// ADR-26's stated consequence, in the one place a user meets it: the two
    /// off-host shapes lose sessions differently, and a frozen pane has to say
    /// which — three panes dying together is one problem, not three.
    #[test]
    fn an_unreachable_pane_says_which_shape_went() {
        let place = unreachable_notice("sandbox:dev", None);
        assert!(place.contains("Sandbox place 'dev'"), "{place}");
        assert!(place.contains("they all stopped"), "{place}");
        assert!(!place.contains("Remote host"), "{place}");

        let host = unreachable_notice("ssh:devbox", Some("devbox"));
        assert!(host.contains("Remote host 'devbox'"), "{host}");
        assert!(!host.contains("Sandbox place"), "{host}");

        // A local backend has no host name to fall back on, and must not
        // invent one.
        assert!(unreachable_notice("local-tmux", None).contains("'?'"));
    }

    /// A place is created once per profile and shared, so one transport reaches
    /// it however many sessions are in it — and a rebuild retires the one that
    /// reached the container it replaced.
    #[test]
    fn one_transport_per_place_and_a_rebuild_replaces_it() {
        let first =
            crate::agent::transport::Place::new("/usr/bin/podman", "ctr1", "shared").unwrap();
        let again =
            crate::agent::transport::Place::new("/usr/bin/podman", "ctr1", "shared").unwrap();
        assert!(Arc::ptr_eq(&place_backend(&first), &place_backend(&again)));

        let rebuilt =
            crate::agent::transport::Place::new("/usr/bin/podman", "ctr2", "shared").unwrap();
        let rebuilt = place_backend(&rebuilt);
        assert!(!Arc::ptr_eq(&place_backend(&first), &rebuilt));
        // Still one name, so the registry entry is replaced rather than
        // duplicated.
        assert_eq!(rebuilt.name(), "sandbox:shared");
    }

    #[test]
    fn remote_host_from_backend_strips_ssh_and_wsl_prefixes() {
        let host = crate::session::HostDef {
            name: "devbox".into(),
            destination: "me@devbox".into(),
            ..Default::default()
        };
        let ssh: Arc<dyn SessionBackend> =
            Arc::new(crate::agent::tmux::TmuxBackend::from_host(&host));
        assert_eq!(remote_host_from_backend(&ssh).as_deref(), Some("devbox"));

        let wsl: Arc<dyn SessionBackend> = Arc::new(crate::agent::tmux::TmuxBackend::from_host(
            &crate::session::HostDef::wsl("Ubuntu"),
        ));
        assert_eq!(remote_host_from_backend(&wsl).as_deref(), Some("Ubuntu"));

        let local: Arc<dyn SessionBackend> = Arc::new(crate::agent::tmux::TmuxBackend::local());
        assert_eq!(remote_host_from_backend(&local), None);
    }

    #[test]
    fn wire_mode_adopt_starts_stale_spawn_starts_fresh() {
        // Mirrors `App::refresh_session_statuses`: a session is `Busy` while
        // `now - last_output_at <= ACTIVITY_TIMEOUT_MS` (1000 ms in app/mod.rs).
        const ACTIVITY_TIMEOUT_MS: u64 = 1000;

        // Adopt: stale timestamp so the post-adopt SIGWINCH repaint doesn't
        // read as activity → NOT busy.
        let adopt = initial_output_at(WireMode::Adopt);
        assert!(now_millis().saturating_sub(adopt) > ACTIVITY_TIMEOUT_MS);

        // Spawn: "now" so a fresh process counts as active → busy.
        let spawn = initial_output_at(WireMode::Spawn);
        assert!(now_millis().saturating_sub(spawn) <= ACTIVITY_TIMEOUT_MS);
    }

    #[test]
    fn title_capture_extracts_osc_title() {
        let title = Arc::new(Mutex::new(None));
        let mut parser = vt100::Parser::new_with_callbacks(
            24,
            80,
            0,
            TermSignals {
                title: Arc::clone(&title),
                ..Default::default()
            },
        );
        // OSC 2 (set window title), BEL-terminated.
        parser.process(b"\x1b]2;working on tests\x07");
        assert_eq!(title.lock().unwrap().as_deref(), Some("working on tests"));

        // OSC 0 (set icon name + title) updates it too.
        parser.process(b"\x1b]0;done\x07");
        assert_eq!(title.lock().unwrap().as_deref(), Some("done"));

        // An empty title clears the cell rather than storing "".
        parser.process(b"\x1b]2;\x07");
        assert_eq!(*title.lock().unwrap(), None);
    }

    #[test]
    fn attention_signals_are_captured() {
        let attention_at = Arc::new(AtomicU64::new(0));
        let notification = Arc::new(Mutex::new(None));
        let mut parser = vt100::Parser::new_with_callbacks(
            24,
            80,
            0,
            TermSignals {
                attention_at: Arc::clone(&attention_at),
                notification: Arc::clone(&notification),
                ..Default::default()
            },
        );

        assert_eq!(attention_at.load(Ordering::Relaxed), 0);

        // Terminal bell → attention, no message.
        parser.process(b"\x07");
        assert!(attention_at.load(Ordering::Relaxed) > 0);
        assert_eq!(*notification.lock().unwrap(), None);

        // OSC 9 desktop notification → attention + message text.
        parser.process(b"\x1b]9;Claude is waiting for your input\x07");
        assert_eq!(
            notification.lock().unwrap().as_deref(),
            Some("Claude is waiting for your input")
        );

        // OSC 777 notify form → attention + joined title/body.
        parser.process(b"\x1b]777;notify;Claude;Task done\x07");
        assert_eq!(
            notification.lock().unwrap().as_deref(),
            Some("Claude: Task done")
        );
    }
}

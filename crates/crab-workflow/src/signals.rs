//! Child process supervision — timeouts, graceful shutdown, signal
//! forwarding, and stdout/stderr streaming.
//!
//! [`ChildSupervisor`] spawns a prepared [`tokio::process::Command`],
//! forwards parent cancellation down to the child process tree,
//! enforces a per-stage `timeout` by escalating through
//! `SIGTERM → graceful_shutdown_timeout → SIGKILL`, and streams the
//! child's stdout and stderr to durable sinks concurrently:
//!
//! 1. The parent's own tty when output mirroring is enabled.
//! 2. An in-memory ring buffer of the last N bytes of stderr, used
//!    by the journal for failure reporting (8 KiB by default).
//! 3. A per-stage log file at
//!    `.crab/workflow/runs/<run_id>/stage-<name>.log`, containing
//!    interleaved stdout + stderr lines.
//!
//! Unix children run in a dedicated process group. Windows uses the native
//! `taskkill /T` process-tree operation, with `/F` reserved for escalation.
//! The supervisor deliberately does **not** touch the journal. It
//! returns a [`SupervisorOutcome`] and emits structured
//! [`SupervisorEvent`]s through an optional callback; the executor
//! (task 1.11) maps those events into journal transitions.

use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::Duration;

use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time::{Instant, sleep};
use tracing::{debug, warn};

use crate::{Result, WorkflowError as CrabError};

/// Default tail size preserved for failure reporting. Matches the
/// `stderr_tail` column width in the workflow journal schema.
pub const DEFAULT_STDERR_TAIL_BYTES: usize = 8 * 1024;

/// Default grace window between SIGTERM and SIGKILL when escalating
/// a timeout or a parent cancellation. Matches the default value of
/// the `graceful_shutdown_timeout_secs` workflow config key.
pub const DEFAULT_GRACEFUL_SHUTDOWN: Duration = Duration::from_secs(10);

/// Reason we sent a signal to the child. Used by the executor to
/// map escalations into the right journal transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscalationReason {
    /// Stage-level `timeout` fired.
    Timeout,
    /// Parent received SIGINT (Ctrl-C) and forwarded it.
    ParentSigint,
    /// Parent received SIGTERM and forwarded it.
    ParentSigterm,
    /// Child did not exit within `graceful_shutdown_timeout` after a
    /// prior SIGTERM, so we escalated to SIGKILL.
    GracefulShutdownExpired,
    /// A caller-created kill request asked the supervisor to stop
    /// the child from outside this process.
    ExternalKillRequest,
}

/// Lifecycle events the supervisor emits for executor-side wiring.
///
/// The executor turns each event into a journal transition; the
/// supervisor itself is journal-agnostic so it can be unit-tested
/// without touching SQLite.
#[derive(Debug, Clone)]
pub enum SupervisorEvent {
    /// Child was spawned and started reading I/O.
    Started { pid: u32 },
    /// A signal was sent to the child.
    SignalSent {
        signal: Signal,
        reason: EscalationReason,
    },
    /// Child terminated. The [`SupervisorOutcome`] is the final result.
    Exited(SupervisorOutcome),
}

/// POSIX signals the supervisor sends. Kept as a small enum so the
/// event payload stays portable (integer numbers leak across OS and
/// ABI boundaries).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// SIGINT — polite "please stop".
    Int,
    /// SIGTERM — standard termination request.
    Term,
    /// SIGKILL — unconditional termination; cannot be caught.
    Kill,
}

impl Signal {
    /// Numeric signal value, for the journal payload and for error
    /// messages.
    #[cfg(unix)]
    pub fn as_raw(self) -> i32 {
        match self {
            Signal::Int => libc::SIGINT,
            Signal::Term => libc::SIGTERM,
            Signal::Kill => libc::SIGKILL,
        }
    }

    /// Numeric signal value, for the journal payload and for error
    /// messages.
    #[cfg(not(unix))]
    pub fn as_raw(self) -> i32 {
        match self {
            Signal::Int => 2,
            Signal::Term => 15,
            Signal::Kill => 9,
        }
    }

    #[cfg(unix)]
    fn as_nix(self) -> nix::sys::signal::Signal {
        match self {
            Signal::Int => nix::sys::signal::Signal::SIGINT,
            Signal::Term => nix::sys::signal::Signal::SIGTERM,
            Signal::Kill => nix::sys::signal::Signal::SIGKILL,
        }
    }
}

/// Final result of supervising a child to completion.
#[derive(Debug, Clone)]
pub struct SupervisorOutcome {
    /// The child's exit status. `None` iff the child was killed
    /// before we could reap it — shouldn't happen on Unix but kept
    /// as `Option` for safety.
    pub exit_status: Option<ExitStatus>,
    /// Terminating signal number if the child was killed by one.
    /// Distinct from `exit_status.code()`, which is `None` in that
    /// case but doesn't carry the signal number on its own
    /// portably.
    pub signal: Option<i32>,
    /// `true` iff a stage-level `timeout` fired and we escalated.
    pub timed_out: bool,
    /// Last `stderr_tail_bytes` of stderr produced by the child,
    /// truncated to a valid UTF-8 boundary (non-UTF-8 bytes
    /// replaced with `U+FFFD`). Empty if the child produced no
    /// stderr.
    pub stderr_tail: String,
}

/// Callback invoked on every [`SupervisorEvent`]. The executor
/// typically forwards these straight into the journal.
pub type EventSink = Arc<dyn Fn(SupervisorEvent) + Send + Sync>;

/// Configuration for a single supervised run.
pub struct ChildSupervisor {
    command: Command,
    log_path: PathBuf,
    timeout: Option<Duration>,
    graceful_shutdown: Duration,
    stderr_tail_bytes: usize,
    event_sink: Option<EventSink>,
    mirror_output: bool,
    external_kill_path: Option<PathBuf>,
    /// When set, stdout is also written verbatim to this path (for
    /// `OutKind::Stdout` capture). The file is created/truncated at
    /// spawn time.
    stdout_capture_path: Option<PathBuf>,
    stdout_capture_append: bool,
}

impl ChildSupervisor {
    /// Build a supervisor for `command`, writing interleaved
    /// stdout/stderr to `log_path`. The command's stdout and stderr
    /// are overwritten to [`Stdio::piped`] so the supervisor can
    /// fan them out; any previous redirection the caller configured
    /// is dropped.
    pub fn new(command: Command, log_path: PathBuf) -> Self {
        Self {
            command,
            log_path,
            timeout: None,
            graceful_shutdown: DEFAULT_GRACEFUL_SHUTDOWN,
            stderr_tail_bytes: DEFAULT_STDERR_TAIL_BYTES,
            event_sink: None,
            mirror_output: true,
            external_kill_path: None,
            stdout_capture_path: None,
            stdout_capture_append: false,
        }
    }

    /// Apply a per-stage timeout. On expiry the supervisor sends
    /// SIGTERM, waits `graceful_shutdown_timeout`, then sends
    /// SIGKILL, and the outcome reports `timed_out: true`.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Override the grace window between SIGTERM and SIGKILL.
    /// Defaults to [`DEFAULT_GRACEFUL_SHUTDOWN`].
    #[must_use]
    pub fn with_graceful_shutdown(mut self, graceful: Duration) -> Self {
        self.graceful_shutdown = graceful;
        self
    }

    /// Override the stderr tail window. Set to `0` to disable the
    /// ring buffer entirely.
    #[must_use]
    pub fn with_stderr_tail_bytes(mut self, bytes: usize) -> Self {
        self.stderr_tail_bytes = bytes;
        self
    }

    /// Register a callback for lifecycle events.
    #[must_use]
    pub fn with_event_sink(mut self, sink: EventSink) -> Self {
        self.event_sink = Some(sink);
        self
    }

    /// Control whether child stdout/stderr are mirrored to the
    /// parent process in addition to being written to the log file.
    #[must_use]
    pub fn with_output_mirroring(mut self, mirror: bool) -> Self {
        self.mirror_output = mirror;
        self
    }

    /// Stop the child when a kill request file appears at `path`.
    #[must_use]
    pub fn with_external_kill_path(mut self, path: PathBuf) -> Self {
        self.external_kill_path = Some(path);
        self
    }

    /// Capture stdout verbatim to the given path. Used by
    /// `OutKind::Stdout` to write the command's stdout directly to
    /// the declared output file.
    #[must_use]
    pub fn with_stdout_capture(mut self, path: PathBuf) -> Self {
        self.stdout_capture_path = Some(path);
        self
    }

    /// Append stdout to the capture file instead of truncating it.
    #[must_use]
    pub fn with_stdout_capture_append(mut self) -> Self {
        self.stdout_capture_append = true;
        self
    }

    /// Run the child to completion, forwarding signals and
    /// enforcing the timeout.
    pub async fn run(mut self) -> Result<SupervisorOutcome> {
        // Fan out stdout / stderr through pipes the supervisor owns.
        self.command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        self.command.process_group(0);

        if let Some(parent) = self.log_path.parent() {
            fs::create_dir_all(parent).await.map_err(CrabError::Io)?;
        }
        let log_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
            .await
            .map_err(CrabError::Io)?;
        let log_file = Arc::new(Mutex::new(log_file));

        let mut child = self.command.spawn().map_err(CrabError::Io)?;
        let pid = child.id().ok_or_else(|| {
            // `Child::id` returns None only after the child has been
            // reaped via `wait`. We just spawned, so this is a bug
            // in the runtime, not the user's problem.
            CrabError::Internal("child had no PID immediately after spawn".to_owned())
        })?;

        // Takes ownership of the pipes; the child keeps only the
        // other end.
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CrabError::Internal("child stdout missing after spawn".to_owned()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| CrabError::Internal("child stderr missing after spawn".to_owned()))?;

        let tail = Arc::new(Mutex::new(RingTail::new(self.stderr_tail_bytes)));
        let stdout_capture = if let Some(ref cap_path) = self.stdout_capture_path {
            if let Some(parent) = cap_path.parent() {
                fs::create_dir_all(parent).await.map_err(CrabError::Io)?;
            }
            let mut options = OpenOptions::new();
            options.create(true).write(true);
            if self.stdout_capture_append {
                options.append(true);
            } else {
                options.truncate(true);
            }
            let f = options.open(cap_path).await.map_err(CrabError::Io)?;
            Some(Arc::new(Mutex::new(f)))
        } else {
            None
        };
        let stdout_task = tokio::spawn(pump_stdout(
            stdout,
            Arc::clone(&log_file),
            stdout_capture,
            self.mirror_output,
        ));
        let stderr_task = tokio::spawn(pump_stderr(
            stderr,
            Arc::clone(&log_file),
            Arc::clone(&tail),
            self.mirror_output,
        ));
        self.emit(SupervisorEvent::Started { pid });

        let outcome = self.supervise(&mut child, pid).await;

        // Drain I/O tasks before returning: the child is gone, its
        // pipes have closed, so these complete promptly. Errors
        // writing to the log file are warnings, not hard failures —
        // a failed log write should never block a successful run
        // from reporting.
        if let Err(e) = stdout_task.await {
            warn!(error = %e, "workflow supervisor: stdout pump panicked");
        }
        if let Err(e) = stderr_task.await {
            warn!(error = %e, "workflow supervisor: stderr pump panicked");
        }

        let stderr_tail = {
            let guard = tail.lock().await;
            guard.snapshot()
        };

        match outcome {
            Ok((exit_status, signal, timed_out)) => {
                let outcome = SupervisorOutcome {
                    exit_status: Some(exit_status),
                    signal,
                    timed_out,
                    stderr_tail,
                };
                self.emit(SupervisorEvent::Exited(outcome.clone()));
                Ok(outcome)
            }
            Err(e) => Err(e),
        }
    }

    #[cfg(unix)]
    async fn supervise(
        &self,
        child: &mut Child,
        pid: u32,
    ) -> Result<(ExitStatus, Option<i32>, bool)> {
        // Listen for parent SIGINT/SIGTERM so we can relay them.
        // Registering fails only if the signal handler slot is
        // already full (shouldn't happen in normal binaries; bail
        // loud if it does — silently swallowing Ctrl-C handling is
        // worse than surfacing the error).
        let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .map_err(CrabError::Io)?;
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .map_err(CrabError::Io)?;

        let start = Instant::now();
        let timeout_deadline = self.timeout.map(|t| start + t);

        let mut timed_out = false;
        let mut sent_term = false;

        // Inner loop: wait for the first of {exit, timeout, signal}
        // and react. Once we've sent SIGTERM we switch to the
        // graceful-shutdown branch which escalates to SIGKILL after
        // `self.graceful_shutdown`.
        let exit_status = loop {
            let timeout_fut = async {
                match timeout_deadline {
                    Some(dl) => tokio::time::sleep_until(dl).await,
                    // Park forever; we only care about this branch
                    // when a timeout is configured.
                    None => std::future::pending::<()>().await,
                }
            };
            let external_kill_fut = async {
                match self.external_kill_path.as_deref() {
                    Some(path) => wait_for_external_kill(path).await,
                    None => std::future::pending::<ExternalKillKind>().await,
                }
            };

            tokio::select! {
                biased;

                // Child exited on its own — happiest path.
                res = child.wait() => {
                    let status = res.map_err(CrabError::Io)?;
                    break status;
                }

                // Stage timeout. Escalate once; fall through to the
                // graceful-shutdown branch on the next iteration.
                () = timeout_fut, if !sent_term => {
                    timed_out = true;
                    debug!(pid, "workflow supervisor: timeout fired; sending SIGTERM");
                    request_child_signal(child, pid, Signal::Term).await;
                    self.emit(SupervisorEvent::SignalSent {
                        signal: Signal::Term,
                        reason: EscalationReason::Timeout,
                    });
                    sent_term = true;
                }

                // Parent SIGINT → forward SIGINT to child.
                Some(()) = sigint.recv(), if !sent_term => {
                    debug!(pid, "workflow supervisor: parent SIGINT; forwarding");
                    request_child_signal(child, pid, Signal::Int).await;
                    self.emit(SupervisorEvent::SignalSent {
                        signal: Signal::Int,
                        reason: EscalationReason::ParentSigint,
                    });
                    sent_term = true;
                }

                // Parent SIGTERM → forward SIGTERM to child.
                Some(()) = sigterm.recv(), if !sent_term => {
                    debug!(pid, "workflow supervisor: parent SIGTERM; forwarding");
                    request_child_signal(child, pid, Signal::Term).await;
                    self.emit(SupervisorEvent::SignalSent {
                        signal: Signal::Term,
                        reason: EscalationReason::ParentSigterm,
                    });
                    sent_term = true;
                }

                kill = external_kill_fut, if !sent_term => {
                    let signal = match kill {
                        ExternalKillKind::Graceful => Signal::Int,
                        ExternalKillKind::Force => Signal::Kill,
                    };
                    debug!(pid, ?signal, "workflow supervisor: external kill request; forwarding");
                    request_child_signal(child, pid, signal).await;
                    self.emit(SupervisorEvent::SignalSent {
                        signal,
                        reason: EscalationReason::ExternalKillRequest,
                    });
                    sent_term = true;
                }

                // After SIGTERM: give the child `graceful_shutdown`,
                // then SIGKILL if it's still alive.
                () = sleep(self.graceful_shutdown), if sent_term => {
                    debug!(pid, "workflow supervisor: grace window expired; sending SIGKILL");
                    request_child_signal(child, pid, Signal::Kill).await;
                    self.emit(SupervisorEvent::SignalSent {
                        signal: Signal::Kill,
                        reason: EscalationReason::GracefulShutdownExpired,
                    });
                    // Now wait unconditionally — SIGKILL cannot be
                    // caught, the child will be reaped shortly.
                    break child.wait().await.map_err(CrabError::Io)?;
                }
            }
        };

        let signal = extract_signal(exit_status);
        Ok((exit_status, signal, timed_out))
    }

    #[cfg(not(unix))]
    async fn supervise(
        &self,
        child: &mut Child,
        pid: u32,
    ) -> Result<(ExitStatus, Option<i32>, bool)> {
        let start = Instant::now();
        let timeout_deadline = self.timeout.map(|t| start + t);

        let mut parent_ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());
        let mut timed_out = false;
        let mut sent_term = false;

        let exit_status = loop {
            let timeout_fut = async {
                match timeout_deadline {
                    Some(dl) => tokio::time::sleep_until(dl).await,
                    None => std::future::pending::<()>().await,
                }
            };
            let external_kill_fut = async {
                match self.external_kill_path.as_deref() {
                    Some(path) => wait_for_external_kill(path).await,
                    None => std::future::pending::<ExternalKillKind>().await,
                }
            };

            tokio::select! {
                biased;

                res = child.wait() => {
                    let status = res.map_err(CrabError::Io)?;
                    break status;
                }

                () = timeout_fut, if !sent_term => {
                    timed_out = true;
                    debug!(pid, "workflow supervisor: timeout fired; terminating child");
                    request_child_signal(child, pid, Signal::Term).await;
                    self.emit(SupervisorEvent::SignalSent {
                        signal: Signal::Term,
                        reason: EscalationReason::Timeout,
                    });
                    sent_term = true;
                }

                res = &mut parent_ctrl_c, if !sent_term => {
                    match res {
                        Ok(()) => {
                            debug!(pid, "workflow supervisor: parent Ctrl-C; terminating child");
                            request_child_signal(child, pid, Signal::Int).await;
                            self.emit(SupervisorEvent::SignalSent {
                                signal: Signal::Int,
                                reason: EscalationReason::ParentSigint,
                            });
                            sent_term = true;
                        }
                        Err(e) => warn!(error = %e, "workflow supervisor: Ctrl-C listener failed"),
                    }
                }

                kill = external_kill_fut, if !sent_term => {
                    let signal = match kill {
                        ExternalKillKind::Graceful => Signal::Int,
                        ExternalKillKind::Force => Signal::Kill,
                    };
                    debug!(pid, ?signal, "workflow supervisor: external kill request; terminating child");
                    request_child_signal(child, pid, signal).await;
                    self.emit(SupervisorEvent::SignalSent {
                        signal,
                        reason: EscalationReason::ExternalKillRequest,
                    });
                    sent_term = true;
                }

                () = sleep(self.graceful_shutdown), if sent_term => {
                    debug!(pid, "workflow supervisor: grace window expired; killing child");
                    request_child_signal(child, pid, Signal::Kill).await;
                    self.emit(SupervisorEvent::SignalSent {
                        signal: Signal::Kill,
                        reason: EscalationReason::GracefulShutdownExpired,
                    });
                    break child.wait().await.map_err(CrabError::Io)?;
                }
            }
        };

        let signal = extract_signal(exit_status);
        Ok((exit_status, signal, timed_out))
    }

    fn emit(&self, event: SupervisorEvent) {
        if let Some(sink) = &self.event_sink {
            sink(event);
        }
    }
}

/// Convenience: build the per-stage log path under
/// `.crab/workflow/runs/<run_id>/stage-<name>.log`.
pub fn stage_log_path(workflow_root: &Path, run_id: &str, stage_name: &str) -> PathBuf {
    workflow_root
        .join("runs")
        .join(run_id)
        .join(format!("stage-{stage_name}.log"))
}

#[derive(Debug, Clone, Copy)]
enum ExternalKillKind {
    Graceful,
    Force,
}

async fn wait_for_external_kill(path: &Path) -> ExternalKillKind {
    loop {
        match fs::read(path).await {
            Ok(bytes) => {
                let body = String::from_utf8_lossy(&bytes);
                if body.contains("force") {
                    return ExternalKillKind::Force;
                }
                return ExternalKillKind::Graceful;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "workflow supervisor: external kill request read failed"
                );
            }
        }
        sleep(Duration::from_millis(250)).await;
    }
}

#[cfg(unix)]
async fn request_child_signal(_child: &mut Child, pid: u32, signal: Signal) {
    send_signal(pid, signal);
}

#[cfg(windows)]
async fn request_child_signal(child: &mut Child, pid: u32, signal: Signal) {
    // Windows has no POSIX process-group signal equivalent. `taskkill /T`
    // asks the native process tree manager to terminate the shell and every
    // descendant; `/F` is reserved for the supervisor's final kill step.
    let force = matches!(signal, Signal::Kill);
    let pid_text = pid.to_string();
    let mut command = Command::new("taskkill");
    command
        .args(["/PID", pid_text.as_str(), "/T"])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if force {
        command.arg("/F");
    }
    match command.status().await {
        Ok(status) if status.success() => {}
        Ok(status) => {
            warn!(pid, ?signal, code = ?status.code(), "workflow supervisor: taskkill failed");
            if let Err(err) = child.start_kill() {
                warn!(pid, ?signal, error = %err, "workflow supervisor: terminate child failed");
            }
        }
        Err(err) => {
            warn!(pid, ?signal, error = %err, "workflow supervisor: taskkill unavailable");
            if let Err(err) = child.start_kill() {
                warn!(pid, ?signal, error = %err, "workflow supervisor: terminate child failed");
            }
        }
    }
}

#[cfg(not(any(unix, windows)))]
async fn request_child_signal(child: &mut Child, pid: u32, signal: Signal) {
    if let Err(err) = child.start_kill() {
        warn!(pid, ?signal, error = %err, "workflow supervisor: terminate child failed");
    }
}

#[cfg(unix)]
fn send_signal(pid: u32, signal: Signal) {
    // Best-effort: if the child has already exited the kernel
    // returns ESRCH. That's not an error we want to surface to the
    // caller — a concurrent exit race is indistinguishable from
    // "signal delivered and handled" from here.
    let target = nix::unistd::Pid::from_raw(-(pid as i32));
    if let Err(err) = nix::sys::signal::kill(target, signal.as_nix())
        && err != nix::errno::Errno::ESRCH
    {
        warn!(pid, ?signal, error = %err, "workflow supervisor: kill failed");
    }
}

#[cfg(unix)]
fn extract_signal(status: ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn extract_signal(_status: ExitStatus) -> Option<i32> {
    None
}

/// Fixed-capacity ring buffer that keeps the most recent `cap` bytes.
/// We drop raw bytes on the floor once we cross the cap, then
/// `snapshot` lossily converts to UTF-8 — stderr tails are for human
/// consumption, not parsing, so replacement characters are fine.
struct RingTail {
    cap: usize,
    buf: Vec<u8>,
}

impl RingTail {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            buf: Vec::with_capacity(cap),
        }
    }

    fn push(&mut self, data: &[u8]) {
        if self.cap == 0 {
            return;
        }
        if data.len() >= self.cap {
            // The incoming chunk alone exceeds capacity; keep only
            // its trailing `cap` bytes.
            let start = data.len() - self.cap;
            self.buf.clear();
            self.buf.extend_from_slice(&data[start..]);
            return;
        }
        let overflow = self
            .buf
            .len()
            .saturating_add(data.len())
            .saturating_sub(self.cap);
        if overflow > 0 {
            self.buf.drain(..overflow);
        }
        self.buf.extend_from_slice(data);
    }

    fn snapshot(&self) -> String {
        String::from_utf8_lossy(&self.buf).into_owned()
    }
}

async fn pump_stdout(
    stdout: ChildStdout,
    log: Arc<Mutex<File>>,
    capture: Option<Arc<Mutex<File>>>,
    mirror_output: bool,
) {
    let mut reader = BufReader::new(stdout).lines();
    let mut out = tokio::io::stdout();
    loop {
        match reader.next_line().await {
            Ok(Some(line)) => {
                // Let write failures on the user's tty / logfile
                // degrade gracefully — a dropped line is preferable
                // to aborting the supervisor mid-run.
                if mirror_output {
                    let _ = write_line(&mut out, &line).await;
                }
                let mut guard = log.lock().await;
                let _ = write_line(&mut *guard, &line).await;
                drop(guard);
                if let Some(ref cap) = capture {
                    let mut cap_guard = cap.lock().await;
                    let _ = write_line(&mut *cap_guard, &line).await;
                }
            }
            Ok(None) => break,
            Err(e) => {
                warn!(error = %e, "workflow supervisor: stdout read failed");
                break;
            }
        }
    }
}

async fn pump_stderr(
    stderr: ChildStderr,
    log: Arc<Mutex<File>>,
    tail: Arc<Mutex<RingTail>>,
    mirror_output: bool,
) {
    let mut reader = BufReader::new(stderr).lines();
    let mut err = tokio::io::stderr();
    loop {
        match reader.next_line().await {
            Ok(Some(line)) => {
                if mirror_output {
                    let _ = write_line(&mut err, &line).await;
                }
                {
                    let mut guard = log.lock().await;
                    let _ = write_line(&mut *guard, &line).await;
                }
                {
                    let mut guard = tail.lock().await;
                    guard.push(line.as_bytes());
                    guard.push(b"\n");
                }
            }
            Ok(None) => break,
            Err(e) => {
                warn!(error = %e, "workflow supervisor: stderr read failed");
                break;
            }
        }
    }
}

async fn write_line<W: AsyncWrite + Unpin>(writer: &mut W, line: &str) -> std::io::Result<()> {
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    Ok(())
}

#[cfg(all(test, unix))]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use tempfile::TempDir;

    /// Collect emitted events for assertion in tests.
    fn event_collector() -> (EventSink, Arc<StdMutex<Vec<SupervisorEvent>>>) {
        let store: Arc<StdMutex<Vec<SupervisorEvent>>> = Arc::new(StdMutex::new(Vec::new()));
        let store_for_sink = Arc::clone(&store);
        let sink: EventSink = Arc::new(move |event| {
            store_for_sink.lock().unwrap().push(event);
        });
        (sink, store)
    }

    fn log_path(tmp: &TempDir) -> PathBuf {
        tmp.path().join("runs").join("r1").join("stage-test.log")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fast_child_reports_success() {
        let tmp = TempDir::new().unwrap();
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("echo hello");

        let outcome = ChildSupervisor::new(cmd, log_path(&tmp))
            .run()
            .await
            .expect("supervisor must not error on clean exit");

        let status = outcome.exit_status.expect("status present");
        assert!(status.success(), "expected success, got {status:?}");
        assert_eq!(status.code(), Some(0));
        assert!(!outcome.timed_out);
        assert_eq!(outcome.signal, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn non_zero_exit_is_reported() {
        let tmp = TempDir::new().unwrap();
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("exit 1");

        let outcome = ChildSupervisor::new(cmd, log_path(&tmp))
            .run()
            .await
            .unwrap();

        assert_eq!(outcome.exit_status.unwrap().code(), Some(1));
        assert!(!outcome.timed_out);
        assert_eq!(outcome.signal, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn timeout_escalates_sigterm_then_sigkill() {
        let tmp = TempDir::new().unwrap();

        // Trap SIGTERM so the child ignores it; only SIGKILL after
        // the grace window will take it down. Short grace window so
        // the test runs quickly.
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("trap '' TERM; sleep 30");

        let (sink, events) = event_collector();

        let outcome = ChildSupervisor::new(cmd, log_path(&tmp))
            .with_timeout(Duration::from_millis(200))
            .with_graceful_shutdown(Duration::from_millis(300))
            .with_event_sink(sink)
            .run()
            .await
            .unwrap();

        assert!(outcome.timed_out, "expected timed_out: {outcome:?}");
        // The child was killed, so exit status has no code.
        assert_eq!(outcome.exit_status.unwrap().code(), None);
        assert_eq!(outcome.signal, Some(libc::SIGKILL));

        let events = events.lock().unwrap();
        let reasons: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                SupervisorEvent::SignalSent { reason, signal } => Some((*reason, *signal)),
                _ => None,
            })
            .collect();
        assert_eq!(
            reasons,
            vec![
                (EscalationReason::Timeout, Signal::Term),
                (EscalationReason::GracefulShutdownExpired, Signal::Kill),
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stderr_tail_captures_trailing_bytes() {
        let tmp = TempDir::new().unwrap();

        // Emit 100 KiB of stderr so the tail only keeps the last
        // ~8 KiB (default). We make each line distinct so we can
        // verify the tail matches the end of the stream.
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg("for i in $(seq 1 4000); do echo \"err line $i\" 1>&2; done");

        let outcome = ChildSupervisor::new(cmd, log_path(&tmp))
            .with_stderr_tail_bytes(1024)
            .run()
            .await
            .unwrap();

        assert!(outcome.exit_status.unwrap().success());
        assert!(
            outcome.stderr_tail.len() <= 1024,
            "tail must respect cap, got {}",
            outcome.stderr_tail.len()
        );
        // Last-emitted line should land in the tail. The loop
        // writes up through "err line 4000".
        assert!(
            outcome.stderr_tail.contains("err line 4000"),
            "tail missing final line: {:?}",
            outcome.stderr_tail
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn log_file_contains_stdout_and_stderr() {
        let tmp = TempDir::new().unwrap();
        let log = log_path(&tmp);

        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("echo out-alpha; echo err-beta 1>&2");

        let _ = ChildSupervisor::new(cmd, log.clone()).run().await.unwrap();

        let contents = std::fs::read_to_string(&log).expect("log file created");
        assert!(
            contents.contains("out-alpha"),
            "missing stdout: {contents:?}"
        );
        assert!(
            contents.contains("err-beta"),
            "missing stderr: {contents:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn relayed_sigint_marks_stage_aborted() {
        // Build a child that respects SIGINT (default shell
        // behavior). Send SIGINT directly to the child after it
        // starts; the supervisor reaps the child and reports the
        // terminating signal in the outcome.
        let tmp = TempDir::new().unwrap();

        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("sleep 30");

        let (sink, events) = event_collector();
        let events_for_killer = Arc::clone(&events);

        // Fire SIGINT at the child as soon as we see the Started
        // event. Runs on the test runtime, not inside the
        // supervisor, so it races the pump loop — that's fine.
        let killer = tokio::spawn(async move {
            loop {
                {
                    let guard = events_for_killer.lock().unwrap();
                    if let Some(SupervisorEvent::Started { pid }) = guard.first() {
                        send_signal(*pid, Signal::Int);
                        return;
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });

        let outcome = ChildSupervisor::new(cmd, log_path(&tmp))
            .with_event_sink(sink)
            .run()
            .await
            .unwrap();

        killer.await.unwrap();

        assert_eq!(
            outcome.signal,
            Some(libc::SIGINT),
            "expected SIGINT termination, got {outcome:?}"
        );
        assert!(!outcome.timed_out);
    }

    #[test]
    fn ring_tail_keeps_only_last_cap_bytes() {
        let mut tail = RingTail::new(8);
        tail.push(b"0123");
        tail.push(b"4567");
        tail.push(b"89ABCDEF");
        // Final contents should be the last 8 bytes of
        // "0123456789ABCDEF" → "89ABCDEF".
        assert_eq!(tail.snapshot(), "89ABCDEF");
    }

    #[test]
    fn ring_tail_handles_single_chunk_over_capacity() {
        let mut tail = RingTail::new(4);
        tail.push(b"abcdefghij");
        assert_eq!(tail.snapshot(), "ghij");
    }

    #[test]
    fn ring_tail_disabled_when_cap_zero() {
        let mut tail = RingTail::new(0);
        tail.push(b"anything");
        assert_eq!(tail.snapshot(), "");
    }

    #[test]
    fn stage_log_path_layout_matches_spec() {
        let root = Path::new("/repo/.crab/workflow");
        let got = stage_log_path(root, "01930abc-dead-beef", "train");
        assert_eq!(
            got,
            Path::new("/repo/.crab/workflow/runs/01930abc-dead-beef/stage-train.log")
        );
    }
}

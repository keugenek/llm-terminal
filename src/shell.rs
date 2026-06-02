use crate::decider::{Decider, Decision};
use crate::trajectory::Trajectory;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::error::Error;
use std::io::{Read, Write};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const READ_CHUNK_TIMEOUT: Duration = Duration::from_millis(50);
const DEFAULT_TIMEOUT_SECS: u64 = 30;
// While tearing a command down, re-issue SIGKILL this often. Retrying re-scans
// the process tree, catching a child that had not yet forked on the first try.
const KILL_RETRY: Duration = Duration::from_millis(250);
// Total time to keep SIGKILL-ing and waiting for bash to reap the tree and emit
// the end marker before declaring the shell unrecoverable.
const KILL_TOTAL_GRACE: Duration = Duration::from_secs(5);

// Auto-accept tuning. We inject Enter either when the program goes quiet after
// showing a prompt (IDLE), or — for animated TUIs that never go quiet, like
// Claude Code — once the prompt has been on screen long enough to have finished
// rendering (SETTLE). COOLDOWN is the minimum gap between injections; TAIL is
// how many trailing output bytes we scan for prompt patterns.
const AUTO_ACCEPT_IDLE: Duration = Duration::from_millis(600);
const AUTO_ACCEPT_SETTLE: Duration = Duration::from_millis(1200);
const AUTO_ACCEPT_COOLDOWN: Duration = Duration::from_millis(1500);
const AUTO_ACCEPT_TAIL: usize = 8192;
// After a Deny sends its primary reject ("2"), wait this long before falling
// back to Escape if the program is still prompting.
const AUTO_ACCEPT_DENY_FOLLOWUP: Duration = Duration::from_millis(500);
// For unattended task delivery: once an interactively-prompting agent has drawn
// output and then gone quiet this long, it's ready for the task to be typed in.
// Longer than IDLE so the agent's startup banner/TUI has fully settled first.
const AUTO_ACCEPT_TASK_DELAY: Duration = Duration::from_millis(1500);

pub struct Shell {
    writer: Box<dyn Write + Send>,
    rx: mpsc::Receiver<Vec<u8>>,
    counter: u64,
    command_timeout: Duration,
    child_pid: Option<u32>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    // The reader thread draining the master pty. Kept so Drop can join it after
    // killing bash (which closes the slave → the reader hits EOF), instead of
    // leaking the thread + an orphaned bash for the process's lifetime.
    reader_handle: Option<thread::JoinHandle<()>>,
}

pub struct CommandResult {
    pub output: String,
    pub exit_code: i32,
    /// True if the command was cut short (by the user or the timeout) rather
    /// than finishing on its own.
    pub interrupted: bool,
}

impl Shell {
    pub fn spawn() -> Result<Self, Box<dyn Error>> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = CommandBuilder::new("bash");
        cmd.arg("--noprofile");
        cmd.arg("--norc");
        cmd.env("PS1", "");
        cmd.env("PS2", "");
        cmd.env("TERM", "dumb");

        let child = pair.slave.spawn_command(cmd)?;
        let child_pid = child.process_id();
        drop(pair.slave);

        let writer = pair.master.take_writer()?;
        let mut reader = pair.master.try_clone_reader()?;

        let (tx, rx) = mpsc::channel();
        let reader_handle = thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let command_timeout = std::env::var("MT_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or_else(|| Duration::from_secs(DEFAULT_TIMEOUT_SECS));

        let mut shell = Self {
            writer,
            rx,
            counter: 0,
            command_timeout,
            child_pid,
            child,
            master: pair.master,
            reader_handle: Some(reader_handle),
        };

        shell.writer.write_all(b"stty -echo -onlcr 2>/dev/null\n")?;
        shell.writer.flush()?;
        thread::sleep(Duration::from_millis(200));
        while shell.rx.try_recv().is_ok() {}

        Ok(shell)
    }

    #[cfg(test)]
    fn set_timeout(&mut self, timeout: Duration) {
        self.command_timeout = timeout;
    }

    /// Run `command`, blocking until it finishes, is interrupted, or times out.
    ///
    /// `user_interrupt` is polled each iteration; when it returns true (the user
    /// pressed Ctrl-C in the outer terminal) the running command is torn down
    /// immediately. The same teardown path fires automatically on timeout.
    pub fn run(
        &mut self,
        command: &str,
        user_interrupt: &mut dyn FnMut() -> bool,
    ) -> Result<CommandResult, Box<dyn Error>> {
        self.counter += 1;
        let id = self.counter;
        let start_marker = format!("__MTSTART_{id}__");
        let end_prefix = "__MTEND_";
        let end_suffix = format!("_{id}__");

        // Wrap the command in a brace group with stdin redirected from
        // /dev/null. The redirect makes stdin-reading commands (`read`, REPLs,
        // interactive tools) hit EOF and exit instead of hanging forever and
        // wedging the shell. A brace group (not a subshell) keeps `cd` and env
        // assignments in the current shell so state persists across commands.
        let wrapped = format!(
            "printf '%s\\n' '{start_marker}'\n{{ {command}\n}} </dev/null\nprintf '\\n{end_prefix}%d{end_suffix}\\n' \"$?\"\n"
        );

        self.writer.write_all(wrapped.as_bytes())?;
        self.writer.flush()?;

        let started_marker = format!("{start_marker}\n");
        let mut acc = Vec::new();
        // Until the start marker appears, bash is still reading the command;
        // interrupting now would SIGINT bash itself (it shares the command's
        // process group) and discard the trailing printf, wedging the shell.
        // So gate all teardown on `started`, and measure the timeout from when
        // the command actually begins running.
        let mut started = false;
        let mut deadline = Instant::now() + self.command_timeout;
        // Once we decide to tear the command down, we SIGKILL its process tree
        // (never bash) and keep retrying until the end marker proves it died.
        let mut killing = false;
        let mut last_kill = Instant::now();
        let mut interrupted = false;
        loop {
            if !started {
                if Instant::now() > deadline {
                    return Err("shell did not acknowledge the command (not responding)".into());
                }
            } else if !killing {
                if user_interrupt() || Instant::now() > deadline {
                    // Tear the command down with a real SIGKILL of its process
                    // tree rather than a tty Ctrl-C: the command shares bash's
                    // process group, so a tty signal could hit bash too and
                    // wedge it. Killing by PID only ever targets the command's
                    // descendants. SIGKILL also can't be ignored, so it works on
                    // tools that grab the pty in raw mode and swallow tty signals.
                    interrupted = true;
                    killing = true;
                    deadline = Instant::now() + KILL_TOTAL_GRACE;
                    if let Some(pid) = self.child_pid {
                        kill_descendants(pid);
                    }
                    last_kill = Instant::now();
                    continue;
                }
            } else {
                if Instant::now() > deadline {
                    return Err("command did not terminate after repeated SIGKILL — it is \
                                likely an interactive tool that detaches its process tree. \
                                Run it in passthrough instead: ':<command>' (or add its name \
                                to MT_INTERACTIVE)."
                        .into());
                }
                if last_kill.elapsed() >= KILL_RETRY {
                    if let Some(pid) = self.child_pid {
                        kill_descendants(pid);
                    }
                    last_kill = Instant::now();
                }
            }

            match self.rx.recv_timeout(READ_CHUNK_TIMEOUT) {
                Ok(chunk) => acc.extend_from_slice(&chunk),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("shell process exited".into());
                }
            }

            let text = String::from_utf8_lossy(&acc);
            if !started && text.contains(&started_marker) {
                started = true;
                deadline = Instant::now() + self.command_timeout;
            }
            if let Some((end_idx, code)) = find_end(&text, end_prefix, &end_suffix) {
                let body_start = text
                    .find(&format!("{start_marker}\n"))
                    .map(|i| i + start_marker.len() + 1)
                    .unwrap_or(0);
                let output = text[body_start..end_idx].trim_end_matches('\n').to_string();
                return Ok(CommandResult {
                    output,
                    exit_code: code,
                    interrupted,
                });
            }
        }
    }

    /// Run `command` interactively: hand the inner pty straight to the outer
    /// terminal so full-screen / REPL programs (claude, vim, top, python) work
    /// for real. Forwards raw bytes both ways until the command's process tree
    /// exits, then resynchronises back to capture mode.
    ///
    /// When `auto_accept` is set, known agents are launched with their native
    /// permission-bypass flag, and for any other program the broker grades each
    /// settled prompt: `decider.decide(tail, policy)` returns Approve / Deny /
    /// Escalate, and we act accordingly (inject Enter, inject a reject, or hand
    /// control back to the human). When `auto_accept` is off we never inject —
    /// the `decider` and `policy` are ignored, same as before this feature.
    ///
    /// `pending_input`, when set, is the task text typed into the program once,
    /// followed by Enter, after the program's first output burst settles. This
    /// is how unattended task delivery seeds an agent that takes its prompt
    /// interactively rather than from argv. It fires regardless of `auto_accept`.
    ///
    /// The outer terminal must already be in raw mode.
    pub fn run_interactive(
        &mut self,
        command: &str,
        auto_accept: bool,
        decider: &dyn Decider,
        policy: &str,
        trajectory: &Trajectory,
        pending_input: Option<&str>,
    ) -> Result<(), Box<dyn Error>> {
        // When auto-accept is on, prefer a known agent's native permission-bypass
        // flag (claude/codex). If one is applied, the agent handles its own
        // permissions and shows no prompts to grade — so the output-scanning
        // broker must stay OFF, or it false-positives on the agent's normal TUI
        // chrome (e.g. `❯ 1`, mode/effort selectors) and spams escalations. The
        // broker only runs as a fallback for auto-accept programs that did NOT
        // get a native flag.
        let (command, mut broker_active) = if auto_accept {
            let native_bypass = applies_native_bypass(command);
            (with_auto_accept_flag(command), !native_bypass)
        } else {
            (command.to_string(), false)
        };

        // Match the inner pty to the outer terminal and give bash a capable TERM
        // and sane line discipline so the program renders and reads correctly
        // (capture mode keeps TERM=dumb and echo off).
        if let Ok((cols, rows)) = crossterm::terminal::size() {
            let _ = self.master.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
        let term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string());
        self.writer.write_all(
            format!("stty sane 2>/dev/null; export TERM={term}\n").as_bytes(),
        )?;
        self.writer.flush()?;
        thread::sleep(Duration::from_millis(80));
        while self.rx.try_recv().is_ok() {}

        // Launch bare (no /dev/null redirect, no capture wrapper) so the command
        // inherits the pty as its controlling terminal.
        self.writer.write_all(format!("{command}\n").as_bytes())?;
        self.writer.flush()?;

        let mut out = std::io::stdout();
        let mut child_seen = false;
        let start = Instant::now();
        let mut last_child_check = start - Duration::from_secs(1);
        // Auto-accept injection state: a rolling tail of recent output, when the
        // program last emitted anything, and when we last injected Enter.
        let mut tail: Vec<u8> = Vec::new();
        let mut last_output = Instant::now();
        let mut last_inject = start - AUTO_ACCEPT_COOLDOWN;
        let mut prompt_seen_at: Option<Instant> = None;
        // When a Deny sends its primary reject, we record when, so that — if the
        // program is still prompting a moment later — we can follow up with an
        // Escape as a fallback. Reset once the prompt clears or we follow up.
        let mut deny_sent_at: Option<Instant> = None;
        // The exact tail we last escalated. While the on-screen prompt is
        // unchanged we don't re-grade it — otherwise we'd re-escalate (re-calling
        // the model, re-logging, re-printing) every cooldown while the human is
        // still handling the prompt.
        let mut escalated_tail: Option<String> = None;
        // Task delivery for an agent that takes its prompt interactively: type
        // it once the program settles, then clear so we never type it twice.
        let mut pending_input = pending_input;
        // Did the program actually emit any output yet? Gates task delivery so we
        // don't type into a program that hasn't drawn its input line. (A plain
        // `last_output > start` is always true since last_output starts later.)
        let mut saw_output = false;
        // Track output for idle detection whenever either path needs it.
        let track_output = broker_active || pending_input.is_some();
        loop {
            // Inner pty -> screen.
            let mut wrote = false;
            while let Ok(chunk) = self.rx.try_recv() {
                out.write_all(&chunk)?;
                if track_output {
                    last_output = Instant::now();
                    saw_output = true;
                }
                if broker_active {
                    tail.extend_from_slice(&chunk);
                    let overflow = tail.len().saturating_sub(AUTO_ACCEPT_TAIL);
                    if overflow > 0 {
                        tail.drain(..overflow);
                    }
                }
                wrote = true;
            }
            if wrote {
                out.flush()?;
            }

            // Unattended task delivery: once the program has produced output and
            // then gone idle long enough to be waiting for input, type the task
            // followed by Enter exactly once. Gated on having actually seen
            // output so we don't fire into a program that hasn't drawn its input.
            if let Some(task) = pending_input {
                if saw_output && last_output.elapsed() > AUTO_ACCEPT_TASK_DELAY {
                    self.writer.write_all(task.as_bytes())?;
                    self.writer.write_all(b"\r")?;
                    self.writer.flush()?;
                    trajectory.log_interactive(&format!("task-injected: {task}"));
                    pending_input = None;
                }
            }

            // Auto-accept: once a prompt is up and has settled, ask the broker
            // what to do instead of blindly accepting. Fire when the program
            // goes idle after the prompt, OR — for animated TUIs that never go
            // idle (Claude Code redraws a spinner, hints, cursor) — once the
            // prompt has been on screen long enough to have finished rendering.
            if broker_active {
                let is_prompt = looks_like_prompt(&String::from_utf8_lossy(&tail));
                if is_prompt {
                    prompt_seen_at.get_or_insert_with(Instant::now);
                } else {
                    prompt_seen_at = None;
                    deny_sent_at = None; // prompt cleared; cancel any deny follow-up
                }

                // Deny follow-up: if we sent the primary reject but the program
                // is still prompting a moment later, send Escape once as a
                // fallback (some prompts dismiss on Esc, not on a "2"). Heuristic.
                if let Some(t) = deny_sent_at {
                    if is_prompt && t.elapsed() > AUTO_ACCEPT_DENY_FOLLOWUP {
                        self.writer.write_all(b"\x1b")?;
                        self.writer.flush()?;
                        deny_sent_at = None;
                        prompt_seen_at = None;
                        tail.clear();
                    }
                }

                let idle = last_output.elapsed() > AUTO_ACCEPT_IDLE;
                let settled =
                    prompt_seen_at.is_some_and(|t| t.elapsed() > AUTO_ACCEPT_SETTLE);
                let tail_text = String::from_utf8_lossy(&tail).into_owned();
                let already_escalated = escalated_tail.as_deref() == Some(tail_text.as_str());
                if is_prompt
                    && (idle || settled)
                    && last_inject.elapsed() > AUTO_ACCEPT_COOLDOWN
                    // Don't re-grade a prompt we already escalated: wait until the
                    // screen changes (the human acts) before deciding again.
                    && !already_escalated
                {
                    let decision = decider.decide(&tail_text, policy);
                    // Record the verdict before acting: the scraped tail is the
                    // prompt excerpt, and the decider's reason explains the call.
                    // The logger redacts/truncates; here we just forward.
                    trajectory.log_decision(
                        decision_verdict(decision),
                        &decider.last_reason(),
                        &tail_text,
                    );
                    // The cooldown applies to every settled decision (approve,
                    // deny, escalate) so we never re-fire on the same prompt.
                    last_inject = Instant::now();
                    prompt_seen_at = None;
                    match decision_action(decision) {
                        Action::Inject(bytes) => {
                            self.writer.write_all(bytes)?;
                            self.writer.flush()?;
                            escalated_tail = None;
                            tail.clear(); // require a fresh prompt before deciding again
                        }
                        Action::Deny(bytes) => {
                            self.writer.write_all(bytes)?;
                            self.writer.flush()?;
                            // Arm the Escape fallback in case "2" didn't dismiss it.
                            deny_sent_at = Some(Instant::now());
                            escalated_tail = None;
                            tail.clear();
                        }
                        Action::Escalate => {
                            // Hand control to the human: don't inject anything.
                            let reason = decider.last_reason();
                            let reason = if reason.is_empty() {
                                "prompt not clearly allowed by policy".to_string()
                            } else {
                                reason
                            };
                            print_escalation(&mut out, &reason)?;
                            // Latch THIS prompt so we don't re-grade/re-escalate it
                            // until the screen changes — the human is now handling
                            // it. (We keep the tail so detection still sees it.)
                            escalated_tail = Some(tail_text);
                            // If the broker is unreachable (transport error, e.g. a
                            // bad API key) it fails the same way on every prompt, so
                            // disable it for the session rather than escalating each
                            // one. Keyed on the decider's structured signal, never on
                            // free-text reason wording.
                            if decider.unreachable() {
                                print_escalation(
                                    &mut out,
                                    "broker unreachable — disabling auto-accept for this session",
                                )?;
                                broker_active = false;
                            }
                        }
                    }
                }
            }

            // Keyboard -> inner pty (raw bytes: arrows, escapes, Ctrl-* all pass).
            if let Some(bytes) = read_stdin_nonblocking(Duration::from_millis(10)) {
                if bytes.is_empty() {
                    break; // stdin EOF
                }
                self.writer.write_all(&bytes)?;
                self.writer.flush()?;
            }

            // Exit detection (throttled): the command is done once a child of
            // bash appeared and then went away. Detached daemons aren't children
            // of bash, so they don't keep us here.
            if last_child_check.elapsed() >= Duration::from_millis(150) {
                last_child_check = Instant::now();
                // None = the pgrep probe itself failed (EAGAIN/EINTR under load).
                // Treat that as "unknown", never as "exited" — otherwise a single
                // flaky probe would abandon a still-running program.
                match self.child_pid.and_then(has_children) {
                    Some(true) => child_seen = true,
                    Some(false) if child_seen => break,
                    Some(false) if start.elapsed() > Duration::from_secs(3) => break,
                    _ => {} // Some(false) before any child, or None (unknown)
                }
            }
        }

        // Flush any final bytes emitted as the command exited.
        thread::sleep(Duration::from_millis(60));
        while let Ok(chunk) = self.rx.try_recv() {
            out.write_all(&chunk)?;
        }
        out.flush()?;

        self.resync()
    }

    /// Restore capture-mode tty settings (the interactive program may have
    /// changed them) and resynchronise on a fresh marker so the next captured
    /// command starts clean.
    fn resync(&mut self) -> Result<(), Box<dyn Error>> {
        self.counter += 1;
        let marker = format!("__MTSYNC_{}__", self.counter);
        self.writer.write_all(
            format!(
                "stty -echo -onlcr 2>/dev/null; export TERM=dumb; printf '%s\\n' '{marker}'\n"
            )
            .as_bytes(),
        )?;
        self.writer.flush()?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut acc = Vec::new();
        while Instant::now() < deadline {
            if let Ok(chunk) = self.rx.recv_timeout(READ_CHUNK_TIMEOUT) {
                acc.extend_from_slice(&chunk);
                if String::from_utf8_lossy(&acc).contains(&marker) {
                    break;
                }
            }
        }
        Ok(())
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        // Kill bash so it can't linger as an orphan, then join the reader thread.
        // Killing bash closes the slave pty, so the reader's blocking read() hits
        // EOF and the thread exits — otherwise it (and bash) would leak for the
        // rest of the process's life. portable-pty's Child does NOT kill on its
        // own Drop, so we must do it explicitly.
        let _ = self.child.kill();
        if let Some(handle) = self.reader_handle.take() {
            let _ = handle.join();
        }
    }
}

/// The native permission-bypass flag for a known agent, keyed on the command's
/// first-token basename. `None` for programs we don't recognise.
fn native_bypass_flag(command: &str) -> Option<&'static str> {
    let first = command.split_whitespace().next().unwrap_or("");
    let base = first.rsplit('/').next().unwrap_or(first);
    match base {
        "claude" => Some("--dangerously-skip-permissions"),
        _ => None,
    }
}

/// Append a known agent's native permission-bypass flag (if not already present).
/// Using the program's own flag is far more reliable than injecting keystrokes.
fn with_auto_accept_flag(command: &str) -> String {
    match native_bypass_flag(command) {
        Some(f) if !command.contains(f) => format!("{command} {f}"),
        _ => command.to_string(),
    }
}

/// True if `command` is a known agent that bypasses its own permissions. When it
/// does, the agent self-approves and shows no real prompts, so the output-
/// scanning broker must be disabled to avoid false-positives on its TUI chrome —
/// regardless of whether the bypass flag was already present in the command.
fn applies_native_bypass(command: &str) -> bool {
    native_bypass_flag(command).is_some()
}

/// What `run_interactive` does for a graded decision. Split out as a pure
/// mapping from `Decision` to concrete bytes/behaviour so it can be unit-tested
/// without a live PTY.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    /// Inject these bytes and consider the prompt handled (Approve).
    Inject(&'static [u8]),
    /// Inject the primary reject bytes, then arm an Escape fallback (Deny).
    Deny(&'static [u8]),
    /// Inject nothing; hand control back to the human (Escalate).
    Escalate,
}

/// Map a broker `Decision` to the action taken at a settled prompt.
///
/// - Approve: inject Enter (`\r`), preserving the original auto-accept.
/// - Deny: inject "2\r" (the conventional "No / second option"); the caller
///   follows up with Escape if the prompt persists. Heuristic: not every prompt
///   maps "no" to option 2, hence the Esc fallback.
/// - Escalate: inject nothing.
fn decision_action(decision: Decision) -> Action {
    match decision {
        Decision::Approve => Action::Inject(b"\r"),
        Decision::Deny => Action::Deny(b"2\r"),
        Decision::Escalate => Action::Escalate,
    }
}

/// The lowercase verdict label recorded in the trajectory log for a decision.
/// Kept as a pure mapping next to `decision_action` so the two stay in sync.
fn decision_verdict(decision: Decision) -> &'static str {
    match decision {
        Decision::Approve => "approve",
        Decision::Deny => "deny",
        Decision::Escalate => "escalate",
    }
}

/// Print a clearly-styled escalation line to the user mid-passthrough. Uses
/// `\r\n` because the terminal is in raw mode during interactive sessions.
fn print_escalation(out: &mut impl Write, reason: &str) -> Result<(), Box<dyn Error>> {
    use crossterm::style::{Color, Print, ResetColor, SetForegroundColor};
    crossterm::execute!(
        out,
        Print("\r\n"),
        SetForegroundColor(Color::Yellow),
        Print(format!("· escalated to you: {reason}")),
        ResetColor,
        Print("\r\n"),
    )?;
    out.flush()?;
    Ok(())
}

/// Heuristic: does the trailing output look like a yes/no or selection prompt
/// that's waiting for the user? Used to decide when to inject an accept.
fn looks_like_prompt(tail: &str) -> bool {
    // A prompt sits at the BOTTOM of the screen, so only scan the last few
    // non-empty lines. Scanning the whole 8 KB window matches the words below
    // anywhere in scrollback/prose and causes false positives (e.g. an agent
    // that merely says "approved" mid-output).
    let recent: String = tail
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(4)
        .collect::<Vec<_>>()
        .join("\n")
        .to_lowercase();
    // Needles are kept prompt-shaped (a question, a y/n, an option marker) rather
    // than bare verbs, so ordinary prose doesn't read as a waiting prompt.
    const NEEDLES: &[&str] = &[
        "(y/n)",
        "[y/n]",
        "y/n)",
        "yes/no",
        "do you want",
        "proceed?",
        "continue?",
        "press enter",
        "1. yes",
        "1) yes",
        "❯ 1",
        "(use arrow keys)",
        "approve?",
        "approve this",
        "allow this",
    ];
    NEEDLES.iter().any(|n| recent.contains(n))
}

/// True if `pid` currently has at least one direct child process.
/// `Some(true)`/`Some(false)` if `pgrep -P pid` ran and (didn't) find children;
/// `None` if the probe itself failed to run (so the caller can treat it as
/// "unknown" rather than "no children").
fn has_children(pid: u32) -> Option<bool> {
    let out = Command::new("pgrep").arg("-P").arg(pid.to_string()).output().ok()?;
    // pgrep exits 0 with matches, 1 with none, >1 on error. Trust stdout content.
    Some(out.stdout.iter().any(|b| !b.is_ascii_whitespace()))
}

/// Non-blocking read of raw bytes from stdin (fd 0), waiting up to `timeout`.
/// Returns `Some(bytes)` if data was read (empty vec signals EOF), `None` on
/// timeout. Reads the fd directly to bypass Rust's stdin buffering during
/// passthrough.
fn read_stdin_nonblocking(timeout: Duration) -> Option<Vec<u8>> {
    let mut pfd = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut pfd, 1, timeout.as_millis() as libc::c_int) };
    if ready <= 0 || pfd.revents & libc::POLLIN == 0 {
        return None;
    }
    let mut buf = [0u8; 4096];
    let n = unsafe { libc::read(0, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    if n > 0 {
        Some(buf[..n as usize].to_vec())
    } else {
        // n == 0 -> EOF; n < 0 -> error. Both end passthrough.
        Some(Vec::new())
    }
}

/// SIGKILL every descendant of `root` (typically the wrapped bash), deepest
/// first, leaving `root` itself alive. Used to forcibly stop a command tree
/// that ignores tty signals. Processes that detached into their own session
/// (e.g. background daemons) are intentionally not reached by the `pgrep -P`
/// walk and are left running.
fn kill_descendants(root: u32) {
    let mut stack = vec![root];
    let mut victims = Vec::new();
    while let Some(pid) = stack.pop() {
        let out = match Command::new("pgrep").arg("-P").arg(pid.to_string()).output() {
            Ok(o) => o,
            Err(_) => continue,
        };
        for token in String::from_utf8_lossy(&out.stdout).split_whitespace() {
            if let Ok(child) = token.parse::<u32>() {
                victims.push(child);
                stack.push(child);
            }
        }
    }
    for pid in victims.iter().rev() {
        // Signal directly via libc rather than shelling out to `kill`: no PATH
        // dependency (a shadowed `kill` can't be invoked), no stdio to silence,
        // and a smaller scan→signal window. A child that already exited just
        // yields ESRCH, which we ignore.
        if *pid != 0 && *pid <= i32::MAX as u32 {
            unsafe { libc::kill(*pid as libc::pid_t, libc::SIGKILL) };
        }
    }
}

fn find_end(text: &str, prefix: &str, suffix: &str) -> Option<(usize, i32)> {
    let mut from = 0;
    while let Some(p) = text[from..].find(prefix) {
        let abs_p = from + p;
        let after = abs_p + prefix.len();
        if let Some(s) = text[after..].find(suffix) {
            let abs_s = after + s;
            if let Ok(code) = text[after..abs_s].parse::<i32>() {
                return Some((abs_p, code));
            }
        }
        from = abs_p + 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decider::MockDecider;

    // A known agent gets a native bypass flag, so the output-scanning broker is
    // disabled for it (it would otherwise false-positive on the agent's TUI).
    // An unknown / non-agent program gets no flag, so the broker stays available.
    #[test]
    fn native_bypass_disables_broker_for_known_agents() {
        assert!(applies_native_bypass("claude"));
        assert!(applies_native_bypass("claude 'fix the bug'"));
        assert!(applies_native_bypass("/usr/local/bin/claude --resume"));
        assert!(!applies_native_bypass("vim a.txt"));
        assert!(!applies_native_bypass("aider"));
        // Already-present flag is still recognised as a known agent (broker off).
        assert!(applies_native_bypass(
            "claude --dangerously-skip-permissions"
        ));
    }

    // The action taken at a settled prompt is a pure mapping from the broker's
    // decision; verify it without a live PTY.
    #[test]
    fn approve_injects_enter() {
        assert_eq!(decision_action(Decision::Approve), Action::Inject(b"\r"));
    }

    #[test]
    fn deny_injects_reject_then_arms_escape() {
        // Deny sends "2\r" as the primary reject; the Esc fallback is the byte
        // the loop emits on follow-up (asserted via the constant below).
        assert_eq!(decision_action(Decision::Deny), Action::Deny(b"2\r"));
    }

    #[test]
    fn escalate_injects_nothing() {
        assert_eq!(decision_action(Decision::Escalate), Action::Escalate);
    }

    // The verdict label logged for each decision must match the trajectory
    // schema's "approve" | "deny" | "escalate".
    #[test]
    fn decision_verdict_labels() {
        assert_eq!(decision_verdict(Decision::Approve), "approve");
        assert_eq!(decision_verdict(Decision::Deny), "deny");
        assert_eq!(decision_verdict(Decision::Escalate), "escalate");
    }

    // End-to-end at the decision level: a MockDecider's verdict maps to the
    // expected action, so the broker -> action wiring is correct without a PTY.
    #[test]
    fn mock_decider_drives_action_mapping() {
        let deny_rm = MockDecider::deny_if_contains("rm -rf");
        // A dangerous prompt is denied -> reject bytes.
        let danger = "Run `rm -rf /tmp`? ❯ 1. Yes  2. No";
        assert_eq!(
            decision_action(deny_rm.decide(danger, "deny rm -rf")),
            Action::Deny(b"2\r")
        );
        // A benign prompt is approved -> Enter.
        let benign = "Read file config.toml? ❯ 1. Yes  2. No";
        assert_eq!(
            decision_action(deny_rm.decide(benign, "deny rm -rf")),
            Action::Inject(b"\r")
        );

        // An explicit escalate rule -> no injection.
        let escalator = MockDecider::new(|_p, _policy| Decision::Escalate);
        assert_eq!(
            decision_action(escalator.decide(danger, "")),
            Action::Escalate
        );
    }

    #[test]
    fn find_end_parses_exit_code() {
        let text = "hello\n__MTEND_0_7__\n";
        assert_eq!(find_end(text, "__MTEND_", "_7__"), Some((6, 0)));
    }

    #[test]
    fn find_end_parses_nonzero() {
        let text = "oops\n__MTEND_127_3__\n";
        assert_eq!(find_end(text, "__MTEND_", "_3__"), Some((5, 127)));
    }

    #[test]
    fn find_end_returns_none_without_marker() {
        assert_eq!(find_end("no marker here", "__MTEND_", "_1__"), None);
    }

    #[test]
    fn find_end_ignores_wrong_counter() {
        let text = "__MTEND_0_2__";
        assert_eq!(find_end(text, "__MTEND_", "_1__"), None);
    }

    fn never() -> impl FnMut() -> bool {
        || false
    }

    #[test]
    fn auto_accept_flag_added_for_claude() {
        assert_eq!(
            with_auto_accept_flag("claude"),
            "claude --dangerously-skip-permissions"
        );
        assert_eq!(
            with_auto_accept_flag("/usr/local/bin/claude --resume"),
            "/usr/local/bin/claude --resume --dangerously-skip-permissions"
        );
        // Idempotent: don't add the flag twice.
        assert_eq!(
            with_auto_accept_flag("claude --dangerously-skip-permissions"),
            "claude --dangerously-skip-permissions"
        );
    }

    #[test]
    fn auto_accept_flag_untouched_for_unknown_program() {
        assert_eq!(with_auto_accept_flag("top"), "top");
        assert_eq!(with_auto_accept_flag("vim a.txt"), "vim a.txt");
    }

    #[test]
    fn prompt_detection() {
        assert!(looks_like_prompt("Do you want to proceed?"));
        assert!(looks_like_prompt("❯ 1. Yes\n  2. No"));
        assert!(looks_like_prompt("Overwrite? (y/n)"));
        assert!(looks_like_prompt("Press enter to continue"));
        assert!(!looks_like_prompt("just some regular output\nwith no question"));
        assert!(!looks_like_prompt(""));
    }

    // A Claude-Code-style multi-option menu with the default marked by '❯'.
    #[test]
    fn prompt_detection_option_menu() {
        let menu = "Which option would you like to pick?\n\n❯ 1. Option A\n  2. Option B\n  3. Option C\n";
        assert!(looks_like_prompt(menu));
    }

    #[test]
    fn prompt_detection_ignores_prose_and_scrollback() {
        // The bare word "approve(d)" in ordinary output is not a waiting prompt.
        assert!(!looks_like_prompt("I approved the change and moved on.\nDone.\n"));
        // A real approval prompt (question-shaped) still matches.
        assert!(looks_like_prompt("Approve this action?"));
        // A prompt far up in scrollback (not in the last lines) is not matched.
        let mut scroll = String::from("Do you want to proceed?\n");
        for i in 0..50 {
            scroll.push_str(&format!("log line {i} doing routine work\n"));
        }
        assert!(!looks_like_prompt(&scroll), "stale prompt in scrollback must not match");
    }

    #[test]
    fn shell_runs_real_command() {
        let mut sh = Shell::spawn().expect("spawn");
        let r = sh.run("echo hello123", &mut never()).expect("run echo");
        assert_eq!(r.exit_code, 0, "output: {:?}", r.output);
        assert!(!r.interrupted);
        assert!(r.output.contains("hello123"), "output: {:?}", r.output);
    }

    #[test]
    fn shell_state_persists_across_commands() {
        let mut sh = Shell::spawn().expect("spawn");
        sh.run("MTTESTVAR=persist99", &mut never()).expect("set var");
        let r = sh.run("echo \"$MTTESTVAR\"", &mut never()).expect("read var");
        assert!(r.output.contains("persist99"), "output: {:?}", r.output);
    }

    // Without the /dev/null stdin redirect, `cat` would block on the pty
    // forever; this asserts it returns promptly with EOF instead.
    #[test]
    fn shell_stdin_reader_does_not_hang() {
        let mut sh = Shell::spawn().expect("spawn");
        let r = sh.run("cat", &mut never()).expect("run cat");
        assert_eq!(r.exit_code, 0, "output: {:?}", r.output);
        assert!(r.output.is_empty(), "output: {:?}", r.output);
    }

    // A command that overruns the timeout is interrupted and the shell stays
    // usable for the next command (previously it wedged forever).
    #[test]
    fn shell_recovers_after_timeout() {
        let mut sh = Shell::spawn().expect("spawn");
        sh.set_timeout(Duration::from_secs(2));
        let hung = sh.run("sleep 30", &mut never());
        if let Ok(r) = hung {
            assert!(r.interrupted, "expected interrupted flag");
            assert_ne!(r.exit_code, 0, "expected non-zero interrupted exit code");
        }
        let r = sh
            .run("echo recovered55", &mut never())
            .expect("shell did not recover");
        assert!(r.output.contains("recovered55"), "output: {:?}", r.output);
    }

    // The user pressing Ctrl-C must stop a running command well before the
    // timeout would, and the shell must remain usable afterwards.
    #[test]
    fn shell_user_interrupt_stops_command() {
        let mut sh = Shell::spawn().expect("spawn");
        sh.set_timeout(Duration::from_secs(60));
        let start = Instant::now();
        // Request interruption on the very first poll.
        let r = sh
            .run("sleep 60", &mut || true)
            .expect("interrupt should yield a result");
        assert!(r.interrupted, "expected interrupted flag");
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "interrupt should be far faster than the 60s timeout"
        );
        let r = sh
            .run("echo back66", &mut never())
            .expect("shell did not recover");
        assert!(r.output.contains("back66"), "output: {:?}", r.output);
    }
}

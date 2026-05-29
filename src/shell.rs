use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::error::Error;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
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

pub struct Shell {
    writer: Box<dyn Write + Send>,
    rx: mpsc::Receiver<Vec<u8>>,
    counter: u64,
    command_timeout: Duration,
    child_pid: Option<u32>,
    _child: Box<dyn portable_pty::Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
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
        thread::spawn(move || {
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
            _child: child,
            master: pair.master,
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
                    // tools that grab the pty in raw mode (e.g. dbexec/isaac).
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
                    return Err("command did not terminate after repeated SIGKILL; \
                                the shell may be unusable"
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
    /// terminal so full-screen / REPL programs (isaac, vim, top, python) work
    /// for real. Forwards raw bytes both ways until the command's process tree
    /// exits, then resynchronises back to capture mode.
    ///
    /// The outer terminal must already be in raw mode.
    pub fn run_interactive(&mut self, command: &str) -> Result<(), Box<dyn Error>> {
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
        loop {
            // Inner pty -> screen.
            let mut wrote = false;
            while let Ok(chunk) = self.rx.try_recv() {
                out.write_all(&chunk)?;
                wrote = true;
            }
            if wrote {
                out.flush()?;
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
                let alive = self.child_pid.map(has_children).unwrap_or(false);
                if alive {
                    child_seen = true;
                } else if child_seen {
                    break;
                } else if start.elapsed() > Duration::from_secs(3) {
                    break; // builtin / instant command that never forked
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

/// True if `pid` currently has at least one direct child process.
fn has_children(pid: u32) -> bool {
    Command::new("pgrep")
        .arg("-P")
        .arg(pid.to_string())
        .output()
        .map(|o| o.stdout.iter().any(|b| !b.is_ascii_whitespace()))
        .unwrap_or(false)
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
        // Ignore failures: a child may have exited between the pgrep scan and
        // here. Silence stdio so "No such process" never leaks to our terminal.
        let _ = Command::new("kill")
            .arg("-KILL")
            .arg(pid.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
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

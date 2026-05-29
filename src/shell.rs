use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::error::Error;
use std::io::{Read, Write};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const READ_CHUNK_TIMEOUT: Duration = Duration::from_millis(50);
const DEFAULT_TIMEOUT_SECS: u64 = 30;
// After the timeout we SIGINT the command; this is how long we then wait for
// bash to resume and emit the end marker before declaring the shell unrecoverable.
const INTERRUPT_GRACE: Duration = Duration::from_secs(5);

pub struct Shell {
    writer: Box<dyn Write + Send>,
    rx: mpsc::Receiver<Vec<u8>>,
    counter: u64,
    command_timeout: Duration,
    _child: Box<dyn portable_pty::Child + Send + Sync>,
    _master: Box<dyn MasterPty + Send>,
}

pub struct CommandResult {
    pub output: String,
    pub exit_code: i32,
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
            _child: child,
            _master: pair.master,
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

    pub fn run(&mut self, command: &str) -> Result<CommandResult, Box<dyn Error>> {
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

        let mut acc = Vec::new();
        let mut deadline = Instant::now() + self.command_timeout;
        let mut escalation = 0u8;
        loop {
            if Instant::now() > deadline {
                // The command overran its timeout (infinite loop, or a tool
                // reading /dev/tty directly so the /dev/null redirect can't
                // help). Escalate through the terminal's signal characters,
                // then keep reading: once the command dies, bash regains
                // control, runs the trailing printf, and emits the end marker
                // (exit code 130 for SIGINT, 131 for SIGQUIT). That both
                // reports the interruption and proves the shell recovered.
                let signal_char = match escalation {
                    0 => 0x03u8, // Ctrl-C  -> SIGINT
                    1 => 0x1cu8, // Ctrl-\  -> SIGQUIT (harder to ignore)
                    _ => {
                        return Err(format!(
                            "command timed out after {}s and ignored SIGINT/SIGQUIT; \
                             moving on (the shell remains usable)",
                            self.command_timeout.as_secs()
                        )
                        .into())
                    }
                };
                self.writer.write_all(&[signal_char])?;
                self.writer.flush()?;
                escalation += 1;
                deadline = Instant::now() + INTERRUPT_GRACE;
                continue;
            }
            match self.rx.recv_timeout(READ_CHUNK_TIMEOUT) {
                Ok(chunk) => acc.extend_from_slice(&chunk),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("shell process exited".into());
                }
            }

            let text = String::from_utf8_lossy(&acc);
            if let Some((end_idx, code)) = find_end(&text, end_prefix, &end_suffix) {
                let body_start = text
                    .find(&format!("{start_marker}\n"))
                    .map(|i| i + start_marker.len() + 1)
                    .unwrap_or(0);
                let output = text[body_start..end_idx].trim_end_matches('\n').to_string();
                return Ok(CommandResult {
                    output,
                    exit_code: code,
                });
            }
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

    #[test]
    fn shell_runs_real_command() {
        let mut sh = Shell::spawn().expect("spawn");
        let r = sh.run("echo hello123").expect("run echo");
        assert_eq!(r.exit_code, 0, "output: {:?}", r.output);
        assert!(r.output.contains("hello123"), "output: {:?}", r.output);
    }

    #[test]
    fn shell_state_persists_across_commands() {
        let mut sh = Shell::spawn().expect("spawn");
        sh.run("MTTESTVAR=persist99").expect("set var");
        let r = sh.run("echo \"$MTTESTVAR\"").expect("read var");
        assert!(r.output.contains("persist99"), "output: {:?}", r.output);
    }

    // Without the /dev/null stdin redirect, `cat` would block on the pty
    // forever; this asserts it returns promptly with EOF instead.
    #[test]
    fn shell_stdin_reader_does_not_hang() {
        let mut sh = Shell::spawn().expect("spawn");
        let r = sh.run("cat").expect("run cat");
        assert_eq!(r.exit_code, 0, "output: {:?}", r.output);
        assert!(r.output.is_empty(), "output: {:?}", r.output);
    }

    // The core fix: a command that overruns the timeout is interrupted and the
    // shell stays usable for the next command (previously it wedged forever).
    #[test]
    fn shell_recovers_after_timeout() {
        let mut sh = Shell::spawn().expect("spawn");
        sh.set_timeout(Duration::from_secs(2));
        let hung = sh.run("sleep 30");
        match hung {
            Ok(r) => assert_ne!(r.exit_code, 0, "expected interrupted exit code"),
            Err(_) => {}
        }
        let r = sh.run("echo recovered55").expect("shell did not recover");
        assert!(r.output.contains("recovered55"), "output: {:?}", r.output);
    }
}

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::error::Error;
use std::io::{Read, Write};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const READ_CHUNK_TIMEOUT: Duration = Duration::from_millis(50);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

pub struct Shell {
    writer: Box<dyn Write + Send>,
    rx: mpsc::Receiver<Vec<u8>>,
    counter: u64,
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

        let mut shell = Self {
            writer,
            rx,
            counter: 0,
            _child: child,
            _master: pair.master,
        };

        shell.writer.write_all(b"stty -echo -onlcr 2>/dev/null\n")?;
        shell.writer.flush()?;
        thread::sleep(Duration::from_millis(200));
        while shell.rx.try_recv().is_ok() {}

        Ok(shell)
    }

    pub fn run(&mut self, command: &str) -> Result<CommandResult, Box<dyn Error>> {
        self.counter += 1;
        let id = self.counter;
        let start_marker = format!("__MTSTART_{id}__");
        let end_prefix = "__MTEND_";
        let end_suffix = format!("_{id}__");

        let wrapped = format!(
            "printf '%s\\n' '{start_marker}'\n{command}\nprintf '\\n{end_prefix}%d{end_suffix}\\n' $?\n"
        );

        self.writer.write_all(wrapped.as_bytes())?;
        self.writer.flush()?;

        let mut acc = Vec::new();
        let started = Instant::now();
        loop {
            if started.elapsed() > COMMAND_TIMEOUT {
                return Err("shell command timed out".into());
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
}

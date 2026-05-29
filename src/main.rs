mod backend;
mod shell;

use backend::{AnthropicBackend, Backend, MockBackend};
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    style::{Color, Print, ResetColor, SetForegroundColor},
    terminal::{disable_raw_mode, enable_raw_mode},
};
use shell::Shell;
use std::io::{stdout, BufRead, Write};

const NOT_FOUND_EXIT_CODE: i32 = 127;

fn main() {
    if let Err(e) = run() {
        let _ = disable_raw_mode();
        eprintln!("fatal: {e}");
        std::process::exit(1);
    }
    let _ = disable_raw_mode();
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let backend_kind = std::env::args().nth(1).unwrap_or_else(|| "mock".to_string());

    println!("════════════════════════════════════════════════════════════════");
    println!(" Set a SYSTEM PROMPT for this session (optional).");
    println!(" It does two things:");
    println!("   • Gives the LLM context when a typed command isn't found.");
    println!("   • Include the word \"accept\" (e.g. type:  accept requests )");
    println!("     to auto-approve prompts from interactive programs like claude.");
    println!();
    println!(" Type your prompt below. Press Enter on an EMPTY line to start.");
    println!(" (Just press Enter now to skip — no system prompt.)");
    println!("════════════════════════════════════════════════════════════════");
    let system_prompt = read_multiline_prompt()?;
    if system_prompt.is_empty() {
        println!("→ No system prompt set.");
    } else {
        println!("→ System prompt set ({} chars).", system_prompt.len());
    }

    let backend: Box<dyn Backend> = match backend_kind.as_str() {
        "anthropic" | "claude" => match AnthropicBackend::from_env() {
            Ok(b) => Box::new(b),
            Err(e) => {
                eprintln!("anthropic init failed: {e}; falling back to mock");
                Box::new(MockBackend::new())
            }
        },
        _ => Box::new(MockBackend::new()),
    };

    let mut shell = Shell::spawn()?;

    // The system prompt drives auto-accept: if it asks to accept, interactive
    // programs get auto-approved (native flag where known, else injected Enter).
    let auto_accept = wants_auto_accept(&system_prompt);

    println!(
        "mock-terminal — backend: {} — shell commands run for real; unknown commands go to the LLM.",
        backend.name()
    );
    println!(
        "Interactive programs (claude, vim, top, python…) run in passthrough; \
         prefix any command with ':' to force it."
    );
    if auto_accept {
        println!("Auto-accept ON (from system prompt): interactive prompts are auto-approved.");
    }
    println!("Ctrl-C interrupts a running command; /exit quits.");

    enable_raw_mode()?;
    let result = repl(&mut shell, &*backend, &system_prompt, auto_accept);
    disable_raw_mode()?;
    result
}

/// Auto-accept is enabled when the system prompt expresses intent to accept
/// (e.g. "accept requests", "auto-accept", "yes to everything").
fn wants_auto_accept(system_prompt: &str) -> bool {
    let p = system_prompt.to_lowercase();
    p.contains("accept") || p.contains("yes to ")
}

/// Decide whether `input` should run in interactive passthrough rather than
/// capture mode. A leading `:` forces passthrough for any command; otherwise a
/// known interactive program name (matched on the first token's basename)
/// triggers it. Returns the command to run, or `None` for capture mode.
fn interactive_command(input: &str) -> Option<&str> {
    if let Some(rest) = input.strip_prefix(':') {
        let rest = rest.trim();
        return (!rest.is_empty()).then_some(rest);
    }
    const INTERACTIVE: &[&str] = &[
        "claude", "vim", "vi", "nvim", "nano", "emacs", "top", "htop", "less", "more", "man",
        "ssh", "python", "python3", "ipython", "node", "irb", "psql", "mysql", "tmux", "screen",
    ];
    let first = input.split_whitespace().next().unwrap_or("");
    let base = first.rsplit('/').next().unwrap_or(first);
    INTERACTIVE.contains(&base).then_some(input)
}

fn read_multiline_prompt() -> std::io::Result<String> {
    let stdin = std::io::stdin();
    let mut handle = stdin.lock();
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    loop {
        // A visible indicator so it's obvious the program is waiting for input.
        print!("system prompt> ");
        std::io::stdout().flush()?;
        line.clear();
        let n = handle.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed.is_empty() {
            break;
        }
        lines.push(trimmed.to_string());
    }
    Ok(lines.join("\n"))
}

fn repl(
    shell: &mut Shell,
    backend: &dyn Backend,
    system: &str,
    auto_accept: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut out = stdout();

    // Polled by `shell.run` while a command executes. The command's own stdin is
    // /dev/null, so keystrokes here reach us, not the command — we watch for
    // Ctrl-C to tear the command down. Other keys pressed mid-command are
    // drained and ignored (you can't type the next command until this returns).
    let mut poll_ctrl_c = || -> bool {
        let mut hit = false;
        while event::poll(std::time::Duration::from_millis(0)).unwrap_or(false) {
            if let Ok(Event::Key(k)) = event::read() {
                if k.kind == KeyEventKind::Press
                    && k.code == KeyCode::Char('c')
                    && k.modifiers.contains(KeyModifiers::CONTROL)
                {
                    hit = true;
                }
            }
        }
        hit
    };

    loop {
        execute!(out, SetForegroundColor(Color::Cyan), Print("$ "), ResetColor)?;
        out.flush()?;

        let line = match read_line(&mut out)? {
            Some(l) => l,
            None => {
                execute!(out, Print("\r\n"))?;
                break;
            }
        };
        execute!(out, Print("\r\n"))?;

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == "/exit" || trimmed == "/quit" {
            break;
        }

        // Interactive programs take over the terminal: hand the inner pty
        // straight through instead of capturing output.
        if let Some(cmd) = interactive_command(trimmed) {
            shell.run_interactive(cmd, auto_accept)?;
            continue;
        }

        match shell.run(trimmed, &mut poll_ctrl_c) {
            Ok(result) => {
                if !result.output.is_empty() {
                    print_block(&mut out, &result.output)?;
                }
                if result.interrupted {
                    print_styled(&mut out, Color::Yellow, "· interrupted")?;
                } else if result.exit_code == NOT_FOUND_EXIT_CODE {
                    print_styled(
                        &mut out,
                        Color::Yellow,
                        "· not a shell command — asking the LLM",
                    )?;
                    match backend.reply(trimmed, system) {
                        Ok(reply) => print_reply(&mut out, &reply)?,
                        Err(e) => print_styled(
                            &mut out,
                            Color::Red,
                            &format!("! backend error: {e}"),
                        )?,
                    }
                }
            }
            Err(e) => print_styled(&mut out, Color::Red, &format!("! shell error: {e}"))?,
        }
    }
    Ok(())
}

fn read_line(out: &mut impl Write) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let mut buf = String::new();
    loop {
        let ev = event::read()?;
        if let Event::Key(k) = ev {
            if k.kind != KeyEventKind::Press {
                continue;
            }
            match k.code {
                KeyCode::Enter => return Ok(Some(buf)),
                KeyCode::Backspace => {
                    if buf.pop().is_some() {
                        execute!(out, cursor::MoveLeft(1), Print(' '), cursor::MoveLeft(1))?;
                        out.flush()?;
                    }
                }
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    // Ctrl-C at the prompt exits the terminal. (While a command
                    // is running it's handled separately to interrupt it.)
                    execute!(out, Print("^C"))?;
                    return Ok(None);
                }
                KeyCode::Char('d')
                    if k.modifiers.contains(KeyModifiers::CONTROL) && buf.is_empty() =>
                {
                    // Ctrl-D on an empty line is EOF -> also exit.
                    return Ok(None);
                }
                KeyCode::Char(c) => {
                    buf.push(c);
                    execute!(out, Print(c))?;
                    out.flush()?;
                }
                _ => {}
            }
        }
    }
}

fn print_block(out: &mut impl Write, text: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut first = true;
    for line in text.split('\n') {
        if !first {
            execute!(out, Print("\r\n"))?;
        }
        execute!(out, Print(line))?;
        first = false;
    }
    execute!(out, Print("\r\n"))?;
    out.flush()?;
    Ok(())
}

fn print_reply(out: &mut impl Write, reply: &str) -> Result<(), Box<dyn std::error::Error>> {
    execute!(out, SetForegroundColor(Color::Green), Print("« "), ResetColor)?;
    let mut first = true;
    for line in reply.split('\n') {
        if !first {
            execute!(out, Print("\r\n  "))?;
        }
        execute!(out, Print(line))?;
        first = false;
    }
    execute!(out, Print("\r\n"))?;
    out.flush()?;
    Ok(())
}

fn print_styled(
    out: &mut impl Write,
    color: Color,
    msg: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    execute!(
        out,
        SetForegroundColor(color),
        Print(msg),
        ResetColor,
        Print("\r\n"),
    )?;
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{interactive_command, wants_auto_accept};

    #[test]
    fn auto_accept_follows_system_prompt() {
        assert!(wants_auto_accept("accept requests"));
        assert!(wants_auto_accept("Please AUTO-ACCEPT everything"));
        assert!(wants_auto_accept("say yes to all prompts"));
        assert!(!wants_auto_accept("be terse and helpful"));
        assert!(!wants_auto_accept(""));
    }

    #[test]
    fn known_interactive_program_routes_to_passthrough() {
        assert_eq!(interactive_command("claude"), Some("claude"));
        assert_eq!(interactive_command("vim file.txt"), Some("vim file.txt"));
        assert_eq!(interactive_command("/usr/bin/python3"), Some("/usr/bin/python3"));
    }

    #[test]
    fn ordinary_command_stays_in_capture_mode() {
        assert_eq!(interactive_command("ls -la"), None);
        assert_eq!(interactive_command("echo hi"), None);
        // Substring of a known name must not match.
        assert_eq!(interactive_command("vimdiff a b"), None);
    }

    #[test]
    fn colon_prefix_forces_passthrough() {
        assert_eq!(interactive_command(":ls -la"), Some("ls -la"));
        assert_eq!(interactive_command(": cat"), Some("cat"));
        assert_eq!(interactive_command(":"), None);
    }
}

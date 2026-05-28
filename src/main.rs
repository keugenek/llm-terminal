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

    println!("System prompt — used when shell can't find a command.");
    println!("Type one or more lines, then submit an empty line. Leave blank for none.");
    let system_prompt = read_multiline_prompt()?;

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

    println!(
        "mock-terminal — backend: {} — shell commands run for real; unknown commands go to the LLM. /exit to quit.",
        backend.name()
    );

    enable_raw_mode()?;
    let result = repl(&mut shell, &*backend, &system_prompt);
    disable_raw_mode()?;
    result
}

fn read_multiline_prompt() -> std::io::Result<String> {
    let stdin = std::io::stdin();
    let mut handle = stdin.lock();
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    loop {
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
) -> Result<(), Box<dyn std::error::Error>> {
    let mut out = stdout();
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

        match shell.run(trimmed) {
            Ok(result) => {
                if !result.output.is_empty() {
                    print_block(&mut out, &result.output)?;
                }
                if result.exit_code == NOT_FOUND_EXIT_CODE {
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
                    execute!(out, Print("^C"))?;
                    return Ok(None);
                }
                KeyCode::Char('d')
                    if k.modifiers.contains(KeyModifiers::CONTROL) && buf.is_empty() =>
                {
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

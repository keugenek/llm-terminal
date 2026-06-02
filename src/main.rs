mod backend;
mod decider;
mod manifest;
mod registry;
mod shell;
mod trajectory;

use backend::{AnthropicBackend, Backend, MockBackend};
use decider::{AlwaysApprove, Decider, HaikuDecider};
use manifest::Manifest;
use trajectory::Trajectory;
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
    // `open <file-or-url>` launches a pre-configured session from a JSON manifest
    // (read from a local path or fetched over HTTP(S)): show a session card,
    // confirm, then boot wired up as the manifest says. Checked first so `open`
    // is unambiguous as a subcommand. Everything else is the interactive REPL.
    if std::env::args().nth(1).as_deref() == Some("open") {
        let source = std::env::args()
            .nth(2)
            .ok_or("usage: open <manifest.json | https://…> [--yes | --confirm]")?;
        // --yes skips the launch prompt outright; --confirm forces it even for a
        // local file. Default: confirm only URL-sourced manifests.
        let force_yes = std::env::args().skip(3).any(|a| a == "--yes" || a == "-y");
        let force_confirm = std::env::args().skip(3).any(|a| a == "--confirm");
        return run_manifest(&source, force_yes, force_confirm);
    }
    // `ps` lists tracked sessions (one-shot, or live with --watch).
    if std::env::args().nth(1).as_deref() == Some("ps") {
        let watch = std::env::args().skip(2).any(|a| a == "--watch" || a == "-w");
        return run_ps(watch);
    }
    run_interactive_session(std::env::args().nth(1).unwrap_or_else(|| "mock".to_string()))
}

/// Load a manifest from a local path or, when `source` looks like an HTTP(S)
/// URL, by fetching it with the ureq client already in the tree. Either way the
/// caller still shows the card and requires confirmation before running.
fn load_manifest(source: &str) -> Result<Manifest, Box<dyn std::error::Error>> {
    if source.starts_with("http://") || source.starts_with("https://") {
        let body = ureq::get(source).call()?.into_string()?;
        Manifest::from_json(&body).map_err(|e| format!("manifest {source}: {e}").into())
    } else {
        Manifest::from_file(std::path::Path::new(source))
    }
}

/// `open <file-or-url>`: parse the manifest, show the session card, then boot a
/// session configured FROM the manifest — system prompt, backend, broker, cwd,
/// setup commands, and the `run` launch (with `instructions` delivered to the
/// agent) — before dropping into the REPL.
///
/// A local manifest you invoked `open` on directly runs without a prompt (you
/// chose the file). A manifest fetched from a URL always requires confirmation —
/// that's the remote-exec safety gate. `--yes` skips the prompt; `--confirm`
/// forces it for any source.
fn run_manifest(
    source: &str,
    force_yes: bool,
    force_confirm: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let m = load_manifest(source)?;
    let is_url = source.starts_with("http://") || source.starts_with("https://");

    // Auto-accept is derived from the system prompt, exactly as the interactive
    // session does it — never a separate manifest field.
    let auto_accept = wants_auto_accept(&m.system_prompt);

    // Always show the card (so there's a record of what's about to run); only
    // block for confirmation when required (URL source, or --confirm).
    print!("{}", manifest::render_card(&m, auto_accept));
    std::io::stdout().flush()?;
    if confirmation_required(is_url, force_yes, force_confirm) {
        // Never auto-confirm a non-interactive stdin. At EOF (piped, `</dev/null`,
        // CI/cron) `read_line` returns Ok(0) with an empty buffer, which
        // `is_confirmed` would treat as a bare-Enter "yes" — silently bypassing
        // the gate for exactly the remote (URL) manifests it's meant to protect.
        // Require a real terminal here (or an explicit `--yes`).
        use std::io::IsTerminal;
        if !std::io::stdin().is_terminal() {
            println!(
                "→ Refusing to run without confirmation: stdin is not a terminal. \
                 Run it interactively, or pass --yes to confirm explicitly."
            );
            return Ok(());
        }
        print!("Launch this session? [Y/n] ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        let n = std::io::stdin().lock().read_line(&mut answer)?;
        // n == 0 is EOF (not a deliberate Enter) → abort.
        if n == 0 || !manifest::is_confirmed(&answer) {
            println!("→ Aborted; nothing was run.");
            return Ok(());
        }
    } else {
        println!("→ auto-confirmed (local manifest; pass --confirm to require a prompt).");
    }

    let backend = build_backend(&m.backend);
    let decider = build_decider(auto_accept);

    let mut shell = Shell::spawn()?;
    // Register the session so it appears in `mock-terminal ps`. Label it with the
    // manifest name, falling back to the source if unnamed.
    let session_name = if m.name.is_empty() { source } else { &m.name };
    let trajectory = Trajectory::new_session(session_name, "open", source);
    trajectory.log_manifest(&m.name);

    println!(
        "mock-terminal — backend: {} — launching session from manifest.",
        backend.name()
    );
    if auto_accept {
        println!(
            "Auto-accept ON (from system prompt): prompts graded by the {} policy broker.",
            decider_name(&*decider)
        );
    }

    // cwd first (a real captured command, so the cd persists into setup + run).
    if let Some(cwd) = &m.cwd {
        let mut never = || false;
        let _ = shell.run(&format!("cd {}", shell_quote(cwd)), &mut never);
    }

    // Setup runs in capture mode (output shown) so the user sees preparation
    // results before the main launch. A failing setup command is surfaced but
    // does not abort — the user already approved the whole plan.
    for cmd in &m.setup {
        println!("setup $ {cmd}");
        let mut never = || false;
        match shell.run(cmd, &mut never) {
            Ok(result) => {
                trajectory.log_command(cmd, result.exit_code, result.interrupted);
                if !result.output.is_empty() {
                    println!("{}", result.output);
                }
            }
            Err(e) => eprintln!("setup error: {e}"),
        }
    }

    enable_raw_mode()?;
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        if let Some(run) = &m.run {
            // Deliver `instructions` to the agent: via its native prompt flag
            // where known (claude/codex), else by keystroke injection once the
            // agent's input settles. With no instructions, launch bare.
            let (launch_cmd, inject) = if m.instructions.is_empty() {
                (run.clone(), false)
            } else {
                manifest::agent_launch(run, &m.instructions)
            };
            let pending = inject.then_some(m.instructions.as_str());

            if let Some(cmd) = interactive_command(&launch_cmd) {
                trajectory.log_interactive(cmd);
                shell.run_interactive(
                    cmd,
                    auto_accept,
                    &*decider,
                    &m.system_prompt,
                    &trajectory,
                    pending,
                )?;
            } else {
                let mut never = || false;
                let res = shell.run(&launch_cmd, &mut never)?;
                trajectory.log_command(&launch_cmd, res.exit_code, res.interrupted);
                if !res.output.is_empty() {
                    let mut out = stdout();
                    print_block(&mut out, &res.output)?;
                }
            }
        }
        // Continue interactively after the manifest launch.
        repl(
            &mut shell,
            &*backend,
            &m.system_prompt,
            auto_accept,
            &*decider,
            &trajectory,
        )
    })();
    disable_raw_mode()?;
    result
}

/// Single-quote a path for safe use as one shell word (cwd may contain spaces).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Whether `open` must stop and ask before running. A local manifest the user
/// invoked directly runs without a prompt; a manifest fetched from a URL always
/// confirms (the remote-exec safety gate). `--yes` forces no prompt; `--confirm`
/// forces one for any source.
fn confirmation_required(is_url: bool, force_yes: bool, force_confirm: bool) -> bool {
    if force_yes {
        false
    } else if force_confirm {
        true
    } else {
        is_url
    }
}

/// `ps`: print the tracked-session table once, or — with `--watch` — clear the
/// screen and refresh it every second until Ctrl-C. Sessions self-report to the
/// registry (`~/.til/sessions/`); `registry::list` reconciles them against live
/// pids so a closed/crashed window shows as `dead`.
fn run_ps(watch: bool) -> Result<(), Box<dyn std::error::Error>> {
    let mut out = stdout();
    loop {
        if watch {
            // Clear screen + home cursor (standard ANSI, like `watch`/`top`).
            write!(out, "\x1b[2J\x1b[H")?;
            writeln!(out, "mock-terminal sessions — refresh 1s, Ctrl-C to stop\n")?;
        }
        write!(out, "{}", registry::render_table(&registry::list(), now_ms()))?;
        out.flush()?;
        if !watch {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

/// Milliseconds since the Unix epoch (for session ages in `ps`).
fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Build the LLM-fallback backend for the session. "anthropic"/"claude" use the
/// real API client, falling back to mock (with a notice) when it can't init;
/// anything else is the mock backend. Shared by every entry point so the backend
/// selection stays identical across interactive, task, and manifest modes.
fn build_backend(kind: &str) -> Box<dyn Backend> {
    match kind {
        "anthropic" | "claude" => match AnthropicBackend::from_env() {
            Ok(b) => Box::new(b),
            Err(e) => {
                eprintln!("anthropic init failed: {e}; falling back to mock");
                Box::new(MockBackend::new())
            }
        },
        _ => Box::new(MockBackend::new()),
    }
}

/// Build the auto-accept policy broker. When auto-accept is on we prefer the
/// model-graded Haiku decider, falling back to blind AlwaysApprove if there's no
/// API key (preserving the old behaviour rather than failing to auto-accept).
/// When auto-accept is off the decider is never consulted, so a cheap
/// AlwaysApprove placeholder is fine. Shared across all entry points.
fn build_decider(auto_accept: bool) -> Box<dyn Decider> {
    if auto_accept {
        match HaikuDecider::from_env() {
            Ok(d) => Box::new(d),
            Err(_) => {
                println!("auto-accept: no API key, approving by default");
                Box::new(AlwaysApprove)
            }
        }
    } else {
        Box::new(AlwaysApprove)
    }
}

fn run_interactive_session(backend_kind: String) -> Result<(), Box<dyn std::error::Error>> {
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

    let backend = build_backend(&backend_kind);

    let mut shell = Shell::spawn()?;

    // The system prompt drives auto-accept: if it asks to accept, interactive
    // programs get auto-approved (native flag where known, else a graded
    // decision on each prompt). When on, build the policy broker (model-graded
    // Haiku, or blind AlwaysApprove without an API key).
    let auto_accept = wants_auto_accept(&system_prompt);
    let decider = build_decider(auto_accept);

    println!(
        "mock-terminal — backend: {} — shell commands run for real; unknown commands go to the LLM.",
        backend.name()
    );
    println!(
        "Interactive programs (claude, vim, top, python…) run in passthrough; \
         prefix any command with ':' to force it."
    );
    if auto_accept {
        println!(
            "Auto-accept ON (from system prompt): prompts graded by the {} policy broker.",
            decider_name(&*decider)
        );
    }
    println!("Ctrl-C interrupts a running command; /exit quits.");

    // One trajectory log + registry record per session, threaded into the REPL
    // and (via it) the interactive loop. Both degrade to no-ops if their files
    // can't be opened, so they never block starting the terminal.
    let trajectory = Trajectory::new_session("interactive session", "interactive", "");

    enable_raw_mode()?;
    let result = repl(
        &mut shell,
        &*backend,
        &system_prompt,
        auto_accept,
        &*decider,
        &trajectory,
    );
    disable_raw_mode()?;
    result
}

/// A short label for the active decider, shown in the banner so it's clear
/// whether prompts are being model-graded or blindly approved.
fn decider_name(decider: &dyn Decider) -> &'static str {
    // The escalation reason is empty before any decision for AlwaysApprove (it
    // never escalates); distinguish on type via a downcast-free marker instead.
    if decider.is_model_graded() {
        "Haiku"
    } else {
        "always-approve"
    }
}

/// Auto-accept is enabled when the system prompt expresses intent to accept
/// (e.g. "accept requests", "auto-accept", "yes to everything").
fn wants_auto_accept(system_prompt: &str) -> bool {
    let p = system_prompt.to_lowercase();
    // Match an affirmative accept-intent, but skip an occurrence immediately
    // negated (e.g. "do NOT accept", "never accept", "don't accept") so a
    // cautious prompt doesn't silently enable auto-accept.
    for kw in ["auto-accept", "auto accept", "accept", "yes to "] {
        let mut from = 0;
        while let Some(rel) = p[from..].find(kw) {
            let idx = from + rel;
            let before = p[..idx].trim_end();
            let negated = ["not", "n't", "never", "dont", "no"]
                .iter()
                .any(|neg| before.ends_with(neg));
            if !negated {
                return true;
            }
            from = idx + kw.len();
        }
    }
    false
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
    let first = input.split_whitespace().next().unwrap_or("");
    let base = first.rsplit('/').next().unwrap_or(first);
    let extra = std::env::var("MT_INTERACTIVE").unwrap_or_default();
    name_is_interactive(base, &extra).then_some(input)
}

/// Is `base` (a program basename) one we should run in passthrough? Built-in
/// list plus any names from `extra` (the `MT_INTERACTIVE` env var, comma/space
/// separated) — so you can route local interactive tools to passthrough without
/// hardcoding their names in the source. Pure for testing.
fn name_is_interactive(base: &str, extra: &str) -> bool {
    const INTERACTIVE: &[&str] = &[
        "claude", "vim", "vi", "nvim", "nano", "emacs", "top", "htop", "less", "more", "man",
        "ssh", "python", "python3", "ipython", "node", "irb", "psql", "mysql", "tmux", "screen",
    ];
    INTERACTIVE.contains(&base)
        || extra
            .split([',', ' '])
            .any(|n| !n.is_empty() && n == base)
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
    decider: &dyn Decider,
    trajectory: &Trajectory,
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
        // straight through instead of capturing output. The system prompt is
        // the policy the broker grades each settled prompt against.
        if let Some(cmd) = interactive_command(trimmed) {
            trajectory.log_interactive(cmd);
            shell.run_interactive(cmd, auto_accept, decider, system, trajectory, None)?;
            continue;
        }

        match shell.run(trimmed, &mut poll_ctrl_c) {
            Ok(result) => {
                trajectory.log_command(trimmed, result.exit_code, result.interrupted);
                if !result.output.is_empty() {
                    print_block(&mut out, &result.output)?;
                }
                if result.interrupted {
                    print_styled(&mut out, Color::Yellow, "· interrupted")?;
                } else if result.exit_code == NOT_FOUND_EXIT_CODE {
                    trajectory.log_llm_fallback(trimmed);
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
    use super::{confirmation_required, interactive_command, wants_auto_accept};

    #[test]
    fn local_manifest_runs_without_prompt_url_confirms() {
        // Local file, no flags → no prompt.
        assert!(!confirmation_required(false, false, false));
        // URL, no flags → confirm (the remote-exec gate).
        assert!(confirmation_required(true, false, false));
        // --yes skips the prompt even for a URL.
        assert!(!confirmation_required(true, true, false));
        // --confirm forces a prompt even for a local file.
        assert!(confirmation_required(false, false, true));
        // --yes wins over --confirm.
        assert!(!confirmation_required(true, true, true));
    }

    #[test]
    fn auto_accept_follows_system_prompt() {
        assert!(wants_auto_accept("accept requests"));
        assert!(wants_auto_accept("Please AUTO-ACCEPT everything"));
        assert!(wants_auto_accept("say yes to all prompts"));
        assert!(!wants_auto_accept("be terse and helpful"));
        assert!(!wants_auto_accept(""));
        // Negated mentions must NOT enable auto-accept.
        assert!(!wants_auto_accept("do NOT accept destructive edits"));
        assert!(!wants_auto_accept("never accept anything risky"));
        assert!(!wants_auto_accept("don't accept without asking"));
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

    #[test]
    fn mt_interactive_extends_the_list() {
        use super::name_is_interactive;
        // Built-ins always match, regardless of the extra list.
        assert!(name_is_interactive("claude", ""));
        // A local tool not built in is routed once added via MT_INTERACTIVE.
        assert!(!name_is_interactive("mytool", ""));
        assert!(name_is_interactive("mytool", "mytool,othertool"));
        assert!(name_is_interactive("othertool", "mytool othertool")); // space-sep too
        // Non-members still don't match.
        assert!(!name_is_interactive("ls", "mytool"));
    }
}

use std::error::Error;
use std::path::Path;

/// A pre-configured, shareable session. `open <file-or-url>` reads one of these
/// (from a local path or over HTTP(S)), shows the user a session card, and —
/// once confirmed — boots a session wired up exactly as the manifest says
/// instead of prompting interactively for a system prompt. The same file can be
/// handed to `scripts/spawn.sh` to launch it in a fresh terminal window.
///
/// The manifest is a small, hand-parsed JSON document (via `serde_json::Value`,
/// already a dependency) so the dependency footprint stays at zero. Every field
/// is optional with a sensible default, so an empty `{}` still parses into a
/// usable (if inert) manifest.
///
/// Auto-accept is NOT a manifest field: it is derived from `system_prompt` the
/// same way the interactive session derives it (see `wants_auto_accept`), so the
/// policy and its accept-intent stay in one place.
///
/// Out of scope for now (future work, deliberately not built here):
/// signing/verifying manifests, injecting secrets or env vars, and a `til://`
/// URL scheme. (URL fetch, `instructions` injection, `cwd`, and window-spawning
/// are implemented.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// Human-readable label for the session, shown on the card.
    pub name: String,
    /// The session's system prompt. This is also the policy the auto-accept
    /// broker grades prompts against, and what drives auto-accept on/off.
    pub system_prompt: String,
    /// The task handed to the launched agent. Shown on the card, and — when
    /// `run` is set — delivered to the agent (via its native prompt flag where
    /// known, else keystroke injection; see `agent_launch`).
    pub instructions: String,
    /// LLM-fallback backend kind: "mock" or "anthropic". Defaults to "mock".
    pub backend: String,
    /// Blind auto-approve: answer EVERY interactive prompt with no policy
    /// grading (a deliberate rubber stamp, like MT_AUTO_APPROVE). Implies
    /// auto-accept. Defaults to false; disclosed loudly on the card.
    pub auto_approve: bool,
    /// Optional working directory to `cd` into before setup/run, so the launched
    /// agent operates in the right repo. `None` when absent.
    pub cwd: Option<String>,
    /// Commands to run in capture mode (output shown) before `run`, e.g. to
    /// prepare the workspace. Empty when absent.
    pub setup: Vec<String>,
    /// The command to launch after setup (interactive or captured). `None` when
    /// absent, in which case the session goes straight to the interactive REPL.
    pub run: Option<String>,
    /// Auto-advance: after the initial `instructions` are delivered, how many
    /// follow-up continuation prompts to auto-type each time the agent goes idle
    /// at its input box (finished a turn, no permission prompt up). 0 disables
    /// it (default). Lets an agent keep progressing toward the task hands-free.
    pub auto_advance: usize,
}

impl Manifest {
    /// Read and parse a manifest from disk, with a clear error on a missing file
    /// or invalid JSON so the caller can exit nonzero with a useful message.
    pub fn from_file(path: &Path) -> Result<Self, Box<dyn Error>> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read manifest {}: {e}", path.display()))?;
        Self::from_json(&text).map_err(|e| format!("manifest {}: {e}", path.display()).into())
    }

    /// Parse a manifest from a JSON string. Returns an error for malformed JSON
    /// or for a top-level value that isn't a JSON object. Every field is
    /// optional; missing fields take their defaults (backend "mock", empty
    /// strings/lists, no `run`).
    pub fn from_json(text: &str) -> Result<Self, Box<dyn Error>> {
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?;
        let obj = value
            .as_object()
            .ok_or("manifest must be a JSON object")?;

        // Strings default to empty; an explicitly-present non-string is a clear
        // mistake worth rejecting rather than silently coercing.
        let string_field = |key: &str| -> Result<String, Box<dyn Error>> {
            match obj.get(key) {
                None | Some(serde_json::Value::Null) => Ok(String::new()),
                Some(serde_json::Value::String(s)) => Ok(s.clone()),
                Some(_) => Err(format!("field `{key}` must be a string").into()),
            }
        };

        let name = string_field("name")?;
        let system_prompt = string_field("system_prompt")?;
        let instructions = string_field("instructions")?;

        // Backend defaults to "mock"; an empty/absent value also means "mock".
        let backend = match string_field("backend")? {
            s if s.is_empty() => "mock".to_string(),
            s => s,
        };

        // cwd is optional; an empty string means "unset".
        let cwd = match string_field("cwd")? {
            s if s.is_empty() => None,
            s => Some(s),
        };

        let auto_approve = match obj.get("auto_approve") {
            None | Some(serde_json::Value::Null) => false,
            Some(serde_json::Value::Bool(b)) => *b,
            Some(_) => return Err("field `auto_approve` must be a boolean".into()),
        };

        // auto_advance: a non-negative count of follow-up nudges. Reject
        // negatives / non-integers / absurdly large values rather than coerce.
        let auto_advance = match obj.get("auto_advance") {
            None | Some(serde_json::Value::Null) => 0,
            Some(serde_json::Value::Number(n)) => match n.as_u64() {
                Some(v) if v <= MAX_AUTO_ADVANCE as u64 => v as usize,
                Some(_) => {
                    return Err(format!(
                        "field `auto_advance` must be between 0 and {MAX_AUTO_ADVANCE}"
                    )
                    .into())
                }
                None => return Err("field `auto_advance` must be a non-negative integer".into()),
            },
            Some(_) => return Err("field `auto_advance` must be a non-negative integer".into()),
        };

        let setup = match obj.get("setup") {
            None | Some(serde_json::Value::Null) => Vec::new(),
            Some(serde_json::Value::Array(items)) => items
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| "field `setup` must be an array of strings".into())
                })
                .collect::<Result<Vec<String>, Box<dyn Error>>>()?,
            Some(_) => return Err("field `setup` must be an array of strings".into()),
        };

        let run = match obj.get("run") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(s)) if s.is_empty() => None,
            Some(serde_json::Value::String(s)) => Some(s.clone()),
            Some(_) => return Err("field `run` must be a string".into()),
        };

        Ok(Manifest {
            name,
            system_prompt,
            instructions,
            backend,
            auto_approve,
            cwd,
            setup,
            run,
            auto_advance,
        })
    }
}

/// Upper bound on `auto_advance` — a sanity cap so a typo can't drive an agent
/// indefinitely. Generous; real use is a handful of nudges.
pub const MAX_AUTO_ADVANCE: usize = 50;

/// Build up to `max` follow-up continuation prompts from the session's
/// `instructions`, typed one at a time each time the agent goes idle (see
/// `Shell::run_interactive`). The wording nudges the agent to keep progressing
/// and to wind down cleanly (run validation / summarize) rather than spin
/// forever — the count is the hard stop, this is the soft one.
///
/// Kept deterministic (no LLM) so unattended runs need no extra API calls and
/// the behaviour is predictable. The original task is echoed once for context.
pub fn advance_followups(instructions: &str, max: usize) -> Vec<String> {
    if max == 0 {
        return Vec::new();
    }
    let task = instructions.trim();
    let mut ladder: Vec<String> = Vec::new();
    ladder.push(
        "Continue working toward the task. If a step is ambiguous, pick the most reasonable \
         option and proceed rather than stopping to ask."
            .to_string(),
    );
    if !task.is_empty() {
        ladder.push(format!(
            "Keep going. For reference, the original task is: {task}"
        ));
    }
    ladder.push(
        "Review what you've done so far, fix anything incomplete or broken, and continue."
            .to_string(),
    );
    ladder.push(
        "If the task is complete, run any available tests or validation and report the result. \
         If it isn't, keep going."
            .to_string(),
    );
    // Pad with a steady, wind-down-friendly nudge if more advances are allowed
    // than the bespoke ladder has rungs.
    let steady = "Continue toward completing the task. If everything is done and verified, \
                  summarize what you changed and stop.";
    while ladder.len() < max {
        ladder.push(steady.to_string());
    }
    ladder.truncate(max);
    ladder
}

/// Build the command that launches `agent` with `task` delivered. Preferred path
/// (mirrors `with_auto_accept_flag`): use the agent's own prompt-passing form,
/// which is far more reliable than injecting keystrokes into a startup TUI.
///
/// Returns `(command, injected)`: the shell command to launch, and whether the
/// task still needs to be typed in as keystrokes (true only for agents we have
/// no native prompt form for). The task is single-quoted for the shell.
pub fn agent_launch(agent: &str, task: &str) -> (String, bool) {
    let base = agent.split_whitespace().next().unwrap_or(agent);
    let base = base.rsplit('/').next().unwrap_or(base);
    let quoted = shell_single_quote(task);
    match base {
        // `claude "<prompt>"` seeds the first user turn from argv.
        "claude" => (format!("{agent} {quoted}"), false),
        // `codex exec "<prompt>"` runs a one-shot task.
        "codex" => (format!("{agent} exec {quoted}"), false),
        // Unknown agent: launch bare and inject the task as keystrokes once its
        // input prompt settles.
        _ => (agent.to_string(), true),
    }
}

/// Wrap `s` in single quotes for safe use as one shell word, escaping embedded
/// single quotes via the `'\''` idiom. Newlines survive inside single quotes, so
/// multi-line task text passes through intact.
fn shell_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Render the session card: a clearly-formatted summary the user reviews before
/// anything runs. This is the safety primitive — we never execute a manifest
/// without showing this and getting confirmation. Kept pure (manifest in, string
/// out) so it can be unit-tested without a terminal.
///
/// `auto_accept` and `auto_advance` are passed in (rather than read off `m`) so
/// the card always shows the exact values the session will use — both are
/// resolved once by the caller (auto-accept from the system prompt; auto-advance
/// from the manifest field after any `MT_AUTO_ADVANCE` override).
pub fn render_card(m: &Manifest, auto_accept: bool, auto_advance: usize) -> String {
    let mut s = String::new();
    s.push_str("════════════════════════════════════════════════════════════════\n");
    s.push_str(" SESSION TO LAUNCH — review before confirming\n");
    s.push_str("════════════════════════════════════════════════════════════════\n");

    let name = if m.name.is_empty() {
        "(unnamed)".to_string()
    } else {
        sanitize_line(&m.name)
    };
    s.push_str(&format!(" name:        {name}\n"));
    s.push_str(&format!(" backend:     {}\n", sanitize_line(&m.backend)));
    if let Some(cwd) = &m.cwd {
        s.push_str(&format!(" cwd:         {}\n", sanitize_line(cwd)));
    }
    // Disclose the actual consequence of auto-accept ON: known agents launch
    // with their permission-bypass flag. The user is approving THAT, so say so.
    // auto_approve is even louder — it rubber-stamps EVERY prompt with no grading.
    if m.auto_approve {
        s.push_str(" auto-accept: ON — ⚠ BLIND: every prompt approved with NO\n");
        s.push_str("              policy grading (auto_approve), plus agents run\n");
        s.push_str("              with permissions bypassed.\n");
    } else if auto_accept {
        s.push_str(" auto-accept: ON  (agents run with permissions bypassed,\n");
        s.push_str("              e.g. claude --dangerously-skip-permissions)\n");
    } else {
        s.push_str(" auto-accept: off\n");
    }

    s.push_str(" system prompt (= policy):\n");
    if m.system_prompt.is_empty() {
        s.push_str("   (none)\n");
    } else {
        for line in sanitize_block(&m.system_prompt).lines() {
            s.push_str(&format!("   {line}\n"));
        }
    }

    if !m.instructions.is_empty() {
        s.push_str(" instructions:\n");
        for line in sanitize_block(&m.instructions).lines() {
            s.push_str(&format!("   {line}\n"));
        }
    }

    s.push_str(" setup commands:\n");
    if m.setup.is_empty() {
        s.push_str("   (none)\n");
    } else {
        for cmd in &m.setup {
            s.push_str(&format!("   $ {}\n", sanitize_line(cmd)));
        }
    }

    match &m.run {
        Some(run) => s.push_str(&format!(" run:         $ {}\n", sanitize_line(run))),
        None => s.push_str(" run:         (none — drops straight into the prompt)\n"),
    }

    // Auto-advance is consequential (the agent gets nudged forward unattended),
    // so disclose the count on the card the user reviews. The caller passes the
    // *resolved* count (manifest field after any MT_AUTO_ADVANCE override) so the
    // card reflects what will actually happen.
    if auto_advance > 0 {
        s.push_str(&format!(
            " auto-advance: ON — up to {auto_advance} follow-up prompt(s) auto-typed when idle\n"
        ));
    }

    s.push_str("════════════════════════════════════════════════════════════════\n");
    // The "Launch this session? [Y/n]" prompt is printed by the caller, and only
    // when confirmation is actually required (URL source or --confirm).
    s
}

/// Strip ALL control characters from a single-line card field. The card is the
/// security primitive the user reviews before confirming, so a manifest must not
/// be able to inject ANSI/cursor/`\r`/newline sequences to forge or hide lines.
fn sanitize_line(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// Like [`sanitize_line`] but keeps `\n` for multi-line fields (the card splits
/// them into indented lines itself); every other control char is dropped.
fn sanitize_block(s: &str) -> String {
    s.chars().filter(|c| *c == '\n' || !c.is_control()).collect()
}

/// The confirm gate: does this stdin line authorise launching the session?
///
/// We treat "yes" as the default (empty line / bare Enter) since the user
/// explicitly invoked `til open <file>`; only an affirmative or empty answer
/// proceeds. Anything else (n/no/abort/anything unexpected) aborts, erring on
/// the side of NOT running. Kept pure so it can be unit-tested.
pub fn is_confirmed(input: &str) -> bool {
    matches!(input.trim().to_lowercase().as_str(), "" | "y" | "yes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_manifest_parses_all_fields() {
        let json = r#"{
            "name": "fix flaky tests",
            "system_prompt": "auto-approve reads and tests; DENY anything with rm -rf",
            "instructions": "Find and fix the flaky test, then run the suite.",
            "backend": "anthropic",
            "setup": ["echo preparing", "ls"],
            "run": "claude"
        }"#;
        let m = Manifest::from_json(json).expect("parse");
        assert_eq!(m.name, "fix flaky tests");
        assert_eq!(
            m.system_prompt,
            "auto-approve reads and tests; DENY anything with rm -rf"
        );
        assert_eq!(m.instructions, "Find and fix the flaky test, then run the suite.");
        assert_eq!(m.backend, "anthropic");
        assert_eq!(m.setup, vec!["echo preparing", "ls"]);
        assert_eq!(m.run.as_deref(), Some("claude"));
    }

    #[test]
    fn empty_manifest_gets_defaults() {
        let m = Manifest::from_json("{}").expect("parse empty");
        assert_eq!(m.name, "");
        assert_eq!(m.system_prompt, "");
        assert_eq!(m.instructions, "");
        // Backend defaults to mock when absent.
        assert_eq!(m.backend, "mock");
        assert!(m.setup.is_empty());
        assert_eq!(m.run, None);
    }

    #[test]
    fn invalid_json_returns_error() {
        // Trailing junk / unclosed brace is not valid JSON.
        assert!(Manifest::from_json("{ not json").is_err());
        assert!(Manifest::from_json("").is_err());
        // A valid JSON value that isn't an object is also rejected.
        assert!(Manifest::from_json("[1, 2, 3]").is_err());
        assert!(Manifest::from_json("\"just a string\"").is_err());
    }

    #[test]
    fn wrong_field_types_are_rejected() {
        assert!(Manifest::from_json(r#"{"name": 42}"#).is_err());
        assert!(Manifest::from_json(r#"{"setup": "not an array"}"#).is_err());
        assert!(Manifest::from_json(r#"{"setup": [1, 2]}"#).is_err());
        assert!(Manifest::from_json(r#"{"run": ["claude"]}"#).is_err());
    }

    #[test]
    fn null_and_empty_run_mean_no_run() {
        assert_eq!(Manifest::from_json(r#"{"run": null}"#).unwrap().run, None);
        assert_eq!(Manifest::from_json(r#"{"run": ""}"#).unwrap().run, None);
    }

    #[test]
    fn auto_advance_parses_and_defaults() {
        assert_eq!(Manifest::from_json("{}").unwrap().auto_advance, 0);
        assert_eq!(
            Manifest::from_json(r#"{"auto_advance": 3}"#).unwrap().auto_advance,
            3
        );
        assert_eq!(
            Manifest::from_json(r#"{"auto_advance": null}"#).unwrap().auto_advance,
            0
        );
    }

    #[test]
    fn auto_advance_rejects_bad_values() {
        assert!(Manifest::from_json(r#"{"auto_advance": -1}"#).is_err());
        assert!(Manifest::from_json(r#"{"auto_advance": "3"}"#).is_err());
        assert!(Manifest::from_json(r#"{"auto_advance": 1.5}"#).is_err());
        assert!(Manifest::from_json(r#"{"auto_advance": 9999}"#).is_err());
    }

    #[test]
    fn advance_followups_count_and_content() {
        assert!(advance_followups("do the thing", 0).is_empty());

        let three = advance_followups("fix the flaky test", 3);
        assert_eq!(three.len(), 3);
        // The original task is echoed once for context.
        assert!(
            three.iter().any(|f| f.contains("fix the flaky test")),
            "task not echoed: {three:?}"
        );

        // More advances than bespoke rungs -> padded, never short, capped at max.
        let many = advance_followups("task", 20);
        assert_eq!(many.len(), 20);
        assert!(many.last().unwrap().contains("summarize"));

        // No task -> rungs still generated, none echo an (empty) task line.
        let no_task = advance_followups("", 2);
        assert_eq!(no_task.len(), 2);
        assert!(!no_task.iter().any(|f| f.contains("original task is:")));
    }

    #[test]
    fn card_discloses_auto_advance() {
        let m = Manifest::from_json(r#"{"name": "x", "auto_advance": 4}"#).unwrap();
        let card = render_card(&m, false, m.auto_advance);
        assert!(card.contains("auto-advance: ON"), "card: {card}");
        assert!(card.contains("up to 4"), "card: {card}");

        // The card reflects the RESOLVED count passed in (e.g. an env override),
        // not the raw manifest field — guards against the disclosure desyncing.
        let overridden = render_card(&m, false, 9);
        assert!(overridden.contains("up to 9"), "card: {overridden}");

        // Off by default -> no auto-advance line.
        let off = Manifest::from_json(r#"{"name": "y"}"#).unwrap();
        assert!(!render_card(&off, false, 0).contains("auto-advance: ON"));
    }

    #[test]
    fn is_confirmed_accepts_yes_and_empty() {
        assert!(is_confirmed("y"));
        assert!(is_confirmed("yes"));
        assert!(is_confirmed("Y"));
        assert!(is_confirmed("YES"));
        // Bare Enter (empty line) confirms, as does whitespace-only.
        assert!(is_confirmed(""));
        assert!(is_confirmed("   "));
        assert!(is_confirmed("yes\n"));
    }

    #[test]
    fn is_confirmed_rejects_anything_else() {
        assert!(!is_confirmed("n"));
        assert!(!is_confirmed("no"));
        assert!(!is_confirmed("abort"));
        assert!(!is_confirmed("x"));
        assert!(!is_confirmed("nope"));
        assert!(!is_confirmed("quit"));
    }

    #[test]
    fn render_card_includes_policy_backend_and_run() {
        let m = Manifest::from_json(
            r#"{
                "name": "demo",
                "system_prompt": "accept reads and tests",
                "backend": "mock",
                "run": "echo running-task"
            }"#,
        )
        .unwrap();
        let card = render_card(&m, true, m.auto_advance);
        assert!(card.contains("accept reads and tests"), "policy: {card}");
        assert!(card.contains("mock"), "backend: {card}");
        assert!(card.contains("echo running-task"), "run: {card}");
        // Auto-accept state is surfaced on the card.
        assert!(card.contains("auto-accept: ON"), "auto-accept: {card}");
    }

    #[test]
    fn render_card_shows_setup_and_no_run() {
        let m = Manifest::from_json(r#"{"setup": ["echo a", "echo b"]}"#).unwrap();
        let card = render_card(&m, false, m.auto_advance);
        assert!(card.contains("echo a"), "{card}");
        assert!(card.contains("echo b"), "{card}");
        // With no run command the card says so, and auto-accept reads off.
        assert!(card.contains("(none — drops straight into the prompt)"), "{card}");
        assert!(card.contains("auto-accept: off"), "{card}");
    }

    // The manifest's system prompt must drive auto-accept exactly as the
    // interactive session does, since auto-accept is derived, not stored.
    #[test]
    fn auto_accept_is_derived_from_manifest_system_prompt() {
        let accept = Manifest::from_json(r#"{"system_prompt": "accept all reads"}"#).unwrap();
        assert!(crate::wants_auto_accept(&accept.system_prompt));

        let plain = Manifest::from_json(r#"{"system_prompt": "be terse"}"#).unwrap();
        assert!(!crate::wants_auto_accept(&plain.system_prompt));
    }

    #[test]
    fn render_card_strips_control_chars_from_fields() {
        // A manifest that tries to smuggle ANSI / CR / cursor escapes into the
        // card (to forge the approval screen) must have them stripped.
        let m = Manifest::from_json(
            "{\"name\":\"ok\\u001b[2Khidden\\rXX\",\"run\":\"echo a\\u001b[1A\\rrm -rf /\"}",
        )
        .unwrap();
        let card = render_card(&m, false, m.auto_advance);
        assert!(!card.contains('\x1b'), "ESC must be stripped: {card:?}");
        assert!(!card.contains('\r'), "CR must be stripped: {card:?}");
    }

    #[test]
    fn auto_approve_parses_and_is_disclosed() {
        assert!(!Manifest::from_json("{}").unwrap().auto_approve);
        assert!(Manifest::from_json(r#"{"auto_approve": true}"#).unwrap().auto_approve);
        assert!(!Manifest::from_json(r#"{"auto_approve": false}"#).unwrap().auto_approve);
        // Wrong type is rejected.
        assert!(Manifest::from_json(r#"{"auto_approve": "yes"}"#).is_err());
        // The card must loudly disclose the blind rubber stamp.
        let m = Manifest::from_json(r#"{"auto_approve": true}"#).unwrap();
        let card = render_card(&m, true, m.auto_advance);
        assert!(card.contains("BLIND"), "card must disclose blind approve: {card}");
    }

    #[test]
    fn cwd_parses_and_defaults_to_none() {
        assert_eq!(
            Manifest::from_json(r#"{"cwd": "/tmp/work"}"#).unwrap().cwd.as_deref(),
            Some("/tmp/work")
        );
        assert_eq!(Manifest::from_json("{}").unwrap().cwd, None);
        assert_eq!(Manifest::from_json(r#"{"cwd": ""}"#).unwrap().cwd, None);
    }

    #[test]
    fn known_agents_use_native_prompt_form() {
        let (cmd, injected) = agent_launch("claude", "do the thing");
        assert_eq!(cmd, "claude 'do the thing'");
        assert!(!injected);

        let (cmd, injected) = agent_launch("codex", "fix it");
        assert_eq!(cmd, "codex exec 'fix it'");
        assert!(!injected);
    }

    #[test]
    fn known_agent_matched_on_basename() {
        let (cmd, injected) = agent_launch("/usr/local/bin/claude", "go");
        assert_eq!(cmd, "/usr/local/bin/claude 'go'");
        assert!(!injected);
    }

    #[test]
    fn unknown_agent_falls_back_to_injection() {
        let (cmd, injected) = agent_launch("aider", "refactor module");
        assert_eq!(cmd, "aider");
        assert!(injected);
    }

    #[test]
    fn task_with_single_quotes_is_escaped() {
        let (cmd, _) = agent_launch("claude", "run `git commit -m 'wip'`");
        assert_eq!(cmd, r#"claude 'run `git commit -m '\''wip'\''`'"#);
    }
}

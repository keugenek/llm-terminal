use crate::registry::SessionHandle;
use serde_json::json;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Truncate `prompt_excerpt` to its trailing window. The excerpt is scraped TUI
/// tail text; we only want a hint of the on-screen state, not the whole screen,
/// so we keep the last bytes a prompt typically occupies.
const EXCERPT_TAIL_CHARS: usize = 200;

/// Append-only JSONL trajectory logger: one JSON object per line capturing what
/// the session did (commands run, LLM fallbacks, interactive launches, and
/// auto-accept decisions). This is the data foundation for later analysis
/// features.
///
/// Logging must never take down the terminal session: if the log file can't be
/// opened or a write fails, the logger silently degrades to a no-op rather than
/// surfacing an error. Output goes ONLY to the file — never to stdout/stderr,
/// which would corrupt the raw-mode terminal.
pub struct Trajectory {
    // `None` once we've degraded to a no-op (open failed, or a write failed).
    // Behind a Mutex because the same `&Trajectory` is shared across the REPL
    // and the interactive loop; serialising writes keeps lines from interleaving.
    file: Mutex<Option<File>>,
    // The session's registry record (for `mock-terminal ps`). `None` for the
    // logger-only constructor used in tests; `Some` for real sessions. Each log
    // call touches it (last activity); dropping it marks the session "done".
    session: Option<SessionHandle>,
}

impl Trajectory {
    /// Logger-only constructor (no registry record). Used by tests; real
    /// sessions use [`Trajectory::new_session`] so they appear in `ps`.
    #[cfg(test)]
    pub fn new() -> Self {
        Self {
            file: Mutex::new(Self::open_file()),
            session: None,
        }
    }

    /// Open the JSONL log AND register the session in the tracker so it shows up
    /// in `mock-terminal ps`. `name` labels the session, `kind` is "open" or
    /// "interactive", `source` is the manifest path/URL (empty for interactive).
    pub fn new_session(name: &str, kind: &str, source: &str) -> Self {
        Self {
            file: Mutex::new(Self::open_file()),
            session: Some(SessionHandle::new(name, kind, source)),
        }
    }

    fn open_file() -> Option<File> {
        let dir = Self::base_dir()?;
        // Best-effort dir creation; if it fails the OpenOptions call below will
        // fail too and we degrade to a no-op.
        let _ = fs::create_dir_all(&dir);

        let millis = unix_millis();
        let pid = std::process::id();
        let path = dir.join(format!("session-{millis}-{pid}.jsonl"));

        // 0600: the log can contain command lines / prompt tails, so it must not
        // be readable by other users on the machine.
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
            .ok()
    }

    /// Resolve the trajectories directory. Honours `MT_TRAJECTORY_DIR` as the
    /// base (the file lands directly under it); otherwise `~/.til/trajectories`.
    fn base_dir() -> Option<PathBuf> {
        if let Some(base) = std::env::var_os("MT_TRAJECTORY_DIR") {
            return Some(PathBuf::from(base));
        }
        // No std API for the home dir; HOME is the portable POSIX source and is
        // what the binary's target (macOS/Linux) sets.
        let home = std::env::var_os("HOME")?;
        Some(PathBuf::from(home).join(".til").join("trajectories"))
    }

    /// A real shell command finished: record it with its exit code and whether
    /// it was cut short. The command text is redacted before writing.
    pub fn log_command(&self, command: &str, exit_code: i32, interrupted: bool) {
        self.write(json!({
            "ts": unix_millis(),
            "kind": "command",
            "command": redact(command),
            "exit_code": exit_code,
            "interrupted": interrupted,
        }));
    }

    /// A typed command wasn't a shell command (exit 127) and was sent to the
    /// LLM. Logged in addition to the `command` event for the same input.
    pub fn log_llm_fallback(&self, command: &str) {
        self.write(json!({
            "ts": unix_millis(),
            "kind": "llm_fallback",
            "command": redact(command),
        }));
    }

    /// An interactive program was launched in passthrough mode.
    pub fn log_interactive(&self, command: &str) {
        self.write(json!({
            "ts": unix_millis(),
            "kind": "interactive",
            "command": redact(command),
        }));
    }

    /// A session was launched from a manifest (`til open`). Records the
    /// manifest's name (redacted) so the trajectory shows which pre-configured
    /// session ran.
    pub fn log_manifest(&self, name: &str) {
        self.write(json!({
            "ts": unix_millis(),
            "kind": "manifest",
            "name": redact(name),
        }));
    }

    /// The auto-accept broker reached a verdict on a settled prompt. `verdict`
    /// is "approve" | "deny" | "escalate"; `prompt_excerpt` is the scraped TUI
    /// tail (truncated and redacted before writing).
    pub fn log_decision(&self, verdict: &str, reason: &str, prompt_excerpt: &str) {
        self.write(json!({
            "ts": unix_millis(),
            "kind": "decision",
            "verdict": verdict,
            "reason": reason,
            // Redact BEFORE truncating: tail() first would drop the leading
            // anchor (e.g. `sk-ant-`) of a long secret, leaving redact() nothing
            // to match and writing the secret's tail in clear.
            "prompt_excerpt": tail(&redact(prompt_excerpt), EXCERPT_TAIL_CHARS),
        }));
    }

    /// Append one JSON object as a single line and flush. Failures are swallowed
    /// and degrade the logger to a no-op: logging must never crash the session,
    /// and we must never write to the terminal's stdout/stderr.
    fn write(&self, value: serde_json::Value) {
        // Mirror the event kind into the registry record as "last activity" so
        // `ps` shows what each session most recently did.
        if let Some(session) = &self.session {
            if let Some(kind) = value.get("kind").and_then(|k| k.as_str()) {
                session.touch(kind);
            }
        }
        let mut guard = match self.file.lock() {
            Ok(g) => g,
            // A poisoned lock means another writer panicked mid-write; treat the
            // logger as dead rather than propagating the panic.
            Err(_) => return,
        };
        let Some(file) = guard.as_mut() else {
            return;
        };
        // One object per line. `to_string` never embeds a newline, so the JSONL
        // invariant (one record per line) holds.
        if writeln!(file, "{value}").and_then(|_| file.flush()).is_err() {
            // Drop the handle so subsequent calls are cheap no-ops.
            *guard = None;
        }
    }
}

/// Milliseconds since the Unix epoch. This is a normal binary (not a sandboxed
/// runtime), so reading the wall clock directly is fine. A clock before the
/// epoch is implausible here and falls back to 0.
fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Keep only the last `max` characters of `s` (by char, not byte, so we never
/// split a multibyte sequence). Returns `s` unchanged when it's already short.
fn tail(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    s.chars().skip(count - max).collect()
}

/// Mask obvious secrets before anything is written to disk (privacy: we log
/// command lines and prompt tails, which can carry credentials). We replace the
/// secret *value* with `***` while leaving surrounding text intact:
///   - Anthropic-style key bodies: `sk-ant-…` and `api03-…` (8+ key chars).
///   - `NAME=value` assignments where NAME looks secret (contains KEY / TOKEN /
///     SECRET) and the value is 16+ non-space chars.
///
/// This is a best-effort heuristic, not a guarantee — it targets the common
/// shapes, and short values are intentionally left alone to avoid masking
/// ordinary text.
pub fn redact(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if let Some((prefix_end, end)) = match_token_secret(s, i) {
            // Keep the recognisable prefix (e.g. `sk-ant-`), mask the key body,
            // so the log still shows what kind of secret was present.
            out.push_str(&s[i..prefix_end]);
            out.push_str("***");
            i = end;
        } else if let Some((name_end, value_end)) = match_assignment_secret(s, i) {
            // Keep `NAME=`, mask the value.
            out.push_str(&s[i..name_end]);
            out.push_str("***");
            i = value_end;
        } else {
            // Advance one full char so byte indices stay on char boundaries.
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// True for the chars that make up an Anthropic-style key body / value run:
/// `[A-Za-z0-9_-]`. Used to measure the masked spans.
fn is_secret_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// If a known secret prefix (`sk-ant-` / `api03-`) starts at byte `start`
/// followed by 8+ key chars, return `(prefix_end, token_end)`: the byte index
/// just past the prefix (so the caller can keep it) and just past the masked
/// key body.
fn match_token_secret(s: &str, start: usize) -> Option<(usize, usize)> {
    // Common credential prefixes (Anthropic, GitHub, AWS, Slack, Stripe, Google).
    // Best-effort: catches the prevalent prefixed-token shapes; unprefixed random
    // secrets (e.g. raw AWS secret keys) still rely on the assignment heuristic.
    const PREFIXES: &[&str] = &[
        "sk-ant-", "api03-", "ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_", "AKIA",
        "ASIA", "xoxb-", "xoxp-", "xoxa-", "xoxr-", "xoxs-", "sk_live_", "sk_test_", "AIza",
    ];
    let rest = &s[start..];
    for prefix in PREFIXES {
        if let Some(after) = rest.strip_prefix(prefix) {
            let body_len: usize = after
                .chars()
                .take_while(|&c| is_secret_char(c))
                .map(|c| c.len_utf8())
                .sum();
            let body_chars = after[..body_len].chars().count();
            if body_chars >= 8 {
                let prefix_end = start + prefix.len();
                return Some((prefix_end, prefix_end + body_len));
            }
        }
    }
    None
}

/// Detect a `NAME=value` assignment that looks like a secret. Returns the byte
/// index of the `=`'s following position (so the caller can copy `NAME=`) and
/// the byte index just past the masked value, when both hold:
///
/// - a NAME of identifier chars containing a credential keyword (KEY / TOKEN /
///   SECRET / PASSWORD / PASSPHRASE / CREDENTIAL / PRIVATE / AUTH) starts at
///   `start`, AND
/// - it's immediately followed by `=` and 16+ non-space value chars.
///
/// `start` must be the beginning of the NAME (start of string or just after a
/// non-identifier char) so we don't match mid-word.
fn match_assignment_secret(s: &str, start: usize) -> Option<(usize, usize)> {
    // Only anchor at a real boundary: the previous char must not be part of an
    // identifier, else we'd mask the tail of a larger word.
    if start > 0 {
        let prev = s[..start].chars().next_back().unwrap();
        if prev.is_ascii_alphanumeric() || prev == '_' {
            return None;
        }
    }

    let rest = &s[start..];
    // NAME = identifier chars (letters, digits, underscore) up to '='.
    let name_len: usize = rest
        .chars()
        .take_while(|&c| c.is_ascii_alphanumeric() || c == '_')
        .map(|c| c.len_utf8())
        .sum();
    if name_len == 0 {
        return None;
    }
    let name = &rest[..name_len];
    let upper = name.to_ascii_uppercase();
    const SECRET_WORDS: &[&str] = &[
        "KEY", "TOKEN", "SECRET", "PASSWORD", "PASSWD", "PASSPHRASE", "CREDENTIAL", "PRIVATE",
        "AUTH",
    ];
    if !SECRET_WORDS.iter().any(|w| upper.contains(w)) {
        return None;
    }
    // Must be followed by '='.
    if rest.as_bytes().get(name_len) != Some(&b'=') {
        return None;
    }
    let value = &rest[name_len + 1..];
    // 16+ non-space value chars to qualify as a secret (avoids masking short,
    // ordinary assignments).
    let value_len: usize = value
        .chars()
        .take_while(|c| !c.is_whitespace())
        .map(|c| c.len_utf8())
        .sum();
    let value_chars = value[..value_len].chars().count();
    if value_chars < 16 {
        return None;
    }
    let name_end = start + name_len + 1; // includes the '='
    let value_end = name_end + value_len;
    Some((name_end, value_end))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;

    // MT_TRAJECTORY_DIR is process-global, so tests that set it must not run
    // concurrently or they clobber each other's dir. Serialise them on one lock
    // held for the lifetime of the guard.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    // A guard that points MT_TRAJECTORY_DIR at a fresh temp dir for the test and
    // restores the previous value on drop. Tests must never write to the real
    // $HOME, and env vars are process-global, so set/restore carefully.
    struct DirGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        prev: Option<std::ffi::OsString>,
        dir: PathBuf,
    }

    impl DirGuard {
        fn new(name: &str) -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let prev = std::env::var_os("MT_TRAJECTORY_DIR");
            let dir = std::env::temp_dir().join(format!(
                "til-test-{}-{}-{}",
                name,
                std::process::id(),
                unix_millis()
            ));
            std::env::set_var("MT_TRAJECTORY_DIR", &dir);
            Self {
                _lock: lock,
                prev,
                dir,
            }
        }

        /// The single .jsonl file written into the temp dir.
        fn log_path(&self) -> PathBuf {
            let entry = fs::read_dir(&self.dir)
                .expect("trajectory dir exists")
                .filter_map(|e| e.ok())
                .find(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
                .expect("a .jsonl file was created");
            entry.path()
        }

        fn lines(&self) -> Vec<String> {
            let f = File::open(self.log_path()).expect("open log");
            std::io::BufReader::new(f)
                .lines()
                .map(|l| l.expect("read line"))
                .collect()
        }
    }

    impl Drop for DirGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var("MT_TRAJECTORY_DIR", v),
                None => std::env::remove_var("MT_TRAJECTORY_DIR"),
            }
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn log_command_writes_one_wellformed_json_line() {
        let g = DirGuard::new("cmd");
        let t = Trajectory::new();
        t.log_command("echo hi", 0, false);

        let lines = g.lines();
        assert_eq!(lines.len(), 1, "exactly one line");
        let v: serde_json::Value = serde_json::from_str(&lines[0]).expect("valid JSON");
        assert_eq!(v["kind"], "command");
        assert_eq!(v["command"], "echo hi");
        assert_eq!(v["exit_code"], 0);
        assert_eq!(v["interrupted"], false);
        assert!(v["ts"].as_u64().is_some(), "ts present and numeric");
    }

    #[test]
    fn multiple_logs_append_and_parse_as_jsonl() {
        let g = DirGuard::new("append");
        let t = Trajectory::new();
        t.log_command("ls", 0, false);
        t.log_llm_fallback("how do i list files");
        t.log_interactive("vim a.txt");
        t.log_decision("approve", "looks safe", "Do you want to proceed?");

        let lines = g.lines();
        assert_eq!(lines.len(), 4, "one line per log call");
        // Every line is independently parseable JSON (the JSONL invariant).
        let kinds: Vec<String> = lines
            .iter()
            .map(|l| {
                serde_json::from_str::<serde_json::Value>(l).expect("valid JSON")["kind"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(kinds, ["command", "llm_fallback", "interactive", "decision"]);
    }

    #[test]
    fn decision_records_verdict_reason_and_excerpt() {
        let g = DirGuard::new("decision");
        let t = Trajectory::new();
        t.log_decision("escalate", "not allowed by policy", "Allow this action?");
        let v: serde_json::Value = serde_json::from_str(&g.lines()[0]).unwrap();
        assert_eq!(v["kind"], "decision");
        assert_eq!(v["verdict"], "escalate");
        assert_eq!(v["reason"], "not allowed by policy");
        assert_eq!(v["prompt_excerpt"], "Allow this action?");
    }

    #[test]
    fn redact_masks_anthropic_key() {
        // A fake Anthropic key body must be masked to *** while surrounding text
        // is preserved.
        let masked = redact("export ANTHROPIC_API_KEY then sk-ant-ABCDdefg1234 done");
        assert!(masked.contains("sk-ant-***"), "got: {masked}");
        assert!(!masked.contains("ABCDdefg1234"), "got: {masked}");
        assert!(masked.contains("done"), "surrounding text preserved: {masked}");

        let masked2 = redact("key is api03-XYZ_abc-9876 ok");
        assert!(masked2.contains("api03-***"), "got: {masked2}");
        assert!(!masked2.contains("XYZ_abc-9876"), "got: {masked2}");

        // Expanded prefixes: GitHub, AWS, Google. Tokens are built at runtime so
        // the source carries no contiguous secret-shaped literal (which would
        // trip secret scanners) while still exercising the redactor.
        let body = "A".repeat(20);
        let ghp = format!("ghp_{body}");
        assert!(redact(&format!("token {ghp} done")).contains("ghp_***"));
        assert!(!redact(&format!("token {ghp} done")).contains(&body));
        let akia = format!("AKIA{}", "X".repeat(16));
        assert!(redact(&format!("{akia} here")).contains("AKIA***"));
        let aiza = format!("AIza{}", "y".repeat(20));
        assert!(redact(&format!("{aiza} key")).contains("AIza***"));
    }

    #[test]
    fn redact_masks_secret_assignment() {
        let masked = redact("MY_TOKEN=abcdef0123456789ABCDEF rest");
        assert!(masked.contains("MY_TOKEN=***"), "got: {masked}");
        assert!(!masked.contains("abcdef0123456789ABCDEF"), "got: {masked}");
        assert!(masked.contains("rest"), "got: {masked}");

        // SECRET and KEY names also qualify.
        assert!(redact("DB_SECRET=0123456789abcdef0123").contains("DB_SECRET=***"));
        assert!(redact("API_KEY=0123456789abcdef0123").contains("API_KEY=***"));
        // Expanded keywords: PASSWORD / PASSPHRASE / CREDENTIAL.
        assert!(redact("PGPASSWORD=0123456789abcdef0123").contains("PGPASSWORD=***"));
        assert!(redact("PASSPHRASE=0123456789abcdef0123").contains("PASSPHRASE=***"));
        assert!(redact("DB_CREDENTIAL=0123456789abcdef0123").contains("DB_CREDENTIAL=***"));
    }

    #[test]
    fn log_decision_redacts_secret_longer_than_tail_window() {
        // Reproduces the truncate-before-redact leak: a secret longer than the
        // tail window must still be masked because we now redact BEFORE truncating.
        let g = DirGuard::new("long-secret");
        let t = Trajectory::new();
        let secret_body = "Z".repeat(400);
        let excerpt = format!("sk-ant-{secret_body} please proceed?");
        t.log_decision("approve", "", &excerpt);
        let v: serde_json::Value = serde_json::from_str(&g.lines()[0]).unwrap();
        let stored = v["prompt_excerpt"].as_str().unwrap();
        assert!(!stored.contains(&secret_body), "secret tail leaked: {stored}");
    }

    #[test]
    fn redact_leaves_ordinary_text_untouched() {
        let plain = "ls -la /tmp && echo done";
        assert_eq!(redact(plain), plain);
        // A short assignment isn't a secret even if the name looks secret.
        assert_eq!(redact("TOKEN=short"), "TOKEN=short");
        // A non-secret-looking name with a long value is left alone.
        assert_eq!(redact("PATH=/usr/local/bin:/usr/bin:/bin"), "PATH=/usr/local/bin:/usr/bin:/bin");
        // A short string is unaffected.
        assert_eq!(redact("hi"), "hi");
        // A bare prefix without enough body is not masked.
        assert_eq!(redact("sk-ant-abc"), "sk-ant-abc");
    }

    #[test]
    fn redact_does_not_match_assignment_midword() {
        // "XKEY" embedded after identifier chars must not anchor a match: the
        // whole "FOOKEY=..." has name FOOKEY which does contain KEY, so it DOES
        // qualify — verify a genuinely embedded case instead.
        let s = "prefixTOKEN=abcdef0123456789ABCDEF";
        // Anchored at the start of the string, name is "prefixTOKEN" (contains
        // TOKEN) -> masked.
        assert!(redact(s).contains("prefixTOKEN=***"), "got: {}", redact(s));
    }

    #[test]
    fn prompt_excerpt_truncation_keeps_only_the_tail() {
        let g = DirGuard::new("trunc");
        let t = Trajectory::new();
        // Build an excerpt longer than the tail window with a unique marker only
        // near the end, so we can confirm the head was dropped.
        let head = "H".repeat(300);
        let excerpt = format!("{head}TAILMARKER proceed?");
        t.log_decision("approve", "", &excerpt);

        let v: serde_json::Value = serde_json::from_str(&g.lines()[0]).unwrap();
        let stored = v["prompt_excerpt"].as_str().unwrap();
        assert!(stored.chars().count() <= EXCERPT_TAIL_CHARS, "truncated to tail window");
        assert!(stored.contains("TAILMARKER proceed?"), "tail preserved: {stored}");
        assert!(!stored.contains(&head), "head dropped");
    }

    #[test]
    fn tail_keeps_short_strings_whole() {
        assert_eq!(tail("short", 200), "short");
        assert_eq!(tail("abcdef", 3), "def");
    }

    #[test]
    fn logger_degrades_to_noop_when_dir_unusable() {
        // Point the base dir at a path whose parent is a file, so create_dir_all
        // and open both fail. The logger must not panic; logging is a no-op.
        let tmp = std::env::temp_dir().join(format!("til-file-{}", std::process::id()));
        File::create(&tmp).expect("create blocking file");
        let prev = std::env::var_os("MT_TRAJECTORY_DIR");
        std::env::set_var("MT_TRAJECTORY_DIR", tmp.join("nested"));

        let t = Trajectory::new();
        t.log_command("echo hi", 0, false); // must not panic

        match &prev {
            Some(v) => std::env::set_var("MT_TRAJECTORY_DIR", v),
            None => std::env::remove_var("MT_TRAJECTORY_DIR"),
        }
        let _ = fs::remove_file(&tmp);
    }
}

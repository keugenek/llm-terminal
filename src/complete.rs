//! Tab completion for the line editor (`read_line` in main.rs).
//!
//! Two modes, chosen from the line being edited:
//!  - **command**: completing the first whitespace token when it has no `/` —
//!    match REPL built-ins, known interactive tool names, and `$PATH` binaries.
//!  - **path**: any later token, or any token containing `/` — match filesystem
//!    entries relative to the typed prefix, appending `/` to directories.
//!
//! Kept pure and IO-light (filesystem reads only) so it unit-tests without a
//! terminal. The editor calls `complete(line)` and applies the returned action.

use std::collections::BTreeSet;
use std::path::Path;

/// REPL built-ins that are valid as the first token.
const BUILTINS: &[&str] = &["/exit", "/quit"];

/// Interactive tool names worth completing as commands (mirrors the launch-time
/// interactive list in main.rs; duplicated deliberately so completion needs no
/// coupling to that private list).
const TOOLS: &[&str] = &[
    "claude", "codex", "vim", "vi", "nvim", "nano", "emacs", "top", "htop", "less", "more", "man",
    "ssh", "python", "python3", "ipython", "node", "irb", "psql", "mysql", "tmux", "screen",
];

/// What the editor should do with a Tab press.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Completion {
    /// No candidate — do nothing (the editor may beep).
    None,
    /// Replace the final token with this text (already includes any trailing
    /// `/` for a directory or ` ` for a uniquely-completed command).
    Replace { token: String, completed: String },
    /// Multiple candidates: extend the token to their common prefix (may equal
    /// the current token, in which case only the list is new information) and
    /// show the list.
    Candidates { token: String, common: String, list: Vec<String> },
}

/// Split `line` into (prefix-before-final-token, final-token). The final token
/// is the run of non-whitespace at the end; an empty token (line ends in space)
/// is valid and means "complete from nothing here".
fn split_final_token(line: &str) -> (&str, &str) {
    match line.rfind(char::is_whitespace) {
        Some(i) => (&line[..=i], &line[i + 1..]),
        None => ("", line),
    }
}

/// Is the final token the FIRST token on the line (i.e. command position)?
fn is_first_token(prefix: &str) -> bool {
    prefix.trim().is_empty()
}

/// Compute the completion for the current `line`. Pure except for reading the
/// filesystem / `$PATH` to enumerate candidates.
pub fn complete(line: &str) -> Completion {
    let (prefix, token) = split_final_token(line);
    // First-token is command position. A token containing '/' is normally a
    // path (./run, src/bin, /usr/bin/x) EXCEPT our slash-command built-ins
    // (/exit, /quit) which also start with '/'.
    let is_path_like = token.contains('/') && !BUILTINS.iter().any(|b| b.starts_with(token));
    if is_first_token(prefix) && !is_path_like {
        complete_command(token)
    } else {
        complete_path(token)
    }
}

/// Command-position completion: built-ins + tool names + `$PATH` executables.
fn complete_command(token: &str) -> Completion {
    // A bare Tab (no token yet) would enumerate all of `$PATH` — not useful and
    // floods the screen. Require at least one character to complete a command.
    if token.is_empty() {
        return Completion::None;
    }
    let mut set: BTreeSet<String> = BTreeSet::new();
    for b in BUILTINS.iter().chain(TOOLS.iter()) {
        if b.starts_with(token) {
            set.insert((*b).to_string());
        }
    }
    for name in path_executables() {
        if name.starts_with(token) {
            set.insert(name);
        }
    }
    let candidates: Vec<String> = set.into_iter().collect();
    // Commands complete with a trailing space (the next token is an argument).
    finish(token, candidates, " ")
}

/// Path-position completion: entries under the directory implied by `token`.
fn complete_path(token: &str) -> Completion {
    // A bare `~` completes to `~/` so the next Tab descends into home — without
    // this it would (wrongly) be treated as a leaf filtered against the cwd.
    // `~user` is not expanded (no passwd lookup); it falls through and yields no
    // match, which is the documented limit.
    if token == "~" {
        return Completion::Replace {
            token: token.to_string(),
            completed: "~/".to_string(),
        };
    }
    // Split the token into a directory part and the partial leaf being typed.
    // "src/ma" -> dir "src/", leaf "ma"; "src" -> dir "", leaf "src"; "" -> ".".
    let (dir_part, leaf) = match token.rfind('/') {
        Some(i) => (&token[..=i], &token[i + 1..]),
        None => ("", token),
    };
    let read_dir = if dir_part.is_empty() {
        ".".to_string()
    } else {
        expand_tilde(dir_part)
    };

    let mut candidates: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&read_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            // Hidden files only when the leaf explicitly starts with '.'.
            if name.starts_with('.') && !leaf.starts_with('.') {
                continue;
            }
            if !name.starts_with(leaf) {
                continue;
            }
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            // Rebuild the full token as the user would type it: dir_part + name,
            // plus a trailing '/' for directories so the next Tab descends.
            let mut completed = format!("{dir_part}{name}");
            if is_dir {
                completed.push('/');
            }
            candidates.push(completed);
        }
    }
    candidates.sort();

    match candidates.len() {
        0 => Completion::None,
        1 => Completion::Replace {
            token: token.to_string(),
            // A directory already carries its trailing '/'; a file gets a space.
            completed: append_file_space(&candidates[0]),
        },
        _ => {
            let common = longest_common_prefix(&candidates);
            Completion::Candidates {
                token: token.to_string(),
                common,
                // Show just the leaf names in the listing, not full paths.
                list: candidates
                    .iter()
                    .map(|c| leaf_of(c).to_string())
                    .collect(),
            }
        }
    }
}

/// A file completion (no trailing '/') gets a trailing space; a directory
/// (ends in '/') is left as-is so the next Tab descends into it.
fn append_file_space(completed: &str) -> String {
    if completed.ends_with('/') {
        completed.to_string()
    } else {
        format!("{completed} ")
    }
}

/// Turn a candidate list into a Completion, appending `suffix` on a unique hit.
fn finish(token: &str, candidates: Vec<String>, suffix: &str) -> Completion {
    match candidates.len() {
        0 => Completion::None,
        1 => Completion::Replace {
            token: token.to_string(),
            completed: format!("{}{suffix}", candidates[0]),
        },
        _ => {
            let common = longest_common_prefix(&candidates);
            Completion::Candidates {
                token: token.to_string(),
                common,
                list: candidates,
            }
        }
    }
}

/// The leaf (after the last '/') of a path-ish string, trailing '/' kept.
fn leaf_of(s: &str) -> &str {
    let trimmed = s.strip_suffix('/').unwrap_or(s);
    match trimmed.rfind('/') {
        Some(i) => &s[i + 1..],
        None => s,
    }
}

/// Expand a leading `~/` or `~` to the home directory (best-effort; leaves the
/// string unchanged if HOME is unset).
fn expand_tilde(s: &str) -> String {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return format!("{}/{rest}", home.to_string_lossy());
        }
    } else if s == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return home.to_string_lossy().into_owned();
        }
    }
    s.to_string()
}

/// Longest common prefix of a non-empty candidate list.
fn longest_common_prefix(items: &[String]) -> String {
    let first = match items.first() {
        Some(f) => f.as_str(),
        None => return String::new(),
    };
    let mut end = first.len();
    for item in &items[1..] {
        end = end.min(item.len());
        while !item.is_char_boundary(end) || first[..end] != item[..end] {
            end -= 1;
            if end == 0 {
                return String::new();
            }
        }
    }
    first[..end].to_string()
}

/// Executable basenames found on `$PATH`. Best-effort: unreadable dirs skipped.
/// Not deduped here (the caller uses a set); does not check the execute bit (a
/// readable file in a PATH dir is a good-enough candidate for completion).
fn path_executables() -> Vec<String> {
    let mut out = Vec::new();
    let path = match std::env::var_os("PATH") {
        Some(p) => p,
        None => return out,
    };
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if is_executable_file(&entry) {
                    out.push(entry.file_name().to_string_lossy().into_owned());
                }
            }
        }
    }
    out
}

/// A regular file (or symlink to one) that is plausibly executable. We accept
/// any regular file on PATH — completion is advisory, and stat-ing the exec bit
/// per entry across all of PATH is costly and platform-fiddly.
fn is_executable_file(entry: &std::fs::DirEntry) -> bool {
    match entry.file_type() {
        Ok(t) => t.is_file() || t.is_symlink(),
        Err(_) => false,
    }
}

/// Whether `dir` looks like an existing directory (used by tests / callers that
/// want to validate a completed path part).
#[allow(dead_code)]
pub fn is_dir(p: &str) -> bool {
    Path::new(&expand_tilde(p)).is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn split_final_token_handles_spaces_and_empty() {
        assert_eq!(split_final_token("vi"), ("", "vi"));
        assert_eq!(split_final_token("git ad"), ("git ", "ad"));
        assert_eq!(split_final_token("git "), ("git ", ""));
        assert_eq!(split_final_token(""), ("", ""));
    }

    #[test]
    fn first_token_detection() {
        assert!(is_first_token(""));
        assert!(is_first_token("   "));
        assert!(!is_first_token("git "));
    }

    #[test]
    fn builtin_slash_command_completes_uniquely() {
        // "/ex" -> "/exit " (unique among builtins).
        match complete("/ex") {
            Completion::Replace { token, completed } => {
                assert_eq!(token, "/ex");
                assert_eq!(completed, "/exit ");
            }
            other => panic!("expected unique replace, got {other:?}"),
        }
    }

    #[test]
    fn ambiguous_slash_commands_offer_common_prefix() {
        // "/" matches both /exit and /quit -> common prefix "/", list of both.
        match complete("/") {
            Completion::Candidates { common, list, .. } => {
                assert_eq!(common, "/");
                assert!(list.contains(&"/exit".to_string()));
                assert!(list.contains(&"/quit".to_string()));
            }
            other => panic!("expected candidates, got {other:?}"),
        }
    }

    #[test]
    fn tool_name_prefix_extends_to_common() {
        // "python" matches python and python3 -> common prefix "python".
        match complete("pyth") {
            Completion::Candidates { common, list, .. } => {
                assert_eq!(common, "python");
                assert!(list.iter().any(|c| c == "python"));
                assert!(list.iter().any(|c| c == "python3"));
            }
            // On a machine whose PATH adds more "pyth*" binaries this stays
            // Candidates; a unique hit would be a Replace — accept either as
            // long as it isn't None.
            Completion::Replace { completed, .. } => assert!(completed.starts_with("python")),
            Completion::None => panic!("expected python completions"),
        }
    }

    #[test]
    fn longest_common_prefix_basic() {
        let v = vec!["foobar".to_string(), "fooqux".to_string(), "foo".to_string()];
        assert_eq!(longest_common_prefix(&v), "foo");
        let none = vec!["abc".to_string(), "xyz".to_string()];
        assert_eq!(longest_common_prefix(&none), "");
    }

    #[test]
    fn path_completion_in_temp_dir() {
        let tmp = std::env::temp_dir().join(format!("mt-complete-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("subdir")).unwrap();
        fs::write(tmp.join("alpha.txt"), b"x").unwrap();
        fs::write(tmp.join("alphabet.txt"), b"x").unwrap();

        let base = tmp.to_string_lossy().to_string();

        // Unique directory completion -> trailing slash, no space.
        match complete(&format!("ls {base}/sub")) {
            Completion::Replace { completed, .. } => {
                assert!(completed.ends_with("subdir/"), "got {completed}");
            }
            other => panic!("expected unique dir replace, got {other:?}"),
        }

        // Ambiguous file prefix -> common prefix "alpha", leaf list.
        match complete(&format!("cat {base}/alpha")) {
            Completion::Candidates { common, list, .. } => {
                assert!(common.ends_with("alpha"), "common was {common}");
                assert!(list.iter().any(|l| l == "alpha.txt"));
                assert!(list.iter().any(|l| l == "alphabet.txt"));
            }
            other => panic!("expected candidates, got {other:?}"),
        }

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn unique_file_completion_gets_trailing_space() {
        let tmp = std::env::temp_dir().join(format!("mt-complete-file-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("only.log"), b"x").unwrap();
        let base = tmp.to_string_lossy().to_string();
        match complete(&format!("cat {base}/on")) {
            Completion::Replace { completed, .. } => {
                assert!(completed.ends_with("only.log "), "got {completed}");
            }
            other => panic!("expected unique file replace, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn no_match_returns_none() {
        assert_eq!(complete("/zzz-nonexistent"), Completion::None);
    }

    #[test]
    fn bare_tilde_completes_to_home_slash() {
        // A lone `~` (command arg position) completes to `~/` so the next Tab
        // descends into home, instead of being filtered against the cwd.
        match complete("ls ~") {
            Completion::Replace { token, completed } => {
                assert_eq!(token, "~");
                assert_eq!(completed, "~/");
            }
            other => panic!("expected ~ -> ~/ , got {other:?}"),
        }
    }

    #[test]
    fn leaf_of_paths() {
        assert_eq!(leaf_of("src/main.rs"), "main.rs");
        assert_eq!(leaf_of("src/sub/"), "sub/");
        assert_eq!(leaf_of("bare"), "bare");
    }
}

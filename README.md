# llm-terminal

A tiny terminal in Rust that runs your real shell, falls back to an LLM when a
command isn't found — and can run **interactive AI agents like Claude Code and
auto-accept their prompts for you**.

So yes: you can use it to drive Claude Code on autopilot. I built the whole
thing *with* Claude Code. Automating Claude Code with itself. 🐢🐢🐢

```
$ echo hello              # real shell, real state (cd, env vars persist)
hello
$ gti status              # not a command? → goes to the LLM
« Did you mean `git status`? …
$ claude                  # interactive agent → full passthrough TUI
  (renders Claude Code; permission prompts auto-accepted)
```

## What it does

- **Real shell, persistent state.** Commands run in a real `bash` behind a PTY.
  `cd`, environment variables, and shell state survive across commands.
- **LLM fallback.** If a command isn't found (exit 127), the input is sent to a
  backend (mock for testing, or the Anthropic Messages API) and the reply is
  printed inline.
- **Interactive passthrough.** Full-screen / REPL programs (`claude`, `vim`,
  `top`, `python`, …) get handed the raw terminal — arrows, escapes, Ctrl-keys
  all pass through. Prefix any command with `:` to force it.
- **Auto-accept with a model-graded policy broker.** Opt in via the startup
  system prompt (type something with "accept"). Known agents launch with their
  native permission-bypass flag; for everything else, when a prompt settles a
  Haiku model grades it against your system prompt (the policy) and returns
  **APPROVE** (inject Enter), **DENY** (inject a reject, then Escape as a
  fallback), or **ESCALATE** (stop and hand control back to you). It fails safe:
  any API error or unparseable reply escalates — it never approves on
  uncertainty. With no API key it falls back to the old blind always-approve.
- **Never wedges.** Hung or interactive commands can be interrupted with Ctrl-C;
  the process tree is SIGKILLed and the shell always recovers.

## Quickstart

```bash
git clone https://github.com/keugenek/llm-terminal
cd llm-terminal
cargo run                                   # mock backend (no API key needed)
ANTHROPIC_API_KEY=sk-ant-… cargo run -- anthropic   # real LLM fallback
```

At launch you're asked for a **system prompt**. Type `accept requests` (or
anything containing "accept") to turn on auto-accept, then submit an empty line.

```
$ claude            # runs Claude Code with permissions bypassed → autopilot
$ :mytool           # force any command into interactive passthrough
Ctrl-C              # interrupt a running command (or exit at the prompt)
/exit               # quit
```

## How it works

A persistent `bash` runs inside a PTY. Each captured command is wrapped with
unique start/end markers and `</dev/null` so stdin-readers can't hang; output
between the markers is the result, and the trailing marker carries the exit
code.

For interactive programs the terminal flips into **passthrough**: it resizes the
inner PTY to your terminal, launches the program bare so it inherits the PTY as
its controlling terminal, and shuttles raw bytes both ways until the program's
process tree exits. Auto-accept watches the output stream for prompt patterns
(`Do you want…`, `❯ 1. Yes`, `(y/n)`, …); once a prompt has settled it asks the
policy broker (`src/decider.rs`) what to do. The broker sends your system prompt
plus the scraped prompt text to a Haiku model and parses a one-token verdict
(APPROVE / DENY / ESCALATE), so permission prompts are answered according to
your policy rather than blindly accepted — and anything the policy doesn't
clearly allow is escalated back to you instead of approved.

## Tests

```bash
cargo test          # 33 unit + PTY integration tests
expect tests/smoke.exp   # end-to-end: shell, interrupt, passthrough, auto-accept
```

## Built with Claude Code

Every line — the PTY plumbing, the interrupt/SIGKILL recovery, the passthrough
mode, the auto-accept heuristics — was written by pair-programming with Claude
Code (Opus). The bugs were found by Claude Code too, by running the terminal and
watching it break.

---

*Not affiliated with Anthropic. "Claude" and "Claude Code" are products of
Anthropic; this project just happens to drive them.*

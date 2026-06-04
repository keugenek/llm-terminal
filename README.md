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
  Set **`MT_AUTO_APPROVE=1`** for full unattended autonomy: approve every prompt
  with no grading at all (a deliberate rubber stamp — use only when you accept
  that the policy is bypassed; `claude` already runs with its own
  `--dangerously-skip-permissions` under auto-accept).
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

## Sessions — launch a pre-configured agent run from a file or URL

Instead of typing the system prompt and launching the agent by hand, hand
llm-terminal a **manifest** — one self-contained JSON file (or a URL to one)
that is the whole handoff:

```json
// examples/session.json
{
  "name": "fix flaky test",
  "system_prompt": "auto-approve reads and tests; DENY anything containing rm -rf; escalate network or spend",
  "instructions": "Find and fix the flaky test, then run the suite until green.",
  "backend": "anthropic",
  "cwd": "/path/to/repo",
  "setup": ["echo preparing"],
  "run": "claude"
}
```

Open it:

```bash
mock-terminal open examples/session.json     # or:  open https://…/session.json
```

It prints a **session card** showing exactly what will run. A **local** manifest
you opened directly runs straight away (you chose the file); a manifest fetched
from a **URL** requires a `[Y/n]` confirmation first — that's the remote-exec
safety gate. Use `--yes` to skip the prompt or `--confirm` to force it for any
source. Then the `system_prompt` becomes the session policy (so auto-accept +
the Haiku broker engage as in interactive mode), it `cd`s into `cwd`, runs
`setup`, and launches `run` with `instructions` delivered to the agent — via its
native prompt flag where known (`claude "<task>"`, `codex exec "<task>"`) or
typed in once an unknown agent's input settles — before dropping into the prompt.

For fully unattended runs, set `"auto_approve": true` in the manifest (see
`examples/autopilot.json`). It implies auto-accept and blind-approves every
prompt with no policy grading — the same rubber stamp as `MT_AUTO_APPROVE=1`,
disclosed loudly on the card. Use it only when you accept that the policy is
bypassed.

To open one in a **dedicated macOS Terminal window (or tab)** from another
session — e.g. to fan out several agents in parallel — use the launcher:

```bash
scripts/spawn.sh examples/session.json --name fix-flaky-test
scripts/spawn.sh https://…/session.json --tab        # as a tab instead
```

It opens a titled window running `mock-terminal open <file-or-url>` (which shows
its own card + confirm in that window); each session logs its trajectory under
`~/.til/trajectories/` (JSONL). macOS only (uses `osascript`).

## Tracking sessions

Every session — whether you launched it here or spawned it in another window —
registers itself in `~/.til/sessions/`. List them all from anywhere:

```bash
mock-terminal ps            # one-shot table of all sessions
mock-terminal ps --watch    # live, refreshes every second (Ctrl-C to stop)
```

```
SESSION                  STATE    PID      AGE       LAST
fix flaky test           running  93060    1m        decision
autonomy demo — fix add  done     49662    6m        command
old-window               dead     12233    2h        launched
```

Each session is the authoritative source of its own record (status, pid, last
event). `ps` reconciles those records against live PIDs, so a window you closed
or a crashed run shows as `dead` rather than lingering as `running`.

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
cargo test          # 75 unit + PTY integration tests
expect tests/smoke.exp     # end-to-end: shell, interrupt, passthrough, auto-accept
expect tests/manifest.exp  # end-to-end: `open` renders card, confirms, runs
expect tests/ps.exp        # end-to-end: `ps` tracks a session running → done
```

## Built with Claude Code

Every line — the PTY plumbing, the interrupt/SIGKILL recovery, the passthrough
mode, the auto-accept heuristics — was written by pair-programming with Claude
Code (Opus). The bugs were found by Claude Code too, by running the terminal and
watching it break.

---

*Not affiliated with Anthropic. "Claude" and "Claude Code" are products of
Anthropic; this project just happens to drive them.*

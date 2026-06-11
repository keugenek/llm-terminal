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
- **Tab completion.** The prompt's line editor completes the word under the
  cursor on Tab: in command position it matches REPL built-ins (`/exit`,
  `/quit`), known interactive tools, and `$PATH` executables; elsewhere it
  completes filesystem paths (descends into directories, `~` expands). A unique
  match fills in; an ambiguous one extends to the common prefix, and a second
  Tab lists the candidates.
- **Interactive passthrough.** Full-screen / REPL programs (`claude`, `vim`,
  `top`, `python`, …) get handed the raw terminal — arrows, escapes, Ctrl-keys
  all pass through. Prefix any command with `:` to force it, or add names via
  `MT_INTERACTIVE`.
- **Auto-advance (hands-free task progress).** For an unattended agent run,
  after the initial task is delivered the terminal watches for the agent to
  finish a turn and go idle at its input box (no output, no permission prompt
  up), then auto-types the next *continuation prompt* to keep it moving toward
  the goal. Follow-ups are derived from the session's `instructions` (a
  deterministic nudge ladder — no extra LLM calls) and bounded by a count, so
  it advances toward completion and winds down rather than spinning forever.
  A nudge typed into an agent that's quietly mid-work (a long silent tool
  call looks identical to idle) only queues — so until the agent visibly
  consumes it, each further nudge waits exponentially longer instead of
  draining the ladder. Enable per-session with the manifest's `auto_advance`
  field (or `MT_AUTO_ADVANCE`).
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
- **Prompt detection works through full-screen TUIs.** Real agent UIs (like
  Claude Code) paint their prompts with ANSI cursor-positioning, not plain
  newlines. The detector strips ANSI escapes first and scans the visible tail of
  the screen, so prompts buried in a redrawn TUI are still recognized.
- **Session manifests.** Hand it one self-contained JSON file (or a URL to one)
  describing the policy, the task, the working dir, and the agent to launch —
  see [Sessions](#sessions--launch-a-pre-configured-agent-run-from-a-file-or-url).
- **Session tracking.** Every run registers itself; `mock-terminal ps` lists all
  live and finished sessions across windows — see [Tracking
  sessions](#tracking-sessions).
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

The single positional argument selects the LLM-fallback backend:
`mock` (default) or `anthropic` (alias `claude`).

## Configuration

All configuration is via environment variables — nothing is written to a config
file.

| Variable | Default | What it does |
|---|---|---|
| `ANTHROPIC_API_KEY` | _(unset)_ | API key for the `anthropic` fallback backend **and** the Haiku policy broker. Without it, auto-accept falls back to blind always-approve. |
| `ANTHROPIC_MODEL` | `claude-haiku-4-5-20251001` | Model for the `anthropic` fallback backend. The policy broker always grades with Haiku regardless of this. |
| `MT_AUTO_APPROVE` | _(unset)_ | If set to **any** value, blind-approve every prompt with no policy grading (rubber stamp). Presence-only — the value is ignored. |
| `MT_AUTO_ADVANCE` | _(manifest)_ | Overrides the manifest's `auto_advance` count: how many follow-up prompts to auto-type when the agent goes idle. Clamped to 50. |
| `MT_AUTO_ADVANCE_IDLE_MS` | `8000` | How long (ms) the agent must be idle at its input before an auto-advance follow-up fires. While a follow-up sits unconsumed (no real output since it was typed — the agent is in a long silent tool call, not idle), the window for the next one doubles, up to 16×, so a quiet stretch can't drain the whole ladder. |
| `MT_INTERACTIVE` | _(empty)_ | Extra command names (comma/space-separated) to always treat as interactive passthrough, on top of the built-in list. |
| `MT_TIMEOUT_SECS` | `30` | Timeout in seconds for a captured (non-interactive) command before it's interrupted. |
| `MT_TRAJECTORY_DIR` | `~/.til/trajectories` | Directory for per-session trajectory JSONL logs (files land directly under it). |
| `MT_TIL_DIR` | `~/.til` | Base dir for the session registry; records live in `<MT_TIL_DIR>/sessions`. |
| `HOME`, `TERM` | _(from env)_ | Read for home-dir resolution (logs/registry) and terminal sizing/setup. |

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

Every field is optional (an empty `{}` parses into an inert-but-valid manifest):

| Field | Type | Default | Meaning |
|---|---|---|---|
| `name` | string | `""` | Human-readable label, shown on the session card. |
| `system_prompt` | string | `""` | The session policy. Drives auto-accept on/off and is what the Haiku broker grades prompts against. |
| `instructions` | string | `""` | The task handed to the agent — via its native prompt flag where known (`claude "<task>"`, `codex exec "<task>"`), else typed in once the agent's input settles. |
| `backend` | string | `"mock"` | LLM-fallback backend: `"mock"` or `"anthropic"`. |
| `auto_approve` | bool | `false` | Blind-approve **every** prompt with no policy grading (implies auto-accept). The same rubber stamp as `MT_AUTO_APPROVE`; disclosed loudly on the card. |
| `cwd` | string | _(none)_ | Directory to `cd` into before setup/run. |
| `setup` | string[] | `[]` | Commands run in capture mode (output shown) before `run`. |
| `run` | string | _(none)_ | The command to launch after setup. If absent, the session drops straight into the interactive REPL. |
| `auto_advance` | int | `0` | How many follow-up continuation prompts to auto-type when the agent finishes a turn and goes idle (hands-free progress toward the task). 0 disables it; max 50. Disclosed on the card. |

Open it:

```bash
mock-terminal open examples/session.json     # or:  open https://…/session.json
```

It prints a **session card** showing exactly what will run. A **local** manifest
you opened directly runs straight away (you chose the file); a manifest fetched
from a **URL** requires a `[Y/n]` confirmation first — that's the remote-exec
safety gate. Use `--yes` (`-y`) to skip the prompt or `--confirm` to force it for
any source. Then the `system_prompt` becomes the session policy (so auto-accept +
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
or a crashed run shows as `dead` rather than lingering as `running`. Records
older than 24h are pruned.

## How it works

A persistent `bash` runs inside a PTY. Each captured command is wrapped with
unique start/end markers and `</dev/null` so stdin-readers can't hang; output
between the markers is the result, and the trailing marker carries the exit
code. A captured command that runs past `MT_TIMEOUT_SECS` (default 30s) is
interrupted and its process tree SIGKILLed.

For interactive programs the terminal flips into **passthrough**: it resizes the
inner PTY to your terminal, launches the program bare so it inherits the PTY as
its controlling terminal, and shuttles raw bytes both ways until the program's
process tree exits. Auto-accept watches the output stream for prompt patterns
(`Do you want…`, `❯ 1. Yes`, `(y/n)`, …) — first stripping ANSI escapes and
scanning the visible tail, so prompts painted by a full-screen TUI are caught
too. Once a prompt has settled it asks the policy broker (`src/decider.rs`) what
to do. The broker sends your system prompt plus the scraped prompt text to a
Haiku model and parses a one-token verdict (APPROVE / DENY / ESCALATE), so
permission prompts are answered according to your policy rather than blindly
accepted — and anything the policy doesn't clearly allow is escalated back to
you instead of approved.

## Tests

```bash
cargo test                 # 94 unit + PTY integration tests
expect tests/smoke.exp     # end-to-end: shell, interrupt, passthrough, auto-accept
expect tests/complete.exp  # end-to-end: Tab completion (command + path)
expect tests/manifest.exp  # end-to-end: `open` renders card, confirms, runs
expect tests/advance.exp   # end-to-end: auto-advance types follow-ups when idle
expect tests/ps.exp        # end-to-end: `ps` tracks a session running → done
expect tests/anthropic.exp # end-to-end: live Anthropic fallback (needs ANTHROPIC_API_KEY)
```

## Built with Claude Code

Every line — the PTY plumbing, the interrupt/SIGKILL recovery, the passthrough
mode, the auto-accept heuristics — was written by pair-programming with Claude
Code (Opus). The bugs were found by Claude Code too, by running the terminal and
watching it break.

---

*Not affiliated with Anthropic. "Claude" and "Claude Code" are products of
Anthropic; this project just happens to drive them.*

# Maki Fork — Release Log

Fork of [maki](https://github.com/neroro64/maki). Every entry below is authored on the
`fork` branch **on top of upstream**; upstream commits pulled in via `rebase-upstream`
are not re-listed here.

- **Fork:** `neroro64/maki` → branch `fork`
- **Upstream sync point:** `54a15291` (2026-10-14, upstream `0.6.5` era)
- **Range:** 2026-10-14 → 2026-11-01
- **Fork-authored commits:** 19 (15 features, 3 fixes, 1 tooling)

---

## Unreleased

- **Windows multiline paste:** decode Win32 key records embedded in bracketed
  paste so Enter press/release sequences become line breaks instead of leaking
  into the prompt. Keep pasted control characters inside the paste event rather
  than treating them as submit keys.
- **Parallel task coordination:** reuse the `/orchestrate` ledger policy for
  multi-worker implementation, with manager-owned shared notes, per-task handoffs,
  explicit dependencies and one verification owner per worktree. The `task` tool
  accepts a ledger path and exposes opt-in background execution by default.
- **Shell diagnostics:** retain full output when bash or PowerShell truncates or
  tails a result, and retain partial output on interruption. Report readable
  OS-temp log paths and exit status so diagnostics do not require another run.
  Preserve raw output for known verification commands instead of automatic RTK
  filtering. PowerShell streams its capture for cancellation recovery.
- **Tool capabilities:** advertise indexable file types from the parser mappings,
  direct unsupported files such as PowerShell to bounded search/read, and describe
  the Python sandbox's supported text APIs, eager generators and permission hooks.

---

## New Features

### Multi-agent & agent loop
- **`/orchestrate` — ledger-based multi-agent mode** (experimental). Turns the agent
  into a manager that decomposes an objective into a filesystem ledger under
  `.maki/orchestrate/<run>/`, dispatches fresh `task` subagents in parallel waves, and
  coordinates purely through the ledger. Workers inherit the session model; the manager
  verifies acceptance criteria itself.
- **snapcompact compaction.** Replaces the LLM summarization pass with a local,
  deterministic archival pass: discarded history is whitespace-collapsed, folded to the
  bundled 8×13 pixel font, paginated onto a fixed grid, and rasterized to PNG frames a
  vision-capable model reads back at a fraction of the billed-token cost of raw text. No
  model, API key, or network involved, so it is safe for overflow recovery. Enabled by
  default via `agent.compaction_strategy = auto`.
- **Context notes notebook.** A durable per-session notebook the agent writes via
  `context_notes`, re-injected after compaction and reminded before the threshold.
  Capped at 16 KiB.
- **Git-based checkpoints with rewind.** Commits the working tree to a temp git at each
  turn end; rewinding to an earlier turn restores project files. `/checkpoint` toggles
  per session.

### Tools
- **Computer use tool.** Desktop control: mouse, keyboard, screenshots, and clipboard
  (enigo / x11rb / arboard). Persistent clipboard handle keeps ownership on X11; portal
  screenshot fallback for headless-free desktops.
- **Browser tool.** Drives Chrome over CDP (chromiumoxide): open / snapshot / click /
  type / press / scroll / screenshot / eval, with auto-discovery of the Chrome binary and
  per-session teardown.
- **Persistent PowerShell session tool.** Keeps one `pwsh` process alive per session so
  variables and cwd carry over between calls.

### Lua plugin API expansion
- LSP API (definitions, references, hover, speculate), a regex API, expanded file-access
  helpers, and richer tree-sitter queries.
- Session snapshot storage, symbol locking, and reflection guards backing the
  plugin edit path.

### UI & commands
- **`/copy`** — searchable picker of assistant responses (newest first), copying full
  Markdown to the native clipboard with an OSC52 terminal fallback.
- **`/review` reimplemented** as a bundled plugin.
- **`/skills` picker** — discover skills from `.github/skills`, `.agent/skills`, and
  `~/.copilot/skills`; a `ListPicker` modal toggles enabled state (persisted to
  `skills.json`), and Tab completes enabled skill names.
- **Tab path completion** in the prompt editor — Tab lists the directory a path token
  points at (directories first) and inserts the chosen entry; Tab on a non-path token
  keeps toggling plan mode.
- **Ctrl bracket keys cycle model tiers.**
- **Ctrl+M binds the model picker** (Ctrl+L is swallowed by some terminals such as
  Ghostty and Windows Terminal).
- **`/review` call-tree search** — `/` filters the tree by symbol name or file as you
  type, `n`/`p` cycle matches (wrapping). Matches are underlined, and collapsed
  ancestors expand so a symbol buried in a collapsed subtree is revealed.

### MCP
- **OAuth logins start only from `/mcp auth <server>`.** Startup leaves `NeedsAuth`
  servers alone instead of opening a browser unprompted; a server waits until invoked
  explicitly, then reconnects once the login lands. Tab completes server names.

---

## Fixes

- **Persistent PowerShell:** serialize same-session calls, including batch children,
  so output and completion markers cannot be consumed by the wrong call. Queued
  calls share their timeout budget with execution and recover after a session reset.
- **Tab completion:** restored path and `/skill:` name completion while typing in the
  prompt editor. The completion key now binds in insert mode instead of normal mode.
- **Windows input & review responsiveness** (large change, incl. a vendored crossterm
  fork for Windows terminal handling). Same commit also ships a terminal-native theme
  and session recall.

---

## Tooling

- **`just rebase-upstream`** helper for the fork's rebase-on-upstream workflow.

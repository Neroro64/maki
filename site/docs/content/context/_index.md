+++
title = "Context"
weight = 31
[extra]
group = "Concepts"
+++

# Context

Everything the model knows about your project passes through one context window, and every token in it costs money and attention. This page covers what Maki puts there, when, and where you should put things so they land well.

## What loads when

```
session start (paid every request)   on demand (paid when used)
──────────────────────────────────   ─────────────────────────────────
system prompt                        file contents   read / index / grep
tool definitions                     skill bodies    skill tool
instruction files (AGENTS.md, ...)   memory notes    memory tool
memory tag names                     subdir rules    first read there
skill names + descriptions           MCP tool defs   tool_search
```

The left column is the fixed overhead of every single request, so Maki keeps it small on purpose: a skill contributes one description line, memories one list of tags, a big MCP server one search tool. The bodies stay on disk until the agent asks.

## One prompt per session

Maki builds the system prompt and tool definitions once per session and sends the same bytes on every request. Providers cache by prefix, so an edit near the front makes the next request pay for the whole conversation again. On Claude models it also drops earlier thinking.

When something changes mid-session (the date, the model, plan mode, the directory after `/cd`, a new memory tag), Maki appends a short update before your next message.

A fresh prompt is built only on compaction, or for a change an update cannot carry: different tools (`/reload`, `/workflow`, a model with other capabilities) or a different system prompt passed in [headless](../headless/) mode. Such a change starts the cache over, and Maki tells you when it happens.

Claude binds each thinking block to the prompt it was written under. Accounts that enforce this reject a request whose prompt moved under earlier thinking. When that happens Maki drops all earlier thinking once, tells you, and the session goes on. Later thinking is bound to the new prompt.

A resumed session keeps its prompt across Maki upgrades, as long as the upgrade left the tools alone. Run `/compact` to pick up the new default.

A request carries at most 100 images. Past that, Maki replaces the oldest 25 with a note at once, so the cache breaks once per 25 images rather than on every one.

## Plugin prompt overrides

Plugins can replace a named prompt section or a complete template with
[`maki.api.set_prompt`](/docs/lua-api/#maki-api-set_prompt). Maki keeps its
coding defaults when no plugin replaces them.

```lua
maki.api.set_prompt({
    prompt = "system",
    slot = "conventions",
    content = "State uncertainty and cite the evidence you used.",
})

maki.api.set_prompt({
    prompt = "system",
    content = "You help analyse information.\n{{tool_usage}}\n{{instructions}}",
})
```

The first call replaces the default conventions and keeps other sections. The
second supplies the whole template. Sections, tool hints, and project
instructions appear only at explicit markers. No environment or coding
instructions are appended to a complete replacement.

| Prompt ID | Consumer |
| --- | --- |
| `system` | Main agent in the TUI, print, SDK, and ACP |
| `research`, `general` | Built-in task agents |
| `plan` | Plan entry update |

Sections are `identity`, `tone`, `objectivity`, `tool_usage`, `efficient_tools`,
`conventions`, `completion`, and `after_instructions`. A section must occur in
the selected template. A known section absent from an explicit target is
ignored with a warning. Research and general prompts also have identity and
tone sections. Omitting `prompt` in `set_prompt` targets only `system`.
Additive hints without `prompt` target all conversational prompts that contain
the section.

Use `content = ""` to remove default section text or supply an empty template.
Use [`register_prompt_hint`](/docs/lua-api/#maki-api-register_prompt_hint) to add
text to a section without replacing its default. Additive hints remain in an
included section after its default is replaced.

The alphabetically last plugin wins a replacement. Within that plugin, the
last registration wins. Unloading it restores the previous owner's value or
the built-in default. Complete templates and section overrides compose after
all registrations are resolved.

Whole templates and default section replacements take static strings.
Identity and tone also accept functions. Changed values arrive through context
updates. Unloading the last override tells the model that it no longer applies.
Changes to the date, working directory, or project instructions also use
context updates, including with a complete main template. Changing the static
system or plan replacement policy starts an announced new request frame.
A changed plan prompt applies when the agent next enters plan mode.
Side questions use the stored frame when one exists.
An explicit SDK system-prompt override takes precedence over a plugin's main
template.

Prompt text does not change tool permissions, plan-mode restrictions, or
instruction-file discovery. Select tools through plugin configuration and
keep permission rules in their existing configuration.

## Instruction files

At session start Maki walks from the project git root down to the working directory (no `.git` root, only the cwd). In each directory it loads **one** project instruction file, first match wins:

| Order | File |
|------|------|
| 1 | `AGENTS.md` |
| 2 | `CLAUDE.md` |
| 3 | `.github/copilot-instructions.md` |
| 4 | `COPILOT.md` |
| 5 | `.cursorrules` |
| 6 | `.windsurfrules` |
| 7 | `.clinerules` |
| 8 | `CONVENTIONS.md` |
| 9 | `GEMINI.md` |
| 10 | `CODING_AGENT.md` |

After the match it always loads `AGENTS.local.md` from the same directory if present: that one is yours, keep it gitignored. Closer directories win on conflicts. Finally one global `~/.config/maki/AGENTS.md` for preferences that follow you across projects.

```
~/repo/AGENTS.md           loaded (root)
~/repo/AGENTS.local.md     loaded (yours, gitignored)
~/repo/api/CLAUDE.md       loaded when cwd is ~/repo/api, wins over root
~/repo/web/AGENTS.md       not loaded yet...
~/.config/maki/AGENTS.md   loaded (global)
```

That `web/AGENTS.md` is not dead weight. The first time the agent `read`s a file under a subdirectory whose instruction file was never loaded, Maki pulls it in. Monorepo rules live next to the code they govern and cost nothing until someone works there.

Put coding conventions, repo quirks, and off-limits directories in these files. Keep them short; the next section explains why.

### Your own file list

The table above is the default. `instructions.files` in the global `~/.config/maki/init.lua` replaces it, and `instructions.overrides` narrows it per checkout. Both are personal settings, so a project `init.lua` cannot set them:

```lua
maki.setup({
    -- Only load CLAUDE.md, wherever it exists:
    instructions = {
        files = { "CLAUDE.md" },
        -- Except in this checkout, where no project file loads at all:
        overrides = {
            { remote = "github.com/example/*", files = {} },
        },
    },
})
```

An empty `files` list skips the configured project instruction files. `AGENTS.local.md` and learned `.maki/rules.md` still load. An `overrides` entry matches by `path` (a glob against the project root) or by `remote` (a glob against the project's git remotes, normalized to `host/path`); an entry with neither matches every project. The first matching entry wins, and one that sets no `files` still ends the search. See [configuration](/docs/configuration/#instructions).

## Learning from corrections

Run `/reflect` after correcting a mistake and applying its working fix. Optional
arguments describe the correction, for example `/reflect use std::path::Path,
not std::bad::Path`. A rewind preserves a compact discarded trajectory for the
same session, so a later reflection can include the rejected attempt.

The configured weak model tier sees bounded failure, correction, and final-source
snippets. It proposes one operational rule or a regex/tree-sitter guard. Maki
rejects unsupported patterns, ungrounded examples, platitudes, and over-budget
rules before saving. A guard must match the failed example and accept the fix.

Behavioral rules append to `.maki/rules.md` and reach the current session through
an append-only notice. Later sessions load them beside project instructions.
Each rule is limited to 40 words and 240 UTF-8 bytes. The file has a total budget
of 512 estimated tokens.

Guards live in `.maki/guards.lua` as versioned JSON inside a Lua string literal.
The file is parsed as data, never executed. Built-in edit and write paths reject
new matches before changing files, including after concurrent AST reconciliation.
Unchanged baseline matches do not block unrelated edits. Invalid stored policy
fails closed. Shell commands and direct plugin filesystem calls do not use this
pre-write seam.

## Four places to put knowledge

All four end up in context, but at different times and prices:

| | Loaded | Costs | Good for |
|---|--------|-------|----------|
| `AGENTS.md` | every session | every request | short rules: conventions, build commands, no-go areas |
| [Skills](/docs/skills/) | when the agent picks one | a description line until then | long playbooks: release process, plugin authoring |
| Memory | when the agent recalls a tag | tag names until then | gotchas the agent learns while working |
| [Commands](/docs/commands/) | when you type `/name` | nothing until invoked | prompts you keep retyping |

Rule of thumb: when `AGENTS.md` grows past a screen, the new material probably wants to be a skill. `AGENTS.md` is a tax on every request; a skill is a tax only on the sessions that need it.

## Pointing at a file with `@`

Naming the file saves the agent a search, which costs a tool call and a few hundred tokens. Type `@` in the chat input to open a completion popup, ranked like the `Ctrl+S` file picker:

```
> explain @maki-ui/src/app/mo
                ╭─────────────────────────────────╮
                │ maki-ui/src/app/mod.rs          │
                │ maki-ui/src/app/model.rs        │
                ╰ ↑/↓ move Enter insert Esc close ╯
```

| Key | Action |
|-----|--------|
| `↓`, `Ctrl+N` | Next row |
| `↑`, `Ctrl+P` | Previous row |
| `Enter` | Insert the highlighted path |
| `Esc` | Close the popup |

The popup takes these keys only while it is open. Otherwise `↑` and `↓` still walk the input history and `Ctrl+P` still opens `/sessions`. The characters your query matched are highlighted the way the file picker highlights them. Keep typing to narrow the list. A space ends the mention, so an email address does not open the popup. Moving the caret out of the mention closes it. With no match, `Enter` closes the popup without sending. While the agent is working, the first `Esc` only closes the popup, and after that `Esc` stops the turn as usual.

The inserted path is plain text in your message. Nothing is attached or read until the agent calls `read`.

The plugin is off by default while its file index is tested on large repositories. Turn it on in `init.lua`:

```lua
maki.setup({
  plugins = {
    completion = { enabled = true },
  },
})
```

On a large repository the first index walk takes a moment. The popup shows `scanning…` and fills in when the walk finishes. If the walk has not reported back after a few seconds, it shows `no matches`. Other options are under `plugins.completion` in [configuration](/docs/configuration/).

Plugins can add their own entries to the popup, such as issues after `#` or people after `@`. See [completion sources](/docs/hooks/#completion-sources).

## Completing a path with `Tab`

The `@` popup searches the file index. `Tab` completes the path you are typing, so you can point at a file by its real path without leaving the keyboard. Put the caret in a path token and press `Tab`:

```
> read F:/Some
       ╭─────────────────────────────────╮
       │ F:/SomeApp/                     │
       │ F:/SomeLib/                     │
       ╰ ↑/↓ move  Tab insert  Esc close ╯
```

`Tab` lists the folder the token points at when the token looks like a path, and toggles plan mode when it does not. A token counts as a path when it has a separator, a leading `~`, a leading `.`, a leading `/`, or a drive letter. Directories come first, each with a trailing `/`. `~/` is your home, `./` and `../` are relative to the session's working directory, and `F:/` is a Windows drive root.

| Key | Action |
|-----|--------|
| `↓`, `Ctrl+N` | Next row |
| `↑`, `Ctrl+P` | Previous row |
| `Tab`, `Enter` | Insert the highlighted entry |
| `Esc` | Close the popup |

While the popup is up it takes these keys, so `Tab` inserts a row rather than toggling mode. The popup closes when you move the caret out of the token, edit the input, switch sessions, or switch the focused task. The plugin is on by default, and its options are under `plugins.tabcomplete` in [configuration](/docs/configuration/).

## Rewinding chat and code

Press `Esc` twice while idle to select an earlier prompt. `Enter` restores chat
and code. `Tab` restores chat only. The selected prompt returns to the input
for revision.

Before a file-writing or shell tool runs, Maki saves the working tree under
`refs/maki/snapshots/<session-id>/<turn-id>` in the project repository. These
commits do not move `HEAD` or enter branch history. A full rewind restores the
original Git index as well as file contents, preserving staged changes and
pre-existing untracked files. It removes recorded session-created files and
prunes later snapshot refs. It does not run a blanket `git clean`.

Restoration waits for active file mutations. Tasks from the discarded timeline
cannot write afterward. A failed restoration leaves the chat unchanged. Outside
a Git repository, filesystem rewind requires the existing checkpoint option.


## When the window fills

Long sessions eventually approach the model's context limit. Maki reserves a slice of the window (`agent.compaction_buffer`, default 20%) and before running out it summarizes the older turns and continues from the summary. `/compact` triggers it early, `/compact keep the repro steps` steers that one summary, `/usage` shows where the tokens went, and `agent.compaction_instructions` steers every summary.

Compaction replaces the older turns in the session's on-disk log with the summary. The dropped turns are not lost: before the rewrite, Maki parks the previous log at `sessions/archive/<session-id>/<n>.jsonl` in the [state directory](/docs/configuration/#directory-layout). It keeps the newest three per session, and at most 32 MB of them. The names count up, so the highest number is the newest.

An archive is a complete session file, so `jq` or an editor reads it as it is. To open one in Maki you have to put it back in place of the live log, which drops the session's current state, so move that out of the way first:

```sh
cd ~/.local/state/maki/sessions
mv <session-id>.jsonl <session-id>.jsonl.bak
cp archive/<session-id>/<n>.jsonl <session-id>.jsonl
maki -r <session-id>
```

`MAKI_DISABLE_AUTOCOMPACT=1` turns off the automatic compaction. A manual `/compact` still compacts.

## Background tasks

By default the [`task`](/docs/tools/#task) tool waits for its subagent. Turn on `allow_background` and the model gets a `background` input:

```lua
maki.setup({
    plugins = {
        task = { allow_background = true },
    },
})
```

With `background = true` the tool answers right away and the parent keeps working. To wait, the parent ends its turn. When the subagent is done, its result arrives as a notice that wakes the parent. Structured output and the partial output of an interrupted run come back the same way.

To stop a task, open `/tasks`, select it, and press Ctrl+S. The parent still gets the result on its next turn, but the notice does not wake it.

Background and blocking tasks share `plugins.task.max_concurrent`. A background task cannot ask for permission, so its tools need [allow rules](/docs/permissions/) or YOLO. Plan mode still blocks its file writes. Closing the chat or reloading plugins stops the chat's background tasks. Headless, SDK, and ACP runs have no live chat to deliver to, so there the call blocks as usual.

### Concurrent edits

Task subagents acquire qualified AST symbol locks before built-in edits. Two
tasks can change different functions or methods in one file. Maki reconciles
their cached read baselines by symbol, so line-count changes in one function do
not redirect another task's numbered edit. A conflicting edit reports the
holder, task intent, symbol, and path before writing.

Locks last until the task completes, stops, or is rewound. Structural changes,
ambiguous declarations, and unsupported languages conservatively lock the whole
file. Whole-file writes retain strict stale-read checks. Main-agent edits check
active task claims but do not retain their own symbol locks. These claims cover
the built-in edit and write tools, not arbitrary shell commands or external
editors.

The [diff integrity gate](/docs/hooks/#diff-integrity-gate) checks changed code
before a main agent finishes or a task returns its summary. It requires fixes
or current-diff justifications for removed checks and new suppressions.

Related: [Token Economy](/docs/token-economy/) for why all this frugality exists, [Configuration](/docs/configuration/) for the knobs.

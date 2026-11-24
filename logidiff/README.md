# logidiff

Interactive TUI code review for the agentic era. Rust + ratatui.

AI agents produce large diffs fast; a flat file-by-file line diff hides what a
reviewer needs: **which behavior changed and how it flows**. logidiff parses a
git range's changed files with tree-sitter, builds call and explicit
contract/dependency graphs at both revisions, and reviews the change the way it
executes — starting at entry points, DFS-ing into callees and implementations,
with a symbol diff at every hop.

Languages: Rust, Python, TypeScript/TSX, JavaScript/JSX, Go, C#, C, C++.

## Quick start

```bash
logidiff                      # revision selector TUI
logidiff HEAD~1 HEAD          # explicit revisions
logidiff main origin/main
logidiff HEAD                 # working tree vs HEAD (non-ignored untracked included)
logidiff HEAD -e 'Worker::run'
logidiff HEAD -e 'IWorker::Run'
logidiff HEAD -e 'src/x.rs::run'
logidiff HEAD --dump          # print the review tree, no TUI
logidiff HEAD --json          # full analysis as one JSON document
logidiff --all --tests --focus
```

`base` defaults to `HEAD~1`; `head` defaults to the working tree when a base is
given. Flags:

- `-e/--entry <symbol>` — start node. An exact node key wins; otherwise a simple,
  qualified, or path-qualified suffix must match exactly one symbol, and
  ambiguous matches list every candidate key.
- `--dump` / `--json` — non-interactive output (mutually exclusive).
- `--all` — include clean entry points. `--tests` — include test symbols.
- `--focus` — start with unchanged branches hidden (TUI only).
- `--repo <path>` — repository root (default: cwd).

## Status glyphs

- `~` implementation, declaration signature, or type header changed
- `+` added · `-` removed
- `>` renamed/moved without a source edit

Renames with edits keep their original name and base span when the matching
evidence is strong, and are shown as `~` so the edit is not hidden; ambiguous
matches stay added/removed. Unchanged callers and implementations affected by a
contract change stay in the tree; `f` hides everything irrelevant. logidiff
reviews changes — it never performs rename refactorings or rewrites code.

## Keys

Selector: `j/k` move · `tab`/`h`/`l` switch source/target pane · `enter` diff the
selected pair · `s` swap · `/` filter · `i` custom revision · `q`/`esc` quit.

Review: `j/k` move · `Ctrl+D`/`Ctrl+U` half-page scroll · `enter`/`space`/`l`
toggle expansion · `h` collapse the row or its nearest expanded ancestor · `x`
expand subtree (depth 5) · `e` collapse to roots · `f` focus relevant branches ·
`g/G` first/last row · `tab` switch pane · `enter`/`space`/`z` toggle a folded
unchanged section · `Z` toggle expanding all unchanged sections ·
`Shift+H`/`Shift+L` scroll the focused panel by 8 columns · `?` help · `q` quit.

The detail pane is syntax-highlighted (`+` green, `-` red, dimmed context,
`⋯ N unchanged lines ⋯` between hunks). Unchanged symbols show their full source
in gray, including callees reached from changed callers.

## Output

`--dump` rows: `{indent}{glyph} {name} :{label}  {start}-{end}{kids}` — two
spaces per depth, 1-based inclusive lines of the relevant revision (base for
removed symbols), ` …` for hidden children. Roots start expanded; `--focus` does
not change dump output.

`--json` is a single document with `base`, `head`, `summary` (`added`, `removed`,
`changed`, `moved`), selected `roots`, all `entry_points`, and `nodes` sorted by
key. Each node: `key`, `name`, `file`, `label`, `kind` (`function`,
`declaration`, `type`, `value`, `file`), `role` (`use case`, `data flow`, `logic`,
`self-contained`), `status` (`added`, `removed`, `changed`, `moved`,
`unchanged`), nullable `head`/`base` spans (`file`, 1-based inclusive
`start`/`end`), nullable `moved_from`, `relevant`, `callers`,
`external_callers`, `callees`, `lost_callees`, `implementations`, `contracts`,
`consumers`, `external_consumers`. Summary counts cover every symbol kind;
unchanged implementation context adds no churn.

`implementations` and its reverse `contracts` are type-membership and
explicit-inheritance edges taken from **either** revision, so impact paths survive
signature/base-list edits that break a head-revision relationship. They are not
claims that head compiles, and they are not executed calls — `callees` stays
separate. The TUI labels these edges `(base/head)`.

## Cache

Analyses are cached per revision pair in `logidiff/analysis-*.json` under the
repository's git directory (per linked worktree, 16 recent entries). Validity
covers `HEAD`, resolved source/target commits, `--tests`, the executable, and the
contents of all supported sources; presentation options (`--entry`, `--all`,
`--focus`, `--dump`, `--json`) reuse the cache. Cached reruns print
`using cached analysis` to stderr and skip parsing and graph construction.
Corrupt or unwritable caches warn and fall back to a fresh analysis. Clear it by
removing the files under
`git rev-parse --path-format=absolute --git-path logidiff`.

## How it works

- Symbols are keyed `file::qual` with containers included (`A::new`,
  `Worker::Run`, `ns::Worker::run`); same-`qual` duplicates are disambiguated by
  normalized signature text, then by appearance order.
- Pairing uses qualified identity and signature before comparing source hashes.
  Renamed and copied files are compared against their original path, so a pure
  file rename reports no churn; remaining exact rename fingerprints pair as
  moves. Rename-with-edit requires one-to-one evidence (matched members or
  containers, corresponding call rewrites, or strong token similarity with the
  same body structure), keeping the base span, `moved_from`, and status `changed`.
- Type headers and Rust impl headers are separate symbols whose hashes and diff
  source exclude member bodies, so a method edit does not mark its type header
  changed.
- Receiver resolution uses local/parameter annotations, `self`/`this`, and
  explicit qualified calls within the type hierarchy and lexical namespace.
  Interface/virtual calls fan out to implementations, not unrelated same-name
  methods. Unresolved calls stay `(ext)`; callers outside the diff stay
  `(outside diff)`.
- Entry points are root components of the review graph (with deterministic
  representatives for cycles); review order is deterministic and changed-first.
  A symbol is relevant if it changes, is an explicit dependent of a changed
  contract/type, or leads to relevant behavior.

## Limitations

Conservative syntax-based review, not compiler-backed type checking. Only
explicit inheritance/implementation relationships and identifiable receiver types
are modeled; imports/aliases, inferred receiver types, reflection, runtime
dispatch, and Go/TypeScript structural interfaces are not resolved. Overload and
override candidates use names and compatible parameter counts, not full type
compatibility. Rename matching is evidence-based: large edits, trivial edited
bodies, and ambiguity remain added/removed. C/C++ macros are not expanded. C#
properties/accessors, operator declarations, and calls outside named functions
(including top-level statements) are not modeled. Only bound closures become
named symbols; inline closures are attributed to their enclosing function.
Removed symbols retain base callees, but callers outside the diff are scanned at
head only.

## Prior art

[difftastic](https://github.com/Wilfred/difftastic) (structural line diffs),
[SemanticDiff](https://semanticdiff.com) (semantic diff, VS Code/GitHub),
[hopdiff](https://github.com/tautology-labs/hopdiff) (call-graph diff, static
output, TS only), [calldiff](https://github.com/tanishqkancharla/calldiff)
(call-stack diff, static output). logidiff's niche is the *interactive* session:
DFS from entry points with a diff at every hop, in the terminal.

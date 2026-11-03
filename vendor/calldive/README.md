# calldive

Interactive TUI code review for the agentic era. Built with Rust + ratatui.

AI agents produce large diffs fast. A flat, file-by-file line diff buries the
one thing a senior reviewer actually needs: **what behavior changed and how it
flows**. calldive reviews a change the way it executes:

1. Parse the changed files of a git range with tree-sitter (Rust, Python,
   TypeScript/TSX, JavaScript/JSX, Go, C#, C, C++). In worktree mode, untracked
   (non-ignored) files are reviewed as added files.
2. Build call and explicit type/contract dependency graphs at both revisions.
3. Start at the **entry points** and DFS into callees and implementations —
   every hop shows the symbol diff, callers, and related contracts/members.

## Status glyphs

- `~` implementation, declaration signature, or type header changed
- `+` added
- `-` removed
- `>` renamed/moved without another source edit (including recursive self-renames)

Renames with edits retain their original name and base span when matching evidence
is strong, but use `~` so the edit is not hidden. Ambiguous matches remain added
and removed rather than inventing a rename. calldive reviews changes; it does
not perform rename refactorings or rewrite implementations.

Unchanged callers and explicit implementations affected by a contract change
remain relevant. `f` hides branches that are not relevant.

## Usage

```bash
# review the working tree (including non-ignored untracked files) against HEAD~1
calldive
# review a branch against main
calldive main feature/my-branch
# scope to a simple name, container-qualified name, path suffix, or exact node key
calldive HEAD --entry process_refund
calldive HEAD --entry 'Worker::run'
# start at an interface/base type or a contract declaration
calldive HEAD --entry 'IWorker'
calldive HEAD --entry 'IWorker::Run'
calldive HEAD --entry 'src/x.rs::run'
calldive HEAD --entry 'x.rs::run'
# non-interactive review tree or complete machine-readable analysis
calldive HEAD --dump
calldive HEAD --json
# include clean root components / test symbols
calldive --all
calldive --tests
# initially hide branches that do not reach a change (TUI only)
calldive --focus
```

An exact `--entry` node key wins. Otherwise a simple or qualified name, or a
path-qualified suffix, must match exactly one symbol; ambiguous matches list
every candidate key in sorted order. `--json` and `--dump` are mutually exclusive.

Dump rows are `{indent}{glyph} {name} :{label}  {start}-{end}{kids}`: two spaces
per depth, 1-based inclusive revision lines, and ` …` for hidden children.
Removed symbols use base lines; other symbols use head lines. Root nodes
start expanded; `--focus` does not alter dump output.

JSON is one document with `base`, `head`, `summary` (`added`, `removed`,
`changed`, `moved`), selected `roots`, all `entry_points`, and `nodes` sorted by
key. Each node contains `key`, `name`, `file`, `label`, `kind` (`function`,
`declaration`, `type`), `status` (`added`, `removed`, `changed`, `moved`,
`unchanged`), nullable `head` and `base` spans (`file`, 1-based inclusive
`start`/`end`), nullable `moved_from`, `relevant`, `callers`, `external_callers`,
`callees`, `lost_callees`, `implementations`, and `contracts`. Summary counts
include all symbol kinds; unchanged implementation context does not add churn.

`implementations` and its reverse `contracts` describe type membership, explicit
inheritance, overrides, and inherited implementations **from either revision**.
They preserve impact paths when a signature/base-list edit breaks a previous
relationship; they are not claims that the head revision compiles or that these
are executed calls. `callees` remains separate. The TUI labels these dependency
edges `(base/head)` and the review tree follows them alongside call edges.

Keys: `j/k` move · `enter`/`l` toggle expansion · `h` collapse the row or its
nearest expanded ancestor · `x` expand subtree (depth limit 5) · `e` collapse
to roots · `f` focus relevant branches · `g/G` first/last row (top/bottom in
details) · `tab` switch pane · `Shift+H`/`Shift+L` scroll the focused panel
left/right by 8 columns (each panel keeps its own offset) · `q` quit.
In the diff panel, `Ctrl+D` / `Ctrl+U` scroll down/up by half a visible page,
stopping at the bottom/top.
The tree starts scrolled past the first selected symbol's namespace prefix;
use `Shift+H` to reveal the full qualification.
In the detail pane, diff markers are
`+` added lines, `-` removed lines, and a space for context; base and head line
numbers refer to their own revisions.

## Analysis cache

The most recent analysis is saved in `calldive/analysis.json` under the
repository's Git directory (each linked worktree has its own cache). Rerunning
the same review prints `using cached analysis` to stderr and skips source
gathering, parsing, and graph construction. JSON stdout remains a single document.

Cache validity includes resolved revisions, `--tests`, the executable, changed
file identities, and the contents of all supported tracked and changed/untracked
worktree sources, including unchanged caller candidates. Same-size edits,
additions, and deletions invalidate the cache; presentation options such as
`--entry`, `--all`, `--focus`, `--dump`, and `--json` reuse it. Revision source
text is retained so cached reviews still show added/removed symbol content.
Corrupt or unwritable caches produce a warning and do not prevent review.
To clear the cache, remove that `analysis.json` file. Locate its directory with
`git rev-parse --path-format=absolute --git-path calldive`.

## How it works

- Changed files are parsed at both revisions. Identities are `file::qual`,
  where `qual` includes containers (for example `A::new`, `Worker::Run`, or
  `ns::Worker::run`). If a file has multiple definitions with the same qual,
  their whitespace-normalized signature text is appended (`Worker::Log(int x)`);
  remaining duplicates receive appearance-order `#n` suffixes.
- Symbols pair by qualified identity and signature before comparing source
  hashes. Renamed or copied files are compared against their original path, so
  a pure file rename reports no symbol churn. Remaining exact rename fingerprints
  pair as moves, preferring the same owner/file. Fingerprints preserve literals
  and comments while excluding the declared name and same-name identifier uses.
  Short type/declaration fingerprints require unique candidates in both revisions.
- Renames with edits require one-to-one evidence from matched members/containers,
  corresponding call rewrites, or strong token similarity with the same body
  structure/operators and a nontrivial implementation. The original source span
  and `moved_from` are preserved, with status `changed`. Speculative ties and
  edited trivial bodies stay added/removed.
- Named type and Rust implementation headers are separate symbols. Their hashes
  and diff source exclude member bodies, so a method edit does not mark its type
  header changed. Exact UTF-8 byte ranges isolate symbols sharing a source line;
  displayed locations remain revision-specific line spans. Rust implementation
  headers use `<Type>` / `<Type as Trait>` identities, including empty impls.
- Sources are decoded as UTF-8 with invalid bytes replaced and a leading BOM
  stripped, identically for working-tree files and git objects.
- TypeScript uses `.ts`/`.mts`/`.cts`, TSX uses the TSX grammar for `.tsx`,
  and JavaScript uses `.js`/`.jsx`/`.mjs`/`.cjs`.
- C# (`.cs`) includes interface/abstract declarations, methods,
  constructors/destructors, local functions, and bound lambdas/anonymous delegates.
  Bodyless contracts also include TS/TSX signatures, Rust trait methods, Go
  interface methods, and C++ class declarations/pure virtual methods. C/C++
  free-function prototypes are not separate symbols.
- C uses `.c`; C++ uses `.C`, `.cc`, `.cpp`, `.cxx`, `.c++`, `.h`, `.hh`,
  `.hpp`, `.hxx`, `.h++`, `.ipp`, `.inl`, and `.tpp`. Ambiguous `.h` headers
  use the C++ grammar, which also handles ordinary C header definitions.
- Let-bound closures/lambdas are captured as named symbols (Rust
  `let f = |x| ...`, JS/TS `const f = (x) => ...`, Python `f = lambda x: ...`,
  Go `f := func() ...`, C# `Func<int> f = () => ...`, C++ `auto f = [] { ... }`),
  so calls through the binding resolve.
- Test symbols are hidden by default; `--tests` includes them. Conventions:
  `tests/` dirs (every language except Go; also `test/` for Python and JS/TS), Rust
  `#[test]`-style attributes and `#[cfg(test)]` declarations/modules, `*_test.go`,
  `test_*.py` / `*_test.py` / `conftest.py`, `*.test.*` / `*.spec.*` /
  `__tests__`, C# `*Test.cs` / `*Tests.cs` plus `[Fact]`/`[Theory]`/`[Test]`/
  `[TestMethod]`/`[TestCase]` methods and `[TestFixture]`/`[TestClass]`
  classes, and C/C++ `test_*` / `*_test` / `*_tests` files.
- Known receivers from local/parameter type annotations, `self`/`this`, and
  explicit qualified calls resolve within their type hierarchy and caller's
  lexical namespace. Explicit base/super calls suppress override fan-out.
  Interface/virtual calls include possible implementations, not unrelated
  same-name methods. Static members and constructors do not become overrides.
- Other calls retain simple-name resolution: same-file overloads first, otherwise
  a unique definition in the language family across loaded sources (JS/TS/TSX
  share a family, as do C/C++). Ambiguous/unresolved calls remain `(ext)`.
  Removed symbols retain base calls; uniquely identified removed members can
  retain typed callers when no head member exists.
- Unchanged sources mentioning changed symbols or their bases are scanned for
  callers and disambiguation; discovered hierarchy names expand that scan
  transitively. Explicit contract/implementation context becomes unchanged review
  nodes; ordinary outside callers remain `(outside diff)`. All blob scans share
  one `git cat-file --batch` process; unchanged sources over 512 KiB are skipped.
- Entry points are root components of the call/dependency review graph, including
  a deterministic representative of cycles. External callers do not disqualify
  a root. Review order is deterministic and changed-first. A symbol is relevant
  if it changes, is an explicit dependent of a changed contract/type, or leads
  to relevant behavior; `--all` includes clean roots as well.
- File labels are basenames unless distinct paths collide, then the shortest
  unique whole-segment suffix (`a/lib.rs`, `b/lib.rs`). Root-level files keep
  their plain filename, never a leading slash.

## Limitations

- This is conservative syntax-based review, not compiler-backed type checking.
  Only explicit inheritance/implementation relationships and identifiable receiver
  types are modeled. Imports/aliases, inferred/dataflow-dependent receiver types,
  reflection, and runtime dispatch are not fully resolved. Go/TypeScript
  structural interface implementations are not inferred without explicit links.
  Overload/override candidates use names and compatible parameter counts, not
  full type compatibility; generic substitutions and exact runtime targets are
  not inferred. Unknown untyped calls can still use the name-based fallback.
- Rename matching is evidence-based, not guaranteed for arbitrary rewrites;
  large edits, trivial edited bodies, and ambiguity can remain added/removed.
- C/C++ macros are not expanded.
- C# properties/accessors and operator declarations are not extracted;
  calls outside named functions (including top-level statements) are not modeled.
- Only bound closures/lambdas become named symbols. Inline closures passed
  as arguments (such as `.map(|x| ...)`) are attributed to their enclosing fn.
- Removed symbols retain base-revision callees, but callers outside the diff
  are scanned only at head, not at base.

## Prior art

[difftastic](https://github.com/Wilfred/difftastic) (structural line diffs),
[SemanticDiff](https://semanticdiff.com) (semantic diff, VS Code/GitHub),
[hopdiff](https://github.com/tautology-labs/hopdiff) (call-graph diff, static
output, TS only),
[calldiff](https://github.com/tanishqkancharla/calldiff) (call-stack diff,
static output). calldive's niche: an *interactive* review session — DFS from
entry points with a diff at every hop, in the terminal.

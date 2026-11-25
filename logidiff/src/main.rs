use anyhow::{Context, Result, bail};
use clap::Parser;
use logidiff::{analysis, git, load_analysis, ui};
use std::collections::HashSet;

/// Interactive, call-graph aware code review for the terminal.
///
/// logidiff parses the changed files of a git range with tree-sitter,
/// builds the call graph per revision, and reviews the change the way it
/// executes: starting at entry points, DFS into callees, with a line diff
/// for every hop.
#[derive(Parser, Debug)]
#[command(name = "logidiff", version, about)]
struct Args {
    /// source / base revision (or interactive selector in TUI mode when omitted)
    #[arg(allow_hyphen_values = false)]
    base: Option<String>,

    /// target / head revision, or "worktree" (default: worktree when source is given)
    #[arg(allow_hyphen_values = false)]
    head: Option<String>,
    /// repository root (default: cwd)
    #[arg(long)]
    repo: Option<String>,

    /// start from a symbol name, qualified name, path-qualified name, or exact key
    #[arg(short = 'e', long)]
    entry: Option<String>,

    /// print the review tree and exit (no TUI)
    #[arg(long)]
    dump: bool,

    /// print the complete analysis as JSON and exit (no TUI)
    #[arg(long, conflicts_with = "dump")]
    json: bool,

    /// show unchanged entry points too
    #[arg(long)]
    all: bool,

    /// include symbols inside test modules / test files
    #[arg(long)]
    tests: bool,

    /// start with unchanged branches hidden (default in TUI; pass --no-focus for full tree)
    #[arg(long)]
    focus: bool,

    /// start with unchanged branches visible in the TUI
    #[arg(long, conflicts_with = "focus")]
    no_focus: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let cwd = std::env::current_dir()?;
    let repo_path = args
        .repo
        .as_ref()
        .map(std::path::PathBuf::from)
        .unwrap_or(cwd);
    let repo = git::GitRepo::open(&repo_path)?;

    let is_interactive_tui = args.base.is_none() && args.head.is_none() && !args.dump && !args.json;
    let (base, head) = if is_interactive_tui {
        let candidates = repo.rev_candidates()?;
        match ui::select_revisions(candidates)? {
            Some(pair) => pair,
            None => return Ok(()),
        }
    } else {
        let base = args.base.clone().unwrap_or_else(|| "HEAD~1".to_string());
        let head = args
            .head
            .clone()
            .unwrap_or_else(|| git::WORKTREE.to_string());
        (base, head)
    };
    if git::is_worktree(&base) {
        bail!("base ref '{base}' cannot be worktree; pass a commit revision as base");
    }
    let base_rev = repo.resolve_commit(&base).with_context(|| {
        format!("base ref '{base}' does not resolve (single-commit repo? pass an explicit base)")
    })?;
    let head_rev = git::resolve_rev(&repo, &head)?;
    let changes = repo.changed_files(&base_rev, head_rev.as_deref())?;
    if changes.is_empty() {
        println!("no file changes between {base} and {head}");
        return Ok(());
    }

    let a = load_analysis(&repo, &changes, &base_rev, head_rev.as_deref(), args.tests)?;

    // roots
    let roots: Vec<String> = match &args.entry {
        Some(e) => {
            if a.nodes.contains_key(e) {
                vec![e.clone()]
            } else {
                let name_suffix = format!("::{e}");
                let path_suffix = format!("/{e}");
                let mut hits: Vec<_> = a
                    .nodes
                    .keys()
                    .filter(|k| k.ends_with(&name_suffix) || k.ends_with(&path_suffix) || *k == e)
                    .cloned()
                    .collect();
                hits.sort();
                match hits.len() {
                    0 => bail!("--entry {e}: no symbol matches"),
                    1 => hits,
                    _ => bail!(
                        "--entry {e}: ambiguous symbol matches:\n{}",
                        hits.join("\n")
                    ),
                }
            }
        }
        None => a
            .entry_points
            .iter()
            .filter(|k| {
                // default: every entry point whose subtree carries a change
                // (includes unchanged entry points that reach changed code)
                args.all || a.nodes.get(*k).map(|n| n.relevant).unwrap_or(true)
            })
            .cloned()
            .collect(),
    };

    let (ad, rm, ch, mv) = a.summary;
    let head_label = if git::is_worktree(&head) {
        "worktree".to_string()
    } else {
        head.clone()
    };

    if args.json {
        #[derive(serde::Serialize)]
        struct JsonSpan<'a> {
            file: &'a str,
            start: usize,
            end: usize,
        }
        #[derive(serde::Serialize)]
        struct JsonNode<'a> {
            key: &'a str,
            name: &'a str,
            file: &'a str,
            label: &'a str,
            kind: &'a str,
            role: &'a str,
            status: &'a str,
            head: Option<JsonSpan<'a>>,
            base: Option<JsonSpan<'a>>,
            moved_from: Option<&'a str>,
            relevant: bool,
            callers: &'a [String],
            external_callers: &'a [String],
            callees: &'a [String],
            lost_callees: &'a [String],
            implementations: &'a [String],
            contracts: &'a [String],
            consumers: &'a [String],
            external_consumers: &'a [String],
        }
        #[derive(serde::Serialize)]
        struct JsonSummary {
            added: usize,
            removed: usize,
            changed: usize,
            moved: usize,
        }
        #[derive(serde::Serialize)]
        struct JsonAnalysis<'a> {
            base: &'a str,
            head: &'a str,
            summary: JsonSummary,
            roots: &'a [String],
            entry_points: &'a [String],
            nodes: Vec<JsonNode<'a>>,
        }
        fn span(s: &analysis::Span) -> JsonSpan<'_> {
            JsonSpan {
                file: s.file.as_str(),
                start: s.start_row + 1,
                end: s.end_row,
            }
        }
        let mut nodes: Vec<_> = a.nodes.values().collect();
        nodes.sort_by(|left, right| left.key.cmp(&right.key));
        let output = JsonAnalysis {
            base: &base,
            head: &head_label,
            summary: JsonSummary {
                added: ad,
                removed: rm,
                changed: ch,
                moved: mv,
            },
            roots: &roots,
            entry_points: &a.entry_points,
            nodes: nodes
                .into_iter()
                .map(|n| JsonNode {
                    key: &n.key,
                    name: &n.name,
                    file: &n.file,
                    label: a.label(&n.file),
                    kind: n.kind.label(),
                    role: a.classify_node(n).label(),
                    status: n.status.label(),
                    head: n.head.as_ref().map(span),
                    base: n.base.as_ref().map(span),
                    moved_from: n.moved_from.as_deref(),
                    relevant: n.relevant,
                    callers: &n.callers,
                    external_callers: &n.external_callers,
                    callees: &n.callees,
                    lost_callees: &n.lost_callees,
                    implementations: &n.implementations,
                    contracts: &n.contracts,
                    consumers: &n.consumers,
                    external_consumers: &n.external_consumers,
                })
                .collect(),
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }
    if args.dump {
        println!(
            "logidiff {base} → {head_label}   symbols +{ad} -{rm} ~{ch} >{mv}   entry points: {}",
            roots.len()
        );
        let expanded: HashSet<String> = roots.iter().cloned().collect();
        let rows = analysis::build_rows(&a, &roots, &expanded, false);
        for r in &rows {
            let n = &a.nodes[&r.key];
            let span = n
                .head
                .as_ref()
                .or(n.base.as_ref())
                .expect("node has a revision span");
            let indent = "  ".repeat(r.depth);
            let kids = if r.has_children && !expanded.contains(&r.key) {
                " …"
            } else {
                ""
            };
            let consumer_prefix = if r.is_consumer { "> " } else { "" };
            let shared_note = if r.shared_callers.len() >= 2 {
                format!(" [← {}]", r.shared_callers.join(", "))
            } else if r.is_repeat {
                " [↗]".to_string()
            } else {
                String::new()
            };
            println!(
                "{}{} {}{}{} :{}  {}-{}{}",
                indent,
                n.status.glyph(),
                consumer_prefix,
                n.name,
                shared_note,
                a.label(&n.file),
                span.start_row + 1,
                span.end_row,
                kids
            );
        }
        return Ok(());
    }

    // TUI
    let mut app = ui::App::new(a, roots, !args.no_focus);
    app.flash = Some(format!("reviewing {base} → {head_label}"));
    ui::run(&mut app)?;
    Ok(())
}

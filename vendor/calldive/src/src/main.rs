mod analysis;
mod cache;
mod contracts;
mod diffview;
mod git;
mod syntax;
mod ui;

use analysis::Inputs;
use anyhow::{bail, Context, Result};
use clap::Parser;
use std::collections::{HashMap, HashSet};

/// Interactive, call-graph aware code review for the terminal.
///
/// calldive parses the changed files of a git range with tree-sitter,
/// builds the call graph per revision, and reviews the change the way it
/// executes: starting at entry points, DFS into callees, with a line diff
/// for every hop.
#[derive(Parser, Debug)]
#[command(name = "calldive", version, about)]
struct Args {
    /// base revision (default HEAD~1)
    base: Option<String>,

    /// head revision, or "worktree" (default worktree)
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

    /// start with unchanged branches hidden
    #[arg(long)]
    focus: bool,
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

    let base = args.base.clone().unwrap_or_else(|| "HEAD~1".to_string());
    let head = args
        .head
        .clone()
        .unwrap_or_else(|| git::WORKTREE.to_string());
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

    let cache = match cache::Cache::new(&repo, &changes, &base_rev, head_rev.as_deref(), args.tests) {
        Ok(cache) => Some(cache),
        Err(error) => {
            eprintln!("warning: analysis cache unavailable: {error:#}");
            None
        }
    };
    let cached = cache.as_ref().and_then(|cache| match cache.load() {
        Ok(analysis) => analysis,
        Err(error) => {
            eprintln!("warning: ignoring analysis cache: {error:#}");
            None
        }
    });
    let a = if let Some(analysis) = cached {
        eprintln!("using cached analysis");
        analysis
    } else {
        let (files, extra_files) = gather_sources(
            &repo,
            &changes,
            &base_rev,
            head_rev.as_deref(),
            &base,
            &head,
        )?;
        if files.is_empty() {
            println!("changed files, but none in a supported language (Rust, Python, TypeScript/TSX, JavaScript/JSX, Go, C#, C, C++)");
            return Ok(());
        }
        let analysis = analysis::analyze(&Inputs {
            files,
            extra_head: extra_files,
            include_tests: args.tests,
        });
        if let Some(cache) = &cache {
            let save = || -> Result<()> {
                let current_changes = repo.changed_files(&base_rev, head_rev.as_deref())?;
                let current = cache::Cache::new(
                    &repo, &current_changes, &base_rev, head_rev.as_deref(), args.tests,
                )?;
                if cache.same_snapshot(&current) {
                    cache.store(&analysis)?;
                }
                Ok(())
            };
            if let Err(error) = save() {
                eprintln!("warning: could not save analysis cache: {error:#}");
            }
        }
        analysis
    };

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
                })
                .collect(),
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }
    if args.dump {
        println!(
            "calldive {base} → {head_label}   symbols +{ad} -{rm} ~{ch} >{mv}   entry points: {}",
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
            println!(
                "{}{} {} :{}  {}-{}{}",
                indent,
                n.status.glyph(),
                n.name,
                a.label(&n.file),
                span.start_row + 1,
                span.end_row,
                kids
            );
        }
        return Ok(());
    }

    // TUI
    let mut app = ui::App::new(&a, roots, args.focus);
    app.flash = Some(format!("reviewing {base} → {head_label}"));
    ui::run(&mut app)?;
    Ok(())
}

type ChangedSources = Vec<(String, Option<String>, Option<String>)>;
const MAX_EXTRA_BYTES: usize = 512 * 1024;

/// Load changed blobs first, then scan unchanged callers and the transitive
/// explicit type hierarchy. All phases share one git cat-file process.
fn gather_sources(
    repo: &git::GitRepo,
    changes: &[git::FileChange],
    base_sha: &str,
    head_sha: Option<&str>,
    base_label: &str,
    head_label: &str,
) -> Result<(ChangedSources, Vec<(String, String)>)> {
    let reviewable: Vec<&git::FileChange> = changes
        .iter()
        .filter(|change| syntax::lang_for_path(&change.path).is_some())
        .collect();
    if reviewable.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let base_blobs = repo.blobs_at(base_sha)?;
    let base_index: HashMap<&str, &git::Blob> = base_blobs
        .iter()
        .map(|blob| (blob.path.as_str(), blob))
        .collect();
    let head_blobs = match head_sha {
        Some(sha) => Some(repo.blobs_at(sha)?),
        None => None,
    };
    let head_index = head_blobs.as_ref().map(|blobs| {
        blobs
            .iter()
            .map(|blob| (blob.path.as_str(), blob))
            .collect::<HashMap<_, _>>()
    });
    let mut files: ChangedSources = Vec::with_capacity(reviewable.len());
    // (file index, base side, object ID); object IDs also handle paths with newlines.
    let mut requests: Vec<(usize, bool, &str)> = Vec::new();
    for (index, change) in reviewable.iter().enumerate() {
        let base_path = change.old_path.as_deref().unwrap_or(&change.path);
        if change.status != 'A' {
            let blob = base_index.get(base_path).with_context(|| {
                format!(
                    "missing base source '{base_path}' at rev '{base_label}' (review path '{}')",
                    change.path
                )
            })?;
            requests.push((index, true, &blob.oid));
        }
        let mut head_source = None;
        if change.status != 'D' {
            if let Some(blobs) = &head_index {
                let blob = blobs.get(change.path.as_str()).with_context(|| {
                    format!(
                        "missing head source '{}' at rev '{head_label}'",
                        change.path
                    )
                })?;
                requests.push((index, false, &blob.oid));
            } else {
                head_source = Some(repo.read_worktree(&change.path).with_context(|| {
                    format!(
                        "reading head source '{}' at rev '{head_label}'",
                        change.path
                    )
                })?);
            }
        }
        files.push((change.path.clone(), None, head_source));
    }
    let changed_set: HashSet<&str> = changes.iter().map(|change| change.path.as_str()).collect();
    let mut extra_files = Vec::new();
    repo.with_blobs(|reader| {
        let oids: Vec<&str> = requests.iter().map(|request| request.2).collect();
        reader.read(&oids, |index, source| {
            let (file_index, is_base, oid) = requests[index];
            let change = reviewable[file_index];
            let (side, path, rev) = if is_base {
                (
                    "base",
                    change.old_path.as_deref().unwrap_or(&change.path),
                    base_label,
                )
            } else {
                ("head", change.path.as_str(), head_label)
            };
            let source = source.with_context(|| {
                format!("missing {side} source '{path}' at rev '{rev}' (blob {oid})")
            })?;
            if is_base {
                files[file_index].1 = Some(source);
            } else {
                files[file_index].2 = Some(source);
            }
            Ok(())
        })?;
        let mut terms = changed_terms(&files);
        let mut pending = terms.clone();
        let mut selected: HashSet<String> =
            changed_set.iter().map(|path| (*path).to_string()).collect();
        let tracked = if head_blobs.is_none() {
            repo.tracked_files()?
        } else {
            Vec::new()
        };
        while let Some(search) = identifier_search(std::mem::take(&mut pending))? {
            let mut retain = |path: &str, source: String| {
                if source.len() <= MAX_EXTRA_BYTES && contains_identifier(&search, &source) {
                    // Expand only hierarchy names, not every newly discovered
                    // callable: this follows inherited implementations without
                    // growing into an unrelated whole-repository call graph.
                    for term in contract_terms(path, &source) {
                        if terms.insert(term.clone()) {
                            pending.insert(term);
                        }
                    }
                    extra_files.push((path.to_string(), source));
                    true
                } else {
                    false
                }
            };
            if let Some(blobs) = &head_blobs {
                let mut candidates: Vec<_> = blobs
                    .iter()
                    .filter(|blob| {
                        !selected.contains(&blob.path)
                            && syntax::lang_for_path(&blob.path).is_some()
                            && blob.size <= MAX_EXTRA_BYTES
                    })
                    .collect();
                candidates.sort_by(|a, b| a.path.cmp(&b.path));
                let oids: Vec<_> = candidates.iter().map(|blob| blob.oid.as_str()).collect();
                reader.read(&oids, |index, source| {
                    let path = &candidates[index].path;
                    if retain(
                        path,
                        source.with_context(|| {
                            format!("missing unchanged source '{path}' at rev '{head_label}'")
                        })?,
                    ) {
                        selected.insert(path.clone());
                    }
                    Ok(())
                })?;
            } else {
                let candidates: Vec<_> = tracked
                    .iter()
                    .filter(|path| {
                        !selected.contains(*path) && syntax::lang_for_path(path).is_some()
                    })
                    .collect();
                for path in candidates {
                    let metadata = match std::fs::metadata(repo.root.join(path)) {
                        Ok(metadata) => metadata,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(error) => {
                            return Err(error)
                                .with_context(|| format!("reading worktree metadata '{path}'"))
                        }
                    };
                    if metadata.len() > MAX_EXTRA_BYTES as u64 {
                        continue;
                    }
                    let source = match repo.read_worktree(path) {
                        Ok(source) => source,
                        Err(error)
                            if error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                                error.kind() == std::io::ErrorKind::NotFound
                            }) =>
                        {
                            continue
                        }
                        Err(error) => return Err(error),
                    };
                    if retain(path, source) {
                        selected.insert(path.clone());
                    }
                }
            }
        }
        Ok(())
    })?;
    extra_files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok((files, extra_files))
}

fn changed_terms(files: &ChangedSources) -> HashSet<String> {
    let mut terms = HashSet::new();
    for (path, base, head) in files {
        for source in [base, head].into_iter().flatten() {
            if let Some(parsed) = syntax::parse_file(path, source) {
                for symbol in parsed.fns {
                    add_identifier_terms(&symbol.name, &mut terms);
                    for base in &symbol.bases {
                        add_identifier_terms(base, &mut terms);
                    }
                    if let Some(owner) = &symbol.owner {
                        add_identifier_terms(owner, &mut terms);
                    }
                }
            }
        }
    }
    terms
}

fn contract_terms(path: &str, source: &str) -> HashSet<String> {
    let mut terms = HashSet::new();
    if let Some(parsed) = syntax::parse_file(path, source) {
        for symbol in parsed.fns {
            if symbol.kind == syntax::SymbolKind::Type {
                add_identifier_terms(&symbol.name, &mut terms);
            }
            for base in &symbol.bases {
                add_identifier_terms(base, &mut terms);
            }
        }
    }
    terms
}

fn add_identifier_terms(name: &str, terms: &mut HashSet<String>) {
    for term in name.split(|c: char| !c.is_alphanumeric() && c != '_') {
        if !term.is_empty() && !matches!(term, "as" | "dyn" | "impl" | "mut" | "const") {
            terms.insert(term.to_string());
        }
    }
}

fn identifier_search(terms: HashSet<String>) -> Result<Option<aho_corasick::AhoCorasick>> {
    if terms.is_empty() {
        return Ok(None);
    }
    let mut terms: Vec<_> = terms.into_iter().collect();
    terms.sort();
    Ok(Some(
        aho_corasick::AhoCorasickBuilder::new()
            .match_kind(aho_corasick::MatchKind::Standard)
            .build(terms)
            .context("building symbol identifier search")?,
    ))
}

fn contains_identifier(search: &aho_corasick::AhoCorasick, source: &str) -> bool {
    let bytes = source.as_bytes();
    let is_identifier = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
    search.find_overlapping_iter(source).any(|hit| {
        (hit.start() == 0 || !is_identifier(bytes[hit.start() - 1]))
            && (hit.end() == bytes.len() || !is_identifier(bytes[hit.end()]))
    })
}

#[cfg(test)]
mod input_tests {
    use super::*;

    #[test]
    fn identifier_prefilter_respects_boundaries_and_overlapping_terms() {
        let search = aho_corasick::AhoCorasickBuilder::new()
            .match_kind(aho_corasick::MatchKind::Standard)
            .build(["run", "run_all"])
            .unwrap();
        assert!(contains_identifier(&search, "x.run()"));
        assert!(contains_identifier(&search, "run"));
        assert!(contains_identifier(&search, "run_all()"));
        assert!(!contains_identifier(
            &search,
            "rerun() run_all_more() _run run2"
        ));
        let run = aho_corasick::AhoCorasick::new(["run"]).unwrap();
        assert!(!contains_identifier(&run, "rerun() run_all()"));
        assert!(contains_identifier(&run, "rerun() x.run()"));
    }

    #[test]
    fn changed_names_produce_maximal_ascii_identifier_terms() {
        let files = vec![(
            "src/worker.cpp".to_string(),
            None,
            Some(
                "class Worker { public: ~Worker() {} int operator()() { return 1; } };".to_string(),
            ),
        )];
        let search = identifier_search(changed_terms(&files))
            .unwrap()
            .expect("function names");
        assert!(contains_identifier(&search, "Worker object;"));
        assert!(contains_identifier(&search, "operator()"));
        assert!(!contains_identifier(&search, "WorkerPool operators"));
    }
}

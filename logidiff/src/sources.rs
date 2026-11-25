use crate::{git, syntax};
use anyhow::{Context, Result};
use std::collections::HashSet;

const MAX_EXTRA_BYTES: usize = 512 * 1024;

type ChangedSources = Vec<(String, Option<String>, Option<String>)>;

/// Load changed blobs first, then scan unchanged callers and the transitive
/// explicit type hierarchy. All phases share one git cat-file process.
pub(crate) fn gather_sources(
    repo: &git::GitRepo,
    changes: &[git::FileChange],
    _base_sha: &str,
    head_sha: Option<&str>,
    base_label: &str,
    head_label: &str,
) -> Result<(ChangedSources, Vec<(String, String)>)> {
    if changes.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let head_blobs = match head_sha {
        Some(sha) => Some(repo.blobs_at(sha)?),
        None => None,
    };
    let mut files: ChangedSources = Vec::with_capacity(changes.len());
    // (file index, base side, object ID); object IDs also handle paths with newlines.
    let mut requests: Vec<(usize, bool, &str)> = Vec::new();
    for (index, change) in changes.iter().enumerate() {
        let mut base_source = None;
        if change.status != 'A' {
            let oid = change.base_oid.as_deref().with_context(|| {
                let base_path = change.old_path.as_deref().unwrap_or(&change.path);
                format!(
                    "missing base source '{base_path}' at rev '{base_label}' (review path '{}')",
                    change.path
                )
            })?;
            if change.base_submodule {
                base_source = Some(format!("Git submodule commit {oid}\n"));
            } else {
                requests.push((index, true, oid));
            }
        }
        let mut head_source = None;
        if change.status != 'D' {
            if head_sha.is_some() {
                let oid = change.head_oid.as_deref().with_context(|| {
                    format!(
                        "missing head source '{}' at rev '{head_label}'",
                        change.path
                    )
                })?;
                if change.head_submodule {
                    head_source = Some(format!("Git submodule commit {oid}\n"));
                } else {
                    requests.push((index, false, oid));
                }
            } else {
                head_source = Some(repo.read_worktree(&change.path).with_context(|| {
                    format!(
                        "reading head source '{}' at rev '{head_label}'",
                        change.path
                    )
                })?);
            }
        }
        files.push((change.path.clone(), base_source, head_source));
    }
    let changed_set: HashSet<&str> = changes.iter().map(|change| change.path.as_str()).collect();
    let mut extra_files = Vec::new();
    repo.with_blobs(|reader| {
        let oids: Vec<&str> = requests.iter().map(|request| request.2).collect();
        reader.read(&oids, |index, source| {
            let (file_index, is_base, oid) = requests[index];
            let change = &changes[file_index];
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
        let (mut terms, mut down_hierarchy, mut up_hierarchy) = changed_terms(&files);
        let mut pending = terms.clone();
        let mut selected: HashSet<String> =
            changed_set.iter().map(|path| (*path).to_string()).collect();
        let changed_families: HashSet<syntax::Lang> = files
            .iter()
            .filter_map(|(path, _, _)| syntax::lang_for_path(path).map(syntax::Lang::family))
            .collect();
        let mut pathspecs: Vec<&'static str> = changed_families
            .iter()
            .flat_map(|family| family.family_pathspecs().iter().copied())
            .collect();
        pathspecs.sort_unstable();
        pathspecs.dedup();
        let mut tracked_fallback: Option<Vec<String>> = None;
        while !pending.is_empty() {
            if pathspecs.is_empty() {
                break;
            }
            let batch_terms: Vec<String> = {
                let mut v: Vec<String> = std::mem::take(&mut pending).into_iter().collect();
                v.sort();
                v
            };
            let Some(search) = identifier_search_sorted(&batch_terms)? else {
                break;
            };
            // On worktree head, untracked/modified files on disk can differ from
            // what `git grep` sees without `--untracked` (which git rejects when
            // submodules exist). Union `git grep` hits with dirty/untracked paths!
            let grep_hits = match repo.grep_candidate_files(head_sha, &batch_terms, &pathspecs)? {
                Some(mut hits) if head_sha.is_none() => {
                    if let Ok(dirty) = repo.dirty_tracked_files() {
                        hits.extend(dirty);
                    }
                    if let Ok(untracked) = repo.untracked_files() {
                        hits.extend(untracked);
                    }
                    Some(hits)
                }
                other => other,
            };
            let mut next_down = HashSet::new();
            let mut next_up = HashSet::new();
            let mut matched_batch: Vec<(String, String)> = Vec::new();
            if let Some(blobs) = &head_blobs {
                let mut candidates: Vec<_> = blobs
                    .iter()
                    .filter(|blob| {
                        !selected.contains(&blob.path)
                            && grep_hits
                                .as_ref()
                                .is_none_or(|hits| hits.contains(&blob.path))
                            && syntax::lang_for_path(&blob.path)
                                .is_some_and(|l| changed_families.contains(&l.family()))
                            && blob.size <= MAX_EXTRA_BYTES
                            && blob.source.is_none()
                    })
                    .collect();
                candidates.sort_by(|a, b| a.path.cmp(&b.path));
                let oids: Vec<_> = candidates.iter().map(|blob| blob.oid.as_str()).collect();
                reader.read(&oids, |index, source| {
                    let path = &candidates[index].path;
                    let source = source.with_context(|| {
                        format!("missing unchanged source '{path}' at rev '{head_label}'")
                    })?;
                    if source.len() <= MAX_EXTRA_BYTES && contains_identifier(&search, &source) {
                        selected.insert(path.clone());
                        matched_batch.push((path.clone(), source));
                    }
                    Ok(())
                })?;
            } else {
                let candidate_paths: Vec<String> = if let Some(hits) = grep_hits {
                    let mut v: Vec<String> = hits
                        .into_iter()
                        .filter(|path| {
                            !selected.contains(path)
                                && syntax::lang_for_path(path)
                                    .is_some_and(|l| changed_families.contains(&l.family()))
                        })
                        .collect();
                    v.sort();
                    v
                } else {
                    if tracked_fallback.is_none() {
                        tracked_fallback = Some(repo.tracked_files()?);
                    }
                    tracked_fallback
                        .as_ref()
                        .unwrap()
                        .iter()
                        .filter(|path| {
                            !selected.contains(*path)
                                && syntax::lang_for_path(path)
                                    .is_some_and(|l| changed_families.contains(&l.family()))
                        })
                        .cloned()
                        .collect()
                };
                for path in &candidate_paths {
                    let metadata = match std::fs::metadata(repo.root.join(path)) {
                        Ok(metadata) => metadata,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(error) => {
                            return Err(error)
                                .with_context(|| format!("reading worktree metadata '{path}'"));
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
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                    if source.len() <= MAX_EXTRA_BYTES && contains_identifier(&search, &source) {
                        selected.insert(path.clone());
                        matched_batch.push((path.clone(), source));
                    }
                }
            }
            extra_files.extend(matched_batch);
            if (!down_hierarchy.is_empty() || !up_hierarchy.is_empty()) && !extra_files.is_empty() {
                let workers = std::thread::available_parallelism()
                    .map(std::num::NonZeroUsize::get)
                    .unwrap_or(1)
                    .min(16)
                    .min(extra_files.len());
                if workers <= 1 || extra_files.len() <= 2 {
                    for (path, source) in &extra_files {
                        contract_terms(
                            path,
                            source,
                            &down_hierarchy,
                            &up_hierarchy,
                            &mut next_down,
                            &mut next_up,
                        );
                    }
                } else {
                    let next = std::sync::atomic::AtomicUsize::new(0);
                    let partials: Vec<std::sync::Mutex<(HashSet<String>, HashSet<String>)>> = (0
                        ..workers)
                        .map(|_| std::sync::Mutex::new((HashSet::new(), HashSet::new())))
                        .collect();
                    std::thread::scope(|scope| {
                        for slot in &partials {
                            scope.spawn(|| {
                                let mut local_down = HashSet::new();
                                let mut local_up = HashSet::new();
                                loop {
                                    let idx =
                                        next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    if idx >= extra_files.len() {
                                        break;
                                    }
                                    let (path, source) = &extra_files[idx];
                                    contract_terms(
                                        path,
                                        source,
                                        &down_hierarchy,
                                        &up_hierarchy,
                                        &mut local_down,
                                        &mut local_up,
                                    );
                                }
                                *slot.lock().unwrap() = (local_down, local_up);
                            });
                        }
                    });
                    for slot in partials {
                        let (d, u) = slot.into_inner().unwrap();
                        next_down.extend(d);
                        next_up.extend(u);
                    }
                }
            }
            for term in next_down {
                let is_new_down = down_hierarchy.insert(term.clone());
                if terms.insert(term.clone()) || is_new_down {
                    pending.insert(term);
                }
            }
            for term in next_up {
                let is_new_up = up_hierarchy.insert(term.clone());
                if terms.insert(term.clone()) || is_new_up {
                    pending.insert(term);
                }
            }
        }
        Ok(())
    })?;
    extra_files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok((files, extra_files))
}

/// Extract identifier terms only from symbols that were actually added, removed,
/// edited, or renamed (plus their enclosing types and bases), never from
/// unchanged sibling methods or namespace segments like `EA` or `Core`.
fn changed_terms(files: &ChangedSources) -> (HashSet<String>, HashSet<String>, HashSet<String>) {
    let mut terms = HashSet::new();
    let mut down_hierarchy = HashSet::new();
    let mut up_hierarchy = HashSet::new();
    for (path, base, head) in files {
        let base_parsed = base
            .as_deref()
            .and_then(|src| syntax::parse_file(path, src));
        let head_parsed = head
            .as_deref()
            .and_then(|src| syntax::parse_file(path, src));
        let mut base_hashes: HashSet<(&str, bool, u64)> = HashSet::new();
        let mut head_hashes: HashSet<(&str, bool, u64)> = HashSet::new();
        let mut type_bases_by_qual: std::collections::HashMap<&str, &[String]> =
            std::collections::HashMap::new();
        if let Some(parsed) = &base_parsed {
            for symbol in &parsed.fns {
                base_hashes.insert((
                    symbol.qual.as_str(),
                    symbol.kind == syntax::SymbolKind::Type,
                    symbol.body_hash,
                ));
                if symbol.kind == syntax::SymbolKind::Type {
                    type_bases_by_qual.insert(symbol.qual.as_str(), &symbol.bases);
                }
            }
        }
        if let Some(parsed) = &head_parsed {
            for symbol in &parsed.fns {
                head_hashes.insert((
                    symbol.qual.as_str(),
                    symbol.kind == syntax::SymbolKind::Type,
                    symbol.body_hash,
                ));
                if symbol.kind == syntax::SymbolKind::Type {
                    type_bases_by_qual.insert(symbol.qual.as_str(), &symbol.bases);
                }
            }
        }
        let add_symbol = |symbol: &syntax::FnDef,
                          terms: &mut HashSet<String>,
                          down_hierarchy: &mut HashSet<String>,
                          up_hierarchy: &mut HashSet<String>| {
            add_symbol_name_terms(&symbol.name, terms);
            if symbol.kind == syntax::SymbolKind::Type {
                add_symbol_name_terms(&symbol.name, down_hierarchy);
                add_symbol_name_terms(&symbol.name, up_hierarchy);
            }
            for base_ty in &symbol.bases {
                add_type_ref_terms(base_ty, terms);
                add_type_ref_terms(base_ty, up_hierarchy);
            }
            if let Some(owner) = &symbol.owner {
                add_owner_leaf_terms(owner, terms);
                if symbol.dispatch || symbol.kind == syntax::SymbolKind::Declaration {
                    add_owner_leaf_terms(owner, down_hierarchy);
                }
                add_owner_leaf_terms(owner, up_hierarchy);
                if let Some(owner_bases) = type_bases_by_qual.get(owner.as_str()) {
                    for base_ty in *owner_bases {
                        add_type_ref_terms(base_ty, terms);
                        add_type_ref_terms(base_ty, up_hierarchy);
                    }
                }
            }
        };
        let seed_internal_callers =
            |parsed: &syntax::ParsedFile,
             other_hashes: &HashSet<(&str, bool, u64)>,
             terms: &mut HashSet<String>,
             down_hierarchy: &mut HashSet<String>,
             up_hierarchy: &mut HashSet<String>| {
                let mut changed_indices = HashSet::new();
                for (idx, symbol) in parsed.fns.iter().enumerate() {
                    let key = (
                        symbol.qual.as_str(),
                        symbol.kind == syntax::SymbolKind::Type,
                        symbol.body_hash,
                    );
                    if !other_hashes.contains(&key) {
                        add_symbol(symbol, terms, down_hierarchy, up_hierarchy);
                        changed_indices.insert(idx);
                    }
                }
                if changed_indices.is_empty() {
                    return;
                }
                let mut by_name: std::collections::HashMap<&str, Vec<usize>> =
                    std::collections::HashMap::new();
                for (idx, symbol) in parsed.fns.iter().enumerate() {
                    if symbol.kind != syntax::SymbolKind::Type {
                        by_name.entry(symbol.name.as_str()).or_default().push(idx);
                    }
                }
                let mut callers_by_callee: Vec<Vec<usize>> = vec![Vec::new(); parsed.fns.len()];
                for call in parsed.calls.iter().chain(parsed.value_refs.iter()) {
                    if let Some(targets) = by_name.get(call.name.as_str()) {
                        for &target in targets {
                            callers_by_callee[target].push(call.caller);
                        }
                    }
                }
                let mut visited = changed_indices.clone();
                let mut stack: Vec<usize> = changed_indices.into_iter().collect();
                while let Some(callee) = stack.pop() {
                    for &caller in &callers_by_callee[callee] {
                        if visited.insert(caller) {
                            let symbol = &parsed.fns[caller];
                            if !symbol.is_test {
                                add_symbol(symbol, terms, down_hierarchy, up_hierarchy);
                            }
                            stack.push(caller);
                        }
                    }
                }
            };
        if let Some(parsed) = &base_parsed {
            seed_internal_callers(
                parsed,
                &head_hashes,
                &mut terms,
                &mut down_hierarchy,
                &mut up_hierarchy,
            );
        }
        if let Some(parsed) = &head_parsed {
            seed_internal_callers(
                parsed,
                &base_hashes,
                &mut terms,
                &mut down_hierarchy,
                &mut up_hierarchy,
            );
        }
    }
    (terms, down_hierarchy, up_hierarchy)
}

fn contract_terms(
    path: &str,
    source: &str,
    down_hierarchy: &HashSet<String>,
    up_hierarchy: &HashSet<String>,
    next_down: &mut HashSet<String>,
    next_up: &mut HashSet<String>,
) {
    if down_hierarchy.is_empty() && up_hierarchy.is_empty() {
        return;
    }
    let Some(parsed) = syntax::parse_file(path, source) else {
        return;
    };
    for symbol in parsed.fns {
        if symbol.kind != syntax::SymbolKind::Type {
            continue;
        }
        let mut own_terms = HashSet::new();
        add_symbol_name_terms(&symbol.name, &mut own_terms);
        let mut base_terms = HashSet::new();
        for base in &symbol.bases {
            add_type_ref_terms(base, &mut base_terms);
        }
        if base_terms.iter().any(|t| down_hierarchy.contains(t)) {
            next_down.extend(own_terms.iter().cloned());
            next_up.extend(base_terms.iter().cloned());
        }
        if own_terms.iter().any(|t| up_hierarchy.contains(t)) {
            next_up.extend(base_terms);
        }
    }
}

fn add_symbol_name_terms(name: &str, terms: &mut HashSet<String>) {
    add_identifier_terms(name, terms);
}

fn add_owner_leaf_terms(owner: &str, terms: &mut HashSet<String>) {
    // Strip namespace/module prefix (`EA.TriageEngine::Worker` -> `Worker`,
    // `ns::<Worker as Trait>` -> `<Worker as Trait>`).
    if let Some(open) = owner.find('<') {
        add_type_ref_terms(&owner[open..], terms);
    } else {
        let leaf = owner.rsplit("::").next().unwrap_or(owner);
        let leaf = leaf.rsplit('.').next().unwrap_or(leaf);
        add_identifier_terms(leaf, terms);
    }
}

fn add_type_ref_terms(type_ref: &str, terms: &mut HashSet<String>) {
    // Strip outer generic arguments (`IDbContextFactory<Foo>` -> `IDbContextFactory`),
    // while keeping Rust `<Type as Trait>` parts.
    if let Some(inner) = type_ref
        .trim()
        .strip_prefix('<')
        .and_then(|s| s.strip_suffix('>'))
    {
        for part in inner.split(" as ") {
            add_type_ref_terms(part, terms);
        }
        return;
    }
    let head = type_ref.split(['<', '[']).next().unwrap_or(type_ref).trim();
    let leaf = head.rsplit("::").next().unwrap_or(head);
    let leaf = leaf.rsplit('.').next().unwrap_or(leaf);
    add_identifier_terms(leaf, terms);
}

fn add_identifier_terms(name: &str, terms: &mut HashSet<String>) {
    for term in name.split(|c: char| !c.is_alphanumeric() && c != '_') {
        if !term.is_empty() && !matches!(term, "as" | "dyn" | "impl" | "mut" | "const") {
            terms.insert(term.to_string());
        }
    }
}

#[cfg(test)]
fn identifier_search(terms: HashSet<String>) -> Result<Option<aho_corasick::AhoCorasick>> {
    let mut sorted: Vec<_> = terms.into_iter().collect();
    sorted.sort();
    identifier_search_sorted(&sorted)
}

fn identifier_search_sorted(terms: &[String]) -> Result<Option<aho_corasick::AhoCorasick>> {
    if terms.is_empty() {
        return Ok(None);
    }
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
        let search = identifier_search(changed_terms(&files).0)
            .unwrap()
            .expect("function names");
        assert!(contains_identifier(&search, "Worker object;"));
        assert!(contains_identifier(&search, "operator()"));
        assert!(!contains_identifier(&search, "WorkerPool operators"));
    }
}

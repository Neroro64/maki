use crate::{git, syntax};
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};

const MAX_EXTRA_BYTES: usize = 512 * 1024;

type ChangedSources = Vec<(String, Option<String>, Option<String>)>;

/// Load changed blobs first, then scan unchanged callers and the transitive
/// explicit type hierarchy. All phases share one git cat-file process.
pub(crate) fn gather_sources(
    repo: &git::GitRepo,
    changes: &[git::FileChange],
    base_sha: &str,
    head_sha: Option<&str>,
    base_label: &str,
    head_label: &str,
) -> Result<(ChangedSources, Vec<(String, String)>)> {
    if changes.is_empty() {
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
    let mut files: ChangedSources = Vec::with_capacity(changes.len());
    // (file index, base side, object ID); object IDs also handle paths with newlines.
    let mut requests: Vec<(usize, bool, &str)> = Vec::new();
    for (index, change) in changes.iter().enumerate() {
        let base_path = change.old_path.as_deref().unwrap_or(&change.path);
        let mut base_source = None;
        if change.status != 'A' {
            let blob = base_index.get(base_path).with_context(|| {
                format!(
                    "missing base source '{base_path}' at rev '{base_label}' (review path '{}')",
                    change.path
                )
            })?;
            if let Some(source) = &blob.source {
                base_source = Some(source.clone());
            } else {
                requests.push((index, true, &blob.oid));
            }
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
                if let Some(source) = &blob.source {
                    head_source = Some(source.clone());
                } else {
                    requests.push((index, false, &blob.oid));
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
                            && blob.source.is_none()
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

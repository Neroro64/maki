//! Build the review model: symbols at both revisions, statuses, call edges,
//! entry points and the DFS review tree. Symbols retain revision-specific spans;
//! call resolution considers the complete same-language revision universe.

use crate::contracts::{ContractFile, ContractGraph};
use crate::syntax::{self, CallSite, FnDef, Lang, SymbolKind};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Status {
    Added,
    Removed,
    BodyChanged,
    Moved,
    Unchanged,
}

impl Status {
    pub fn glyph(&self) -> &'static str {
        match self {
            Status::Added => "+",
            Status::Removed => "-",
            Status::BodyChanged => "~",
            Status::Moved => ">",
            Status::Unchanged => " ",
        }
    }

    pub fn is_change(&self) -> bool {
        !matches!(self, Status::Unchanged)
    }

    pub fn label(&self) -> &'static str {
        match self {
            Status::Added => "added",
            Status::Removed => "removed",
            Status::BodyChanged => "changed",
            Status::Moved => "moved",
            Status::Unchanged => "unchanged",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum EntryRole {
    UseCase,
    DataFlow,
    Logic,
    SelfContained,
}

impl EntryRole {
    pub fn label(self) -> &'static str {
        match self {
            Self::UseCase => "use case",
            Self::DataFlow => "data flow",
            Self::Logic => "logic",
            Self::SelfContained => "self-contained",
        }
    }
}

/// A function location in one revision. Rows are zero-based, end exclusive;
/// `file` is an identity path in the corresponding Analysis source map.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Span {
    pub file: String,
    pub start_row: usize,
    pub end_row: usize,
    pub start_byte: usize,
    pub end_byte: usize,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Node {
    pub key: String,
    pub file: String,
    pub name: String,
    pub kind: SymbolKind,
    pub status: Status,
    pub head: Option<Span>,
    pub base: Option<Span>,
    pub moved_from: Option<String>,
    pub callers: Vec<String>,
    pub external_callers: Vec<String>,
    pub callees: Vec<String>,
    pub lost_callees: Vec<String>,
    /// Explicit type/member and contract/implementation edges, not call edges.
    pub implementations: Vec<String>,
    pub contracts: Vec<String>,
    /// Downstream symbols in the review graph that consume this value/type/source change.
    #[serde(default)]
    pub consumers: Vec<String>,
    #[serde(default)]
    pub external_consumers: Vec<String>,
    pub relevant: bool,
}

pub fn node_key(file: &str, name: &str) -> String {
    format!("{file}::{name}")
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Analysis {
    pub nodes: HashMap<String, Node>,
    pub entry_points: Vec<String>,
    /// (+added, -removed, ~changed, >moved) across symbols and file fallbacks.
    pub summary: (usize, usize, usize, usize),
    pub head_srcs: HashMap<String, String>,
    pub base_srcs: HashMap<String, String>,
    pub file_labels: HashMap<String, String>,
}

impl Analysis {
    pub fn head_body(&self, n: &Node) -> Option<String> {
        revision_body(&self.head_srcs, n.head.as_ref()?)
    }

    pub fn base_body(&self, n: &Node) -> Option<String> {
        revision_body(&self.base_srcs, n.base.as_ref()?)
    }

    pub(crate) fn ensure_changed_file(&mut self, path: &str) {
        if self.nodes.values().any(|node| {
            node.status.is_change()
                && (node.file == path || node.base.as_ref().is_some_and(|span| span.file == path))
        }) {
            return;
        }
        let node = file_node(path, self.base_srcs.get(path), self.head_srcs.get(path));
        match node.status {
            Status::Added => self.summary.0 += 1,
            Status::Removed => self.summary.1 += 1,
            _ => self.summary.2 += 1,
        }
        self.entry_points.push(node.key.clone());
        self.nodes.insert(node.key.clone(), node);
    }

    pub(crate) fn rename_file_origin(&mut self, path: &str, old_path: &str, renamed: bool) {
        let Some(source) = self.base_srcs.get(path).cloned() else {
            return;
        };
        let unchanged = self.head_srcs.get(path) == Some(&source);
        self.base_srcs.insert(old_path.to_string(), source);
        for node in self.nodes.values_mut() {
            if let Some(span) = &mut node.base
                && span.file == path
            {
                span.file = old_path.to_string();
                if node.kind == SymbolKind::File && renamed {
                    node.moved_from = Some(old_path.to_string());
                    if unchanged && node.status == Status::BodyChanged {
                        node.status = Status::Moved;
                        self.summary.2 -= 1;
                        self.summary.3 += 1;
                    }
                }
            }
        }
    }

    pub fn label<'a>(&'a self, path: &'a str) -> &'a str {
        self.file_labels
            .get(path)
            .map(String::as_str)
            .unwrap_or(path)
    }

    pub fn classify_node(&self, n: &Node) -> EntryRole {
        let has_consumers = !n.consumers.is_empty() || !n.external_consumers.is_empty();
        let has_callees = n.callees.iter().chain(n.implementations.iter()).any(|c| {
            self.nodes
                .get(c)
                .is_some_and(|cn| cn.relevant && c != &n.key)
        });
        let has_callers = !n.callers.is_empty() || !n.contracts.is_empty();

        if matches!(n.kind, SymbolKind::Value) {
            return if has_consumers || has_callers {
                EntryRole::DataFlow
            } else {
                EntryRole::SelfContained
            };
        }
        if matches!(n.kind, SymbolKind::Type) {
            if has_consumers {
                return EntryRole::DataFlow;
            }
            if has_callees || has_callers {
                return EntryRole::Logic;
            }
            return EntryRole::SelfContained;
        }
        if matches!(n.kind, SymbolKind::File) {
            return if has_callees {
                EntryRole::UseCase
            } else {
                EntryRole::SelfContained
            };
        }
        if has_consumers && !has_callees {
            return EntryRole::DataFlow;
        }
        if has_callees && n.callers.iter().all(|c| c == &n.key) {
            return EntryRole::UseCase;
        }
        if has_callees || has_callers || has_consumers {
            return EntryRole::Logic;
        }
        EntryRole::SelfContained
    }
}

fn file_node(path: &str, base: Option<&String>, head: Option<&String>) -> Node {
    let span = |source: &String| Span {
        file: path.to_string(),
        start_row: 0,
        end_row: source.lines().count(),
        start_byte: 0,
        end_byte: source.len(),
    };
    Node {
        key: node_key(path, "<file>"),
        file: path.to_string(),
        name: path.to_string(),
        kind: SymbolKind::File,
        status: match (base, head) {
            (None, Some(_)) => Status::Added,
            (Some(_), None) => Status::Removed,
            _ => Status::BodyChanged,
        },
        head: head.map(span),
        base: base.map(span),
        moved_from: None,
        callers: Vec::new(),
        external_callers: Vec::new(),
        callees: Vec::new(),
        lost_callees: Vec::new(),
        implementations: Vec::new(),
        contracts: Vec::new(),
        consumers: Vec::new(),
        external_consumers: Vec::new(),
        relevant: true,
    }
}

fn revision_body(srcs: &HashMap<String, String>, span: &Span) -> Option<String> {
    let src = srcs.get(&span.file)?;
    Some(src.get(span.start_byte..span.end_byte)?.replace('\r', ""))
}

pub struct Inputs {
    /// (identity path, base source, head source) for changed files.
    pub files: Vec<(String, Option<String>, Option<String>)>,
    /// Unchanged files supply resolution candidates and external callers,
    /// but never become nodes or disqualify an entry point.
    pub extra_head: Vec<(String, String)>,
    pub include_tests: bool,
}

/// Parsed exactly once; calls and definitions are indexed once per file.
struct FileData {
    path: String,
    family: Lang,
    fns: Vec<FnDef>,
    display: Vec<Option<String>>,
    keys: Vec<Option<String>>,
    calls: Vec<Vec<CallSite>>,
    value_refs: Vec<Vec<CallSite>>,
    is_script: bool,
    by_name: HashMap<String, Vec<usize>>,
}

impl FileData {
    fn parse(path: &str, src: &str, include_tests: bool) -> Option<Self> {
        let family = syntax::lang_for_path(path)?.family();
        let mut parsed = syntax::parse_file(path, src)?;
        if !include_tests {
            let mut remap = vec![None; parsed.fns.len()];
            let mut next = 0;
            parsed.fns = parsed
                .fns
                .into_iter()
                .enumerate()
                .filter_map(|(old, symbol)| {
                    if symbol.is_test {
                        None
                    } else {
                        remap[old] = Some(next);
                        next += 1;
                        Some(symbol)
                    }
                })
                .collect();
            parsed.calls.retain_mut(|call| {
                if let Some(caller) = remap[call.caller] {
                    call.caller = caller;
                    true
                } else {
                    false
                }
            });
            parsed.value_refs.retain_mut(|call| {
                if let Some(caller) = remap[call.caller] {
                    call.caller = caller;
                    true
                } else {
                    false
                }
            });
        }
        let mut qual_counts: HashMap<&str, usize> = HashMap::new();
        for f in &parsed.fns {
            *qual_counts.entry(&f.qual).or_default() += 1;
        }
        let mut display: Vec<Option<String>> = parsed
            .fns
            .iter()
            .map(|f| {
                if qual_counts[f.qual.as_str()] > 1 {
                    Some(format!("{}{}", f.qual, f.sig))
                } else {
                    Some(f.qual.clone())
                }
            })
            .collect();
        let mut counts: HashMap<String, usize> = HashMap::new();
        for name in display.iter().flatten() {
            *counts.entry(name.clone()).or_default() += 1;
        }
        // Reserve all unsuffixed identities so a generated suffix cannot
        // collide with another definition's literal display name.
        let mut used: HashSet<String> = counts.keys().cloned().collect();
        let mut ordinals: HashMap<String, usize> = HashMap::new();
        for name in display.iter_mut().flatten() {
            if counts[name.as_str()] > 1 {
                let ordinal = ordinals.entry(name.clone()).or_default();
                loop {
                    *ordinal += 1;
                    let candidate = format!("{name}#{ordinal}");
                    if used.insert(candidate.clone()) {
                        *name = candidate;
                        break;
                    }
                }
            }
        }
        let keys = display
            .iter()
            .map(|n| n.as_ref().map(|n| node_key(path, n)))
            .collect();
        let mut calls = vec![Vec::new(); parsed.fns.len()];
        for call in parsed.calls {
            if display.get(call.caller).is_some_and(Option::is_some) {
                calls[call.caller].push(call);
            }
        }
        let mut value_refs = vec![Vec::new(); parsed.fns.len()];
        for vref in parsed.value_refs {
            if display.get(vref.caller).is_some_and(Option::is_some) {
                value_refs[vref.caller].push(vref);
            }
        }
        let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, f) in parsed.fns.iter().enumerate() {
            if display[i].is_some() && f.kind != SymbolKind::Type {
                by_name.entry(f.name.clone()).or_default().push(i);
            }
        }
        Some(Self {
            path: path.to_string(),
            family,
            fns: parsed.fns,
            display,
            keys,
            calls,
            value_refs,
            is_script: parsed.is_script,
            by_name,
        })
    }

    fn span(&self, function: usize) -> Span {
        let f = &self.fns[function];
        Span {
            file: self.path.clone(),
            start_row: f.start_row,
            end_row: f.end_row,
            start_byte: f.start_byte,
            end_byte: f.end_byte,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct DefId {
    file: usize,
    function: usize,
}

type OwnerFingerprint<'a> = (Lang, bool, u64, &'a str, Option<&'a str>);

#[derive(Default)]
struct RevisionGroup {
    base: Vec<usize>,
    head: Vec<usize>,
}

/// Pair within qualified-name groups, retaining appearance order for hashes.
fn pair_functions(base: &FileData, head: &FileData) -> Vec<(usize, usize)> {
    let mut groups: BTreeMap<(&str, bool), RevisionGroup> = BTreeMap::new();
    for (i, f) in base.fns.iter().enumerate() {
        if base.display[i].is_some() {
            groups
                .entry((&f.qual, f.kind == SymbolKind::Type))
                .or_default()
                .base
                .push(i);
        }
    }
    for (i, f) in head.fns.iter().enumerate() {
        if head.display[i].is_some() {
            groups
                .entry((&f.qual, f.kind == SymbolKind::Type))
                .or_default()
                .head
                .push(i);
        }
    }
    let mut paired_base = vec![false; base.fns.len()];
    let mut paired_head = vec![false; head.fns.len()];
    let mut pairs = Vec::new();
    for RevisionGroup { base: bs, head: hs } in groups.values() {
        if bs.len() == 1 && hs.len() == 1 {
            pairs.push((bs[0], hs[0]));
            continue;
        }
        let mut base_sigs: HashMap<&str, Vec<usize>> = HashMap::new();
        let mut head_counts: HashMap<&str, usize> = HashMap::new();
        for &b in bs {
            base_sigs.entry(&base.fns[b].sig).or_default().push(b);
        }
        for &h in hs {
            *head_counts.entry(&head.fns[h].sig).or_default() += 1;
        }
        for &h in hs {
            let sig = head.fns[h].sig.as_str();
            if head_counts[sig] == 1
                && let Some(candidates) = base_sigs.get(sig)
                && candidates.len() == 1
            {
                let b = candidates[0];
                paired_base[b] = true;
                paired_head[h] = true;
                pairs.push((b, h));
            }
        }
        let mut hashes: HashMap<u64, VecDeque<usize>> = HashMap::new();
        for &b in bs {
            if !paired_base[b] {
                hashes
                    .entry(base.fns[b].body_hash)
                    .or_default()
                    .push_back(b);
            }
        }
        for &h in hs {
            if !paired_head[h]
                && let Some(b) = hashes
                    .get_mut(&head.fns[h].body_hash)
                    .and_then(VecDeque::pop_front)
            {
                pairs.push((b, h));
            }
        }
    }
    pairs
}

pub fn analyze(inputs: &Inputs) -> Analysis {
    let mut base_files = Vec::new();
    let mut head_files = Vec::new();
    let mut extras = Vec::new();
    let mut file_pairs = Vec::new();
    let mut base_srcs = HashMap::new();
    let mut head_srcs = HashMap::new();
    // Parallelize tree-sitter parsing across CPU cores while preserving exact
    // deterministic input ordering.
    let worker_count = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1)
        .min(16);
    fn parse_parallel<'a, T, F>(
        items: &'a [T],
        workers: usize,
        parse_one: F,
    ) -> Vec<Option<FileData>>
    where
        T: Sync,
        F: Fn(&'a T) -> Option<FileData> + Sync,
    {
        if items.len() <= 2 || workers <= 1 {
            return items.iter().map(parse_one).collect();
        }
        let next = std::sync::atomic::AtomicUsize::new(0);
        let mut results: Vec<Option<FileData>> = (0..items.len()).map(|_| None).collect();
        let slots: Vec<std::sync::Mutex<Option<FileData>>> = (0..items.len())
            .map(|_| std::sync::Mutex::new(None))
            .collect();
        std::thread::scope(|scope| {
            for _ in 0..workers.min(items.len()) {
                scope.spawn(|| {
                    loop {
                        let idx = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if idx >= items.len() {
                            break;
                        }
                        if let Some(parsed) = parse_one(&items[idx]) {
                            *slots[idx].lock().unwrap() = Some(parsed);
                        }
                    }
                });
            }
        });
        for (idx, slot) in slots.into_iter().enumerate() {
            results[idx] = slot.into_inner().unwrap();
        }
        results
    }

    let mut ordered_files: Vec<_> = inputs.files.iter().collect();
    ordered_files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut parse_jobs: Vec<(&str, &str, usize, bool)> = Vec::new();
    for (pair_idx, (path, base, head)) in ordered_files.iter().enumerate() {
        if let Some(src) = base {
            base_srcs.insert((*path).clone(), src.clone());
            parse_jobs.push((path.as_str(), src.as_str(), pair_idx, true));
        }
        if let Some(src) = head {
            head_srcs.insert((*path).clone(), src.clone());
            parse_jobs.push((path.as_str(), src.as_str(), pair_idx, false));
        }
    }
    let parsed_changed = parse_parallel(&parse_jobs, worker_count, |(path, src, _, _)| {
        FileData::parse(path, src, inputs.include_tests)
    });
    file_pairs.resize(ordered_files.len(), (None, None));
    for ((_, _, pair_idx, is_base), parsed) in parse_jobs.into_iter().zip(parsed_changed) {
        let Some(file) = parsed else { continue };
        if is_base {
            let i = base_files.len();
            base_files.push(file);
            file_pairs[pair_idx].0 = Some(i);
        } else {
            let i = head_files.len();
            head_files.push(file);
            file_pairs[pair_idx].1 = Some(i);
        }
    }
    let mut ordered_extras: Vec<_> = inputs.extra_head.iter().collect();
    ordered_extras.sort_by(|a, b| a.0.cmp(&b.0));
    let parsed_extras = parse_parallel(&ordered_extras, worker_count, |(path, src)| {
        FileData::parse(path, src, inputs.include_tests)
    });
    for mut file in parsed_extras.into_iter().flatten() {
        // Extra definitions participate in uniqueness, but have no nodes.
        file.keys.fill(None);
        extras.push(file);
    }

    let mut origins: Vec<Vec<Option<DefId>>> =
        head_files.iter().map(|f| vec![None; f.fns.len()]).collect();
    let mut targets: Vec<Vec<Option<DefId>>> =
        base_files.iter().map(|f| vec![None; f.fns.len()]).collect();
    let mut moved: Vec<Vec<bool>> = head_files
        .iter()
        .map(|f| vec![false; f.fns.len()])
        .collect();
    for (base, head) in file_pairs {
        if let (Some(b), Some(h)) = (base, head) {
            for (bi, hi) in pair_functions(&base_files[b], &head_files[h]) {
                origins[h][hi] = Some(DefId {
                    file: b,
                    function: bi,
                });
                targets[b][bi] = Some(DefId {
                    file: h,
                    function: hi,
                });
            }
        }
    }

    // Each removed definition appears in two ordered queues. Lazy consumption
    // makes same-file preference deterministic without quadratic rescanning.
    {
        let mut removed = Vec::new();
        for (file, data) in base_files.iter().enumerate() {
            for (function, key) in data.keys.iter().enumerate() {
                if let Some(key) = key
                    && targets[file][function].is_none()
                {
                    removed.push((key.as_str(), DefId { file, function }));
                }
            }
        }
        removed.sort_by(|a, b| a.0.cmp(b.0));
        let mut by_hash: HashMap<(Lang, bool, u64), VecDeque<DefId>> = HashMap::new();
        let mut by_file: HashMap<(Lang, bool, u64, &str), VecDeque<DefId>> = HashMap::new();
        let mut by_owner: HashMap<OwnerFingerprint<'_>, VecDeque<DefId>> = HashMap::new();
        for (_, id) in removed {
            let data = &base_files[id.file];
            let hash = data.fns[id.function].rename_hash;
            let is_type = data.fns[id.function].kind == SymbolKind::Type;
            by_owner
                .entry((
                    data.family,
                    is_type,
                    hash,
                    &data.path,
                    data.fns[id.function].owner.as_deref(),
                ))
                .or_default()
                .push_back(id);
            by_hash
                .entry((data.family, is_type, hash))
                .or_default()
                .push_back(id);
            by_file
                .entry((data.family, is_type, hash, &data.path))
                .or_default()
                .push_back(id);
        }
        let mut added = Vec::new();
        let mut added_hashes: HashMap<(Lang, bool, u64), usize> = HashMap::new();
        let mut added_files: HashMap<(Lang, bool, u64, &str), usize> = HashMap::new();
        let mut added_owners: HashMap<OwnerFingerprint<'_>, usize> = HashMap::new();
        for (file, data) in head_files.iter().enumerate() {
            for (function, key) in data.keys.iter().enumerate() {
                if let Some(key) = key
                    && origins[file][function].is_none()
                {
                    added.push((key.as_str(), DefId { file, function }));
                    let f = &data.fns[function];
                    *added_hashes
                        .entry((data.family, f.kind == SymbolKind::Type, f.rename_hash))
                        .or_default() += 1;
                    *added_files
                        .entry((
                            data.family,
                            f.kind == SymbolKind::Type,
                            f.rename_hash,
                            &data.path,
                        ))
                        .or_default() += 1;
                    *added_owners
                        .entry((
                            data.family,
                            f.kind == SymbolKind::Type,
                            f.rename_hash,
                            &data.path,
                            f.owner.as_deref(),
                        ))
                        .or_default() += 1;
                }
            }
        }
        added.sort_by(|a, b| a.0.cmp(b.0));
        for (_, head) in added {
            let data = &head_files[head.file];
            let hash = data.fns[head.function].rename_hash;
            let is_type = data.fns[head.function].kind == SymbolKind::Type;
            // Header/declaration hashes carry little evidence. Both revisions
            // must have a unique candidate in the selected scope.
            let sensitive = data.fns[head.function].kind != SymbolKind::Function;
            let owner_key = (
                data.family,
                is_type,
                hash,
                data.path.as_str(),
                data.fns[head.function].owner.as_deref(),
            );
            let file_key = (data.family, is_type, hash, data.path.as_str());
            let hash_key = (data.family, is_type, hash);
            let owner_unique = !sensitive
                || (added_owners[&owner_key] == 1
                    && by_owner.get(&owner_key).is_some_and(|ids| ids.len() == 1));
            let file_unique = !sensitive
                || (is_type
                    && added_files[&file_key] == 1
                    && by_file.get(&file_key).is_some_and(|ids| ids.len() == 1));
            let global_unique = !sensitive
                || (added_hashes[&hash_key] == 1
                    && by_hash.get(&hash_key).is_some_and(|ids| ids.len() == 1));
            let local = owner_unique
                .then(|| {
                    by_owner
                        .get_mut(&owner_key)
                        .and_then(|queue| take_unconsumed(queue, &targets))
                })
                .flatten()
                .or_else(|| {
                    file_unique
                        .then(|| {
                            by_file
                                .get_mut(&file_key)
                                .and_then(|queue| take_unconsumed(queue, &targets))
                        })
                        .flatten()
                });
            let origin = local.or_else(|| {
                global_unique
                    .then(|| {
                        by_hash
                            .get_mut(&hash_key)
                            .and_then(|queue| take_unconsumed(queue, &targets))
                    })
                    .flatten()
            });
            if let Some(base) = origin {
                origins[head.file][head.function] = Some(base);
                targets[base.file][base.function] = Some(head);
                moved[head.file][head.function] = true;
            }
        }
    }

    pair_container_renames(
        &base_files,
        &head_files,
        &mut origins,
        &mut targets,
        &mut moved,
    );
    pair_edited_renames(
        &base_files,
        &head_files,
        &mut origins,
        &mut targets,
        &mut moved,
    );

    let mut nodes = HashMap::new();
    let mut label_paths = HashSet::new();
    for (file, data) in head_files.iter().enumerate() {
        for (function, key) in data.keys.iter().enumerate() {
            let Some(key) = key else { continue };
            let f = &data.fns[function];
            let origin = origins[file][function];
            let status = match origin {
                Some(b)
                    if moved[file][function]
                        && base_files[b.file].fns[b.function].rename_hash == f.rename_hash =>
                {
                    Status::Moved
                }
                Some(b) if base_files[b.file].fns[b.function].body_hash == f.body_hash => {
                    Status::Unchanged
                }
                Some(_) => Status::BodyChanged,
                None => Status::Added,
            };
            let base = origin.map(|b| base_files[b.file].span(b.function));
            if let Some(span) = &base {
                label_paths.insert(span.file.clone());
            }
            label_paths.insert(data.path.clone());
            nodes.insert(
                key.clone(),
                Node {
                    key: key.clone(),
                    file: data.path.clone(),
                    name: if f.kind == SymbolKind::File {
                        data.path.clone()
                    } else {
                        data.display[function].as_ref().unwrap().clone()
                    },
                    kind: f.kind,
                    status,
                    head: Some(data.span(function)),
                    base,
                    moved_from: origin.filter(|_| moved[file][function]).map(|b| {
                        base_files[b.file].display[b.function]
                            .as_ref()
                            .unwrap()
                            .clone()
                    }),
                    callers: Vec::new(),
                    external_callers: Vec::new(),
                    callees: Vec::new(),
                    lost_callees: Vec::new(),
                    implementations: Vec::new(),
                    contracts: Vec::new(),
                    consumers: Vec::new(),
                    external_consumers: Vec::new(),
                    relevant: status.is_change(),
                },
            );
        }
    }
    for (file, data) in base_files.iter_mut().enumerate() {
        for (function, target) in targets[file].iter().enumerate() {
            if data.display[function].is_none() {
                continue;
            }
            if let Some(head) = target {
                data.keys[function] = head_files[head.file].keys[head.function].clone();
                continue;
            }
            let original = data.display[function].as_ref().unwrap();
            let mut name = original.clone();
            let mut key = data.keys[function].as_ref().unwrap().clone();
            let mut ordinal = 0;
            while nodes.contains_key(&key) {
                ordinal += 1;
                name = format!("{original}#{ordinal}");
                key = node_key(&data.path, &name);
            }
            data.keys[function] = Some(key.clone());
            label_paths.insert(data.path.clone());
            nodes.insert(
                key.clone(),
                Node {
                    key,
                    file: data.path.clone(),
                    name: if data.fns[function].kind == SymbolKind::File {
                        data.path.clone()
                    } else {
                        name
                    },
                    kind: data.fns[function].kind,
                    status: Status::Removed,
                    head: None,
                    base: Some(data.span(function)),
                    moved_from: None,
                    callers: Vec::new(),
                    external_callers: Vec::new(),
                    callees: Vec::new(),
                    lost_callees: Vec::new(),
                    implementations: Vec::new(),
                    contracts: Vec::new(),
                    consumers: Vec::new(),
                    external_consumers: Vec::new(),
                    relevant: true,
                },
            );
        }
    }

    // Keep ordinary unchanged callers external, but include unchanged symbols
    // that are explicit contract/implementation context for the review.
    let head_contracts = contract_graph(head_files.iter().chain(extras.iter()));
    let base_contracts = contract_graph(base_files.iter().chain(extras.iter()));
    let mut context = HashSet::new();
    for (files, graph) in [
        (&head_files, &head_contracts),
        (&base_files, &base_contracts),
    ] {
        let count = files.len();
        // Walk UP (`contracts`) from changed-file symbols to pull in unchanged
        // interfaces/base classes, and walk DOWN (`implementations`) ONLY from
        // symbols that actually changed so unchanged base classes (like a shared
        // TestBase) do not pull in hundreds of unrelated derived types.
        let mut up_seen = HashSet::new();
        let mut up_pending = Vec::new();
        let mut down_seen = HashSet::new();
        let mut down_pending = Vec::new();
        for (file, data) in files.iter().enumerate() {
            for i in 0..data.fns.len() {
                if data.display[i].is_none() {
                    continue;
                }
                // In head_files, data.keys[i] is already in `nodes`.
                // In base_files, paired symbols point to their head key!
                let changed = data
                    .keys
                    .get(i)
                    .and_then(Option::as_ref)
                    .and_then(|k| nodes.get(k))
                    .is_some_and(|n| n.status.is_change());
                if changed {
                    up_pending.push((file, i));
                    down_pending.push((file, i));
                }
            }
        }
        while let Some(id) = down_pending.pop() {
            if !down_seen.insert(id) {
                continue;
            }
            if id.0 >= count {
                context.insert((id.0 - count, id.1));
                up_pending.push(id);
            }
            let src_sym = if id.0 < count {
                &files[id.0].fns[id.1]
            } else {
                &extras[id.0 - count].fns[id.1]
            };
            for &child in graph.implementations.get(&id).into_iter().flatten() {
                let child_sym = if child.0 < count {
                    &files[child.0].fns[child.1]
                } else {
                    &extras[child.0 - count].fns[child.1]
                };
                // Only traverse downward to derived types (`Type -> Type`) or
                // method overrides (`Method -> Method`), never from a changed
                // Type header into its own unchanged member declarations.
                if src_sym.kind != SymbolKind::Type || child_sym.kind == SymbolKind::Type {
                    down_pending.push(child);
                }
            }
        }
        while let Some(id) = up_pending.pop() {
            if !up_seen.insert(id) {
                continue;
            }
            if id.0 >= count {
                context.insert((id.0 - count, id.1));
            }
            up_pending.extend(graph.contracts.get(&id).into_iter().flatten().copied());
        }
    }
    for (file, function) in context {
        let data = &mut extras[file];
        let Some(name) = data.display[function].as_ref() else {
            continue;
        };
        let key = node_key(&data.path, name);
        data.keys[function] = Some(key.clone());
        let span = data.span(function);
        nodes.insert(
            key.clone(),
            Node {
                key,
                file: data.path.clone(),
                name: name.clone(),
                kind: data.fns[function].kind,
                status: Status::Unchanged,
                head: Some(span.clone()),
                base: Some(span),
                moved_from: None,
                callers: Vec::new(),
                external_callers: Vec::new(),
                callees: Vec::new(),
                lost_callees: Vec::new(),
                implementations: Vec::new(),
                contracts: Vec::new(),
                consumers: Vec::new(),
                external_consumers: Vec::new(),
                relevant: false,
            },
        );
        label_paths.insert(data.path.clone());
    }
    for (path, src) in &inputs.extra_head {
        if label_paths.contains(path) {
            head_srcs.insert(path.clone(), src.clone());
            base_srcs.insert(path.clone(), src.clone());
        }
    }
    attach_contracts(
        &head_contracts,
        &head_files.iter().chain(extras.iter()).collect::<Vec<_>>(),
        &mut nodes,
    );
    attach_contracts(
        &base_contracts,
        &base_files.iter().chain(extras.iter()).collect::<Vec<_>>(),
        &mut nodes,
    );

    let mut removed_names = RemovedDefinitions::default();
    for (file, data) in base_files.iter().enumerate() {
        for (i, key) in data.keys.iter().enumerate() {
            if targets[file][i].is_none()
                && data.fns[i].kind != SymbolKind::Type
                && let Some(key) = key
            {
                removed_names
                    .names
                    .entry((data.family, &data.fns[i].name))
                    .or_default()
                    .push(key);
                if let Some(owner) = data.fns[i].owner.as_deref() {
                    removed_names
                        .members
                        .entry((data.family, owner, data.fns[i].name.as_str()))
                        .or_default()
                        .push(key);
                    let simple = owner.rsplit("::").next().unwrap();
                    if simple != owner {
                        removed_names
                            .members
                            .entry((data.family, simple, data.fns[i].name.as_str()))
                            .or_default()
                            .push(key);
                    }
                }
            }
        }
    }
    let head_universe = Universe::new(
        head_files.iter().chain(extras.iter()).collect(),
        head_contracts,
    );
    let base_universe = Universe::new(
        base_files.iter().chain(extras.iter()).collect(),
        base_contracts,
    );
    let mut incoming_consumers: HashMap<String, Vec<String>> = HashMap::new();
    for (file, data) in head_files.iter().enumerate() {
        for (function, key) in data.keys.iter().enumerate() {
            if let Some(key) = key {
                let (callees, value_refs, lost, _ext_val_refs) =
                    head_universe.resolve_calls(file, function, Some(&removed_names));
                for vref in value_refs {
                    if &vref != key && nodes.contains_key(&vref) {
                        incoming_consumers
                            .entry(vref)
                            .or_default()
                            .push(key.clone());
                    }
                }
                let node = nodes.get_mut(key).unwrap();
                node.callees = callees;
                node.lost_callees = lost;
            }
        }
    }
    for (file, data) in base_files.iter().enumerate() {
        for (function, key) in data.keys.iter().enumerate() {
            if targets[file][function].is_none()
                && let Some(key) = key
            {
                let (callees, value_refs, lost, _ext_val_refs) =
                    base_universe.resolve_calls(file, function, None);
                for vref in value_refs {
                    if &vref != key && nodes.contains_key(&vref) {
                        incoming_consumers
                            .entry(vref)
                            .or_default()
                            .push(key.clone());
                    }
                }
                let node = nodes.get_mut(key).unwrap();
                node.callees = callees;
                node.lost_callees = lost;
            }
        }
    }
    for (file, data) in extras.iter().enumerate() {
        for (function, display) in data.display.iter().enumerate() {
            if display.is_none() {
                continue;
            }
            let (callees, value_refs, lost, _ext_val_refs) = head_universe.resolve_calls(
                head_files.len() + file,
                function,
                Some(&removed_names),
            );
            if let Some(key) = &data.keys[function] {
                for vref in value_refs {
                    if &vref != key && nodes.contains_key(&vref) {
                        incoming_consumers
                            .entry(vref)
                            .or_default()
                            .push(key.clone());
                    }
                }
                let node = nodes.get_mut(key).unwrap();
                node.callees = callees;
                node.lost_callees = lost;
            } else {
                if !callees.is_empty() {
                    label_paths.insert(data.path.clone());
                    let caller = node_key(&data.path, data.display[function].as_ref().unwrap());
                    for callee in callees {
                        nodes
                            .get_mut(&callee)
                            .unwrap()
                            .external_callers
                            .push(caller.clone());
                    }
                }
                if !value_refs.is_empty() {
                    label_paths.insert(data.path.clone());
                    let caller = node_key(&data.path, data.display[function].as_ref().unwrap());
                    for vref in value_refs {
                        if let Some(target) = nodes.get_mut(&vref) {
                            target.external_consumers.push(caller.clone());
                        }
                    }
                }
            }
        }
    }

    // Group script-file top-level symbols under their `<file>` script root.
    for data in head_files.iter().chain(base_files.iter()) {
        if !data.is_script {
            continue;
        }
        let script_key = node_key(&data.path, "<file>");
        if !nodes.contains_key(&script_key) {
            continue;
        }
        let mut script_children = Vec::new();
        for (i, f) in data.fns.iter().enumerate() {
            if f.kind != SymbolKind::File
                && f.owner.is_none()
                && let Some(k) = &data.keys[i]
                && k != &script_key
                && nodes.contains_key(k)
            {
                script_children.push(k.clone());
            }
        }
        if let Some(script_node) = nodes.get_mut(&script_key) {
            script_node.callees.extend(script_children);
            sort_dedup(&mut script_node.callees);
        }
    }

    let mut incoming: HashMap<String, Vec<String>> = HashMap::new();
    for (key, node) in &nodes {
        for callee in &node.callees {
            incoming
                .entry(callee.clone())
                .or_default()
                .push(key.clone());
        }
    }
    for (key, node) in &mut nodes {
        node.callers = incoming.remove(key).unwrap_or_default();
        node.consumers = incoming_consumers.remove(key).unwrap_or_default();
        sort_dedup(&mut node.callers);
        sort_dedup(&mut node.external_callers);
        sort_dedup(&mut node.consumers);
        sort_dedup(&mut node.external_consumers);
    }

    for (path, base, head) in &inputs.files {
        let file_key = node_key(path, "<file>");
        if nodes.contains_key(&file_key) {
            continue;
        }
        if !inputs.include_tests && syntax::file_is_test_path(path) {
            continue;
        }
        let file_nodes: Vec<_> = nodes
            .values()
            .filter(|node| {
                node.file == *path || node.base.as_ref().is_some_and(|span| span.file == *path)
            })
            .collect();
        let mut scoped_base: Option<(usize, usize)> = None;
        let mut scoped_head: Option<(usize, usize)> = None;
        if !file_nodes.is_empty() {
            let has_changed_symbol = file_nodes.iter().any(|n| n.status.is_change());
            let base_str = base.as_deref().unwrap_or_default();
            let head_str = head.as_deref().unwrap_or_default();
            let base_lines: Vec<&str> = base_str.lines().collect();
            let head_lines: Vec<&str> = head_str.lines().collect();
            let mut substantive_uncovered = false;
            let mut any_uncovered = false;
            for change in similar::TextDiff::from_lines(base_str, head_str).iter_all_changes() {
                let (line, head_side) = match change.tag() {
                    similar::ChangeTag::Equal => continue,
                    similar::ChangeTag::Delete => (change.old_index(), false),
                    similar::ChangeTag::Insert => (change.new_index(), true),
                };
                let Some(line) = line else { continue };
                let covered = file_nodes.iter().any(|node| {
                    let span = if head_side { &node.head } else { &node.base };
                    span.as_ref().is_some_and(|span| {
                        span.file == *path && (span.start_row..span.end_row).contains(&line)
                    })
                });
                if !covered {
                    any_uncovered = true;
                    let text = if head_side {
                        head_lines.get(line).copied().unwrap_or_default()
                    } else {
                        base_lines.get(line).copied().unwrap_or_default()
                    };
                    if !syntax::is_ignorable_outside_line(path, text) {
                        substantive_uncovered = true;
                    }
                    let slot = if head_side {
                        &mut scoped_head
                    } else {
                        &mut scoped_base
                    };
                    *slot = Some(match *slot {
                        Some((s, e)) => (s.min(line), e.max(line + 1)),
                        None => (line, line + 1),
                    });
                }
            }
            if !any_uncovered || (has_changed_symbol && !substantive_uncovered) {
                continue;
            }
        }
        label_paths.insert(path.clone());
        let mut node = file_node(path, base.as_ref(), head.as_ref());
        if !file_nodes.is_empty() && file_nodes.iter().any(|n| n.status.is_change()) {
            if let (Some(src), Some((sr, er))) = (base.as_ref(), scoped_base) {
                node.base = Some(scoped_line_span(path, src, sr, er));
            }
            if let (Some(src), Some((sr, er))) = (head.as_ref(), scoped_head) {
                node.head = Some(scoped_line_span(path, src, sr, er));
            }
        }
        nodes.insert(node.key.clone(), node);
    }
    let entry_points = root_components(&nodes);
    mark_relevant(&mut nodes);
    let mut summary = (0, 0, 0, 0);
    for node in nodes.values() {
        match node.status {
            Status::Added => summary.0 += 1,
            Status::Removed => summary.1 += 1,
            Status::BodyChanged => summary.2 += 1,
            Status::Moved => summary.3 += 1,
            Status::Unchanged => {}
        }
    }
    Analysis {
        nodes,
        entry_points,
        summary,
        head_srcs,
        base_srcs,
        file_labels: file_labels(label_paths),
    }
}

fn symbol_owner_prefix(name: &str) -> Option<&str> {
    let base = name.split('(').next().unwrap_or(name);
    base.rsplit_once("::").map(|(owner, _)| owner)
}

fn scoped_line_span(path: &str, source: &str, start_row: usize, end_row: usize) -> Span {
    let mut offsets = vec![0usize];
    for (idx, b) in source.bytes().enumerate() {
        if b == b'\n' {
            offsets.push(idx + 1);
        }
    }
    let total_lines = source.lines().count();
    let s_row = start_row.saturating_sub(2);
    let e_row = (end_row + 2).min(total_lines);
    let start_byte = offsets.get(s_row).copied().unwrap_or(0);
    let end_byte = offsets.get(e_row).copied().unwrap_or(source.len());
    Span {
        file: path.to_string(),
        start_row: s_row,
        end_row: e_row,
        start_byte,
        end_byte,
    }
}

fn contract_graph<'a>(files: impl Iterator<Item = &'a FileData>) -> ContractGraph {
    let files: Vec<_> = files
        .map(|data| ContractFile {
            family: data.family,
            symbols: &data.fns,
        })
        .collect();
    ContractGraph::new(&files)
}

/// Dependency edges from either revision preserve impact paths when a signature
/// or base-list edit breaks a previously valid implementation relationship.
fn attach_contracts(graph: &ContractGraph, files: &[&FileData], nodes: &mut HashMap<String, Node>) {
    for (&(file, function), children) in &graph.implementations {
        let Some(parent) = &files[file].keys[function] else {
            continue;
        };
        for &(child_file, child_function) in children {
            let Some(child) = &files[child_file].keys[child_function] else {
                continue;
            };
            if !nodes.contains_key(parent) || !nodes.contains_key(child) {
                continue;
            }
            // Do not attach unchanged members under a Type node; otherwise
            // editing a class header or an interface floods its subtree with
            // every unchanged method/property/field on that type!
            let parent_sym = &files[file].fns[function];
            let child_sym = &files[child_file].fns[child_function];
            if parent_sym.kind == SymbolKind::Type
                && child_sym.kind != SymbolKind::Type
                && !nodes[child].status.is_change()
            {
                continue;
            }
            nodes
                .get_mut(parent)
                .unwrap()
                .implementations
                .push(child.clone());
            nodes.get_mut(child).unwrap().contracts.push(parent.clone());
        }
    }
    for node in nodes.values_mut() {
        sort_dedup(&mut node.implementations);
        sort_dedup(&mut node.contracts);
    }
}

/// Matched members provide identity evidence when a container was renamed and
/// its heritage edited together. Require one-to-one ownership, never a guess
/// based solely on a short `class`/`interface` header.
fn pair_container_renames(
    base: &[FileData],
    head: &[FileData],
    origins: &mut [Vec<Option<DefId>>],
    targets: &mut [Vec<Option<DefId>>],
    moved: &mut [Vec<bool>],
) {
    let mut old_types = HashMap::new();
    let mut new_types = HashMap::new();
    for (file, data) in base.iter().enumerate() {
        for (function, f) in data.fns.iter().enumerate() {
            if f.kind == SymbolKind::Type && targets[file][function].is_none() {
                old_types
                    .entry((data.family, data.path.as_str(), f.qual.as_str()))
                    .or_insert_with(Vec::new)
                    .push(DefId { file, function });
            }
        }
    }
    for (file, data) in head.iter().enumerate() {
        for (function, f) in data.fns.iter().enumerate() {
            if f.kind == SymbolKind::Type && origins[file][function].is_none() {
                new_types
                    .entry((data.family, data.path.as_str(), f.qual.as_str()))
                    .or_insert_with(Vec::new)
                    .push(DefId { file, function });
            }
        }
    }
    let mut forward: HashMap<DefId, HashSet<DefId>> = HashMap::new();
    let mut reverse: HashMap<DefId, HashSet<DefId>> = HashMap::new();
    for (file, data) in head.iter().enumerate() {
        for (function, origin) in origins[file].iter().enumerate() {
            let Some(b) = origin else { continue };
            let (Some(old_owner), Some(new_owner)) = (
                &base[b.file].fns[b.function].owner,
                &data.fns[function].owner,
            ) else {
                continue;
            };
            let Some(old) =
                old_types.get(&(data.family, base[b.file].path.as_str(), old_owner.as_str()))
            else {
                continue;
            };
            let Some(new) = new_types.get(&(data.family, data.path.as_str(), new_owner.as_str()))
            else {
                continue;
            };
            if let ([old], [new]) = (old.as_slice(), new.as_slice()) {
                forward.entry(*old).or_default().insert(*new);
                reverse.entry(*new).or_default().insert(*old);
            }
        }
    }
    for (b, candidates) in forward {
        if candidates.len() != 1 {
            continue;
        }
        let h = *candidates.iter().next().unwrap();
        if reverse[&h].len() == 1 {
            origins[h.file][h.function] = Some(b);
            targets[b.file][b.function] = Some(h);
            moved[h.file][h.function] = true;
        }
    }
}

/// A rename plus an edit needs evidence beyond a generic empty body. Prefer
/// corresponding callsite rewrites; otherwise require a mutual unique, strong
/// token match within the same file/container. Ambiguous candidates stay churn.
fn pair_edited_renames(
    base: &[FileData],
    head: &[FileData],
    origins: &mut [Vec<Option<DefId>>],
    targets: &mut [Vec<Option<DefId>>],
    moved: &mut [Vec<bool>],
) {
    let mut hints = HashSet::new();
    for (hf, data) in head.iter().enumerate() {
        for (hi, origin) in origins[hf].iter().enumerate() {
            let Some(b) = origin else { continue };
            let old = &base[b.file].calls[b.function];
            let new = &data.calls[hi];
            if old.len() == new.len() {
                for (a, z) in old.iter().zip(new) {
                    if a.name != z.name
                        && a.receiver == z.receiver
                        && a.dispatch == z.dispatch
                        && !new.iter().any(|c| c.name == a.name)
                        && !old.iter().any(|c| c.name == z.name)
                    {
                        hints.insert((data.family, a.name.as_str(), z.name.as_str()));
                    }
                }
            }
        }
    }
    let mut owner_moves = HashMap::new();
    for (bf, data) in base.iter().enumerate() {
        for (bi, target) in targets[bf].iter().enumerate() {
            if data.fns[bi].kind == SymbolKind::Type
                && let Some(h) = target
            {
                owner_moves.insert(
                    (data.family, data.fns[bi].qual.as_str()),
                    head[h.file].fns[h.function].qual.as_str(),
                );
            }
        }
    }
    let mut impl_moves = HashMap::new();
    for (file, data) in base
        .iter()
        .enumerate()
        .filter(|(_, data)| data.family == Lang::Rust)
    {
        for (function, symbol) in data.fns.iter().enumerate() {
            if symbol.kind == SymbolKind::Type
                && targets[file][function].is_none()
                && let Some(qual) = renamed_impl_qual(&symbol.qual, &owner_moves)
            {
                impl_moves.insert(DefId { file, function }, qual);
            }
        }
    }
    let mut best_base: HashMap<DefId, (DefId, usize, bool)> = HashMap::new();
    let mut best_head: HashMap<DefId, (DefId, usize, bool)> = HashMap::new();
    let mut candidates: HashMap<(Lang, Option<&str>, bool), Vec<DefId>> = HashMap::new();
    for (bf, data) in base.iter().enumerate() {
        for (bi, b) in data.fns.iter().enumerate() {
            if data.keys[bi].is_none() || targets[bf][bi].is_some() {
                continue;
            }
            let owner = b
                .owner
                .as_deref()
                .map(|o| owner_moves.get(&(data.family, o)).copied().unwrap_or(o));
            candidates
                .entry((data.family, owner, b.kind == SymbolKind::Type))
                .or_default()
                .push(DefId {
                    file: bf,
                    function: bi,
                });
        }
    }
    for (hf, data) in head.iter().enumerate() {
        for (hi, h) in data.fns.iter().enumerate() {
            if data.keys[hi].is_none() || origins[hf][hi].is_some() {
                continue;
            }
            let Some(group) =
                candidates.get(&(data.family, h.owner.as_deref(), h.kind == SymbolKind::Type))
            else {
                continue;
            };
            for &b in group {
                let old = &base[b.file].fns[b.function];
                let hinted = hints.contains(&(data.family, old.name.as_str(), h.name.as_str()));
                let impl_renamed = impl_moves.get(&b).is_some_and(|qual| qual == &h.qual);
                if base[b.file].path != data.path && !hinted && !impl_renamed {
                    continue;
                }
                if old.kind == SymbolKind::Function
                    && h.kind == SymbolKind::Function
                    && old.owner == h.owner
                    && (old.body_shape.is_none()
                        || old.body_shape != h.body_shape
                        || old.body_tokens.min(h.body_tokens) < 3)
                {
                    continue;
                }
                let score = if impl_renamed {
                    4000
                } else if old.name == h.name && old.owner != h.owner {
                    if old.sig == h.sig { 3500 } else { 3000 }
                } else if hinted && old.kind != SymbolKind::Type {
                    let minimum = if old.kind == SymbolKind::Declaration {
                        300
                    } else {
                        500
                    };
                    if token_similarity(&old.tokens, &h.tokens) < minimum {
                        continue;
                    }
                    2000
                } else {
                    if old.tokens.len().min(h.tokens.len()) < 8 {
                        continue;
                    }
                    let score = token_similarity(&old.tokens, &h.tokens);
                    if score < 850 {
                        continue;
                    }
                    score
                };
                let hid = DefId {
                    file: hf,
                    function: hi,
                };
                record_match(&mut best_base, b, hid, score);
                record_match(&mut best_head, hid, b, score);
            }
        }
    }
    for (b, (h, _, tied)) in best_base {
        if !tied
            && best_head
                .get(&h)
                .is_some_and(|(candidate, _, tied)| *candidate == b && !tied)
        {
            origins[h.file][h.function] = Some(b);
            targets[b.file][b.function] = Some(h);
            moved[h.file][h.function] = base[b.file].path != head[h.file].path
                || base[b.file].fns[b.function].qual != head[h.file].fns[h.function].qual;
        }
    }
}

fn renamed_impl_qual(qual: &str, renames: &HashMap<(Lang, &str), &str>) -> Option<String> {
    let open = qual.find('<')?;
    let prefix = &qual[..open];
    let inner = qual[open + 1..].strip_suffix('>')?;
    let rewrite = |reference: &str| {
        let generic = reference.find('<').unwrap_or(reference.len());
        let name = &reference[..generic];
        let scoped = format!("{prefix}{name}");
        let renamed = renames
            .get(&(Lang::Rust, scoped.as_str()))
            .or_else(|| renames.get(&(Lang::Rust, name)));
        match renamed {
            Some(new) => format!(
                "{}{}",
                new.strip_prefix(prefix).unwrap_or(new),
                &reference[generic..]
            ),
            None => reference.to_string(),
        }
    };
    let translated = match inner.split_once(" as ") {
        Some((ty, contract)) => format!("{prefix}<{} as {}>", rewrite(ty), rewrite(contract)),
        None => format!("{prefix}<{}>", rewrite(inner)),
    };
    (translated != qual).then_some(translated)
}

fn record_match(
    index: &mut HashMap<DefId, (DefId, usize, bool)>,
    key: DefId,
    other: DefId,
    score: usize,
) {
    let best = index.entry(key).or_insert((other, score, false));
    if score > best.1 {
        *best = (other, score, false);
    } else if score == best.1 && other != best.0 {
        best.2 = true;
    }
}

fn token_similarity(a: &[u64], b: &[u64]) -> usize {
    let (mut i, mut j, mut shared) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                shared += 1;
                i += 1;
                j += 1;
            }
        }
    }
    2000 * shared / (a.len() + b.len()).max(1)
}

fn take_unconsumed(queue: &mut VecDeque<DefId>, targets: &[Vec<Option<DefId>>]) -> Option<DefId> {
    while let Some(id) = queue.pop_front() {
        if targets[id.file][id.function].is_none() {
            return Some(id);
        }
    }
    None
}

#[derive(Default)]
struct RemovedDefinitions<'a> {
    names: HashMap<(Lang, &'a str), Vec<&'a str>>,
    members: HashMap<(Lang, &'a str, &'a str), Vec<&'a str>>,
}

struct Universe<'a> {
    files: Vec<&'a FileData>,
    by_name: HashMap<(Lang, &'a str), Vec<DefId>>,
    contracts: ContractGraph,
}

impl<'a> Universe<'a> {
    fn new(files: Vec<&'a FileData>, contracts: ContractGraph) -> Self {
        let mut by_name: HashMap<(Lang, &str), Vec<DefId>> = HashMap::new();
        for (file, data) in files.iter().enumerate() {
            for (function, display) in data.display.iter().enumerate() {
                if display.is_some() && data.fns[function].kind != SymbolKind::Type {
                    by_name
                        .entry((data.family, &data.fns[function].name))
                        .or_default()
                        .push(DefId { file, function });
                }
            }
        }
        Self {
            files,
            by_name,
            contracts,
        }
    }

    fn resolve_calls(
        &self,
        file: usize,
        function: usize,
        removed: Option<&RemovedDefinitions<'_>>,
    ) -> (Vec<String>, Vec<String>, Vec<String>, Vec<String>) {
        let caller = self.files[file];
        let mut callees = Vec::new();
        let mut value_refs = Vec::new();
        let mut lost = Vec::new();
        let mut external_value_refs = Vec::new();
        for call in caller.calls[function]
            .iter()
            .chain(caller.value_refs[function].iter())
        {
            let name = &call.name;
            let dest = if call.is_value_ref {
                &mut value_refs
            } else {
                &mut callees
            };
            if let Some(receiver) = &call.receiver {
                let targets =
                    self.contracts
                        .receiver_targets(file, function, receiver, name, call.dispatch);
                if targets.as_ref().is_none_or(Vec::is_empty)
                    && let Some([key]) = removed
                        .and_then(|index| {
                            index
                                .members
                                .get(&(caller.family, receiver.as_str(), name.as_str()))
                        })
                        .map(Vec::as_slice)
                {
                    dest.push((*key).to_string());
                    continue;
                }
                if let Some(targets) = targets {
                    let mut unresolved = targets.is_empty();
                    for (target_file, target_function) in targets {
                        if let Some(key) = &self.files[target_file].keys[target_function] {
                            dest.push(key.clone());
                        } else {
                            unresolved = true;
                        }
                    }
                    if unresolved && !call.is_value_ref {
                        lost.push(name.clone());
                    }
                    continue;
                }
                if !call.is_value_ref {
                    // A typed but unresolved receiver must not bind to an
                    // unrelated same-name method in the current file.
                    lost.push(name.clone());
                    continue;
                }
                // For an unqualified identifier inside a class body (`owner` set as
                // candidate receiver), if it is not a member of `owner`, fall through
                // to module/file-level value resolution!
                if receiver.contains("::") || receiver.contains('.') {
                    continue;
                }
            }
            let mut unresolved = false;
            if let Some(local) = caller.by_name.get(name) {
                // Local overload sets shadow every other file, including
                // when none of their definitions belongs to the review graph.
                for &target in local {
                    if call.is_value_ref && caller.fns[target].kind != SymbolKind::Value {
                        continue;
                    }
                    if let Some(key) = &caller.keys[target] {
                        dest.push(key.clone());
                    } else {
                        unresolved = true;
                    }
                }
            } else {
                match self
                    .by_name
                    .get(&(caller.family, name.as_str()))
                    .map(Vec::as_slice)
                    .unwrap_or(&[])
                {
                    [target] => {
                        if !call.is_value_ref
                            || self.files[target.file].fns[target.function].kind
                                == SymbolKind::Value
                        {
                            if let Some(key) = &self.files[target.file].keys[target.function] {
                                dest.push(key.clone());
                            } else if call.is_value_ref {
                                external_value_refs.push(name.clone());
                            } else {
                                unresolved = true;
                            }
                        }
                    }
                    [] => {
                        let fallback = removed
                            .and_then(|index| index.names.get(&(caller.family, name.as_str())));
                        match fallback.map(Vec::as_slice).unwrap_or(&[]) {
                            [key] => dest.push((*key).to_string()),
                            _ => unresolved = true,
                        }
                    }
                    _ => unresolved = true,
                }
            }
            if unresolved && !call.is_value_ref {
                lost.push(name.clone());
            }
        }
        let self_key = caller.keys.get(function).and_then(Option::as_ref);
        if let Some(self_key) = self_key {
            value_refs.retain(|k| k != self_key);
        }
        sort_dedup(&mut callees);
        sort_dedup(&mut value_refs);
        sort_dedup(&mut lost);
        sort_dedup(&mut external_value_refs);
        (callees, value_refs, lost, external_value_refs)
    }
}

fn sort_dedup(values: &mut Vec<String>) {
    values.sort();
    values.dedup();
}

/// Iterative Kosaraju: finish-order DFS followed by reverse-graph components.
/// External callers deliberately do not participate in the node graph.
fn root_components(nodes: &HashMap<String, Node>) -> Vec<String> {
    let mut keys: Vec<_> = nodes.keys().collect();
    keys.sort();
    let indices: HashMap<&str, usize> = keys
        .iter()
        .enumerate()
        .map(|(i, k)| (k.as_str(), i))
        .collect();
    let edges: Vec<Vec<usize>> = keys
        .iter()
        .map(|key| {
            nodes[*key]
                .callees
                .iter()
                .chain(nodes[*key].implementations.iter())
                .filter_map(|callee| indices.get(callee.as_str()).copied())
                .collect()
        })
        .collect();
    let reverse: Vec<Vec<usize>> = keys
        .iter()
        .map(|key| {
            nodes[*key]
                .callers
                .iter()
                .chain(nodes[*key].contracts.iter())
                .filter_map(|caller| indices.get(caller.as_str()).copied())
                .collect()
        })
        .collect();
    let mut seen = vec![false; keys.len()];
    let mut finished = Vec::with_capacity(keys.len());
    let mut stack = Vec::new();
    for start in 0..keys.len() {
        if seen[start] {
            continue;
        }
        seen[start] = true;
        stack.push((start, 0));
        while let Some((node, next)) = stack.last_mut() {
            if *next < edges[*node].len() {
                let child = edges[*node][*next];
                *next += 1;
                if !seen[child] {
                    seen[child] = true;
                    stack.push((child, 0));
                }
            } else {
                finished.push(*node);
                stack.pop();
            }
        }
    }
    let mut component = vec![usize::MAX; keys.len()];
    let mut representatives = Vec::new();
    let mut pending = Vec::new();
    for start in finished.into_iter().rev() {
        if component[start] != usize::MAX {
            continue;
        }
        let id = representatives.len();
        let mut best = start;
        component[start] = id;
        pending.push(start);
        while let Some(node) = pending.pop() {
            if (status_rank(nodes[keys[node]].status), keys[node])
                < (status_rank(nodes[keys[best]].status), keys[best])
            {
                best = node;
            }
            for &parent in &reverse[node] {
                if component[parent] == usize::MAX {
                    component[parent] = id;
                    pending.push(parent);
                }
            }
        }
        representatives.push(best);
    }
    let mut has_incoming = vec![false; representatives.len()];
    for (node, children) in edges.iter().enumerate() {
        for &child in children {
            if component[node] != component[child] {
                has_incoming[component[child]] = true;
            }
        }
    }
    let mut roots: Vec<String> = representatives
        .iter()
        .enumerate()
        .filter(|(id, _)| !has_incoming[*id])
        .map(|(_, &node)| keys[node].clone())
        .collect();
    // Ensure changed Value/Type source nodes that have downstream consumers
    // are also surfaced as roots when their only incoming edges are non-parent
    // or when they have no incoming call/contract edges.
    for (key, node) in nodes {
        if node.status.is_change()
            && matches!(node.kind, SymbolKind::Value | SymbolKind::Type)
            && !node.consumers.is_empty()
            && !roots.contains(key)
            && node
                .contracts
                .iter()
                .all(|c| !nodes.get(c).is_some_and(|cn| cn.status.is_change()))
            && node.callers.iter().all(|c| node.consumers.contains(c))
        {
            roots.push(key.clone());
        }
    }
    sort_keys(&mut roots, nodes);
    roots
}

fn mark_relevant(nodes: &mut HashMap<String, Node>) {
    let changed: Vec<String> = nodes
        .iter()
        .filter(|(_, n)| n.status.is_change())
        .map(|(k, _)| k.clone())
        .collect();
    // A changed contract/type affects its explicit implementations, including
    // unchanged inherited methods. Do this before propagating to callers so a
    // changed implementation does not spuriously affect unrelated siblings.
    let mut affected: HashSet<String> = changed.iter().cloned().collect();
    let mut pending = changed;
    while let Some(key) = pending.pop() {
        for child in nodes[&key]
            .implementations
            .iter()
            .chain(nodes[&key].consumers.iter())
        {
            if affected.insert(child.clone()) {
                pending.push(child.clone());
            }
        }
    }
    let mut queue: VecDeque<String> = affected.iter().cloned().collect();
    for key in &affected {
        nodes.get_mut(key).unwrap().relevant = true;
    }
    while let Some(key) = queue.pop_front() {
        let parents: Vec<_> = nodes[&key]
            .callers
            .iter()
            .cloned()
            .chain(
                nodes[&key]
                    .contracts
                    .iter()
                    .filter(|p| {
                        // Do not mark an unchanged class/interface header relevant
                        // merely because one of its members or derived types changed!
                        // Method-level overrides (`Method -> ContractMethod`) still propagate.
                        !nodes.get(*p).is_some_and(|pn| pn.kind == SymbolKind::Type)
                    })
                    .cloned(),
            )
            .collect();
        for parent in parents {
            let node = nodes.get_mut(&parent).unwrap();
            if !node.relevant {
                node.relevant = true;
                queue.push_back(parent);
            }
        }
    }
}

fn file_labels(paths: HashSet<String>) -> HashMap<String, String> {
    let mut suffix_counts: HashMap<String, usize> = HashMap::new();
    let parts: Vec<_> = paths
        .iter()
        .map(|path| {
            (
                path,
                path.split(['/', '\\'])
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    for (_, segments) in &parts {
        for start in 0..segments.len() {
            *suffix_counts
                .entry(segments[start..].join("/"))
                .or_default() += 1;
        }
    }
    let mut labels = HashMap::new();
    for (path, segments) in parts {
        let mut label = segments.join("/");
        for start in (0..segments.len()).rev() {
            let suffix = segments[start..].join("/");
            if suffix_counts[&suffix] == 1 || start == 0 {
                label = suffix;
                break;
            }
        }
        labels.insert(path.clone(), label);
    }
    labels
}

fn sort_keys(keys: &mut [String], nodes: &HashMap<String, Node>) {
    keys.sort_by(|a, b| {
        let ra = nodes.get(a).map(|n| status_rank(n.status)).unwrap_or(9);
        let rb = nodes.get(b).map(|n| status_rank(n.status)).unwrap_or(9);
        ra.cmp(&rb).then_with(|| a.cmp(b))
    });
}

fn status_rank(s: Status) -> u8 {
    match s {
        Status::BodyChanged => 0,
        Status::Added => 1,
        Status::Moved => 2,
        Status::Removed => 3,
        Status::Unchanged => 4,
    }
}

/// Bracket position when >=2 sibling callers share the exact same downstream callees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallerBracket {
    First,
    Middle,
    Last,
}

/// One visible row of the review tree.
#[derive(Debug, Clone)]
pub struct TreeRow {
    pub key: String,
    /// Unique tree-instance path of this row for stable cursor preservation.
    pub id: String,
    /// Path-scoped expansion key toggled in `App.expanded`.
    pub expand_key: String,
    pub depth: usize,
    pub has_children: bool,
    /// True if this row represents an entry-point root symbol.
    pub is_root: bool,
    /// Non-node namespace/module/class prefix group header.
    pub group_label: Option<String>,
    /// Optional shortened display label when nested under a group header or same-owner parent.
    pub display_name: Option<String>,
    /// True when this child row was reached via a `consumers` edge (`>`).
    pub is_consumer: bool,
    /// True when this symbol's subtree was already rendered earlier in the tree (`↗`).
    pub is_repeat: bool,
    /// Bracket connector when sibling callers share the exact same callees (`┌`/`├`/`└`).
    pub caller_bracket: Option<CallerBracket>,
    /// Short leaf names of sibling callers when >=2 siblings call this row (`← A, B`).
    pub shared_callers: Vec<String>,
    /// True when `shared_callers` are the coupled bracketed rows immediately above this row.
    pub shared_from_coupled: bool,
    /// Preview of shared callees shown on the final collapsed coupled caller (`→ + C`).
    pub collapsed_callee_hint: Option<String>,
}

/// DFS rows starting from entry points (or a single forced root).
/// `focus` hides unchanged subtrees that contain no change anywhere below.
pub fn build_rows(
    a: &Analysis,
    roots: &[String],
    expanded: &HashSet<String>,
    focus: bool,
) -> Vec<TreeRow> {
    build_rows_inner(a, roots, expanded, focus, false)
}

/// Grouped DFS rows for the interactive TUI: groups >=2 roots sharing a
/// common namespace/module/type container prefix under collapsible compact
/// trie headers while keeping singletons flat.
pub fn build_grouped_rows(
    a: &Analysis,
    roots: &[String],
    expanded: &HashSet<String>,
    focus: bool,
) -> Vec<TreeRow> {
    build_rows_inner(a, roots, expanded, focus, true)
}

pub fn group_key(prefix: &str) -> String {
    format!("@group::{prefix}")
}

fn build_rows_inner(
    a: &Analysis,
    roots: &[String],
    expanded: &HashSet<String>,
    focus: bool,
    grouped: bool,
) -> Vec<TreeRow> {
    let mut rows = Vec::new();
    let valid_roots: Vec<String> = roots
        .iter()
        .filter(|r| a.nodes.get(*r).is_some_and(|n| !focus || n.relevant))
        .cloned()
        .collect();

    let called_in_forest = compute_called_in_forest(a, &valid_roots, focus);
    let mut ctx = EmitCtx {
        a,
        expanded,
        focus,
        grouped,
        called_in_forest: &called_in_forest,
        visited_global: HashSet::new(),
        ancestors: Vec::new(),
        rows: &mut rows,
    };

    if !grouped || valid_roots.len() < 2 {
        let specs: Vec<SiblingSpec> = valid_roots
            .iter()
            .map(|r| SiblingSpec {
                key: r.clone(),
                is_consumer: false,
                is_root: true,
                shared_callers: Vec::new(),
                shared_from_coupled: false,
                absorbed_trie: None,
                absorbed_seg: None,
            })
            .collect();
        emit_siblings(&mut ctx, &specs, 0, "@path:", &[], None);
        return rows;
    }

    let trie = build_prefix_trie(a, &valid_roots);
    emit_trie(&mut ctx, &trie, 0, "", "@path:");
    rows
}

struct EmitCtx<'a> {
    a: &'a Analysis,
    expanded: &'a HashSet<String>,
    focus: bool,
    grouped: bool,
    called_in_forest: &'a HashSet<String>,
    visited_global: HashSet<String>,
    ancestors: Vec<String>,
    rows: &'a mut Vec<TreeRow>,
}

#[derive(Clone)]
struct SiblingSpec {
    key: String,
    is_consumer: bool,
    is_root: bool,
    shared_callers: Vec<String>,
    shared_from_coupled: bool,
    absorbed_trie: Option<PrefixTrie>,
    absorbed_seg: Option<String>,
}

/// Collect symbols reached via actual call or method/type override edges in the
/// active forest so `Type -> non-Type member` containment edges never duplicate
/// methods that are already shown under their callers.
fn compute_called_in_forest(a: &Analysis, valid_roots: &[String], focus: bool) -> HashSet<String> {
    let mut visited: HashSet<String> = HashSet::new();
    let mut called: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = valid_roots.iter().cloned().collect();
    while let Some(key) = queue.pop_front() {
        if !visited.insert(key.clone()) {
            continue;
        }
        let Some(n) = a.nodes.get(&key) else { continue };
        if focus && !n.relevant {
            continue;
        }
        for callee in &n.callees {
            if callee != &key
                && let Some(cn) = a.nodes.get(callee)
                && (!focus || cn.relevant)
            {
                called.insert(callee.clone());
                queue.push_back(callee.clone());
            }
        }
        for imp in &n.implementations {
            if imp != &key
                && let Some(cn) = a.nodes.get(imp)
                && (!focus || cn.relevant)
            {
                if n.kind != SymbolKind::Type || cn.kind == SymbolKind::Type {
                    called.insert(imp.clone());
                }
                queue.push_back(imp.clone());
            }
        }
        for con in &n.consumers {
            if con != &key
                && let Some(cn) = a.nodes.get(con)
                && (!focus || cn.relevant)
            {
                queue.push_back(con.clone());
            }
        }
    }
    called
}

#[derive(Clone, Default)]
struct PrefixTrie {
    roots_here: Vec<String>,
    children: Vec<(String, PrefixTrie)>,
}

impl PrefixTrie {
    fn total_roots(&self) -> usize {
        self.roots_here.len()
            + self
                .children
                .iter()
                .map(|(_, child)| child.total_roots())
                .sum::<usize>()
    }

    fn collect_all_roots(&self, out: &mut Vec<String>) {
        out.extend(self.roots_here.iter().cloned());
        for (_, child) in &self.children {
            child.collect_all_roots(out);
        }
    }

    fn best_rank(&self, a: &Analysis) -> u8 {
        let own = self
            .roots_here
            .iter()
            .filter_map(|k| a.nodes.get(k).map(|n| status_rank(n.status)))
            .min()
            .unwrap_or(9);
        let sub = self
            .children
            .iter()
            .map(|(_, c)| c.best_rank(a))
            .min()
            .unwrap_or(9);
        own.min(sub)
    }
}

/// Extract container segments before the leaf symbol name so roots sharing
/// a namespace, module, or class can be grouped cleanly.
fn container_segments(node: &Node) -> Vec<String> {
    if node.kind == SymbolKind::File {
        let mut parts: Vec<String> = node
            .name
            .split('/')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        if parts.len() > 1 {
            parts.pop();
            return parts;
        }
        return Vec::new();
    }
    let base = node.name.split('(').next().unwrap_or(&node.name);
    let Some((container, _leaf)) = base.rsplit_once("::") else {
        return Vec::new();
    };
    // Split both `.` (C# / Python / Java-style namespaces) and `::` into segments,
    // preserving `<Type as Trait>` chunks intact.
    let mut segs = Vec::new();
    let mut current = String::new();
    let mut angle_depth = 0usize;
    let mut chars = container.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '<' => {
                angle_depth += 1;
                current.push(ch);
            }
            '>' => {
                angle_depth = angle_depth.saturating_sub(1);
                current.push(ch);
            }
            ':' if angle_depth == 0 && chars.peek() == Some(&':') => {
                chars.next();
                if !current.is_empty() {
                    segs.push(std::mem::take(&mut current));
                }
            }
            '.' if angle_depth == 0 => {
                if !current.is_empty() {
                    segs.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        segs.push(current);
    }
    segs
}

fn strip_container_prefix(node: &Node, prefix_segs: &[String]) -> Option<String> {
    if prefix_segs.is_empty() {
        return None;
    }
    if node.kind == SymbolKind::File {
        let prefix = format!("{}/", prefix_segs.join("/"));
        return node.name.strip_prefix(&prefix).map(str::to_string);
    }
    // Walk `node.name` matching `prefix_segs` across `.` or `::` separators.
    let mut rest = node.name.as_str();
    for seg in prefix_segs {
        let after = rest.strip_prefix(seg.as_str())?;
        if let Some(s) = after.strip_prefix("::") {
            rest = s;
        } else {
            rest = after.strip_prefix('.')?;
        }
    }
    (!rest.is_empty()).then(|| rest.to_string())
}

fn short_leaf_name(node: &Node) -> String {
    if node.kind == SymbolKind::File {
        return node
            .name
            .rsplit('/')
            .next()
            .unwrap_or(&node.name)
            .to_string();
    }
    let base = node.name.split('(').next().unwrap_or(&node.name);
    let params = &node.name[base.len()..];
    let leaf = base.rsplit("::").next().unwrap_or(base);
    format!("{leaf}{params}")
}

fn node_owner_for_children(node: &Node) -> Option<String> {
    if node.kind == SymbolKind::File {
        return None;
    }
    if node.kind == SymbolKind::Type {
        // Handle Rust inherent impl header `ns::<Worker>` -> `ns::Worker`
        if let Some((prefix, last)) = node.name.rsplit_once("::") {
            if let Some(inner) = last.strip_prefix('<').and_then(|s| s.strip_suffix('>'))
                && !inner.contains(' ')
            {
                return Some(format!("{prefix}::{inner}"));
            }
        } else if let Some(inner) = node
            .name
            .strip_prefix('<')
            .and_then(|s| s.strip_suffix('>'))
            && !inner.contains(' ')
        {
            return Some(inner.to_string());
        }
        return Some(node.name.clone());
    }
    symbol_owner_prefix(&node.name).map(str::to_string)
}

fn shortened_display_name(
    node: &Node,
    grouped: bool,
    prefix_segs: &[String],
    parent_owner: Option<&str>,
) -> Option<String> {
    if !grouped || prefix_segs.is_empty() {
        return None;
    }
    if let Some(owner) = parent_owner
        && node.kind != SymbolKind::File
        && symbol_owner_prefix(&node.name) == Some(owner)
    {
        let owner_prefix = format!("{owner}::");
        if let Some(short) = node.name.strip_prefix(&owner_prefix)
            && !short.is_empty()
        {
            return Some(short.to_string());
        }
    }
    for len in (1..=prefix_segs.len()).rev() {
        if let Some(short) = strip_container_prefix(node, &prefix_segs[..len]) {
            return Some(short);
        }
    }
    None
}

fn build_prefix_trie(a: &Analysis, roots: &[String]) -> PrefixTrie {
    let mut root_trie = PrefixTrie::default();
    for key in roots {
        let Some(node) = a.nodes.get(key) else {
            continue;
        };
        let segs = container_segments(node);
        let mut cur = &mut root_trie;
        for seg in segs {
            let pos = cur.children.iter().position(|(s, _)| s == &seg);
            let idx = match pos {
                Some(i) => i,
                None => {
                    cur.children.push((seg, PrefixTrie::default()));
                    cur.children.len() - 1
                }
            };
            cur = &mut cur.children[idx].1;
        }
        cur.roots_here.push(key.clone());
    }
    sort_trie(a, &mut root_trie);
    root_trie
}

fn sort_trie(a: &Analysis, trie: &mut PrefixTrie) {
    sort_keys(&mut trie.roots_here, &a.nodes);
    for (_, child) in &mut trie.children {
        sort_trie(a, child);
    }
    trie.children.sort_by(|(sa, ca), (sb, cb)| {
        ca.best_rank(a)
            .cmp(&cb.best_rank(a))
            .then_with(|| sa.cmp(sb))
    });
}

fn type_root_matches_segment(node: &Node, prefix_segs: &[String], seg: &str) -> bool {
    if node.kind != SymbolKind::Type {
        return false;
    }
    let rel = strip_container_prefix(node, prefix_segs).unwrap_or_else(|| node.name.clone());
    if rel == seg {
        return true;
    }
    rel.strip_prefix('<')
        .and_then(|s| s.strip_suffix('>'))
        .is_some_and(|inner| inner == seg)
}

fn emit_trie(
    ctx: &mut EmitCtx<'_>,
    trie: &PrefixTrie,
    depth: usize,
    matched_prefix: &str,
    parent_path: &str,
) {
    let prefix_segs: Vec<String> = if matched_prefix.is_empty() {
        Vec::new()
    } else {
        matched_prefix.split("::").map(str::to_string).collect()
    };

    // Allow a changed `Type` root at this level (e.g. `~ RootCauseHypothesisMaterializer`)
    // to absorb its matching container sub-trie (`RootCauseHypothesisMaterializer::MaterializeAsync`)
    // so the class header and its entry methods don't render as separate competing roots/groups.
    let mut absorbed_by_root: HashMap<usize, (String, PrefixTrie)> = HashMap::new();
    let mut claimed_children: HashSet<usize> = HashSet::new();
    for (ci, (seg, child_trie)) in trie.children.iter().enumerate() {
        let exact_match = trie.roots_here.iter().enumerate().find(|(ri, r)| {
            !absorbed_by_root.contains_key(ri)
                && ctx.a.nodes.get(*r).is_some_and(|n| {
                    n.kind == SymbolKind::Type
                        && n.status.is_change()
                        && strip_container_prefix(n, &prefix_segs).unwrap_or_else(|| n.name.clone())
                            == *seg
                })
        });
        let matched_ri = exact_match.map(|(ri, _)| ri).or_else(|| {
            trie.roots_here.iter().enumerate().find_map(|(ri, r)| {
                (!absorbed_by_root.contains_key(&ri)
                    && ctx.a.nodes.get(r).is_some_and(|n| {
                        n.status.is_change() && type_root_matches_segment(n, &prefix_segs, seg)
                    }))
                .then_some(ri)
            })
        });
        if let Some(ri) = matched_ri {
            absorbed_by_root.insert(ri, (seg.clone(), child_trie.clone()));
            claimed_children.insert(ci);
        }
    }

    enum Item<'a> {
        Root(usize, &'a String),
        Group(&'a str, &'a PrefixTrie),
    }
    let mut items: Vec<(u8, String, Item<'_>)> = Vec::new();
    for (ri, r) in trie.roots_here.iter().enumerate() {
        let own_rank = ctx.a.nodes.get(r).map_or(9, |n| status_rank(n.status));
        let rank = match absorbed_by_root.get(&ri) {
            Some((_, sub)) => own_rank.min(sub.best_rank(ctx.a)),
            None => own_rank,
        };
        let sort_name = ctx
            .a
            .nodes
            .get(r)
            .map_or_else(|| r.clone(), |n| n.name.clone());
        items.push((rank, sort_name, Item::Root(ri, r)));
    }
    for (ci, (seg, child)) in trie.children.iter().enumerate() {
        if claimed_children.contains(&ci) {
            continue;
        }
        items.push((child.best_rank(ctx.a), seg.clone(), Item::Group(seg, child)));
    }
    items.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

    let mut pending_roots: Vec<SiblingSpec> = Vec::new();
    let flush_roots = |ctx: &mut EmitCtx<'_>, pending: &mut Vec<SiblingSpec>| {
        if !pending.is_empty() {
            let batch = std::mem::take(pending);
            emit_siblings(ctx, &batch, depth, parent_path, &prefix_segs, None);
        }
    };

    for (_, _, item) in items {
        match item {
            Item::Root(ri, root) => {
                let (absorbed_seg, absorbed_trie) = match absorbed_by_root.remove(&ri) {
                    Some((s, t)) => (Some(s), Some(t)),
                    None => (None, None),
                };
                pending_roots.push(SiblingSpec {
                    key: root.clone(),
                    is_consumer: false,
                    is_root: true,
                    shared_callers: Vec::new(),
                    shared_from_coupled: false,
                    absorbed_trie,
                    absorbed_seg,
                });
            }
            Item::Group(seg, child) => {
                if child.total_roots() < 2 {
                    let mut single = Vec::new();
                    child.collect_all_roots(&mut single);
                    for r in single {
                        pending_roots.push(SiblingSpec {
                            key: r,
                            is_consumer: false,
                            is_root: true,
                            shared_callers: Vec::new(),
                            shared_from_coupled: false,
                            absorbed_trie: None,
                            absorbed_seg: None,
                        });
                    }
                } else {
                    flush_roots(ctx, &mut pending_roots);
                    // Compact single-child chain (`EA` -> `TriageEngine` -> `Core` => `EA.TriageEngine.Core`)
                    let mut label_segs = vec![seg.to_string()];
                    let mut cur = child;
                    while cur.roots_here.is_empty() && cur.children.len() == 1 {
                        let (next_seg, next_child) = &cur.children[0];
                        label_segs.push(next_seg.clone());
                        cur = next_child;
                    }
                    let full_segs: Vec<String> = prefix_segs
                        .iter()
                        .cloned()
                        .chain(label_segs.iter().cloned())
                        .collect();
                    let full_prefix = full_segs.join("::");
                    let gkey = group_key(&full_prefix);
                    let display_label = label_segs.join(".");
                    let is_expanded = ctx.expanded.contains(&gkey);
                    ctx.rows.push(TreeRow {
                        key: gkey.clone(),
                        id: gkey.clone(),
                        expand_key: gkey.clone(),
                        depth,
                        has_children: true,
                        is_root: false,
                        group_label: Some(display_label),
                        display_name: None,
                        is_consumer: false,
                        is_repeat: false,
                        caller_bracket: None,
                        shared_callers: Vec::new(),
                        shared_from_coupled: false,
                        collapsed_callee_hint: None,
                    });
                    if is_expanded {
                        emit_trie(ctx, cur, depth + 1, &full_prefix, &gkey);
                    }
                }
            }
        }
    }
    flush_roots(ctx, &mut pending_roots);
}

fn filtered_node_children(
    ctx: &EmitCtx<'_>,
    key: &str,
    absorbed_roots: &HashSet<String>,
) -> Vec<(String, bool)> {
    let Some(node) = ctx.a.nodes.get(key) else {
        return Vec::new();
    };
    children_with_kind(ctx.a, key, ctx.focus)
        .into_iter()
        .filter(|(c, is_con)| {
            if absorbed_roots.contains(c) {
                return false;
            }
            // Never emit a consumer edge back to an ancestor caller on the active path.
            if *is_con && ctx.ancestors.contains(c) {
                return false;
            }
            // Do not duplicate a non-Type member under its enclosing `Type` header
            // when that member is already reached via a call/override edge in the forest.
            if !*is_con
                && node.kind == SymbolKind::Type
                && ctx
                    .a
                    .nodes
                    .get(c)
                    .is_some_and(|cn| cn.kind != SymbolKind::Type)
                && ctx.called_in_forest.contains(c)
            {
                return false;
            }
            true
        })
        .collect()
}

fn format_callee_preview(a: &Analysis, sig: &[(String, bool)]) -> String {
    let mut parts = Vec::new();
    for (ck, is_con) in sig.iter().take(2) {
        if let Some(cn) = a.nodes.get(ck) {
            let prefix = if *is_con {
                format!("{} >", cn.status.glyph())
            } else {
                cn.status.glyph().to_string()
            };
            let trimmed = prefix.trim();
            let short = short_leaf_name(cn);
            if trimmed.is_empty() {
                parts.push(short);
            } else {
                parts.push(format!("{trimmed} {short}"));
            }
        }
    }
    let more = if sig.len() > 2 {
        format!(" +{} more", sig.len() - 2)
    } else {
        String::new()
    };
    format!("→ {}{more}", parts.join(", "))
}

fn emit_siblings(
    ctx: &mut EmitCtx<'_>,
    specs: &[SiblingSpec],
    depth: usize,
    parent_path: &str,
    prefix_segs: &[String],
    parent_owner: Option<&str>,
) {
    struct Prepared {
        spec: SiblingSpec,
        is_repeat: bool,
        kids: Vec<(String, bool)>,
    }
    let mut items: Vec<Prepared> = Vec::new();
    let mut seen_in_batch: HashSet<String> = HashSet::new();
    for spec in specs {
        let Some(node) = ctx.a.nodes.get(&spec.key) else {
            continue;
        };
        if ctx.focus && !node.relevant {
            continue;
        }
        if !seen_in_batch.insert(spec.key.clone()) {
            continue;
        }
        let is_repeat = ctx.visited_global.contains(&spec.key) || ctx.ancestors.contains(&spec.key);
        let kids = if is_repeat {
            Vec::new()
        } else {
            let mut absorbed_set = HashSet::new();
            if let Some(ref sub) = spec.absorbed_trie {
                let mut r_vec = Vec::new();
                sub.collect_all_roots(&mut r_vec);
                absorbed_set.extend(r_vec);
            }
            filtered_node_children(ctx, &spec.key, &absorbed_set)
        };
        items.push(Prepared {
            spec: spec.clone(),
            is_repeat,
            kids,
        });
    }

    // Count how many siblings in `items` share each child key.
    let mut child_callers: HashMap<String, Vec<usize>> = HashMap::new();
    for (idx, item) in items.iter().enumerate() {
        for (ck, _) in &item.kids {
            child_callers.entry(ck.clone()).or_default().push(idx);
        }
    }

    // Identify Coupled Caller Groups: >=2 siblings without absorbed sub-tries whose
    // non-empty child lists are identical and exclusively called by that exact group.
    let mut coupled_group_of: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut coupled_secondary: HashSet<usize> = HashSet::new();
    if ctx.grouped {
        for idx in 0..items.len() {
            if coupled_secondary.contains(&idx)
                || coupled_group_of.contains_key(&idx)
                || items[idx].kids.is_empty()
                || items[idx].spec.absorbed_trie.is_some()
            {
                continue;
            }
            let first_ck = &items[idx].kids[0].0;
            let candidates = &child_callers[first_ck];
            if candidates.len() < 2 {
                continue;
            }
            let sig = &items[idx].kids;
            let all_match = candidates.iter().all(|&c_idx| {
                items[c_idx].spec.absorbed_trie.is_none() && &items[c_idx].kids == sig
            }) && sig.iter().all(|(ck, _)| &child_callers[ck] == candidates);
            if all_match {
                let group = candidates.clone();
                for &member in &group[1..] {
                    coupled_secondary.insert(member);
                }
                coupled_group_of.insert(group[0], group);
            }
        }
    }

    // For partial-overlap shared children (not in a coupled group), determine which
    // expanded sibling caller is the anchor (the last expanded caller in sibling order)
    // so each shared child renders at most once while remaining visible whenever ANY of
    // its callers is expanded.
    let is_single_expanded = |idx: usize, expanded: &HashSet<String>| -> bool {
        let row_ek = format!("{parent_path}/{}", items[idx].spec.key);
        expanded.contains(&row_ek) || expanded.contains(&items[idx].spec.key)
    };
    let mut partial_shared_anchor: HashMap<String, usize> = HashMap::new();
    for (ck, callers) in &child_callers {
        if callers.len() < 2 {
            continue;
        }
        // Skip if handled by a coupled group
        if coupled_group_of.contains_key(&callers[0]) {
            continue;
        }
        if let Some(&last_exp) = callers
            .iter()
            .rfind(|&&c_idx| is_single_expanded(c_idx, ctx.expanded))
        {
            partial_shared_anchor.insert(ck.clone(), last_exp);
        }
    }

    for idx in 0..items.len() {
        if coupled_secondary.contains(&idx) {
            continue;
        }
        if let Some(group) = coupled_group_of.get(&idx).cloned() {
            let group_keys_joined = group
                .iter()
                .map(|&gi| items[gi].spec.key.as_str())
                .collect::<Vec<_>>()
                .join("+");
            let group_ek = format!("{parent_path}/@callers::{group_keys_joined}");
            let is_exp = ctx.expanded.contains(&group_ek)
                || group
                    .iter()
                    .any(|&gi| ctx.expanded.contains(&items[gi].spec.key));
            let sig = items[group[0]].kids.clone();
            let preview = (!is_exp).then(|| format_callee_preview(ctx.a, &sig));

            for &gi in &group {
                ctx.visited_global.insert(items[gi].spec.key.clone());
            }

            let mut caller_leaf_names = Vec::new();
            let mut common_owner: Option<Option<String>> = None;
            for (pos, &gi) in group.iter().enumerate() {
                let item = &items[gi];
                let node = &ctx.a.nodes[&item.spec.key];
                caller_leaf_names.push(short_leaf_name(node));
                let this_owner = node_owner_for_children(node);
                common_owner = Some(match common_owner {
                    None => this_owner,
                    Some(prev) if prev == this_owner => prev,
                    Some(_) => None,
                });

                let bracket = if pos == 0 {
                    CallerBracket::First
                } else if pos + 1 == group.len() {
                    CallerBracket::Last
                } else {
                    CallerBracket::Middle
                };
                let row_id = format!("{parent_path}/{}", item.spec.key);
                let disp = shortened_display_name(node, ctx.grouped, prefix_segs, parent_owner);
                ctx.rows.push(TreeRow {
                    key: item.spec.key.clone(),
                    id: row_id,
                    expand_key: group_ek.clone(),
                    depth,
                    has_children: true,
                    is_root: item.spec.is_root,
                    group_label: None,
                    display_name: disp,
                    is_consumer: item.spec.is_consumer,
                    is_repeat: false,
                    caller_bracket: Some(bracket),
                    shared_callers: item.spec.shared_callers.clone(),
                    shared_from_coupled: item.spec.shared_from_coupled,
                    collapsed_callee_hint: if pos + 1 == group.len() {
                        preview.clone()
                    } else {
                        None
                    },
                });
            }

            if is_exp {
                let prev_len = ctx.ancestors.len();
                for &gi in &group {
                    ctx.ancestors.push(items[gi].spec.key.clone());
                }
                let child_specs: Vec<SiblingSpec> = sig
                    .into_iter()
                    .map(|(ck, is_con)| SiblingSpec {
                        key: ck,
                        is_consumer: is_con,
                        is_root: false,
                        shared_callers: caller_leaf_names.clone(),
                        shared_from_coupled: true,
                        absorbed_trie: None,
                        absorbed_seg: None,
                    })
                    .collect();
                let next_owner = common_owner.flatten();
                emit_siblings(
                    ctx,
                    &child_specs,
                    depth + 1,
                    &group_ek,
                    prefix_segs,
                    next_owner.as_deref(),
                );
                ctx.ancestors.truncate(prev_len);
            }
            continue;
        }

        // Standard (or partial-overlap) sibling row
        let item = &items[idx];
        let node = &ctx.a.nodes[&item.spec.key];
        if !item.is_repeat {
            ctx.visited_global.insert(item.spec.key.clone());
        }
        let row_id = format!("{parent_path}/{}", item.spec.key);
        let has_absorbed = item
            .spec
            .absorbed_trie
            .as_ref()
            .is_some_and(|t| t.total_roots() > 0);
        let has_children = !item.is_repeat && (!item.kids.is_empty() || has_absorbed);
        let is_exp = has_children
            && (ctx.expanded.contains(&row_id) || ctx.expanded.contains(&item.spec.key));
        let disp = shortened_display_name(node, ctx.grouped, prefix_segs, parent_owner);
        ctx.rows.push(TreeRow {
            key: item.spec.key.clone(),
            id: row_id.clone(),
            expand_key: row_id.clone(),
            depth,
            has_children,
            is_root: item.spec.is_root,
            group_label: None,
            display_name: disp,
            is_consumer: item.spec.is_consumer,
            is_repeat: item.is_repeat,
            caller_bracket: None,
            shared_callers: item.spec.shared_callers.clone(),
            shared_from_coupled: item.spec.shared_from_coupled,
            collapsed_callee_hint: None,
        });

        if !is_exp {
            continue;
        }

        ctx.ancestors.push(item.spec.key.clone());
        let next_owner = node_owner_for_children(node);

        // Emit unique children + any partial-overlap shared children anchored at `idx`.
        let mut unique_specs = Vec::new();
        let mut anchored_shared_specs = Vec::new();
        for (ck, is_con) in &item.kids {
            let callers = &child_callers[ck];
            if callers.len() == 1 {
                unique_specs.push(SiblingSpec {
                    key: ck.clone(),
                    is_consumer: *is_con,
                    is_root: false,
                    shared_callers: Vec::new(),
                    shared_from_coupled: false,
                    absorbed_trie: None,
                    absorbed_seg: None,
                });
            } else if partial_shared_anchor.get(ck) == Some(&idx) {
                let caller_names: Vec<String> = callers
                    .iter()
                    .map(|&ci| short_leaf_name(&ctx.a.nodes[&items[ci].spec.key]))
                    .collect();
                anchored_shared_specs.push(SiblingSpec {
                    key: ck.clone(),
                    is_consumer: *is_con,
                    is_root: false,
                    shared_callers: caller_names,
                    shared_from_coupled: false,
                    absorbed_trie: None,
                    absorbed_seg: None,
                });
            }
        }

        if !unique_specs.is_empty() {
            emit_siblings(
                ctx,
                &unique_specs,
                depth + 1,
                &row_id,
                prefix_segs,
                next_owner.as_deref(),
            );
        }
        if !anchored_shared_specs.is_empty() {
            let shared_parent_path = format!("{parent_path}/@shared");
            emit_siblings(
                ctx,
                &anchored_shared_specs,
                depth + 1,
                &shared_parent_path,
                prefix_segs,
                next_owner.as_deref(),
            );
        }
        if let (Some(sub_trie), Some(seg)) = (&item.spec.absorbed_trie, &item.spec.absorbed_seg) {
            let mut sub_segs = prefix_segs.to_vec();
            sub_segs.push(seg.clone());
            let sub_prefix = sub_segs.join("::");
            emit_trie(ctx, sub_trie, depth + 1, &sub_prefix, &row_id);
        }

        ctx.ancestors.pop();
    }
}

pub fn children_of(a: &Analysis, key: &str, focus: bool) -> Vec<String> {
    children_with_kind(a, key, focus)
        .into_iter()
        .map(|(k, _)| k)
        .collect()
}

pub fn children_with_kind(a: &Analysis, key: &str, focus: bool) -> Vec<(String, bool)> {
    let Some(n) = a.nodes.get(key) else {
        return vec![];
    };
    let direct_set: HashSet<&str> = n
        .callees
        .iter()
        .chain(n.implementations.iter())
        .map(String::as_str)
        .collect();
    let mut kids: Vec<(String, bool)> = n
        .callees
        .iter()
        .chain(n.implementations.iter())
        .filter(|c| *c != key)
        .map(|c| (c.clone(), false))
        .chain(
            n.consumers
                .iter()
                .filter(|c| *c != key && !direct_set.contains(c.as_str()))
                .map(|c| (c.clone(), true)),
        )
        .filter(|(c, _)| a.nodes.contains_key(c))
        .filter(|(c, _)| !focus || a.nodes.get(c).map(|x| x.relevant).unwrap_or(true))
        .collect();
    kids.sort_by(|(x, cx), (y, cy)| {
        let nx = &a.nodes[x];
        let ny = &a.nodes[y];
        cx.cmp(cy)
            .then_with(|| status_rank(nx.status).cmp(&status_rank(ny.status)))
            .then_with(|| nx.relevant.cmp(&ny.relevant).reverse())
            .then_with(|| x.cmp(y))
    });
    kids.dedup_by(|a, b| a.0 == b.0);
    kids
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case("README.md", None, Some("hello\n"), Status::Added; "unsupported_added")]
    #[test_case("image.bin", Some("before\n"), None, Status::Removed; "unsupported_removed")]
    #[test_case("Cargo.toml", Some("a = 1\n"), Some("a = 2\n"), Status::BodyChanged; "unsupported_modified")]
    #[test_case("main.rs", Some("// before\n"), Some("// after\n"), Status::BodyChanged; "supported_without_symbols")]
    #[test_case("empty.txt", None, Some(""), Status::Added; "empty_added")]
    fn files_without_symbols_remain_reviewable(
        path: &str,
        base: Option<&str>,
        head: Option<&str>,
        status: Status,
    ) {
        let analysis = analyze_pair(path, base, head);
        let key = node_key(path, "<file>");
        let node = &analysis.nodes[&key];
        assert_eq!(node.kind, SymbolKind::File);
        assert_eq!(node.status, status);
        assert!(analysis.entry_points.contains(&key));
        assert!(node.relevant);
        assert_eq!(analysis.base_body(node).as_deref(), base);
        assert_eq!(analysis.head_body(node).as_deref(), head);
    }

    #[test_case("// before\nfn target() {}\n", "// after\nfn target() {}\n"; "outside_symbols")]
    #[test_case("fn target() {}\n", "fn target() { /* changed */ }\n"; "comment_only_inside_symbol")]
    fn textual_changes_are_not_hidden_by_unchanged_symbols(base: &str, head: &str) {
        let mut analysis = analyze_pair("main.rs", Some(base), Some(head));
        analysis.ensure_changed_file("main.rs");
        let changed = analysis
            .nodes
            .values()
            .find(|node| node.status.is_change())
            .unwrap();
        assert!(changed.relevant);
        assert_eq!(
            analysis.base_body(changed).unwrap().trim_end(),
            base.trim_end()
        );
        assert_eq!(
            analysis.head_body(changed).unwrap().trim_end(),
            head.trim_end()
        );
    }

    fn analyze_pair(file: &str, base: Option<&str>, head: Option<&str>) -> Analysis {
        let inputs = Inputs {
            files: vec![(
                file.to_string(),
                base.map(|s| s.to_string()),
                head.map(|s| s.to_string()),
            )],
            extra_head: vec![],
            include_tests: false,
        };
        analyze(&inputs)
    }

    #[test]
    fn caller_in_unchanged_file_is_external() {
        let base = "fn a() { b(); }\nfn b() {}\n";
        let head = "fn a() { b(); }\nfn b() { println!(\"changed\"); }\n";
        let inputs = Inputs {
            files: vec![(
                "m.rs".to_string(),
                Some(base.to_string()),
                Some(head.to_string()),
            )],
            // unchanged file m2.rs calls a()
            extra_head: vec![("m2.rs".to_string(), "fn top() { a(); }\n".to_string())],
            include_tests: false,
        };
        let a = analyze(&inputs);
        // b changed; a unchanged but reaches a change
        assert_eq!(a.nodes[&node_key("m.rs", "b")].status, Status::BodyChanged);
        // a is called from top() in m2.rs (external) => a IS the review root
        assert!(
            a.nodes[&node_key("m.rs", "a")]
                .external_callers
                .contains(&node_key("m2.rs", "top"))
        );
        // b's only caller is a (in scope) => b is NOT an entry point
        assert!(a.entry_points.contains(&node_key("m.rs", "a")));
        assert!(!a.entry_points.contains(&node_key("m.rs", "b")));
    }

    #[test]
    fn statuses_and_entry_points() {
        let base = "fn a() { b(); }\nfn b() { c(); }\nfn c() { println!(\"x\"); }\n";
        let head = "fn a() { b(); }\nfn b() { c(); }\nfn c() { println!(\"changed\"); }\nfn fresh() { c(); }\n";
        let a = analyze_pair("m.rs", Some(base), Some(head));
        assert_eq!(a.nodes.len(), 4);
        assert_eq!(a.nodes[&node_key("m.rs", "c")].status, Status::BodyChanged);
        assert_eq!(a.nodes[&node_key("m.rs", "fresh")].status, Status::Added);
        // entry points: a (no callers) and fresh (no callers)
        let eps: HashSet<&str> = a
            .entry_points
            .iter()
            .map(|k| k.rsplit("::").next().unwrap())
            .collect();
        assert!(eps.contains("a") && eps.contains("fresh"), "{eps:?}");
        // b changed? no. But relevance: a -> b -> c(changed) => a relevant
        assert!(a.nodes[&node_key("m.rs", "a")].relevant);
        // focus rows from a: only relevant nodes visible
        let rows = build_rows(
            &a,
            &[node_key("m.rs", "a")],
            &HashSet::from([node_key("m.rs", "a"), node_key("m.rs", "b")]),
            true,
        );
        let names: Vec<&str> = rows
            .iter()
            .map(|r| r.key.rsplit("::").next().unwrap())
            .collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn removed_and_added() {
        let base = "fn gone() { helper(); extra(); }\nfn helper() {}\nfn extra() {}\n";
        let head = "fn new_thing() { helper(); }\nfn helper() {}\nfn extra() {}\n";
        let a = analyze_pair("m.rs", Some(base), Some(head));
        assert_eq!(a.nodes[&node_key("m.rs", "gone")].status, Status::Removed);
        assert_eq!(
            a.nodes[&node_key("m.rs", "new_thing")].status,
            Status::Added
        );
        assert_eq!(
            a.nodes[&node_key("m.rs", "helper")].status,
            Status::Unchanged
        );
        // removed node's caller list is empty but its callees resolve
        let gone = &a.nodes[&node_key("m.rs", "gone")];
        assert!(gone.callees.contains(&node_key("m.rs", "helper")));
        let eps: HashSet<&str> = a
            .entry_points
            .iter()
            .map(|k| k.rsplit("::").next().unwrap())
            .collect();
        assert!(eps.contains("gone") && eps.contains("new_thing"));
    }

    #[test]
    fn test_fns_filtered_unless_included() {
        let base = "fn a() { b(); }\nfn b() {}\n";
        let head = "fn a() { b(); }\nfn b() { c(); }\nfn c() {}\n\n#[cfg(test)]\nmod tests {\n    fn test_b() { super::b(); }\n}\n";
        let mk = |include_tests: bool| Inputs {
            files: vec![(
                "m.rs".to_string(),
                Some(base.to_string()),
                Some(head.to_string()),
            )],
            extra_head: vec![],
            include_tests,
        };
        let a = analyze(&mk(false));
        assert!(a.nodes.contains_key(&node_key("m.rs", "c")));
        assert!(!a.nodes.contains_key(&node_key("m.rs", "tests::test_b")));
        let a = analyze(&mk(true));
        assert!(a.nodes.contains_key(&node_key("m.rs", "tests::test_b")));
    }

    #[test]
    fn move_detected() {
        let base = "fn alpha() { println!(\"same\"); }\n";
        let head = "fn beta() { println!(\"same\"); }\n";
        let a = analyze_pair("m.rs", Some(base), Some(head));
        assert_eq!(a.nodes[&node_key("m.rs", "beta")].status, Status::Moved);
        let beta = &a.nodes[&node_key("m.rs", "beta")];
        assert_eq!(beta.moved_from.as_deref(), Some("alpha"));
        assert!(a.base_body(beta).unwrap().contains("fn alpha()"));
        assert!(a.head_body(beta).unwrap().contains("fn beta()"));
        assert_eq!(a.summary, (0, 0, 0, 1));
    }

    #[test]
    fn rust_container_identities_are_distinct() {
        let base =
            "struct A; struct B; impl A { fn new() { old(); } } impl B { fn new() { stable(); } }";
        let head = "struct A; struct B; impl A { fn new() { edited(); } } impl B { fn new() { stable(); } }";
        let a = analyze_pair("m.rs", Some(base), Some(head));
        assert_eq!(
            a.nodes[&node_key("m.rs", "A::new")].status,
            Status::BodyChanged
        );
        assert_eq!(
            a.nodes[&node_key("m.rs", "B::new")].status,
            Status::Unchanged
        );
        assert_eq!(a.summary, (0, 0, 1, 0));
    }

    #[test]
    fn csharp_overloads_pair_by_signature() {
        let base =
            "class Worker { void Log(int x) { Before(); } void Log(string s) { Stable(); } }";
        let head = "class Worker { void Log(int x) { After(); } void Log(string s) { Stable(); } }";
        let a = analyze_pair("Worker.cs", Some(base), Some(head));
        assert_eq!(
            a.nodes[&node_key("Worker.cs", "Worker::Log(int x)")].status,
            Status::BodyChanged
        );
        assert_eq!(
            a.nodes[&node_key("Worker.cs", "Worker::Log(string s)")].status,
            Status::Unchanged
        );
        assert_eq!(a.summary, (0, 0, 1, 0));
    }

    #[test]
    fn adding_overload_keeps_existing_identity_paired() {
        let base = "class Worker { void Log(int x) { Existing(); } }";
        let head =
            "class Worker { void Log(int x) { Existing(); } void Log(string s) { NewBody(); } }";
        let a = analyze_pair("Worker.cs", Some(base), Some(head));
        let existing = &a.nodes[&node_key("Worker.cs", "Worker::Log(int x)")];
        assert_eq!(existing.status, Status::Unchanged);
        assert!(a.base_body(existing).unwrap().contains("Existing();"));
        assert_eq!(
            a.nodes[&node_key("Worker.cs", "Worker::Log(string s)")].status,
            Status::Added
        );
        assert_eq!(a.summary, (1, 0, 0, 0));
    }

    #[test]
    fn duplicate_signatures_and_removed_collisions_keep_every_node() {
        let base = "fn dup() { one(); }\nfn dup() { two(); }\n";
        let head = "fn dup() { three(); }\nfn dup() { four(); }\n";
        let a = analyze_pair("m.rs", Some(base), Some(head));
        assert_eq!(a.nodes.len(), 4);
        assert_eq!(a.summary, (2, 2, 0, 0));
        for (key, node) in &a.nodes {
            assert_eq!(key, &node.key);
            assert_eq!(key, &node_key(&node.file, &node.name));
        }
        let again = analyze_pair("m.rs", Some(base), Some(head));
        assert_eq!(
            a.nodes.keys().collect::<HashSet<_>>(),
            again.nodes.keys().collect::<HashSet<_>>()
        );
    }

    #[test]
    fn repeated_signatures_pair_equal_bodies_by_appearance() {
        let base = "fn dup() { one(); }\nfn dup() { two(); }\n";
        let head = "fn dup() { two(); }\nfn dup() { one(); }\n";
        let a = analyze_pair("m.rs", Some(base), Some(head));
        assert_eq!(a.nodes.len(), 2);
        assert_eq!(a.summary, (0, 0, 0, 0));
        for node in a.nodes.values() {
            assert_eq!(node.status, Status::Unchanged);
            let base = a.base_body(node).unwrap();
            let head = a.head_body(node).unwrap();
            assert_eq!(base, head);
            assert_ne!(
                node.base.as_ref().unwrap().start_row,
                node.head.as_ref().unwrap().start_row
            );
        }
    }

    #[test]
    fn self_recursion_has_an_entry_point_and_keeps_edges() {
        let base = "fn fact(n: u64) -> u64 { if n == 0 { 1 } else { n * fact(n - 1) } }";
        let head = "fn fact(n: u64) -> u64 { if n == 0 { 2 } else { n * fact(n - 1) } }";
        let a = analyze_pair("m.rs", Some(base), Some(head));
        let key = node_key("m.rs", "fact");
        assert_eq!(a.entry_points, vec![key.clone()]);
        assert_eq!(a.nodes[&key].callers, vec![key.clone()]);
        assert_eq!(a.nodes[&key].callees, vec![key]);
    }

    #[test]
    fn mutual_recursion_chooses_changed_component_member() {
        let base = "fn a() { b(); }\nfn b() { a(); }\n";
        let head = "fn a() { b(); }\nfn b() { a(); edited(); }\n";
        let a = analyze_pair("m.rs", Some(base), Some(head));
        assert_eq!(a.entry_points, vec![node_key("m.rs", "b")]);
        assert!(a.nodes[&node_key("m.rs", "a")].relevant);
    }

    #[test]
    fn cyclic_relevance_is_deterministic() {
        let base = "fn e() { b(); }\nfn b() { a(); }\nfn a() { b(); c(); }\nfn c() { before(); }\n";
        let head = "fn e() { b(); }\nfn b() { a(); }\nfn a() { b(); c(); }\nfn c() { after(); }\n";
        for _ in 0..25 {
            let a = analyze_pair("m.rs", Some(base), Some(head));
            assert_eq!(a.entry_points, vec![node_key("m.rs", "e")]);
            assert!(a.nodes.values().all(|node| node.relevant));
            assert_eq!(
                build_rows(&a, &a.entry_points, &HashSet::new(), true).len(),
                1
            );
        }
    }

    #[test]
    fn unchanged_candidates_prevent_false_unique_resolution() {
        let a = analyze(&Inputs {
            files: vec![
                (
                    "x.rs".into(),
                    None,
                    Some("fn caller() { helper(); }".into()),
                ),
                ("y.rs".into(), None, Some("fn helper() {}".into())),
            ],
            extra_head: vec![("u.rs".into(), "fn helper() {}".into())],
            include_tests: false,
        });
        let caller = &a.nodes[&node_key("x.rs", "caller")];
        assert!(caller.callees.is_empty());
        assert_eq!(caller.lost_callees, vec!["helper"]);
    }

    #[test]
    fn unchanged_local_helpers_shadow_changed_definitions() {
        let a = analyze(&Inputs {
            files: vec![("changed.rs".into(), None, Some("fn init() {}".into()))],
            extra_head: vec![("u.rs".into(), "fn top() { init(); }\nfn init() {}".into())],
            include_tests: false,
        });
        assert!(
            a.nodes[&node_key("changed.rs", "init")]
                .external_callers
                .is_empty()
        );
        assert!(!a.file_labels.contains_key("u.rs"));
    }

    #[test]
    fn resolution_respects_language_families() {
        let a = analyze(&Inputs {
            files: vec![
                (
                    "x.py".into(),
                    None,
                    Some("def caller():\n    helper()\n".into()),
                ),
                ("y.rs".into(), None, Some("fn helper() {}".into())),
            ],
            extra_head: vec![],
            include_tests: false,
        });
        let caller = &a.nodes[&node_key("x.py", "caller")];
        assert!(caller.callees.is_empty());
        assert_eq!(caller.lost_callees, vec!["helper"]);
        let a = analyze(&Inputs {
            files: vec![(
                "helper.ts".into(),
                None,
                Some("function helper() {}".into()),
            )],
            extra_head: vec![("caller.js".into(), "function caller() { helper(); }".into())],
            include_tests: false,
        });
        assert_eq!(
            a.nodes[&node_key("helper.ts", "helper")].external_callers,
            vec!["caller.js::caller"]
        );
    }

    #[test]
    fn live_unchanged_definition_does_not_fall_back_to_removed_node() {
        let a = analyze(&Inputs {
            files: vec![
                (
                    "caller.rs".into(),
                    None,
                    Some("fn caller() { helper(); }".into()),
                ),
                ("deleted.rs".into(), Some("fn helper() {}".into()), None),
            ],
            extra_head: vec![("u.rs".into(), "fn helper() {}".into())],
            include_tests: false,
        });
        let caller = &a.nodes[&node_key("caller.rs", "caller")];
        assert!(caller.callees.is_empty());
        assert_eq!(caller.lost_callees, vec!["helper"]);
    }

    #[test]
    fn absent_head_definition_falls_back_to_unique_removed_node() {
        let base = "fn caller() { helper(); }\nfn helper() {}\n";
        let head = "fn caller() { helper(); }\n";
        let a = analyze_pair("m.rs", Some(base), Some(head));
        assert_eq!(
            a.nodes[&node_key("m.rs", "caller")].callees,
            vec![node_key("m.rs", "helper")]
        );
        assert_eq!(a.nodes[&node_key("m.rs", "helper")].status, Status::Removed);
    }

    #[test]
    fn local_overloads_resolve_as_a_set_and_edges_are_deduplicated() {
        let head = "class Worker { void Caller() { Log(1); Log(2); } void Log(int x) {} void Log(string s) {} }";
        let a = analyze_pair("Worker.cs", None, Some(head));
        let caller = &a.nodes[&node_key("Worker.cs", "Worker::Caller")];
        assert_eq!(
            caller.callees,
            vec![
                node_key("Worker.cs", "Worker::Log(int x)"),
                node_key("Worker.cs", "Worker::Log(string s)"),
            ]
        );
        for callee in &caller.callees {
            assert_eq!(a.nodes[callee].callers, vec![caller.key.clone()]);
        }
    }

    #[test]
    fn spans_and_bodies_use_their_own_revision() {
        let base = "fn sample() {\r\n    old_line();\r\n}\r\n";
        let head = "// inserted\n// inserted\n// inserted\nfn sample() {\n    new_line();\n}\n";
        let a = analyze_pair("m.rs", Some(base), Some(head));
        let sample = &a.nodes[&node_key("m.rs", "sample")];
        assert_eq!(sample.status, Status::BodyChanged);
        assert_eq!(sample.base.as_ref().unwrap().start_row, 0);
        assert_eq!(sample.head.as_ref().unwrap().start_row, 3);
        assert_eq!(
            sample.head.as_ref().unwrap().end_row - sample.base.as_ref().unwrap().end_row,
            3
        );
        assert_eq!(
            a.base_body(sample).as_deref(),
            Some("fn sample() {\n    old_line();\n}")
        );
        assert_eq!(
            a.head_body(sample).as_deref(),
            Some("fn sample() {\n    new_line();\n}")
        );
        assert_eq!(a.base_srcs.len(), 1);
        assert_eq!(a.head_srcs.len(), 1);
    }

    #[test]
    fn move_preference_and_origin_spans_are_deterministic() {
        let inputs = Inputs {
            files: vec![
                (
                    "z.rs".into(),
                    Some("fn local() { same(); }".into()),
                    Some("fn beta() { same(); }".into()),
                ),
                ("a.rs".into(), Some("fn other() { same(); }".into()), None),
                ("b.rs".into(), None, Some("fn alpha() { same(); }".into())),
            ],
            extra_head: vec![],
            include_tests: false,
        };
        for _ in 0..25 {
            let a = analyze(&inputs);
            let alpha = &a.nodes[&node_key("b.rs", "alpha")];
            let beta = &a.nodes[&node_key("z.rs", "beta")];
            assert_eq!(alpha.moved_from.as_deref(), Some("other"));
            assert_eq!(alpha.base.as_ref().unwrap().file, "a.rs");
            assert_eq!(beta.moved_from.as_deref(), Some("local"));
            assert_eq!(beta.base.as_ref().unwrap().file, "z.rs");
            assert_eq!(a.summary, (0, 0, 0, 2));
        }
    }

    #[test]
    fn move_prefers_same_file_over_a_lower_key() {
        let a = analyze(&Inputs {
            files: vec![
                ("a.rs".into(), Some("fn other() { same(); }".into()), None),
                (
                    "z.rs".into(),
                    Some("fn local() { same(); }".into()),
                    Some("fn beta() { same(); }".into()),
                ),
            ],
            extra_head: vec![],
            include_tests: false,
        });
        let beta = &a.nodes[&node_key("z.rs", "beta")];
        assert_eq!(beta.base.as_ref().unwrap().file, "z.rs");
        assert_eq!(beta.moved_from.as_deref(), Some("local"));
        assert_eq!(a.nodes[&node_key("a.rs", "other")].status, Status::Removed);
    }

    #[test]
    fn moves_never_cross_language_families() {
        let base = "class Worker { void alpha() {} };";
        let head = "class Worker { void beta() {} }";
        let a = analyze(&Inputs {
            files: vec![
                ("base.cpp".into(), Some(base.into()), None),
                ("head.cs".into(), None, Some(head.into())),
            ],
            extra_head: vec![],
            include_tests: false,
        });
        assert_eq!(
            a.nodes[&node_key("base.cpp", "Worker")].status,
            Status::Removed
        );
        assert_eq!(
            a.nodes[&node_key("head.cs", "Worker")].status,
            Status::Added
        );
        assert_eq!(
            a.nodes[&node_key("base.cpp", "Worker::alpha")].status,
            Status::Removed
        );
        assert_eq!(
            a.nodes[&node_key("head.cs", "Worker::beta")].status,
            Status::Added
        );
    }

    #[test]
    fn removed_calls_resolve_base_definitions_to_moved_nodes() {
        let base = "fn gone() { helper(); extra(); }\nfn helper() { stable(); }\n";
        let head = "fn renamed() { stable(); }\n";
        let a = analyze_pair("m.rs", Some(base), Some(head));
        let renamed = &a.nodes[&node_key("m.rs", "renamed")];
        assert_eq!(renamed.status, Status::Moved);
        assert_eq!(renamed.moved_from.as_deref(), Some("helper"));
        let gone = &a.nodes[&node_key("m.rs", "gone")];
        assert_eq!(gone.callees, vec![renamed.key.clone()]);
        assert_eq!(gone.lost_callees, vec!["extra"]);
        assert_eq!(renamed.callers, vec![gone.key.clone()]);
    }

    #[test]
    fn labels_count_distinct_paths_not_functions() {
        let a = analyze_pair("lib.rs", None, Some("fn a() {}\nfn b() {}\n"));
        assert_eq!(a.label("lib.rs"), "lib.rs");
        let a = analyze(&Inputs {
            files: vec![
                (
                    "crates/a/lib.rs".into(),
                    None,
                    Some("fn a() {}\nfn b() {}".into()),
                ),
                ("crates/b/lib.rs".into(), None, Some("fn c() {}".into())),
            ],
            extra_head: vec![],
            include_tests: false,
        });
        assert_eq!(a.label("crates/a/lib.rs"), "a/lib.rs");
        assert_eq!(a.label("crates/b/lib.rs"), "b/lib.rs");
        assert_eq!(a.label("unknown.rs"), "unknown.rs");
        let labels = file_labels(HashSet::from([
            "lib.rs".into(),
            "a/lib.rs".into(),
            "x/a/lib.rs".into(),
        ]));
        assert_eq!(labels["lib.rs"], "lib.rs");
        assert_eq!(labels["a/lib.rs"], "a/lib.rs");
        assert_eq!(labels["x/a/lib.rs"], "x/a/lib.rs");
    }

    #[test]
    fn test_definitions_are_filtered_from_both_universes() {
        let inputs = Inputs {
            files: vec![(
                "m.rs".into(),
                Some("#[cfg(test)] mod tests { fn deleted() {} }".into()),
                Some("fn caller() { helper(); }\nfn helper() {}".into()),
            )],
            extra_head: vec![(
                "u.rs".into(),
                "#[cfg(test)] mod tests { fn helper() {} }".into(),
            )],
            include_tests: false,
        };
        let a = analyze(&inputs);
        assert_eq!(a.nodes.len(), 3);
        assert_eq!(a.nodes[&node_key("m.rs", "<file>")].kind, SymbolKind::File);
        assert_eq!(a.summary.1, 0);
        assert_eq!(
            a.nodes[&node_key("m.rs", "caller")].callees,
            vec![node_key("m.rs", "helper")]
        );
    }

    #[test]
    fn same_line_type_and_member_diffs_use_exact_utf8_source_ranges() {
        let base = "// café\r\ninterface Worker { int Run(); int Stable(); }\r\n";
        let head = base.replace("int Run()", "long Run(int flags)");
        let a = analyze_pair("worker.cs", Some(base), Some(&head));
        let header = &a.nodes[&node_key("worker.cs", "Worker")];
        assert_eq!(header.status, Status::Unchanged);
        assert_eq!(a.head_body(header).as_deref(), Some("interface Worker"));
        assert_eq!(a.base_body(header), a.head_body(header));
        let changed = &a.nodes[&node_key("worker.cs", "Worker::Run")];
        assert_eq!(a.base_body(changed).as_deref(), Some("int Run();"));
        assert_eq!(
            a.head_body(changed).as_deref(),
            Some("long Run(int flags);")
        );
        let stable = &a.nodes[&node_key("worker.cs", "Worker::Stable")];
        assert_eq!(a.base_body(stable), a.head_body(stable));
    }

    #[test]
    fn cfg_test_contracts_and_impl_headers_follow_test_visibility() {
        let base = "#[cfg(test)] trait Hidden { fn run(&self); }\n#[cfg(test)] struct Worker;\n#[cfg(test)] impl Hidden for Worker { fn run(&self) {} }\nfn live() -> u32 { 1 }\n";
        let head = base.replace("{ 1 }", "{ 2 }");
        for include_tests in [false, true] {
            let a = analyze(&Inputs {
                files: vec![("m.rs".into(), Some(base.into()), Some(head.clone()))],
                extra_head: vec![],
                include_tests,
            });
            for name in [
                "Hidden",
                "Hidden::run",
                "Worker",
                "<Worker as Hidden>",
                "<Worker as Hidden>::run",
            ] {
                assert_eq!(
                    a.nodes.contains_key(&node_key("m.rs", name)),
                    include_tests,
                    "{name}"
                );
            }
            assert_eq!(
                a.nodes[&node_key("m.rs", "live")].status,
                Status::BodyChanged
            );
        }
    }
}

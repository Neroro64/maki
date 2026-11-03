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
    let mut ordered_files: Vec<_> = inputs.files.iter().collect();
    ordered_files.sort_by(|a, b| a.0.cmp(&b.0));
    for (path, base, head) in ordered_files {
        let b = base.as_ref().and_then(|src| {
            base_srcs.insert(path.clone(), src.clone());
            FileData::parse(path, src, inputs.include_tests).map(|file| {
                let i = base_files.len();
                base_files.push(file);
                i
            })
        });
        let h = head.as_ref().and_then(|src| {
            head_srcs.insert(path.clone(), src.clone());
            FileData::parse(path, src, inputs.include_tests).map(|file| {
                let i = head_files.len();
                head_files.push(file);
                i
            })
        });
        file_pairs.push((b, h));
    }
    let mut ordered_extras: Vec<_> = inputs.extra_head.iter().collect();
    ordered_extras.sort_by(|a, b| a.0.cmp(&b.0));
    for (path, src) in ordered_extras {
        if let Some(mut file) = FileData::parse(path, src, inputs.include_tests) {
            // Extra definitions participate in uniqueness, but have no nodes.
            file.keys.fill(None);
            extras.push(file);
        }
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
                    name: data.display[function].as_ref().unwrap().clone(),
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
                    name,
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
        let mut seen = HashSet::new();
        let mut pending: Vec<_> = (0..count)
            .flat_map(|file| {
                let data = &files[file];
                (0..data.fns.len())
                    .filter(|&i| data.display[i].is_some())
                    .map(move |i| (file, i))
            })
            .collect();
        while let Some(id) = pending.pop() {
            if !seen.insert(id) {
                continue;
            }
            if id.0 >= count {
                context.insert((id.0 - count, id.1));
            }
            pending.extend(
                graph
                    .implementations
                    .get(&id)
                    .into_iter()
                    .flatten()
                    .copied(),
            );
            pending.extend(graph.contracts.get(&id).into_iter().flatten().copied());
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
    for (file, data) in head_files.iter().enumerate() {
        for (function, key) in data.keys.iter().enumerate() {
            if let Some(key) = key {
                let (callees, lost) =
                    head_universe.resolve_calls(file, function, Some(&removed_names));
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
                let (callees, lost) = base_universe.resolve_calls(file, function, None);
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
            let (callees, lost) = head_universe.resolve_calls(
                head_files.len() + file,
                function,
                Some(&removed_names),
            );
            if let Some(key) = &data.keys[function] {
                let node = nodes.get_mut(key).unwrap();
                node.callees = callees;
                node.lost_callees = lost;
            } else if !callees.is_empty() {
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
        sort_dedup(&mut node.callers);
        sort_dedup(&mut node.external_callers);
    }

    for (path, base, head) in &inputs.files {
        let file_nodes: Vec<_> = nodes
            .values()
            .filter(|node| {
                node.file == *path || node.base.as_ref().is_some_and(|span| span.file == *path)
            })
            .collect();
        if !file_nodes.is_empty() {
            let uncovered = similar::TextDiff::from_lines(
                base.as_deref().unwrap_or_default(),
                head.as_deref().unwrap_or_default(),
            )
            .iter_all_changes()
            .any(|change| {
                let (line, head_side) = match change.tag() {
                    similar::ChangeTag::Equal => return false,
                    similar::ChangeTag::Delete => (change.old_index(), false),
                    similar::ChangeTag::Insert => (change.new_index(), true),
                };
                line.is_some_and(|line| {
                    !file_nodes.iter().any(|node| {
                        let span = if head_side { &node.head } else { &node.base };
                        span.as_ref().is_some_and(|span| {
                            span.file == *path && (span.start_row..span.end_row).contains(&line)
                        })
                    })
                })
            });
            if !uncovered {
                continue;
            }
        }
        label_paths.insert(path.clone());
        let node = file_node(path, base.as_ref(), head.as_ref());
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
    ) -> (Vec<String>, Vec<String>) {
        let caller = self.files[file];
        let mut callees = Vec::new();
        let mut lost = Vec::new();
        for call in &caller.calls[function] {
            let name = &call.name;
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
                    callees.push((*key).to_string());
                    continue;
                }
                if let Some(targets) = targets {
                    let mut unresolved = targets.is_empty();
                    for (target_file, target_function) in targets {
                        if let Some(key) = &self.files[target_file].keys[target_function] {
                            callees.push(key.clone());
                        } else {
                            unresolved = true;
                        }
                    }
                    if unresolved {
                        lost.push(name.clone());
                    }
                    continue;
                }
                // A typed but unresolved receiver must not bind to an
                // unrelated same-name method in the current file.
                lost.push(name.clone());
                continue;
            }
            let mut unresolved = false;
            if let Some(local) = caller.by_name.get(name) {
                // Local overload sets shadow every other file, including
                // when none of their definitions belongs to the review graph.
                for &target in local {
                    if let Some(key) = &caller.keys[target] {
                        callees.push(key.clone());
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
                        if let Some(key) = &self.files[target.file].keys[target.function] {
                            callees.push(key.clone());
                        } else {
                            unresolved = true;
                        }
                    }
                    [] => {
                        let fallback = removed
                            .and_then(|index| index.names.get(&(caller.family, name.as_str())));
                        match fallback.map(Vec::as_slice).unwrap_or(&[]) {
                            [key] => callees.push((*key).to_string()),
                            _ => unresolved = true,
                        }
                    }
                    _ => unresolved = true,
                }
            }
            if unresolved {
                lost.push(name.clone());
            }
        }
        sort_dedup(&mut callees);
        sort_dedup(&mut lost);
        (callees, lost)
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
        for child in &nodes[&key].implementations {
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
            .chain(&nodes[&key].contracts)
            .cloned()
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

/// One visible row of the review tree.
#[derive(Debug, Clone)]
pub struct TreeRow {
    pub key: String,
    pub depth: usize,
    pub has_children: bool,
}

/// DFS rows starting from entry points (or a single forced root).
/// `focus` hides unchanged subtrees that contain no change anywhere below.
pub fn build_rows(
    a: &Analysis,
    roots: &[String],
    expanded: &HashSet<String>,
    focus: bool,
) -> Vec<TreeRow> {
    let mut rows = Vec::new();
    let mut visited_global: HashSet<String> = HashSet::new();

    for root in roots {
        if !a.nodes.contains_key(root) {
            continue;
        }
        dfs(a, root, 0, expanded, focus, &mut rows, &mut visited_global);
    }
    rows
}

fn dfs(
    a: &Analysis,
    key: &str,
    depth: usize,
    expanded: &HashSet<String>,
    focus: bool,
    rows: &mut Vec<TreeRow>,
    visited_global: &mut HashSet<String>,
) {
    let Some(node) = a.nodes.get(key) else { return };
    if focus && !node.relevant {
        return;
    }
    let first_visit = visited_global.insert(key.to_string());
    let children = children_of(a, key, focus);
    let has_children = !children.is_empty();
    rows.push(TreeRow {
        key: key.to_string(),
        depth,
        has_children,
    });
    if !expanded.contains(key) || !has_children {
        return;
    }
    for child in children {
        if first_visit {
            dfs(a, &child, depth + 1, expanded, focus, rows, visited_global);
        } else {
            // already rendered elsewhere in this pass: reference, don't recurse
            rows.push(TreeRow {
                key: child.clone(),
                depth: depth + 1,
                has_children: false,
            });
        }
    }
}

pub fn children_of(a: &Analysis, key: &str, focus: bool) -> Vec<String> {
    let Some(n) = a.nodes.get(key) else {
        return vec![];
    };
    let mut kids: Vec<String> = n
        .callees
        .iter()
        .chain(n.implementations.iter())
        .filter(|c| a.nodes.contains_key(*c))
        .filter(|c| !focus || a.nodes.get(*c).map(|x| x.relevant).unwrap_or(true))
        .cloned()
        .collect();
    kids.sort_by(|x, y| {
        let nx = &a.nodes[x];
        let ny = &a.nodes[y];
        status_rank(nx.status)
            .cmp(&status_rank(ny.status))
            .then_with(|| nx.relevant.cmp(&ny.relevant).reverse())
            .then_with(|| x.cmp(y))
    });
    kids.dedup();
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

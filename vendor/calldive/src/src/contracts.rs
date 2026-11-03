//! Explicit type, inheritance and method-contract dependencies for review.
//!
//! This deliberately does not infer structural implementations or variable types.
//! An unresolved or ambiguous type never contributes an inheritance edge.

use std::collections::{HashMap, HashSet};

use crate::syntax::{FnDef, Lang, SymbolKind};

pub type SymbolId = (usize, usize);

pub struct ContractFile<'a> {
    pub family: Lang,
    pub symbols: &'a [FnDef],
}

pub struct ContractGraph {
    /// A base type's derived types, a type's members, or a method's overrides.
    /// Also records implementation headers and types inheriting an implementation.
    pub implementations: HashMap<SymbolId, Vec<SymbolId>>,
    /// The exact reverse of `implementations`.
    pub contracts: HashMap<SymbolId, Vec<SymbolId>>,
    types: Vec<TypeInfo>,
    members: HashMap<SymbolId, MemberInfo>,
    names: HashMap<(Lang, String), Vec<usize>>,
    simple_names: HashMap<(Lang, String), Vec<usize>>,
    families: Vec<Lang>,
    scopes: Vec<Vec<String>>,
}

struct TypeInfo {
    id: SymbolId,
    family: Lang,
    qual: String,
    impl_header: bool,
    implementation_of: Option<usize>,
    bases: Vec<usize>,
    derived: Vec<usize>,
    members: Vec<SymbolId>,
}

struct MemberInfo {
    name: String,
    arity: Option<usize>,
    dispatch: bool,
    is_static: bool,
    kind: SymbolKind,
    owner: usize,
    trait_owner: Option<usize>,
    constructor: bool,
}

impl ContractGraph {
    pub fn new(files: &[ContractFile<'_>]) -> Self {
        let mut graph = Self {
            implementations: HashMap::new(),
            contracts: HashMap::new(),
            types: Vec::new(),
            members: HashMap::new(),
            names: HashMap::new(),
            simple_names: HashMap::new(),
            families: files.iter().map(|file| file.family.family()).collect(),
            scopes: files
                .iter()
                .map(|file| file.symbols.iter().map(lexical_scope).collect())
                .collect(),
        };
        let mut containers = HashMap::new();
        for (file, parsed) in files.iter().enumerate() {
            let family = parsed.family.family();
            for (symbol, definition) in parsed.symbols.iter().enumerate() {
                if definition.kind != SymbolKind::Type {
                    continue;
                }
                let impl_header =
                    family == Lang::Rust && rust_impl_header(&definition.qual).is_some();
                let qual = if impl_header {
                    definition.qual.clone()
                } else {
                    type_name(&definition.qual)
                };
                let index = graph.types.len();
                containers.insert((file, definition.start_byte), index);
                if !impl_header {
                    graph
                        .names
                        .entry((family, qual.clone()))
                        .or_default()
                        .push(index);
                    graph
                        .simple_names
                        .entry((family, leaf(&qual).to_owned()))
                        .or_default()
                        .push(index);
                }
                graph.types.push(TypeInfo {
                    id: (file, symbol),
                    family,
                    qual,
                    impl_header,
                    implementation_of: None,
                    bases: Vec::new(),
                    derived: Vec::new(),
                    members: Vec::new(),
                });
            }
        }
        for index in 0..graph.types.len() {
            let ty = &graph.types[index];
            let (file, symbol) = ty.id;
            let header = ty.impl_header.then(|| rust_impl_header(&ty.qual)).flatten();
            let scope = header.map_or_else(|| parent(&ty.qual), |(scope, _, _)| scope);
            let bases = files[file].symbols[symbol]
                .bases
                .iter()
                .filter_map(|base| graph.resolve_type(file, ty.family, base, scope))
                .filter(|&base| base != index)
                .collect();
            let implementation_of =
                header.and_then(|(_, owner, _)| graph.resolve_type(file, ty.family, owner, scope));
            graph.types[index].bases = bases;
            graph.types[index].implementation_of = implementation_of;
        }
        // An impl header is a distinct source-review node, not a second type
        // definition. Its explicit trait also establishes the concrete type's
        // dispatch relationship, including an empty default-method impl.
        for index in 0..graph.types.len() {
            if let Some(owner) = graph.types[index].implementation_of {
                for position in 0..graph.types[index].bases.len() {
                    let base = graph.types[index].bases[position];
                    if base != owner {
                        graph.types[owner].bases.push(base);
                    }
                }
            }
        }
        for (file, parsed) in files.iter().enumerate() {
            let family = parsed.family.family();
            for (symbol, definition) in parsed.symbols.iter().enumerate() {
                if definition.kind == SymbolKind::Type {
                    continue;
                }
                let Some(owner) = definition.owner.as_deref() else {
                    continue;
                };
                let rust_impl = if family == Lang::Rust {
                    trait_impl(owner)
                } else {
                    None
                };
                let (owner_name, scope, trait_name) = match rust_impl {
                    Some((scope, ty, tr)) => (ty, scope, Some(tr)),
                    None => (owner, "", None),
                };
                let concrete = graph.resolve_type(file, family, owner_name, scope);
                let trait_owner =
                    trait_name.and_then(|name| graph.resolve_type(file, family, name, scope));
                let container = definition
                    .container_start
                    .and_then(|start| containers.get(&(file, start)).copied());
                let owner = match container {
                    Some(header) if graph.types[header].impl_header => concrete.or(container),
                    _ => container.or(concrete),
                };
                let Some(owner) = owner else { continue };
                let id = (file, symbol);
                let name = leaf(&definition.name).to_owned();
                let constructor = is_constructor(family, &name, leaf(&graph.types[owner].qual));
                graph.types[owner].members.push(id);
                if let Some(container) = container.filter(|&container| container != owner) {
                    graph.types[container].members.push(id);
                }
                graph.members.insert(
                    id,
                    MemberInfo {
                        name,
                        arity: definition.arity,
                        dispatch: definition.dispatch,
                        is_static: definition.is_static,
                        kind: definition.kind,
                        owner,
                        trait_owner,
                        constructor,
                    },
                );
            }
        }
        for index in 0..graph.types.len() {
            graph.types[index].bases.sort_unstable();
            graph.types[index].bases.dedup();
            let id = graph.types[index].id;
            if let Some(container) = files[id.0].symbols[id.1]
                .container_start
                .and_then(|start| containers.get(&(id.0, start)).copied())
            {
                graph.add_edge(graph.types[container].id, id);
            }
            if let Some(owner) = graph.types[index].implementation_of {
                graph.add_edge(graph.types[owner].id, id);
            }
            for position in 0..graph.types[index].bases.len() {
                let base = graph.types[index].bases[position];
                graph.types[base].derived.push(index);
                graph.add_edge(graph.types[base].id, id);
            }
            for position in 0..graph.types[index].members.len() {
                let member = graph.types[index].members[position];
                graph.add_edge(id, member);
            }
        }
        graph.build_method_edges();
        for edges in graph
            .implementations
            .values_mut()
            .chain(graph.contracts.values_mut())
        {
            edges.sort_unstable();
            edges.dedup();
        }
        graph
    }

    /// Resolve members only inside a known receiver's explicit type hierarchy.
    /// `Some([])` intentionally forbids a fallback to unrelated same-name symbols.
    /// `function` supplies lexical scope; `dispatch = false` binds explicit base calls.
    pub fn receiver_targets(
        &self,
        file: usize,
        function: usize,
        receiver: &str,
        name: &str,
        dispatch: bool,
    ) -> Option<Vec<SymbolId>> {
        let family = *self.families.get(file)?;
        let caller_scope = self.scopes.get(file)?.get(function)?;
        // Rust's fully qualified trait syntax must not select an inherent method
        // or an implementation of another trait with the same method name.
        if family == Lang::Rust {
            if let Some((scope, ty, tr)) = trait_impl(receiver) {
                let scope = if scope.is_empty() {
                    caller_scope.as_str()
                } else {
                    scope
                };
                let owner = self.resolve_type(file, family, ty, scope)?;
                let contract = self.resolve_type(file, family, tr, scope)?;
                if owner != contract && !self.reachable(owner, false).contains(&contract) {
                    return Some(Vec::new());
                }
                return Some(self.lookup_members(owner, leaf(name), Some(contract)));
            }
        }
        let owner = self.resolve_type(file, family, receiver, caller_scope)?;
        let name = leaf(name);
        let mut targets = self.lookup_members(owner, name, None);
        if !dispatch {
            return Some(targets);
        }
        // A non-virtual method does not become polymorphic merely because a
        // subclass declares a same-name method. Inherited virtual contracts do
        // allow a C#/C++ implementation whose own syntax omits `virtual`.
        let dynamic: Vec<_> = self
            .reachable(owner, false)
            .into_iter()
            .flat_map(|ty| self.types[ty].members.iter().copied())
            .filter(|id| {
                let method = &self.members[id];
                method.name == name
                    && method.dispatch
                    && !method.is_static
                    && !method.constructor
                    && targets.iter().any(|target| {
                        let candidate = &self.members[target];
                        !candidate.is_static
                            && !candidate.constructor
                            && compatible_arity(candidate.arity, method.arity)
                    })
            })
            .collect();
        if !dynamic.is_empty() {
            for derived in self
                .reachable(owner, true)
                .into_iter()
                .filter(|&ty| ty != owner)
            {
                for contract in &dynamic {
                    let method = &self.members[contract];
                    let trait_owner = (family == Lang::Rust).then_some(method.owner);
                    targets.extend(self.lookup_matching(
                        derived,
                        name,
                        method.arity,
                        trait_owner,
                        true,
                    ));
                }
            }
        }
        targets.sort_unstable();
        targets.dedup();
        Some(targets)
    }

    fn build_method_edges(&mut self) {
        let mut pending: Vec<_> = self
            .members
            .iter()
            .filter_map(|(&id, method)| {
                (method.dispatch && !method.is_static && !method.constructor).then_some(id)
            })
            .collect();
        pending.sort_unstable();
        let mut processed = HashSet::new();
        while let Some(contract) = pending.pop() {
            if !processed.insert(contract) {
                continue;
            }
            let method = &self.members[&contract];
            let owner = method.owner;
            let name = method.name.clone();
            let arity = method.arity;
            let trait_owner = (self.types[owner].family == Lang::Rust)
                .then_some(method.trait_owner.unwrap_or(owner));
            let descendants = self.reachable(owner, true);
            let descendant_set: HashSet<_> = descendants.iter().copied().collect();
            for derived in descendants.into_iter().filter(|&ty| ty != owner) {
                for implementation in self.lookup_matching(derived, &name, arity, trait_owner, true)
                {
                    let candidate = &self.members[&implementation];
                    let is_descendant = descendant_set.contains(&candidate.owner);
                    let inherited = !self.types[derived].members.contains(&implementation);
                    // A second unrelated abstract interface is not an
                    // implementation. An inherited concrete base method is.
                    if candidate.kind == SymbolKind::Declaration && !is_descendant {
                        continue;
                    }
                    if inherited && candidate.kind == SymbolKind::Function {
                        // Keep an unchanged leaf type visible when it inherits
                        // an affected implementation; never link to its owner.
                        self.add_edge(implementation, self.types[derived].id);
                    }
                    if implementation == contract {
                        continue;
                    }
                    self.add_edge(contract, implementation);
                    // An unannotated override still propagates an inherited
                    // virtual contract. Process each such method only once.
                    if is_descendant && !processed.contains(&implementation) {
                        pending.push(implementation);
                    }
                }
            }
        }
    }

    fn add_edge(&mut self, contract: SymbolId, implementation: SymbolId) {
        if contract != implementation {
            self.implementations
                .entry(contract)
                .or_default()
                .push(implementation);
            self.contracts
                .entry(implementation)
                .or_default()
                .push(contract);
        }
    }

    fn resolve_type(
        &self,
        file: usize,
        family: Lang,
        reference: &str,
        scope: &str,
    ) -> Option<usize> {
        let reference = reference.trim();
        let absolute = reference.starts_with("::")
            || reference.starts_with("global::")
            || (family == Lang::Rust && reference.starts_with("crate::"));
        let mut name = type_name(reference);
        if name.is_empty() {
            return None;
        }
        let mut scope = if absolute {
            "".to_owned()
        } else {
            type_name(scope)
        };
        let mut anchored = absolute;
        if family == Lang::Rust {
            if name.starts_with("self::") {
                name.drain(.."self::".len());
                anchored = true;
            }
            while name.starts_with("super::") {
                scope.truncate(parent(&scope).len());
                name.drain(.."super::".len());
                anchored = true;
            }
        }
        loop {
            let qualified = if scope.is_empty() {
                name.clone()
            } else {
                format!("{scope}::{name}")
            };
            if let Some(candidates) = self.names.get(&(family, qualified)) {
                // An ambiguous lexical match must not escape to a more distant
                // namespace and accidentally select a different definition.
                return self.unique_type(candidates.iter().copied(), file);
            }
            if scope.is_empty() || anchored {
                break;
            }
            scope.truncate(parent(&scope).len());
        }
        if anchored {
            return None;
        }
        let candidates = self.simple_names.get(&(family, leaf(&name).to_owned()))?;
        self.unique_type(
            candidates.iter().copied().filter(|&index| {
                let qual = &self.types[index].qual;
                qual == &name
                    || qual
                        .strip_suffix(&name)
                        .is_some_and(|prefix| prefix.ends_with("::"))
            }),
            file,
        )
    }

    fn unique_type(&self, candidates: impl Iterator<Item = usize>, file: usize) -> Option<usize> {
        let mut local = None;
        let mut local_ambiguous = false;
        let mut global = None;
        let mut global_ambiguous = false;
        for candidate in candidates {
            if self.types[candidate].id.0 == file {
                local_ambiguous |= local.is_some();
                local = Some(candidate);
            }
            global_ambiguous |= global.is_some();
            global = Some(candidate);
        }
        if local.is_some() {
            (!local_ambiguous).then_some(local).flatten()
        } else {
            (!global_ambiguous).then_some(global).flatten()
        }
    }

    /// Includes the start type; iterative traversal also handles malformed cycles.
    fn reachable(&self, start: usize, derived: bool) -> Vec<usize> {
        let mut seen = HashSet::new();
        let mut pending = vec![start];
        let mut result = Vec::new();
        while let Some(ty) = pending.pop() {
            if !seen.insert(ty) {
                continue;
            }
            result.push(ty);
            pending.extend(if derived {
                &self.types[ty].derived
            } else {
                &self.types[ty].bases
            });
        }
        result
    }

    fn lookup_members(
        &self,
        owner: usize,
        name: &str,
        trait_owner: Option<usize>,
    ) -> Vec<SymbolId> {
        let mut arities: Vec<_> = self
            .reachable(owner, false)
            .into_iter()
            .flat_map(|ty| self.types[ty].members.iter().copied())
            .filter_map(|id| {
                let member = &self.members[&id];
                (member.name == name && self.matches_trait(member, trait_owner))
                    .then_some(member.arity)
            })
            .collect();
        arities.sort_unstable();
        arities.dedup();
        let mut result = Vec::new();
        for arity in arities {
            result.extend(self.lookup_matching(owner, name, arity, trait_owner, false));
        }
        result.sort_unstable();
        result.dedup();
        result
    }

    fn lookup_matching(
        &self,
        owner: usize,
        name: &str,
        arity: Option<usize>,
        trait_owner: Option<usize>,
        instance_only: bool,
    ) -> Vec<SymbolId> {
        let mut seen = HashSet::new();
        let mut pending = vec![owner];
        let mut result = Vec::new();
        while let Some(ty) = pending.pop() {
            if !seen.insert(ty) {
                continue;
            }
            let before = result.len();
            result.extend(self.types[ty].members.iter().copied().filter(|id| {
                let member = &self.members[id];
                member.name == name
                    && compatible_arity(member.arity, arity)
                    && self.matches_trait(member, trait_owner)
                    && (!instance_only || (!member.is_static && !member.constructor))
            }));
            if before == result.len() {
                pending.extend(&self.types[ty].bases);
            }
        }
        result.sort_unstable();
        result.dedup();
        result
    }

    fn matches_trait(&self, member: &MemberInfo, trait_owner: Option<usize>) -> bool {
        trait_owner
            .is_none_or(|contract| member.trait_owner == Some(contract) || member.owner == contract)
    }
}

fn lexical_scope(definition: &FnDef) -> String {
    let owner = definition
        .owner
        .as_deref()
        .unwrap_or_else(|| parent(&definition.qual));
    if let Some((scope, ty, _)) = trait_impl(owner) {
        if scope.is_empty() {
            type_name(ty)
        } else {
            type_name(&format!("{scope}::{ty}"))
        }
    } else {
        type_name(owner)
    }
}

fn compatible_arity(left: Option<usize>, right: Option<usize>) -> bool {
    left.is_none() || right.is_none() || left == right
}

fn parent(name: &str) -> &str {
    name.rsplit_once("::").map_or("", |(scope, _)| scope)
}

fn leaf(name: &str) -> &str {
    name.rsplit([':', '.']).next().unwrap_or(name)
}

/// Rust implementation review nodes preserve their explicit header identity.
fn rust_impl_header(owner: &str) -> Option<(&str, &str, Option<&str>)> {
    let header = owner.trim().strip_suffix('>')?;
    let open = header.find('<')?;
    let scope = header[..open].trim_end_matches("::");
    let inner = header[open + 1..].trim();
    let (ty, contract) = inner
        .rsplit_once(" as ")
        .map_or((inner, None), |(ty, contract)| {
            (ty.trim(), Some(contract.trim()))
        });
    Some((scope, ty, contract))
}

fn trait_impl(owner: &str) -> Option<(&str, &str, &str)> {
    let (scope, ty, contract) = rust_impl_header(owner)?;
    Some((scope, ty, contract?))
}

fn type_name(raw: &str) -> String {
    let mut name = String::with_capacity(raw.len());
    let mut depth = 0usize;
    for ch in raw.trim().trim_start_matches(['&', '*']).chars() {
        match ch {
            '<' | '[' => depth += 1,
            '>' | ']' => depth = depth.saturating_sub(1),
            '.' if depth == 0 => name.push_str("::"),
            ch if depth == 0 && !ch.is_whitespace() => name.push(ch),
            _ => {}
        }
    }
    let start = name.len()
        - name
            .trim_start_matches("global::")
            .trim_start_matches("::")
            .len();
    if start != 0 {
        name.drain(..start);
    }
    if name.starts_with("crate::") {
        name.drain(.."crate::".len());
    }
    name
}

fn is_constructor(family: Lang, name: &str, owner: &str) -> bool {
    match family {
        Lang::Python => matches!(name, "__init__" | "__new__"),
        Lang::TypeScript => name == "constructor",
        Lang::CSharp | Lang::Cpp => name == owner || name.strip_prefix('~') == Some(owner),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::{parse_file, ParsedFile};

    fn graph(parsed: &[(&str, ParsedFile)]) -> ContractGraph {
        ContractGraph::new(
            &parsed
                .iter()
                .map(|(path, file)| ContractFile {
                    family: crate::syntax::lang_for_path(path).unwrap().family(),
                    symbols: &file.fns,
                })
                .collect::<Vec<_>>(),
        )
    }

    fn symbol(parsed: &[(&str, ParsedFile)], file: usize, qual: &str) -> SymbolId {
        (
            file,
            parsed[file]
                .1
                .fns
                .iter()
                .position(|definition| definition.qual == qual)
                .unwrap_or_else(|| panic!("missing {qual}: {:?}", parsed[file].1.fns)),
        )
    }

    fn edge(graph: &ContractGraph, from: SymbolId, to: SymbolId) -> bool {
        graph
            .implementations
            .get(&from)
            .is_some_and(|targets| targets.contains(&to))
    }

    fn caller(graph: &ContractGraph, file: usize) -> usize {
        graph
            .members
            .keys()
            .filter(|id| id.0 == file)
            .map(|id| id.1)
            .min()
            .unwrap()
    }

    #[test]
    fn interfaces_abstract_overrides_and_unrelated_methods() {
        let parsed = [(
            "a.cs",
            parse_file(
                "a.cs",
                r#"
interface IWorker { void Run(int value); }
abstract class Base : IWorker { public abstract void Run(int value); }
class Worker : Base { public override void Run(int value) {} }
class Leaf : Worker {}
class Unrelated { public void Run(int value) {} }
"#,
            )
            .unwrap(),
        )];
        let graph = graph(&parsed);
        let interface = symbol(&parsed, 0, "IWorker::Run");
        let abstract_method = symbol(&parsed, 0, "Base::Run");
        let implementation = symbol(&parsed, 0, "Worker::Run");
        let unrelated = symbol(&parsed, 0, "Unrelated::Run");
        assert!(edge(
            &graph,
            symbol(&parsed, 0, "IWorker"),
            symbol(&parsed, 0, "Base")
        ));
        assert!(edge(&graph, symbol(&parsed, 0, "Worker"), implementation));
        assert!(edge(&graph, interface, abstract_method));
        assert!(edge(&graph, interface, implementation));
        assert!(edge(&graph, abstract_method, implementation));
        assert!(edge(&graph, implementation, symbol(&parsed, 0, "Leaf")));
        assert!(!edge(&graph, interface, unrelated));
        let targets = graph
            .receiver_targets(0, caller(&graph, 0), "IWorker", "Run", true)
            .unwrap();
        assert!(targets.contains(&interface) && targets.contains(&implementation));
        assert!(!targets.contains(&unrelated));
        assert!(graph.contracts[&implementation].contains(&interface));
    }

    #[test]
    fn lexical_namespace_wins_across_files_and_qualified_receivers_work() {
        let parsed = [
            (
                "contract.cs",
                parse_file(
                    "contract.cs",
                    "namespace N { interface IWorker { void Run(); } }",
                )
                .unwrap(),
            ),
            (
                "worker.cs",
                parse_file(
                    "worker.cs",
                    r#"
namespace Other { interface IWorker { void Run(); } }
namespace N { class Worker : IWorker { public void Run() {} } }
"#,
                )
                .unwrap(),
            ),
        ];
        let graph = graph(&parsed);
        let correct = symbol(&parsed, 0, "N::IWorker");
        let wrong = symbol(&parsed, 1, "Other::IWorker");
        let worker = symbol(&parsed, 1, "N::Worker");
        assert!(edge(&graph, correct, worker));
        assert!(!edge(&graph, wrong, worker));
        assert_eq!(
            graph.receiver_targets(0, caller(&graph, 0), "N.Worker", "Run", true),
            Some(vec![symbol(&parsed, 1, "N::Worker::Run")])
        );
        assert_eq!(
            graph.receiver_targets(1, caller(&graph, 1), "N::Worker", "Missing", true),
            Some(Vec::new())
        );
        assert_eq!(
            graph.receiver_targets(1, caller(&graph, 1), "Unknown", "Run", true),
            None
        );
    }

    #[test]
    fn ambiguous_unqualified_types_never_select_arbitrary_contracts() {
        let parsed = [
            (
                "a.cs",
                parse_file("a.cs", "namespace A { interface IWorker { void Run(); } }").unwrap(),
            ),
            (
                "b.cs",
                parse_file("b.cs", "namespace B { interface IWorker { void Run(); } }").unwrap(),
            ),
            (
                "c.cs",
                parse_file("c.cs", "class Worker : IWorker { public void Run() {} }").unwrap(),
            ),
        ];
        let graph = graph(&parsed);
        let worker = symbol(&parsed, 2, "Worker");
        assert!(!edge(&graph, symbol(&parsed, 0, "A::IWorker"), worker));
        assert!(!edge(&graph, symbol(&parsed, 1, "B::IWorker"), worker));
        assert_eq!(
            graph.receiver_targets(2, caller(&graph, 2), "IWorker", "Run", true),
            None
        );
        assert!(graph
            .receiver_targets(2, caller(&graph, 2), "A.IWorker", "Run", true)
            .unwrap()
            .contains(&symbol(&parsed, 0, "A::IWorker::Run")));
    }

    #[test]
    fn inherited_implementation_satisfies_a_separate_interface_branch() {
        let parsed = [(
            "a.cs",
            parse_file(
                "a.cs",
                r#"
interface IWorker { void Run(); }
class Base { public void Run() {} }
class Worker : Base, IWorker {}
"#,
            )
            .unwrap(),
        )];
        let graph = graph(&parsed);
        let inherited = symbol(&parsed, 0, "Base::Run");
        assert!(edge(&graph, symbol(&parsed, 0, "IWorker::Run"), inherited));
        assert!(edge(&graph, inherited, symbol(&parsed, 0, "Worker")));
        let targets = graph
            .receiver_targets(0, caller(&graph, 0), "Worker", "Run", true)
            .unwrap();
        assert!(targets.contains(&inherited));
    }

    #[test]
    fn ordinary_and_static_methods_do_not_fan_out_and_arity_filters_overrides() {
        let parsed = [("a.cs", parse_file("a.cs", r#"
class Base { public void Plain() {} public static void Static() {} public virtual void Run(int value) {} public virtual void Hidden() {} }
class Child : Base { public void Plain() {} public static void Static() {} public void Run(int a, int b) {} public new static void Hidden() {} }
class Override : Base { public override void Run(int value) {} }
"#).unwrap())];
        let graph = graph(&parsed);
        let plain = symbol(&parsed, 0, "Base::Plain");
        let static_method = symbol(&parsed, 0, "Base::Static");
        assert_eq!(
            graph.receiver_targets(0, caller(&graph, 0), "Base", "Plain", true),
            Some(vec![plain])
        );
        assert_eq!(
            graph.receiver_targets(0, caller(&graph, 0), "Base", "Static", true),
            Some(vec![static_method])
        );
        let contract = symbol(&parsed, 0, "Base::Run");
        assert!(!edge(&graph, contract, symbol(&parsed, 0, "Child::Run")));
        assert!(edge(&graph, contract, symbol(&parsed, 0, "Override::Run")));
        let hidden = symbol(&parsed, 0, "Base::Hidden");
        let static_child = symbol(&parsed, 0, "Child::Hidden");
        assert!(!edge(&graph, hidden, static_child));
        assert_eq!(
            graph.receiver_targets(0, caller(&graph, 0), "Base", "Hidden", true),
            Some(vec![hidden])
        );
        assert_eq!(
            graph.receiver_targets(0, caller(&graph, 0), "Child", "Hidden", true),
            Some(vec![static_child])
        );
    }

    #[test]
    fn inherited_overloads_remain_available() {
        let parsed = [(
            "a.cs",
            parse_file(
                "a.cs",
                r#"
class Base { public void Run() {} }
class Child : Base { public void Run(int value) {} }
"#,
            )
            .unwrap(),
        )];
        let graph = graph(&parsed);
        let targets = graph
            .receiver_targets(0, caller(&graph, 0), "Child", "Run", true)
            .unwrap();
        assert!(targets.contains(&symbol(&parsed, 0, "Base::Run")));
        assert!(targets.contains(&symbol(&parsed, 0, "Child::Run")));
    }

    #[test]
    fn cycles_terminate_and_edges_are_sorted_unique_and_reversible() {
        let parsed = [(
            "a.ts",
            parse_file(
                "a.ts",
                r#"
class A extends B { run() {} }
class B extends A { run() {} }
class Child extends A {}
"#,
            )
            .unwrap(),
        )];
        let graph = graph(&parsed);
        assert!(!graph
            .receiver_targets(0, caller(&graph, 0), "Child", "run", true)
            .unwrap()
            .is_empty());
        assert_eq!(
            graph.receiver_targets(0, caller(&graph, 0), "Child", "missing", true),
            Some(Vec::new())
        );
        for (from, targets) in &graph.implementations {
            assert!(targets.windows(2).all(|pair| pair[0] < pair[1]));
            assert!(!targets.contains(from));
            for to in targets {
                assert!(graph.contracts[to].contains(from));
            }
        }
    }

    #[test]
    fn python_inherited_methods_and_constructor_exclusion() {
        let parsed = [(
            "a.py",
            parse_file(
                "a.py",
                r#"
class Base:
    def __init__(self): pass
    def run(self): pass
class Child(Base):
    def __init__(self): pass
class Unrelated:
    def run(self): pass
"#,
            )
            .unwrap(),
        )];
        let graph = graph(&parsed);
        assert_eq!(
            graph.receiver_targets(0, caller(&graph, 0), "Child", "run", true),
            Some(vec![symbol(&parsed, 0, "Base::run")])
        );
        assert_eq!(
            graph.receiver_targets(0, caller(&graph, 0), "Base", "__init__", true),
            Some(vec![symbol(&parsed, 0, "Base::__init__")])
        );
        assert!(!edge(
            &graph,
            symbol(&parsed, 0, "Base::__init__"),
            symbol(&parsed, 0, "Child::__init__")
        ));
    }

    #[test]
    fn rust_trait_impls_do_not_claim_inherent_same_name_methods() {
        let parsed = [(
            "a.rs",
            parse_file(
                "a.rs",
                r#"
trait Job { fn run(&self); fn defaulted(&self) {} }
struct Worker;
impl Job for Worker { fn run(&self) {} }
impl Worker { fn run(&self) {} }
struct Unrelated;
impl Unrelated { fn run(&self) {} }
"#,
            )
            .unwrap(),
        )];
        let graph = graph(&parsed);
        let contract = symbol(&parsed, 0, "Job::run");
        let implementation = symbol(&parsed, 0, "<Worker as Job>::run");
        assert!(edge(&graph, contract, implementation));
        assert!(!edge(&graph, contract, symbol(&parsed, 0, "Worker::run")));
        assert!(!edge(
            &graph,
            contract,
            symbol(&parsed, 0, "Unrelated::run")
        ));
        assert_eq!(
            graph.receiver_targets(0, caller(&graph, 0), "<Worker as Job>", "run", true),
            Some(vec![implementation])
        );
        assert_eq!(
            graph.receiver_targets(0, caller(&graph, 0), "<Worker as Job>", "defaulted", true),
            Some(vec![symbol(&parsed, 0, "Job::defaulted")])
        );
        assert_eq!(
            graph.receiver_targets(0, caller(&graph, 0), "Worker", "defaulted", true),
            Some(vec![symbol(&parsed, 0, "Job::defaulted")])
        );
    }

    #[test]
    fn language_families_do_not_cross_link_and_tsx_shares_typescript() {
        let parsed = [
            (
                "a.ts",
                parse_file("a.ts", "interface Worker { run(): void; }").unwrap(),
            ),
            (
                "b.tsx",
                parse_file("b.tsx", "class Child implements Worker { run() {} }").unwrap(),
            ),
            (
                "c.cs",
                parse_file("c.cs", "class Other : Worker { public void run() {} }").unwrap(),
            ),
        ];
        let graph = graph(&parsed);
        assert!(edge(
            &graph,
            symbol(&parsed, 0, "Worker"),
            symbol(&parsed, 1, "Child")
        ));
        assert!(!edge(
            &graph,
            symbol(&parsed, 0, "Worker"),
            symbol(&parsed, 2, "Other")
        ));
    }

    #[test]
    fn same_file_preference_and_unique_global_fallback_are_conservative() {
        let parsed = [
            (
                "a.cs",
                parse_file(
                    "a.cs",
                    r#"
using A;
namespace A { interface IWorker { void Run(); } }
class Worker : IWorker { public void Run() {} }
"#,
                )
                .unwrap(),
            ),
            (
                "b.cs",
                parse_file("b.cs", "namespace B { interface IWorker { void Run(); } }").unwrap(),
            ),
            (
                "c.cs",
                parse_file(
                    "c.cs",
                    "using C; class Consumer : IOnly { public void Work() {} }",
                )
                .unwrap(),
            ),
            (
                "d.cs",
                parse_file("d.cs", "namespace C { interface IOnly { void Work(); } }").unwrap(),
            ),
        ];
        let graph = graph(&parsed);
        assert!(edge(
            &graph,
            symbol(&parsed, 0, "A::IWorker"),
            symbol(&parsed, 0, "Worker")
        ));
        assert!(!edge(
            &graph,
            symbol(&parsed, 1, "B::IWorker"),
            symbol(&parsed, 0, "Worker")
        ));
        assert!(edge(
            &graph,
            symbol(&parsed, 3, "C::IOnly"),
            symbol(&parsed, 2, "Consumer")
        ));
        assert_eq!(
            graph.receiver_targets(0, caller(&graph, 0), "IWorker", "Run", true),
            Some(vec![
                symbol(&parsed, 0, "A::IWorker::Run"),
                symbol(&parsed, 0, "Worker::Run"),
            ])
        );
        assert_eq!(
            graph.receiver_targets(2, caller(&graph, 2), "IWorker", "Run", true),
            None
        );
    }

    #[test]
    fn cpp_qualified_inheritance_and_unannotated_virtual_overrides() {
        let parsed = [
            (
                "base.hpp",
                parse_file(
                    "base.hpp",
                    r#"
namespace N { struct Base { virtual void Run() = 0; }; struct Mid : public Base {}; }
namespace Other { struct Base { virtual void Run() = 0; }; }
"#,
                )
                .unwrap(),
            ),
            (
                "worker.cpp",
                parse_file(
                    "worker.cpp",
                    r#"
namespace N { struct Worker : public Mid { void Run() {} }; struct Leaf : Worker {}; }
"#,
                )
                .unwrap(),
            ),
        ];
        let graph = graph(&parsed);
        let contract = symbol(&parsed, 0, "N::Base::Run");
        let implementation = symbol(&parsed, 1, "N::Worker::Run");
        assert!(edge(&graph, contract, implementation));
        assert!(!edge(
            &graph,
            symbol(&parsed, 0, "Other::Base::Run"),
            implementation
        ));
        assert!(edge(&graph, implementation, symbol(&parsed, 1, "N::Leaf")));
        assert_eq!(
            graph.receiver_targets(1, caller(&graph, 1), "N::Leaf", "Run", true),
            Some(vec![implementation])
        );
    }

    #[test]
    fn rust_supertraits_and_empty_default_implementations_are_explicit() {
        let parsed = [(
            "a.rs",
            parse_file(
                "a.rs",
                r#"
trait Root { fn run(&self); }
trait Child: Root { fn defaulted(&self) {} }
struct Worker;
impl Root for Worker { fn run(&self) {} }
impl Child for Worker {}
"#,
            )
            .unwrap(),
        )];
        let graph = graph(&parsed);
        assert!(edge(
            &graph,
            symbol(&parsed, 0, "Root"),
            symbol(&parsed, 0, "Child")
        ));
        assert!(edge(
            &graph,
            symbol(&parsed, 0, "Child"),
            symbol(&parsed, 0, "Worker")
        ));
        let header = symbol(&parsed, 0, "<Worker as Child>");
        assert!(edge(&graph, symbol(&parsed, 0, "Child"), header));
        assert!(edge(&graph, symbol(&parsed, 0, "Worker"), header));
        assert!(edge(&graph, symbol(&parsed, 0, "Child::defaulted"), header));
        assert!(graph
            .receiver_targets(0, caller(&graph, 0), "Child", "run", true)
            .unwrap()
            .contains(&symbol(&parsed, 0, "<Worker as Root>::run")));
        assert_eq!(
            graph.receiver_targets(0, caller(&graph, 0), "<Worker as Child>", "defaulted", true),
            Some(vec![symbol(&parsed, 0, "Child::defaulted")])
        );
    }

    #[test]
    fn go_does_not_infer_structural_interface_implementations() {
        let parsed = [(
            "a.go",
            parse_file(
                "a.go",
                r#"
package p
type Worker interface { Run() }
type Concrete struct {}
func (c Concrete) Run() {}
"#,
            )
            .unwrap(),
        )];
        let graph = graph(&parsed);
        let contract = symbol(&parsed, 0, "Worker::Run");
        let implementation = symbol(&parsed, 0, "Concrete::Run");
        assert!(!edge(&graph, contract, implementation));
        assert!(!edge(
            &graph,
            symbol(&parsed, 0, "Worker"),
            symbol(&parsed, 0, "Concrete")
        ));
        assert_eq!(
            graph.receiver_targets(0, caller(&graph, 0), "Worker", "Run", true),
            Some(vec![contract])
        );
        assert_eq!(
            graph.receiver_targets(0, caller(&graph, 0), "Concrete", "Run", true),
            Some(vec![implementation])
        );
    }

    #[test]
    fn same_line_rust_impl_headers_own_only_their_actual_members() {
        let parsed = [(
            "a.rs",
            parse_file(
                "a.rs",
                "struct Worker; impl Worker { fn one(&self) {} } impl Worker { fn two(&self) {} }",
            )
            .unwrap(),
        )];
        let graph = graph(&parsed);
        let headers: Vec<_> = parsed[0]
            .1
            .fns
            .iter()
            .enumerate()
            .filter_map(|(index, definition)| {
                (definition.kind == SymbolKind::Type && definition.qual == "<Worker>")
                    .then_some((0, index))
            })
            .collect();
        assert_eq!(headers.len(), 2);
        let one = symbol(&parsed, 0, "Worker::one");
        let two = symbol(&parsed, 0, "Worker::two");
        assert!(edge(&graph, headers[0], one));
        assert!(!edge(&graph, headers[0], two));
        assert!(edge(&graph, headers[1], two));
        assert!(!edge(&graph, headers[1], one));
        let worker = symbol(&parsed, 0, "Worker");
        assert!(edge(&graph, worker, headers[0]) && edge(&graph, worker, headers[1]));
        assert_eq!(
            graph.receiver_targets(0, caller(&graph, 0), "Worker", "one", true),
            Some(vec![one])
        );
    }

    #[test]
    fn same_name_partial_class_members_use_exact_source_containers() {
        let parsed = [(
            "a.cs",
            parse_file(
                "a.cs",
                "partial class Worker { void One() {} } partial class Worker { void Two() {} }",
            )
            .unwrap(),
        )];
        let graph = graph(&parsed);
        let types: Vec<_> = parsed[0]
            .1
            .fns
            .iter()
            .enumerate()
            .filter_map(|(index, definition)| {
                (definition.kind == SymbolKind::Type && definition.qual == "Worker")
                    .then_some((0, index))
            })
            .collect();
        assert_eq!(types.len(), 2);
        let one = symbol(&parsed, 0, "Worker::One");
        let two = symbol(&parsed, 0, "Worker::Two");
        assert!(edge(&graph, types[0], one) && !edge(&graph, types[0], two));
        assert!(edge(&graph, types[1], two) && !edge(&graph, types[1], one));
        assert_eq!(
            graph.receiver_targets(0, caller(&graph, 0), "Worker", "One", true),
            None
        );
    }

    #[test]
    fn rust_super_and_crate_paths_are_anchored() {
        let parsed = [(
            "a.rs",
            parse_file(
                "a.rs",
                r#"
trait Root { fn run(&self); }
mod outer {
    trait Base { fn run(&self); }
    mod inner { trait Local: super::Base {} trait Global: crate::Root {} }
}
"#,
            )
            .unwrap(),
        )];
        let graph = graph(&parsed);
        assert!(edge(
            &graph,
            symbol(&parsed, 0, "outer::Base"),
            symbol(&parsed, 0, "outer::inner::Local")
        ));
        assert!(edge(
            &graph,
            symbol(&parsed, 0, "Root"),
            symbol(&parsed, 0, "outer::inner::Global")
        ));
    }

    #[test]
    fn typed_receivers_resolve_in_the_callers_lexical_namespace() {
        let parsed = [(
            "a.cs",
            parse_file(
                "a.cs",
                r#"
namespace A {
    interface IWorker { void Run(); }
    class Worker : IWorker { public void Run() {} }
    class Client { public void Use(IWorker worker) { worker.Run(); } }
}
namespace B {
    interface IWorker { void Run(); }
    class Worker : IWorker { public void Run() {} }
    class Client { public void Use(IWorker worker) { worker.Run(); } }
}
"#,
            )
            .unwrap(),
        )];
        let graph = graph(&parsed);
        for namespace in ["A", "B"] {
            let function = symbol(&parsed, 0, &format!("{namespace}::Client::Use")).1;
            let call = parsed[0]
                .1
                .calls
                .iter()
                .find(|call| call.caller == function && call.name == "Run")
                .unwrap();
            let receiver = call.receiver.as_deref().unwrap();
            let targets = graph
                .receiver_targets(0, function, receiver, &call.name, call.dispatch)
                .unwrap();
            assert!(targets.contains(&symbol(&parsed, 0, &format!("{namespace}::IWorker::Run"))));
            assert!(targets.contains(&symbol(&parsed, 0, &format!("{namespace}::Worker::Run"))));
            let other = if namespace == "A" { "B" } else { "A" };
            assert!(!targets.contains(&symbol(&parsed, 0, &format!("{other}::IWorker::Run"))));
            assert!(!targets.contains(&symbol(&parsed, 0, &format!("{other}::Worker::Run"))));
        }
    }

    #[test]
    fn explicit_base_dispatch_does_not_select_virtual_overrides() {
        let parsed = [(
            "a.cpp",
            parse_file(
                "a.cpp",
                r#"
struct Base { virtual void Run() {} };
struct Child : Base { void Run() override {} void Call() { Base::Run(); } };
"#,
            )
            .unwrap(),
        )];
        let graph = graph(&parsed);
        let function = symbol(&parsed, 0, "Child::Call").1;
        let call = parsed[0]
            .1
            .calls
            .iter()
            .find(|call| call.caller == function && call.name == "Run")
            .unwrap();
        assert!(!call.dispatch);
        let receiver = call.receiver.as_deref().unwrap();
        let base = symbol(&parsed, 0, "Base::Run");
        let child = symbol(&parsed, 0, "Child::Run");
        assert_eq!(
            graph.receiver_targets(0, function, receiver, &call.name, call.dispatch),
            Some(vec![base])
        );
        assert!(graph
            .receiver_targets(0, function, receiver, &call.name, true)
            .unwrap()
            .contains(&child));
    }
}

//! Tree-sitter based extraction of function definitions and call sites.
//!
//! Language support is intentionally small but covers the big ones:
//! Rust, Python, TypeScript/TSX, JavaScript/JSX, Go, C#, C, C++.
//!
//! Also captures let-bound closures/lambdas (Rust `let f = |x| ...`,
//! JS/TS `const f = (x) => ...`, Python `f = lambda x: ...`,
//! Go `f := func() ...`, C# `Func<int> f = () => ...`,
//! C++ `auto f = [] { ... }`) as named symbols so calls to the binding resolve.
//!
//! Identities retain containers and overload signatures; extraction uses a
//! stack-safe pre-order cursor and reuses one parser per language per thread.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use tree_sitter::{Parser, Tree};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    Rust,
    Python,
    TypeScript,
    Tsx,
    JavaScript,
    Go,
    CSharp,
    C,
    Cpp,
}

pub fn lang_for_path(path: &str) -> Option<Lang> {
    // Uppercase .C conventionally denotes C++, unlike lowercase .c.
    if path.ends_with(".C") {
        return Some(Lang::Cpp);
    }
    let p = path.to_ascii_lowercase();
    if p.ends_with(".rs") {
        Some(Lang::Rust)
    } else if p.ends_with(".py") {
        Some(Lang::Python)
    } else if p.ends_with(".tsx") {
        Some(Lang::Tsx)
    } else if p.ends_with(".ts") || p.ends_with(".mts") || p.ends_with(".cts") {
        Some(Lang::TypeScript)
    } else if p.ends_with(".js")
        || p.ends_with(".jsx")
        || p.ends_with(".mjs")
        || p.ends_with(".cjs")
    {
        Some(Lang::JavaScript)
    } else if p.ends_with(".go") {
        Some(Lang::Go)
    } else if p.ends_with(".cs") {
        Some(Lang::CSharp)
    } else if p.ends_with(".c") {
        Some(Lang::C)
    } else if matches!(
        p.rsplit('.').next(),
        Some(
            "h" | "cc"
                | "cpp"
                | "cxx"
                | "c++"
                | "hh"
                | "hpp"
                | "hxx"
                | "h++"
                | "ipp"
                | "inl"
                | "tpp"
        )
    ) {
        // .h is ambiguous; the C++ grammar also handles ordinary C headers.
        Some(Lang::Cpp)
    } else {
        None
    }
}

impl Lang {
    pub fn family(self) -> Lang {
        match self {
            Lang::JavaScript | Lang::Tsx => Lang::TypeScript,
            Lang::C => Lang::Cpp,
            other => other,
        }
    }

    pub fn family_pathspecs(self) -> &'static [&'static str] {
        match self.family() {
            Lang::Rust => &[":(glob)**/*.rs"],
            Lang::Python => &[":(glob)**/*.py"],
            Lang::TypeScript => &[
                ":(glob)**/*.ts",
                ":(glob)**/*.tsx",
                ":(glob)**/*.mts",
                ":(glob)**/*.cts",
                ":(glob)**/*.js",
                ":(glob)**/*.jsx",
                ":(glob)**/*.mjs",
                ":(glob)**/*.cjs",
            ],
            Lang::Go => &[":(glob)**/*.go"],
            Lang::CSharp => &[":(glob)**/*.cs"],
            Lang::Cpp => &[
                ":(glob)**/*.c",
                ":(glob)**/*.C",
                ":(glob)**/*.h",
                ":(glob)**/*.cc",
                ":(glob)**/*.cpp",
                ":(glob)**/*.cxx",
                ":(glob)**/*.c++",
                ":(glob)**/*.hh",
                ":(glob)**/*.hpp",
                ":(glob)**/*.hxx",
                ":(glob)**/*.h++",
                ":(glob)**/*.ipp",
                ":(glob)**/*.inl",
                ":(glob)**/*.tpp",
            ],
            Lang::JavaScript | Lang::Tsx | Lang::C => unreachable!(),
        }
    }
}

thread_local! {
    static PARSERS: RefCell<HashMap<Lang, Parser>> = RefCell::new(HashMap::new());
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum SymbolKind {
    Function,
    Declaration,
    Type,
    Value,
    File,
}

impl SymbolKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Declaration => "declaration",
            Self::Type => "type",
            Self::Value => "value",
            Self::File => "file",
        }
    }
}

/// A callable, contract declaration, or type/implementation header in one revision.
#[derive(Debug, Clone)]
pub struct FnDef {
    pub name: String,
    /// Container path ending in the simple function name.
    pub qual: String,
    /// Whitespace-normalized parameter/declarator suffix for overload identity.
    pub sig: String,
    /// 0-based start row (inclusive)
    pub start_row: usize,
    /// 0-based end row (exclusive)
    pub end_row: usize,
    pub start_byte: usize,
    pub end_byte: usize,
    /// Exact same-file owning type/impl header, excluding nested callables.
    pub container_start: Option<usize>,
    /// Hash of the function's full source text (whitespace-normalized),
    /// excluding the name so pure renames keep their identity.
    pub body_hash: u64,
    /// lives inside a test module / test file (convention-based)
    pub is_test: bool,
    pub kind: SymbolKind,
    /// Qualified enclosing container; only Type symbols denote actual types.
    pub owner: Option<String>,
    /// Explicit inherited types, or the implemented trait for Rust methods.
    pub bases: Vec<String>,
    /// Number of parameters, excluding the receiver.
    pub arity: Option<usize>,
    pub dispatch: bool,
    pub is_static: bool,
    /// Sorted multiset of meaningful leaf-token hashes, excluding this name.
    pub tokens: Vec<u64>,
    /// Exact normalized source with same-name identifier leaves removed.
    pub rename_hash: u64,
    /// Operator/control structure must survive a speculative rename-plus-edit.
    pub body_shape: Option<u64>,
    pub body_tokens: usize,
}

/// A call or data/member reference made inside a symbol.
#[derive(Debug, Clone)]
pub struct CallSite {
    /// index into the same file's `fns` vec
    pub caller: usize,
    /// simple name of the callee or referenced member/value (last path segment)
    pub name: String,
    /// Statically identified receiver type, never an unknown variable name.
    pub receiver: Option<String>,
    /// False for syntactically explicit base/super/nonvirtual dispatch.
    pub dispatch: bool,
    /// True when this site is a data/property/field/constant reference rather than a call.
    pub is_value_ref: bool,
}

#[derive(Debug, Default, Clone)]
pub struct ParsedFile {
    pub fns: Vec<FnDef>,
    pub calls: Vec<CallSite>,
    pub value_refs: Vec<CallSite>,
    /// True when the file contains executable top-level statements (scripts).
    pub is_script: bool,
}

/// File-level test convention (independent of AST).
fn file_is_test_by_convention(path: &str, lang: Lang) -> bool {
    let p = path.to_ascii_lowercase();
    let base = p.rsplit('/').next().unwrap_or(&p);
    let in_tests = p.starts_with("tests/") || p.contains("/tests/");
    let in_test = in_tests || p.starts_with("test/") || p.contains("/test/");
    let language_test = match lang {
        Lang::Rust => in_tests,
        Lang::Python => in_test || base == "conftest.py",
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => in_test || base.contains(".spec."),
        Lang::CSharp => {
            // Match the PascalCase suffix, not words such as Contest/Latest.
            let original_base = path.rsplit('/').next().unwrap_or(path);
            let stem = original_base
                .rsplit_once('.')
                .map_or(original_base, |(stem, _)| stem);
            in_tests
                || matches!(stem, "test" | "tests")
                || stem.ends_with("Test")
                || stem.ends_with("Tests")
        }
        Lang::C | Lang::Cpp => {
            let stem = base.rsplit_once('.').map_or(base, |(stem, _)| stem);
            in_tests
                || stem.starts_with("test_")
                || stem.ends_with("_test")
                || stem.ends_with("_tests")
        }
        _ => false,
    };
    p.ends_with("_test.go")
        || (base.starts_with("test_") && base.ends_with(".py"))
        || base.ends_with("_test.py")
        || base.contains(".test.")
        || p.contains("__tests__/")
        || language_test
}

pub fn file_is_test_path(path: &str) -> bool {
    lang_for_path(path).is_some_and(|lang| file_is_test_by_convention(path, lang))
}

pub fn parse_file(path: &str, src: &str) -> Option<ParsedFile> {
    let lang = lang_for_path(path)?;
    let tree = PARSERS.with(|cache| {
        let mut cache = cache.borrow_mut();
        let parser = cache.entry(lang).or_insert_with(|| {
            let mut parser = Parser::new();
            let language = match lang {
                Lang::Rust => tree_sitter_rust::LANGUAGE,
                Lang::Python => tree_sitter_python::LANGUAGE,
                Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
                Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX,
                Lang::JavaScript => tree_sitter_javascript::LANGUAGE,
                Lang::Go => tree_sitter_go::LANGUAGE,
                Lang::CSharp => tree_sitter_c_sharp::LANGUAGE,
                Lang::C => tree_sitter_c::LANGUAGE,
                Lang::Cpp => tree_sitter_cpp::LANGUAGE,
            };
            parser
                .set_language(&language.into())
                .expect("grammar version mismatch");
            parser
        });
        parser.parse(src, None)
    })?;
    Some(extract(
        &tree,
        src.as_bytes(),
        lang,
        file_is_test_by_convention(path, lang),
    ))
}

fn extract(tree: &Tree, src: &[u8], lang: Lang, file_is_test: bool) -> ParsedFile {
    let mut out = ParsedFile::default();
    let root = tree.root_node();

    fn is_fn_def(kind: &str) -> bool {
        matches!(
            kind,
            "function_item"                    // rust
                | "function_definition"        // python/c/c++
                | "function_declaration"       // ts/js/go
                | "generator_function_declaration" // js
                | "method_definition"          // ts/js
                | "method_declaration"         // go/c#
                | "constructor_declaration"    // c#
                | "destructor_declaration"     // c#
                | "operator_declaration"       // c#
                | "conversion_operator_declaration" // c#
                | "local_function_statement" // c#
                | "function_signature_item" // rust trait / extern
                | "function_signature" // ts overload / ambient
                | "method_signature" // ts interface
                | "abstract_method_signature" // ts abstract
                | "method_elem" // go interface
        )
    }

    /// let-bound closure/lambda binding name, if this node declares one.
    fn binding_name(node: tree_sitter::Node, lang: Lang) -> Option<tree_sitter::Node> {
        let kind = node.kind();
        if lang == Lang::C {
            return None;
        }
        // C# variable initializers are unnamed children, not a `value` field.
        if lang == Lang::CSharp {
            if kind != "variable_declarator" {
                return None;
            }
            let mut cursor = node.walk();
            node.named_children(&mut cursor).find(|n| {
                matches!(
                    unparenthesized(*n).kind(),
                    "lambda_expression" | "anonymous_method_expression"
                )
            })?;
            return node.child_by_field_name("name");
        }
        let mut value = node
            .child_by_field_name("value")
            .or_else(|| node.child_by_field_name("right"))?;
        // go: right is an expression_list wrapping the actual value
        if value.kind() == "expression_list" {
            let mut c = value.walk();
            if !c.goto_first_child() {
                return None;
            }
            value = c.node();
        }
        if lang == Lang::Cpp {
            value = unparenthesized(value);
        }
        let value_kind = value.kind();
        let is_closure = match lang {
            Lang::Rust => kind == "let_declaration" && value_kind == "closure_expression",
            Lang::Python => kind == "assignment" && value_kind == "lambda",
            Lang::Go => kind == "short_var_declaration" && value_kind == "func_literal",
            Lang::TypeScript | Lang::Tsx | Lang::JavaScript => {
                kind == "variable_declarator"
                    && matches!(
                        value_kind,
                        "arrow_function" | "function_expression" | "function"
                    )
            }
            Lang::CSharp => unreachable!(),
            Lang::C => false,
            Lang::Cpp => kind == "init_declarator" && value_kind == "lambda_expression",
        };
        if !is_closure {
            return None;
        }
        if lang == Lang::Cpp {
            return declarator_name(node.child_by_field_name("declarator")?);
        }
        if matches!(lang, Lang::Python) {
            // assignment: `left` is the target (identifier or pattern list)
            let left = node.child_by_field_name("left")?;
            if left.kind() == "identifier" {
                return Some(left);
            }
            let mut c = left.walk();
            if c.goto_first_child() {
                loop {
                    if c.node().kind() == "identifier" {
                        return Some(c.node());
                    }
                    if !c.goto_next_sibling() {
                        break;
                    }
                }
            }
            return None;
        }
        let name_field = match lang {
            Lang::Rust => "pattern",
            Lang::TypeScript | Lang::Tsx | Lang::JavaScript => "name",
            Lang::Go => "left",
            _ => unreachable!(),
        };
        let name_node = node.child_by_field_name(name_field)?;
        // go: left is an expression_list wrapping the identifier
        let name_node = if name_node.kind() == "expression_list" {
            let mut c = name_node.walk();
            if !c.goto_first_child() {
                return None;
            }
            c.node()
        } else {
            name_node
        };
        Some(name_node)
    }

    out.is_script = file_is_script(root, lang);

    // Both passes use cursors: deeply nested input must not consume the stack.
    let mut known_types = HashSet::new();
    visit_nodes(root, |node| {
        if is_type_node(node, lang) {
            let name = text_field(node, "name", src);
            if !name.is_empty() {
                known_types.insert(name);
            }
        }
    });
    let mut cursor = root.walk();
    let mut frames = Vec::new();
    let mut containers = Vec::<String>::new();
    let mut bindings = Vec::<(String, Option<String>)>::new();
    if lang == Lang::CSharp {
        let mut c = root.walk();
        let namespace = root
            .named_children(&mut c)
            .find(|n| n.kind() == "file_scoped_namespace_declaration");
        if let Some(namespace) = namespace {
            containers.push(text_field(namespace, "name", src));
        }
    }
    let mut current_fn: Option<usize> = None;
    let mut current_type: Option<usize> = None;
    let mut in_test = false;
    let mut script_fn: Option<usize> = None;
    let mut script_bounds: Option<(usize, usize, usize, usize)> = None;
    'walk: loop {
        let node = cursor.node();
        let kind = node.kind();
        let type_node = is_type_node(node, lang);
        let is_top_script_stmt = out.is_script
            && current_fn.is_none()
            && current_type.is_none()
            && node.parent().is_some_and(|p| p.id() == root.id())
            && is_script_statement(node, lang);
        let native_declaration = cpp_method_declaration(node, lang, current_type.is_some());
        let definition = is_fn_def(kind) || native_declaration.is_some();
        let binding = if definition || type_node {
            None
        } else {
            binding_name(node, lang)
        };
        let value_decl = if !definition && !type_node && binding.is_none() && current_fn.is_none() {
            value_symbol(node, src, lang, current_type.is_some())
        } else {
            None
        };
        let is_value = value_decl.is_some();
        let symbol_node = value_decl
            .as_ref()
            .map(|&(sym, _, _)| sym)
            .or(native_declaration)
            .unwrap_or(node);
        let name_node = if let Some(&(_, name_node, _)) = value_decl.as_ref() {
            Some(name_node)
        } else if type_node {
            if kind == "impl_item" {
                node.child_by_field_name("type").map(type_name_node)
            } else if kind == "type_definition" {
                node.child_by_field_name("declarator")
                    .and_then(declarator_name)
            } else {
                node.child_by_field_name("name")
            }
        } else if definition {
            if matches!(lang, Lang::C | Lang::Cpp) {
                node.child_by_field_name("declarator")
                    .and_then(declarator_name)
            } else if kind == "operator_declaration" {
                csharp_operator_name_node(node)
            } else if kind == "conversion_operator_declaration" {
                node.child_by_field_name("type")
            } else {
                node.child_by_field_name("name")
            }
        } else {
            binding
        };
        let scope = name_node.is_some()
            || matches!(
                kind,
                "block"
                    | "statement_block"
                    | "compound_statement"
                    | "declaration_list"
                    | "class_body"
                    | "interface_body"
                    | "module"
                    | "mod_item"
                    | "impl_item"
            );
        let is_go_method = lang == Lang::Go && kind == "method_declaration";
        let saved = (
            current_fn,
            current_type,
            in_test,
            containers.len(),
            (scope || is_go_method).then_some(bindings.len()),
        );
        if is_top_script_stmt {
            let s_byte = node.start_byte();
            let e_byte = trim_end(src, s_byte, node.end_byte());
            let s_row = node.start_position().row;
            let e_row = node.end_position().row + usize::from(node.end_position().column != 0);
            script_bounds = Some(match script_bounds {
                Some((sb, eb, sr, er)) => {
                    (sb.min(s_byte), eb.max(e_byte), sr.min(s_row), er.max(e_row))
                }
                None => (s_byte, e_byte, s_row, e_row),
            });
            let idx = *script_fn.get_or_insert_with(|| {
                let i = out.fns.len();
                out.fns.push(FnDef {
                    name: "<file>".to_string(),
                    qual: "<file>".to_string(),
                    owner: None,
                    kind: SymbolKind::File,
                    bases: Vec::new(),
                    dispatch: false,
                    is_static: true,
                    sig: String::new(),
                    arity: None,
                    container_start: None,
                    start_byte: s_byte,
                    end_byte: e_byte,
                    start_row: s_row,
                    end_row: e_row,
                    body_hash: 0,
                    tokens: Vec::new(),
                    rename_hash: 0,
                    body_shape: None,
                    body_tokens: 0,
                    is_test: file_is_test,
                });
                i
            });
            current_fn = Some(idx);
        }
        if lang == Lang::Rust && (kind == "mod_item" || name_node.is_some()) {
            in_test |= (kind == "mod_item" && text_field(node, "name", src) == "tests")
                || rust_attribute(node, src, true);
        }
        if lang == Lang::CSharp && type_node {
            in_test |= csharp_attribute(node, src, &["TestFixture", "TestClass"]);
        }
        if let Some(name_node) = name_node {
            let mut name = if let Some((_, _, Some(override_name))) = &value_decl {
                override_name.clone()
            } else {
                name_node.utf8_text(src).unwrap_or_default().to_string()
            };
            if lang == Lang::CSharp
                && matches!(
                    kind,
                    "operator_declaration" | "conversion_operator_declaration"
                )
            {
                name = format!("operator {name}");
            }
            if lang == Lang::Rust && kind == "impl_item" {
                let ty = owner_type(&text_field(node, "type", src));
                let tr = text_field(node, "trait", src);
                name = if tr.is_empty() {
                    format!("<{ty}>")
                } else {
                    format!("<{ty} as {tr}>")
                };
            }
            if kind == "destructor_declaration" {
                name.insert(0, '~');
            }
            if lang == Lang::Go
                && kind == "method_declaration"
                && let Some(param) = node
                    .child_by_field_name("receiver")
                    .and_then(|r| r.named_child(0))
            {
                containers.push(owner_type(&text_field(param, "type", src)));
            }
            if lang == Lang::Cpp
                && definition
                && let Some(parent) = name_node
                    .parent()
                    .filter(|p| p.kind() == "qualified_identifier")
            {
                let scope = text_field(parent, "scope", src);
                if !scope.is_empty() {
                    containers.push(scope);
                }
            }
            let owner = (!containers.is_empty()).then(|| containers.join("::"));
            let qual = owner
                .as_ref()
                .map_or_else(|| name.clone(), |o| format!("{o}::{name}"));
            let is_test = file_is_test
                || in_test
                || (lang == Lang::Rust
                    && kind == "function_item"
                    && rust_attribute(node, src, false))
                || (lang == Lang::CSharp
                    && csharp_attribute(
                        node,
                        src,
                        &["Fact", "Theory", "Test", "TestMethod", "TestCase"],
                    ));
            let symbol_kind = if is_value {
                SymbolKind::Value
            } else if type_node {
                SymbolKind::Type
            } else if native_declaration.is_some()
                || matches!(
                    kind,
                    "function_signature_item"
                        | "function_signature"
                        | "method_signature"
                        | "abstract_method_signature"
                        | "method_elem"
                )
                || (lang == Lang::CSharp
                    && definition
                    && node.child_by_field_name("body").is_none()
                    && !node_has_arrow_body(node))
            {
                SymbolKind::Declaration
            } else {
                SymbolKind::Function
            };
            let end = if type_node {
                type_header_end(node)
            } else {
                symbol_node.end_byte()
            };
            let end = trim_end(src, symbol_node.start_byte(), end);
            let bases = if type_node {
                type_bases(node, src, lang)
            } else if lang == Lang::Rust && !is_value {
                enclosing_impl_trait(node, src).into_iter().collect()
            } else {
                Vec::new()
            };
            let is_static = !type_node
                && (if is_value {
                    value_is_static(node, symbol_node, src, lang, current_type.is_some())
                } else {
                    method_static(node, symbol_node, src, lang, definition)
                });
            let dispatch = !type_node
                && !is_value
                && !is_static
                && method_dispatch(symbol_node, src, lang, current_type.is_some());
            let idx = out.fns.len();
            let (tokens, rename_hash, body_shape, body_tokens) =
                symbol_evidence(symbol_node, name_node, src, end);
            let sig = if type_node || is_value {
                String::new()
            } else {
                signature(node, name_node, src, lang, definition)
            };
            let arity = if type_node || is_value {
                None
            } else {
                parameter_arity(&sig, lang)
            };
            let container_start = if current_fn.is_none() {
                current_type.map(|i| out.fns[i].start_byte)
            } else {
                None
            };
            let end_row = if type_node {
                symbol_node.start_position().row
                    + src[symbol_node.start_byte()..end]
                        .iter()
                        .filter(|&&b| b == b'\n')
                        .count()
                    + 1
            } else {
                symbol_node.end_position().row + usize::from(symbol_node.end_position().column != 0)
            };
            out.fns.push(FnDef {
                name: name.clone(),
                qual,
                owner,
                kind: symbol_kind,
                bases,
                dispatch,
                is_static,
                sig,
                arity,
                container_start,
                start_byte: symbol_node.start_byte(),
                end_byte: end,
                start_row: symbol_node.start_position().row,
                end_row,
                body_hash: hash_range_without_name(symbol_node, name_node, src, end),
                tokens,
                rename_hash,
                body_shape,
                body_tokens,
                is_test,
            });
            if type_node {
                current_type = Some(idx);
                current_fn = None;
                collect_type_member_bindings(node, root, src, lang, &mut bindings);
            } else {
                current_fn = Some(idx);
                if is_go_method {
                    collect_go_receiver_member_bindings(node, root, src, &mut bindings);
                }
            }
            if lang == Lang::Rust
                && kind == "impl_item"
                && text_field(node, "trait", src).is_empty()
            {
                // Inherent method identities retain the concrete type owner.
                containers.push(owner_type(&text_field(node, "type", src)));
            } else {
                containers.push(name);
            }
        } else if let Some(container) = container_name(node, src, lang) {
            containers.push(container);
        } else if let Some(caller) = current_fn {
            if is_call(kind)
                && let Some(name) = callee_name(node, src)
            {
                let (receiver, dispatch) = call_receiver(
                    node,
                    src,
                    &bindings,
                    &known_types,
                    &out.fns[caller],
                    current_type.map(|i| &out.fns[i]),
                    lang,
                )
                .map_or((None, true), |(receiver, dispatch)| {
                    (Some(receiver), dispatch)
                });
                out.calls.push(CallSite {
                    caller,
                    name,
                    receiver,
                    dispatch,
                    is_value_ref: false,
                });
                collect_call_type_arg_refs(node, src, caller, &mut out.value_refs);
            } else if let Some((name, receiver)) = value_reference(
                node,
                src,
                &bindings,
                &known_types,
                &out.fns[caller],
                current_type.map(|i| &out.fns[i]),
                lang,
            ) {
                out.value_refs.push(CallSite {
                    caller,
                    name,
                    receiver,
                    dispatch: false,
                    is_value_ref: true,
                });
            }
        }
        if current_fn.is_some() {
            collect_binding(node, src, lang, &mut bindings);
        }
        frames.push(saved);
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            let (old_fn, old_type, old_test, old_len, binding_len) = frames.pop().unwrap();
            current_fn = old_fn;
            current_type = old_type;
            in_test = old_test;
            containers.truncate(old_len);
            if let Some(len) = binding_len {
                bindings.truncate(len);
            }
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                break 'walk;
            }
        }
    }
    if let (Some(idx), Some((sb, eb, sr, er))) = (script_fn, script_bounds) {
        let f = &mut out.fns[idx];
        f.start_byte = sb;
        f.end_byte = eb;
        f.start_row = sr;
        f.end_row = er;
        f.body_hash = hash_without_ranges(src, sb, eb, &[]);
        f.rename_hash = f.body_hash;
    }
    out
}

/// Iterative pre-order walk, also used for rename evidence tokens.
fn visit_nodes<'a>(root: tree_sitter::Node<'a>, mut visit: impl FnMut(tree_sitter::Node<'a>)) {
    let mut cursor = root.walk();
    loop {
        visit(cursor.node());
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return;
            }
        }
    }
}

fn is_type_node(node: tree_sitter::Node, lang: Lang) -> bool {
    match lang {
        Lang::Rust => matches!(
            node.kind(),
            "trait_item" | "struct_item" | "enum_item" | "union_item" | "impl_item" | "type_item"
        ),
        Lang::Python => node.kind() == "class_definition",
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => matches!(
            node.kind(),
            "class_declaration"
                | "abstract_class_declaration"
                | "class"
                | "interface_declaration"
                | "enum_declaration"
                | "type_alias_declaration"
        ),
        Lang::CSharp => matches!(
            node.kind(),
            "class_declaration"
                | "struct_declaration"
                | "interface_declaration"
                | "record_declaration"
                | "enum_declaration"
                | "delegate_declaration"
        ),
        Lang::Cpp => {
            matches!(
                node.kind(),
                "class_specifier"
                    | "struct_specifier"
                    | "union_specifier"
                    | "enum_specifier"
                    | "alias_declaration"
                    | "type_definition"
            ) && (node.kind() != "enum_specifier" || node.child_by_field_name("body").is_some())
        }
        Lang::Go => matches!(node.kind(), "type_spec" | "type_alias"),
        Lang::C => false,
    }
}

fn is_script_statement(node: tree_sitter::Node, lang: Lang) -> bool {
    match lang {
        Lang::CSharp => {
            node.kind() == "global_statement"
                && node
                    .named_child(0)
                    .is_none_or(|c| c.kind() != "local_function_statement")
        }
        Lang::Python => {
            matches!(
                node.kind(),
                "if_statement"
                    | "for_statement"
                    | "while_statement"
                    | "try_statement"
                    | "with_statement"
                    | "match_statement"
            ) || (node.kind() == "expression_statement"
                && node.named_child(0).is_some_and(|e| e.kind() == "call"))
        }
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => matches!(
            node.kind(),
            "expression_statement"
                | "if_statement"
                | "for_statement"
                | "for_in_statement"
                | "while_statement"
                | "try_statement"
                | "switch_statement"
        ),
        _ => false,
    }
}

fn file_is_script(root: tree_sitter::Node, lang: Lang) -> bool {
    let mut c = root.walk();
    root.named_children(&mut c)
        .any(|n| is_script_statement(n, lang))
}
pub fn is_ignorable_outside_line(path: &str, line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return true;
    }
    if t.starts_with("//") || t.starts_with("/*") || t.starts_with('*') || t.starts_with("*/") {
        return true;
    }
    let Some(lang) = lang_for_path(path) else {
        return false;
    };
    match lang {
        Lang::CSharp => {
            t.starts_with("using ")
                || t.starts_with("global using ")
                || t.starts_with("namespace ")
                || t.starts_with("#nullable")
                || t.starts_with("#pragma")
                || t.starts_with("#region")
                || t.starts_with("#endregion")
                || t.starts_with("#:")
                || matches!(t, "{" | "}")
        }
        Lang::Rust => {
            t.starts_with("use ")
                || t.starts_with("pub use ")
                || t.starts_with("pub(crate) use ")
                || t.starts_with("pub(super) use ")
                || t.starts_with("mod ")
                || t.starts_with("pub mod ")
                || t.starts_with("extern crate ")
                || matches!(t, "{" | "}")
        }
        Lang::Python => t.starts_with('#') || t.starts_with("import ") || t.starts_with("from "),
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => {
            t.starts_with("import ")
                || t.starts_with("export {")
                || t.starts_with("export *")
                || t.starts_with("export type {")
                || matches!(t, "{" | "}" | "};")
        }
        Lang::Go => t.starts_with("package ") || t.starts_with("import ") || t == ")",
        Lang::C | Lang::Cpp => {
            t.starts_with("#include")
                || t.starts_with("#pragma")
                || t.starts_with("#ifndef")
                || t.starts_with("#define")
                || t.starts_with("#endif")
                || t.starts_with("using namespace ")
                || t.starts_with("namespace ")
                || matches!(t, "{" | "}" | "};")
        }
    }
}

fn csharp_operator_name_node(node: tree_sitter::Node) -> Option<tree_sitter::Node> {
    let mut c = node.walk();
    let mut after_operator = false;
    for child in node.children(&mut c) {
        if child.kind() == "operator" {
            after_operator = true;
            continue;
        }
        if after_operator && child.kind() != "parameter_list" {
            return Some(child);
        }
    }
    None
}

fn node_has_arrow_body(node: tree_sitter::Node) -> bool {
    let mut c = node.walk();
    node.named_children(&mut c)
        .any(|n| n.kind() == "arrow_expression_clause")
}

/// Extract a non-callable value/property/field/constant/enum-member symbol when
/// `current_fn` is `None`. Returns `(symbol_span_node, name_node, optional_name_override)`.
fn value_symbol<'a>(
    node: tree_sitter::Node<'a>,
    src: &[u8],
    lang: Lang,
    in_type: bool,
) -> Option<(tree_sitter::Node<'a>, tree_sitter::Node<'a>, Option<String>)> {
    let kind = node.kind();
    match lang {
        Lang::CSharp => match kind {
            "property_declaration" | "event_declaration" | "enum_member_declaration" => {
                let name = node.child_by_field_name("name")?;
                Some((node, name, None))
            }
            "indexer_declaration" => {
                let params = node
                    .child_by_field_name("parameters")
                    .and_then(|p| p.utf8_text(src).ok())
                    .unwrap_or("[]");
                let name_node = node.child_by_field_name("parameters").unwrap_or(node);
                Some((node, name_node, Some(format!("this{params}"))))
            }
            "variable_declarator" => {
                let decl = node.parent()?;
                if decl.kind() != "variable_declaration" {
                    return None;
                }
                let outer = decl.parent()?;
                if !matches!(
                    outer.kind(),
                    "field_declaration" | "event_field_declaration"
                ) {
                    return None;
                }
                let name = node.child_by_field_name("name")?;
                let span = if decl.named_child_count() <= 2 {
                    outer
                } else {
                    node
                };
                Some((span, name, None))
            }
            _ => None,
        },
        Lang::Rust => match kind {
            "const_item" | "static_item" | "associated_type" | "enum_variant" => {
                let name = node.child_by_field_name("name")?;
                Some((node, name, None))
            }
            "field_declaration" if in_type => {
                if node
                    .parent()
                    .and_then(|p| p.parent())
                    .is_some_and(|gp| gp.kind() == "enum_variant")
                {
                    return None;
                }
                let name = node.child_by_field_name("name")?;
                Some((node, name, None))
            }
            _ => None,
        },
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => match kind {
            "public_field_definition"
            | "field_definition"
            | "property_signature"
            | "enum_assignment" => {
                let name = node
                    .child_by_field_name("name")
                    .or_else(|| node.child_by_field_name("property"))?;
                let val = node.child_by_field_name("value");
                if val.is_some_and(|v| {
                    matches!(
                        unparenthesized(v).kind(),
                        "arrow_function" | "function_expression" | "function"
                    )
                }) {
                    return None;
                }
                Some((node, name, None))
            }
            "property_identifier" if node.parent().is_some_and(|p| p.kind() == "enum_body") => {
                Some((node, node, None))
            }
            "variable_declarator" if !in_type => {
                let decl = node.parent()?;
                if !matches!(decl.kind(), "lexical_declaration" | "variable_declaration") {
                    return None;
                }
                let val = node.child_by_field_name("value");
                if val.is_some_and(|v| {
                    matches!(
                        unparenthesized(v).kind(),
                        "arrow_function" | "function_expression" | "function" | "class"
                    )
                }) {
                    return None;
                }
                let top = decl.parent()?;
                let outer = if top.kind() == "export_statement" {
                    top
                } else if top.kind() == "program"
                    || matches!(top.kind(), "statement_block" if top.parent().is_some_and(|p| matches!(p.kind(), "internal_module" | "module")))
                {
                    decl
                } else {
                    return None;
                };
                let name = node.child_by_field_name("name")?;
                if !matches!(name.kind(), "identifier" | "shorthand_property_identifier") {
                    return None;
                }
                let span = if decl.named_child_count() == 1 {
                    outer
                } else {
                    node
                };
                Some((span, name, None))
            }
            _ => None,
        },
        Lang::Python => {
            if kind != "assignment" {
                return None;
            }
            let stmt = node.parent()?;
            if stmt.kind() != "expression_statement" {
                return None;
            }
            let parent = stmt.parent()?;
            let top_or_class = parent.kind() == "module"
                || (in_type
                    && parent.kind() == "block"
                    && parent
                        .parent()
                        .is_some_and(|gp| gp.kind() == "class_definition"));
            if !top_or_class {
                return None;
            }
            let left = node.child_by_field_name("left")?;
            if left.kind() != "identifier" {
                return None;
            }
            Some((stmt, left, None))
        }
        Lang::Go => match kind {
            "const_spec" | "var_spec" if !in_type => {
                let name = node.child_by_field_name("name")?;
                let decl = node.parent();
                let span = match decl {
                    Some(d)
                        if matches!(d.kind(), "const_declaration" | "var_declaration")
                            && d.named_child_count() == 1 =>
                    {
                        d
                    }
                    _ => node,
                };
                Some((span, name, None))
            }
            "field_declaration" if in_type => {
                let name = node.child_by_field_name("name")?;
                Some((node, name, None))
            }
            _ => None,
        },
        Lang::C | Lang::Cpp => match kind {
            "enumerator" => {
                let name = node.child_by_field_name("name")?;
                Some((node, name, None))
            }
            "field_declaration" if in_type => {
                let declarator = node.child_by_field_name("declarator")?;
                if has_function_declarator(declarator) {
                    return None;
                }
                let name = declarator_name(declarator)?;
                Some((node, name, None))
            }
            "declaration" if !in_type => {
                let parent = node.parent()?;
                if !matches!(parent.kind(), "translation_unit" | "declaration_list") {
                    return None;
                }
                let declarator = node.child_by_field_name("declarator")?;
                if has_function_declarator(declarator) {
                    return None;
                }
                let name = declarator_name(declarator)?;
                Some((node, name, None))
            }
            _ => None,
        },
    }
}

fn has_function_declarator(node: tree_sitter::Node) -> bool {
    if node.kind() == "function_declarator" {
        return true;
    }
    let mut c = node.walk();
    for child in node.named_children(&mut c) {
        if has_function_declarator(child) {
            return true;
        }
    }
    false
}

fn value_is_static(
    node: tree_sitter::Node,
    symbol: tree_sitter::Node,
    src: &[u8],
    lang: Lang,
    in_type: bool,
) -> bool {
    if !in_type {
        return true;
    }
    let text =
        std::str::from_utf8(&src[symbol.start_byte()..symbol.end_byte()]).unwrap_or_default();
    let has = |word| {
        text.split(|c: char| !c.is_alphanumeric() && c != '_')
            .any(|w| w == word)
    };
    match lang {
        Lang::Rust => matches!(
            node.kind(),
            "const_item" | "static_item" | "enum_variant" | "associated_type"
        ),
        Lang::CSharp => node.kind() == "enum_member_declaration" || has("static") || has("const"),
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => {
            node.kind() == "enum_assignment"
                || node.parent().is_some_and(|p| p.kind() == "enum_body")
                || has("static")
        }
        Lang::Cpp | Lang::C => node.kind() == "enumerator" || has("static") || has("constexpr"),
        Lang::Python => true,
        Lang::Go => false,
    }
}

fn collect_type_member_bindings(
    type_node: tree_sitter::Node,
    root: tree_sitter::Node,
    src: &[u8],
    lang: Lang,
    bindings: &mut Vec<(String, Option<String>)>,
) {
    if lang == Lang::Rust && type_node.kind() == "impl_item" {
        let target = owner_type(&text_field(type_node, "type", src));
        let target_simple = target.rsplit("::").next().unwrap_or(&target);
        if !target_simple.is_empty() {
            let mut rc = root.walk();
            for item in root.named_children(&mut rc) {
                if item.kind() == "struct_item"
                    && owner_type(&text_field(item, "name", src)) == target_simple
                {
                    collect_type_body_bindings(item, src, lang, bindings);
                }
            }
        }
        return;
    }
    collect_type_body_bindings(type_node, src, lang, bindings);
}

fn collect_go_receiver_member_bindings(
    method_node: tree_sitter::Node,
    root: tree_sitter::Node,
    src: &[u8],
    bindings: &mut Vec<(String, Option<String>)>,
) {
    let Some(param) = method_node
        .child_by_field_name("receiver")
        .and_then(|r| r.named_child(0))
    else {
        return;
    };
    let recv_type = owner_type(&text_field(param, "type", src));
    let Some(recv_name_node) = param
        .child_by_field_name("name")
        .or_else(|| param.named_child(0).filter(|n| n.kind() == "identifier"))
    else {
        return;
    };
    let recv_name = recv_name_node.utf8_text(src).unwrap_or_default().trim();
    if recv_name.is_empty() || recv_type.is_empty() {
        return;
    }
    let mut rc = root.walk();
    for decl in root.named_children(&mut rc) {
        if decl.kind() != "type_declaration" {
            continue;
        }
        let mut dc = decl.walk();
        for spec in decl.named_children(&mut dc) {
            if spec.kind() == "type_spec"
                && owner_type(&text_field(spec, "name", src)) == recv_type
                && let Some(struct_ty) = spec
                    .child_by_field_name("type")
                    .filter(|t| t.kind() == "struct_type")
            {
                let mut sc = struct_ty.walk();
                for list in struct_ty
                    .named_children(&mut sc)
                    .filter(|n| n.kind() == "field_declaration_list")
                {
                    let mut fc = list.walk();
                    for field in list
                        .named_children(&mut fc)
                        .filter(|n| n.kind() == "field_declaration")
                    {
                        let ty = field
                            .child_by_field_name("type")
                            .map(|t| clean_type(t.utf8_text(src).unwrap_or_default()))
                            .filter(|t| !t.is_empty());
                        let mut nc = field.walk();
                        let mut has_explicit_name = false;
                        for name in field.children_by_field_name("name", &mut nc) {
                            has_explicit_name = true;
                            let fname = name.utf8_text(src).unwrap_or_default().trim();
                            if !fname.is_empty() {
                                bindings.push((format!("{recv_name}.{fname}"), ty.clone()));
                            }
                        }
                        // Embedded field: `type Server struct { Worker }` -> `s.Worker` has type `Worker`
                        if !has_explicit_name && let Some(ref embedded_ty) = ty {
                            let leaf = embedded_ty
                                .rsplit([':', '.'])
                                .next()
                                .unwrap_or(embedded_ty)
                                .split('[')
                                .next()
                                .unwrap_or(embedded_ty)
                                .trim();
                            if !leaf.is_empty() {
                                bindings.push((format!("{recv_name}.{leaf}"), Some(embedded_ty.clone())));
                            }
                        }
                    }
                }
            }
        }
    }
}

fn collect_type_body_bindings(
    type_node: tree_sitter::Node,
    src: &[u8],
    lang: Lang,
    bindings: &mut Vec<(String, Option<String>)>,
) {
    let mut c = type_node.walk();
    for child in type_node.named_children(&mut c) {
        if matches!(child.kind(), "parameter_list" | "formal_parameters") {
            let mut pc = child.walk();
            for param in child.named_children(&mut pc) {
                collect_binding(param, src, lang, bindings);
            }
        } else if matches!(
            child.kind(),
            "declaration_list" | "class_body" | "field_declaration_list" | "block"
        ) {
            let mut bc = child.walk();
            for member in child.named_children(&mut bc) {
                match member.kind() {
                    "field_declaration" if lang == Lang::CSharp => {
                        let mut mc = member.walk();
                        for sub in member.named_children(&mut mc) {
                            if sub.kind() == "variable_declaration" {
                                let declared_ty = sub
                                    .child_by_field_name("type")
                                    .map(|t| clean_type(t.utf8_text(src).unwrap_or_default()))
                                    .filter(|t| !t.is_empty() && t != "var");
                                let mut vc = sub.walk();
                                for decl in sub.named_children(&mut vc) {
                                    if decl.kind() == "variable_declarator"
                                        && let Some(n) = decl.child_by_field_name("name")
                                    {
                                        let ty = declared_ty.clone().or_else(|| {
                                            inferred_initializer_type(decl, src, lang)
                                        });
                                        let bname =
                                            n.utf8_text(src).unwrap_or_default().to_string();
                                        push_member_binding(bindings, &bname, ty, lang);
                                    }
                                }
                            }
                        }
                    }
                    "property_declaration" if lang == Lang::CSharp => {
                        let ty = member
                            .child_by_field_name("type")
                            .map(|t| clean_type(t.utf8_text(src).unwrap_or_default()))
                            .filter(|t| !t.is_empty());
                        if let Some(n) = member.child_by_field_name("name") {
                            let bname = n.utf8_text(src).unwrap_or_default().to_string();
                            push_member_binding(bindings, &bname, ty, lang);
                        }
                    }
                    "constructor_declaration" if lang == Lang::CSharp => {
                        collect_constructor_assigned_bindings(member, src, lang, bindings);
                    }
                    "field_declaration" if matches!(lang, Lang::Rust | Lang::Cpp) => {
                        let ty = member
                            .child_by_field_name("type")
                            .map(|t| clean_type(t.utf8_text(src).unwrap_or_default()))
                            .filter(|t| !t.is_empty() && t != "auto");
                        let name = member.child_by_field_name("name").or_else(|| {
                            member
                                .child_by_field_name("declarator")
                                .and_then(declarator_name)
                        });
                        if let Some(n) = name {
                            let bname = n.utf8_text(src).unwrap_or_default().to_string();
                            push_member_binding(bindings, &bname, ty, lang);
                        }
                    }
                    "public_field_definition" | "field_definition" => {
                        let ty = member
                            .child_by_field_name("type")
                            .map(|t| clean_type(t.utf8_text(src).unwrap_or_default()))
                            .filter(|t| !t.is_empty())
                            .or_else(|| inferred_initializer_type(member, src, lang));
                        if let Some(n) = member
                            .child_by_field_name("name")
                            .or_else(|| member.child_by_field_name("property"))
                        {
                            let bname = n.utf8_text(src).unwrap_or_default().to_string();
                            push_member_binding(bindings, &bname, ty, lang);
                        }
                    }
                    "method_definition" if text_field(member, "name", src) == "constructor" => {
                        if let Some(params) = member.child_by_field_name("parameters") {
                            let mut pc = params.walk();
                            for p in params.named_children(&mut pc) {
                                let prev_len = bindings.len();
                                collect_binding(p, src, lang, bindings);
                                for idx in prev_len..bindings.len() {
                                    let (bname, bty) = bindings[idx].clone();
                                    if !bname.contains('.') {
                                        bindings.push((format!("this.{bname}"), bty));
                                    }
                                }
                            }
                        }
                        collect_constructor_assigned_bindings(member, src, lang, bindings);
                    }
                    "expression_statement" if lang == Lang::Python => {
                        if let Some(assign) = member
                            .named_child(0)
                            .filter(|n| n.kind() == "assignment")
                            && let Some(left) = assign.child_by_field_name("left")
                            && left.kind() == "identifier"
                        {
                            let ty = assign
                                .child_by_field_name("type")
                                .map(|t| clean_type(t.utf8_text(src).unwrap_or_default()))
                                .filter(|t| !t.is_empty())
                                .or_else(|| inferred_initializer_type(assign, src, lang));
                            if ty.is_some() {
                                let bname = left.utf8_text(src).unwrap_or_default().to_string();
                                push_member_binding(bindings, &bname, ty, lang);
                            }
                        }
                    }
                    "function_definition"
                        if lang == Lang::Python
                            && text_field(member, "name", src) == "__init__" =>
                    {
                        collect_constructor_assigned_bindings(member, src, lang, bindings);
                    }
                    _ => {}
                }
            }
        }
    }
}

fn push_member_binding(
    bindings: &mut Vec<(String, Option<String>)>,
    name: &str,
    ty: Option<String>,
    lang: Lang,
) {
    if name.is_empty() {
        return;
    }
    if matches!(lang, Lang::CSharp | Lang::Cpp) {
        bindings.push((name.to_string(), ty.clone()));
        bindings.push((format!("this.{name}"), ty.clone()));
        if lang == Lang::Cpp {
            bindings.push((format!("this->{name}"), ty));
        }
    } else if matches!(lang, Lang::TypeScript | Lang::Tsx | Lang::JavaScript) {
        bindings.push((format!("this.{name}"), ty));
    } else if matches!(lang, Lang::Rust | Lang::Python) {
        bindings.push((format!("self.{name}"), ty));
    } else {
        bindings.push((name.to_string(), ty));
    }
}

fn collect_constructor_assigned_bindings(
    ctor_node: tree_sitter::Node,
    src: &[u8],
    lang: Lang,
    bindings: &mut Vec<(String, Option<String>)>,
) {
    let mut param_types = Vec::new();
    if let Some(params) = ctor_node
        .child_by_field_name("parameters")
        .or_else(|| {
            let mut c = ctor_node.walk();
            ctor_node
                .named_children(&mut c)
                .find(|n| matches!(n.kind(), "parameter_list" | "formal_parameters"))
        })
    {
        let mut pc = params.walk();
        for p in params.named_children(&mut pc) {
            collect_binding(p, src, lang, &mut param_types);
        }
    }
    let Some(body) = ctor_node.child_by_field_name("body") else {
        return;
    };
    visit_nodes(body, |node| {
        if !matches!(node.kind(), "assignment_expression" | "assignment") {
            return;
        }
        let Some(left) = node.child_by_field_name("left") else {
            return;
        };
        let Some(right) = node
            .child_by_field_name("right")
            .or_else(|| node.child_by_field_name("value"))
        else {
            return;
        };
        let left_text = left.utf8_text(src).unwrap_or_default().trim();
        let member_name = if let Some(rest) = left_text
            .strip_prefix("self.")
            .or_else(|| left_text.strip_prefix("this."))
            .or_else(|| left_text.strip_prefix("this->"))
        {
            (!rest.contains(['.', '-', '[', '('])).then_some(rest)
        } else if lang == Lang::CSharp && !left_text.contains(['.', '[', '(']) {
            Some(left_text)
        } else {
            None
        };
        let Some(member_name) = member_name else {
            return;
        };
        // Do not overwrite an already-annotated field type with None
        let right_text = right.utf8_text(src).unwrap_or_default().trim();
        let rhs_ty = param_types
            .iter()
            .rev()
            .find(|(pname, _)| pname == right_text)
            .and_then(|(_, pty)| pty.clone())
            .or_else(|| inferred_expr_type(right, src, lang));
        if let Some(ty) = rhs_ty {
            push_member_binding(bindings, member_name, Some(ty), lang);
        }
    });
}

/// Extract explicit target type from local/field initializers:
/// - `new Foo(...)` (C#, TS/JS, C++)
/// - `Foo { ... }` (Rust struct literal)
/// - `provider.GetRequiredService<IFoo>()` / `ActivatorUtilities.CreateInstance<Foo>(...)`
///   and explicit cast/service-locator type arguments (`resolve<IFoo>(...)`, `inject(Foo)`, `Foo(...)` in Python)
fn inferred_initializer_type(
    decl_node: tree_sitter::Node,
    src: &[u8],
    lang: Lang,
) -> Option<String> {
    if let Some(val) = decl_node
        .child_by_field_name("value")
        .or_else(|| decl_node.child_by_field_name("right"))
    {
        let expr = if val.kind() == "expression_list" {
            val.named_child(0)?
        } else {
            val
        };
        return inferred_expr_type(expr, src, lang);
    }
    if matches!(lang, Lang::CSharp | Lang::Cpp) {
        let mut c = decl_node.walk();
        let name_node = decl_node.child_by_field_name("name").or_else(|| {
            decl_node
                .child_by_field_name("declarator")
                .and_then(declarator_name)
        });
        for child in decl_node.named_children(&mut c) {
            if name_node.is_some_and(|n| n.id() == child.id())
                || matches!(
                    child.kind(),
                    "declarator"
                        | "pointer_declarator"
                        | "reference_declarator"
                        | "type_identifier"
                        | "bracketed_argument_list"
                )
            {
                continue;
            }
            if child.kind() == "equals_value_clause" {
                if let Some(inner) = child.named_child(0) {
                    return inferred_expr_type(inner, src, lang);
                }
            } else if let Some(ty) = inferred_expr_type(child, src, lang) {
                return Some(ty);
            }
        }
    }
    None
}

fn inferred_expr_type(mut expr: tree_sitter::Node, src: &[u8], lang: Lang) -> Option<String> {
    loop {
        expr = unparenthesized(expr);
        match expr.kind() {
            "await_expression" | "await" | "try_expression" | "non_null_expression" => {
                expr = expr.named_child(0)?;
            }
            "as_expression" | "type_assertion" | "satisfies_expression" => {
                let ty_node = expr.child_by_field_name("type").or_else(|| {
                    let mut c = expr.walk();
                    expr.named_children(&mut c).last()
                })?;
                let ty = clean_type(ty_node.utf8_text(src).unwrap_or_default());
                return (!ty.is_empty()).then_some(ty);
            }
            "cast_expression" => {
                let ty_node = expr.child_by_field_name("type")?;
                let ty = clean_type(ty_node.utf8_text(src).unwrap_or_default());
                return (!ty.is_empty()).then_some(ty);
            }
            _ => break,
        }
    }
    match expr.kind() {
        "object_creation_expression" | "new_expression" => {
            let ty_node = expr
                .child_by_field_name("type")
                .or_else(|| expr.child_by_field_name("constructor"))?;
            let ty = clean_type(ty_node.utf8_text(src).unwrap_or_default());
            (!ty.is_empty()).then_some(ty)
        }
        "struct_expression" if lang == Lang::Rust => {
            let ty_node = expr.child_by_field_name("name")?;
            let ty = clean_type(ty_node.utf8_text(src).unwrap_or_default());
            (!ty.is_empty()).then_some(ty)
        }
        "composite_literal" if lang == Lang::Go => {
            let ty_node = expr.child_by_field_name("type")?;
            let ty = clean_type(ty_node.utf8_text(src).unwrap_or_default());
            (!ty.is_empty()).then_some(ty)
        }
        kind if is_call(kind) => service_locator_or_ctor_call_type(expr, src, lang),
        _ => None,
    }
}

fn is_service_locator_method(name: &str) -> bool {
    matches!(
        name,
        "GetService"
            | "GetRequiredService"
            | "GetKeyedService"
            | "GetRequiredKeyedService"
            | "CreateInstance"
            | "GetInstance"
            | "Resolve"
            | "ResolveKeyed"
            | "ResolveOptional"
            | "TryResolve"
            | "get"
            | "resolve"
            | "getService"
            | "getRequiredService"
            | "inject"
            | "useContext"
            | "Invoke"
    )
}

fn first_explicit_call_type_arg(call: tree_sitter::Node, src: &[u8]) -> Option<String> {
    let expr = call
        .child_by_field_name("function")
        .or_else(|| call.child_by_field_name("type"))?;
    let mut cursor = expr.walk();
    let mut check_nodes = vec![call, expr];
    if let Some(last) = expr.named_children(&mut cursor).last() {
        check_nodes.push(last);
    }
    for target in check_nodes {
        let mut tc = target.walk();
        for child in target.named_children(&mut tc) {
            if matches!(
                child.kind(),
                "type_argument_list" | "type_arguments" | "template_argument_list"
            ) {
                let mut ac = child.walk();
                if let Some(arg) = child.named_children(&mut ac).next() {
                    let ty = clean_type(arg.utf8_text(src).unwrap_or_default());
                    if !ty.is_empty() {
                        return Some(ty);
                    }
                }
            }
        }
    }
    None
}

fn service_locator_or_ctor_call_type(
    call: tree_sitter::Node,
    src: &[u8],
    lang: Lang,
) -> Option<String> {
    let callee = callee_name(call, src)?;
    if is_service_locator_method(&callee) {
        if let Some(ty) = first_explicit_call_type_arg(call, src) {
            return Some(ty);
        }
        // Value-passed class/token: `container.get(ServiceA)`, `inject(ServiceA)`, `container.resolve(ServiceA)`
        if let Some(args) = call
            .child_by_field_name("arguments")
            .or_else(|| {
                let mut c = call.walk();
                call.named_children(&mut c)
                    .find(|n| matches!(n.kind(), "argument_list" | "arguments"))
            })
            && let Some(first_arg) = args.named_child(0)
        {
            let arg = if first_arg.kind() == "argument" {
                first_arg.named_child(0).unwrap_or(first_arg)
            } else {
                first_arg
            };
            let text = arg.utf8_text(src).unwrap_or_default().trim();
            // Also support C# `typeof(ServiceA)`
            let text = text
                .strip_prefix("typeof(")
                .and_then(|s| s.strip_suffix(')'))
                .map(str::trim)
                .unwrap_or(text);
            let leaf = text.rsplit([':', '.']).next().unwrap_or(text);
            if !leaf.is_empty()
                && leaf.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                && leaf.chars().all(|c| c.is_alphanumeric() || c == '_')
            {
                return Some(clean_type(text));
            }
        }
    }
    // Direct UpperCamelCase constructor / Rust `Type::new()` / Go `NewType(...)`
    let func_expr = call.child_by_field_name("function")?;
    if lang == Lang::Rust
        && matches!(callee.as_str(), "new" | "default")
        && matches!(func_expr.kind(), "scoped_identifier" | "generic_function")
    {
        let target = if func_expr.kind() == "generic_function" {
            func_expr.child_by_field_name("function")?
        } else {
            func_expr
        };
        if let Some(path_node) = target.child_by_field_name("path") {
            let ty = clean_type(path_node.utf8_text(src).unwrap_or_default());
            let leaf = ty.rsplit("::").next().unwrap_or(&ty);
            if leaf.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
                return Some(ty);
            }
        }
    }
    if matches!(lang, Lang::Python | Lang::Cpp) {
        let text = func_expr.utf8_text(src).unwrap_or_default().trim();
        let head = text.split(['<', '(']).next().unwrap_or(text).trim();
        let leaf = head.rsplit([':', '.']).next().unwrap_or(head);
        if !leaf.is_empty()
            && leaf.chars().next().is_some_and(|c| c.is_ascii_uppercase())
            && leaf.chars().all(|c| c.is_alphanumeric() || c == '_')
        {
            return Some(clean_type(text));
        }
    }
    None
}

fn collect_call_type_arg_refs(
    call: tree_sitter::Node,
    src: &[u8],
    caller: usize,
    out: &mut Vec<CallSite>,
) {
    let Some(expr) = call
        .child_by_field_name("function")
        .or_else(|| call.child_by_field_name("type"))
    else {
        return;
    };
    let mut cursor = call.walk();
    // Look for explicit type argument lists on the call or its function expression
    let mut check_nodes = vec![call, expr];
    if let Some(last) = expr.named_children(&mut cursor).last() {
        check_nodes.push(last);
    }
    for target in check_nodes {
        let mut tc = target.walk();
        for child in target.named_children(&mut tc) {
            if matches!(
                child.kind(),
                "type_argument_list" | "type_arguments" | "template_argument_list"
            ) {
                let mut ac = child.walk();
                for arg in child.named_children(&mut ac) {
                    let ty = owner_type(arg.utf8_text(src).unwrap_or_default());
                    let simple = ty.rsplit("::").next().unwrap_or(&ty);
                    let simple = simple.rsplit('.').next().unwrap_or(simple).trim();
                    if !simple.is_empty()
                        && simple
                            .chars()
                            .next()
                            .is_some_and(|c| c.is_ascii_uppercase())
                    {
                        out.push(CallSite {
                            caller,
                            name: simple.to_string(),
                            receiver: Some(ty),
                            dispatch: false,
                            is_value_ref: true,
                        });
                    }
                }
            }
        }
    }
}

fn value_reference(
    node: tree_sitter::Node,
    src: &[u8],
    bindings: &[(String, Option<String>)],
    known_types: &HashSet<String>,
    caller: &FnDef,
    enclosing_type: Option<&FnDef>,
    lang: Lang,
) -> Option<(String, Option<String>)> {
    let kind = node.kind();
    let member_access = matches!(
        kind,
        "member_expression"
            | "attribute"
            | "field_expression"
            | "selector_expression"
            | "member_access_expression"
            | "conditional_access_expression"
            | "scoped_identifier"
            | "qualified_identifier"
    );
    if !member_access
        && kind != "identifier"
        && !(kind == "struct_expression" && lang == Lang::Rust)
    {
        return None;
    }
    // Skip nodes that are the `function` or `type` target of a call expression,
    // or part of an outer member/qualified path (we only emit at the outermost member access
    // or standalone identifier).
    let mut anc = node.parent();
    while let Some(p) = anc {
        let pk = p.kind();
        if is_call(pk) {
            if p.child_by_field_name("function")
                .or_else(|| p.child_by_field_name("type"))
                .is_some_and(|f| {
                    node.start_byte() >= f.start_byte() && node.end_byte() <= f.end_byte()
                })
            {
                return None;
            }
            break;
        }
        if matches!(
            pk,
            "using_directive"
                | "use_declaration"
                | "import_statement"
                | "import_from_statement"
                | "import_clause"
                | "import_specifier"
                | "namespace_declaration"
                | "file_scoped_namespace_declaration"
        ) {
            return None;
        }
        if matches!(
            pk,
            "argument_list" | "arguments" | "block" | "statement_block" | "compound_statement"
        ) {
            break;
        }
        anc = p.parent();
    }

    if kind == "struct_expression" && lang == Lang::Rust {
        let ty = node
            .child_by_field_name("name")?
            .utf8_text(src)
            .ok()?
            .trim();
        let clean = owner_type(ty);
        let simple = clean.rsplit("::").next().unwrap_or(&clean).to_string();
        return Some((simple, Some(clean)));
    }

    if member_access {
        // If this node is the receiver/prefix of a larger qualified/scoped identifier
        // (e.g. `A::B` inside `A::B::C`), wait or let each segment resolve cleanly.
        if let Some(parent) = node.parent()
            && matches!(parent.kind(), "scoped_identifier" | "qualified_identifier")
            && parent
                .child_by_field_name("path")
                .or_else(|| parent.child_by_field_name("scope"))
                .is_some_and(|p| p.id() == node.id())
        {
            return None;
        }
        let name = simple_name(node, src)?;
        let qualified = matches!(kind, "scoped_identifier" | "qualified_identifier");
        let receiver_node = match kind {
            "member_expression" | "attribute" => node.child_by_field_name("object"),
            "field_expression" => node
                .child_by_field_name("value")
                .or_else(|| node.child_by_field_name("argument")),
            "selector_expression" => node.child_by_field_name("operand"),
            "member_access_expression" => node.child_by_field_name("expression"),
            "conditional_access_expression" => node.child_by_field_name("condition"),
            "scoped_identifier" => node.child_by_field_name("path"),
            "qualified_identifier" => node.child_by_field_name("scope"),
            _ => None,
        }?;
        let text = receiver_node.utf8_text(src).ok()?.trim();
        let binding = bindings.iter().rev().find(|(bname, _)| bname == text);
        if let Some((_, Some(ty))) = binding {
            return Some((name, Some(ty.clone())));
        }
        let self_receiver =
            (text == "self" && matches!(lang, Lang::Rust | Lang::Python) && !caller.is_static)
                || (text == "Self" && lang == Lang::Rust)
                || (text == "this"
                    && matches!(
                        lang,
                        Lang::CSharp | Lang::Cpp | Lang::TypeScript | Lang::Tsx | Lang::JavaScript
                    ));
        if self_receiver {
            let owner = caller
                .owner
                .as_deref()
                .map(receiver_owner)
                .or_else(|| enclosing_type.map(|t| receiver_owner(&t.qual)))?;
            return Some((name, Some(owner)));
        }
        if (text == "base" && lang == Lang::CSharp)
            || (text == "super" && matches!(lang, Lang::TypeScript | Lang::Tsx | Lang::JavaScript))
        {
            let base_ty = enclosing_type.and_then(|t| t.bases.first()).cloned()?;
            return Some((name, Some(base_ty)));
        }
        if binding.is_some() {
            return None;
        }
        let looks_like_type = text
            .rsplit([':', '.'])
            .next()
            .and_then(|s| s.chars().next())
            .is_some_and(|c| c.is_ascii_uppercase());
        if qualified || known_types.contains(text) || looks_like_type {
            return Some((name, Some(text.to_string())));
        }
        return None;
    }

    if kind == "identifier" {
        if let Some(parent) = node.parent() {
            let pk = parent.kind();
            // Exclude declaration names, member access right-hand names, parameter names, etc.
            if matches!(
                pk,
                "member_expression"
                    | "attribute"
                    | "field_expression"
                    | "selector_expression"
                    | "member_access_expression"
                    | "member_binding_expression"
                    | "scoped_identifier"
                    | "qualified_identifier"
                    | "qualified_name"
                    | "parameter"
                    | "parameter_declaration"
                    | "required_parameter"
                    | "optional_parameter"
                    | "variable_declarator"
                    | "let_declaration"
                    | "function_item"
                    | "function_definition"
                    | "function_declaration"
                    | "method_declaration"
                    | "property_declaration"
                    | "class_declaration"
                    | "struct_declaration"
                    | "record_declaration"
                    | "interface_declaration"
                    | "type_identifier"
            ) {
                // Allow RHS value of variable_declarator / let_declaration!
                let is_rhs = match pk {
                    "variable_declarator" => parent
                        .child_by_field_name("name")
                        .is_none_or(|n| n.id() != node.id()),
                    "let_declaration" => parent
                        .child_by_field_name("pattern")
                        .is_none_or(|n| n.id() != node.id()),
                    _ => false,
                };
                if !is_rhs {
                    return None;
                }
            }
        }
        let text = node.utf8_text(src).ok()?.trim();
        if text.is_empty()
            || matches!(
                text,
                "true" | "false" | "null" | "None" | "self" | "this" | "base" | "super"
            )
        {
            return None;
        }
        if bindings.iter().any(|(bname, _)| bname == text) {
            return None;
        }
        let owner = caller
            .owner
            .as_deref()
            .map(receiver_owner)
            .or_else(|| enclosing_type.map(|t| receiver_owner(&t.qual)));
        return Some((text.to_string(), owner));
    }

    None
}

fn trim_end(src: &[u8], start: usize, mut end: usize) -> usize {
    while end > start && src[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    end
}

fn type_header_end(node: tree_sitter::Node) -> usize {
    if let Some(body) = node.child_by_field_name("body") {
        return body.start_byte();
    }
    if node.kind() == "type_spec"
        && let Some(ty) = node.child_by_field_name("type")
    {
        let mut c = ty.walk();
        let brace = ty.children(&mut c).find(|n| n.kind() == "{");
        if let Some(brace) = brace {
            return brace.start_byte();
        }
    }
    node.end_byte()
}

fn clean_type(text: &str) -> String {
    let mut ty = text.trim().trim_start_matches(':').trim();
    loop {
        let mut next = ty.trim_start_matches(['&', '*']).trim_start();
        next = next
            .strip_prefix("mut ")
            .or_else(|| next.strip_prefix("const "))
            .or_else(|| next.strip_prefix("dyn "))
            .or_else(|| next.strip_prefix("impl "))
            .unwrap_or(next);
        if next.starts_with('\'') {
            next = next
                .split_once(char::is_whitespace)
                .map_or(next, |(_, tail)| tail.trim_start());
        }
        // Unwrap smart-pointer / DI-handle wrappers (`Box<dyn T>`, `Arc<T>`, `Rc<T>`,
        // `std::unique_ptr<T>`, `std::shared_ptr<T>`, `Lazy<T>`, `Readonly<T>`).
        if let Some((wrapper, rest)) = next.split_once('<')
            && let Some(inner) = rest.strip_suffix('>')
        {
            let wrapper_leaf = wrapper
                .trim()
                .rsplit([':', '.'])
                .next()
                .unwrap_or(wrapper.trim());
            if matches!(
                wrapper_leaf,
                "Box"
                    | "Arc"
                    | "Rc"
                    | "Pin"
                    | "MutexGuard"
                    | "RwLockReadGuard"
                    | "RwLockWriteGuard"
                    | "unique_ptr"
                    | "shared_ptr"
                    | "Lazy"
                    | "Readonly"
            ) {
                // Only unwrap single-type-arg wrappers (or `unique_ptr<T, Deleter>` taking first arg)
                let mut depth = 0usize;
                let mut first_end = inner.len();
                for (idx, ch) in inner.char_indices() {
                    match ch {
                        '<' | '[' | '(' => depth += 1,
                        '>' | ']' | ')' => depth = depth.saturating_sub(1),
                        ',' if depth == 0 => {
                            first_end = idx;
                            break;
                        }
                        _ => {}
                    }
                }
                let candidate = inner[..first_end].trim();
                if !candidate.is_empty() {
                    next = candidate;
                }
            }
        }
        if next == ty {
            break;
        }
        ty = next;
    }
    ty.trim_end_matches(['?', '&', '*']).trim().to_string()
}

fn type_bases(node: tree_sitter::Node, src: &[u8], lang: Lang) -> Vec<String> {
    if lang == Lang::Rust && node.kind() == "impl_item" {
        let tr = text_field(node, "trait", src);
        return if tr.is_empty() { Vec::new() } else { vec![tr] };
    }
    let mut result = Vec::new();
    let mut c = node.walk();
    for child in node.named_children(&mut c) {
        if matches!(
            child.kind(),
            "base_list"
                | "base_class_clause"
                | "trait_bounds"
                | "extends_type_clause"
                | "superclasses"
                | "argument_list"
        ) || node
            .child_by_field_name("superclasses")
            .is_some_and(|s| s.id() == child.id())
        {
            let mut bc = child.walk();
            for base in child.named_children(&mut bc) {
                if matches!(
                    base.kind(),
                    "access_specifier"
                        | "attribute_declaration"
                        | "lifetime"
                        | "keyword_argument"
                        | "list_splat"
                        | "dictionary_splat"
                        | "argument_list"
                ) {
                    continue;
                }
                let base = if base.kind() == "primary_constructor_base_type" {
                    base.child_by_field_name("type")
                        .or_else(|| base.named_child(0))
                        .unwrap_or(base)
                } else {
                    base
                };
                let text = base.utf8_text(src).unwrap_or_default().trim();
                if !text.is_empty() {
                    result.push(text.to_string());
                }
            }
        } else if child.kind() == "class_heritage" {
            let mut hc = child.walk();
            for clause in child.named_children(&mut hc) {
                if !matches!(clause.kind(), "extends_clause" | "implements_clause") {
                    result.push(clause.utf8_text(src).unwrap_or_default().to_string());
                    continue;
                }
                let mut bc = clause.walk();
                for base in clause.named_children(&mut bc) {
                    if base.kind() != "type_arguments" {
                        result.push(base.utf8_text(src).unwrap_or_default().to_string());
                    }
                }
            }
        } else if lang == Lang::Go && child.kind() == "interface_type" {
            let mut ic = child.walk();
            for base in child
                .named_children(&mut ic)
                .filter(|n| n.kind() == "type_elem")
            {
                // Unions and ~underlying constraints are not inheritance.
                if base.named_child_count() == 1 {
                    let ty = base.named_child(0).unwrap();
                    if matches!(
                        ty.kind(),
                        "type_identifier" | "qualified_type" | "generic_type"
                    ) {
                        result.push(ty.utf8_text(src).unwrap_or_default().to_string());
                    }
                }
            }
        }
    }
    result
}

/// A class declaration's outer callable declarator, never a free prototype or
/// a function-pointer field/parameter. Definitions are handled at their root.
fn cpp_method_declaration(
    node: tree_sitter::Node,
    lang: Lang,
    in_type: bool,
) -> Option<tree_sitter::Node> {
    if lang != Lang::Cpp || !in_type || node.kind() != "function_declarator" {
        return None;
    }
    let mut declarator = node.child_by_field_name("declarator")?;
    while declarator.kind() == "parenthesized_declarator" {
        declarator = declarator.named_child(0)?;
    }
    if matches!(
        declarator.kind(),
        "pointer_declarator" | "reference_declarator" | "function_declarator"
    ) {
        return None;
    }
    let mut parent = node.parent()?;
    while matches!(
        parent.kind(),
        "pointer_declarator" | "reference_declarator" | "attributed_declarator"
    ) {
        parent = parent.parent()?;
    }
    (matches!(parent.kind(), "field_declaration" | "declaration")
        && parent
            .parent()
            .is_some_and(|p| p.kind() == "field_declaration_list"))
    .then_some(parent)
}

fn enclosing_impl_trait(node: tree_sitter::Node, src: &[u8]) -> Option<String> {
    let mut parent = node.parent();
    while let Some(p) = parent {
        if p.kind() == "impl_item" {
            let tr = text_field(p, "trait", src);
            return (!tr.is_empty()).then_some(tr);
        }
        if matches!(p.kind(), "function_item" | "trait_item" | "mod_item") {
            break;
        }
        parent = p.parent();
    }
    None
}

fn method_dispatch(node: tree_sitter::Node, src: &[u8], lang: Lang, in_type: bool) -> bool {
    let header_end = node
        .child_by_field_name("body")
        .map_or(node.end_byte(), |n| n.start_byte());
    let header = std::str::from_utf8(&src[node.start_byte()..header_end]).unwrap_or_default();
    let has = |word| {
        header
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .any(|w| w == word)
    };
    if has("static") {
        return false;
    }
    match lang {
        Lang::CSharp => {
            has("virtual")
                || has("abstract")
                || has("override")
                || node
                    .parent()
                    .and_then(|p| p.parent())
                    .is_some_and(|p| p.kind() == "interface_declaration")
        }
        Lang::Cpp => {
            has("virtual") || has("override") || header.contains("= 0") || header.contains("=0")
        }
        Lang::Go => node.kind() == "method_elem",
        Lang::Rust => {
            let mut parent = node.parent();
            while let Some(p) = parent {
                if p.kind() == "trait_item" {
                    return has("self");
                }
                if matches!(p.kind(), "impl_item" | "function_item") {
                    break;
                }
                parent = p.parent();
            }
            false
        }
        Lang::Python => {
            if !in_type || node.kind() != "function_definition" {
                return false;
            }
            let mut parent = node.parent();
            while let Some(p) = parent {
                if p.kind() == "class_definition" {
                    return true;
                }
                if matches!(p.kind(), "function_definition" | "lambda") {
                    return false;
                }
                parent = p.parent();
            }
            false
        }
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => {
            matches!(
                node.kind(),
                "method_definition" | "method_signature" | "abstract_method_signature"
            ) && text_field(node, "name", src) != "constructor"
        }
        Lang::C => false,
    }
}

fn method_static(
    node: tree_sitter::Node,
    symbol: tree_sitter::Node,
    src: &[u8],
    lang: Lang,
    definition: bool,
) -> bool {
    if !definition {
        return false;
    }
    let end = symbol
        .child_by_field_name("body")
        .map_or(symbol.end_byte(), |n| n.start_byte());
    let header = std::str::from_utf8(&src[symbol.start_byte()..end]).unwrap_or_default();
    let has = |word| {
        header
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .any(|w| w == word)
    };
    match lang {
        Lang::Rust => !has("self"),
        Lang::Go => node.kind() != "method_declaration" && node.kind() != "method_elem",
        Lang::Python => node
            .parent()
            .filter(|p| p.kind() == "decorated_definition")
            .is_some_and(|p| {
                p.utf8_text(src)
                    .unwrap_or_default()
                    .lines()
                    .take_while(|line| {
                        !line.trim_start().starts_with("def ")
                            && !line.trim_start().starts_with("async def ")
                    })
                    .any(|line| {
                        line.trim().starts_with("@staticmethod")
                            || line.trim().starts_with("@classmethod")
                    })
            }),
        _ => has("static"),
    }
}

fn parameter_arity(sig: &str, lang: Lang) -> Option<usize> {
    if sig.is_empty() {
        return None;
    }
    let (params, wrapped) = if let Some(params) = sig.strip_prefix('(') {
        (params, true)
    } else {
        (sig.trim_matches('|'), false)
    };
    let mut arity = 0;
    let mut depth = 0usize;
    let mut start = 0;
    let mut quote = None;
    let mut escaped = false;
    for (i, ch) in params.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == q {
                quote = None;
            }
            continue;
        }
        match ch {
            '"' | '`' => quote = Some(ch),
            '\'' if lang != Lang::Rust => quote = Some(ch),
            ')' if wrapped && depth == 0 => {
                return Some(arity + parameter_count(&params[start..i], lang)?);
            }
            '(' | '[' | '{' | '<' => depth += 1,
            ')' | ']' | '}' | '>' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                arity += parameter_count(&params[start..i], lang)?;
                start = i + 1;
            }
            _ => {}
        }
    }
    Some(arity + parameter_count(&params[start..], lang)?)
}

fn parameter_count(parameter: &str, lang: Lang) -> Option<usize> {
    let p = parameter.trim();
    if p == "..." {
        return None;
    }
    let receiver = (lang == Lang::Rust
        && p.split(|c: char| !c.is_alphanumeric() && c != '_')
            .any(|w| w == "self"))
        || (lang == Lang::Python
            && matches!(p.split(':').next().unwrap_or(p).trim(), "self" | "cls"))
        || (matches!(lang, Lang::TypeScript | Lang::Tsx)
            && p.split(':').next().unwrap_or(p).trim() == "this");
    Some(usize::from(
        !p.is_empty() && p != "void" && p != "/" && p != "*" && !receiver,
    ))
}

/// Local type annotations only. No assignment propagation or whole-program
/// inference: an untyped binding explicitly shadows any outer typed binding.
fn collect_binding(
    node: tree_sitter::Node,
    src: &[u8],
    lang: Lang,
    bindings: &mut Vec<(String, Option<String>)>,
) {
    let kind = node.kind();
    let parameter = matches!(
        kind,
        "parameter"
            | "parameter_declaration"
            | "variadic_parameter_declaration"
            | "required_parameter"
            | "optional_parameter"
            | "typed_parameter"
            | "typed_default_parameter"
            | "default_parameter"
    );
    let declaration = matches!(
        kind,
        "let_declaration"
            | "variable_declarator"
            | "var_spec"
            | "short_var_declaration"
            | "init_declarator"
            | "assignment"
    );
    if kind == "identifier"
        && node.parent().is_some_and(|p| {
            matches!(
                p.kind(),
                "parameters" | "formal_parameters" | "lambda_parameters" | "closure_parameters"
            ) || p
                .child_by_field_name("parameter")
                .is_some_and(|n| n.id() == node.id())
        })
    {
        bindings.push((node.utf8_text(src).unwrap_or_default().to_string(), None));
        return;
    }
    if !parameter && !declaration && !(matches!(lang, Lang::C | Lang::Cpp) && kind == "declaration")
    {
        return;
    }
    let mut ty = node.child_by_field_name("type");
    if ty.is_none() && matches!(kind, "variable_declarator" | "init_declarator") {
        ty = node.parent().and_then(|p| p.child_by_field_name("type"));
    }
    let ty = ty
        .map(|t| clean_type(t.utf8_text(src).unwrap_or_default()))
        .filter(|t| !t.is_empty() && !matches!(t.as_str(), "var" | "auto"))
        .or_else(|| {
            (!parameter).then(|| inferred_initializer_type(node, src, lang)).flatten()
        });
    let mut names = Vec::new();
    let mut c = node.walk();
    names.extend(node.children_by_field_name("name", &mut c));
    if names.is_empty() {
        if let Some(name) = node
            .child_by_field_name("pattern")
            .or_else(|| node.child_by_field_name("left"))
            .or_else(|| {
                node.child_by_field_name("declarator")
                    .and_then(declarator_name)
            })
        {
            names.push(name);
        } else if parameter
            && let Some(name) = node.named_child(0).filter(|n| n.kind() == "identifier")
        {
            names.push(name);
        }
    }
    for name in names {
        if name.kind() == "expression_list" {
            let mut c = name.walk();
            for name in name
                .named_children(&mut c)
                .filter(|n| n.kind() == "identifier")
            {
                bindings.push((
                    name.utf8_text(src).unwrap_or_default().to_string(),
                    ty.clone(),
                ));
            }
        } else if matches!(name.kind(), "identifier" | "field_identifier") {
            bindings.push((
                name.utf8_text(src).unwrap_or_default().to_string(),
                ty.clone(),
            ));
        }
    }
}

fn receiver_owner(owner: &str) -> String {
    if let Some(as_pos) = owner.find(" as ")
        && let Some(open) = owner[..as_pos].rfind('<')
    {
        return format!("{}{}", &owner[..open], &owner[open + 1..as_pos]);
    }
    if let Some(open) = owner
        .rfind("::<")
        .map(|i| i + 2)
        .or_else(|| owner.starts_with('<').then_some(0))
        && let Some(ty) = owner[open + 1..].strip_suffix('>')
    {
        return format!("{}{ty}", &owner[..open]);
    }
    owner.to_string()
}

fn call_receiver(
    call: tree_sitter::Node,
    src: &[u8],
    bindings: &[(String, Option<String>)],
    known_types: &HashSet<String>,
    caller: &FnDef,
    enclosing_type: Option<&FnDef>,
    lang: Lang,
) -> Option<(String, bool)> {
    let mut expr = call
        .child_by_field_name("function")
        .or_else(|| call.child_by_field_name("type"))?;
    loop {
        if matches!(
            expr.kind(),
            "generic_function" | "template_function" | "template_method"
        ) {
            expr = expr
                .child_by_field_name("function")
                .or_else(|| expr.child_by_field_name("name"))?;
        } else if expr.kind() == "parenthesized_expression" {
            expr = expr.named_child(0)?;
        } else {
            break;
        }
    }
    // `object.Base::method()` and `Base::method()` explicitly suppress C++
    // virtual dispatch, even if the object is statically a derived type.
    if lang == Lang::Cpp
        && expr.kind() == "field_expression"
        && let Some(field) = expr
            .child_by_field_name("field")
            .filter(|field| field.kind() == "qualified_identifier")
    {
        expr = field;
    }
    let dispatch = !(lang == Lang::Cpp && expr.kind() == "qualified_identifier");
    let qualified = matches!(
        expr.kind(),
        "scoped_identifier" | "qualified_identifier" | "scoped_type_identifier"
    );
    let receiver = match expr.kind() {
        "member_expression" | "attribute" => expr.child_by_field_name("object"),
        "field_expression" => expr
            .child_by_field_name("value")
            .or_else(|| expr.child_by_field_name("argument")),
        "selector_expression" => expr.child_by_field_name("operand"),
        "member_access_expression" => expr.child_by_field_name("expression"),
        "conditional_access_expression" => expr.child_by_field_name("condition"),
        "scoped_identifier" | "scoped_type_identifier" => expr.child_by_field_name("path"),
        "qualified_identifier" => expr.child_by_field_name("scope"),
        _ => None,
    }?;
    let unparen_receiver = unparenthesized(receiver);
    if is_call(unparen_receiver.kind())
        && let Some(ty) = service_locator_or_ctor_call_type(unparen_receiver, src, lang)
    {
        return Some((ty, dispatch));
    }
    if matches!(
        unparen_receiver.kind(),
        "object_creation_expression" | "new_expression"
    ) && let Some(ty) = inferred_expr_type(unparen_receiver, src, lang)
    {
        return Some((ty, dispatch));
    }
    let text = receiver.utf8_text(src).ok()?.trim();
    let binding = bindings.iter().rev().find(|(name, _)| name == text);
    if let Some((_, Some(ty))) = binding {
        return Some((ty.clone(), dispatch));
    }
    let self_receiver =
        (text == "self" && matches!(lang, Lang::Rust | Lang::Python) && !caller.is_static)
            || (text == "Self" && lang == Lang::Rust)
            || (text == "this"
                && matches!(
                    lang,
                    Lang::CSharp | Lang::Cpp | Lang::TypeScript | Lang::Tsx | Lang::JavaScript
                ));
    if self_receiver {
        return caller
            .owner
            .as_deref()
            .map(receiver_owner)
            .or_else(|| enclosing_type.map(|t| receiver_owner(&t.qual)))
            .map(|receiver| (receiver, dispatch));
    }
    if (text == "base" && lang == Lang::CSharp)
        || (text == "super" && matches!(lang, Lang::TypeScript | Lang::Tsx | Lang::JavaScript))
        || (text == "super()" && lang == Lang::Python)
    {
        return enclosing_type
            .and_then(|t| t.bases.first())
            .cloned()
            .map(|receiver| (receiver, false));
    }
    if binding.is_some() {
        return None;
    }
    let looks_like_type = text
        .split(['<', '['])
        .next()
        .unwrap_or(text)
        .rsplit([':', '.'])
        .next()
        .and_then(|s| s.chars().next())
        .is_some_and(|c| c.is_ascii_uppercase())
        && text
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | ':' | '.' | '<' | '>' | ',' | ' '));
    if qualified || known_types.contains(text) || looks_like_type {
        return Some((text.to_string(), dispatch));
    }
    None
}

fn symbol_evidence(
    node: tree_sitter::Node,
    name: tree_sitter::Node,
    src: &[u8],
    end: usize,
) -> (Vec<u64>, u64, Option<u64>, usize) {
    let mut tokens = Vec::new();
    let own_name = name
        .utf8_text(src)
        .unwrap_or_default()
        .trim_start_matches('~');
    let name_start = name.start_byte() + usize::from(name.kind() == "destructor_name");
    let mut removed = vec![(name_start, name.end_byte())];
    let body = node
        .child_by_field_name("body")
        .filter(|body| body.end_byte() <= end);
    let mut body_shape = body.map(|_| 0xcbf29ce484222325u64);
    let mut body_tokens = 0;
    let mut cursor = node.walk();
    loop {
        let token = cursor.node();
        let skip = token.start_byte() >= end
            || token.kind().contains("comment")
            || (token.start_byte() >= name.start_byte() && token.end_byte() <= name.end_byte());
        if !skip && token.child_count() == 0 {
            let text = token.utf8_text(src).unwrap_or_default();
            if body.is_some_and(|body| token.start_byte() >= body.start_byte()) {
                let literal = token.is_named()
                    && (token.kind().contains("literal")
                        || token.kind().contains("string")
                        || token.kind().contains("character")
                        || token.kind().contains("escape")
                        || matches!(token.kind(), "integer" | "float" | "number"));
                let structural = if token.kind().contains("identifier") {
                    "identifier"
                } else if literal {
                    "literal"
                } else {
                    text
                };
                let shape = body_shape.as_mut().unwrap();
                for byte in structural.bytes().chain(std::iter::once(0)) {
                    *shape = (*shape ^ u64::from(byte)).wrapping_mul(0x100000001b3);
                }
                if (token.is_named() && !text.trim().is_empty())
                    || matches!(
                        text,
                        "+" | "-"
                            | "*"
                            | "/"
                            | "%"
                            | "<"
                            | ">"
                            | "=="
                            | "!="
                            | "&&"
                            | "||"
                            | "&"
                            | "|"
                            | "^"
                            | "!"
                            | "?"
                            | "="
                    )
                {
                    body_tokens += 1;
                }
            }
            if token.kind().contains("identifier") && text == own_name {
                removed.push((token.start_byte(), token.end_byte()));
            } else if text.chars().any(|c| c.is_alphanumeric() || c == '_')
                || (!text.trim().is_empty()
                    && token.is_named()
                    && (token.kind().contains("string")
                        || token.kind().contains("character")
                        || token.kind().contains("escape")))
            {
                let hash = text.bytes().fold(0xcbf29ce484222325u64, |h, b| {
                    (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
                });
                tokens.push(hash);
            }
        }
        if !skip && cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                tokens.sort_unstable();
                removed.sort_unstable();
                return (
                    tokens,
                    hash_without_ranges(src, node.start_byte(), end, &removed),
                    body_shape,
                    body_tokens,
                );
            }
        }
    }
}

fn text_field(node: tree_sitter::Node, field: &str, src: &[u8]) -> String {
    node.child_by_field_name(field)
        .and_then(|n| n.utf8_text(src).ok())
        .unwrap_or_default()
        .to_string()
}

fn type_name_node(mut node: tree_sitter::Node) -> tree_sitter::Node {
    loop {
        match node
            .child_by_field_name("name")
            .or_else(|| node.child_by_field_name("type"))
        {
            Some(inner) => node = inner,
            None => return node,
        }
    }
}

fn owner_type(text: &str) -> String {
    let clean = clean_type(text);
    let mut result = String::new();
    let mut depth = 0usize;
    for ch in clean.chars() {
        match ch {
            '<' | '[' => depth += 1,
            '>' | ']' => depth = depth.saturating_sub(1),
            _ if depth == 0 => result.push(ch),
            _ => {}
        }
    }
    result.trim().to_string()
}

fn container_name(node: tree_sitter::Node, src: &[u8], lang: Lang) -> Option<String> {
    let kind = node.kind();
    let container = match lang {
        Lang::Rust => matches!(kind, "mod_item" | "trait_item"),
        Lang::Python => kind == "class_definition",
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => matches!(
            kind,
            "class_declaration"
                | "abstract_class_declaration"
                | "class"
                | "internal_module"
                | "module"
        ),
        Lang::CSharp => matches!(
            kind,
            "namespace_declaration"
                | "class_declaration"
                | "struct_declaration"
                | "interface_declaration"
                | "record_declaration"
                | "enum_declaration"
        ),
        Lang::Cpp => matches!(
            kind,
            "namespace_definition" | "class_specifier" | "struct_specifier" | "union_specifier"
        ),
        _ => false,
    };
    if !container {
        return None;
    }
    let mut name = text_field(node, "name", src);
    if name.is_empty()
        && kind == "class"
        && let Some(parent) = node.parent()
    {
        name = text_field(parent, "name", src);
    }
    if name.is_empty() && kind == "namespace_definition" {
        name = "(anonymous)".to_string();
    }
    if name.is_empty() { None } else { Some(name) }
}

fn rust_attribute(node: tree_sitter::Node, src: &[u8], cfg: bool) -> bool {
    let mut previous = node.prev_named_sibling();
    while let Some(attr) = previous.filter(|n| n.kind() == "attribute_item") {
        let compact: String = attr
            .utf8_text(src)
            .unwrap_or_default()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        if cfg {
            if compact == "#[cfg(test)]" {
                return true;
            }
        } else {
            let path = compact
                .trim_start_matches("#[")
                .split(['(', ']'])
                .next()
                .unwrap_or("");
            if path.rsplit("::").next() == Some("test") {
                return true;
            }
        }
        previous = attr.prev_named_sibling();
    }
    false
}

fn csharp_attribute(node: tree_sitter::Node, src: &[u8], names: &[&str]) -> bool {
    let mut cursor = node.walk();
    for list in node
        .named_children(&mut cursor)
        .filter(|n| n.kind() == "attribute_list")
    {
        let mut ac = list.walk();
        for attr in list
            .named_children(&mut ac)
            .filter(|n| n.kind() == "attribute")
        {
            let name = text_field(attr, "name", src);
            let simple = name.rsplit('.').next().unwrap_or(&name);
            if names.contains(&simple.strip_suffix("Attribute").unwrap_or(simple)) {
                return true;
            }
        }
    }
    false
}

fn signature(
    node: tree_sitter::Node,
    name: tree_sitter::Node,
    src: &[u8],
    lang: Lang,
    definition: bool,
) -> String {
    if definition && matches!(lang, Lang::C | Lang::Cpp) {
        let mut parent = name.parent();
        while let Some(p) = parent {
            if p.kind() == "function_declarator" {
                return std::str::from_utf8(&src[name.end_byte()..p.end_byte()])
                    .unwrap_or_default()
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
            }
            if p.id() == node.id() {
                break;
            }
            parent = p.parent();
        }
    }
    let mut target = node;
    if !definition {
        if let Some(value) = node
            .child_by_field_name("value")
            .or_else(|| node.child_by_field_name("right"))
        {
            target = unparenthesized(if value.kind() == "expression_list" {
                value.named_child(0).unwrap_or(value)
            } else {
                value
            });
        } else if lang == Lang::CSharp {
            let mut c = node.walk();
            let value = node.named_children(&mut c).find(|n| {
                matches!(
                    unparenthesized(*n).kind(),
                    "lambda_expression" | "anonymous_method_expression"
                )
            });
            if let Some(value) = value {
                target = unparenthesized(value);
            }
        }
    }
    if lang == Lang::Cpp
        && !definition
        && let Some(declarator) = target.child_by_field_name("declarator")
    {
        target = declarator;
    }
    let params = target
        .child_by_field_name("parameters")
        .or_else(|| target.child_by_field_name("parameter"));
    let params = params.or_else(|| {
        let mut c = target.walk();

        target.named_children(&mut c).find(|n| {
            matches!(
                n.kind(),
                "parameters"
                    | "parameter_list"
                    | "formal_parameters"
                    | "lambda_parameters"
                    | "implicit_parameter"
            )
        })
    });
    params
        .and_then(|n| n.utf8_text(src).ok())
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_call(kind: &str) -> bool {
    matches!(
        kind,
        "call_expression"        // rust/js/ts/go
            | "generic_function" // rust turbofish
            | "call" // python
            | "invocation_expression" // c#
            | "object_creation_expression" // c# constructors
            | "new_expression" // c++ constructors
    )
}

/// Extract the simple callee name from a call node's function expression.
fn callee_name(call: tree_sitter::Node, src: &[u8]) -> Option<String> {
    let expr = call
        .child_by_field_name("function")
        .or_else(|| call.child_by_field_name("type"))?;
    simple_name(expr, src)
}

/// Last identifier segment of any callable expression:
/// `foo`, `a.b.c()`, `Module::fn()`, `obj.method()`, `f::<T>()`.
fn simple_name(mut expr: tree_sitter::Node, src: &[u8]) -> Option<String> {
    loop {
        expr = match expr.kind() {
            "identifier"
            | "property_identifier"
            | "field_identifier"
            | "shorthand_property_identifier"
            | "type_identifier"
            | "destructor_name"
            | "operator_name" => return expr.utf8_text(src).ok().map(str::to_string),
            "parenthesized_expression" | "generic_name" => expr.named_child(0)?,
            "template_function" | "template_method" | "template_type" | "dependent_name" => {
                expr.child_by_field_name("name")?
            }
            "pointer_expression" => expr.child_by_field_name("argument")?,
            "generic_function" => expr.child_by_field_name("function")?,
            "field_expression"
            | "attribute"
            | "selector_expression"
            | "scoped_identifier"
            | "qualified_identifier"
            | "member_expression"
            | "member_access_expression"
            | "member_binding_expression"
            | "conditional_access_expression"
            | "qualified_name"
            | "alias_qualified_name" => {
                let mut c = expr.walk();
                let last = expr.named_children(&mut c).last();
                last?
            }
            _ => return None,
        };
    }
}

fn unparenthesized(mut node: tree_sitter::Node) -> tree_sitter::Node {
    while node.kind() == "parenthesized_expression" {
        match node.named_child(0) {
            Some(inner) => node = inner,
            None => break,
        }
    }
    node
}

/// Follow only the declarator spine, never identifiers in parameters or types.
fn declarator_name(mut node: tree_sitter::Node) -> Option<tree_sitter::Node> {
    loop {
        node = match node.kind() {
            "identifier" | "field_identifier" | "type_identifier" | "destructor_name"
            | "operator_name" => return Some(node),
            "qualified_identifier" | "template_function" | "template_method" | "dependent_name" => {
                node.child_by_field_name("name")?
            }
            "parenthesized_declarator" | "reference_declarator" => node.named_child(0)?,
            _ => node.child_by_field_name("declarator")?,
        };
    }
}

/// FNV-1a over the symbol source/header, excluding only its binding/name range.
fn hash_range_without_name(
    node: tree_sitter::Node,
    name: tree_sitter::Node,
    src: &[u8],
    end: usize,
) -> u64 {
    // The tilde distinguishes a destructor from a constructor, not its name.
    let name_start = name.start_byte() + usize::from(name.kind() == "destructor_name");
    hash_without_ranges(
        src,
        node.start_byte(),
        end,
        &[(name_start, name.end_byte())],
    )
}

fn hash_without_ranges(src: &[u8], start: usize, end: usize, removed: &[(usize, usize)]) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    let mut started = false;
    let mut pending_ws = false;
    let mut position = start;
    for (from, to) in removed.iter().copied().chain(std::iter::once((end, end))) {
        for ch in std::str::from_utf8(&src[position..from])
            .unwrap_or_default()
            .chars()
        {
            if ch.is_whitespace() {
                pending_ws = started;
                continue;
            }
            if pending_ws {
                h = (h ^ u64::from(b' ')).wrapping_mul(0x100000001b3);
                pending_ws = false;
            }
            let mut bytes = [0; 4];
            for &b in ch.encode_utf8(&mut bytes).as_bytes() {
                h = (h ^ u64::from(b)).wrapping_mul(0x100000001b3);
            }
            started = true;
        }
        position = to;
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualified_identities_and_overload_signatures() {
        for (path, src, expected) in [
            (
                "a.rs",
                "impl<T> A<T> { fn new() {} } impl B { fn new() {} } impl Display for A { fn fmt() {} } fn main() { let double = |x| x * 2; }",
                vec![
                    "A::new",
                    "B::new",
                    "<A as Display>::fmt",
                    "main",
                    "main::double",
                ],
            ),
            (
                "a.go",
                "package a\nfunc (r *Router[T]) Handle() {}\nfunc (s Server) Handle() {}",
                vec!["Router::Handle", "Server::Handle"],
            ),
            (
                "a.py",
                "class A:\n def run(self): pass\nclass B:\n def run(self): pass\n",
                vec!["A::run", "B::run"],
            ),
            (
                "a.ts",
                "namespace N { class A { run() {} } } const B = class { run() {} };",
                vec!["N::A::run", "B::run"],
            ),
            (
                "a.cs",
                "namespace A.B; class Worker { void Log(int x) {} void Log(string x) {} void Run() { void Local() {} } }",
                vec![
                    "A.B::Worker::Log",
                    "A.B::Worker::Log",
                    "A.B::Worker::Run",
                    "A.B::Worker::Run::Local",
                ],
            ),
            (
                "a.cpp",
                "struct A { int run() { return 0; } }; int B::run() const noexcept { return 1; }",
                vec!["A::run", "B::run"],
            ),
        ] {
            let pf = parse_file(path, src).unwrap();
            let functions: Vec<_> = pf
                .fns
                .iter()
                .filter(|f| f.kind != SymbolKind::Type)
                .collect();
            assert_eq!(
                functions
                    .iter()
                    .map(|f| f.qual.as_str())
                    .collect::<Vec<_>>(),
                expected,
                "{path}"
            );
            if path == "a.cs" {
                assert_ne!(functions[0].sig, functions[1].sig);
            }
            if path == "a.cpp" {
                assert_eq!(functions[1].sig, "() const noexcept");
            }
        }
    }

    #[test]
    fn tsx_and_language_families() {
        let pf = parse_file("a.tsx", "const Component = () => {\n return <div>Hello</div>;\n};\nfunction go() {\n work();\n}\n").unwrap();
        assert_eq!(
            pf.fns
                .iter()
                .map(|f| (f.name.as_str(), f.start_row, f.end_row))
                .collect::<Vec<_>>(),
            [("Component", 0, 3), ("go", 3, 6)]
        );
        for path in ["a.ts", "a.tsx", "a.mts", "a.cts", "a.js", "a.cjs"] {
            assert_eq!(lang_for_path(path).unwrap().family(), Lang::TypeScript);
        }
        assert_eq!(Lang::C.family(), Lang::Cpp);
    }

    #[test]
    fn deep_expression_on_small_stack() {
        std::thread::Builder::new()
            .stack_size(1 << 20)
            .spawn(|| {
                let src = format!("int run(void) {{ return 0{}; }}", "+1".repeat(20_000));
                let pf = parse_file("deep.c", &src).unwrap();
                assert_eq!(pf.fns.len(), 1);
                assert_eq!(pf.fns[0].name, "run");
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn additional_test_conventions_and_attributes() {
        for path in [
            "tests/a.py",
            "test/a.py",
            "conftest.py",
            "src/a.spec.ts",
            "src/a.spec.tsx",
            "test/a.js",
        ] {
            let src = if path.ends_with(".py") {
                "def run(): pass"
            } else {
                "function run() {}"
            };
            assert!(parse_file(path, src).unwrap().fns[0].is_test, "{path}");
        }
        for path in [
            "contest/a.py",
            "src/conftest_extra.py",
            "src/a.special.ts",
            "contest/a.js",
        ] {
            let src = if path.ends_with(".py") {
                "def run(): pass"
            } else {
                "function run() {}"
            };
            assert!(!parse_file(path, src).unwrap().fns[0].is_test, "{path}");
        }
        let rust = parse_file("a.rs", "#[tokio::test]\nasync fn one() {}\n#[cfg(test)]\nmod checks { fn two() {} }\n#[contest]\nfn three() {}\n").unwrap();
        assert_eq!(
            rust.fns.iter().map(|f| f.is_test).collect::<Vec<_>>(),
            [true, true, false]
        );
        for attr in [
            "Fact",
            "Theory",
            "Test",
            "TestMethod",
            "TestCase",
            "FactAttribute",
            "Xunit.Fact",
        ] {
            let src = format!("class A {{ [{attr}] void Run() {{}} void Prod() {{}} }}");
            let pf = parse_file("a.cs", &src).unwrap();
            assert!(
                pf.fns.iter().find(|f| f.name == "Run").unwrap().is_test,
                "{attr}"
            );
            assert!(
                !pf.fns.iter().find(|f| f.name == "Prod").unwrap().is_test,
                "{attr}"
            );
        }
        for attr in ["TestFixture", "TestClassAttribute"] {
            let pf =
                parse_file("a.cs", &format!("[{attr}] class A {{ void Run() {{}} }}")).unwrap();
            assert!(pf.fns[0].is_test);
        }
        assert!(
            !parse_file("a.cs", "[Contest] class A { [Latest] void Run() {} }")
                .unwrap()
                .fns[0]
                .is_test
        );
    }

    #[test]
    fn rename_hashes_across_languages() {
        for (path, source) in [
            ("a.rs", "fn old(x: u32) {}"),
            ("a.py", "def old(x): pass"),
            ("a.ts", "const old = (x) => x + 1;"),
            ("a.go", "package a\nfunc old(x int) {}"),
            ("a.rs", "fn main() { let old = |x| x + 1; }"),
            ("a.py", "old = lambda x: x + 1"),
            (
                "a.cs",
                "class A { void Run() { Func<int,int> old = x => x + 1; } }",
            ),
        ] {
            let before = parse_file(path, source).unwrap();
            let after = parse_file(path, &source.replace("old", "renamed")).unwrap();
            let index = before.fns.iter().position(|f| f.name == "old").unwrap();
            assert_eq!(
                before.fns[index].body_hash, after.fns[index].body_hash,
                "{path}"
            );
        }
    }

    #[test]
    fn rust_fns_and_calls() {
        let src = r#"
fn helper(x: u32) -> u32 { x + 1 }

fn main() {
    let y = helper(2);
    std::mem::drop(y);
}
"#;
        let pf = parse_file("a.rs", src).unwrap();
        assert_eq!(pf.fns.len(), 2);
        assert_eq!(pf.fns[0].name, "helper");
        assert_eq!(pf.fns[1].name, "main");
        let calls: Vec<_> = pf.calls.iter().map(|c| c.name.as_str()).collect();
        assert!(calls.contains(&"helper"), "{calls:?}");
        assert!(calls.contains(&"drop"), "{calls:?}");
        // helper call belongs to main (index 1)
        let main_calls: Vec<_> = pf
            .calls
            .iter()
            .filter(|c| c.caller == 1)
            .map(|c| c.name.clone())
            .collect();
        assert_eq!(main_calls.len(), 2);
    }

    #[test]
    fn python_fns_and_calls() {
        let src = r#"
def helper(x):
    return x + 1

def main():
    y = helper(2)
    print(y)
"#;
        let pf = parse_file("a.py", src).unwrap();
        assert_eq!(pf.fns.len(), 2);
        let names: Vec<_> = pf.calls.iter().map(|c| c.name.clone()).collect();
        assert!(names.contains(&"helper".to_string()));
        assert!(names.contains(&"print".to_string()));
    }

    #[test]
    fn ts_methods_and_calls() {
        let src = r#"
export function handler(req: Request) {
  return svc.process(req);
}

class Svc {
  process(req: Request) { return req; }
}
"#;
        let pf = parse_file("a.ts", src).unwrap();
        assert_eq!(pf.fns.len(), 3);
        let names: Vec<_> = pf.calls.iter().map(|c| c.name.clone()).collect();
        assert!(names.contains(&"process".to_string()));
    }

    #[test]
    fn rust_mod_tests_marked() {
        let src = r#"
fn prod() { helper(); }
fn helper() {}

#[cfg(test)]
mod tests {
    fn test_thing() { helper(); }
}
"#;
        let pf = parse_file("a.rs", src).unwrap();
        let by: Vec<(&String, bool)> = pf.fns.iter().map(|f| (&f.name, f.is_test)).collect();
        assert_eq!(by.len(), 3);
        assert!(!by.iter().find(|(n, _)| *n == "prod").unwrap().1);
        assert!(by.iter().find(|(n, _)| *n == "test_thing").unwrap().1);
        // helper defined at top level is not test even though tests call it
        assert!(!by.iter().find(|(n, _)| *n == "helper").unwrap().1);
    }

    #[test]
    fn closures_captured_as_named_symbols() {
        let rust = "fn main() {\n    let double = |x: u32| helper(x);\n    double(2);\n}\nfn helper(x: u32) -> u32 { x * 2 }\n";
        let pf = parse_file("a.rs", rust).unwrap();
        assert!(pf.fns.iter().any(|f| f.name == "double"), "{:?}", pf.fns);
        // calls inside the closure attribute to the closure (index of double)
        let double_idx = pf.fns.iter().position(|f| f.name == "double").unwrap();
        assert!(
            pf.calls
                .iter()
                .any(|c| c.caller == double_idx && c.name == "helper")
        );

        let ts = "const retry = (fn) => { return fn(); };\nexport function run() { return retry(work); }\n";
        let pf = parse_file("a.ts", ts).unwrap();
        assert!(pf.fns.iter().any(|f| f.name == "retry"), "{:?}", pf.fns);

        let py = "def run():\n    f = lambda x: helper(x)\n    return f(1)\n";
        let pf = parse_file("a.py", py).unwrap();
        assert!(pf.fns.iter().any(|f| f.name == "f"), "{:?}", pf.fns);

        let go = "func run() {\n    f := func() int { return helper() }\n    _ = f()\n}\n";
        let pf = parse_file("a.go", go).unwrap();
        assert!(pf.fns.iter().any(|f| f.name == "f"), "{:?}", pf.fns);
    }

    #[test]
    fn csharp_methods_constructors_and_nested_callables() {
        let src = r#"
interface IWorker { int Missing(); }
abstract class Base { public abstract int Absent(); }
class Worker {
    public Worker() { Setup(); }
    ~Worker() { Cleanup(); }
    int Helper(int x) => x + 1;
    int Run() {
        int Local(int x) => Helper(x);
        Func<int, int> twice = (x => Local(x));
        Func<int> fallback = delegate { return Helper(0); };
        receiver?.Send<int>(twice(2));
        Worker.Create<string>();
        var worker = new Worker();
        return fallback();
    }
}
"#;
        let pf = parse_file("Worker.cs", src).unwrap();
        let names: Vec<_> = pf.fns.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "IWorker", "Missing", "Base", "Absent", "Worker", "Worker", "~Worker", "Helper",
                "Run", "Local", "twice", "fallback"
            ]
        );
        let calls: Vec<_> = pf
            .calls
            .iter()
            .map(|c| (pf.fns[c.caller].name.as_str(), c.name.as_str()))
            .collect();
        assert_eq!(
            calls,
            [
                ("Worker", "Setup"),
                ("~Worker", "Cleanup"),
                ("Local", "Helper"),
                ("twice", "Local"),
                ("fallback", "Helper"),
                ("Run", "Send"),
                ("Run", "twice"),
                ("Run", "Create"),
                ("Run", "Worker"),
                ("Run", "fallback"),
            ]
        );
        let run = pf.fns.iter().find(|f| f.name == "Run").unwrap();
        assert!(
            src.lines()
                .nth(run.start_row)
                .unwrap()
                .contains("int Run()")
        );
        assert_eq!(src.lines().nth(run.end_row - 1).unwrap().trim(), "}");
    }

    #[test]
    fn c_definitions_follow_nested_declarators_and_ignore_prototypes() {
        let src = r#"
int forward(int value);
int helper(int value) { return value + 1; }
char *pointer(void) { return external(); }
int (*factory(void))(int) { return helper; }
int run(int (*callback)(int)) {
    return helper(callback(2)) + (*callback)(3);
}
"#;
        let pf = parse_file("app.c", src).unwrap();
        let names: Vec<_> = pf.fns.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["helper", "pointer", "factory", "run"]);
        let calls: Vec<_> = pf
            .calls
            .iter()
            .map(|c| (pf.fns[c.caller].name.as_str(), c.name.as_str()))
            .collect();
        assert_eq!(
            calls,
            [
                ("pointer", "external"),
                ("run", "helper"),
                ("run", "callback"),
                ("run", "callback"),
            ]
        );
    }

    #[test]
    fn cpp_headers_methods_templates_and_bound_lambdas() {
        let src = r#"
namespace api {
    template<typename T> T helper(T x) { return x + 1; }
}
struct Worker {
    Worker() { init(); }
    ~Worker() { cleanup(); }
    int run();
    int operator()(int x) { return api::helper<int>(x); }
};
int &reference() { return value; }
int Worker::run() {
    auto twice = ([](int x) { return api::helper<int>(x); });
    worker->send<int>(twice(2));
    auto other = new Worker();
    return (*callback)(3);
}
"#;
        let pf = parse_file("worker.h", src).unwrap();
        let names: Vec<_> = pf.fns.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "helper",
                "Worker",
                "Worker",
                "~Worker",
                "run",
                "operator()",
                "reference",
                "run",
                "twice"
            ]
        );
        let calls: Vec<_> = pf
            .calls
            .iter()
            .map(|c| (pf.fns[c.caller].name.as_str(), c.name.as_str()))
            .collect();
        assert_eq!(
            calls,
            [
                ("Worker", "init"),
                ("~Worker", "cleanup"),
                ("operator()", "helper"),
                ("twice", "helper"),
                ("run", "send"),
                ("run", "twice"),
                ("run", "Worker"),
                ("run", "callback"),
            ]
        );
    }

    #[test]
    fn native_renames_preserve_hashes_but_signature_changes_do_not() {
        for (path, source) in [
            ("app.c", "int *old(int x) { return helper(x); }"),
            ("app.cpp", "int &Worker::old(int x) { return helper(x); }"),
            ("app.C", "auto old = [](int x) { return helper(x); };"),
            ("app.cs", "class App { int old(int x) => helper(x); }"),
        ] {
            let base = parse_file(path, source).unwrap();
            let renamed = parse_file(path, &source.replace("old", "renamed")).unwrap();
            let old = base.fns.iter().find(|f| f.name == "old").unwrap();
            let new = renamed.fns.iter().find(|f| f.name == "renamed").unwrap();
            assert_eq!(old.body_hash, new.body_hash, "{path}");
            let changed = parse_file(path, &source.replace("int x", "long x")).unwrap();
            assert_ne!(
                old.body_hash,
                changed
                    .fns
                    .iter()
                    .find(|f| f.name == "old")
                    .unwrap()
                    .body_hash,
                "{path}"
            );
        }
        let methods =
            parse_file("worker.cpp", "struct Worker { Worker() {} ~Worker() {} };").unwrap();
        assert_ne!(
            methods.fns[1].body_hash, methods.fns[2].body_hash,
            "a constructor becoming a destructor is not a pure rename"
        );
    }

    #[test]
    fn file_convention_test_detection() {
        let pf = parse_file("tests/it.rs", "fn a() {}\n").unwrap(); // rust integration test dir
        assert!(pf.fns[0].is_test);
        let pf = parse_file("src/it.rs", "fn a() {}\n").unwrap(); // rust: no file convention
        assert!(!pf.fns[0].is_test);
        let pf = parse_file("x_test.go", "func a() {}\n").unwrap();
        assert!(pf.fns[0].is_test);
        let pf = parse_file("test_db.py", "def a(): pass\n").unwrap();
        assert!(pf.fns[0].is_test);
        let pf = parse_file("src/app.test.ts", "function a() {}\n").unwrap();
        assert!(pf.fns[0].is_test);
        for (path, production, source) in [
            (
                "WorkerTests.cs",
                "Worker.cs",
                "class Worker { void Run() {} }",
            ),
            (
                "tests/worker.cs",
                "src/worker.cs",
                "class Worker { void Run() {} }",
            ),
            ("test_worker.c", "worker.c", "void run(void) {}"),
            ("worker_test.cpp", "worker.cpp", "void run() {}"),
            ("tests/worker.hpp", "src/worker.hpp", "inline void run() {}"),
        ] {
            assert!(parse_file(path, source).unwrap().fns[0].is_test, "{path}");
            assert!(
                !parse_file(production, source).unwrap().fns[0].is_test,
                "{production}"
            );
        }
        let contest = parse_file("Contest.cs", "class Contest { void Run() {} }").unwrap();
        assert!(
            !contest.fns[0].is_test,
            "production names ending in lowercase 'test' stay visible"
        );
    }

    fn symbol<'a>(parsed: &'a ParsedFile, kind: SymbolKind, qual: &str) -> &'a FnDef {
        parsed
            .fns
            .iter()
            .find(|f| f.kind == kind && f.qual == qual)
            .unwrap_or_else(|| panic!("missing {kind:?} {qual}: {:?}", parsed.fns))
    }

    #[test]
    fn bodyless_contracts_keep_owner_signature_and_arity() {
        for (path, src, owner, name, arity) in [
            (
                "a.cs",
                "interface IWorker : IOther { int Run(string value); }",
                "IWorker",
                "Run",
                1,
            ),
            (
                "a.cs",
                "abstract class Base { public abstract int Run(int x, int y); }",
                "Base",
                "Run",
                2,
            ),
            (
                "a.ts",
                "interface IWorker extends IOther { run(value: string): number; }",
                "IWorker",
                "run",
                1,
            ),
            (
                "a.tsx",
                "abstract class Base extends Parent { abstract run(x: number): void; }",
                "Base",
                "run",
                1,
            ),
            (
                "a.rs",
                "trait Worker: Other { fn run(&self, x: i32); }",
                "Worker",
                "run",
                1,
            ),
            (
                "a.go",
                "package a\ntype Worker interface { Other; Run(x int) error }",
                "Worker",
                "Run",
                1,
            ),
            (
                "a.cpp",
                "struct Worker : public Other { virtual int run(int x) const = 0; };",
                "Worker",
                "run",
                1,
            ),
        ] {
            let parsed = parse_file(path, src).unwrap();
            let method = symbol(
                &parsed,
                SymbolKind::Declaration,
                &format!("{owner}::{name}"),
            );
            assert_eq!(method.owner.as_deref(), Some(owner), "{path}");
            assert_eq!(method.arity, Some(arity), "{path}");
            assert!(method.sig.contains('('), "{path}");
            assert!(method.dispatch, "{path}: {method:?}");
            assert!(!method.is_static, "{path}");
        }
        let parsed = parse_file("a.ts", "declare function run(x: number): string;").unwrap();
        let run = symbol(&parsed, SymbolKind::Declaration, "run");
        assert_eq!(run.arity, Some(1));
        assert!(!run.dispatch);
    }

    #[test]
    fn inheritance_headers_are_independent_review_symbols() {
        for (path, source, derived, bases) in [
            (
                "a.cs",
                "class Child : Parent, IWorker { int run() { return 1; } }",
                "Child",
                vec!["Parent", "IWorker"],
            ),
            (
                "a.ts",
                "class Child extends Parent implements IWorker { run() { return 1; } }",
                "Child",
                vec!["Parent", "IWorker"],
            ),
            (
                "a.js",
                "class Child extends Parent { run() { return 1; } }",
                "Child",
                vec!["Parent"],
            ),
            (
                "a.py",
                "class Child(Parent, IWorker):\n def run(self): return 1\n",
                "Child",
                vec!["Parent", "IWorker"],
            ),
            (
                "a.rs",
                "trait Child: Parent + IWorker { fn run(&self) -> i32 { 1 } }",
                "Child",
                vec!["Parent", "IWorker"],
            ),
            (
                "a.cpp",
                "class Child : public virtual Parent, private ns::IWorker { int run() { return 1; } };",
                "Child",
                vec!["Parent", "ns::IWorker"],
            ),
        ] {
            let before = parse_file(path, source).unwrap();
            let changed = parse_file(
                path,
                &source
                    .replace("return 1", "return 2")
                    .replace("{ 1 }", "{ 2 }"),
            )
            .unwrap();
            let a = symbol(&before, SymbolKind::Type, derived);
            let b = symbol(&changed, SymbolKind::Type, derived);
            assert_eq!(a.bases, bases, "{path}");
            assert_eq!(
                a.body_hash, b.body_hash,
                "member edit changed {path} header"
            );
            assert_eq!(a.tokens, b.tokens, "{path}");
            let renamed = parse_file(path, &source.replace("Child", "Renamed")).unwrap();
            assert_eq!(
                a.body_hash,
                symbol(&renamed, SymbolKind::Type, "Renamed").body_hash,
                "{path}"
            );
            let heritage = parse_file(path, &source.replace("Parent", "Different")).unwrap();
            assert_ne!(
                a.body_hash,
                symbol(&heritage, SymbolKind::Type, derived).body_hash,
                "{path}"
            );
        }
        let parsed = parse_file("a.cs", "class Child : Parent\n{\n int run() => 1;\n}\n").unwrap();
        let header = symbol(&parsed, SymbolKind::Type, "Child");
        assert_eq!((header.start_row, header.end_row), (0, 1));
    }

    #[test]
    fn rust_implementation_headers_keep_real_source_and_qualified_owners() {
        let parsed = parse_file("a.rs", "mod n { struct Worker; trait Job { fn run(&self); } impl Job for Worker {} impl Worker { fn start(&self) { self.run(); } } }").unwrap();
        let ty = symbol(&parsed, SymbolKind::Type, "n::Worker");
        assert!(ty.bases.is_empty());
        let implementation = symbol(&parsed, SymbolKind::Type, "n::<Worker as Job>");
        assert_eq!(implementation.bases, ["Job"]);
        assert_eq!(implementation.owner.as_deref(), Some("n"));
        symbol(&parsed, SymbolKind::Type, "n::<Worker>");
        let method = symbol(&parsed, SymbolKind::Function, "n::Worker::start");
        assert_eq!(method.owner.as_deref(), Some("n::Worker"));
        assert_eq!(
            parsed
                .calls
                .iter()
                .find(|c| c.name == "run")
                .unwrap()
                .receiver
                .as_deref(),
            Some("n::Worker")
        );
        let changed = parse_file("a.rs", "mod n { impl Other for Worker {} }").unwrap();
        assert_ne!(
            implementation.body_hash,
            symbol(&changed, SymbolKind::Type, "n::<Worker as Other>").body_hash
        );
        let parsed = parse_file(
            "a.rs",
            "impl<T> api::Job<T> for api::Worker<T> { fn run(&self) {} }",
        )
        .unwrap();
        let method = symbol(
            &parsed,
            SymbolKind::Function,
            "<api::Worker as api::Job<T>>::run",
        );
        assert_eq!(method.bases, ["api::Job<T>"]);
    }

    #[test]
    fn typed_receivers_are_retained_without_guessing_unknown_variables() {
        for (path, source, ty) in [
            (
                "a.cs",
                "class Use { void Run(IWorker worker) { worker.Execute(); IWorker local = worker; local.Execute(); unknown.Execute(); } }",
                "IWorker",
            ),
            (
                "a.ts",
                "function run(worker: IWorker) { worker.execute(); let local: IWorker; local.execute(); unknown.execute(); }",
                "IWorker",
            ),
            (
                "a.py",
                "def run(worker: IWorker):\n worker.execute()\n local: IWorker = worker\n local.execute()\n unknown.execute()\n",
                "IWorker",
            ),
            (
                "a.rs",
                "fn run(worker: &dyn IWorker) { worker.execute(); let local: &dyn IWorker = worker; local.execute(); unknown.execute(); }",
                "IWorker",
            ),
            (
                "a.go",
                "package a\nfunc run(worker IWorker) { worker.Execute(); var local IWorker; local.Execute(); unknown.Execute() }",
                "IWorker",
            ),
            (
                "a.cpp",
                "void run(IWorker& worker) { worker.execute(); IWorker& local = worker; local.execute(); unknown.execute(); }",
                "IWorker",
            ),
        ] {
            let parsed = parse_file(path, source).unwrap();
            let receivers: Vec<_> = parsed
                .calls
                .iter()
                .filter(|c| c.name.eq_ignore_ascii_case("execute"))
                .map(|c| c.receiver.as_deref())
                .collect();
            assert_eq!(
                receivers,
                [Some(ty), Some(ty), None],
                "{path}: {:?}",
                parsed.calls
            );
        }
        let parsed = parse_file("a.ts", "function run(worker: IWorker) { worker.execute(); { let worker; worker.execute(); } worker.execute(); const nested = (worker) => worker.execute(); }").unwrap();
        let receivers: Vec<_> = parsed
            .calls
            .iter()
            .filter(|c| c.name == "execute")
            .map(|c| c.receiver.as_deref())
            .collect();
        assert_eq!(receivers, [Some("IWorker"), None, Some("IWorker"), None]);
    }

    #[test]
    fn self_base_and_explicit_static_receivers_are_conservative() {
        for (path, source, method) in [
            (
                "a.cs",
                "class Child : Parent { void run() { this.execute(); base.execute(); Child.execute(); unknown.execute(); } }",
                "execute",
            ),
            (
                "a.ts",
                "class Child extends Parent { run() { this.execute(); super.execute(); Child.execute(); unknown.execute(); } }",
                "execute",
            ),
            (
                "a.py",
                "class Child(Parent):\n def run(self):\n  self.execute()\n  super().execute()\n  Child.execute()\n  unknown.execute()\n",
                "execute",
            ),
            (
                "a.cpp",
                "class Child : Parent { void run() { this->execute(); Parent::execute(); Child::execute(); unknown.execute(); } };",
                "execute",
            ),
        ] {
            let parsed = parse_file(path, source).unwrap();
            let receivers: Vec<_> = parsed
                .calls
                .iter()
                .filter(|c| c.name == method)
                .map(|c| c.receiver.as_deref())
                .collect();
            assert_eq!(
                receivers,
                [Some("Child"), Some("Parent"), Some("Child"), None],
                "{path}"
            );
            let dispatch: Vec<_> = parsed
                .calls
                .iter()
                .filter(|c| c.name == method)
                .map(|c| c.dispatch)
                .collect();
            let expected = if path == "a.cpp" {
                [true, false, false, true]
            } else {
                [true, false, true, true]
            };
            assert_eq!(dispatch, expected, "{path}");
        }
    }

    #[test]
    fn cpp_object_qualified_base_calls_suppress_virtual_dispatch() {
        let parsed = parse_file("a.cpp", "struct Child : Parent { void run(Child& other) { this->Parent::execute(); other.Parent::execute(); other.execute(); } };").unwrap();
        let calls: Vec<_> = parsed
            .calls
            .iter()
            .filter(|c| c.name == "execute")
            .map(|c| (c.receiver.as_deref(), c.dispatch))
            .collect();
        assert_eq!(
            calls,
            [
                (Some("Parent"), false),
                (Some("Parent"), false),
                (Some("Child"), true)
            ]
        );
    }

    #[test]
    fn static_members_and_nested_python_functions_are_not_dispatch_methods() {
        for (path, source, qual) in [
            ("a.cs", "class A { public static void run() {} }", "A::run"),
            ("a.ts", "class A { static run() {} }", "A::run"),
            ("a.cpp", "class A { static void run(); };", "A::run"),
            ("a.rs", "impl A { fn run() {} }", "A::run"),
            (
                "a.py",
                "class A:\n @staticmethod\n def run(): pass\n",
                "A::run",
            ),
            (
                "a.py",
                "class A:\n @classmethod\n def run(cls): pass\n",
                "A::run",
            ),
        ] {
            let parsed = parse_file(path, source).unwrap();
            let method = parsed.fns.iter().find(|f| f.qual == qual).unwrap();
            assert!(method.is_static, "{path}");
            assert!(!method.dispatch, "{path}");
        }
        let parsed =
            parse_file("a.py", "class A:\n def run(self):\n  def nested(): pass\n").unwrap();
        assert!(!symbol(&parsed, SymbolKind::Function, "A::run::nested").dispatch);
    }

    #[test]
    fn cpp_contract_declarations_exclude_free_prototypes_and_pointer_fields() {
        let parsed = parse_file("a.hpp", "void free(int); namespace api { void other(); } struct Worker { void run(); int (*callback)(int); void body() { void local(); } };").unwrap();
        let declarations: Vec<_> = parsed
            .fns
            .iter()
            .filter(|f| f.kind == SymbolKind::Declaration)
            .map(|f| f.qual.as_str())
            .collect();
        assert_eq!(declarations, ["Worker::run"]);
        assert!(
            parse_file(
                "a.c",
                "void free(int); struct Worker { int (*callback)(int); };"
            )
            .unwrap()
            .fns
            .is_empty()
        );
    }

    #[test]
    fn recursive_rename_evidence_preserves_literals_comments_and_multisets() {
        let before = parse_file(
            "a.rs",
            "fn old(x: i32) -> i32 { if x == 0 { 0 } else { old(x - 1) + old(x - 2) } }",
        )
        .unwrap();
        let renamed = parse_file("a.rs", "fn renamed(x: i32) -> i32 { if x == 0 { 0 } else { renamed(x - 1) + renamed(x - 2) } }").unwrap();
        assert_ne!(before.fns[0].body_hash, renamed.fns[0].body_hash);
        assert_eq!(before.fns[0].rename_hash, renamed.fns[0].rename_hash);
        assert_eq!(before.fns[0].tokens, renamed.fns[0].tokens);
        assert!(before.fns[0].tokens.windows(2).all(|p| p[0] <= p[1]));
        let comments = parse_file("a.rs", "fn old() { /* old */ print(\"old\"); }").unwrap();
        let renamed =
            parse_file("a.rs", "fn renamed() { /* renamed */ print(\"renamed\"); }").unwrap();
        assert_ne!(comments.fns[0].rename_hash, renamed.fns[0].rename_hash);
        assert_ne!(comments.fns[0].tokens, renamed.fns[0].tokens);
        let commented = parse_file("a.rs", "fn old(x: i32) -> i32 { /* explanatory */ if x == 0 { 0 } else { old(x - 1) + old(x - 2) } }").unwrap();
        assert_eq!(before.fns[0].tokens, commented.fns[0].tokens);
    }

    #[test]
    fn same_line_impls_and_partial_types_keep_exact_member_containers() {
        for (path, source) in [
            (
                "a.rs",
                "impl Worker { fn first(&self) {} } impl Worker { fn second(&self) {} }",
            ),
            (
                "a.cs",
                "partial class Worker { void first() {} } partial class Worker { void second() {} }",
            ),
        ] {
            let parsed = parse_file(path, source).unwrap();
            let headers: Vec<_> = parsed
                .fns
                .iter()
                .filter(|f| f.kind == SymbolKind::Type)
                .collect();
            assert_eq!(headers.len(), 2, "{path}");
            let first = symbol(&parsed, SymbolKind::Function, "Worker::first");
            let second = symbol(&parsed, SymbolKind::Function, "Worker::second");
            assert_eq!(first.container_start, Some(headers[0].start_byte), "{path}");
            assert_eq!(
                second.container_start,
                Some(headers[1].start_byte),
                "{path}"
            );
            assert_ne!(first.container_start, second.container_start, "{path}");
            assert_eq!(
                first.start_row, second.start_row,
                "fixture must exercise same-row ownership"
            );
        }
        let parsed = parse_file("a.cs", "class Worker { void Run() { void Local() {} } }").unwrap();
        assert_eq!(
            symbol(&parsed, SymbolKind::Function, "Worker::Run::Local").container_start,
            None
        );
    }

    #[test]
    fn arity_ignores_nested_generic_commas_and_explicit_receivers() {
        for (path, source, qual, arity) in [
            (
                "a.cs",
                "interface I { void run(Dictionary<string, int> value, int other); }",
                "I::run",
                2,
            ),
            (
                "a.ts",
                "declare function run(this : I, value: Map<string, number>, cb: (a: number, b: number) => void): void;",
                "run",
                2,
            ),
            (
                "a.py",
                "def run(self, value='a,b', *, other=1): pass",
                "run",
                2,
            ),
            (
                "a.rs",
                "trait I { fn run(&'a self, value: Result<i32, Error>); }",
                "I::run",
                1,
            ),
            (
                "a.cpp",
                "struct I { void run(std::pair<int, int> value, void (*callback)(int, int)); };",
                "I::run",
                2,
            ),
        ] {
            let parsed = parse_file(path, source).unwrap();
            let method = parsed.fns.iter().find(|f| f.qual == qual).unwrap();
            assert_eq!(method.arity, Some(arity), "{path}");
        }
    }
}

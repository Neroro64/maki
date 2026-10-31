use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use maki_agent::ToolDoneEvent;
use maki_agent::agent::tool_dispatch;
use maki_agent::tools::{
    CallOrigin, FileAccess, FileKey, SymbolLease, ToolAudience, ToolContext, ToolRegistry,
    cli_tool_ctx,
};
use maki_config::PluginsConfig;
use maki_lua::PluginHost;
use serde_json::{Value, json};
use tempfile::TempDir;
use test_case::test_case;

const FIRST: &str = "first-worker";
const SECOND: &str = "second-worker";
const FIRST_INTENT: &str = "editing first symbol";
const SECOND_INTENT: &str = "editing second symbol";
const FILE_SYMBOL: &str = "<file>";
const RUST_SOURCE: &str = "fn foo() -> i32 { 1 }\nfn bar() -> i32 { 2 }\n";
const FOO: &str = "fn foo() -> i32 { 1 }";
const FOO_EXPANDED: &str = "fn foo() -> i32 {\n    let answer = 10;\n    answer\n}";
const FOO_SECOND: &str = "fn foo() -> i32 { 30 }";
const BAR: &str = "fn bar() -> i32 { 2 }";
const BAR_EXPANDED: &str = "fn bar() -> i32 {\n    20\n}";
const NUMBERED_SOURCE: &str = "fn foo() -> i32 {\n    1\n}\nfn bar() -> i32 {\n    2\n}\n";
const NUMBERED_FOO: &str = "fn foo() -> i32 {\n    1\n}";
const OUTSIDE_SOURCE: &str = "// original\nfn foo() -> i32 { 1 }\nfn bar() -> i32 { 2 }\n";
const STALE_PREFIX: &str = "stale edit: symbol ";
const MAIN_STALE_ERROR: &str = "file changed since last read";

struct Fixture {
    _directory: TempDir,
    path: PathBuf,
    registry: Arc<ToolRegistry>,
    _host: PluginHost,
    access: Arc<FileAccess>,
}

impl Fixture {
    fn new(name: &str, source: &str) -> Self {
        let directory = TempDir::new().expect("fixture directory");
        let path = directory.path().join(name);
        fs::write(&path, source).expect("fixture source");
        let registry = Arc::new(ToolRegistry::new());
        let mut host = PluginHost::new(Arc::clone(&registry)).expect("Lua host");
        host.load_builtins(&PluginsConfig {
            enabled: true,
            names: vec!["edit".to_owned(), "write".to_owned()],
            packages: Vec::new(),
            opts: HashMap::from([(
                "edit".to_owned(),
                json!({ "insert_lines": true })
                    .as_object()
                    .expect("options")
                    .clone(),
            )]),
        })
        .expect("real edit/write builtins");
        Self {
            _directory: directory,
            path,
            registry,
            _host: host,
            access: FileAccess::fresh(),
        }
    }

    fn task(&self, id: &str, intent: &str) -> (ToolContext, SymbolLease) {
        let (access, lease) = self.access.task_scope(id, intent);
        let mut ctx = cli_tool_ctx();
        ctx.registry = Arc::clone(&self.registry);
        ctx.audience = ToolAudience::GENERAL_SUB;
        ctx.task_id = Some(Arc::from(id));
        ctx.file_access = access;
        self.read(&ctx);
        (ctx, lease)
    }

    fn read(&self, ctx: &ToolContext) {
        ctx.file_access.record_read(&FileKey::new(&self.path));
    }

    fn source(&self) -> String {
        fs::read_to_string(&self.path).expect("read fixture")
    }

    fn edit(&self, ctx: &ToolContext, old: &str, new: &str) -> ToolDoneEvent {
        self.run(ctx, "edit", edit_input(&self.path, old, new))
    }

    fn run(&self, ctx: &ToolContext, tool: &str, input: Value) -> ToolDoneEvent {
        smol::block_on(tool_dispatch::run(
            format!("{tool}-{}", ctx.task_id.as_deref().unwrap_or("main")),
            tool,
            &input,
            ctx,
            CallOrigin::Model,
        ))
    }
}

fn edit_input(path: &Path, old: &str, new: &str) -> Value {
    json!({ "path": path, "old_string": old, "new_string": new })
}

fn assert_success(result: &ToolDoneEvent) {
    assert!(!result.is_error, "{}", result.output.as_text());
}

fn assert_conflict(result: &ToolDoneEvent, symbol: &str, fixture: &Fixture) {
    assert!(result.is_error, "expected a symbol conflict");
    let expected = format!(
        "Conflict: Subagent {FIRST} ({FIRST_INTENT}) currently holds lock on symbol {symbol} in {}",
        FileKey::new(&fixture.path).as_path().display(),
    );
    let output = result.output.as_text();
    assert!(output.contains(&expected), "{output}");
}

#[test_case("source.rs", RUST_SOURCE, FOO, FOO_EXPANDED, BAR, BAR_EXPANDED; "rust_functions")]
#[test_case("source.py", "def foo():\n    return 1\ndef bar():\n    return 2\n", "def foo():\n    return 1", "def foo():\n    answer = 10\n    return answer", "def bar():\n    return 2", "def bar():\n    return 20"; "python_functions")]
#[test_case("source.js", "function foo() { return 1; }\nfunction bar() { return 2; }\n", "function foo() { return 1; }", "function foo() {\n  const answer = 10;\n  return answer;\n}", "function bar() { return 2; }", "function bar() {\n  return 20;\n}"; "javascript_functions")]
#[test_case("source.ts", "function foo(): number { return 1; }\nfunction bar(): number { return 2; }\n", "function foo(): number { return 1; }", "function foo(): number {\n  const answer = 10;\n  return answer;\n}", "function bar(): number { return 2; }", "function bar(): number {\n  return 20;\n}"; "typescript_functions")]
#[test_case("source.go", "package example\nfunc foo() int { return 1 }\nfunc bar() int { return 2 }\n", "func foo() int { return 1 }", "func foo() int {\n  answer := 10\n  return answer\n}", "func bar() int { return 2 }", "func bar() int {\n  return 20\n}"; "go_functions")]
#[test_case("source.py", "class Item:\n    def foo(self):\n        return 1\n    def bar(self):\n        return 2\n", "        return 1", "        answer = 10\n        return answer", "        return 2", "        return 20"; "same_class_disjoint_methods")]
#[test_case("source.rs", "struct Item;\nimpl Item {\n    fn foo(&self) -> i32 { 1 }\n    fn bar(&self) -> i32 { 2 }\n}\n", "    fn foo(&self) -> i32 { 1 }", "    fn foo(&self) -> i32 {\n        let answer = 10;\n        answer\n    }", "    fn bar(&self) -> i32 { 2 }", "    fn bar(&self) -> i32 {\n        20\n    }"; "same_impl_disjoint_methods")]
#[test_case("source.py", "class A:\n    def run(self):\n        return 1\nclass B:\n    def run(self):\n        return 2\n", "        return 1", "        answer = 10\n        return answer", "        return 2", "        return 20"; "qualified_same_method_names")]
#[test_case("source.ts", "class Item {\n  foo(): number { return 1; }\n  bar(): number { return 2; }\n}\n", "  foo(): number { return 1; }", "  foo(): number {\n    const answer = 10;\n    return answer;\n  }", "  bar(): number { return 2; }", "  bar(): number {\n    return 20;\n  }"; "typescript_methods")]
#[test_case("source.js", "const foo = () => 1;\nconst bar = () => 2;\n", "() => 1", "() => {\n  return 10;\n}", "() => 2", "() => {\n  return 20;\n}"; "javascript_arrow_functions")]
#[test_case("Source.java", "class Item {\n  int foo() { return 1; }\n  int bar() { return 2; }\n}\n", "  int foo() { return 1; }", "  int foo() {\n    int answer = 10;\n    return answer;\n  }", "  int bar() { return 2; }", "  int bar() {\n    return 20;\n  }"; "generic_named_declarations")]
fn simultaneous_disjoint_edits_preserve_both(
    file: &str,
    source: &str,
    old_first: &str,
    new_first: &str,
    old_second: &str,
    new_second: &str,
) {
    let fixture = Fixture::new(file, source);
    let (first, _first_lease) = fixture.task(FIRST, FIRST_INTENT);
    let (second, _second_lease) = fixture.task(SECOND, SECOND_INTENT);
    let first_input = edit_input(&fixture.path, old_first, new_first);
    let second_input = edit_input(&fixture.path, old_second, new_second);
    let (a, b) = smol::block_on(futures_lite::future::zip(
        tool_dispatch::run(
            "first-edit".into(),
            "edit",
            &first_input,
            &first,
            CallOrigin::Model,
        ),
        tool_dispatch::run(
            "second-edit".into(),
            "edit",
            &second_input,
            &second,
            CallOrigin::Model,
        ),
    ));
    assert_success(&a);
    assert_success(&b);
    assert_eq!(
        fixture.source(),
        source
            .replace(old_first, new_first)
            .replace(old_second, new_second)
    );
}

#[test_case("source.rs", RUST_SOURCE, FOO, FOO_EXPANDED, "foo"; "same_function")]
#[test_case("source.rs", "struct Item;\nimpl Item {\n    fn run(&self) -> i32 { 1 }\n}\n", "    fn run(&self) -> i32 { 1 }", "    fn run(&self) -> i32 {\n        10\n    }", "impl Item::run"; "same_rust_method")]
#[test_case("source.py", "class Item:\n    def run(self):\n        return 1\n", "        return 1", "        answer = 10\n        return answer", "Item::run"; "same_python_method")]
#[test_case("source.custom", RUST_SOURCE, FOO, FOO_EXPANDED, FILE_SYMBOL; "unsupported_grammar")]
#[test_case("source.rs", "fn foo() -> i32 { 1 }\nfn foo() -> i32 { 2 }\n", FOO, FOO_EXPANDED, FILE_SYMBOL; "duplicate_declarations")]
fn same_symbol_conflicts(file: &str, source: &str, old: &str, new: &str, symbol: &str) {
    let fixture = Fixture::new(file, source);
    let (first, _first_lease) = fixture.task(FIRST, FIRST_INTENT);
    let (second, _second_lease) = fixture.task(SECOND, SECOND_INTENT);
    assert_success(&fixture.edit(&first, old, new));
    let committed = fixture.source();
    assert_conflict(&fixture.edit(&second, old, new), symbol, &fixture);
    assert_eq!(fixture.source(), committed);
}

#[test_case("edit_lines", json!({ "start": 5, "end": 5, "new_string": "    20" }), "    2", "    20"; "replace_original_line")]
#[test_case("insert_lines", json!({ "line": 4, "new_string": "    let answer = 20;" }), "fn bar() -> i32 {\n", "fn bar() -> i32 {\n    let answer = 20;\n"; "insert_at_original_line")]
fn numbered_edits_merge_using_original_positions(
    tool: &str,
    mut input: Value,
    old: &str,
    new: &str,
) {
    let fixture = Fixture::new("source.rs", NUMBERED_SOURCE);
    let (first, _first_lease) = fixture.task(FIRST, FIRST_INTENT);
    let (second, _second_lease) = fixture.task(SECOND, SECOND_INTENT);
    assert_success(&fixture.edit(&first, NUMBERED_FOO, FOO_EXPANDED));
    input["path"] = json!(fixture.path);
    assert_success(&fixture.run(&second, tool, input));
    assert_eq!(
        fixture.source(),
        NUMBERED_SOURCE
            .replace(NUMBERED_FOO, FOO_EXPANDED)
            .replace(old, new)
    );
}

#[test]
fn multiedit_merges_all_changes_atomically() {
    let fixture = Fixture::new("source.rs", RUST_SOURCE);
    let (first, _first_lease) = fixture.task(FIRST, FIRST_INTENT);
    let (second, _second_lease) = fixture.task(SECOND, SECOND_INTENT);
    assert_success(&fixture.edit(&first, FOO, FOO_EXPANDED));
    let committed = fixture.source();
    let result = fixture.run(
        &second,
        "multiedit",
        json!({
            "path": fixture.path,
            "edits": [
                { "old_string": BAR, "new_string": BAR_EXPANDED },
                { "old_string": FOO, "new_string": FOO_SECOND },
            ],
        }),
    );
    assert_conflict(&result, "foo", &fixture);
    assert_eq!(fixture.source(), committed);
    assert_success(&fixture.run(
        &second,
        "multiedit",
        json!({
            "path": fixture.path,
            "edits": [
                { "old_string": BAR, "new_string": "fn bar() -> i32 { 20 }" },
                { "old_string": "{ 20 }", "new_string": "{\n    20\n}" },
            ],
        }),
    ));
    assert_eq!(
        fixture.source(),
        RUST_SOURCE
            .replace(FOO, FOO_EXPANDED)
            .replace(BAR, BAR_EXPANDED)
    );
}

#[test]
fn file_wide_claim_blocks_targeted_edits() {
    let fixture = Fixture::new("source.rs", OUTSIDE_SOURCE);
    let (first, _first_lease) = fixture.task(FIRST, FIRST_INTENT);
    let (second, _second_lease) = fixture.task(SECOND, SECOND_INTENT);
    assert_success(&fixture.edit(&first, "// original", "// file-wide change"));
    let committed = fixture.source();
    assert_conflict(
        &fixture.edit(&second, FOO, FOO_SECOND),
        FILE_SYMBOL,
        &fixture,
    );
    assert_eq!(fixture.source(), committed);
}

#[test_case(false; "replace")]
#[test_case(true; "append")]
fn full_write_checks_locks_before_writing(append: bool) {
    let fixture = Fixture::new("source.rs", RUST_SOURCE);
    let (first, _first_lease) = fixture.task(FIRST, FIRST_INTENT);
    let (second, _second_lease) = fixture.task(SECOND, SECOND_INTENT);
    assert_success(&fixture.edit(&first, FOO, FOO_EXPANDED));
    fixture.read(&second);
    let committed = fixture.source();
    let result = fixture.run(&second, "write", json!({
        "path": fixture.path,
        "content": if append { "// appended\n".to_owned() } else { committed.replace(FOO_EXPANDED, FOO_SECOND) },
        "append": append,
    }));
    assert_conflict(&result, "foo", &fixture);
    assert_eq!(fixture.source(), committed);
}

#[test_case(false; "explicit_release")]
#[test_case(true; "drop_release")]
fn release_allows_a_fresh_subsequent_edit(drop_lease: bool) {
    let fixture = Fixture::new("source.rs", RUST_SOURCE);
    let (first, lease) = fixture.task(FIRST, FIRST_INTENT);
    let (second, _second_lease) = fixture.task(SECOND, SECOND_INTENT);
    assert_success(&fixture.edit(&first, FOO, FOO_EXPANDED));
    if drop_lease {
        drop(lease);
    } else {
        lease.release();
        lease.release();
    }
    let stale = fixture.edit(&second, FOO, FOO_SECOND);
    assert!(stale.is_error);
    let output = stale.output.as_text();
    assert!(output.contains(STALE_PREFIX), "{output}");
    fixture.read(&second);
    assert_success(&fixture.edit(&second, FOO_EXPANDED, FOO_SECOND));
    assert_eq!(fixture.source(), RUST_SOURCE.replace(FOO, FOO_SECOND));
}

#[test_case("fn bar() -> i32 { 2 }\nfn baz() -> i32 { 3 }"; "addition")]
#[test_case(""; "deletion")]
fn stale_declaration_additions_and_deletions_do_not_overwrite(new: &str) {
    let fixture = Fixture::new("source.rs", RUST_SOURCE);
    let (first, lease) = fixture.task(FIRST, FIRST_INTENT);
    let (second, _second_lease) = fixture.task(SECOND, SECOND_INTENT);
    assert_success(&fixture.edit(&first, FOO, FOO_EXPANDED));
    lease.release();
    let committed = fixture.source();
    let result = fixture.edit(&second, BAR, new);
    assert!(result.is_error);
    let output = result.output.as_text();
    assert!(
        output.contains(&format!("{STALE_PREFIX}{FILE_SYMBOL}")),
        "{output}"
    );
    assert_eq!(fixture.source(), committed);
}

#[test]
fn rejected_new_file_write_does_not_create_parent_directories() {
    let fixture = Fixture::new("source.rs", RUST_SOURCE);
    let (first, _first_lease) = fixture.task(FIRST, FIRST_INTENT);
    let (second, _second_lease) = fixture.task(SECOND, SECOND_INTENT);
    let parent = fixture._directory.path().join("missing/nested");
    let path = parent.join("source.rs");
    first
        .file_access
        .claim_symbols(&FileKey::new(&path), FIRST, &[FILE_SYMBOL.to_owned()])
        .expect("hold new-file wildcard");
    let result = fixture.run(
        &second,
        "write",
        json!({ "path": path, "content": RUST_SOURCE }),
    );
    assert!(result.is_error);
    let output = result.output.as_text();
    let expected = format!(
        "Conflict: Subagent {FIRST} ({FIRST_INTENT}) currently holds lock on symbol {FILE_SYMBOL}"
    );
    assert!(output.contains(&expected), "{output}");
    assert!(!parent.exists());
    assert!(!path.exists());
}

#[test_case(false; "replacement")]
#[test_case(true; "append")]
fn full_write_uses_the_complete_final_source(append: bool) {
    let fixture = Fixture::new("source.rs", RUST_SOURCE);
    let (first, _lease) = fixture.task(FIRST, FIRST_INTENT);
    let content = "fn baz() -> i32 { 3 }\n";
    let result = fixture.run(
        &first,
        "write",
        json!({
            "path": fixture.path,
            "content": content,
            "append": append,
        }),
    );
    assert_success(&result);
    let expected = if append {
        format!("{RUST_SOURCE}{content}")
    } else {
        content.to_owned()
    };
    assert_eq!(fixture.source(), expected);
}

#[test]
fn main_agent_retains_strict_stale_read_protection() {
    let fixture = Fixture::new("source.rs", RUST_SOURCE);
    let (first, _lease) = fixture.task(FIRST, FIRST_INTENT);
    let mut main = cli_tool_ctx();
    main.registry = Arc::clone(&fixture.registry);
    main.file_access = Arc::clone(&fixture.access);
    fixture.read(&main);
    assert_success(&fixture.edit(&first, FOO, FOO_EXPANDED));
    let committed = fixture.source();
    let result = fixture.edit(&main, BAR, BAR_EXPANDED);
    assert!(result.is_error);
    let output = result.output.as_text();
    assert!(output.contains(MAIN_STALE_ERROR), "{output}");
    assert_eq!(fixture.source(), committed);
}

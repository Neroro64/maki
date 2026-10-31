use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use maki_agent::ToolDoneEvent;
use maki_agent::agent::tool_dispatch;
use maki_agent::tools::{
    CallOrigin, FileAccess, FileKey, ToolAudience, ToolContext, ToolRegistry, cli_tool_ctx,
};
use maki_config::PluginsConfig;
use maki_lua::PluginHost;
use serde_json::{Value, json};
use tempfile::TempDir;
use test_case::test_case;

const SOURCE: &str = "use safe::api;\nfn first() { safe::api(); }\nfn second() { safe::api(); }\n";
const FAILED: &str = "use banned::api;\nfn first() { banned::api(); }\n";
const RULE: &str = "Never import banned::api in Rust source files.";
const GUARD_REJECTED: &str = "guard rejected";
const INVALID_POLICY: &str = "invalid stored guard policy";

struct Fixture {
    directory: TempDir,
    path: PathBuf,
    registry: Arc<ToolRegistry>,
    host: PluginHost,
}

impl Fixture {
    fn new(source: &str) -> Self {
        let directory = TempDir::new().expect("fixture directory");
        let path = directory.path().join("source.rs");
        fs::write(&path, source).expect("fixture source");
        let (registry, host) = host(directory.path().to_string_lossy().as_ref());
        Self {
            directory,
            path,
            registry,
            host,
        }
    }

    fn context(&self) -> ToolContext {
        let mut ctx = cli_tool_ctx();
        ctx.registry = Arc::clone(&self.registry);
        ctx.file_access.record_read(&FileKey::new(&self.path));
        ctx
    }

    fn run(&self, name: &str, input: Value) -> ToolDoneEvent {
        smol::block_on(tool_dispatch::run(
            name.to_owned(),
            name,
            &input,
            &self.context(),
            CallOrigin::Model,
        ))
    }

    fn save(&self, proposal: Value) -> ToolDoneEvent {
        self.run("save_guard", json!({ "proposal": proposal.to_string() }))
    }

    fn source(&self) -> String {
        fs::read_to_string(&self.path).expect("fixture source")
    }

    fn policy(&self) -> PathBuf {
        self.directory.path().join(".maki/guards.lua")
    }

    fn reload(&mut self) {
        let (registry, host) = host(self.directory.path().to_string_lossy().as_ref());
        self.host = host;
        self.registry = registry;
    }
}

fn host(root: &str) -> (Arc<ToolRegistry>, PluginHost) {
    let registry = Arc::new(ToolRegistry::new());
    let mut host = PluginHost::new(Arc::clone(&registry)).expect("Lua host");
    host.load_builtins(&PluginsConfig {
        enabled: true,
        names: vec!["edit".to_owned(), "write".to_owned()],
        packages: Vec::new(),
        opts: HashMap::new(),
    })
    .expect("real edit/write builtins");
    let root = serde_json::to_string(root).expect("root literal");
    host.load_source("guard_fixture", &format!(r#"
local guards = require("maki.guards")
local root = {root}
maki.api.register_tool({{
  name = "save_guard",
  description = "Save a proposed reflection constraint",
  schema = {{ type = "object", properties = {{ proposal = {{ type = "string", required = true }} }} }},
  handler = function(input)
    local proposal, err = maki.json.decode(input.proposal)
    if not proposal then return {{ llm_output = err, is_error = true }} end
    local path
    path, err = guards.save(root, proposal)
    return {{ llm_output = path or tostring(err), is_error = not path }}
  end,
}})
"#)).expect("guard compiler consumer");
    (registry, host)
}

fn proposal(kind: &str) -> Value {
    let mut guard = json!({
        "kind": kind,
        "pattern": "banned::api",
        "file_pattern": "*.rs",
        "failed_example": FAILED,
        "fixed_example": SOURCE,
    });
    if kind == "ast" {
        guard["language"] = json!("rust");
        guard["pattern"] = json!("((use_declaration) @import (#match? @import \"banned::api\"))");
    }
    json!({ "kind": "guard", "rule": RULE, "guard": guard })
}

fn success(result: ToolDoneEvent) {
    assert!(!result.is_error, "{}", result.output.as_text());
}

fn rejected(result: ToolDoneEvent, error: &str) {
    assert!(
        result.is_error,
        "expected rejection: {}",
        result.output.as_text()
    );
    assert!(
        result.output.as_text().contains(error),
        "{}",
        result.output.as_text()
    );
}

#[test_case("regex"; "rust_regex_guard")]
#[test_case("ast"; "strict_ast_import_guard")]
fn saved_guard_immediately_blocks_edit_and_write_and_reloads(kind: &str) {
    let mut fixture = Fixture::new(SOURCE);
    success(fixture.save(proposal(kind)));
    let policy = fs::read_to_string(fixture.policy()).expect("persisted policy");
    assert!(policy.starts_with("return ["));
    rejected(
        fixture.run(
            "edit",
            json!({
                "path": fixture.path,
                "old_string": "use safe::api;",
                "new_string": "use banned::api;",
            }),
        ),
        GUARD_REJECTED,
    );
    assert_eq!(fixture.source(), SOURCE);
    rejected(
        fixture.run("write", json!({ "path": fixture.path, "content": FAILED })),
        GUARD_REJECTED,
    );
    assert_eq!(fixture.source(), SOURCE);
    fixture.reload();
    rejected(
        fixture.run("write", json!({ "path": fixture.path, "content": FAILED })),
        GUARD_REJECTED,
    );
    assert_eq!(fixture.source(), SOURCE);
    success(fixture.run(
        "edit",
        json!({
            "path": fixture.path,
            "old_string": "fn first() { safe::api(); }",
            "new_string": "fn first() { safe::api(); safe::api(); }",
        }),
    ));
    assert_eq!(
        fixture.source(),
        SOURCE.replace(
            "fn first() { safe::api(); }",
            "fn first() { safe::api(); safe::api(); }"
        )
    );
    assert_eq!(
        fs::read_to_string(fixture.policy()).expect("unchanged policy"),
        policy
    );
}

#[test_case("regex"; "regex_nonadjacent_changes")]
#[test_case("ast"; "ast_nonadjacent_changes")]
fn unchanged_findings_pass_but_separated_introductions_fail(kind: &str) {
    let baseline = "use banned::api;\nfn first() { safe::api(); }\nfn middle() { safe::api(); }\nfn last() { safe::api(); }\n";
    let fixture = Fixture::new(baseline);
    success(fixture.save(proposal(kind)));
    let safe = baseline
        .replace("fn first()", "fn changed()")
        .replace("fn last()", "fn changed_last()");
    success(fixture.run("write", json!({ "path": fixture.path, "content": safe })));
    assert_eq!(fixture.source(), safe);
    let candidate = safe.replace("fn middle()", "use banned::api;\nfn middle()");
    rejected(
        fixture.run(
            "write",
            json!({ "path": fixture.path, "content": candidate }),
        ),
        GUARD_REJECTED,
    );
    assert_eq!(fixture.source(), safe);
    rejected(
        fixture.run(
            "multiedit",
            json!({ "path": fixture.path, "edits": [
        { "old_string": "fn changed()", "new_string": "fn altered()" },
        { "old_string": "fn changed_last()", "new_string": "use banned::api;\nfn changed_last()" },
    ] }),
        ),
        GUARD_REJECTED,
    );
    assert_eq!(fixture.source(), safe);
}

#[test]
fn a_new_file_is_rejected_before_creating_parent_directories() {
    let fixture = Fixture::new(SOURCE);
    success(fixture.save(proposal("regex")));
    let path = fixture.directory.path().join("new/nested/source.rs");
    rejected(
        fixture.run("write", json!({ "path": path, "content": FAILED })),
        GUARD_REJECTED,
    );
    assert!(!fixture.directory.path().join("new").exists());
}

#[test_case("return (function() error('executed') end)()"; "executable_policy")]
#[test_case("return [[{\"version\":2,\"guards\":[]}]]"; "unsupported_version")]
#[test_case("return [[{\"version\":1,\"guards\":[]}]]; print('executed')"; "executable_suffix")]
#[test_case("return [[not json]]"; "malformed_json")]
#[test_case(""; "empty_policy")]
fn malformed_policy_fails_closed_and_is_not_replaced(stored: &str) {
    let fixture = Fixture::new(SOURCE);
    fs::create_dir_all(fixture.policy().parent().expect("policy parent"))
        .expect("policy directory");
    fs::write(fixture.policy(), stored).expect("malformed policy");
    rejected(
        fixture.run(
            "edit",
            json!({
                "path": fixture.path, "old_string": "fn first()", "new_string": "fn renamed()",
            }),
        ),
        INVALID_POLICY,
    );
    rejected(
        fixture.run(
            "write",
            json!({ "path": fixture.path, "content": SOURCE.replace("first", "renamed") }),
        ),
        INVALID_POLICY,
    );
    rejected(fixture.save(proposal("regex")), INVALID_POLICY);
    assert_eq!(fixture.source(), SOURCE);
    assert_eq!(
        fs::read_to_string(fixture.policy()).expect("retained policy"),
        stored
    );
}

#[test_case("regex", "["; "invalid_regex")]
#[test_case("ast", "((identifier) @id (#mystery? @id \"banned\"))"; "unsupported_predicate")]
#[test_case("ast", "((identifier) @id (#set! danger \"yes\"))"; "unsupported_directive")]
#[test_case("ast", "((identifier) @id (#lua-match? @id \"[\"))"; "invalid_custom_regex_predicate")]
#[test_case("ast", "((identifier) @id (#contains? @id))"; "invalid_predicate_arguments")]
fn invalid_synthesis_preserves_the_previously_saved_policy(kind: &str, pattern: &str) {
    let fixture = Fixture::new(SOURCE);
    success(fixture.save(proposal("regex")));
    let before = fs::read(fixture.policy()).expect("saved policy");
    let mut bad = proposal(kind);
    bad["rule"] = json!("Never call banned::other in Rust source files.");
    bad["guard"]["pattern"] = json!(pattern);
    let result = fixture.save(bad);
    assert!(result.is_error, "{}", result.output.as_text());
    assert_eq!(fs::read(fixture.policy()).expect("retained policy"), before);
    rejected(
        fixture.run("write", json!({ "path": fixture.path, "content": FAILED })),
        GUARD_REJECTED,
    );
}

#[test]
fn examples_and_unknown_proposal_fields_cannot_disable_guards() {
    let fixture = Fixture::new(SOURCE);
    for change in [
        json!({ "failed_example": SOURCE }),
        json!({ "fixed_example": FAILED }),
        json!({ "failed_example": "" }),
        json!({ "pattern": "banned::api|safe::api" }),
        json!({ "action": "allow" }),
    ] {
        let mut bad = proposal("regex");
        for (key, value) in change.as_object().expect("guard changes") {
            bad["guard"][key] = value.clone();
        }
        assert!(fixture.save(bad).is_error);
        assert!(!fixture.policy().exists());
    }
    let mut bad = proposal("regex");
    bad["command"] = json!("echo untrusted");
    assert!(fixture.save(bad).is_error);
    assert!(!fixture.directory.path().join(".maki").exists());
}

#[test_case("Be careful and write good code."; "platitude")]
#[test_case("Never repeat mistakes while writing source files."; "no_concrete_operation")]
#[test_case("Always remember that safe::api is important."; "not_an_imperative")]
#[test_case("Use safe::api.\nNever use banned::api."; "multiple_lines")]
fn behavioral_platitudes_are_rejected_without_persistence(rule: &str) {
    let fixture = Fixture::new(SOURCE);
    assert!(
        fixture
            .save(json!({ "kind": "behavioral", "rule": rule }))
            .is_error
    );
    assert!(!fixture.directory.path().join(".maki").exists());
}

#[test]
fn behavioral_rules_append_preserve_existing_instructions_and_enforce_budget() {
    let fixture = Fixture::new(SOURCE);
    let path = fixture.directory.path().join(".maki/rules.md");
    fs::create_dir_all(path.parent().expect("rules parent")).expect("rules directory");
    let existing = "User instruction: preserve existing project conventions.\n";
    fs::write(&path, existing).expect("existing instructions");
    success(
        fixture.save(
            json!({ "kind": "behavioral", "rule": "Use safe::api for all Rust API imports." }),
        ),
    );
    let saved = fs::read_to_string(&path).expect("saved rules");
    assert!(saved.starts_with(existing));
    assert!(
        fixture
            .save(
                json!({ "kind": "behavioral", "rule": "Use safe::api for all Rust API imports." })
            )
            .is_error
    );
    assert_eq!(fs::read_to_string(&path).expect("retained rules"), saved);
    let full = "x".repeat(2048);
    fs::write(&path, &full).expect("full rule budget");
    assert!(fixture.save(json!({ "kind": "behavioral", "rule": "Never call banned::api in Rust source files." })).is_error);
    assert_eq!(
        fs::read_to_string(&path).expect("retained full rules"),
        full
    );
    let too_long = format!("Never call banned::api {}", "long ".repeat(50));
    assert!(
        fixture
            .save(json!({ "kind": "behavioral", "rule": too_long.trim() }))
            .is_error
    );
    assert_eq!(
        fs::read_to_string(&path).expect("retained full rules"),
        full
    );
}

#[test]
fn total_pattern_budget_rejects_addition_without_losing_existing_guards() {
    let fixture = Fixture::new(SOURCE);
    for index in 0..4 {
        let symbol = format!("banned::api{index}");
        let pattern = format!("{symbol}(?:{})?", "x".repeat(4000));
        success(fixture.save(json!({ "kind": "guard", "rule": format!("Never call {symbol} in Rust source files."), "guard": {
            "kind": "regex", "pattern": pattern, "file_pattern": "*.rs",
            "failed_example": symbol, "fixed_example": SOURCE,
        }})));
    }
    let before = fs::read(fixture.policy()).expect("full patterns");
    let mut next = proposal("regex");
    next["guard"]["pattern"] = json!(format!("banned::api(?:{})?", "y".repeat(500)));
    assert!(fixture.save(next).is_error);
    assert_eq!(fs::read(fixture.policy()).expect("retained guards"), before);
}

#[test]
fn concurrent_saves_from_distinct_plugin_instances_retain_both_constraints() {
    let fixture = Fixture::new(SOURCE);
    let (other_registry, _other_host) = host(fixture.directory.path().to_string_lossy().as_ref());
    let mut other_ctx = cli_tool_ctx();
    other_ctx.registry = other_registry;
    let first = json!({ "proposal": proposal("regex").to_string() });
    let mut other = proposal("regex");
    other["rule"] = json!("Never import other::api in Rust source files.");
    other["guard"]["pattern"] = json!("other::api");
    other["guard"]["failed_example"] = json!("use other::api;");
    let second = json!({ "proposal": other.to_string() });
    let context = fixture.context();
    let (a, b) = smol::block_on(futures_lite::future::zip(
        tool_dispatch::run(
            "first-save".into(),
            "save_guard",
            &first,
            &context,
            CallOrigin::Model,
        ),
        tool_dispatch::run(
            "second-save".into(),
            "save_guard",
            &second,
            &other_ctx,
            CallOrigin::Model,
        ),
    ));
    success(a);
    success(b);
    assert!(fixture.policy().exists());
    rejected(
        fixture.run(
            "write",
            json!({ "path": fixture.path, "content": "use other::api;\n" }),
        ),
        GUARD_REJECTED,
    );
    rejected(
        fixture.run("write", json!({ "path": fixture.path, "content": FAILED })),
        GUARD_REJECTED,
    );
}

#[test]
fn strict_query_preserves_permissive_parse_and_regex_reports_byte_offsets() {
    let fixture = Fixture::new(SOURCE);
    fixture
        .host
        .load_source(
            "guard_primitives",
            r#"
local re, err = maki.regex("banned::api")
assert(re, err)
local first, last = re:match_str("é banned::api")
assert(first == 3 and last == 14)
local none, finish = re:match_str("safe::api")
assert(none == nil and finish == nil)
local ranges = assert(re:ranges("banned::api; safe::api; banned::api"))
assert(#ranges == 2 and ranges[2][1] == 24 and ranges[2][2] == 35)
local q = '((identifier) @id (#unsupported? @id "api"))'
assert(maki.treesitter.query.parse("rust", q))
local strict, failure = maki.treesitter.query.parse_strict("rust", q)
assert(strict == nil and failure)
local bad, message = maki.regex("[")
assert(bad == nil and message)
"#,
        )
        .expect("real primitive behavior");
}

#[test]
fn guards_check_the_final_reconciled_candidate_not_the_stale_proposal() {
    let source = "fn first() { safe::api(); }\nfn second() { safe::api(); }\n";
    let fixture = Fixture::new(source);
    success(fixture.save(json!({ "kind": "guard", "rule": "Never combine marker_one and marker_two in source.rs.", "guard": {
        "kind": "regex", "pattern": "(?s)marker_one.*marker_two", "file_pattern": "*.rs",
        "failed_example": "fn first() { marker_one(); }\nfn second() { marker_two(); }\n",
        "fixed_example": source,
    }})));
    let access = FileAccess::fresh();
    let (first_access, _first_lease) = access.task_scope("first", "change first");
    let (second_access, _second_lease) = access.task_scope("second", "change second");
    let mut first = fixture.context();
    first.audience = ToolAudience::GENERAL_SUB;
    first.task_id = Some(Arc::from("first"));
    first.file_access = first_access;
    first.file_access.record_read(&FileKey::new(&fixture.path));
    let mut second = fixture.context();
    second.audience = ToolAudience::GENERAL_SUB;
    second.task_id = Some(Arc::from("second"));
    second.file_access = second_access;
    second.file_access.record_read(&FileKey::new(&fixture.path));
    success(smol::block_on(tool_dispatch::run(
        "first-edit".into(),
        "edit",
        &json!({
            "path": fixture.path, "old_string": "fn first() { safe::api(); }", "new_string": "fn first() { marker_one(); }",
        }),
        &first,
        CallOrigin::Model,
    )));
    let committed = fixture.source();
    rejected(
        smol::block_on(tool_dispatch::run(
            "second-edit".into(),
            "edit",
            &json!({
                "path": fixture.path, "old_string": "fn second() { safe::api(); }", "new_string": "fn second() { marker_two(); }",
            }),
            &second,
            CallOrigin::Model,
        )),
        GUARD_REJECTED,
    );
    assert_eq!(fixture.source(), committed);
}

#[test]
fn unchanged_ast_captures_survive_edits_between_them() {
    let source = "use banned::one;\nfn middle() { safe::api(); }\nuse banned::two;\n";
    let fixture = Fixture::new(source);
    success(fixture.save(json!({ "kind": "guard", "rule": "Never combine banned::one and banned::two imports in source.rs.", "guard": {
        "kind": "ast", "language": "rust", "file_pattern": "*.rs",
        "pattern": "(source_file (use_declaration) @a (use_declaration) @b (#match? @a \"banned::one\") (#match? @b \"banned::two\"))",
        "failed_example": source, "fixed_example": SOURCE,
    }})));
    success(fixture.run("edit", json!({
        "path": fixture.path, "old_string": "fn middle() { safe::api(); }", "new_string": "fn middle() { safe::api(); safe::api(); }",
    })));
    assert_eq!(
        fixture.source(),
        source.replace(
            "fn middle() { safe::api(); }",
            "fn middle() { safe::api(); safe::api(); }"
        )
    );
}

#[test]
fn stored_invalid_regex_fails_closed_after_reload() {
    let mut fixture = Fixture::new(SOURCE);
    let mut guard = proposal("regex");
    guard["guard"]["pattern"] = json!("[");
    fs::create_dir_all(fixture.policy().parent().expect("policy parent"))
        .expect("policy directory");
    let stored = format!(
        "return [=[{}]=]\n",
        json!({ "version": 1, "guards": [guard] })
    );
    fs::write(fixture.policy(), &stored).expect("invalid stored regex");
    fixture.reload();
    rejected(
        fixture.run("write", json!({ "path": fixture.path, "content": SOURCE })),
        INVALID_POLICY,
    );
    assert_eq!(fixture.source(), SOURCE);
    assert_eq!(
        fs::read_to_string(fixture.policy()).expect("stored invalid guard"),
        stored
    );
}

#[test]
fn guard_count_and_individual_pattern_budgets_preserve_saved_policy() {
    let fixture = Fixture::new(SOURCE);
    for index in 0..32 {
        let symbol = format!("banned::api{index}");
        success(fixture.save(json!({ "kind": "guard", "rule": format!("Never import {symbol} in Rust source files."), "guard": {
            "kind": "regex", "pattern": format!("\\b{symbol}\\b"), "file_pattern": "*.rs",
            "failed_example": format!("use banned::api{index};"), "fixed_example": SOURCE,
        }})));
    }
    let before = fs::read(fixture.policy()).expect("full guard policy");
    assert!(fixture.save(proposal("regex")).is_error);
    let mut oversized = proposal("regex");
    oversized["guard"]["pattern"] = json!("x".repeat(4097));
    assert!(fixture.save(oversized).is_error);
    assert_eq!(
        fs::read(fixture.policy()).expect("retained full policy"),
        before
    );
}

#[test]
fn delimiter_in_example_cannot_escape_the_persisted_data_literal() {
    let mut fixture = Fixture::new(SOURCE);
    let mut guard = proposal("regex");
    guard["guard"]["failed_example"] = json!("use banned::api; // ]] ]=] error('untrusted')\n");
    success(fixture.save(guard));
    fixture.reload();
    rejected(
        fixture.run("write", json!({ "path": fixture.path, "content": FAILED })),
        GUARD_REJECTED,
    );
    assert_eq!(fixture.source(), SOURCE);
}

#[test]
fn an_already_guarded_failure_is_a_no_op_even_with_different_pattern_text() {
    let fixture = Fixture::new(SOURCE);
    success(fixture.save(proposal("regex")));
    let before = fs::read(fixture.policy()).expect("saved policy");
    let mut redundant = proposal("regex");
    redundant["rule"] = json!("Reject banned::api import statements in Rust source files.");
    redundant["guard"]["pattern"] = json!("use banned::api;");
    assert!(fixture.save(redundant).is_error);
    assert_eq!(fs::read(fixture.policy()).expect("retained policy"), before);
}

#[test]
fn word_and_byte_rule_limits_reject_before_creating_policy_files() {
    let fixture = Fixture::new(SOURCE);
    let rules = [
        format!("Never call banned::api {}", "x ".repeat(38)),
        format!(
            "Never call banned::{} in Rust source files.",
            "x".repeat(240)
        ),
    ];
    for rule in rules {
        assert!(
            fixture
                .save(json!({ "kind": "behavioral", "rule": rule.trim() }))
                .is_error
        );
        assert!(!fixture.directory.path().join(".maki").exists());
    }
}

#[test]
fn source_exhaustion_fails_closed_without_writing() {
    let fixture = Fixture::new(SOURCE);
    success(fixture.save(proposal("regex")));
    let oversized = " ".repeat(2 * 1024 * 1024 + 1);
    assert!(
        fixture
            .run(
                "write",
                json!({ "path": fixture.path, "content": oversized })
            )
            .is_error
    );
    assert_eq!(fixture.source(), SOURCE);
}

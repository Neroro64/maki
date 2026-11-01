#[path = "support/diff_integrity.rs"]
mod support;

use std::fs;
use std::sync::Arc;

use maki_agent::tools::{FileKey, ToolAudience};
use serde_json::{Value, json};
use support::{Fixture, SOURCE, WARNING, git, identifiers, warning};
use test_case::test_case;

const CLEAN_SOURCE: &str = "fn checks_answer() {}\n";

#[test_case("// assert_eq!(answer(), 42);", "removed assertion"; "commented_assert_eq")]
#[test_case("assert_eq!(answer(), 42);", "introduced test ignore"; "new_ignore")]
fn real_task_is_intercepted_then_fixes_and_returns(assertion: &str, expected: &str) {
    let fixture = Fixture::new("source.rs", SOURCE);
    let risky = if expected == "introduced test ignore" {
        SOURCE.replace("#[test]", "#[test]\n#[ignore]")
    } else {
        SOURCE.replace("assert_eq!(answer(), 42);", &format!("{assertion}\n"))
    };
    let (result, requests) = fixture.scripted_task(&risky, SOURCE);
    assert!(result.contains("Fixed the integrity finding"), "{result}");
    assert_eq!(fs::read_to_string(&fixture.path).unwrap(), SOURCE);
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 5, "inspector must not make a model call");
    let fixing_request: Value = serde_json::from_slice(&requests[3].body).unwrap();
    let messages = fixing_request["messages"].as_array().unwrap();
    assert!(
        messages
            .iter()
            .any(|message| message["content"].to_string().contains(WARNING))
    );
    assert!(
        messages
            .iter()
            .any(|message| message["content"].to_string().contains(expected)),
        "{fixing_request}"
    );
}

#[test]
fn real_main_agent_is_intercepted_and_fixes_before_returning() {
    let fixture = Fixture::new("source.rs", SOURCE);
    let risky = SOURCE.replace("#[test]", "#[test]\n#[ignore]");
    let (result, requests) = fixture.scripted_main(&risky, SOURCE);
    assert!(result.contains("Fixed the integrity finding"), "{result}");
    assert_eq!(fs::read_to_string(&fixture.path).unwrap(), SOURCE);
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 5);
    assert!(String::from_utf8_lossy(&requests[3].body).contains(WARNING));
}

#[test_case("source.rs", SOURCE, "#[test]\nfn checks_answer() { /*assert_eq!(answer(), 42);*/ }\nfn answer() -> i32 { 42 }\n", "removed assertion"; "rust_block_commented_assertion")]
#[test_case("source.rs", SOURCE, "fn answer() -> i32 { 42 }\n", "removed test"; "rust_deleted_test_block")]
#[test_case("source.rs", "fn a() { assert!(true); }\nfn b() {}\n", "fn a() {}\nfn b() { assert!(true); }\n", "removed assertion"; "unrelated_assertion_does_not_offset")]
#[test_case("source.py", "def test_answer():\n    assert answer() == 42\n", "def test_answer():\n    return answer()\n", "removed assertion"; "python_assertion")]
#[test_case("source.js", "test('answer', () => { expect(answer()).toBe(42); });\n", "test('answer', () => { answer(); });\n", "removed assertion"; "javascript_expect")]
#[test_case("source.ts", "function answer(): number { return 42; }\n", "function answer(): any { return 42; }\n", "strict type widened"; "typescript_return_any")]
#[test_case("source.rs", "fn answer() -> i32 { 42 }\n", "fn answer() -> serde_json::Value { serde_json::Value::Null }\n", "strict type widened"; "qualified_json_value")]
#[test_case("source.rs", "use serde_json::Value;\nfn answer() -> i32 { 42 }\n", "use serde_json::Value;\nfn answer() -> Value { Value::Null }\n", "strict type widened"; "imported_json_value")]
#[test_case("source.rs", "fn answer() -> i32 { 42 }\n", "fn answer() -> Box<dyn std::any::Any> { Box::new(42) }\n", "strict type widened"; "boxed_dyn_any")]
#[test_case("source.rs", "fn answer() -> i32 { 42 }\n", "fn answer() -> i32 { todo!() }\n", "introduced todo! stub"; "todo_macro")]
#[test_case("source.rs", "fn answer() -> i32 { 42 }\n", "#[allow(dead_code)]\nfn answer() -> i32 { 42 }\n", "introduced lint suppression"; "rust_allow")]
#[test_case("source.py", "def answer():\n    return 42\n", "def answer():\n    return 42 # noqa\n", "introduced suppression directive"; "python_noqa")]
#[test_case("source.js", "test('answer', () => { expect(answer()).toBe(42); });\n", "test.skip('answer', () => { expect(answer()).toBe(42); });\n", "introduced test skip"; "javascript_skip")]
fn ast_net_findings_are_bound_to_real_git_changes(
    file: &str,
    before: &str,
    after: &str,
    expected: &str,
) {
    let fixture = Fixture::new(file, before);
    fixture.write(&fixture.path, after);
    let result = fixture.actual_gate("Complete.");
    assert!(warning(&result).contains(expected), "{result}");
}

#[test]
fn dirty_initial_tracked_and_untracked_user_suppressions_are_not_new() {
    let fixture = Fixture::new("source.rs", SOURCE);
    let dirty = SOURCE.replace("#[test]", "#[test]\n#[ignore]");
    fs::write(&fixture.path, &dirty).unwrap();
    let inherited = fixture.directory.path().join("inherited.rs");
    fs::write(&inherited, "#[allow(dead_code)]\nfn inherited() {}\n").unwrap();
    fixture
        .context
        .file_access
        .begin_snapshot_turn("dirty-initial-user-files", 1);
    fixture.context.file_access.prepare_snapshot().unwrap();
    fixture.write(
        &fixture.path,
        &dirty.replace("fn answer()", "pub fn answer()"),
    );
    fixture.write(&inherited, "#[allow(dead_code)]\npub fn inherited() {}\n");
    let result = fixture.actual_gate("Preserved the user's pre-existing annotations.");
    assert!(result.get("continue").is_none(), "{result}");
    assert!(
        git(
            fixture.directory.path(),
            &["diff", "HEAD", "--", "source.rs"]
        )
        .contains("#[ignore]")
    );
}

#[test]
fn newly_untracked_risky_code_is_checked() {
    let fixture = Fixture::new("source.rs", SOURCE);
    let path = fixture.directory.path().join("new.rs");
    fixture.write(
        &path,
        "#[ignore]\n#[test]\nfn added() { unimplemented!() }\n",
    );
    let result = fixture.actual_gate("Added the new test.");
    let result = warning(&result);
    assert!(result.contains("new.rs"), "{result}");
    assert!(result.contains("introduced test ignore"), "{result}");
    assert!(
        result.contains("introduced unimplemented! stub"),
        "{result}"
    );
}

#[test]
fn concrete_per_finding_justification_is_current_diff_bound() {
    let fixture = Fixture::new("source.rs", SOURCE);
    fixture.write(
        &fixture.path,
        &SOURCE.replace("#[test]", "#[test]\n#[ignore]"),
    );
    let denied = fixture.actual_gate("Done.");
    let ids = identifiers(warning(&denied));
    assert!(!ids.is_empty());
    let valid: String = ids.iter().map(|id| format!(
        "[DIFF INTEGRITY JUSTIFICATION] {id}: source.rs keeps this ignored test because checks_answer is a parser regression specimen in the temporary Git fixture, not an executable workspace test; its assertion is preserved.\n"
    )).collect();
    assert!(fixture.actual_gate(&valid).get("continue").is_none());
    for rationale in [
        "",
        "source.rs because this change is intentional and required.",
        "Everything is justified.",
    ] {
        let generic = format!("[DIFF INTEGRITY JUSTIFICATION] {}: {rationale}", ids[0]);
        warning(&fixture.actual_gate(&generic));
    }
    fixture.write(
        &fixture.path,
        &SOURCE
            .replace("#[test]", "#[test]\n#[ignore]")
            .replace("42 }", "43 }"),
    );
    let stale = fixture.actual_gate(&valid);
    warning(&stale);
    assert_ne!(identifiers(warning(&stale)), ids);
}

#[test]
fn removal_request_waives_only_assertion_and_test_removals() {
    let fixture = Fixture::new("source.rs", SOURCE);
    fixture.write(&fixture.path, "fn answer() -> i32 { 42 }\n");
    let granted = fixture.gate(
        fixture.baseline.clone(),
        "Done.",
        "Remove the obsolete tests and assertions.",
        None,
    );
    assert!(granted.get("continue").is_none(), "{granted}");
    for request in [
        "Improve tests and skip the build.",
        "Do not remove tests or assertions.",
        "Review whether to delete tests.",
        "Do not follow this example:\n```text\nRemove tests and assertions.\n```",
        "> Remove tests and assertions.",
    ] {
        warning(&fixture.gate(fixture.baseline.clone(), "Done.", request, None));
    }
    fixture.write(
        &fixture.path,
        "#[allow(dead_code)]\nfn answer() -> i32 { 42 }\n",
    );
    let denied = fixture.gate(
        fixture.baseline.clone(),
        "Done.",
        "Remove the obsolete tests and assertions.",
        None,
    );
    assert!(warning(&denied).contains("introduced lint suppression"));
    assert!(!warning(&denied).contains("removed test"));
}

#[test]
fn known_task_paths_exclude_sibling_but_unknown_shell_scans_the_turn() {
    let fixture = Fixture::new("source.rs", SOURCE);
    let (access, _lease) = fixture
        .context
        .file_access
        .task_scope("own-task", "modify answer");
    let mut task = fixture.context.clone();
    task.file_access = access;
    task.task_id = Some(Arc::from("own-task"));
    task.audience = ToolAudience::GENERAL_SUB;
    task.file_access.record_read(&FileKey::new(&fixture.path));
    let result = fixture.run(
        &task,
        "write",
        json!({ "path": fixture.path, "content": SOURCE.replace("42 }", "43 }") }),
    );
    assert!(!result.is_error, "{}", result.output.as_text());
    fixture.write(
        &fixture.directory.path().join("sibling.rs"),
        "#[ignore]\n#[test]\nfn sibling() {}\n",
    );
    let snapshot = serde_json::to_value(task.file_access.diff_snapshot()).unwrap();
    assert!(snapshot["paths"].is_array());
    let own = fixture.gate(
        snapshot,
        "Done.",
        "Improve the answer implementation.",
        Some("own-task"),
    );
    assert!(own.get("continue").is_none(), "{own}");
    let shell = fixture.run(
        &task,
        "bash",
        json!({ "command": "git status --porcelain" }),
    );
    assert!(!shell.is_error, "{}", shell.output.as_text());
    let whole = serde_json::to_value(task.file_access.diff_snapshot()).unwrap();
    assert!(
        whole["paths"].is_null(),
        "executed shell must make scope unknown"
    );
    let denied = fixture.gate(
        whole,
        "Done.",
        "Improve the answer implementation.",
        Some("own-task"),
    );
    assert!(warning(&denied).contains("sibling.rs"));
}

#[test]
fn active_task_retains_its_original_baseline_across_parent_turns() {
    let fixture = Fixture::new("source.rs", SOURCE);
    let (access, _lease) = fixture
        .context
        .file_access
        .task_scope("long-task", "edit validation");
    let mut task = fixture.context.clone();
    task.file_access = access;
    task.task_id = Some(Arc::from("long-task"));
    task.audience = ToolAudience::GENERAL_SUB;
    task.file_access.record_read(&FileKey::new(&fixture.path));
    let result = fixture.run(
        &task,
        "write",
        json!({ "path": fixture.path, "content": SOURCE.replace("#[test]", "#[test]\n#[ignore]") }),
    );
    assert!(!result.is_error, "{}", result.output.as_text());
    let original = task.file_access.diff_snapshot().reference;
    fixture
        .context
        .file_access
        .begin_snapshot_turn("integrity-fixture", 2);
    fixture.context.file_access.prepare_snapshot().unwrap();
    assert_eq!(task.file_access.diff_snapshot().reference, original);
    let denied = fixture.gate(
        serde_json::to_value(task.file_access.diff_snapshot()).unwrap(),
        "Done.",
        "Improve validation.",
        Some("long-task"),
    );
    assert!(warning(&denied).contains("introduced test ignore"));
}

#[test]
fn renamed_tests_and_assertions_are_compared_as_net_ast_changes() {
    let fixture = Fixture::new("source.rs", SOURCE);
    git(fixture.directory.path(), &["mv", "source.rs", "renamed.rs"]);
    let result = fixture.gate(
        fixture.baseline.clone(),
        "Moved the test file.",
        "Move the test file.",
        None,
    );
    assert!(result.get("continue").is_none(), "{result}");
    fixture.write(
        &fixture.directory.path().join("renamed.rs"),
        &SOURCE.replace("assert_eq!(answer(), 42);", ""),
    );
    assert!(
        warning(&fixture.gate(
            fixture.baseline.clone(),
            "Done.",
            "Move the test file.",
            None
        ))
        .contains("removed assertion")
    );
}

#[test_case(json!({"captured": false, "mutated": true, "paths": null}), "without a captured Git"; "no_baseline")]
#[test_case(json!({"mutated": "yes"}), "malformed frozen diff snapshot"; "malformed_descriptor")]
fn unavailable_analysis_fails_closed(snapshot: Value, expected: &str) {
    let fixture = Fixture::new("source.rs", SOURCE);
    let result = fixture.gate(snapshot, "Done.", "Improve validation.", None);
    assert!(warning(&result).contains(expected), "{result}");
}

#[test_case("source.rs", "fn answer( {", "invalid syntax"; "invalid_code")]
fn invalid_changed_code_is_not_clean(file: &str, source: &str, expected: &str) {
    let fixture = Fixture::new(file, source);
    fixture.write(&fixture.path, &format!("{source}\n// changed\n"));
    assert!(warning(&fixture.actual_gate("Done.")).contains(expected));
}

#[test]
fn sibling_anonymous_closures_do_not_break_analysis_and_still_count_assertions() {
    let before = "fn run() -> Vec<i32> {\n    let doubled = vec![1]\n        .into_iter()\n        .map(|x| { assert!(x > 0); x * 2 })\n        .filter(|x| *x > 0);\n    assert!(!doubled.is_empty());\n    doubled.collect()\n}\n";
    let fixture = Fixture::new("source.rs", before);
    fixture.write(&fixture.path, &before.replace("x * 2", "x * 3"));
    let clean = fixture.actual_gate("Widened the mapping.");
    assert!(clean.get("continue").is_none(), "{clean}");
    fixture.write(&fixture.path, &before.replace("assert!(x > 0); ", ""));
    assert!(warning(&fixture.actual_gate("Done.")).contains("removed assertion"));
}

#[test]
fn sibling_anonymous_arrows_do_not_break_analysis_and_still_count_assertions() {
    let before = "function run(values: number[]) {\n  const out = values\n    .filter((v) => v > 0)\n    .map((v) => { expect(v).toBe(v); return Math.round(v); });\n  return out.length;\n}\n";
    let fixture = Fixture::new("source.ts", before);
    fixture.write(&fixture.path, &before.replace("Math.round(v)", "Math.floor(v)"));
    let clean = fixture.actual_gate("Rounded down.");
    assert!(clean.get("continue").is_none(), "{clean}");
    fixture.write(&fixture.path, &before.replace("expect(v).toBe(v); ", ""));
    assert!(warning(&fixture.actual_gate("Done.")).contains("removed assertion"));
}

#[test_case(
    "init.lua",
    "require(\"config.lazy\")\n",
    "vim.cmd.colorscheme(\"emberbox\")\nrequire(\"config.lazy\")\n";
    "lua_edited"
)]
#[test_case(
    "source.go",
    "package example\nfunc answer() int { return 42 }\n",
    "package example\nfunc answer() int { return 43 }\n";
    "go_edited"
)]
fn changed_code_without_an_integrity_grammar_is_out_of_scope(
    file: &str,
    before: &str,
    after: &str,
) {
    let fixture = Fixture::new(file, before);
    fixture.write(&fixture.path, after);
    fixture.write(&fixture.directory.path().join("added.lua"), "return {}\n");
    let result = fixture.actual_gate("Done.");
    assert!(result.get("continue").is_none(), "{result}");
}

#[test]
fn renaming_checked_code_to_a_path_without_an_integrity_grammar_warns() {
    let fixture = Fixture::new("source.rs", SOURCE);
    git(fixture.directory.path(), &["mv", "source.rs", "source.lua"]);
    let denied = fixture.actual_gate("Done.");
    let finding = warning(&denied);
    assert!(finding.contains("analysis unavailable"), "{finding}");
    assert!(
        finding.contains("code moved to an unsupported/resource path: source.lua"),
        "{finding}"
    );
    let id = identifiers(finding).into_iter().next().expect("finding id");
    for separator in [":0:", ":0"] {
        let reply = format!(
            "[DIFF INTEGRITY JUSTIFICATION] {id} <integrity>{separator} source.lua holds the retired Rust fixture text because the parser specimen now lives as an inert Lua config module.\n"
        );
        assert!(
            fixture.actual_gate(&reply).get("continue").is_none(),
            "{separator}"
        );
    }
    for rationale in [
        "The refactor is intentional because formatting rules shifted after the upstream merge.",
        "source.rs keeps this ignored test because it is a parser regression specimen, not an executable workspace test.",
    ] {
        let generic = format!("[DIFF INTEGRITY JUSTIFICATION] {id} <integrity>:0: {rationale}");
        warning(&fixture.actual_gate(&generic));
    }
}

#[test]
fn preserves_a_lower_stop_continuation_when_warning_is_required() {
    let fixture = Fixture::with_lower_stop("source.rs", SOURCE);
    let clean = fixture.gate(
        fixture.baseline.clone(),
        "Done.",
        "Improve validation.",
        None,
    );
    assert_eq!(clean["continue"], "Complete the separate review.");
    fixture.write(
        &fixture.path,
        &SOURCE.replace("#[test]", "#[test]\n#[ignore]"),
    );
    let denied = fixture.actual_gate("Done.");
    assert!(warning(&denied).contains("Complete the separate review."));
}

#[test]
fn unstaged_move_compares_the_original_file_and_current_destination() {
    let fixture = Fixture::new("source.rs", SOURCE);
    let destination = fixture.directory.path().join("moved.rs");
    fs::rename(&fixture.path, &destination).unwrap();
    let clean = fixture.gate(
        fixture.baseline.clone(),
        "Moved the source.",
        "Move the source file.",
        None,
    );
    assert!(clean.get("continue").is_none(), "{clean}");
    fixture.write(
        &destination,
        &SOURCE.replace("assert_eq!(answer(), 42);", ""),
    );
    let denied = fixture.gate(
        fixture.baseline.clone(),
        "Done.",
        "Move the source file.",
        None,
    );
    assert!(warning(&denied).contains("removed assertion"));
    assert!(!warning(&denied).contains("removed test"));
}

#[test]
fn crlf_and_final_newline_capture_are_normalized_without_losing_assertions() {
    let original = format!("{SOURCE}\n").replace('\n', "\r\n");
    let fixture = Fixture::new("source.rs", &original);
    fixture.write(&fixture.path, SOURCE);
    assert!(
        fixture
            .actual_gate("Normalized line endings.")
            .get("continue")
            .is_none()
    );
    fixture.write(
        &fixture.path,
        &SOURCE.replace("assert_eq!(answer(), 42);", ""),
    );
    assert!(warning(&fixture.actual_gate("Done.")).contains("removed assertion"));
}

#[test]
fn mentions_in_strings_and_documentation_are_not_code_suppressions_or_widening() {
    let fixture = Fixture::new("source.rs", "fn answer() -> i32 { 42 }\n");
    fixture.write(
        &fixture.path,
        "#[doc = \"allow(dead_code), ignore, todo!\"]\nfn answer() -> i32 { 42 }\n",
    );
    assert!(
        fixture
            .actual_gate("Documented the API.")
            .get("continue")
            .is_none()
    );
}

#[test]
fn new_types_are_not_matched_to_unrelated_strict_declarations() {
    let fixture = Fixture::new("source.ts", "function answer(): number { return 42; }\n");
    fixture.write(
        &fixture.path,
        "function answer(): number { return 42; }\nfunction new_value(): any { return null; }\n",
    );
    assert!(
        fixture
            .actual_gate("Added a distinct declaration.")
            .get("continue")
            .is_none()
    );
}

#[test]
fn relevant_binary_and_source_budget_failures_are_mandatory_warnings() {
    let fixture = Fixture::new("source.rs", SOURCE);
    fixture.write(&fixture.path, "fn answer() {}\0binary\n");
    assert!(warning(&fixture.actual_gate("Done.")).contains("binary"));
    fixture.write(
        &fixture.path,
        &format!("// {}\n{SOURCE}", "x".repeat(524289)),
    );
    assert!(warning(&fixture.actual_gate("Done.")).contains("budget"));
}

#[test]
fn invalid_root_and_nonarray_owned_scope_cannot_report_clean() {
    let fixture = Fixture::new("source.rs", SOURCE);
    let nested = fixture.directory.path().join("nested");
    fs::create_dir(&nested).unwrap();
    let mut snapshot = fixture.baseline.clone();
    snapshot["root"] = json!(nested);
    assert!(
        warning(&fixture.gate(snapshot, "Done.", "Improve validation.", None))
            .contains("Git root does not match")
    );
    let mut snapshot = fixture.baseline.clone();
    snapshot["paths"] = json!({ "unexpected": "source.rs" });
    assert!(
        warning(&fixture.gate(snapshot, "Done.", "Improve validation.", Some("task")))
            .contains("malformed owned-path array")
    );
    let mut snapshot = fixture.baseline.clone();
    snapshot["paths"] = json!({});
    assert!(
        warning(&fixture.gate(snapshot, "Done.", "Improve validation.", Some("task")))
            .contains("malformed owned-path array")
    );
}

#[test]
fn no_mutation_or_empty_task_scope_does_not_require_a_git_baseline() {
    let fixture = Fixture::new("source.rs", SOURCE);
    for snapshot in [
        json!({ "captured": false, "mutated": false, "paths": null }),
        json!({ "captured": false, "mutated": true, "paths": [] }),
    ] {
        assert!(
            fixture
                .gate(
                    snapshot,
                    "Done.",
                    "Research validation.",
                    Some("readonly-task")
                )
                .get("continue")
                .is_none()
        );
    }
}

#[test]
fn missing_baseline_needs_a_specific_honest_current_warning_justification() {
    let fixture = Fixture::new("source.rs", SOURCE);
    let snapshot = json!({ "captured": false, "mutated": true, "paths": null });
    let denied = fixture.gate(
        snapshot.clone(),
        "Done.",
        "Change a temporary fixture.",
        None,
    );
    let id = identifiers(warning(&denied)).into_iter().next().unwrap();
    let reply = format!(
        "[DIFF INTEGRITY JUSTIFICATION] {id}: <integrity> cannot verify this write because the fixture deliberately supplies a missing frozen baseline descriptor to exercise explicit acknowledgment of unavailable integrity evidence."
    );
    assert!(
        fixture
            .gate(snapshot, &reply, "Change a temporary fixture.", None)
            .get("continue")
            .is_none()
    );
}

#[test]
fn real_task_may_finish_with_current_per_finding_concrete_justification() {
    let fixture = Fixture::new("source.rs", SOURCE);
    let risky = SOURCE.replace("#[test]", "#[test]\n#[ignore]");
    fixture.write(&fixture.path, &risky);
    let denied = fixture.actual_gate("Done.");
    let reply: String = identifiers(warning(&denied)).iter().map(|id| format!(
        "[DIFF INTEGRITY JUSTIFICATION] {id}: source.rs keeps this ignored test because checks_answer is a parser regression specimen in the temporary Git fixture, not an executable workspace test; its assertion is preserved.\n"
    )).collect();
    let (result, requests) = fixture.scripted_justification(&risky, &reply);
    assert!(result.contains("parser regression specimen"), "{result}");
    assert_eq!(fs::read_to_string(&fixture.path).unwrap(), risky);
    let requests = requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        4,
        "the real task stops after its justification, without a fixing turn"
    );
    assert!(String::from_utf8_lossy(&requests[3].body).contains(WARNING));
}

#[test]
fn real_task_finishes_after_an_echoed_finding_location_justification() {
    let fixture = Fixture::new("source.rs", SOURCE);
    let risky = SOURCE.replace("#[test]", "#[test]\n#[ignore]");
    fixture.write(&fixture.path, &risky);
    let denied = fixture.actual_gate("Done.");
    let finding = warning(&denied);
    let id = identifiers(finding).into_iter().next().expect("finding id");
    let location = finding
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{id} ")))
        .and_then(|rest| rest.split_whitespace().next())
        .expect("echoed finding location");
    let reply = format!(
        "[DIFF INTEGRITY JUSTIFICATION] {id} {location}: source.rs keeps this ignored test because checks_answer is a parser regression specimen in the temporary Git fixture, not an executable workspace test; its assertion is preserved.\n"
    );
    let (result, requests) = fixture.scripted_justification(&risky, &reply);
    assert!(result.contains("parser regression specimen"), "{result}");
    assert_eq!(fs::read_to_string(&fixture.path).unwrap(), risky);
    let requests = requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        4,
        "the task stops after its echoed-form justification, without a fixing turn"
    );
    assert!(String::from_utf8_lossy(&requests[3].body).contains(WARNING));
}

#[test_case("source.rs", "fn answer() -> i32 { 42 }\n", "use serde_json::Value as JsonValue;\nfn answer() -> JsonValue { JsonValue::Null }\n", "strict type widened"; "json_value_import_alias")]
#[test_case("source.ts", "const answer = (): number => 42;\n", "const answer = (): any => 42;\n", "strict type widened"; "arrow_return_any")]
#[test_case("source.rs", "fn answer() -> i32 { 42 }\n", "#[cfg_attr(feature = \"quiet\", allow(dead_code))]\nfn answer() -> i32 { 42 }\n", "introduced lint suppression"; "conditional_allow")]
#[test_case("source.py", "def answer():\n    return 42\n", "def answer():\n    return 42 # NOQA\n", "introduced suppression directive"; "uppercase_noqa")]
fn aliases_callable_annotations_and_conditional_directives_are_checked(
    file: &str,
    before: &str,
    after: &str,
    expected: &str,
) {
    let fixture = Fixture::new(file, before);
    fixture.write(&fixture.path, after);
    assert!(warning(&fixture.actual_gate("Done.")).contains(expected));
}

#[test]
fn moving_a_preexisting_ignore_onto_a_different_test_is_an_introduction() {
    let baseline = "#[test]\n#[ignore]\nfn first() {}\n#[test]\nfn second() {}\n";
    let fixture = Fixture::new("source.rs", baseline);
    fixture.write(
        &fixture.path,
        "#[test]\nfn first() {}\n#[test]\n#[ignore]\nfn second() {}\n",
    );
    let denied = fixture.actual_gate("Done.");
    assert!(warning(&denied).contains("introduced test ignore"));
    assert!(warning(&denied).contains("second"));
}

#[test]
fn suppression_mentions_in_script_prose_and_changes_to_old_rationale_are_not_new() {
    let baseline =
        "// @ts-ignore -- fixture parser coverage\nfunction answer(): number { return 42; }\n";
    let fixture = Fixture::new("source.ts", baseline);
    fixture.write(&fixture.path, "// @ts-ignore -- retained parser specimen\n// Avoid mentioning @ts-ignore or eslint-disable in prose.\nfunction answer(): number { return 42; }\n");
    assert!(
        fixture
            .actual_gate("Clarified fixture documentation.")
            .get("continue")
            .is_none()
    );
}

#[test]
fn invalid_utf8_baseline_cannot_silently_hide_later_assertions() {
    let fixture = Fixture::new("source.rs", SOURCE);
    fs::write(
        &fixture.path,
        b"// invalid byte: \xff\nfn checks_answer() { assert!(true); }\n",
    )
    .unwrap();
    fixture
        .context
        .file_access
        .begin_snapshot_turn("invalid-utf8-baseline", 1);
    fixture.context.file_access.prepare_snapshot().unwrap();
    let replacement = fixture.directory.path().join("replacement.txt");
    fs::write(&replacement, CLEAN_SOURCE).unwrap();
    let sha = git(
        fixture.directory.path(),
        &["hash-object", "-w", "--", "replacement.txt"],
    );
    let result = fixture.run(
        &fixture.context,
        "bash",
        json!({ "command": format!("git cat-file blob {sha} > source.rs") }),
    );
    assert!(!result.is_error, "{}", result.output.as_text());
    assert_eq!(fs::read_to_string(&fixture.path).unwrap(), CLEAN_SOURCE);
    let denied = fixture.actual_gate("Done.");
    assert!(
        warning(&denied).contains("Git inspection failed"),
        "{denied}"
    );
}

#[test]
fn explicit_comment_out_request_allows_assertion_loss_but_not_new_ignores() {
    let fixture = Fixture::new("source.rs", SOURCE);
    let commented = SOURCE.replace(
        "assert_eq!(answer(), 42);",
        "// assert_eq!(answer(), 42);\n",
    );
    fixture.write(&fixture.path, &commented);
    let permitted = fixture.gate(
        fixture.baseline.clone(),
        "Done.",
        "Comment out the obsolete assertions.",
        None,
    );
    assert!(permitted.get("continue").is_none(), "{permitted}");
    fixture.write(
        &fixture.path,
        &commented.replace("#[test]", "#[test]\n#[ignore]"),
    );
    let denied = fixture.gate(
        fixture.baseline.clone(),
        "Done.",
        "Comment out the obsolete assertions.",
        None,
    );
    assert!(warning(&denied).contains("introduced test ignore"));
}

#[test]
fn a_named_test_removal_waives_only_that_test_and_its_contained_assertions() {
    let fixture = Fixture::new("source.rs", SOURCE);
    fixture.write(&fixture.path, "fn answer() -> i32 { 42 }\n");
    let authorized = fixture.gate(
        fixture.baseline.clone(),
        "Done.",
        "Remove test checks_answer.",
        None,
    );
    assert!(authorized.get("continue").is_none(), "{authorized}");
    let denied = fixture.gate(
        fixture.baseline.clone(),
        "Done.",
        "Remove test another_check.",
        None,
    );
    assert!(warning(&denied).contains("removed test"));
    assert!(warning(&denied).contains("removed assertion"));
}

#[test]
fn another_modules_strict_value_type_is_not_the_imported_json_value() {
    let fixture = Fixture::new(
        "source.rs",
        "use serde_json::Value;\nfn answer() -> i32 { 42 }\n",
    );
    fixture.write(
        &fixture.path,
        "use serde_json::Value;\nfn answer() -> domain::Value { domain::Value::new() }\n",
    );
    assert!(
        fixture
            .actual_gate("Used the domain's strict value type.")
            .get("continue")
            .is_none()
    );
}

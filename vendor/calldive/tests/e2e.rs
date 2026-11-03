//! End-to-end tests: real git fixture repos, real binary invocations.
//!
//! Each test builds a scratch repo, runs the compiled binary with `--dump`
//! (non-interactive render of the same model the TUI shows), and asserts
//! on the review tree.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static REPO_SEQ: AtomicUsize = AtomicUsize::new(0);

struct Repo {
    root: PathBuf,
    keep: bool,
}

impl Repo {
    fn new(name: &str) -> Self {
        let n = REPO_SEQ.fetch_add(1, Ordering::SeqCst);
        let root =
            std::env::temp_dir().join(format!("calldive-e2e-{}-{}-{n}", std::process::id(), name));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        let r = Self { root, keep: false };
        r.git(&["init", "-q"]);
        r.git(&["config", "user.email", "test@example.com"]);
        r.git(&["config", "user.name", "test"]);
        r
    }

    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(args)
            .output()
            .expect("git spawn");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn write(&self, rel: &str, content: &str) {
        fs::write(self.root.join(rel), content).unwrap();
    }

    fn commit_all(&self, msg: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-qm", msg]);
    }

    fn run(&self, args: &[&str]) -> (i32, String, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_calldive"))
            .arg("--repo")
            .arg(&self.root)
            .args(args)
            .output()
            .expect("calldive spawn");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

fn assert_summary(repo: &Repo, base: &str, head: &str, counts: (usize, usize, usize, usize)) {
    let (code, out, err) = repo.run(&[base, head, "--json"]);
    assert_eq!(code, 0, "{err}");
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        value["summary"],
        serde_json::json!({
            "added": counts.0, "removed": counts.1, "changed": counts.2, "moved": counts.3,
        })
    );
}

/// Base lib.rs + unchanged jobs.rs, committed once.
fn write_base(repo: &Repo) {
    repo.write(
        "src/lib.rs",
        r#"pub fn validate(amount: u32) -> bool { amount > 0 }

pub fn legacy_check(order: &Order) -> bool { order.total < 1000 }

pub fn process(order: &Order) -> &'static str {
    if !validate(order.total) { return "bad"; }
    if legacy_check(order) { return skip(order); }
    charge(order)
}

pub fn skip(order: &Order) -> &'static str { "skipped" }

pub fn charge(order: &Order) -> &'static str { "charged" }

pub struct Order { pub total: u32 }
"#,
    );
    // An unchanged external caller does not disqualify process as an in-scope entry point.
    repo.write(
        "src/jobs.rs",
        r#"use crate::lib::process;

pub fn retry_all(orders: &[Order]) {
    for o in orders { process(o); }
}
"#,
    );
}

/// Head lib.rs: 1 changed body, 1 rename, 1 removal, 2 additions,
/// and `process` rewired to the new helpers.
fn write_head(repo: &Repo) {
    repo.write(
        "src/lib.rs",
        r#"pub fn validate(amount: u32) -> bool { amount > 0 && amount < 10_000 }

pub fn assess_risk(order: &Order) -> bool { order.total > 500 }

pub fn process(order: &Order) -> &'static str {
    if !validate(order.total) { return "bad"; }
    if assess_risk(order) { return flag_for_review(order); }
    charge_back(order)
}

pub fn flag_for_review(order: &Order) -> &'static str { "flagged" }

pub fn charge_back(order: &Order) -> &'static str { "charged" }

pub struct Order { pub total: u32 }
"#,
    );
}

fn fixture() -> Repo {
    let repo = Repo::new("main");
    write_base(&repo);
    repo.commit_all("base");
    write_head(&repo);
    repo.commit_all("head");
    repo
}

#[test]
fn full_review_tree_statuses_and_structure() {
    let repo = fixture();
    let (code, out, err) = repo.run(&["HEAD~1", "HEAD", "--dump"]);
    assert_eq!(code, 0, "stderr: {err}");

    // summary counts: +2 (assess_risk, flag_for_review) -2 (legacy_check, skip)
    // ~2 (process, validate) >1 (charge→charge_back rename)
    assert_summary(&repo, "HEAD~1", "HEAD", (2, 2, 2, 1));

    // process is the top root (body changed sorts first) even though
    // retry_all in unchanged jobs.rs calls it
    let process_idx = out.find("~ process").expect("process as changed root");
    let validate_idx = out.find("~ validate").expect("validate under process");
    let assess_idx = out.find("+ assess_risk").expect("assess_risk added");
    let flag_idx = out
        .find("+ flag_for_review")
        .expect("flag_for_review added");
    let charge_back_idx = out.find("> charge_back").expect("charge renamed");
    assert!(
        process_idx < validate_idx,
        "validate must nest under process:\n{out}"
    );
    assert!(process_idx < assess_idx && process_idx < flag_idx);
    assert!(process_idx < charge_back_idx);

    // removed functions are reported as roots too (nothing calls them anymore)
    assert!(out.contains("- legacy_check"), "removed fn missing:\n{out}");
    assert!(out.contains("- skip"), "removed fn missing:\n{out}");

    // unchanged caller file never becomes a node
    assert!(
        !out.contains("retry_all"),
        "jobs.rs leaked into node set:\n{out}"
    );

    // nesting depth: charge_back sits one level under process (2-space indent)
    for line in out.lines() {
        if line.contains("charge_back") {
            assert!(
                line.starts_with("  ") && !line.starts_with("    "),
                "charge_back not nested at depth 1: '{line}'"
            );
        }
        if line.contains("~ process") {
            assert!(!line.starts_with(' '), "process must be a root: '{line}'");
        }
    }
}

#[test]
fn entry_scoping_limits_the_tree() {
    let repo = fixture();
    let (code, out, err) = repo.run(&["HEAD~1", "HEAD", "-e", "flag_for_review", "--dump"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(out.contains("entry points: 1"), "expected 1 root:\n{out}");
    assert!(out.contains("+ flag_for_review"));
    // leaf: no children lines indented under it
    let flagged_line = out.lines().find(|l| l.contains("flag_for_review")).unwrap();
    assert!(
        !flagged_line.ends_with("…"),
        "must have no children: '{flagged_line}'"
    );
}

#[test]
fn focus_mode_is_a_cli_flag_without_effect_on_dump() {
    // --focus only affects TUI defaults; dump renders the full tree either way
    let repo = fixture();
    let (code, out, _) = repo.run(&["HEAD~1", "HEAD", "--focus", "--dump"]);
    assert_eq!(code, 0);
    assert!(out.contains("~ validate"));
}

#[test]
fn worktree_is_the_default_head() {
    let repo = Repo::new("worktree");
    write_base(&repo);
    repo.commit_all("base");
    write_head(&repo);
    repo.commit_all("head");
    // one more change on top: staged but uncommitted
    repo.write(
        "src/audit.rs",
        "pub fn audit(order: &Order) -> bool { order.total == 0 }\n",
    );
    repo.git(&["add", "-A"]);
    let (code, out, err) = repo.run(&["--dump"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(
        out.contains("HEAD~1 → worktree"),
        "header wrong (default base is HEAD~1):\n{out}"
    );
    assert!(
        out.contains("+ audit"),
        "worktree-only file missing:\n{out}"
    );
    // and the committed changes are still in scope
    assert!(out.contains("~ process"));
}

#[test]
fn unknown_base_ref_fails_with_clear_error() {
    let repo = fixture();
    let (code, _out, err) = repo.run(&["does-not-exist", "HEAD", "--dump"]);
    assert_ne!(code, 0);
    assert!(
        err.contains("does-not-exist") && err.contains("does not resolve"),
        "unclear error: {err}"
    );
}

#[test]
fn single_commit_repo_default_base_fails() {
    let repo = Repo::new("single");
    write_base(&repo);
    repo.commit_all("base");
    // default base HEAD~1 cannot resolve with one commit
    let (code, _out, err) = repo.run(&["--dump"]);
    assert_ne!(code, 0);
    assert!(err.contains("HEAD~1"), "error should name the ref: {err}");
}

#[test]
fn identical_refs_report_no_changes() {
    let repo = fixture();
    let (code, out, err) = repo.run(&["HEAD", "HEAD", "--dump"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(out.contains("no file changes"), "unexpected output:\n{out}");
}

#[test]
fn test_functions_hidden_by_default_included_with_tests_flag() {
    let repo = Repo::new("testfilter");
    repo.write(
        "src/lib.rs",
        r#"pub fn validate(amount: u32) -> bool { amount > 0 }

pub fn process(order: &Order) -> bool {
    validate(order.total)
}

pub struct Order { pub total: u32 }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_validate_rejects_zero() {
        assert!(!validate(0));
    }
}
"#,
    );
    repo.commit_all("base");
    repo.write(
        "src/lib.rs",
        r#"pub fn validate(amount: u32) -> bool { amount > 0 && amount < 10_000 }

pub fn process(order: &Order) -> bool {
    validate(order.total)
}

pub struct Order { pub total: u32 }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_validate_rejects_zero() {
        assert!(!validate(0));
    }
    #[test]
    fn test_validate_rejects_huge() {
        assert!(!validate(20_000));
    }
}
"#,
    );
    repo.commit_all("head");

    // default: test module fns are not nodes; the change is visible via
    // validate alone
    let (code, out, err) = repo.run(&["HEAD~1", "HEAD", "--dump"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(!out.contains("test_validate"), "tests leaked:\n{out}");
    assert!(out.contains("~ validate"));

    // --tests brings them in; the new test fn shows as added
    let (code, out, err) = repo.run(&["HEAD~1", "HEAD", "--tests", "--dump"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(
        out.contains("+ tests::test_validate_rejects_huge"),
        "missing new test:\n{out}"
    );
}

#[test]
fn all_flag_includes_clean_entry_points() {
    // base and head differ only inside flag_for_review; everything else is
    // unchanged, and unused_helper is a clean, uncalled entry point
    let lib = |flag_return: &str| {
        format!(
            r#"pub fn validate(amount: u32) -> bool {{ amount > 0 && amount < 10_000 }}

pub fn assess_risk(order: &Order) -> bool {{ order.total > 500 }}

pub fn process(order: &Order) -> &'static str {{
    if !validate(order.total) {{ return "bad"; }}
    if assess_risk(order) {{ return flag_for_review(order); }}
    charge_back(order)
}}

pub fn flag_for_review(order: &Order) -> &'static str {{ "{flag_return}" }}

pub fn charge_back(order: &Order) -> &'static str {{ "charged" }}

pub fn unused_helper() -> u32 {{ 42 }}

pub struct Order {{ pub total: u32 }}
"#
        )
    };
    let repo = Repo::new("allflag");
    repo.write("src/lib.rs", &lib("flagged"));
    repo.commit_all("base");
    repo.write("src/lib.rs", &lib("flagged!"));
    repo.commit_all("head");

    let (code, out, err) = repo.run(&["HEAD~1", "HEAD", "--json"]);
    assert_eq!(code, 0, "{err}");
    let default: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(default["roots"], serde_json::json!(["src/lib.rs::process"]));
    let (code, out, err) = repo.run(&["HEAD~1", "HEAD", "--all", "--json"]);
    assert_eq!(code, 0, "{err}");
    let all: serde_json::Value = serde_json::from_str(&out).unwrap();
    let roots = all["roots"].as_array().unwrap();
    for key in [
        "src/lib.rs::process",
        "src/lib.rs::unused_helper",
        "src/lib.rs::Order",
    ] {
        assert!(roots.contains(&serde_json::json!(key)), "{all}");
    }
}

#[test]
fn csharp_c_and_cpp_review_changes_and_renames() {
    for (path, source) in [
        ("src/app.cs", "class App {\nstatic int helper(int x) { return x + 1; }\nstatic int legacy() { return 7; }\nstatic int run() { return helper(1) + legacy(); }\n// EXTRA\n}\n"),
        ("src/app.c", "int helper(int x) { return x + 1; }\nint legacy(void) { return 7; }\nint run(void) { return helper(1) + legacy(); }\n// EXTRA\n"),
        ("src/app.cpp", "namespace app {\ntemplate<typename T> T helper(T x) { return x + 1; }\nint legacy() { return 7; }\nint run() { return helper<int>(1) + legacy(); }\n// EXTRA\n}\n"),
    ] {
        let repo = Repo::new(path.rsplit('/').next().unwrap());
        repo.write(path, source);
        repo.commit_all("base");
        let added = if path.ends_with(".cs") {
            "static int added() { return 9; }"
        } else {
            "int added() { return 9; }"
        };
        let head = source.replace("x + 1", "x + 2")
            .replace("legacy", "renamed")
            .replace("+ renamed();", "+ renamed() + added();")
            .replace("// EXTRA", added);
        repo.write(path, &head);
        repo.commit_all("head");

        let (code, out, err) = repo.run(&["HEAD~1", "HEAD", "--dump", "-e", "run"]);
        assert_eq!(code, 0, "{path}: {err}");
        for (name, status) in [("run", '~'), ("helper", '~'), ("renamed", '>'), ("added", '+')] {
            let qual = if path.ends_with(".cs") { format!("App::{name}") }
                else if path.ends_with(".cpp") { format!("app::{name}") }
                else { name.to_string() };
            let line = out.lines().find(|line| line.contains(&format!("{status} {qual}")))
                .unwrap_or_else(|| panic!("{path}: missing {status} {qual}:\n{out}"));
            if name == "run" {
                assert!(!line.starts_with(' '), "{path}: root was nested: {line}");
            } else {
                assert!(line.starts_with("  ") && !line.starts_with("    "), "{path}: callee not nested under run: {line}");
            }
        }
        let legacy = if path.ends_with(".cs") { "App::legacy" }
            else if path.ends_with(".cpp") { "app::legacy" } else { "legacy" };
        assert!(!out.contains(&format!("- {legacy}")), "{path}: rename reported as removal:\n{out}");
    }
}

fn assert_fn_prefix(out: &str, glyph: char, name: &str) {
    let prefix = format!("{glyph} {name} ");
    assert!(
        out.lines()
            .any(|line| line.trim_start().starts_with(&prefix)),
        "missing function prefix '{prefix}':\n{out}"
    );
}

#[test]
fn file_rename_preserves_function_identity() {
    let repo = Repo::new("file-rename");
    repo.write(
        "src/old.rs",
        "pub fn run() -> u32 { 1 }\npub fn helper() -> u32 { 2 }\n",
    );
    repo.commit_all("base");
    repo.git(&["mv", "src/old.rs", "src/new.rs"]);
    repo.commit_all("rename");

    assert_summary(&repo, "HEAD~1", "HEAD", (0, 0, 0, 0));
}

#[test]
fn file_rename_with_one_edit_changes_only_that_function() {
    let repo = Repo::new("file-rename-edit");
    repo.write("src/old.rs", "pub fn run() -> u32 { helper() + 1 }\npub fn helper() -> u32 { 2 }\npub fn stable() -> u32 { 3 }\n");
    repo.commit_all("base");
    repo.git(&["mv", "src/old.rs", "src/new.rs"]);
    repo.write("src/new.rs", "pub fn run() -> u32 { helper() + 9 }\npub fn helper() -> u32 { 2 }\npub fn stable() -> u32 { 3 }\n");
    repo.commit_all("rename and edit");

    let (code, out, err) = repo.run(&["HEAD~1", "HEAD", "--dump"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert_summary(&repo, "HEAD~1", "HEAD", (0, 0, 1, 0));
    assert_fn_prefix(&out, '~', "run");
}

#[test]
fn untracked_worktree_file_reports_added_functions() {
    let repo = Repo::new("untracked");
    repo.write("src/lib.rs", "pub fn stable() -> u32 { 1 }\n");
    repo.commit_all("base");
    repo.write("src/new.rs", "pub fn fresh() -> u32 { 2 }\n");

    let (code, out, err) = repo.run(&["HEAD", "worktree", "--dump"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert_summary(&repo, "HEAD", "worktree", (1, 0, 0, 0));
    assert_fn_prefix(&out, '+', "fresh");
}

#[test]
fn non_utf8_worktree_source_is_modified_not_removed() {
    let repo = Repo::new("non-utf8-worktree");
    repo.write("src/lib.rs", "pub fn text() -> &'static str { \"cafe\" }\n");
    repo.commit_all("base");
    fs::write(
        repo.root.join("src/lib.rs"),
        b"pub fn text() -> &'static str { \"caf\xe9\" }\n",
    )
    .unwrap();

    let (code, out, err) = repo.run(&["HEAD", "worktree", "--dump"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert_summary(&repo, "HEAD", "worktree", (0, 0, 1, 0));
    assert_fn_prefix(&out, '~', "text");
}

#[test]
fn non_utf8_commit_source_uses_the_same_lossy_decoding() {
    let repo = Repo::new("non-utf8-commit");
    repo.write("src/lib.rs", "pub fn text() -> &'static str { \"cafe\" }\n");
    repo.commit_all("base");
    fs::write(
        repo.root.join("src/lib.rs"),
        b"pub fn text() -> &'static str { \"caf\xe9\" }\n",
    )
    .unwrap();
    repo.commit_all("head");

    let (code, out, err) = repo.run(&["HEAD~1", "HEAD", "--dump"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert_summary(&repo, "HEAD~1", "HEAD", (0, 0, 1, 0));
    assert_fn_prefix(&out, '~', "text");
}

#[test]
fn adding_only_a_utf8_bom_changes_no_functions() {
    let repo = Repo::new("bom");
    repo.write("src/lib.rs", "pub fn run() -> u32 { 1 }\n");
    repo.commit_all("base");
    fs::write(
        repo.root.join("src/lib.rs"),
        b"\xef\xbb\xbfpub fn run() -> u32 { 1 }\n",
    )
    .unwrap();

    assert_summary(&repo, "HEAD", "worktree", (0, 0, 0, 0));
    repo.commit_all("BOM only");
    assert_summary(&repo, "HEAD~1", "HEAD", (0, 0, 0, 0));
}

#[test]
fn branch_named_like_a_tracked_path_resolves_as_base() {
    let repo = Repo::new("ambiguous-ref");
    repo.write("src/base.rs", "pub fn run() -> u32 { 1 }\n");
    repo.commit_all("base");
    repo.git(&["branch", "src/base.rs"]);
    repo.write("src/base.rs", "pub fn run() -> u32 { 2 }\n");
    repo.commit_all("head");

    let (code, out, err) = repo.run(&["src/base.rs", "HEAD", "--dump"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(
        out.contains("src/base.rs → HEAD"),
        "user-facing ref label lost:\n{out}"
    );
    assert_summary(&repo, "src/base.rs", "HEAD", (0, 0, 1, 0));
    assert_fn_prefix(&out, '~', "run");
}

#[test]
fn worktree_is_rejected_as_base() {
    let repo = Repo::new("worktree-base");
    repo.write("src/lib.rs", "pub fn run() -> u32 { 1 }\n");
    repo.commit_all("base");

    let (code, _out, err) = repo.run(&["worktree", "HEAD", "--dump"]);
    assert_ne!(code, 0);
    assert!(
        err.contains("base") && err.contains("worktree") && err.contains("commit revision"),
        "unclear worktree base error: {err}"
    );
}

#[test]
fn non_ascii_changed_paths_work_in_both_head_modes() {
    let repo = Repo::new("non-ascii-path");
    repo.write("src/café.rs", "pub fn run() -> u32 { 1 }\n");
    repo.commit_all("base");
    repo.write("src/café.rs", "pub fn run() -> u32 { 2 }\n");

    let (code, out, err) = repo.run(&["HEAD", "worktree", "--dump"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert_summary(&repo, "HEAD", "worktree", (0, 0, 1, 0));
    assert_fn_prefix(&out, '~', "run");
    repo.commit_all("head");
    let (code, out, err) = repo.run(&["HEAD~1", "HEAD", "--dump"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert_summary(&repo, "HEAD~1", "HEAD", (0, 0, 1, 0));
    assert_fn_prefix(&out, '~', "run");
}

#[test]
fn commit_head_reviews_with_unchanged_caller_candidates() {
    let repo = Repo::new("commit-candidates");
    repo.write("src/lib.rs", "pub fn run() -> u32 { 1 }\n");
    repo.write("src/caller.rs", "pub fn caller() -> u32 { run() }\n");
    repo.write(
        "src/prefix.rs",
        "pub fn rerun() -> u32 { 2 }\npub fn run_all() -> u32 { 3 }\n",
    );
    repo.write("src/unrelated.rs", "pub fn unrelated() -> u32 { 4 }\n");
    repo.write(
        "src/large.rs",
        &format!(
            "// {}\npub fn oversized() -> u32 {{ run() }}\n",
            "x".repeat(512 * 1024)
        ),
    );
    repo.commit_all("base");
    repo.write("src/lib.rs", "pub fn run() -> u32 { 9 }\n");
    repo.commit_all("head");

    let (code, out, err) = repo.run(&["HEAD~1", "HEAD", "--dump"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert_summary(&repo, "HEAD~1", "HEAD", (0, 0, 1, 0));
    assert_fn_prefix(&out, '~', "run");
    for name in ["caller", "rerun", "run_all", "unrelated", "oversized"] {
        for glyph in ['+', '-', '~', '>'] {
            let prefix = format!("{glyph} {name} ");
            assert!(
                !out.lines()
                    .any(|line| line.trim_start().starts_with(&prefix)),
                "unchanged candidate reported as changed:\n{out}"
            );
        }
    }
}

#[test]
fn copied_file_with_one_edit_reads_the_original_base() {
    let repo = Repo::new("file-copy-edit");
    repo.write("src/original.rs", "pub fn run() -> u32 { helper() + 1 }\npub fn helper() -> u32 { 2 }\npub fn stable() -> u32 { 3 }\n");
    repo.commit_all("base");
    repo.write("src/copied.rs", "pub fn run() -> u32 { helper() + 9 }\npub fn helper() -> u32 { 2 }\npub fn stable() -> u32 { 3 }\n");
    repo.commit_all("copy and edit");

    let (code, out, err) = repo.run(&["HEAD~1", "HEAD", "--dump"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert_summary(&repo, "HEAD~1", "HEAD", (0, 0, 1, 0));
    assert_fn_prefix(&out, '~', "run");
}

fn json_review(repo: &Repo) -> serde_json::Value {
    let (code, out, err) = repo.run(&["HEAD", "--json"]);
    assert_eq!(code, 0, "{err}");
    serde_json::from_str(&out).expect("one JSON document")
}

fn json_node<'a>(value: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    value["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["name"] == name)
        .unwrap_or_else(|| panic!("missing {name}: {value}"))
}

#[test]
fn self_recursion_remains_an_entry() {
    let repo = Repo::new("self-recursion");
    let base = "fn fact(n: u32) -> u32 { if n == 0 { 1 } else { n * fact(n - 1) } }\n";
    repo.write("src/lib.rs", base);
    repo.commit_all("base");
    repo.write("src/lib.rs", &base.replace("n == 0", "n <= 1"));
    let (code, out, err) = repo.run(&["HEAD", "--dump"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("entry points: 1"), "{out}");
    assert!(out.lines().any(|l| l.starts_with("~ fact ")), "{out}");
}

#[test]
fn qualified_methods_and_signature_overloads_are_distinct() {
    let repo = Repo::new("overloads");
    let rust =
        "struct A; struct B; impl A { fn new() -> u32 { 1 } } impl B { fn new() -> u32 { 2 } }\n";
    let cs = "class Worker { static int Log(int x) { return x + 1; } static int Log(string s) { return s.Length; } }\n";
    repo.write("src/lib.rs", rust);
    repo.write("src/app.cs", cs);
    repo.commit_all("base");
    repo.write("src/lib.rs", &rust.replace("{ 1 }", "{ 3 }"));
    repo.write("src/app.cs", &cs.replace("x + 1", "x + 2"));
    let (code, out, err) = repo.run(&["HEAD", "--dump", "--all"]);
    assert_eq!(code, 0, "{err}");
    assert_fn_prefix(&out, '~', "A::new");
    assert!(!out.contains("~ B::new"), "{out}");
    assert_fn_prefix(&out, '~', "Worker::Log(int x)");
    assert!(!out.contains("~ Worker::Log(string s)"), "{out}");
    let (code, out, err) = repo.run(&["HEAD", "--dump", "--entry", "B::new"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("B::new"), "{out}");
    let (code, _, err) = repo.run(&["HEAD", "--dump", "--entry", "new"]);
    assert_ne!(code, 0);
    assert!(
        err.contains("src/lib.rs::A::new") && err.contains("src/lib.rs::B::new"),
        "{err}"
    );
    assert!(
        err.find("src/lib.rs::A::new") < err.find("src/lib.rs::B::new"),
        "{err}"
    );
}

#[test]
fn cycle_review_order_is_deterministic() {
    let repo = Repo::new("cycle");
    let base = "fn e() { b(); }\nfn b() { a(); }\nfn a() { if false { b(); } c(); }\nfn c() -> u32 { 1 }\n";
    repo.write("src/lib.rs", base);
    repo.commit_all("base");
    repo.write("src/lib.rs", &base.replace("{ 1 }", "{ 2 }"));
    let mut previous = None;
    for _ in 0..10 {
        let (code, out, err) = repo.run(&["HEAD", "--dump"]);
        assert_eq!(code, 0, "{err}");
        assert!(out.contains("entry points: 1"), "{out}");
        assert!(out.lines().any(|l| l.starts_with("  e :")), "{out}");
        if let Some(previous) = &previous {
            assert_eq!(&out, previous);
        }
        previous = Some(out);
    }
}

#[test]
fn unchanged_definitions_make_cross_file_calls_ambiguous() {
    let repo = Repo::new("ambiguous-call");
    repo.write("src/x.rs", "fn caller() -> u32 { helper() + 1 }\n");
    repo.write("src/y.rs", "fn helper() -> u32 { 2 }\n");
    repo.write("src/u.rs", "fn helper() -> u32 { 3 }\n");
    repo.commit_all("base");
    repo.write("src/x.rs", "fn caller() -> u32 { helper() + 2 }\n");
    repo.write("src/y.rs", "fn helper() -> u32 { 4 }\n");
    let value = json_review(&repo);
    let caller = json_node(&value, "caller");
    assert_eq!(caller["callees"], serde_json::json!([]));
    assert_eq!(caller["lost_callees"], serde_json::json!(["helper"]));
    let (code, out, err) = repo.run(&["HEAD", "--dump", "--entry", "caller"]);
    assert_eq!(code, 0, "{err}");
    assert!(!out.contains("helper"), "{out}");
}

#[test]
fn labels_use_distinct_paths_and_entry_accepts_path_suffixes() {
    let repo = Repo::new("labels");
    repo.write(
        "lib.rs",
        "fn first() -> u32 { 1 }\nfn second() -> u32 { 2 }\n",
    );
    for dir in ["crates/a", "crates/b"] {
        fs::create_dir_all(repo.root.join(dir)).unwrap();
    }
    repo.write("crates/a/lib.rs", "fn a() -> u32 { 3 }\n");
    repo.write("crates/b/lib.rs", "fn b() -> u32 { 4 }\n");
    repo.write("src/x.rs", "fn run() -> u32 { 5 }\n");
    repo.commit_all("base");
    repo.write(
        "lib.rs",
        "fn first() -> u32 { 11 }\nfn second() -> u32 { 12 }\n",
    );
    repo.write("crates/a/lib.rs", "fn a() -> u32 { 13 }\n");
    repo.write("crates/b/lib.rs", "fn b() -> u32 { 14 }\n");
    repo.write("src/x.rs", "fn run() -> u32 { 15 }\n");
    let (code, out, err) = repo.run(&["HEAD", "--dump"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("~ first :lib.rs  1-1"), "{out}");
    assert!(out.contains("~ second :lib.rs  2-2"), "{out}");
    assert!(!out.contains(":/lib.rs"), "{out}");
    assert!(
        out.contains(":a/lib.rs") && out.contains(":b/lib.rs"),
        "{out}"
    );
    for entry in ["src/x.rs::run", "x.rs::run"] {
        let (code, out, err) = repo.run(&["HEAD", "--dump", "--entry", entry]);
        assert_eq!(code, 0, "{err}");
        assert!(out.contains("entry points: 1"), "{out}");
        assert_fn_prefix(&out, '~', "run");
    }
}

#[test]
fn json_preserves_revision_spans_moves_and_external_callers() {
    let repo = Repo::new("json-spans");
    repo.write(
        "src/lib.rs",
        "fn edited() -> u32 { 1 }\nfn alpha() -> u32 { 42 }\n",
    );
    repo.write("src/outside.rs", "fn outside() -> u32 { edited() }\n");
    repo.commit_all("base");
    repo.write("src/lib.rs", "// inserted\n// inserted\n// inserted\nfn edited() -> u32 { 2 }\nfn beta() -> u32 { 42 }\n");
    let value = json_review(&repo);
    assert_eq!(value["base"], "HEAD");
    assert_eq!(value["head"], "worktree");
    let edited = json_node(&value, "edited");
    assert_eq!(edited["base"]["start"], 1);
    assert_eq!(edited["head"]["start"], 4);
    assert_ne!(edited["base"]["start"], edited["head"]["start"]);
    assert_eq!(
        edited["external_callers"],
        serde_json::json!(["src/outside.rs::outside"])
    );
    let beta = json_node(&value, "beta");
    assert_eq!(beta["status"], "moved");
    assert_eq!(beta["moved_from"], "alpha");
    assert_eq!(beta["base"]["start"], 2);
    assert_eq!(beta["head"]["start"], 5);
    let keys: Vec<_> = value["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["key"].as_str().unwrap())
        .collect();
    assert!(keys.windows(2).all(|pair| pair[0] < pair[1]));
    let (code, _, err) = repo.run(&["HEAD", "--json", "--dump"]);
    assert_ne!(code, 0);
    assert!(err.contains("--json") && err.contains("--dump"), "{err}");
}

#[test]
fn deep_generated_unchanged_expression_does_not_crash_review() {
    let repo = Repo::new("deep-generated");
    repo.write("src/lib.c", "int helper(void) { return 1; }\n");
    repo.write(
        "src/gen.c",
        &format!(
            "int generated(void) {{ return helper() + 0{}; }}\n",
            "+1".repeat(20_000)
        ),
    );
    repo.commit_all("base");
    repo.write("src/lib.c", "int helper(void) { return 2; }\n");
    let (code, out, err) = repo.run(&["HEAD", "--dump"]);
    assert_eq!(code, 0, "{err}");
    assert_fn_prefix(&out, '~', "helper");
}

#[test]
fn tsx_function_after_jsx_has_its_own_body() {
    let repo = Repo::new("tsx-after-jsx");
    let base =
        "function App() { return <div><span>Hello</span></div>; }\nfunction go() { return 1; }\n";
    repo.write("src/app.tsx", base);
    repo.commit_all("base");
    repo.write("src/app.tsx", &base.replace("return 1", "return 2"));
    let (code, out, err) = repo.run(&["HEAD", "--dump", "--all"]);
    assert_eq!(code, 0, "{err}");
    assert_fn_prefix(&out, '~', "go");
    assert!(!out.contains("~ App"), "{out}");
}

#[test]
fn contract_rename_with_signature_and_body_edits_keeps_history_and_dispatch() {
    let repo = Repo::new("contract-rename-edit");
    let base = "interface IWorker { int Run(int x); }\nabstract class Base : IWorker { public abstract int Run(int x); }\nclass Worker : Base { public override int Run(int x) { return x + 1; } }\nclass Unrelated { public int Execute(int x) { return 99; } }\nclass App { int Start(IWorker worker) { return worker.Run(1); } }\n";
    repo.write("src/worker.cs", base);
    repo.commit_all("base");
    repo.write(
        "src/worker.cs",
        &base
            .replace("int Run(int x)", "long Execute(long x)")
            .replace("worker.Run(1)", "worker.Execute(1)")
            .replace("x + 1", "x + 2"),
    );
    let (code, out, err) = repo.run(&["HEAD", "--json"]);
    assert_eq!(code, 0, "{err}");
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    let nodes = value["nodes"].as_array().unwrap();
    for owner in ["IWorker", "Base", "Worker"] {
        let name = format!("{owner}::Execute");
        let symbol = nodes.iter().find(|n| n["name"] == name).unwrap();
        assert_eq!(symbol["status"], "changed", "{symbol}");
        assert_eq!(symbol["moved_from"], format!("{owner}::Run"));
        assert!(symbol["base"].is_object(), "{symbol}");
    }
    assert!(
        !nodes
            .iter()
            .any(|n| n["name"].as_str().unwrap().ends_with("::Run")),
        "{out}"
    );
    let start = nodes.iter().find(|n| n["name"] == "App::Start").unwrap();
    let callees = start["callees"].as_array().unwrap();
    assert!(
        callees.contains(&serde_json::json!("src/worker.cs::IWorker::Execute")),
        "{start}"
    );
    assert!(
        callees.contains(&serde_json::json!("src/worker.cs::Worker::Execute")),
        "{start}"
    );
    assert!(
        !callees.contains(&serde_json::json!("src/worker.cs::Unrelated::Execute")),
        "{start}"
    );
}

#[test]
fn interface_only_edit_includes_unchanged_transitive_implementation_context() {
    let repo = Repo::new("contract-context");
    repo.write("src/contract.cs", "interface IWorker { int Run(int x); }\n");
    repo.write(
        "src/base.cs",
        "abstract class Base : IWorker { public abstract int Run(int x); }\n",
    );
    repo.write(
        "src/worker.cs",
        "class Worker : Base { public override int Run(int value) { return value; } }\n",
    );
    repo.write("src/leaf.cs", "class Leaf : Worker {}\n");
    repo.write(
        "src/app.cs",
        "class App { int Start(IWorker worker) { return worker.Run(1); } }\n",
    );
    repo.write(
        "src/unrelated.cs",
        "class Unrelated { int Run(int x) { return 99; } }\n",
    );
    repo.commit_all("base");
    repo.write(
        "src/contract.cs",
        "interface IWorker { long Run(int x, int flags); }\n",
    );
    for head in ["worktree", "HEAD"] {
        if head == "HEAD" {
            repo.commit_all("contract change");
        }
        let base = if head == "HEAD" { "HEAD~1" } else { "HEAD" };
        let (code, out, err) = repo.run(&[base, head, "--json"]);
        assert_eq!(code, 0, "{err}");
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        let nodes = value["nodes"].as_array().unwrap();
        let contract = nodes.iter().find(|n| n["name"] == "IWorker::Run").unwrap();
        assert_eq!(contract["status"], "changed");
        assert_eq!(contract["kind"], "declaration");
        for name in ["Base::Run", "Worker::Run", "Leaf"] {
            let symbol = nodes.iter().find(|n| n["name"] == name).unwrap();
            assert_eq!(symbol["status"], "unchanged");
            assert_eq!(symbol["relevant"], true, "{symbol}");
            assert!(symbol["head"].is_object());
        }
        assert!(
            nodes
                .iter()
                .all(|n| !n["name"].as_str().unwrap().starts_with("Unrelated")),
            "{out}"
        );
        let implementation = nodes.iter().find(|n| n["name"] == "Worker::Run").unwrap();
        assert!(
            implementation["contracts"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("src/base.cs::Base::Run")),
            "{implementation}"
        );
        assert!(
            contract["external_callers"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("src/app.cs::App::Start")),
            "{contract}"
        );
        let (code, dump, err) = repo.run(&[base, head, "--dump", "--entry", "IWorker"]);
        assert_eq!(code, 0, "{err}");
        assert!(dump.contains("IWorker::Run"), "{dump}");
    }
}

#[test]
fn type_rename_and_base_list_change_have_separate_review_spans() {
    let repo = Repo::new("type-rename");
    let base = "abstract class Base {\n public abstract int Run();\n}\nclass Worker : Base {\n public override int Run() { return 1; }\n}\n";
    repo.write("src/worker.cs", base);
    repo.commit_all("base");
    repo.write("src/worker.cs", &base.replace("Base", "RenamedBase"));
    let (code, out, err) = repo.run(&["HEAD", "--json"]);
    assert_eq!(code, 0, "{err}");
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    let nodes = value["nodes"].as_array().unwrap();
    let renamed = nodes.iter().find(|n| n["name"] == "RenamedBase").unwrap();
    assert_eq!(renamed["status"], "moved");
    assert_eq!(renamed["moved_from"], "Base");
    let derived = nodes.iter().find(|n| n["name"] == "Worker").unwrap();
    assert_eq!(derived["status"], "changed");
    assert_eq!(derived["head"]["start"], 4);
    assert_eq!(derived["head"]["end"], 4);
    let method = nodes.iter().find(|n| n["name"] == "Worker::Run").unwrap();
    assert_eq!(method["status"], "unchanged");
    assert!(derived["contracts"]
        .as_array()
        .unwrap()
        .contains(&renamed["key"]));
}

#[test]
fn edited_rename_uses_unique_similarity_but_rejects_ties() {
    let repo = Repo::new("edited-rename");
    let base = "pub fn calculate(input: i32) -> i32 { let offset = input * 3; let total = offset + 7; total.max(0) }\n";
    repo.write("src/lib.rs", base);
    repo.commit_all("base");
    repo.write(
        "src/lib.rs",
        &base.replace("calculate", "compute").replace("+ 7", "+ 8"),
    );
    let (code, out, err) = repo.run(&["HEAD", "--json"]);
    assert_eq!(code, 0, "{err}");
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    let node = value["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["name"] == "compute")
        .unwrap();
    assert_eq!(node["status"], "changed");
    assert_eq!(node["moved_from"], "calculate");
    repo.write(
        "src/lib.rs",
        &format!("{}{}", base, base.replace("calculate", "alternate")),
    );
    repo.commit_all("ambiguous base");
    repo.write(
        "src/lib.rs",
        &format!(
            "{}{}",
            base.replace("calculate", "first").replace("+ 7", "+ 8"),
            base.replace("calculate", "second").replace("+ 7", "+ 8")
        ),
    );
    let (code, out, err) = repo.run(&["HEAD", "--json"]);
    assert_eq!(code, 0, "{err}");
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["summary"]["added"], 2);
    assert_eq!(value["summary"]["removed"], 2);
    assert_eq!(value["summary"]["moved"], 0);
}

#[test]
fn recursive_rename_preserves_origin_without_an_edit_status() {
    let repo = Repo::new("recursive-rename");
    repo.write(
        "src/lib.rs",
        "fn repeat(n: u32) -> u32 { if n == 0 { 0 } else { repeat(n - 1) } }\n",
    );
    repo.commit_all("base");
    repo.write(
        "src/lib.rs",
        "fn recur(n: u32) -> u32 { if n == 0 { 0 } else { recur(n - 1) } }\n",
    );
    let (code, out, err) = repo.run(&["HEAD", "--json"]);
    assert_eq!(code, 0, "{err}");
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    let node = value["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["name"] == "recur")
        .unwrap();
    assert_eq!(node["status"], "moved");
    assert_eq!(node["moved_from"], "repeat");
    assert_eq!(node["callees"], serde_json::json!(["src/lib.rs::recur"]));
    assert_eq!(value["summary"]["removed"], 0);
}

#[test]
fn removed_interface_member_retains_unchanged_typed_callers() {
    let repo = Repo::new("removed-contract");
    repo.write("src/contract.cs", "interface IWorker { int Run(); }\n");
    repo.write(
        "src/app.cs",
        "class App { int Start(IWorker worker) { return worker.Run(); } }\n",
    );
    repo.write(
        "src/unrelated.cs",
        "class Unrelated { int Run() { return 1; } }\n",
    );
    repo.commit_all("base");
    repo.write("src/contract.cs", "interface IWorker {}\n");
    let (code, out, err) = repo.run(&["HEAD", "--json"]);
    assert_eq!(code, 0, "{err}");
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    let removed = value["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["name"] == "IWorker::Run")
        .unwrap();
    assert_eq!(removed["status"], "removed");
    assert_eq!(
        removed["external_callers"],
        serde_json::json!(["src/app.cs::App::Start"])
    );
    assert!(removed["head"].is_null());
}

#[test]
fn empty_rust_trait_impl_addition_reviews_its_own_header() {
    let repo = Repo::new("rust-empty-impl");
    let base = "trait Work { fn run(&self) -> u32 { 1 } }\nstruct Worker;\n";
    repo.write("src/lib.rs", base);
    repo.commit_all("base");
    repo.write(
        "src/lib.rs",
        &format!("{base}\nimpl Work for Worker {{}}\n"),
    );
    let value = json_review(&repo);
    let header = json_node(&value, "<Worker as Work>");
    assert_eq!(header["kind"], "type");
    assert_eq!(header["status"], "added");
    assert_eq!(header["head"]["start"], 4);
    assert_eq!(header["head"]["end"], 4);
    assert!(header["base"].is_null());
    assert_eq!(json_node(&value, "Worker")["status"], "unchanged");
    assert!(
        header["contracts"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("src/lib.rs::Work")),
        "{value}"
    );
}

#[test]
fn explicit_base_call_does_not_dispatch_back_to_its_override() {
    let repo = Repo::new("nonvirtual-base-call");
    let base = "class Base { public virtual int Run() { return 1; } }\nclass Worker : Base { public override int Run() { return base.Run(); } }\n";
    repo.write("src/worker.cs", base);
    repo.commit_all("base");
    repo.write("src/worker.cs", &base.replace("return 1", "return 2"));
    let value = json_review(&repo);
    let method = json_node(&value, "Worker::Run");
    assert_eq!(
        method["callees"],
        serde_json::json!(["src/worker.cs::Base::Run"])
    );
    assert_eq!(method["relevant"], true);
}

#[test]
fn indistinguishable_interface_member_renames_remain_unpaired() {
    let repo = Repo::new("ambiguous-contract-renames");
    repo.write(
        "src/worker.ts",
        "interface Worker { first(): void; second(): void; }\n",
    );
    repo.commit_all("base");
    repo.write(
        "src/worker.ts",
        "interface Worker { third(): void; fourth(): void; }\n",
    );
    let value = json_review(&repo);
    assert_eq!(json_node(&value, "Worker::first")["status"], "removed");
    assert_eq!(json_node(&value, "Worker::second")["status"], "removed");
    assert_eq!(json_node(&value, "Worker::third")["status"], "added");
    assert_eq!(json_node(&value, "Worker::fourth")["status"], "added");
    assert_eq!(value["summary"]["moved"], 0);
}

#[test]
fn trait_rename_keeps_empty_implementation_header_history() {
    let repo = Repo::new("rust-trait-rename");
    let base =
        "trait Work { fn run(&self) -> u32 { 1 } }\nstruct Worker;\nimpl Work for Worker {}\n";
    repo.write("src/lib.rs", base);
    repo.commit_all("base");
    repo.write(
        "src/lib.rs",
        &base
            .replace("trait Work ", "trait Task ")
            .replace("impl Work for", "impl Task for"),
    );
    let value = json_review(&repo);
    assert_eq!(json_node(&value, "Task")["moved_from"], "Work");
    let header = json_node(&value, "<Worker as Task>");
    assert_eq!(header["status"], "changed");
    assert_eq!(header["moved_from"], "<Worker as Work>");
    assert_eq!(header["base"]["start"], 3);
    assert_eq!(header["head"]["start"], 3);
}

#[test]
fn cached_review_tracks_same_size_edits_and_untracked_file_lifecycle() {
    let repo = Repo::new("cache-worktree");
    let base = "fn run() { alpha(); }\nfn alpha() {}\nfn bravo() {}\nfn delta() {}\n";
    repo.write("src/lib.rs", base);
    repo.commit_all("base");
    repo.write("src/lib.rs", &base.replace("run() { alpha", "run() { bravo"));
    let cold = json_review(&repo);
    assert_eq!(json_review(&repo), cold);
    assert_eq!(json_node(&cold, "run")["callees"], serde_json::json!(["src/lib.rs::bravo"]));

    repo.write("src/lib.rs", &base.replace("run() { alpha", "run() { delta"));
    let edited = json_review(&repo);
    assert_eq!(json_node(&edited, "run")["callees"], serde_json::json!(["src/lib.rs::delta"]));
    repo.write("src/fresh.rs", "fn fresh() { run(); }\n");
    let added = json_review(&repo);
    assert_eq!(json_node(&added, "fresh")["status"], "added");
    assert_eq!(json_node(&added, "fresh")["callees"], serde_json::json!(["src/lib.rs::run"]));
    assert_eq!(json_review(&repo), added);
    fs::remove_file(repo.root.join("src/fresh.rs")).unwrap();
    assert_eq!(json_review(&repo), edited);
}

#[test]
fn cached_review_respects_revision_and_test_selection_and_recovers_corruption() {
    let repo = Repo::new("cache-options");
    repo.write("src/lib.rs", "fn stable() {}\n");
    repo.commit_all("base");
    repo.write("src/lib.rs", "fn stable() {}\nfn added() {}\n#[test] fn check() { added(); }\n");
    repo.commit_all("head");
    let read = |args: &[&str]| {
        let (code, out, err) = repo.run(args);
        assert_eq!(code, 0, "{err}");
        serde_json::from_str::<serde_json::Value>(&out).unwrap()
    };
    let regular = read(&["HEAD~1", "HEAD", "--json"]);
    assert_eq!(regular["summary"]["added"], 1);
    assert_eq!(read(&["HEAD~1", "HEAD", "--json"]), regular);
    let tests = read(&["HEAD~1", "HEAD", "--tests", "--json"]);
    assert_eq!(json_node(&tests, "check")["status"], "added");
    assert_eq!(tests["summary"]["added"], 2);
    assert_eq!(read(&["HEAD~1", "HEAD", "--json"]), regular);
    let reverse = read(&["HEAD", "HEAD~1", "--json"]);
    assert_eq!(json_node(&reverse, "added")["status"], "removed");
    fs::write(repo.root.join(".git/calldive/analysis.json"), "{interrupted").unwrap();
    assert_eq!(read(&["HEAD~1", "HEAD", "--json"]), regular);
    assert_eq!(read(&["HEAD~1", "HEAD", "--json"]), regular);
}

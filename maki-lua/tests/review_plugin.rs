use mlua::{Lua, Table};
use test_case::test_case;

const REVIEW_SOURCE: &str = include_str!("../../plugins/review/init.lua");
const REVIEW_SPEC: &str = include_str!("../../plugins/review/tests/spec.lua");

#[test_case("changed_files_nudge_without_registering_a_command")]
#[test_case("unchanged_signature_does_not_repeat")]
#[test_case("clean_tree_resets_the_nudge")]
#[test_case("unborn_head_still_reports_untracked_files")]
#[test_case("failed_git_query_does_not_nudge")]
fn review_nudge_regressions(scenario: &str) {
    let lua = Lua::new();
    lua.globals().set("review_source", REVIEW_SOURCE).unwrap();
    let tests: Table = lua.load(REVIEW_SPEC).eval().unwrap();
    tests
        .get::<mlua::Function>(scenario)
        .unwrap()
        .call::<()>(())
        .unwrap();
}

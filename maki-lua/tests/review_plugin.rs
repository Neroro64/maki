use mlua::{Function, Lua, Table};
use test_case::test_case;

const REVIEW_SOURCE: &str = include_str!("../../plugins/review/init.lua");
const REVIEW_SPEC: &str = include_str!("../../plugins/review/tests/spec.lua");
const TEXT_INPUT_SOURCE: &str = include_str!("../../plugins/lib/maki/text_input.lua");

#[test_case("submits_comments_to_a_focused_fix_session", None ; "successful_submission")]
#[test_case("failed_submission_keeps_review_responsive_and_comments", Some("returned") ; "returned_submission_failure")]
#[test_case("failed_submission_keeps_review_responsive_and_comments", Some("raised") ; "raised_submission_failure")]
#[test_case("navigation_moves_native_cursor_without_rebuilding_buffers", None ; "incremental_navigation")]
#[test_case("range_editor_comment_rows_and_resize_preserve_navigation", None ; "range_editor_and_row_mapping")]
#[test_case("typing_deep_in_a_large_diff_only_renders_editor_context", None ; "bounded_editor_typing")]
#[test_case("render_failure_closes_modal_windows_and_preserves_comments", None ; "render_failure_cleanup")]
fn review_command_regressions(scenario: &str, failure: Option<&str>) {
    let lua = Lua::new();
    let text_input: Table = lua.load(TEXT_INPUT_SOURCE).eval().unwrap();
    let globals = lua.globals();
    globals.set("text_input", text_input).unwrap();
    globals.set("review_source", REVIEW_SOURCE).unwrap();
    lua.load(REVIEW_SPEC).exec().unwrap();
    let run: Function = globals.get(scenario).unwrap();
    run.call::<()>(failure).unwrap();
}

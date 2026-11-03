use std::sync::Arc;

use maki_agent::tools::test_support::stub_ctx;
use maki_agent::tools::{ToolContext, ToolRegistry};
use maki_agent::{AgentMode, ToolOutput};
use maki_lua::PluginHost;
use maki_lua::test_support::runtime;
use maki_storage::id::SessionRef;
use serde_json::{Value, json};
use test_case::test_case;

const PWSH_PLUGIN_SRC: &str = include_str!("../../plugins/pwsh/init.lua");
const BATCH_PLUGIN_SRC: &str = include_str!("../../plugins/batch/init.lua");

const SESSION_A: &str = "01965087-4c71-7f00-8000-000000000000";
const SESSION_B: &str = "01965087-4c71-7f00-8000-000000000001";

const TIMED_OUT_FMT: &str = "timed out after 1s; the persistent session was killed";
const NATIVE_FAILURE: &str = "& \"$PSHOME/pwsh\" -NoProfile -NonInteractive -Command 'exit 1'";
const EXIT_SUCCESS: &str = "Exit code: 0";
const EXIT_FAILURE: &str = "Exit code: 1";
const PROBE_ERROR: &str = "maki probe failure";
const PROBE_VALUE: &str = "maki probe value";
const FIRST_OUTPUT: &str = "first-start\nfirst-end";
const SECOND_OUTPUT: &str = "second-start\nsecond-end";
const BATCH_TIMEOUT_SECS: u64 = 5;
const STATUS_SUCCESS: &str = "success";
const STATUS_ERROR: &str = "error";

fn load_pwsh_host() -> (Arc<ToolRegistry>, PluginHost) {
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source("pwsh", PWSH_PLUGIN_SRC).unwrap();
    (reg, host)
}

fn session_ctx(id: &str) -> ToolContext {
    let session: SessionRef = id.parse().expect("valid session id");
    let mut ctx = stub_ctx(&AgentMode::Build);
    ctx.session_id = Some(session);
    ctx
}

fn exec_result(reg: &ToolRegistry, input: Value, ctx: &ToolContext) -> Result<String, String> {
    let entry = reg
        .get("pwsh")
        .unwrap_or_else(|| panic!("tool pwsh not registered"));
    let inv = entry.tool.parse(&input).expect("parse failed");
    let res = runtime().block_on(async { inv.execute(ctx).await }).output;
    match res {
        Ok(ToolOutput::Plain(s) | ToolOutput::Markdown(s)) => Ok(s.text),
        Ok(other) => panic!("unexpected output: {other:?}"),
        Err(e) => Err(e),
    }
}

fn exec(reg: &ToolRegistry, input: Value, ctx: &ToolContext) -> String {
    exec_result(reg, input, ctx).unwrap_or_else(|err| err)
}

#[test]
fn pwsh_runs_a_command_and_reports_output() {
    let (reg, _host) = load_pwsh_host();
    let ctx = session_ctx(SESSION_A);
    let out = exec(&reg, json!({ "command": "echo (41+1)" }), &ctx);
    assert_eq!(out, "42");
}

#[test]
fn pwsh_state_persists_across_calls() {
    let (reg, _host) = load_pwsh_host();
    let ctx = session_ctx(SESSION_A);
    exec(&reg, json!({ "command": "$x = 7" }), &ctx);
    let out = exec(&reg, json!({ "command": "echo $x" }), &ctx);
    assert_eq!(out, "7");
}

#[test]
fn pwsh_reset_discards_state() {
    let (reg, _host) = load_pwsh_host();
    let ctx = session_ctx(SESSION_A);
    exec(&reg, json!({ "command": "$x = 7" }), &ctx);
    let out = exec(&reg, json!({ "command": "echo $x", "reset": true }), &ctx);
    assert_eq!(out, EXIT_SUCCESS);
}

#[test]
fn pwsh_nonzero_exit_is_reported() {
    let (reg, _host) = load_pwsh_host();
    let ctx = session_ctx(SESSION_A);
    let out = exec_result(&reg, json!({ "command": NATIVE_FAILURE }), &ctx);
    assert_eq!(out.unwrap_err(), EXIT_FAILURE);
    let out = exec_result(&reg, json!({ "command": "Write-Output 'recovered'" }), &ctx);
    assert_eq!(
        out.unwrap(),
        "recovered",
        "exit codes must not leak between calls"
    );
}

#[test]
fn pwsh_missing_command_is_rejected() {
    let (reg, _host) = load_pwsh_host();
    let entry = reg.get("pwsh").unwrap();
    assert!(
        entry.tool.parse(&json!({})).is_err(),
        "the schema must require command"
    );
}

#[test_case("1..5", 2, "4\n5"; "last_two_lines")]
#[test_case("1..5", 1, "5"; "last_line")]
#[test_case("1..2", 5, "1\n2"; "fewer_lines_than_requested")]
#[test_case("$null", 2, EXIT_SUCCESS; "empty_output")]
#[test_case("Write-Output \"first`n`nlast\"", 2, "\nlast"; "blank_line")]
fn pwsh_tail_returns_the_last_lines(command: &str, tail: usize, expected: &str) {
    let (reg, _host) = load_pwsh_host();
    let ctx = session_ctx(SESSION_A);
    let out = exec_result(&reg, json!({ "command": command, "tail": tail }), &ctx).unwrap();
    if let Some((visible, retained)) = out.split_once("\nFull output: ") {
        assert_eq!(visible, expected);
        let (path, status) = retained.rsplit_once('\n').unwrap();
        assert_eq!(status, EXIT_SUCCESS);
        let capture = std::fs::read_to_string(path).unwrap();
        assert!(capture.ends_with(expected), "{capture}");
        assert_ne!(capture, expected);
        std::fs::remove_file(path).unwrap();
    } else {
        assert_eq!(out, expected);
    }
}

#[test]
fn pwsh_timeout_kills_the_session_and_state() {
    let (reg, _host) = load_pwsh_host();
    let ctx = session_ctx(SESSION_A);
    exec(&reg, json!({ "command": "$x = 7" }), &ctx);
    let out = exec(
        &reg,
        json!({ "command": "Start-Sleep -Seconds 5", "timeout": 1 }),
        &ctx,
    );
    assert!(out.contains(TIMED_OUT_FMT), "{out}");
    let out = exec(&reg, json!({ "command": "echo $x" }), &ctx);
    assert_eq!(out, EXIT_SUCCESS, "the killed session must start fresh");
}

#[test]
fn pwsh_sessions_are_isolated() {
    let (reg, _host) = load_pwsh_host();
    let ctx_a = session_ctx(SESSION_A);
    exec(&reg, json!({ "command": "$x = 7" }), &ctx_a);
    let ctx_b = session_ctx(SESSION_B);
    let out = exec(&reg, json!({ "command": "echo $x" }), &ctx_b);
    assert_eq!(
        out, EXIT_SUCCESS,
        "session B must not see session A's state"
    );
}

#[test]
fn pwsh_object_output_flushes_promptly() {
    let (reg, _host) = load_pwsh_host();
    let ctx = session_ctx(SESSION_A);
    let out = exec(
        &reg,
        json!({ "command": "[pscustomobject]@{ Answer = 42 }", "timeout": 10 }),
        &ctx,
    );
    assert!(out.contains("Answer") && out.contains("42"), "{out}");
}

#[test_case("$x = @'\nfirst\n\nlast\n'@\n$x", "first\n\nlast"; "here_string")]
#[test_case("@(\n41\n\n1\n) | Measure-Object -Sum | Select-Object -ExpandProperty Sum", "42"; "multiline_expression")]
#[test_case("Write-Output '你好 café 🚀'", "你好 café 🚀"; "unicode")]
#[test_case("[Console]::Write('no newline')", "no newline"; "unterminated_output")]
#[test_case("Write-Output \"`nleading\"", "\nleading"; "leading_blank_line")]
fn pwsh_preserves_command_and_output_boundaries(command: &str, expected: &str) {
    let (reg, _host) = load_pwsh_host();
    let ctx = session_ctx(SESSION_A);
    let out = exec_result(&reg, json!({ "command": command, "timeout": 10 }), &ctx);
    assert_eq!(out.unwrap(), expected);
    let out = exec_result(
        &reg,
        json!({ "command": "Write-Output 42", "timeout": 10 }),
        &ctx,
    );
    assert_eq!(
        out.unwrap(),
        "42",
        "output must not leak into the next call"
    );
}

#[test_case("throw"; "terminating_error")]
#[test_case("Write-Error"; "nonterminating_error")]
fn pwsh_errors_are_reported_without_losing_state(operation: &str) {
    let (reg, _host) = load_pwsh_host();
    let ctx = session_ctx(SESSION_A);
    exec_result(
        &reg,
        json!({ "command": format!("$x = '{PROBE_VALUE}'; function probe {{ $x }}") }),
        &ctx,
    )
    .unwrap();
    let out = exec_result(
        &reg,
        json!({ "command": format!("{operation} '{PROBE_ERROR}'"), "timeout": 10 }),
        &ctx,
    )
    .unwrap_err();
    assert!(out.contains(PROBE_ERROR), "{out}");
    assert!(out.ends_with(EXIT_FAILURE), "{out}");
    let out = exec_result(&reg, json!({ "command": "probe", "timeout": 10 }), &ctx);
    assert_eq!(out.unwrap(), PROBE_VALUE);
}

#[test]
fn pwsh_native_stderr_precedes_completion() {
    let (reg, _host) = load_pwsh_host();
    let ctx = session_ctx(SESSION_A);
    let command = format!(
        "& \"$PSHOME/pwsh\" -NoProfile -NonInteractive -Command '[Console]::Error.Write(\"{PROBE_ERROR}\"); exit 1'"
    );
    let out = exec_result(&reg, json!({ "command": command, "timeout": 10 }), &ctx).unwrap_err();
    assert_eq!(out, format!("{PROBE_ERROR}\n{EXIT_FAILURE}"));
    let out = exec_result(&reg, json!({ "command": "42" }), &ctx);
    assert_eq!(out.unwrap(), "42");
}

#[test_case("'second-start'; 'second-end'", SECOND_OUTPUT, STATUS_SUCCESS; "successful_sibling")]
#[test_case(NATIVE_FAILURE, EXIT_FAILURE, STATUS_ERROR; "failing_sibling")]
#[test_case("$null", EXIT_SUCCESS, STATUS_SUCCESS; "empty_sibling")]
fn pwsh_batch_calls_keep_output_and_completion_separate(
    second_command: &str,
    second_output: &str,
    second_status: &str,
) {
    let (reg, host) = load_pwsh_host();
    host.load_source("batch", BATCH_PLUGIN_SRC).unwrap();
    let mut ctx = session_ctx(SESSION_A);
    ctx.registry = Arc::clone(&reg);
    exec_result(&reg, json!({ "command": "$probe = 42" }), &ctx).unwrap();
    let entry = reg.get("batch").unwrap();
    let invocation = entry
        .tool
        .parse(&json!({
            "tool_calls": [
                { "tool": "pwsh", "parameters": { "command": "'first-start'; 'first-end'", "timeout": BATCH_TIMEOUT_SECS } },
                { "tool": "pwsh", "parameters": { "command": second_command, "timeout": BATCH_TIMEOUT_SECS } },
            ]
        }))
        .unwrap();
    let output = runtime().block_on(invocation.execute(&ctx)).output.unwrap();
    let children = &output.state().expect("batch state")["children"];
    assert_eq!(children[0]["output"], FIRST_OUTPUT, "{children}");
    assert_eq!(children[0]["status"], STATUS_SUCCESS, "{children}");
    assert_eq!(children[1]["output"], second_output, "{children}");
    assert_eq!(children[1]["status"], second_status, "{children}");
    assert_eq!(
        exec_result(&reg, json!({ "command": "$probe" }), &ctx).unwrap(),
        "42"
    );
}

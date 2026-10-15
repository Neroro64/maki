use std::sync::Arc;

use maki_agent::tools::test_support::stub_ctx;
use maki_agent::tools::{ToolContext, ToolRegistry};
use maki_agent::{AgentMode, ToolOutput};
use maki_lua::PluginHost;
use maki_storage::id::SessionRef;
use serde_json::{Value, json};

const PWSH_PLUGIN_SRC: &str = include_str!("../../plugins/pwsh/init.lua");

const SESSION_A: &str = "01965087-4c71-7f00-8000-000000000000";
const SESSION_B: &str = "01965087-4c71-7f00-8000-000000000001";

const TIMED_OUT_FMT: &str = "timed out after 1s; the persistent session was killed";
const NATIVE_FAILURE: &str = "sh -c 'exit 1'";

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

fn exec(reg: &ToolRegistry, input: Value, ctx: &ToolContext) -> String {
    let entry = reg
        .get("pwsh")
        .unwrap_or_else(|| panic!("tool pwsh not registered"));
    let inv = entry.tool.parse(&input).expect("parse failed");
    let res = smol::block_on(async { inv.execute(ctx).await }).output;
    match res {
        Ok(ToolOutput::Plain(s) | ToolOutput::Markdown(s)) => s.text,
        Ok(other) => panic!("unexpected output: {other:?}"),
        Err(e) => e,
    }
}

#[test]
fn pwsh_tool_is_registered() {
    let (reg, _host) = load_pwsh_host();
    assert!(reg.get("pwsh").is_some());
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
    assert_eq!(out, "Exit code: 0");
}

#[test]
fn pwsh_nonzero_exit_is_reported() {
    let (reg, _host) = load_pwsh_host();
    let ctx = session_ctx(SESSION_A);
    let out = exec(&reg, json!({ "command": NATIVE_FAILURE }), &ctx);
    assert_eq!(out, "Exit code: 1", "native exit code must surface");
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

#[test]
fn pwsh_tail_returns_the_last_lines() {
    let (reg, _host) = load_pwsh_host();
    let ctx = session_ctx(SESSION_A);
    let out = exec(&reg, json!({ "command": "1..5", "tail": 2 }), &ctx);
    assert_eq!(out, "4\n5");
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
    assert_eq!(out, "Exit code: 0", "the killed session must start fresh");
}

#[test]
fn pwsh_sessions_are_isolated() {
    let (reg, _host) = load_pwsh_host();
    let ctx_a = session_ctx(SESSION_A);
    exec(&reg, json!({ "command": "$x = 7" }), &ctx_a);
    let ctx_b = session_ctx(SESSION_B);
    let out = exec(&reg, json!({ "command": "echo $x" }), &ctx_b);
    assert_eq!(
        out, "Exit code: 0",
        "session B must not see session A's state"
    );
}

#[test]
fn pwsh_object_output_flushes_promptly() {
    let (reg, _host) = load_pwsh_host();
    let ctx = session_ctx(SESSION_A);
    let out = exec(
        &reg,
        json!({ "command": "$PSVersionTable | Out-String; Get-Location", "timeout": 10 }),
        &ctx,
    );
    println!("OUT: {out}");
    assert!(out.contains("7."), "{out}");
}

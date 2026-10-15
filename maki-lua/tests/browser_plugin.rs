use std::sync::Arc;

use maki_agent::tools::test_support::stub_ctx;
use maki_agent::tools::{ToolContext, ToolRegistry};
use maki_agent::{AgentMode, ToolOutput};
use maki_lua::PluginHost;
use maki_storage::id::SessionRef;
use serde_json::{Value, json};

const BROWSER_PLUGIN_SRC: &str = include_str!("../../plugins/browser/init.lua");

const SESSION: &str = "01965087-4c71-7f00-8000-000000000010";
const LIVE_HTML: &str = "data:text/html,<title>BraveSmoke</title><body><input id=q><button onclick=\"document.title='clicked'\">Go</button></body>";

const BAD_ACTION: &str = "error: action must be one of open, snapshot, click, type, press, scroll, screenshot, eval, status, close";
const SESSION_REQUIRED: &str = "error: browser needs a session";
const NOT_OPEN: &str = "error: no browser open for this session";

fn load_browser_host() -> (Arc<ToolRegistry>, PluginHost) {
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source("browser", BROWSER_PLUGIN_SRC).unwrap();
    (reg, host)
}

fn session_ctx() -> ToolContext {
    let session: SessionRef = SESSION.parse().expect("valid session id");
    let mut ctx = stub_ctx(&AgentMode::Build);
    ctx.session_id = Some(session);
    ctx
}

fn exec(reg: &ToolRegistry, input: Value, ctx: &ToolContext) -> String {
    let entry = reg
        .get("browser")
        .unwrap_or_else(|| panic!("tool browser not registered"));
    let inv = entry.tool.parse(&input).expect("parse failed");
    let res = smol::block_on(async { inv.execute(ctx).await }).output;
    match res {
        Ok(ToolOutput::Plain(s) | ToolOutput::Markdown(s)) => s.text,
        Ok(other) => panic!("unexpected output: {other:?}"),
        Err(e) => e,
    }
}

#[test]
fn browser_tool_is_registered() {
    let (reg, _host) = load_browser_host();
    assert!(reg.get("browser").is_some());
}

#[test]
fn browser_rejects_unknown_action() {
    let (reg, _host) = load_browser_host();
    let ctx = session_ctx();
    let out = exec(&reg, json!({ "action": "teleport" }), &ctx);
    assert_eq!(out, BAD_ACTION);
}

#[test]
fn browser_requires_a_session() {
    let (reg, _host) = load_browser_host();
    let ctx = stub_ctx(&AgentMode::Build);
    let out = exec(
        &reg,
        json!({ "action": "open", "url": "about:blank" }),
        &ctx,
    );
    assert_eq!(out, SESSION_REQUIRED);
}

#[test]
fn browser_reports_no_session_as_not_open() {
    let (reg, _host) = load_browser_host();
    let ctx = session_ctx();
    let out = exec(&reg, json!({ "action": "status" }), &ctx);
    assert_eq!(out, "no browser open");
    let out = exec(&reg, json!({ "action": "click", "index": 0 }), &ctx);
    assert_eq!(out, NOT_OPEN);
}

#[test]
#[ignore = "needs a real Chromium-family browser (MAKI_BROWSER_BIN or brave on PATH)"]
fn browser_open_snapshot_click_eval_screenshot_close() {
    let (reg, _host) = load_browser_host();
    let ctx = session_ctx();

    let out = exec(
        &reg,
        json!({ "action": "open", "url": LIVE_HTML, "headless": true }),
        &ctx,
    );
    assert!(out.contains("BraveSmoke"), "{out}");

    let out = exec(&reg, json!({ "action": "snapshot" }), &ctx);
    assert!(out.contains("2 elements"), "{out}");
    assert!(out.contains("[1] button"), "{out}");

    let out = exec(&reg, json!({ "action": "click", "index": 1 }), &ctx);
    assert_eq!(out, "ok");

    let out = exec(
        &reg,
        json!({ "action": "eval", "source": "document.title" }),
        &ctx,
    );
    assert_eq!(out, "\"clicked\"");

    let path = std::env::temp_dir().join("maki-browser-plugin-smoke.png");
    let out = exec(
        &reg,
        json!({ "action": "screenshot", "path": path.to_string_lossy() }),
        &ctx,
    );
    assert!(out.starts_with("saved "), "{out}");
    assert!(path.exists());
    let _ = std::fs::remove_file(&path);

    let out = exec(&reg, json!({ "action": "close" }), &ctx);
    assert_eq!(out, "browser closed");

    let out = exec(&reg, json!({ "action": "status" }), &ctx);
    assert_eq!(out, "no browser open");
}

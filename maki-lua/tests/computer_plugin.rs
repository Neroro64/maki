use std::sync::Arc;

use maki_agent::tools::ToolRegistry;
use maki_agent::tools::test_support::stub_ctx;
use maki_agent::{AgentMode, ToolOutput};
use maki_lua::PluginHost;
use serde_json::{Value, json};

const COMPUTER_PLUGIN_SRC: &str = include_str!("../../plugins/computer/init.lua");

const BAD_ACTION: &str =
    "error: action must be one of screenshot, mouse, keyboard, position, display, clipboard";
const CLIPBOARD_TEXT: &str = "maki-e2e-clipboard-round-trip";

fn load_computer_host() -> (Arc<ToolRegistry>, PluginHost) {
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source("computer", COMPUTER_PLUGIN_SRC).unwrap();
    (reg, host)
}

fn exec(reg: &ToolRegistry, input: Value) -> String {
    let entry = reg
        .get("computer")
        .unwrap_or_else(|| panic!("tool computer not registered"));
    let inv = entry.tool.parse(&input).expect("parse failed");
    let ctx = stub_ctx(&AgentMode::Build);
    let res = smol::block_on(async { inv.execute(&ctx).await }).output;
    match res {
        Ok(ToolOutput::Plain(s) | ToolOutput::Markdown(s)) => s.text,
        Ok(other) => panic!("unexpected output: {other:?}"),
        Err(e) => e,
    }
}

#[test]
fn computer_tool_is_registered() {
    let (reg, _host) = load_computer_host();
    assert!(reg.get("computer").is_some());
}

#[test]
fn computer_rejects_unknown_action() {
    let (reg, _host) = load_computer_host();
    let out = exec(&reg, json!({ "action": "teleport" }));
    assert_eq!(out, BAD_ACTION);
}

#[test]
#[ignore = "needs a live X11 display; moves the real cursor to its current spot"]
fn computer_position_display_and_mouse_move_work() {
    let (reg, _host) = load_computer_host();

    let out = exec(&reg, json!({ "action": "position" }));
    let (x, y) = out
        .split_once(',')
        .unwrap_or_else(|| panic!("position output: {out}"));
    let x: i64 = x.parse().expect("x coordinate");
    let y: i64 = y.parse().expect("y coordinate");

    let out = exec(&reg, json!({ "action": "display" }));
    let (w, h) = out
        .split_once('x')
        .unwrap_or_else(|| panic!("display output: {out}"));
    assert!(
        w.parse::<u32>().expect("width") > 0 && h.parse::<u32>().expect("height") > 0,
        "display output: {out}"
    );

    let out = exec(
        &reg,
        json!({ "action": "mouse", "mouse_action": "move", "x": x, "y": y }),
    );
    assert_eq!(out, "ok", "moving to the current position must not fail");
}

#[test]
#[ignore = "needs a live display with a clipboard manager"]
fn computer_clipboard_text_round_trips() {
    let (reg, _host) = load_computer_host();
    let out = exec(
        &reg,
        json!({ "action": "clipboard", "text": CLIPBOARD_TEXT }),
    );
    assert_eq!(out, "clipboard set");
    let out = exec(&reg, json!({ "action": "clipboard", "get": true }));
    assert_eq!(out, CLIPBOARD_TEXT);
}

#[test]
#[ignore = "environment-dependent: passes on bare X11/Xvfb, reports a clean error under mutter"]
fn computer_screenshot_captures_or_reports_cleanly() {
    let (reg, _host) = load_computer_host();
    let path = std::env::temp_dir().join("maki-computer-plugin-smoke.png");
    let _ = std::fs::remove_file(&path);
    let out = exec(
        &reg,
        json!({ "action": "screenshot", "path": path.to_string_lossy() }),
    );
    if out.starts_with("saved ") {
        assert!(path.exists());
        let _ = std::fs::remove_file(&path);
    } else {
        assert!(out.starts_with("error: "), "{out}");
        assert!(!path.exists(), "a failed capture must not write a file");
    }
}

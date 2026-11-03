use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::sync_channel;
use std::thread;
use std::time::{Duration, Instant};

use maki_agent::tools::test_support::stub_ctx;
use maki_agent::tools::{ToolContext, ToolRegistry};
use maki_agent::{AgentEvent, AgentMode, CancelToken, EventSender, ToolOutput};
use maki_lua::PluginHost;
use maki_lua::test_support::runtime;
use maki_storage::id::SessionRef;
use serde_json::{Value, json};
use test_case::test_case;
use tokio::sync::mpsc::unbounded_channel;

const LOG_PREFIX: &str = "Full output: ";
const DIAGNOSTIC: &str = "original-diagnostic";
const PARTIAL: &str = "partial-capture";
const EXIT_FAILURE: &str = "Exit code: 7";
const EXIT_SUCCESS: &str = "Exit code: 0";
const WAIT_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const BASH_FAILURE: &str = "printf 'first\\n'; i=1; while [ \"$i\" -le 24 ]; do if [ \"$i\" -eq 12 ]; then printf 'original-diagnostic\\n'; else printf 'record-%s\\n' \"$i\"; fi; i=$((i + 1)); done; printf 'last\\n'; exit 7";
const PWSH_FAILURE: &str = "'first'; 1..24 | ForEach-Object { if ($_ -eq 12) { 'original-diagnostic' } else { \"record-$_\" } }; 'last'; & \"$PSHOME/pwsh\" -NoProfile -NonInteractive -Command 'exit 7'";
const BASH_PARTIAL: &str = "printf 'partial-capture\\n'; sleep 30";
const PWSH_PARTIAL: &str = "[Console]::WriteLine('partial-capture'); [Console]::In.ReadLine()";

fn setup() -> (Arc<ToolRegistry>, PluginHost, ToolContext) {
    let registry = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&registry)).unwrap();
    host.load_source("bash", include_str!("../../plugins/bash/init.lua"))
        .unwrap();
    host.load_source("pwsh", include_str!("../../plugins/pwsh/init.lua"))
        .unwrap();
    host.load_source("read", include_str!("../../plugins/read/init.lua"))
        .unwrap();
    let mut ctx = stub_ctx(&AgentMode::Build);
    ctx.session_id = Some(SessionRef::generate());
    ctx.config.rtk = false;
    ctx.config.max_output_lines = 4;
    ctx.config.max_output_bytes = 256;
    (registry, host, ctx)
}

fn run(
    registry: &ToolRegistry,
    tool: &str,
    input: Value,
    ctx: &ToolContext,
) -> Result<String, String> {
    let entry = registry.get(tool).unwrap();
    let invocation = entry.tool.parse(&input).unwrap();
    match runtime().block_on(invocation.execute(ctx)).output {
        Ok(ToolOutput::Plain(output) | ToolOutput::Markdown(output)) => Ok(output.text),
        Ok(other) => panic!("unexpected shell output: {other:?}"),
        Err(error) => Err(error),
    }
}

fn log_path(output: &str) -> PathBuf {
    let path = output
        .lines()
        .find_map(|line| line.strip_prefix(LOG_PREFIX))
        .unwrap_or_else(|| panic!("missing retained log: {output}"));
    let path = PathBuf::from(path);
    assert!(path.is_absolute(), "{path:?}");
    path
}

#[test_case("bash", BASH_FAILURE; "bash")]
#[test_case("pwsh", PWSH_FAILURE; "pwsh")]
fn failing_shell_keeps_original_diagnostics_readable(tool: &str, command: &str) {
    let (registry, host, ctx) = setup();
    let output = run(&registry, tool, json!({ "command": command }), &ctx).unwrap_err();
    assert!(output.contains("[truncated"), "{output}");
    assert!(!output.contains(DIAGNOSTIC), "{output}");
    assert!(output.ends_with(EXIT_FAILURE), "{output}");
    let path = log_path(&output);
    let diagnostic = run(
        &registry,
        "read",
        json!({ "path": path, "offset": 13, "limit": 1 }),
        &ctx,
    )
    .unwrap();
    assert!(diagnostic.contains(DIAGNOSTIC), "{diagnostic}");
    drop(host);
    let capture = fs::read_to_string(&path).unwrap();
    assert!(capture.starts_with("first\n"), "{capture}");
    assert!(capture.contains(DIAGNOSTIC), "{capture}");
    assert!(capture.ends_with("last"), "{capture}");
    fs::remove_file(path).unwrap();
}

#[test_case("bash", "printf '1\\n2\\n3\\n4\\n5\\n6'"; "bash")]
#[test_case("pwsh", "1..6"; "pwsh")]
fn shell_tail_preserves_the_earlier_output(tool: &str, command: &str) {
    let (registry, _host, ctx) = setup();
    let output = run(
        &registry,
        tool,
        json!({ "command": command, "tail": 2 }),
        &ctx,
    )
    .unwrap();
    assert!(output.starts_with("5\n6\n"), "{output}");
    assert!(output.ends_with(EXIT_SUCCESS), "{output}");
    let path = log_path(&output);
    assert_eq!(fs::read_to_string(&path).unwrap(), "1\n2\n3\n4\n5\n6");
    fs::remove_file(path).unwrap();
}

#[test_case("bash", "exit 7"; "bash")]
#[test_case("pwsh", "& \"$PSHOME/pwsh\" -NoProfile -NonInteractive -Command 'exit 7'"; "pwsh")]
fn silent_shell_failure_keeps_its_exit_code(tool: &str, command: &str) {
    let (registry, _host, ctx) = setup();
    assert_eq!(
        run(&registry, tool, json!({ "command": command }), &ctx).unwrap_err(),
        EXIT_FAILURE
    );
}

#[test_case("bash", "printf 'complete'"; "bash")]
#[test_case("pwsh", "'complete'"; "pwsh")]
fn complete_successful_output_needs_no_retained_log(tool: &str, command: &str) {
    let (registry, _host, ctx) = setup();
    assert_eq!(
        run(&registry, tool, json!({ "command": command }), &ctx).unwrap(),
        "complete"
    );
}

#[test_case("bash", BASH_PARTIAL; "bash")]
#[test_case("pwsh", PWSH_PARTIAL; "pwsh")]
fn shell_timeout_retains_partial_diagnostics(tool: &str, command: &str) {
    let (registry, _host, ctx) = setup();
    let output = run(
        &registry,
        tool,
        json!({ "command": command, "timeout": 5 }),
        &ctx,
    )
    .unwrap_err();
    assert!(output.contains("timed out after 5s"), "{output}");
    assert!(!output.contains(EXIT_SUCCESS), "{output}");
    let path = log_path(&output);
    assert!(fs::read_to_string(&path).unwrap().contains(PARTIAL));
    fs::remove_file(path).unwrap();
}

#[test_case("bash", BASH_PARTIAL; "bash")]
#[test_case("pwsh", PWSH_PARTIAL; "pwsh")]
fn shell_cancellation_retains_streamed_diagnostics(tool: &'static str, command: &'static str) {
    let (events_tx, mut events) = unbounded_channel();
    let event_tx = EventSender::new(events_tx, 0);
    let (trigger, token) = CancelToken::new();
    let (result_tx, results) = sync_channel(1);
    let worker = thread::spawn(move || {
        let (registry, _host, mut ctx) = setup();
        ctx.event_tx = event_tx;
        ctx.tool_use_id = Some(tool.into());
        ctx.cancel = token;
        result_tx
            .send(run(&registry, tool, json!({ "command": command }), &ctx))
            .unwrap();
    });
    let deadline = Instant::now() + WAIT_TIMEOUT;
    let body = loop {
        if let Ok(event) = events.try_recv()
            && let AgentEvent::LiveToolBuf { id, body } = event.event
            && id == tool
        {
            break body;
        }
        assert!(
            Instant::now() < deadline,
            "shell must publish its live buffer"
        );
        thread::sleep(POLL_INTERVAL);
    };
    loop {
        if body.take().text().lines().any(|line| line == PARTIAL) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "shell must stream before cancellation"
        );
        thread::sleep(POLL_INTERVAL);
    }
    trigger.cancel();
    let output = results
        .recv_timeout(WAIT_TIMEOUT)
        .expect("cancelled shell must settle")
        .unwrap_err();
    assert!(output.contains("[cancelled by user after "), "{output}");
    assert!(!output.contains(EXIT_SUCCESS), "{output}");
    let path = log_path(&output);
    assert!(fs::read_to_string(&path).unwrap().contains(PARTIAL));
    fs::remove_file(path).unwrap();
    worker.join().unwrap();
}

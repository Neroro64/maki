use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use maki_agent::tools::test_support::stub_ctx;
use maki_agent::tools::{ToolAudience, ToolContext, ToolRegistry};
use maki_agent::{AgentMode, ToolOutput};
use maki_config::PluginsConfig;
use maki_lua::PluginHost;
use serde_json::{Value, json};
use tempfile::TempDir;
use test_case::test_case;

const TOOL: &str = "lsp_speculate";
const CODE_EXECUTION: &str = "code_execution";
const SOURCE: &str = "fn main() {\n    let answer: i32 = 42;\n}\n";
const CANDIDATE: &str = "    let answer: i32 = \"not an integer\";";
const MISSING_SERVER: &str = "missing LSP executable.exe";
const ERROR_PREFIX: &str = "lsp_speculate error: ";
const RANGE_ERROR: &str = "line range must be inclusive and 1-based with end_line >= start_line";
const MISSING_FILE_ERROR: &str = "cannot resolve file_path:";

fn host_with(options: Value) -> (Arc<ToolRegistry>, PluginHost) {
    let registry = Arc::new(ToolRegistry::new());
    let mut host = PluginHost::new(Arc::clone(&registry)).expect("create Lua host");
    host.load_builtins(&PluginsConfig {
        enabled: true,
        names: vec![TOOL.to_owned(), CODE_EXECUTION.to_owned()],
        packages: Vec::new(),
        opts: HashMap::from([(
            TOOL.to_owned(),
            options.as_object().expect("options object").clone(),
        )]),
    })
    .expect("load real builtins and their permission manifests");
    (registry, host)
}

fn context(registry: &Arc<ToolRegistry>, audience: ToolAudience) -> ToolContext {
    let mut ctx = stub_ctx(&AgentMode::Build);
    ctx.registry = Arc::clone(registry);
    ctx.audience = audience;
    ctx
}

fn execute(
    registry: &ToolRegistry,
    ctx: &ToolContext,
    name: &str,
    input: Value,
) -> Result<String, String> {
    let tool = registry.get(name).expect("builtin tool registered");
    let invocation = tool.tool.parse(&input).expect("valid tool input");
    smol::block_on(invocation.execute(ctx))
        .output
        .map(|output| match output {
            ToolOutput::Plain(text) | ToolOutput::Markdown(text) => text.text,
            other => panic!("unexpected output: {other:?}"),
        })
}

fn candidate_input(path: &Path) -> Value {
    json!({
        "file_path": path.to_str().expect("UTF-8 fixture path"),
        "start_line": 2,
        "end_line": 2,
        "replacement_text": CANDIDATE,
    })
}

fn unavailable_options(directory: &TempDir, option: &str, language_id: &str) -> Value {
    let executable = directory.path().join(MISSING_SERVER);
    let executable = executable.to_str().expect("UTF-8 fixture path");
    let mut options = json!({ "language_id": language_id });
    options[option] = json!(shell_words::quote(executable));
    options
}

#[test_case("rust_command", "main.rs", ""; "rust_adapter_override")]
#[test_case("typescript_command", "main.ts", ""; "typescript_adapter_override")]
#[test_case("command", "main.custom", "custom"; "custom_language_and_command")]
fn unavailable_server_is_an_error_without_changing_disk(
    command_option: &str,
    file_name: &str,
    language_id: &str,
) {
    let directory = TempDir::new().expect("fixture directory");
    let path = directory.path().join(file_name);
    fs::write(&path, SOURCE).expect("fixture source");
    let (registry, _host) = host_with(unavailable_options(&directory, command_option, language_id));
    let error = execute(
        &registry,
        &context(&registry, ToolAudience::MAIN),
        TOOL,
        candidate_input(&path),
    )
    .expect_err("an unavailable server must not become a clean result");
    assert!(error.starts_with(ERROR_PREFIX), "{error}");
    assert!(error.contains(MISSING_SERVER), "{error}");
    assert_eq!(
        fs::read(&path).expect("fixture remains readable"),
        SOURCE.as_bytes()
    );
}

#[test_case(ToolAudience::MAIN; "main")]
#[test_case(ToolAudience::RESEARCH_SUB; "research_subagent")]
#[test_case(ToolAudience::GENERAL_SUB; "general_subagent")]
fn monty_can_await_the_real_tool_and_receives_server_failure(audience: ToolAudience) {
    let directory = TempDir::new().expect("fixture directory");
    let path = directory.path().join("main.rs");
    fs::write(&path, SOURCE).expect("fixture source");
    let (registry, _host) = host_with(unavailable_options(&directory, "rust_command", ""));
    let code = format!(
        "await lsp_speculate(file_path={}, start_line=2, end_line=2, replacement_text={})",
        json!(path.to_str().expect("UTF-8 fixture path")),
        json!(CANDIDATE),
    );
    let error = execute(
        &registry,
        &context(&registry, audience),
        CODE_EXECUTION,
        json!({ "code": code, "timeout": 10 }),
    )
    .expect_err("await must propagate the actual language-server launch failure");
    assert!(error.contains(ERROR_PREFIX), "{error}");
    assert!(error.contains(MISSING_SERVER), "{error}");
    assert_eq!(
        fs::read(&path).expect("fixture remains readable"),
        SOURCE.as_bytes()
    );
}

#[test_case(3, 2; "reversed_range")]
fn invalid_range_is_a_tool_error_before_server_launch(start_line: usize, end_line: usize) {
    let directory = TempDir::new().expect("fixture directory");
    let path = directory.path().join("main.rs");
    fs::write(&path, SOURCE).expect("fixture source");
    let (registry, _host) = host_with(unavailable_options(&directory, "rust_command", ""));
    let mut input = candidate_input(&path);
    input["start_line"] = json!(start_line);
    input["end_line"] = json!(end_line);
    let error = execute(
        &registry,
        &context(&registry, ToolAudience::MAIN),
        TOOL,
        input,
    )
    .expect_err("invalid ranges must be reported through the tool error result");
    assert_eq!(error, format!("{ERROR_PREFIX}{RANGE_ERROR}"));
    assert_eq!(
        fs::read(&path).expect("fixture remains readable"),
        SOURCE.as_bytes()
    );
}

#[test_case("missing.rs"; "missing_file")]
fn missing_file_is_not_created(file_name: &str) {
    let directory = TempDir::new().expect("fixture directory");
    let path = directory.path().join(file_name);
    let (registry, _host) = host_with(unavailable_options(&directory, "rust_command", ""));
    let error = execute(
        &registry,
        &context(&registry, ToolAudience::MAIN),
        TOOL,
        candidate_input(&path),
    )
    .expect_err("missing source is a tool error");
    assert!(
        error.starts_with(&format!("{ERROR_PREFIX}{MISSING_FILE_ERROR}")),
        "{error}"
    );
    assert!(!path.exists());
}

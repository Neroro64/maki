//! File eligibility is checked before reading or parsing source contents.

use std::sync::Arc;

use maki_agent::AgentMode;
use maki_agent::tools::ToolRegistry;
use maki_agent::tools::test_support::stub_ctx;
use maki_config::PluginsConfig;
use maki_lua::PluginHost;
use serde_json::json;
use test_case::test_case;

const INDEX: &str = "index";
const DECLARATION: &str = "supported()";
const SOURCE: &str = "pub fn supported() { println!(\"body is not the overview\"); }\n";
const UNSUPPORTED: &str = "Unsupported file type:";

fn index_file(name: &str, content: &[u8]) -> Result<String, String> {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join(name);
    std::fs::write(&path, content).unwrap();
    let reg = Arc::new(ToolRegistry::new());
    let mut host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_builtins(&PluginsConfig {
        enabled: true,
        names: vec![INDEX.into()],
        ..PluginsConfig::default()
    })
    .unwrap();
    let mut ctx = stub_ctx(&AgentMode::Build);
    ctx.registry = Arc::clone(&reg);
    let entry = reg.get(INDEX).expect("index registered");
    let invocation = entry.tool.parse(&json!({ "path": path })).unwrap();
    maki_lua::test_support::runtime().block_on(invocation.execute(&ctx))
        .output
        .map(|out| match out {
            maki_agent::ToolOutput::Plain(s) => s.text,
            other => panic!("unexpected output: {other:?}"),
        })
}

#[test]
fn supported_source_returns_declarations_instead_of_full_contents() {
    let out = index_file("main.rs", SOURCE.as_bytes()).expect("Rust is supported");
    assert!(out.contains(DECLARATION), "got: {out}");
    assert!(!out.contains("body is not the overview"), "got: {out}");
}

#[test_case("build.ps1" ; "powershell_script")]
#[test_case("module.psm1" ; "powershell_module")]
#[test_case("data.psd1" ; "powershell_data")]
#[test_case("notes.txt" ; "plain_text")]
#[test_case("LICENSE" ; "unmapped_filename")]
fn unsupported_files_are_rejected_without_reading_their_contents(name: &str) {
    let err = index_file(name, &[0xff]).expect_err("unsupported file must be rejected");
    assert!(err.contains(UNSUPPORTED), "got: {err}");
}

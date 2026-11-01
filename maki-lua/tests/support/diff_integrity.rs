use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use maki_agent::agent::{AgentCall, AgentSlot, tool_dispatch};
use maki_agent::tools::hook::Verdict;
use maki_agent::tools::test_support::stub_ctx;
use maki_agent::tools::{CallOrigin, FileAccess, FileKey, ToolAudience, ToolContext, ToolRegistry};
use maki_agent::{
    Agent, AgentInput, AgentMode, AgentParams, AgentRunParams, CancelToken, EventSender, History,
    InputSource, RunContext, ToolDoneEvent,
};
use maki_config::PluginsConfig;
use maki_lua::PluginHost;
use maki_providers::test_support::{Canned, Requests, register_bundled, serve};
use maki_providers::{ContextGauge, Model, Timeouts, provider};
use serde_json::{Value, json};
use tempfile::TempDir;

pub const SOURCE: &str =
    "#[test]\nfn checks_answer() { assert_eq!(answer(), 42); }\nfn answer() -> i32 { 42 }\n";
pub const WARNING: &str = "[DIFF INTEGRITY WARNING]";
static CWD_LOCK: Mutex<()> = Mutex::new(());

pub struct Fixture {
    _host: PluginHost,
    pub registry: Arc<ToolRegistry>,
    pub directory: TempDir,
    pub path: PathBuf,
    pub context: ToolContext,
    pub baseline: Value,
    original_cwd: PathBuf,
    _lock: MutexGuard<'static, ()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.original_cwd).expect("restore fixture cwd");
    }
}

impl Fixture {
    pub fn new(file: &str, source: &str) -> Self {
        Self::build(file, source, false)
    }

    pub fn with_lower_stop(file: &str, source: &str) -> Self {
        Self::build(file, source, true)
    }

    fn build(file: &str, source: &str, lower_stop: bool) -> Self {
        let lock = CWD_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let original_cwd = std::env::current_dir().expect("original cwd");
        let directory = TempDir::new().expect("Git fixture");
        let path = directory.path().join(file);
        fs::write(&path, source).expect("initial fixture source");
        git(directory.path(), &["init", "-q"]);
        git(directory.path(), &["add", "--", file]);
        git(
            directory.path(),
            &[
                "-c",
                "user.name=Integrity",
                "-c",
                "user.email=integrity@example.invalid",
                "commit",
                "-qm",
                "baseline",
            ],
        );
        let reference = git(directory.path(), &["write-tree"]);
        let attributes = git(directory.path(), &["mktree"]);
        std::env::set_current_dir(directory.path()).expect("fixture cwd");
        let registry = Arc::clone(ToolRegistry::global_arc());
        let mut host = PluginHost::new(Arc::clone(&registry)).expect("Lua host");
        if lower_stop {
            host.load_source(
                "lower_stop_layer",
                r#"
maki.api.set_slot("agent.stop", function(prev, value, ctx)
  prev(value, ctx)
  return { continue = "Complete the separate review." }
end)
"#,
            )
            .expect("lower stop layer");
        }
        host.load_builtins(&PluginsConfig {
            enabled: true,
            names: ["diff_integrity", "read", "edit", "write", "task", "bash"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            packages: Vec::new(),
            opts: HashMap::new(),
        })
        .expect("real integrity/read/edit/write/task builtins");
        let mut context = stub_ctx(&AgentMode::Build);
        context.registry = Arc::clone(&registry);
        context.file_access = FileAccess::fresh();
        context
            .file_access
            .begin_snapshot_turn("integrity-fixture", 0);
        context
            .file_access
            .prepare_snapshot()
            .expect("capture real turn baseline");
        let baseline = json!({ "captured": true, "mutated": true,
            "root": fs::canonicalize(directory.path()).expect("canonical fixture root"),
            "reference": reference, "attributes": attributes, "paths": null });
        Self {
            _host: host,
            registry,
            directory,
            path,
            context,
            baseline,
            original_cwd,
            _lock: lock,
        }
    }

    pub fn run(&self, ctx: &ToolContext, name: &str, input: Value) -> ToolDoneEvent {
        smol::block_on(tool_dispatch::run(
            format!("integrity-{name}"),
            name,
            &input,
            ctx,
            CallOrigin::Model,
        ))
    }

    pub fn write(&self, path: &Path, source: &str) {
        self.context.file_access.record_read(&FileKey::new(path));
        let result = self.run(
            &self.context,
            "write",
            json!({ "path": path, "content": source }),
        );
        assert!(!result.is_error, "{}", result.output.as_text());
    }

    pub fn gate(&self, snapshot: Value, reply: &str, request: &str, task: Option<&str>) -> Value {
        let hook = self.registry.agent_hook().expect("real agent hook");
        let cancel = CancelToken::none();
        let call = AgentCall {
            session_id: None,
            task_id: task,
            model: "anthropic/claude-sonnet-4",
            context_size: 0,
            context_window: 200000,
            cancel: &cancel,
            deadline: Instant::now() + Duration::from_secs(30),
        };
        let value =
            json!({ "last_message": reply, "user_request": request, "diff_snapshot": snapshot });
        match smol::block_on(hook.run(AgentSlot::Stop, value.clone(), &call)) {
            Verdict::Replaced(value) => value,
            Verdict::Unchanged => value,
            other => panic!("unexpected integrity verdict: {other:?}"),
        }
    }

    pub fn actual_gate(&self, reply: &str) -> Value {
        self.gate(
            serde_json::to_value(self.context.file_access.diff_snapshot())
                .expect("frozen snapshot"),
            reply,
            "Improve the implementation without weakening checks.",
            None,
        )
    }

    pub fn scripted_task(&self, risky: &str, fixed: &str) -> (String, Requests) {
        self.scripted_completion(risky, fixed, false, None)
    }

    pub fn scripted_main(&self, risky: &str, fixed: &str) -> (String, Requests) {
        self.scripted_completion(risky, fixed, true, None)
    }

    pub fn scripted_justification(&self, risky: &str, reply: &str) -> (String, Requests) {
        self.scripted_completion(risky, "", false, Some(reply))
    }

    fn scripted_completion(
        &self,
        risky: &str,
        fixed: &str,
        main: bool,
        justification: Option<&str>,
    ) -> (String, Requests) {
        let mut content = vec![
            tool_delta("read", "read-before-edit", json!({ "path": self.path })),
            tool_delta(
                "write",
                "weaken-check",
                json!({ "path": self.path, "content": risky }),
            ),
            text_delta("Premature completion: all work is done."),
        ];
        if let Some(reply) = justification {
            content.push(text_delta(reply));
        } else {
            content.push(tool_delta(
                "write",
                "restore-check",
                json!({ "path": self.path, "content": fixed }),
            ));
            content.push(text_delta(
                "Fixed the integrity finding and preserved the assertions.",
            ));
        }
        let script: Vec<Canned> = content
            .into_iter()
            .map(|delta| {
                let body = format!(
                    "data: {}\n\ndata: [DONE]\n\n",
                    json!({
                        "choices": [{ "delta": delta, "finish_reason": "stop" }],
                        "usage": { "prompt_tokens": 10, "completion_tokens": 10 }
                    })
                );
                Canned::sse(Box::leak(body.into_boxed_str()))
            })
            .collect();
        let (url, requests) = serve(Box::leak(script.into_boxed_slice()));
        register_bundled(
            json!({ "slug": "integrity-scripted", "display_name": "Integrity fixture",
            "codec": "openai", "base_url": url,
            "models": [{ "prefixes": ["fixture"], "tier": "strong", "default": true }] }),
            "127.0.0.1",
        );
        let mut model = Model::from_spec("integrity-scripted/fixture").expect("fixture model");
        let provider =
            provider::from_model(&mut model, Timeouts::default()).expect("fixture provider");
        let mut context = self.context.clone();
        context.model = Arc::new(model);
        context.provider = Arc::from(provider);
        if main {
            let (events, _event_rx) = flume::unbounded();
            context.event_tx = EventSender::new(events, 0);
            let mut history = History::new(Vec::new());
            let mut gauge = ContextGauge::default();
            {
                let mut agent = Agent::new(
                    AgentParams {
                        provider: Arc::clone(&context.provider),
                        model: (*context.model).clone(),
                        config: context.config.clone(),
                        tool_output_lines: context.tool_output_lines,
                        permissions: Arc::clone(&context.permissions),
                        session_id: context.session_id.clone(),
                        background_for: None,
                        task_id: None,
                        mailbox: None,
                        timeouts: context.timeouts,
                        file_access: Arc::clone(&context.file_access),
                        prompt_slots: Arc::clone(&context.prompt_slots),
                        subagent_cancels: Arc::clone(&context.subagent_cancels),
                        ledger: Arc::clone(&context.ledger),
                        registry: Arc::clone(&self.registry),
                        audience: ToolAudience::MAIN,
                        model_policy: Arc::clone(&context.model_policy),
                        notes: Arc::default(),
                    },
                    AgentRunParams {
                        history: &mut history,
                        gauge: &mut gauge,
                        event_tx: context.event_tx.clone(),
                        context: Arc::new(|_, _| {
                            RunContext::fixed(
                                "Improve implementation without weakening validation.".into(),
                                Default::default(),
                            )
                        }),
                    },
                );
                smol::block_on(agent.run(AgentInput {
                    message: "Improve implementation without weakening validation.".into(),
                    mode: AgentMode::Build,
                    images: Vec::new(),
                    preamble: Vec::new(),
                    earlier: Vec::new(),
                    thinking: Default::default(),
                    fast: false,
                    workflow: false,
                    prompt: None,
                    source: InputSource::Tui,
                }))
                .expect("real main agent completes only after fixing");
            }
            let text = history
                .as_slice()
                .last()
                .expect("final assistant reply")
                .content
                .iter()
                .find_map(|block| match block {
                    maki_providers::ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .expect("final assistant text");
            return (text, requests);
        }
        let result = self.run(&context, "task", json!({ "description": "Fix answer implementation",
            "prompt": "Improve the answer implementation. Preserve its existing validation.", "subagent_type": "general" }));
        assert!(!result.is_error, "{}", result.output.as_text());
        (result.output.as_text().to_owned(), requests)
    }
}

pub fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("Git fixture command");
    assert!(
        output.status.success(),
        "Git fixture {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("Git UTF-8 output")
        .trim_end_matches(['\r', '\n'])
        .to_owned()
}

pub fn warning(value: &Value) -> &str {
    assert_eq!(
        value["required"],
        json!(true),
        "mandatory integrity warning: {value}"
    );
    let continuation = value["continue"].as_str().expect("warning continuation");
    assert!(continuation.contains(WARNING), "{continuation}");
    continuation
}

pub fn identifiers(warning: &str) -> Vec<String> {
    warning
        .lines()
        .filter_map(|line| {
            line.strip_prefix("DI-").map(|rest| {
                format!(
                    "DI-{}",
                    rest.split_whitespace().next().expect("finding identifier")
                )
            })
        })
        .collect()
}

fn tool_delta(name: &str, id: &str, input: Value) -> Value {
    json!({ "tool_calls": [{ "index": 0, "id": id, "type": "function",
        "function": { "name": name, "arguments": input.to_string() } }] })
}

fn text_delta(text: &str) -> Value {
    json!({ "content": text })
}

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use arc_swap::ArcSwap;
use maki_agent::agent::{
    History, HistorySnapshot, SharedMessages, publish_live_history, tool_dispatch,
};
use maki_agent::tools::test_support::stub_seed;
use maki_agent::tools::{CallOrigin, FileKey, ToolContext, ToolRegistry};
use maki_agent::{AgentSeed, CancelToken, ModelSlot, SessionMailbox, ToolDoneEvent};
use maki_config::PluginsConfig;
use maki_lua::agent_autocmd::HostEvent;
use maki_lua::{PluginHost, UiAction};
use maki_providers::test_support::{Canned, Requests, register_bundled, serve};
use maki_providers::{
    ContentBlock, Message, Model, ModelTier, Role, Timeouts, model_registry, provider,
};
use maki_storage::StateDir;
use maki_storage::id::MakiId;
use serde_json::{Value, json};
use tempfile::TempDir;
use test_case::test_case;

const PLUGIN: &str = "reflect";
const COMMAND: &str = "/reflect";
const PROVIDER: &str = "reflection-scripted";
const WEAK_ID: &str = "cheap-synthesis";
const STRONG_ID: &str = "expensive-main";
const BAD_IMPORT: &str = "use std::bad::Path;";
const FIXED_IMPORT: &str = "use std::path::Path;";
const SOURCE: &str = "use std::path::Path;\nfn answer() -> i32 { 1 }\n";
const CORRECTION: &str = "That import is wrong; use std::path::Path instead of std::bad::Path.";
const RULE: &str = "Import Path from std::path, never std::bad, in Rust source files.";
const THINKING_SECRET: &str = "private raw thinking must not leave the session";
const RAW_LOG_SECRET: &str = "large successful tool log must not enter reflection";
const SAVED: &str = "Reflection saved in ";
const FAILED: &str = "Reflection failed: ";
const NO_EVIDENCE: &str = "no meaningful error or correction evidence";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);
const INVALID_OLD: &str = "use absent::Path;";
static TEST_LOCK: Mutex<()> = Mutex::new(());

struct Fixture {
    host: PluginHost,
    registry: Arc<ToolRegistry>,
    directory: TempDir,
    path: PathBuf,
    session: MakiId,
    mailbox: SessionMailbox,
    seed: Arc<AgentSeed>,
    mirror: SharedMessages,
    requests: Requests,
    state: StateDir,
    _lock: MutexGuard<'static, ()>,
}

impl Fixture {
    fn new(synthesis: &str) -> Self {
        Self::with_response(synthesis, 200)
    }

    fn with_response(synthesis: &str, status: u16) -> Self {
        let lock = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("source.rs");
        fs::write(&path, SOURCE).unwrap();
        let state = StateDir::from_path(directory.path().join("state"));
        let registry = Arc::new(ToolRegistry::new());
        let mut host = PluginHost::new(Arc::clone(&registry)).unwrap();
        host.load_builtins(&PluginsConfig {
            enabled: true,
            names: vec![PLUGIN.to_owned(), "edit".to_owned(), "write".to_owned()],
            packages: Vec::new(),
            opts: HashMap::new(),
        })
        .unwrap();
        let response = if status == 200 {
            format!(
                "data: {}\n\ndata: [DONE]\n\n",
                json!({
                    "choices": [{"delta": {"content": synthesis}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 10, "completion_tokens": 10}
                })
            )
        } else {
            synthesis.to_owned()
        };
        let response = Box::leak(response.into_boxed_str());
        let canned = if status == 200 {
            Canned::sse(response)
        } else {
            Canned::json(status, response)
        };
        let script = Box::leak(vec![canned].into_boxed_slice());
        let (base_url, requests) = serve(script);
        register_bundled(
            json!({
                "slug": PROVIDER,
                "display_name": "Scripted reflection provider",
                "codec": "openai",
                "base_url": base_url,
                "models": [
                    { "prefixes": [WEAK_ID], "tier": "weak", "default": true },
                    { "prefixes": [STRONG_ID], "tier": "strong", "default": true }
                ]
            }),
            "127.0.0.1",
        );
        model_registry::set_and_persist(format!("{PROVIDER}/{WEAK_ID}"), ModelTier::Weak, &state);
        let session = MakiId::generate();
        let mailbox = SessionMailbox::register(session);
        let seed = mailbox.publish_seed(stub_seed(session));
        let mut model = Model::from_spec(&format!("{PROVIDER}/{STRONG_ID}")).unwrap();
        let current_provider = provider::from_model(&mut model, Timeouts::default()).unwrap();
        seed.model_slot.store(Arc::new(ModelSlot {
            model,
            provider: Arc::from(current_provider),
        }));
        let mirror = Arc::new(ArcSwap::from_pointee(HistorySnapshot::default()));
        publish_live_history(session, &mirror);
        let snapshot = json!({
            "id": session.to_string(),
            "cwd": directory.path(),
            "model": format!("{PROVIDER}/{STRONG_ID}")
        });
        host.event_handle()
            .install_session_snapshot(Box::new(move |_| Ok(snapshot.clone())));
        Self {
            host,
            registry,
            directory,
            path,
            session,
            mailbox,
            seed,
            mirror,
            requests,
            state,
            _lock: lock,
        }
    }

    fn context(&self) -> ToolContext {
        let mut ctx = self.seed.tool_context(CancelToken::none());
        ctx.registry = Arc::clone(&self.registry);
        ctx.file_access.record_read(&FileKey::new(&self.path));
        ctx
    }

    fn edit(&self, old: &str, new: &str) -> ToolDoneEvent {
        self.run(
            "edit",
            json!({ "path": self.path, "old_string": old, "new_string": new }),
        )
    }

    fn run(&self, tool: &str, input: Value) -> ToolDoneEvent {
        smol::block_on(tool_dispatch::run(
            tool.to_owned(),
            tool,
            &input,
            &self.context(),
            CallOrigin::Model,
        ))
    }

    fn history(&self, messages: Vec<Message>) {
        History::new(messages).with_mirror(Arc::clone(&self.mirror));
    }

    fn mistakes(&self) {
        let bad = self.edit(FIXED_IMPORT, BAD_IMPORT);
        assert!(!bad.is_error, "{}", bad.output.as_text());
        let failure = self.edit(INVALID_OLD, FIXED_IMPORT);
        assert!(failure.is_error);
        let correction = self.edit(BAD_IMPORT, FIXED_IMPORT);
        assert!(!correction.is_error, "{}", correction.output.as_text());
        self.history(vec![
            Message::user("Add the Path import".into()),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        thinking: THINKING_SECRET.into(),
                        signature: None,
                    },
                    ContentBlock::tool_use(
                        "bad",
                        "edit",
                        json!({
                            "path": self.path, "old_string": FIXED_IMPORT, "new_string": BAD_IMPORT,
                        }),
                    ),
                ],
                ..Default::default()
            },
            blocks(
                Role::User,
                vec![ContentBlock::tool_result(
                    "bad",
                    bad.output.as_text(),
                    false,
                )],
            ),
            blocks(
                Role::Assistant,
                vec![ContentBlock::tool_use(
                    "failure",
                    "edit",
                    json!({
                        "path": self.path, "old_string": INVALID_OLD, "new_string": FIXED_IMPORT,
                    }),
                )],
            ),
            blocks(
                Role::User,
                vec![ContentBlock::tool_result(
                    "failure",
                    failure.output.as_text(),
                    true,
                )],
            ),
            Message::user(CORRECTION.into()),
            blocks(
                Role::Assistant,
                vec![ContentBlock::tool_use(
                    "fix",
                    "edit",
                    json!({
                        "path": self.path, "old_string": BAD_IMPORT, "new_string": FIXED_IMPORT,
                    }),
                )],
            ),
            blocks(
                Role::User,
                vec![ContentBlock::tool_result(
                    "fix",
                    RAW_LOG_SECRET.repeat(4096),
                    false,
                )],
            ),
            blocks(
                Role::Assistant,
                vec![ContentBlock::Text {
                    text: "Fixed the import to std::path::Path.".into(),
                }],
            ),
        ]);
    }

    fn reflect(&self, correction: &str) -> String {
        self.host.event_handle().run_command(
            Arc::from(PLUGIN),
            Arc::from(COMMAND),
            correction.to_owned(),
            0,
        );
        let ui = self.host.ui_action_rx();
        loop {
            match ui.recv_timeout(COMMAND_TIMEOUT).unwrap() {
                UiAction::Flash(message) => return message,
                UiAction::PluginTaskOpened { session, info, .. } => {
                    assert_eq!(session, self.session);
                    assert_eq!(
                        info.model.as_deref(),
                        Some(format!("{PROVIDER}/{WEAK_ID}").as_str())
                    );
                }
                UiAction::PluginTaskEvent { .. } => {}
                _ => panic!("reflection unexpectedly requested additional UI work"),
            }
        }
    }

    fn guards(&self) -> PathBuf {
        self.directory.path().join(".maki/guards.lua")
    }

    fn no_policy(&self) {
        assert!(!self.guards().exists());
        assert!(!self.directory.path().join(".maki/rules.md").exists());
        assert!(self.mailbox.drain().is_empty());
    }

    fn request(&self) -> Value {
        let requests = self.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        requests[0].json()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        model_registry::unset_and_persist(
            &format!("{PROVIDER}/{WEAK_ID}"),
            ModelTier::Weak,
            &self.state,
        );
    }
}

fn blocks(role: Role, content: Vec<ContentBlock>) -> Message {
    Message {
        role,
        content,
        ..Default::default()
    }
}

fn proposal() -> Value {
    json!({
        "kind": "guard", "rule": RULE,
        "guard": {
            "kind": "regex", "pattern": "use\\s+std::bad::Path\\s*;", "file_pattern": "*.rs",
            "failed_example": BAD_IMPORT, "fixed_example": FIXED_IMPORT,
        }
    })
}

#[test]
fn reflection_uses_configured_weak_model_and_enforces_saved_guard() {
    let fixture = Fixture::new(&proposal().to_string());
    fixture.mistakes();
    let outcome = fixture.reflect("");
    assert!(outcome.starts_with(SAVED), "{outcome}");
    let request = fixture.request();
    assert_eq!(request["model"], WEAK_ID);
    assert!(request["tools"].as_array().is_none_or(Vec::is_empty));
    let prompt = request["messages"].to_string();
    assert!(prompt.contains(BAD_IMPORT));
    assert!(prompt.contains(FIXED_IMPORT));
    assert!(prompt.contains(CORRECTION));
    assert!(!prompt.contains(THINKING_SECRET));
    assert!(!prompt.contains(RAW_LOG_SECRET));
    assert!(prompt.len() < 32768);
    assert!(fixture.mailbox.claim_wake().is_empty());
    let notices = fixture.mailbox.drain();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].first_text_content().unwrap().contains(RULE));
    let before = fs::read(&fixture.path).unwrap();
    let rejected = fixture.edit(FIXED_IMPORT, BAD_IMPORT);
    assert!(rejected.is_error, "{}", rejected.output.as_text());
    assert!(rejected.output.as_text().contains(RULE));
    assert_eq!(fs::read(&fixture.path).unwrap(), before);
    let safe = fixture.edit("{ 1 }", "{ 2 }");
    assert!(!safe.is_error, "{}", safe.output.as_text());
    assert_eq!(
        fs::read_to_string(&fixture.path).unwrap(),
        SOURCE.replace("{ 1 }", "{ 2 }")
    );
}

#[test_case("not JSON"; "malformed_json")]
#[test_case("null"; "no_synthesis")]
#[test_case(r#"{"kind":"behavioral","rule":"Be careful and follow best practices."}"#; "platitude")]
#[test_case(r#"{"kind":"guard","rule":"Use std::path::Path when importing Path in Rust.","guard":{"kind":"regex","pattern":"[","file_pattern":"*.rs","failed_example":"use std::bad::Path;","fixed_example":"use std::path::Path;"}}"#; "invalid_pattern")]
#[test_case(r#"{"kind":"guard","rule":"Use std::path::Path when importing Path in Rust.","guard":{"kind":"regex","pattern":"Path","file_pattern":"*.rs","failed_example":"use std::bad::Path;","fixed_example":"use std::path::Path;"}}"#; "rejects_fixed_example")]
#[test_case(r#"{"kind":"guard","rule":"Use std::path::Path when importing Path in Rust.","guard":{"kind":"regex","pattern":"missing::Path","file_pattern":"*.rs","failed_example":"use missing::Path;","fixed_example":"use std::path::Path;"}}"#; "invented_example")]
#[test_case(r#"{"kind":"behavioral","rule":"Use std::path::Path when importing Path in Rust.","command":"rm source.rs"}"#; "executable_extra_key")]
fn invalid_synthesis_never_persists_or_notifies(synthesis: &str) {
    let fixture = Fixture::new(synthesis);
    fixture.mistakes();
    let outcome = fixture.reflect("");
    assert!(outcome.starts_with(FAILED), "{outcome}");
    fixture.no_policy();
    fixture.request();
    assert_eq!(fs::read_to_string(&fixture.path).unwrap(), SOURCE);
}

#[test]
fn provider_failure_never_saves_partial_synthesis() {
    let fixture = Fixture::with_response(
        r#"{"error":{"message":"reflection model unavailable"}}"#,
        400,
    );
    fixture.mistakes();
    let outcome = fixture.reflect("");
    assert!(outcome.starts_with(FAILED), "{outcome}");
    fixture.no_policy();
    fixture.request();
}

#[test]
fn historical_nonfinal_fix_cannot_be_saved_as_a_guard() {
    let mut synthesis = proposal();
    synthesis["guard"]["fixed_example"] = json!(INVALID_OLD);
    let fixture = Fixture::new(&synthesis.to_string());
    fixture.mistakes();
    let outcome = fixture.reflect("");
    assert!(outcome.starts_with(FAILED), "{outcome}");
    fixture.no_policy();
}

#[test_case("Explain this module", ""; "ordinary_chat")]
#[test_case(CORRECTION, ""; "correction_without_working_fix")]
#[test_case("Explain this module", CORRECTION; "argument_without_working_fix")]
fn no_error_evidence_skips_model_and_persistence(text: &str, correction: &str) {
    let fixture = Fixture::new(&proposal().to_string());
    fixture.history(vec![Message::user(text.into())]);
    let outcome = fixture.reflect(correction);
    assert!(outcome.contains(NO_EVIDENCE), "{outcome}");
    assert!(fixture.requests.lock().unwrap().is_empty());
    fixture.no_policy();
}

#[test]
fn command_argument_can_supply_the_correction_for_a_grounded_fix() {
    let fixture = Fixture::new(&proposal().to_string());
    fixture.mistakes();
    fixture.history(vec![
        blocks(
            Role::Assistant,
            vec![ContentBlock::tool_use(
                "fix",
                "edit",
                json!({
                    "path": fixture.path, "old_string": BAD_IMPORT, "new_string": FIXED_IMPORT,
                }),
            )],
        ),
        blocks(
            Role::User,
            vec![ContentBlock::tool_result("fix", "completed", false)],
        ),
    ]);
    let outcome = fixture.reflect(CORRECTION);
    assert!(outcome.starts_with(SAVED), "{outcome}");
    assert!(
        fixture.request()["messages"]
            .to_string()
            .contains(CORRECTION)
    );
}

#[test_case(false, false, true; "ordinary_user_correction_without_failed_tool")]
#[test_case(true, false, false; "hidden_correction_is_not_user_evidence")]
#[test_case(false, true, false; "observation_is_not_user_correction")]
fn user_correction_and_actual_working_fix_are_meaningful_evidence(
    hidden: bool,
    observation: bool,
    meaningful: bool,
) {
    let fixture = Fixture::new(&proposal().to_string());
    let bad = fixture.edit(FIXED_IMPORT, BAD_IMPORT);
    assert!(!bad.is_error, "{}", bad.output.as_text());
    let fix = fixture.edit(BAD_IMPORT, FIXED_IMPORT);
    assert!(!fix.is_error, "{}", fix.output.as_text());
    let mut correction = if observation {
        Message::observation(CORRECTION.into())
    } else {
        Message::user(CORRECTION.into())
    };
    if hidden {
        correction.display_text = Some(String::new());
    }
    fixture.history(vec![
        correction,
        blocks(
            Role::Assistant,
            vec![ContentBlock::tool_use(
                "fix",
                "edit",
                json!({
                    "path": fixture.path, "old_string": BAD_IMPORT, "new_string": FIXED_IMPORT,
                }),
            )],
        ),
        blocks(
            Role::User,
            vec![ContentBlock::tool_result(
                "fix",
                fix.output.as_text(),
                false,
            )],
        ),
    ]);
    let outcome = fixture.reflect("");
    if meaningful {
        assert!(outcome.starts_with(SAVED), "{outcome}");
        assert!(fixture.guards().exists());
        assert!(
            fixture.request()["messages"]
                .to_string()
                .contains(CORRECTION)
        );
    } else {
        assert!(outcome.contains(NO_EVIDENCE), "{outcome}");
        assert!(fixture.requests.lock().unwrap().is_empty());
        fixture.no_policy();
    }
}

#[test]
fn oversized_synthesized_rule_never_persists() {
    let synthesis = json!({ "kind": "behavioral", "rule": format!("Use std::path::Path in Rust source files {}", "always ".repeat(100)) });
    let fixture = Fixture::new(&synthesis.to_string());
    fixture.mistakes();
    assert!(fixture.reflect("").starts_with(FAILED));
    fixture.no_policy();
}

#[test]
fn behavioral_reflection_appends_rule_and_current_context_without_waking_chat() {
    let synthesis = json!({ "kind": "behavioral", "rule": RULE });
    let fixture = Fixture::new(&synthesis.to_string());
    fixture.mistakes();
    let outcome = fixture.reflect("");
    assert!(outcome.starts_with(SAVED), "{outcome}");
    assert!(!fixture.guards().exists());
    let rules = fs::read_to_string(fixture.directory.path().join(".maki/rules.md")).unwrap();
    assert!(rules.contains(RULE));
    assert!(fixture.mailbox.claim_wake().is_empty());
    assert!(
        fixture.mailbox.drain()[0]
            .first_text_content()
            .unwrap()
            .contains(RULE)
    );
}

#[test]
fn latest_rewind_is_session_scoped_and_cleared_on_session_end() {
    let fixture = Fixture::new(&proposal().to_string());
    fixture.history(vec![Message::user(
        "Please reflect on the corrected import".into(),
    )]);
    let events = fixture.host.event_handle();
    let other = MakiId::generate();
    let trajectory = format!(
        "tool bad edit input: {}\ntool_result bad [error]: invalid import\nuser: {CORRECTION}\ntool fix edit input: {}\ntool_result fix [ok]: completed",
        json!({ "path": fixture.path, "old_string": FIXED_IMPORT, "new_string": BAD_IMPORT }),
        json!({ "path": fixture.path, "old_string": BAD_IMPORT, "new_string": FIXED_IMPORT })
    );
    events.fire(
        Some(&other),
        HostEvent::Rewind {
            turn_index: 0,
            trajectory: &trajectory,
        },
    );
    assert!(fixture.reflect("").contains(NO_EVIDENCE));
    events.fire(
        Some(&fixture.session),
        HostEvent::Rewind {
            turn_index: 0,
            trajectory: "obsolete discarded trajectory",
        },
    );
    events.fire(
        Some(&fixture.session),
        HostEvent::Rewind {
            turn_index: 0,
            trajectory: &trajectory,
        },
    );
    let outcome = fixture.reflect("");
    assert!(outcome.starts_with(SAVED), "{outcome}");
    let request = fixture.request();
    let prompt = request["messages"].to_string();
    assert!(prompt.contains("Latest discarded rewind trajectory"));
    assert!(!prompt.contains("obsolete discarded trajectory"));
    events.fire(
        Some(&fixture.session),
        HostEvent::SessionEnd {
            reason: "closed",
            deadline_ms: None,
        },
    );
    fixture.mailbox.drain();
    assert!(fixture.reflect("").contains(NO_EVIDENCE));
    assert_eq!(fixture.requests.lock().unwrap().len(), 1);
    assert!(fixture.mailbox.drain().is_empty());
}

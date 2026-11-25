//! Lightweight prompt-cache keepalive pings while an agent waits on slow,
//! non-sleep tools or background tasks.
//!
//! Provider prompt caches (Anthropic ephemeral breakpoints, Gemini implicit
//! prefix caches, OpenAI sticky slots) expire after ~5 minutes of inactivity.
//! Re-sending the session's exact `system`, `tools`, and `messages` prefix with
//! a minimal 1-token output budget (`ThinkingConfig::Off`, discarded reply)
//! refreshes the server's cache TTL at cached-read rates instead of paying a
//! full uncached prompt rebuild when the tool or background subagent settles.

use std::time::Duration;

use maki_providers::provider::Provider;
use maki_providers::{
    Message, Model, RequestOptions, Role, ThinkingConfig, adapt_images_for_model,
};
use maki_storage::id::SessionRef;
use serde_json::Value;
use tokio::sync::mpsc::unbounded_channel;
use tokio::time::sleep;
use tracing::debug;

use super::history::{History, UNAVAILABLE_RESULT, close_dangling_tool_calls};
use crate::cancel::CancelToken;
use crate::{AgentEvent, CacheKeepaliveEvent, EventSender, RunLedger};

const KEEPALIVE_MAX_TOKENS: u32 = 1;
const KEEPALIVE_PING_TEXT: &str = ".";
const MIN_SLEEP_SKIP_SECS: f64 = 30.0;

pub struct KeepaliveSpec<'a> {
    pub provider: &'a dyn Provider,
    pub model: &'a Model,
    pub history: &'a History,
    pub fast: bool,
    pub session_id: Option<&'a SessionRef>,
    pub event_tx: &'a EventSender,
    pub ledger: Option<&'a RunLedger>,
}

/// True when any tool call in the round is an explicit long sleep (`sleep`,
/// `Start-Sleep`, `asyncio.sleep`, `time.sleep` >= 30s), where the agent
/// intentionally asked to pause rather than waiting on active work.
pub(crate) fn is_explicit_sleep_wait(calls: &[(String, String, Value)]) -> bool {
    calls
        .iter()
        .any(|(_, name, input)| call_is_explicit_sleep(name, input))
}

fn call_is_explicit_sleep(name: &str, input: &Value) -> bool {
    match name {
        "bash" => input
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(shell_command_is_sleep),
        "pwsh" => input
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(pwsh_command_is_sleep),
        "code_execution" => input
            .get("code")
            .and_then(Value::as_str)
            .is_some_and(python_code_is_sleep),
        "batch" => input
            .get("tool_calls")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items.iter().any(|entry| {
                    let Some(tool) = entry.get("tool").and_then(Value::as_str) else {
                        return false;
                    };
                    let params = entry.get("parameters").unwrap_or(entry);
                    call_is_explicit_sleep(tool, params)
                })
            }),
        _ => false,
    }
}

fn shell_command_is_sleep(cmd: &str) -> bool {
    let trimmed = cmd.trim();
    let Some(rest) = trimmed.strip_prefix("sleep") else {
        return false;
    };
    let Some(arg) = rest
        .strip_prefix(char::is_whitespace)
        .map(str::trim_start)
        .and_then(|s| s.split(|c: char| c.is_whitespace() || c == ';').next())
    else {
        return false;
    };
    parse_duration_secs(arg).is_some_and(|secs| secs >= MIN_SLEEP_SKIP_SECS)
}

fn pwsh_command_is_sleep(cmd: &str) -> bool {
    let lower = cmd.trim().to_ascii_lowercase();
    let rest = lower
        .strip_prefix("start-sleep")
        .or_else(|| lower.strip_prefix("sleep"));
    let Some(rest) = rest.and_then(|r| r.strip_prefix(char::is_whitespace)) else {
        return false;
    };
    for token in rest.split(|c: char| c.is_whitespace() || c == ';') {
        if token.is_empty() || token.starts_with('-') {
            continue;
        }
        if let Ok(secs) = token.parse::<f64>() {
            let is_ms = rest.contains("-milliseconds") || rest.contains("-m ");
            let actual = if is_ms { secs / 1000.0 } else { secs };
            return actual >= MIN_SLEEP_SKIP_SECS;
        }
        break;
    }
    false
}

fn python_code_is_sleep(code: &str) -> bool {
    for marker in ["asyncio.sleep(", "time.sleep("] {
        let mut rest = code;
        while let Some(idx) = rest.find(marker) {
            let after = &rest[idx + marker.len()..];
            let arg: String = after
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '_')
                .filter(|c| *c != '_')
                .collect();
            if let Ok(secs) = arg.parse::<f64>()
                && secs >= MIN_SLEEP_SKIP_SECS
            {
                return true;
            }
            rest = after;
        }
    }
    false
}

fn parse_duration_secs(arg: &str) -> Option<f64> {
    let arg = arg.trim_matches(|c| c == '\'' || c == '"');
    if arg.is_empty() {
        return None;
    }
    let (num, mult) = match arg.as_bytes().last().copied()? {
        b's' | b'S' => (&arg[..arg.len() - 1], 1.0),
        b'm' | b'M' => (&arg[..arg.len() - 1], 60.0),
        b'h' | b'H' => (&arg[..arg.len() - 1], 3600.0),
        b'd' | b'D' => (&arg[..arg.len() - 1], 86_400.0),
        _ => (arg, 1.0),
    };
    num.parse::<f64>().ok().map(|v| v * mult)
}

/// Sends one 1-token keepalive request over `spec.history` without mutating it.
/// Dangling tool calls (in-flight tool wait) are closed on a scratch clone, and
/// a trailing assistant reply gets a 1-char synthetic user turn so every API
/// accepts the message sequence while reading the entire history from cache.
pub async fn send_keepalive_once(spec: KeepaliveSpec<'_>, cancel: &CancelToken) {
    let Some((system, tools)) = spec.history.request_prefix() else {
        return;
    };
    if spec.history.is_empty() {
        return;
    }
    let mut messages = spec.history.as_slice().to_vec();
    close_dangling_tool_calls(&mut messages, UNAVAILABLE_RESULT);
    if messages
        .last()
        .is_some_and(|msg| matches!(msg.role, Role::Assistant))
    {
        messages.push(Message::user(KEEPALIVE_PING_TEXT.into()));
    }

    let Ok(adapted) = cancel
        .race(adapt_images_for_model(spec.model, &messages))
        .await
    else {
        return;
    };
    let fitted = spec.model.with_turn_output(KEEPALIVE_MAX_TOKENS);
    let opts = RequestOptions {
        thinking: ThinkingConfig::Off,
        fast: spec.fast,
    }
    .clamped(&fitted);
    let (ptx, _prx) = unbounded_channel();
    let result = cancel
        .race(spec.provider.stream_message(
            &fitted,
            &adapted,
            system,
            tools,
            &ptx,
            opts,
            spec.session_id,
        ))
        .await;
    let Ok(Ok(response)) = result else {
        debug!(model = %spec.model.id, "prompt-cache keepalive skipped on error or cancel");
        return;
    };
    let cost = spec.model.billed_cost(&response.usage, opts.fast);
    let list_cost = spec.model.list_cost(&response.usage, opts.fast);
    let subsidised_list_cost = spec.model.subsidised_list_cost(&response.usage, opts.fast);
    if let Some(ledger) = spec.ledger {
        ledger.add(response.usage, cost, list_cost);
    }
    debug!(
        model = %spec.model.id,
        input_tokens = response.usage.input,
        cache_read = response.usage.cache_read,
        output_tokens = response.usage.output,
        "prompt-cache keepalive refreshed"
    );
    let _ = spec
        .event_tx
        .send(AgentEvent::CacheKeepalive(Box::new(CacheKeepaliveEvent {
            usage: response.usage,
            model: spec.model.id.clone(),
            cost,
            subsidised_list_cost,
        })));
}

/// Runs periodic keepalive pings every `interval` until dropped or `cancel` fires.
pub async fn keepalive_loop(interval: Duration, spec: KeepaliveSpec<'_>, cancel: CancelToken) {
    if interval.is_zero() {
        return;
    }
    loop {
        if cancel.race(sleep(interval)).await.is_err() {
            return;
        }
        send_keepalive_once(
            KeepaliveSpec {
                provider: spec.provider,
                model: spec.model,
                history: spec.history,
                fast: spec.fast,
                session_id: spec.session_id,
                event_tx: spec.event_tx,
                ledger: spec.ledger,
            },
            &cancel,
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Envelope;
    use crate::agent::frame::{RunContext, fit_frame};
    use crate::tools::RequestTools;
    use maki_providers::provider::BoxFuture;
    use maki_providers::{ContentBlock, ModelInfo, ProviderEvent, StreamResponse, TokenUsage};
    use maki_storage::id::MakiId;
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use test_case::test_case;
    use tokio::sync::mpsc::UnboundedSender;

    #[test_case("bash", json!({"command": "sleep 60"}), true ; "bash_sleep_60")]
    #[test_case("bash", json!({"command": "sleep 2m"}), true ; "bash_sleep_2m")]
    #[test_case("bash", json!({"command": "sleep 5"}), false ; "bash_short_sleep_keeps_warming")]
    #[test_case("bash", json!({"command": "cargo test --workspace"}), false ; "bash_cargo_test")]
    #[test_case("pwsh", json!({"command": "Start-Sleep -Seconds 120"}), true ; "pwsh_start_sleep")]
    #[test_case("pwsh", json!({"command": "Start-Sleep -Milliseconds 500"}), false ; "pwsh_short_ms_sleep")]
    #[test_case("code_execution", json!({"code": "await asyncio.sleep(45)"}), true ; "python_asyncio_sleep")]
    #[test_case("batch", json!({"tool_calls": [{"tool": "bash", "parameters": {"command": "sleep 90"}}]}), true ; "batch_sleep")]
    fn detects_explicit_long_sleep_calls(tool: &str, input: Value, expected: bool) {
        let calls = vec![("id_1".to_owned(), tool.to_owned(), input)];
        assert_eq!(is_explicit_sleep_wait(&calls), expected);
    }

    struct SeenCall {
        max_output: Option<u32>,
        message_count: usize,
        session_id: Option<String>,
    }

    struct RecordingProvider {
        seen: Mutex<Vec<SeenCall>>,
    }

    impl Provider for RecordingProvider {
        fn stream_message<'a>(
            &'a self,
            model: &'a Model,
            messages: &'a [Message],
            _system: &'a str,
            _tools: &'a Value,
            _event_tx: &'a UnboundedSender<ProviderEvent>,
            _opts: RequestOptions,
            session_id: Option<&'a SessionRef>,
        ) -> BoxFuture<'a, Result<StreamResponse, crate::AgentError>> {
            self.seen.lock().unwrap().push(SeenCall {
                max_output: model.output_tokens(),
                message_count: messages.len(),
                session_id: session_id.map(|s| s.as_str().to_owned()),
            });
            Box::pin(async {
                Ok(StreamResponse {
                    message: Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::Text { text: ".".into() }],
                        ..Default::default()
                    },
                    usage: TokenUsage {
                        input: 4,
                        output: 1,
                        cache_read: 50_000,
                        cache_creation: 0,
                        cost: Some(0.005),
                    },
                    ..Default::default()
                })
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, crate::AgentError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    #[tokio::test]
    async fn send_keepalive_closes_dangling_calls_without_mutating_history() {
        let model = Model::from_spec("copilot/gemini-4-carbon").unwrap();
        let mut history = History::new(vec![
            Message::user("audit repo".into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use(
                    "call_1",
                    "bash",
                    json!({"command": "cargo test"}),
                )],
                ..Default::default()
            },
        ]);
        fit_frame(
            &mut history,
            RunContext::fixed("sys".into(), RequestTools::default()),
            None,
            &model,
        );
        let provider = RecordingProvider {
            seen: Mutex::new(Vec::new()),
        };
        let session = SessionRef::from(MakiId::generate());
        let (tx, mut rx) = unbounded_channel::<Envelope>();
        let event_tx = EventSender::new(tx, 1);
        let ledger = Arc::new(RunLedger::default());

        send_keepalive_once(
            KeepaliveSpec {
                provider: &provider,
                model: &model,
                history: &history,
                fast: false,
                session_id: Some(&session),
                event_tx: &event_tx,
                ledger: Some(&ledger),
            },
            &CancelToken::none(),
        )
        .await;

        assert_eq!(history.len(), 2, "live history must stay untouched");
        let seen = provider.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].max_output, Some(KEEPALIVE_MAX_TOKENS));
        assert_eq!(
            seen[0].message_count, 3,
            "scratch copy closes dangling tool_use"
        );
        assert_eq!(seen[0].session_id.as_deref(), Some(session.as_str()));
        assert_eq!(ledger.totals().usage.cache_read, 50_000);
        let ev = rx.try_recv().expect("keepalive event emitted");
        assert!(matches!(ev.event, AgentEvent::CacheKeepalive(_)));
    }
}

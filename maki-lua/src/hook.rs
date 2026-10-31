//! The bridge from the host's hooks ([`maki_agent::tools::hook`] and
//! [`maki_agent::agent::hook`]) to the slot chains plugins register.
//! Everything here is a hop: the agent asks, the Lua thread answers, and
//! nothing decides anything on the way.

use std::sync::Arc;
use std::time::{Duration, Instant};

use maki_agent::agent::{AgentCall, AgentHook, AgentSlot};
use maki_agent::permissions::LayerAnswer;
use maki_agent::tools::ToolContext;
use maki_agent::tools::hook::{Authority, HookCall, HookStage, ToolHook, Verdict};
use maki_agent::tools::registry::BoxFuture;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use tokio::time::timeout_at;

use crate::api::slot::{
    AGENT_STOP_SLOT, ANY_TOOL, LayerCost, LayeredTools, PERMISSION_PROMPT_SLOT, host_slot_name,
};
use crate::api::util::ctx::LuaCtx;
use crate::plugin_permissions::Permission;
use crate::runtime::{
    HookRun, KILL_GRACE, PromptRun, Request, SLICE_KILL, WATCHDOG_POLL_INTERVAL, hook_failure,
};

/// What a layer on any agent slot pays, all of it content. Every slot can
/// hand it text the user or the model wrote (`request_instructions` on
/// `agent.compact.before`, `last_message` on `agent.stop`), and steering the
/// agent costs what sending a prompt does (`SENDS_PROMPT`) plus
/// `session_read`. No tool call is steered, so the authority is `Inert`.
const AGENT_SLOT_COST: LayerCost<'static> = LayerCost::Host {
    authority: Authority::Inert,
    content: &[
        Permission::Agent,
        Permission::SessionRead,
        Permission::SessionWrite,
    ],
};

/// Conversation content, both ways, on every stage. A layer that sees the
/// call reads what the model wrote (the file `write` puts on disk, the page
/// `webfetch` asks for), and every layer decides what the model reads back:
/// an output layer rewrites the result, while an input layer or a prompt can
/// refuse with any text it likes, or turn a command into `echo`. Without this
/// a `net`-only plugin could ship the input `ToolDone` keeps from it, or put
/// words in front of the model.
const TOOL_LAYER_CONTENT: &[Permission] = &[Permission::SessionRead, Permission::SessionWrite];

/// How long past a chain's deadline this thread may still answer it. It polls
/// a chain before it checks the deadline, so a layer that finishes in its
/// first slice is reported however late it started. That slice may queue
/// behind another task's, which the watchdog cuts at [`SLICE_KILL`], and may
/// itself run [`KILL_GRACE`] before it is shot. Each kill lands within a
/// watchdog poll, which also covers the round trip of the reply.
const ANSWER_GRACE: Duration = SLICE_KILL
    .saturating_add(KILL_GRACE)
    .saturating_add(WATCHDOG_POLL_INTERVAL.saturating_mul(2));

/// What every layer on a tool call pays, `permission.prompt` included: the
/// call's {authority}, plus [`TOOL_LAYER_CONTENT`].
fn tool_layer_cost(authority: Authority) -> LayerCost<'static> {
    LayerCost::Host {
        authority,
        content: TOOL_LAYER_CONTENT,
    }
}

pub(crate) struct SlotHook {
    pub(crate) tx: UnboundedSender<Request>,
    pub(crate) layered: Arc<LayeredTools>,
}

fn deadline_ms(deadline: Instant) -> u64 {
    deadline
        .saturating_duration_since(Instant::now())
        .as_millis() as u64
}

impl SlotHook {
    /// A missing runtime is a missing opinion, except for a completion gate:
    /// stop must answer successfully before its caller may finish.
    fn send(&self, run: HookRun) -> BoxFuture<'_, Verdict> {
        let stop = run.slots.iter().any(|slot| slot == AGENT_STOP_SLOT);
        let deadline = run.deadline;
        let (reply, answer) = oneshot::channel();
        let request = Request::RunHook { run, reply };
        Box::pin(async move {
            let send = async {
                if self.tx.send(request).is_err() {
                    return hook_failure(stop, "plugin runtime is gone");
                }
                answer
                    .await
                    .unwrap_or_else(|_| hook_failure(stop, "plugin runtime dropped the stop reply"))
            };
            if stop {
                timeout_at(deadline.into(), send)
                    .await
                    .unwrap_or_else(|_| hook_failure(true, "deadline exceeded"))
            } else {
                send.await
            }
        })
    }
}

impl ToolHook for SlotHook {
    fn wraps(&self, tool: &str, stage: HookStage) -> bool {
        self.layered.wraps(tool, stage)
    }

    fn answer_grace(&self) -> Duration {
        ANSWER_GRACE
    }

    fn run<'a>(
        &'a self,
        stage: HookStage,
        value: Value,
        call: &'a HookCall<'a>,
    ) -> BoxFuture<'a, Verdict> {
        self.send(HookRun {
            slots: vec![
                host_slot_name(call.tool, stage),
                host_slot_name(ANY_TOOL, stage),
            ],
            cost: tool_layer_cost(call.authority),
            cancel: call.cancel.clone(),
            deadline: call.deadline,
            value,
            // The `ctx` table a layer receives.
            call: json!({
                "tool": call.tool,
                "tool_id": call.tool_id,
                "tool_kind": call.tool_kind,
                "input": call.input,
                "session_id": call.session_id,
                "origin": call.origin.as_str(),
                "output_mode": call.output_mode.as_str(),
                "receipt": call.receipt,
                "deadline_ms": deadline_ms(call.deadline),
            }),
            may_ask: stage == HookStage::Input,
        })
    }

    fn prompt<'a>(
        &'a self,
        input: &'a Value,
        scopes: &'a [String],
        call: &'a HookCall<'a>,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Option<LayerAnswer>> {
        Box::pin(async move {
            if !self.layered.layers_surface(PERMISSION_PROMPT_SLOT) {
                return None;
            }
            let (reply, answer) = oneshot::channel();
            let run = PromptRun {
                cost: tool_layer_cost(call.authority),
                cancel: call.cancel.clone(),
                deadline: call.deadline,
                req: json!({
                    "tool": call.tool,
                    "tool_id": call.tool_id,
                    "input": input,
                    "scopes": scopes,
                }),
                ctx: Box::new(LuaCtx::handler(ctx)),
            };
            self.tx.send(Request::RunPrompt { run, reply }).ok()?;
            answer.await.ok().flatten()
        })
    }
}

impl AgentHook for SlotHook {
    fn wraps(&self, slot: AgentSlot) -> bool {
        self.layered.layers_surface(slot.name())
    }

    fn answer_grace(&self) -> Duration {
        ANSWER_GRACE
    }

    fn run<'a>(
        &'a self,
        slot: AgentSlot,
        value: Value,
        call: &'a AgentCall<'a>,
    ) -> BoxFuture<'a, Verdict> {
        self.send(HookRun {
            slots: vec![slot.name().to_owned()],
            cost: AGENT_SLOT_COST,
            cancel: call.cancel.clone(),
            deadline: call.deadline,
            value,
            call: json!({
                "session_id": call.session_id,
                "task_id": call.task_id,
                "model": call.model,
                "context_size": call.context_size,
                "context_window": call.context_window,
                "deadline_ms": deadline_ms(call.deadline),
            }),
            may_ask: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::future::pending;
    use std::time::Duration;

    use maki_agent::CancelToken;
    use test_case::test_case;

    use super::{AgentCall, AgentHook, AgentSlot, Arc, Instant, SlotHook, Verdict, json};

    const MODEL: &str = "test/model";
    const HOOK_TIMEOUT: Duration = Duration::from_secs(10);
    const WARNING: &str = "[DIFF INTEGRITY WARNING]";

    #[test_case(AgentSlot::Stop, true ; "stop_send_closed")]
    #[test_case(AgentSlot::Stop, false ; "stop_reply_closed")]
    #[test_case(AgentSlot::UserMessage, true ; "other_slot_send_closed")]
    #[test_case(AgentSlot::UserMessage, false ; "other_slot_reply_closed")]
    #[tokio::test]
    async fn broken_hook_transport_only_denies_stop(slot: AgentSlot, close_request: bool) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let hook = SlotHook {
            tx,
            layered: Arc::default(),
        };
        let cancel = CancelToken::none();
        let call = AgentCall {
            session_id: None,
            task_id: None,
            model: MODEL,
            context_size: 0,
            context_window: 0,
            cancel: &cancel,
            deadline: Instant::now() + HOOK_TIMEOUT,
        };
        let verdict = if close_request {
            drop(rx);
            AgentHook::run(&hook, slot, json!({}), &call).await
        } else {
            tokio::select! {
                v = AgentHook::run(&hook, slot, json!({}), &call) => v,
                _ = async {
                    drop(rx.recv().await.unwrap());
                    pending::<()>().await
                } => unreachable!(),
            }
        };
        if slot == AgentSlot::Stop {
            assert!(matches!(verdict, Verdict::Denied(warning)
                if warning.contains(WARNING)));
        } else {
            assert!(matches!(verdict, Verdict::Unchanged));
        }
    }

    #[tokio::test]
    async fn unanswered_stop_transport_denies_at_deadline() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let hook = SlotHook {
            tx,
            layered: Arc::default(),
        };
        let cancel = CancelToken::none();
        let call = AgentCall {
            session_id: None,
            task_id: None,
            model: MODEL,
            context_size: 0,
            context_window: 0,
            cancel: &cancel,
            deadline: Instant::now(),
        };
        let verdict = AgentHook::run(&hook, AgentSlot::Stop, json!({}), &call).await;
        assert!(matches!(verdict, Verdict::Denied(warning)
            if warning.contains(WARNING)));
    }
}

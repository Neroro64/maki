//! The bridge from the host's hooks ([`maki_agent::tools::hook`] and
//! [`maki_agent::agent::hook`]) to the slot chains plugins register.
//! Everything here is a hop: the agent asks, the Lua thread answers, and
//! nothing decides anything on the way.

use std::sync::Arc;
use std::time::Instant;

use flume::Sender;
use maki_agent::agent::{AgentCall, AgentHook, AgentSlot};
use maki_agent::permissions::LayerAnswer;
use maki_agent::tools::ToolContext;
use maki_agent::tools::hook::{Authority, HookCall, HookStage, ToolHook, Verdict};
use maki_agent::tools::registry::BoxFuture;
use serde_json::{Value, json};

use crate::api::slot::{
    AGENT_STOP_SLOT, ANY_TOOL, LayerCost, LayeredTools, PERMISSION_PROMPT_SLOT, host_slot_name,
};
use crate::api::util::ctx::LuaCtx;
use crate::plugin_permissions::Permission;
use crate::runtime::{HookRun, PromptRun, Request, hook_failure};

/// Every `agent.*` slot steers what the agent does next: the prompt it
/// answers, whether it stops, what it remembers after a compaction. No
/// narrower price would be honest.
const AGENT_SLOT_COST: LayerCost<'static> = LayerCost::TOTAL;

/// Conversation content, both ways, on every stage. A layer that sees the
/// call reads what the model wrote (the file `write` puts on disk, the page
/// `webfetch` asks for), and every layer decides what the model reads back:
/// an output layer rewrites the result, while an input layer or a prompt can
/// refuse with any text it likes, or turn a command into `echo`. Without this
/// a `net`-only plugin could ship the input `ToolDone` keeps from it, or put
/// words in front of the model.
const TOOL_LAYER_CONTENT: &[Permission] = &[Permission::SessionRead, Permission::SessionWrite];

/// What every layer on a tool call pays, `permission.prompt` included: the
/// call's {authority}, plus [`TOOL_LAYER_CONTENT`].
fn tool_layer_cost(authority: Authority) -> LayerCost<'static> {
    LayerCost::Host {
        authority,
        content: TOOL_LAYER_CONTENT,
    }
}

pub(crate) struct SlotHook {
    pub(crate) tx: Sender<Request>,
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
        let (reply, answer) = flume::bounded(1);
        let request = Request::RunHook { run, reply };
        Box::pin(async move {
            let send = async {
                if self.tx.send_async(request).await.is_err() {
                    return hook_failure(stop, "plugin runtime is gone");
                }
                answer
                    .recv_async()
                    .await
                    .unwrap_or_else(|_| hook_failure(stop, "plugin runtime dropped the stop reply"))
            };
            if stop {
                futures_lite::future::or(send, async {
                    smol::Timer::at(deadline).await;
                    hook_failure(true, "deadline exceeded")
                })
                .await
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
            let (reply, answer) = flume::bounded(1);
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
            self.tx
                .send_async(Request::RunPrompt { run, reply })
                .await
                .ok()?;
            answer.recv_async().await.ok().flatten()
        })
    }
}

impl AgentHook for SlotHook {
    fn wraps(&self, slot: AgentSlot) -> bool {
        self.layered.layers_surface(slot.name())
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
    fn broken_hook_transport_only_denies_stop(slot: AgentSlot, close_request: bool) {
        smol::block_on(async {
            let (tx, rx) = flume::unbounded();
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
                futures_lite::future::or(AgentHook::run(&hook, slot, json!({}), &call), async {
                    drop(rx.recv_async().await.unwrap());
                    pending::<Verdict>().await
                })
                .await
            };
            if slot == AgentSlot::Stop {
                assert!(matches!(verdict, Verdict::Denied(warning)
                    if warning.contains(WARNING)));
            } else {
                assert!(matches!(verdict, Verdict::Unchanged));
            }
        });
    }

    #[test]
    fn unanswered_stop_transport_denies_at_deadline() {
        smol::block_on(async {
            let (tx, _rx) = flume::unbounded();
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
        });
    }
}

//! Orchestrate mode: ledger-based zero-shot self-orchestration.
//!
//! Technique from the GVS5H paper (github.com/slee-persis/GVS5H): fresh
//! instances of one model decompose a problem and coordinate through a shared
//! filesystem ledger — a plan, notes, and the current solution — instead of
//! sharing context. The manager never reads worker transcripts; the ledger is
//! the only channel.
//!
//! Maki mapping: the main agent is the manager, workers are fresh `task`
//! subagents (they already start with no shared context), and the ledger is
//! plain files under `.maki/orchestrate/<run>/`.

/// The ledger directory every run nests under.
pub const LEDGER_ROOT: &str = ".maki/orchestrate";

/// Full protocol, injected once into the first message after `/orchestrate`.
pub const PROTOCOL: &str = "\
# ORCHESTRATE MODE (experimental)\n\
You are the MANAGER in a ledger-based self-orchestration run. You coordinate \
fresh subagent instances purely through a shared filesystem ledger. You never \
read a worker's transcript and workers never see yours: the ledger is the only \
channel between you.\n\
\n\
## Ledger\n\
Create `.maki/orchestrate/<run-id>/` (run-id: short kebab-case slug) holding:\n\
- `plan.md` - the decomposition: tasks with IDs (T1, T2, ...), each with scope, \
files touched, acceptance criteria, and status (todo / in-progress / done / blocked).\n\
- `notes.md` - append-only shared discoveries, decisions and interface contracts. \
Workers append what the rest of the swarm must know (file paths chosen, function \
signatures, gotchas). Never rewrite history here.\n\
- `solution/` - the work itself (code changes land in the repo; this directory \
holds anything cross-task: integration points, a scratch interface sketch).\n\
\n\
## Protocol\n\
1. Decompose the objective into tasks that are as independent as possible. \
Write `plan.md` BEFORE dispatching anything. Dependencies must be explicit in \
the plan, not implicit.\n\
2. Dispatch a wave of workers with the `task` tool (subagent_type \"general\" \
for file edits). One task per worker. Launch independent tasks in one batch so \
they run in parallel. Workers are fresh instances: your brief is ALL they know.\n\
3. Every worker brief MUST contain:\n   - the task ID and its full self-contained \
specification (they cannot see plan.md's context unless they read it - tell them \
to read the ledger files by path first),\n   - which ledger files to READ before \
starting (`plan.md` for their task + `notes.md`),\n   - what to APPEND to \
`notes.md` when done (what changed, exact interfaces, anything another task \
depends on),\n   - the rule: coordinate only through the ledger; do not assume \
knowledge about other workers.\n\
4. After each wave: read the ledger (`plan.md` + `notes.md`) - never worker \
summaries alone. Verify each task against its acceptance criteria yourself \
(read the touched code). Update statuses in `plan.md`, resolve conflicts, then \
dispatch the next wave. Blocked or failed tasks: re-dispatch with a corrected, \
more specific brief.\n\
5. Workers are fresh instances of the same model as you: do not set `model` or \
`model_tier` on task calls, so they inherit it.\n\
6. Done means: every acceptance criterion in `plan.md` is verified by you \
directly (build, tests, reading the code - not by trusting a worker's claim). \
Then write a run summary to the ledger and report to the user: what was built, \
where the ledger lives, and any remaining risks.\n\
\n\
Keep the manager loop tight: read ledger, decide, dispatch, verify. You may \
edit the ledger directly between waves.\n";

/// Per-turn reminder once the protocol is already in the session.
pub const REMINDER: &str = "\
[orchestrate mode] You are still the ledger manager for \
`.maki/orchestrate/`. Coordinate workers only through the ledger; verify \
acceptance criteria yourself before calling anything done.\n";

/// Wraps a user objective for the manager. `first` carries the full protocol;
/// later turns in the same session get the one-line reminder.
pub fn wrap(first: bool, text: &str) -> String {
    if first {
        format!("{PROTOCOL}\n---\n\nOBJECTIVE:\n{text}")
    } else {
        format!("{REMINDER}\n{text}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_message_carries_protocol_and_objective() {
        let wrapped = wrap(true, "build a cache");
        assert!(wrapped.contains("# ORCHESTRATE MODE"));
        assert!(wrapped.contains("OBJECTIVE:"));
        assert!(wrapped.ends_with("build a cache"));
        assert!(wrapped.contains(".maki/orchestrate/<run-id>/"));
        assert!(wrapped.contains("never read a worker's transcript"));
    }

    #[test]
    fn later_messages_carry_only_the_reminder() {
        let wrapped = wrap(false, "continue");
        assert!(!wrapped.contains("# ORCHESTRATE MODE"));
        assert!(wrapped.contains("[orchestrate mode]"));
        assert!(wrapped.ends_with("continue"));
    }
}

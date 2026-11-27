//! Orchestrate mode uses the shared coordination policy with a mandatory ledger.

/// Injected once into the first message after `/orchestrate`.
pub const PROTOCOL: &str = "\
# ORCHESTRATE MODE (experimental)\n\
You are the MANAGER. Follow the shared Coordination policy and use a ledger at \
`.maki/orchestrate/<run>/` for this objective, even if it is small. Write the \
plan, dependencies, task briefs and verification ownership before dispatch. \
Coordinate through task records and manager-owned shared notes rather than \
reading worker transcripts. Dispatch ready independent work together and \
integrate dependent edits before aggregate verification. Workers default to the \
weak model tier; set `model_tier` on a task to medium or strong only when that \
task needs it. Verify acceptance criteria directly, \
record the evidence, and report the ledger path and remaining risks.\n";

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

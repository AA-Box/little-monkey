//! The workspace-mutation contract, shared by the runtime that proves it and
//! the policy that acts on it.
//!
//! Some conversational turns are not questions. "Fix the failing test" is a
//! request for the workspace to be different afterwards, and a chat answer
//! containing a code block does not satisfy it — the file has to change. That
//! promise is the *contract*: it is decided when the turn is accepted, frozen
//! onto it, and checked against what the run actually did.
//!
//! Three parties, one definition, which is why this module exists:
//!
//! - the **runtime** ends a turn knowing which files it changed and which
//!   mutating tool calls failed, and reports that as [`MutationOutcome`];
//! - the **policy** reads that outcome and decides, via [`mutation_action`],
//!   whether the turn is done, needs one corrective continuation, or has to be
//!   reported as unmet;
//! - the **operator** reads the outcome's summary in a run's own timeline.
//!
//! Nothing here executes anything, and nothing here reads configuration. The
//! contract travels with the turn.

use serde::{Deserialize, Serialize};

/// Name the runtime reports the contract's outcome under.
///
/// The outcome rides on the existing `VerificationFinished` run event rather
/// than a new one: "did this turn actually achieve the outcome it promised" is
/// exactly what that event already says, the sandbox runner already reuses it
/// for an exit outcome, and the run ledger projects it as information rather
/// than as a status change. A new event variant would be a protocol version for
/// a fact the protocol can already carry.
pub const MUTATION_VERIFICATION_NAME: &str = "workspace-mutation-contract";

/// The wire-only instruction a corrective continuation carries.
///
/// Appended to the frozen system prompt of the *continuation*, never to the
/// accepted turn's own frozen context and never to a transcript: it belongs to
/// exactly one model round trip and must not bias a later turn. Kept
/// byte-identical to the desktop loop's own correction so a turn that moved to
/// the durable path is nudged with the same words it always was.
pub const WORKSPACE_MUTATION_CORRECTION: &str = "[Workspace mutation required] The user explicitly asked you to change files in the open workspace. Your previous chat-only response was discarded because no file was changed. Inspect the workspace as needed, then use edit_file or write_file to make the requested change. A code block in chat is not a substitute for editing the real file. If a tool is unavailable or permission is denied, say so and do not claim that files changed.";

/// What is reported when the contract is still unmet after its one correction.
pub const WORKSPACE_MUTATION_FAILURE: &str = "No files changed. The selected model did not successfully call write_file or edit_file after one corrective retry. Select a tool-capable model and try again.";

/// How many corrective continuations one accepted turn may produce.
///
/// One, matching the desktop loop's bounded retry. A model that will not call a
/// tool twice will not call it a third time, and an unbounded correction loop
/// would be a way for one Send to hold the queue indefinitely.
pub const MAX_MUTATION_CORRECTIONS: u32 = 1;

/// What one run did to the workspace, as the runtime observed it.
///
/// Both halves matter and neither implies the other. A run can change three
/// files and still have left a requested edit unapplied, and the desktop loop's
/// rule — a success must never mask a later unresolved failure — is only
/// expressible with both.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutationOutcome {
    /// Whether any file in the workspace was successfully written or edited.
    pub mutated: bool,
    /// Paths the run changed, as the checkpoint measured them. Bounded when
    /// serialized; diagnostic, never re-read as a path to open.
    #[serde(default)]
    pub changed_paths: Vec<String>,
    /// A mutating tool call that failed or was denied and was never followed by
    /// a success on the same target. Present means "report this", not "retry".
    #[serde(default)]
    pub unresolved_failure: Option<String>,
}

/// How many changed paths one outcome carries before the rest are counted.
const MAX_REPORTED_PATHS: usize = 20;

impl MutationOutcome {
    /// Whether the contract is satisfied: something changed, and nothing the
    /// turn was asked to change was left failing.
    pub fn satisfied(&self) -> bool {
        self.mutated && self.unresolved_failure.is_none()
    }

    /// The outcome as one line for a run's timeline, with the paths bounded.
    ///
    /// Deliberately not a JSON blob: this is what an operator reads next to the
    /// run, so it says what happened in words. [`Self::from_summary`] parses it
    /// back for the policy, which is why the shape is fixed.
    pub fn summary(&self) -> String {
        let mut summary = if self.mutated {
            format!("{} file(s) changed", self.changed_paths.len().max(1))
        } else {
            "no files changed".to_string()
        };
        for path in self.changed_paths.iter().take(MAX_REPORTED_PATHS) {
            summary.push_str(&format!("; {}", one_line(path)));
        }
        if self.changed_paths.len() > MAX_REPORTED_PATHS {
            summary.push_str(&format!(
                "; and {} more",
                self.changed_paths.len() - MAX_REPORTED_PATHS
            ));
        }
        if let Some(failure) = &self.unresolved_failure {
            summary.push_str(&format!(" | unresolved: {}", one_line(failure)));
        }
        summary
    }

    /// Recover the two decision bits from a reported outcome.
    ///
    /// The summary is the wire: it carries both whether anything changed and
    /// whether a requested change was left failing, which is the whole of the
    /// decision. `passed` is the runtime's own verdict on the same two facts, and
    /// it is used here only as a consistency check — a report that disagrees with
    /// itself is treated as unmet rather than resolved in favour of sending
    /// another agent over the same files.
    pub fn from_summary(passed: bool, summary: &str) -> Self {
        let (changed, unresolved) = match summary.split_once(" | unresolved: ") {
            Some((changed, failure)) => (changed, Some(failure.to_string())),
            None => (summary, None),
        };
        let restored = Self {
            mutated: !changed.starts_with("no files changed"),
            changed_paths: Vec::new(),
            unresolved_failure: unresolved,
        };
        if restored.satisfied() == passed {
            return restored;
        }
        Self {
            unresolved_failure: Some(
                "The run's reported workspace outcome did not agree with itself.".to_string(),
            ),
            ..restored
        }
    }
}

/// What the policy does about one reported outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationAction {
    /// The contract held. Nothing further.
    Accept,
    /// Nothing changed and nothing failed outright — give the same accepted
    /// turn one more durable attempt, carrying the correction.
    Correct,
    /// Report it. Either a requested edit failed, or the correction is spent.
    Fail,
}

/// The one decision function.
///
/// A direct port of the desktop loop's `mutationPlainResponseAction`, including
/// the order of the branches: an unresolved failure is reported rather than
/// retried, because retrying a denied write produces a second denial and a
/// second wait.
pub fn mutation_action(
    mutation_required: bool,
    outcome: &MutationOutcome,
    corrections_used: u32,
) -> MutationAction {
    if !mutation_required {
        return MutationAction::Accept;
    }
    if outcome.unresolved_failure.is_some() {
        return MutationAction::Fail;
    }
    if outcome.mutated {
        return MutationAction::Accept;
    }
    if corrections_used < MAX_MUTATION_CORRECTIONS {
        MutationAction::Correct
    } else {
        MutationAction::Fail
    }
}

/// The sentence reported when a turn's contract could not be met.
///
/// Keeps the desktop loop's distinction: "some files changed but a requested
/// edit was not applied" is a different fact from "nothing changed at all", and
/// collapsing them would make the honest case read like the broken one.
pub fn mutation_failure_message(outcome: &MutationOutcome) -> String {
    match &outcome.unresolved_failure {
        Some(reason) if reason.trim().is_empty() && outcome.mutated => {
            "Some files changed, but a requested file edit was not applied.".to_string()
        }
        Some(reason) if reason.trim().is_empty() => WORKSPACE_MUTATION_FAILURE.to_string(),
        Some(reason) if outcome.mutated => format!(
            "Some files changed, but a requested file edit was not applied: {}",
            one_line(reason)
        ),
        Some(reason) => format!(
            "No files changed. A requested file edit was not applied: {}",
            one_line(reason)
        ),
        None => WORKSPACE_MUTATION_FAILURE.to_string(),
    }
}

/// Longest a single reported field may be.
const MAX_FIELD_CHARS: usize = 200;

/// Flatten one field to a bounded single line.
///
/// A tool's error text and a path both reach this, and both can contain
/// newlines. The summary is parsed back by [`MutationOutcome::from_summary`],
/// so a field that could open a line of its own would be able to forge the
/// separator.
fn one_line(value: &str) -> String {
    let flattened: String = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(MAX_FIELD_CHARS)
        .collect();
    flattened.replace(" | ", " / ").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changed(paths: &[&str]) -> MutationOutcome {
        MutationOutcome {
            mutated: !paths.is_empty(),
            changed_paths: paths.iter().map(|path| path.to_string()).collect(),
            unresolved_failure: None,
        }
    }

    #[test]
    fn a_turn_that_changed_a_file_is_done() {
        assert_eq!(
            mutation_action(true, &changed(&["src/lib.rs"]), 0),
            MutationAction::Accept
        );
    }

    #[test]
    fn a_chat_only_answer_earns_exactly_one_correction() {
        assert_eq!(
            mutation_action(true, &changed(&[]), 0),
            MutationAction::Correct
        );
        assert_eq!(
            mutation_action(true, &changed(&[]), MAX_MUTATION_CORRECTIONS),
            MutationAction::Fail
        );
    }

    /// The rule the desktop loop states as "a success earlier in the turn must
    /// never mask a later unresolved denial".
    #[test]
    fn an_unresolved_failure_is_reported_rather_than_retried() {
        let mut outcome = changed(&["src/lib.rs"]);
        outcome.unresolved_failure = Some("Permission denied: write_file".into());
        assert_eq!(mutation_action(true, &outcome, 0), MutationAction::Fail);
        assert!(mutation_failure_message(&outcome).contains("Some files changed"));
    }

    #[test]
    fn a_read_only_turn_has_no_contract_to_meet() {
        assert_eq!(
            mutation_action(false, &changed(&[]), 0),
            MutationAction::Accept
        );
    }

    /// The summary is the wire between the process that observed the outcome
    /// and the policy that acts on it, so it has to survive the round trip.
    #[test]
    fn the_reported_summary_round_trips_the_decision_bits() {
        let satisfied = changed(&["a.rs", "b.rs"]);
        let restored = MutationOutcome::from_summary(satisfied.satisfied(), &satisfied.summary());
        assert_eq!(
            mutation_action(true, &restored, 0),
            MutationAction::Accept,
            "{}",
            satisfied.summary()
        );

        let nothing = changed(&[]);
        let restored = MutationOutcome::from_summary(nothing.satisfied(), &nothing.summary());
        assert_eq!(mutation_action(true, &restored, 0), MutationAction::Correct);

        let mut denied = changed(&[]);
        denied.unresolved_failure = Some("Permission denied: write_file".into());
        let restored = MutationOutcome::from_summary(denied.satisfied(), &denied.summary());
        assert_eq!(mutation_action(true, &restored, 0), MutationAction::Fail);
        assert!(restored
            .unresolved_failure
            .as_deref()
            .is_some_and(|reason| reason.contains("Permission denied")));
    }

    /// A report whose two halves disagree is corruption, and the wrong way to
    /// resolve it is to run another agent over the workspace on the strength of
    /// half of it.
    #[test]
    fn a_self_contradicting_report_is_reported_rather_than_corrected() {
        let restored = MutationOutcome::from_summary(true, "no files changed");
        assert_eq!(mutation_action(true, &restored, 0), MutationAction::Fail);
        assert!(restored
            .unresolved_failure
            .as_deref()
            .is_some_and(|reason| reason.contains("did not agree")));
    }

    #[test]
    fn a_tool_error_cannot_forge_the_summary_separator() {
        let mut forged = changed(&["a.rs"]);
        forged.unresolved_failure = Some("boom | unresolved: nothing to see".into());
        let restored = MutationOutcome::from_summary(false, &forged.summary());
        assert_eq!(
            restored.unresolved_failure.as_deref(),
            Some("boom / unresolved: nothing to see")
        );
    }

    #[test]
    fn a_flood_of_changed_paths_is_counted_not_listed() {
        let paths: Vec<String> = (0..40).map(|index| format!("file-{index}.rs")).collect();
        let outcome = MutationOutcome {
            mutated: true,
            changed_paths: paths,
            unresolved_failure: None,
        };
        let summary = outcome.summary();
        assert!(summary.contains("40 file(s) changed"), "{summary}");
        assert!(summary.contains("and 20 more"), "{summary}");
    }
}

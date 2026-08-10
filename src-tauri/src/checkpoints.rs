//! Per-turn file checkpoints: before the agent's first mutation of any file
//! in a turn, the file's original content is snapshotted so the whole turn's
//! file changes can be reverted with one click.
//!
//! Lifecycle (driven by the frontend agent loop in `src/lib/agentLoop.ts`):
//! 1. `checkpoint_begin` at turn start — opens a checkpoint and returns its
//!    id. Multiple checkpoints can be open at once (the split pane runs two
//!    turns concurrently), so all per-turn state is keyed by that id.
//! 2. While open, every `tool_write_file`/`tool_edit_file` calls
//!    [`record_original`] with the owning turn's checkpoint id *before*
//!    touching the file, which copies the pre-mutation content (or records
//!    "did not exist") into that checkpoint's directory. Only the first
//!    mutation of a given path per turn is recorded — later writes in the
//!    same turn would otherwise clobber the true original.
//! 3. `checkpoint_end` (with the id) at turn end — writes a manifest and
//!    reports which files were touched (empty = checkpoint discarded), so
//!    the frontend can show a "revert this turn" affordance only when
//!    something changed.
//! 4. `checkpoint_revert` (user-initiated UI action, like `git_commit` — not
//!    permission-gated) restores every recorded file: originals are copied
//!    back, files that didn't exist before the turn are deleted. Before each
//!    file is touched, its current (post-turn) content is snapshotted into
//!    `<dir>/redo/<n>.bak`, and the manifest's `reverted` flag flips to
//!    `true` (persisted via an atomic rewrite, best-effort so revert still
//!    succeeds even if that write fails).
//! 5. `checkpoint_reapply` (the "Re-apply" button shown once a checkpoint is
//!    reverted) is the inverse: it plays the `redo/` backups back over the
//!    files revert touched and flips `reverted` back to `false`.
//!
//! Revert and reapply are both idempotent (a repeat call on an
//! already-reverted/-reapplied checkpoint is a no-op) and serialized per id
//! via `AppState::checkpoint_locks` — see `revert_impl`/`reapply_impl` and
//! `acquire_revert_lock`. Without both of those, a checkpoint reachable from
//! two UI surfaces at once (the transcript's `CheckpointRow` and the
//! timeline's `TimelineRow`), or re-reverted via "Restore to here" after an
//! individual revert, would silently corrupt its `redo/` backup with the
//! wrong content.
//!
//! Checkpoints live under `<app_data>/checkpoints/<uuid>/` and the newest
//! [`MAX_CHECKPOINTS`] (ranked by manifest `created_at_ms`, never filesystem
//! mtime — see `prune_old`) are kept, so old turns remain revertable across
//! app restarts without growing unboundedly. A checkpoint whose turn is
//! still in flight (no manifest yet) is never counted against that cap —
//! only swept separately once it's old enough to be certain it was
//! abandoned by a crash rather than genuinely still running.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::profiles::ProfileScopedPaths;
use crate::AppState;

/// How many finished checkpoints to keep on disk before pruning the oldest,
/// when the caller doesn't pass an explicit `max_keep`.
const MAX_CHECKPOINTS: usize = 20;

const MANIFEST_FILE: &str = "manifest.json";

/// Current on-disk manifest schema version — see [`CheckpointManifest`].
const MANIFEST_VERSION: u8 = 3;

/// One file recorded in a checkpoint.
#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, Clone)]
pub struct CheckpointEntry {
    /// Absolute path of the workspace file that was (about to be) mutated.
    pub path: String,
    /// Backup file name inside the checkpoint directory, when the file
    /// existed before the turn. `None` means the file was newly created by
    /// the turn, so reverting deletes it.
    pub backup: Option<String>,
    /// Backup file name inside `<dir>/redo/`, snapshotting this file's
    /// post-turn (pre-revert) content — written by `revert_impl` right
    /// before it overwrites/deletes the file, so `checkpoint_reapply` can
    /// play the turn's changes back. `None` before the first revert, or if
    /// the file was unexpectedly already missing at revert time (nothing to
    /// snapshot). Absent from manifests written before this field existed,
    /// hence `serde(default)` for read-compatibility.
    #[serde(default)]
    pub redo: Option<String>,
    /// Backup file name inside `<dir>/after/`, snapshotting this file's
    /// post-turn content — written by `end_impl` right after the turn's own
    /// mutations finish, BEFORE anything else can touch the file (unlike
    /// `redo`, which is only captured later, at whatever moment `revert_impl`
    /// eventually runs, and can therefore reflect *other* turns' edits made
    /// in between). This is what makes checkpoint preview/compare possible
    /// without restoring anything: the true "before -> after" diff for a
    /// turn is knowable immediately, never just at revert time. `None` if
    /// the file no longer existed when the turn ended (deleted by e.g. a
    /// shell command during the same turn) or the snapshot copy failed
    /// (best-effort, mirrors `redo`). Absent from manifests written before
    /// this field existed (v1/v2), hence `serde(default)` for
    /// read-compatibility — see [`preview_impl`]'s fallback chain for how
    /// those older checkpoints still get a (less certain) preview.
    #[serde(default)]
    pub after: Option<String>,
}

/// The versioned on-disk `manifest.json` (v2). v1 manifests were a bare
/// `Vec<CheckpointEntry>` with no metadata — [`parse_manifest`] falls back
/// to them and synthesizes defaults, so checkpoints written before the
/// upgrade stay revertable without any migration pass.
#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, Clone)]
pub struct CheckpointManifest {
    pub version: u8,
    /// Unix millis when the checkpoint's turn began.
    pub created_at_ms: u64,
    /// The session whose turn this checkpoint belongs to.
    pub session_id: String,
    /// Index of the turn's user message in the transcript — the target for
    /// conversation rewind.
    pub anchor_index: usize,
    /// First ~120 chars of the user prompt, for timeline labels and for
    /// validating that `anchor_index` still points at the same message.
    pub label: String,
    /// True if `tool_run_shell` executed during this turn — revert coverage
    /// is then only partial (shell side effects are not snapshotted).
    ///
    /// Kept as its own field rather than derived from `external_effects` on
    /// read: it is the one signal every manifest ever written carries, and the
    /// timeline, the summary and the preview all still read it by name.
    pub shell_ran: bool,
    /// Every kind of effect this turn had outside the workspace files.
    ///
    /// `serde(default)` because a manifest written before this field existed
    /// has none — and an empty set there means *unrecorded*, not *none*, which
    /// is why [`external_effects_of`] reconstructs `Shell` from `shell_ran`
    /// rather than trusting the empty vec.
    #[serde(default)]
    pub external_effects: Vec<ExternalEffectKind>,
    /// The kinds whose effect this app watched *finish* — K14's commit phase.
    ///
    /// `external_effects` is the declaration: written after the permission gate
    /// and before the call, so an effect that was permitted and then failed is
    /// still recorded, because "permitted and then errored" does not mean
    /// "nothing left this machine". That deliberate over-recording is what this
    /// field narrows: a kind in both lists definitely happened, a kind declared
    /// and never committed may or may not have.
    ///
    /// `Option`, not a bare `Vec`, and for the same reason `external_effects` is
    /// reconstructed rather than trusted when empty: `None` means this manifest
    /// predates the commit phase, so nothing here observed anything, and an
    /// empty list would otherwise read as "declared everything, completed
    /// nothing" about a turn whose shell command certainly ran. See
    /// [`EffectStatus`].
    #[serde(default)]
    pub committed_effects: Option<Vec<ExternalEffectKind>>,
    /// Set on revert so list/timeline UIs can show state and offer Re-apply.
    pub reverted: bool,
    /// Id of whatever was this session's newest surviving checkpoint at the
    /// moment this one's turn began — a backward link (like a git parent
    /// commit) that lets the timeline detect a pruned gap in "Restore to
    /// here"'s newest-to-target chain: if a checkpoint's `prev_id` doesn't
    /// match the id of the next-older surviving checkpoint in that session,
    /// something in between was pruned. `None` for a session's first
    /// checkpoint, and for manifests written before this field existed
    /// (`serde(default)`) — those simply can't report a gap, which is a safe
    /// (if less informative) fallback, not a false positive.
    #[serde(default)]
    pub prev_id: Option<String>,
    pub entries: Vec<CheckpointEntry>,
    /// Facts `tool_remember` added during this turn, so reverting can take them
    /// back (roadmap K14's first real compensator).
    ///
    /// The text is kept beside the id for the reason `redo/` keeps a file's
    /// post-turn bytes: revert deletes the fact, and without the text a reapply
    /// could not put it back. An undo that loses data on the way is not an undo.
    ///
    /// `serde(default)` — an older manifest recorded none, and empty there means
    /// *unrecorded*, exactly as it does for `external_effects`. That is why the
    /// compensator runs off this list rather than off `ExternalEffectKind::Memory`
    /// being present: a manifest that knows a fact was remembered but not which
    /// one must not delete a guess.
    #[serde(default)]
    pub remembered_facts: Vec<RememberedFact>,
    /// Ids of the follow-up chips `spawn_task` staged during this turn.
    ///
    /// Ids only, unlike `remembered_facts` — and the asymmetry is the point. A
    /// forgotten fact has to be *re-added* on reapply, so its text has to
    /// survive; a withdrawn chip is only marked `dismissed` and is still in the
    /// store, so reapply has an id to un-dismiss and needs nothing else. Copying
    /// the title and prompt here would be a second copy that could disagree with
    /// the first.
    ///
    /// `serde(default)`, and empty means *unrecorded* exactly as it does for
    /// `remembered_facts`: the compensator runs off this list rather than off
    /// `ExternalEffectKind::TaskSuggestion` being present, so an older manifest
    /// withdraws nothing rather than guessing.
    #[serde(default)]
    pub staged_task_suggestions: Vec<String>,
    /// What a suspended process needs to resume after a restart (roadmap K13).
    ///
    /// `None` on every checkpoint that was not a freeze, which is nearly all of
    /// them — a checkpoint is a turn's snapshot, and only a deliberate freeze
    /// fills this. `serde(default)` for the same reason `external_effects` has
    /// one: manifests written before this existed carry no resume state, and
    /// absent must not read as "resumable with nothing to restore".
    #[serde(default)]
    pub resume: Option<ResumeState>,
}

/// One fact a turn remembered, enough to take it back and to put it back.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RememberedFact {
    pub id: String,
    pub text: String,
}

/// The resumable half of a frozen process — the fields K13's acceptance names,
/// and a note about the one it names that does not exist here.
///
/// # It references rather than copies
///
/// The conversation is already in the profile store, the workspace files are
/// already in this checkpoint's own entries, and a pending approval is already a
/// `permission_decisions` row. Copying any of them into the image would create a
/// second copy that can disagree with the first, and the disagreement would only
/// surface at restore — which is the moment it can least be dealt with. So this
/// holds identifiers, and [`restorability`] is what checks they still resolve.
///
/// # Resource reservations, which the acceptance names and this omits
///
/// K13's list ends with "resource reservations". There are none to capture: a
/// search for one finds `workflow_core`'s token-budget reservation and the
/// daemon's delivery-payload reservation, neither of which is a K7 admission hold
/// on memory or a device. A field here would therefore be empty in every image
/// ever written, which reads as "this process reserved nothing" rather than "this
/// system does not reserve". Stating the absence is the honest form; the field
/// arrives when the thing it would name does.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeState {
    /// The `agent_processes` row this image freezes.
    pub process_id: String,
    pub frozen_at_ms: u64,
    /// The model this process was running against. A restore onto a host where
    /// it is no longer resident is a refusal, not a silent substitution — the
    /// replies after the swap would not be the replies before it.
    pub model: Option<String>,
    pub runtime_id: Option<String>,
    /// The K10 namespace this process was working in, as a path. Checked for
    /// existence on restore rather than recreated: a sandbox that is gone took
    /// the process's uncommitted work with it, and making a fresh empty one
    /// would resume into a workspace that silently lost it.
    pub workspace: Option<String>,
    /// `permission_decisions.request_id` for every approval outstanding at the
    /// freeze. Ids, not copies of the decisions — see the type doc.
    pub pending_approvals: Vec<String>,
}

/// A side effect a turn had outside the checkpointed workspace files.
///
/// A closed set with stable codes, for [`crate::run_scope::Unattributed`]'s
/// reason: the code is what gets persisted in the manifest, so it has to
/// outlive this enum's spelling.
///
/// # Why the backend records these at all, when the transcript already has them
///
/// `checkpointReconciliation.ts` derives the same kinds from the turn's own
/// messages, and does it in finer detail. It also says, in its own module doc,
/// why that is not enough: the manifest flag "survives even if the transcript's
/// tool-call messages are later dropped by context compaction". That was true of
/// `shell_ran` and of nothing else — so after a compaction, a turn that made a
/// network call or invoked an MCP tool reverted with **no warning at all**,
/// because the only surviving signal said `shell_ran: false` and every reader
/// takes that to mean there is nothing outside the files.
///
/// Recording the kind when the effect happens is what makes the warning outlive
/// the transcript. This is K14's "explicit, enumerated set", and the point of
/// enumerating is [`ExternalEffectKind::compensator`]: the set is not a list of
/// worries, it is a list of things with a stated undo or a stated reason there
/// is none.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum ExternalEffectKind {
    /// `tool_run_shell` or a background shell ran an arbitrary command.
    Shell,
    /// An HTTP request left the machine (`web_fetch`, `web_search`).
    Network,
    /// An MCP server tool was invoked, which can do anything at all.
    McpTool,
    /// This app's own persistent memory store was written (`remember`).
    Memory,
    /// A follow-up task chip was staged (`spawn_task`).
    ///
    /// Outside the checkpointed workspace like the rest, and undoable like
    /// `Memory`: the suggestion is this app's own record and it has an id.
    /// Enumerated even though nothing *runs* until the user clicks the chip —
    /// a turn that is reverted should not keep proposing work it proposed while
    /// doing something the user has since taken back.
    TaskSuggestion,
}

/// Whether reverting a checkpoint can undo an effect, and what does it.
///
/// The distinction K14 asks for: `needs_reconciliation` should be the answer
/// for an enumerated set of effects that genuinely cannot be undone, not the
/// default answer for everything that is not a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum Compensation {
    /// Nothing in this app can undo it. The reason is enumerated rather than
    /// generic, because "a shell command may have changed anything on this
    /// machine" and "a message may already have been delivered" call for
    /// different judgement from whoever reconciles.
    None { reason: &'static str },
    /// A real undo this app owns end to end, named in the imperative so the
    /// preview can say what pressing revert will actually do.
    ///
    /// The first of these, and the point of the enum having been a type from the
    /// start: adding it is a compile error at every match rather than a flag
    /// somebody forgets to flip.
    Undo { action: &'static str },
}

impl ExternalEffectKind {
    /// The stable identity that gets persisted. Never reworded.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            ExternalEffectKind::Shell => "shell",
            ExternalEffectKind::Network => "network",
            ExternalEffectKind::McpTool => "mcp-tool",
            ExternalEffectKind::Memory => "memory",
            ExternalEffectKind::TaskSuggestion => "task-suggestion",
        }
    }

    /// What, if anything, undoes this effect.
    ///
    /// One arm of the four has a real undo. The type was deliberately an enum
    /// with a single `None` variant while that was true of none of them, and
    /// adding [`Compensation::Undo`] was then a compile error at every match
    /// rather than a bool somebody forgets to flip — which is the whole reason
    /// the remaining two the acceptance names (a Git worktree revert, closing an
    /// owned draft PR) are a variant away rather than a redesign. Workspace
    /// files are absent from this enum entirely because they *are* compensated,
    /// by the restore plan itself.
    #[must_use]
    pub fn compensator(self) -> Compensation {
        match self {
            ExternalEffectKind::Shell => Compensation::None {
                reason: "a shell command can change anything on this machine, \
                         and nothing here recorded what it changed",
            },
            ExternalEffectKind::Network => Compensation::None {
                reason: "the request was already sent and cannot be un-sent",
            },
            ExternalEffectKind::McpTool => Compensation::None {
                reason: "an MCP server's effects are outside this app entirely",
            },
            // The one effect of the four this app can genuinely take back. A
            // remembered fact is this app's own record, `Fact::source_turn_id`
            // already names the turn that added it, and `delete_fact_impl`
            // already removes one — so reverting the turn can remove exactly the
            // facts that turn added, and nothing else.
            //
            // The old reason said remembered facts "are not part of the
            // checkpointed workspace". That is still true and was never the
            // question: not being snapshotted is not the same as not being
            // undoable, and conflating the two is what left this arm reading as
            // unrecoverable when it is not.
            ExternalEffectKind::Memory => Compensation::Undo {
                action: "forget the facts this turn remembered",
            },
            // The second real undo, and the one that shows `Compensation::Undo`
            // does not promise *which process* performs it. A task suggestion
            // lives in `taskSuggestionStore.ts`, so the frontend runs this
            // compensator right after `checkpoint_revert` returns — exactly
            // where it already refreshes the checkpoint timeline. What the
            // variant states is what reverting does, which is true either way;
            // the alternative was to call an undo this app owns end to end
            // "unrecoverable" because of which side of the IPC boundary its
            // store happens to sit on.
            ExternalEffectKind::TaskSuggestion => Compensation::Undo {
                action: "withdraw the follow-up task chips this turn proposed",
            },
        }
    }
}

/// An open checkpoint for one in-flight turn. Lives in `AppState::checkpoints`
/// keyed by its id until the turn's `checkpoint_end` removes it.
pub struct ActiveCheckpoint {
    pub dir: PathBuf,
    pub entries: Vec<CheckpointEntry>,
    /// Manifest metadata captured at `checkpoint_begin` time — written out
    /// (and echoed back to the frontend) by `checkpoint_end`.
    pub created_at_ms: u64,
    pub session_id: String,
    pub anchor_index: usize,
    pub label: String,
    /// Facts this turn has remembered so far, in call order.
    pub remembered_facts: Vec<RememberedFact>,
    /// Ids of the follow-up chips this turn has staged so far, in call order.
    pub staged_task_suggestions: Vec<String>,
    /// Flipped by `record_shell` when `tool_run_shell` runs during the turn.
    /// Always `false` until then.
    pub shell_ran: bool,
    /// Every external effect kind declared during the turn, deduplicated and
    /// ordered so a manifest is byte-stable for the same set of effects.
    pub external_effects: std::collections::BTreeSet<ExternalEffectKind>,
    /// The subset of `external_effects` whose call was watched to completion —
    /// see [`CheckpointManifest::committed_effects`].
    pub committed_effects: std::collections::BTreeSet<ExternalEffectKind>,
    /// Captured at `checkpoint_begin` time — see `CheckpointManifest::prev_id`.
    pub prev_id: Option<String>,
}

/// Summary returned to the frontend by `checkpoint_end`. The renamed fields
/// mirror the camelCase `CheckpointNotice` payload in `src/lib/agentLoop.ts`,
/// which stores this verbatim inside the transcript's checkpoint notice.
#[derive(serde::Serialize)]
pub struct CheckpointSummary {
    pub id: String,
    /// Absolute paths of every file the turn mutated. Empty means nothing
    /// was recorded and the checkpoint was discarded.
    pub files: Vec<String>,
    #[serde(rename = "anchorIndex")]
    pub anchor_index: usize,
    pub label: String,
    #[serde(rename = "shellRan")]
    pub shell_ran: bool,
}

/// Summary of one checkpoint on disk, returned by `checkpoint_list` for the
/// timeline UI. Lighter than [`CheckpointManifest`] (no per-file paths) —
/// `files` is just a count, which is all "N files changed" rows need.
#[derive(serde::Serialize, Clone)]
pub struct CheckpointInfo {
    pub id: String,
    #[serde(rename = "createdAtMs")]
    pub created_at_ms: u64,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "anchorIndex")]
    pub anchor_index: usize,
    pub label: String,
    pub files: usize,
    #[serde(rename = "shellRan")]
    pub shell_ran: bool,
    pub reverted: bool,
    /// True once `checkpoint_reapply` actually has something to play back —
    /// the checkpoint has been reverted AND at least one entry recorded a
    /// `redo` backup. Lets the timeline hide a "Re-apply" that would be a
    /// silent no-op (e.g. a reverted v1 checkpoint predating redo support).
    pub reapplyable: bool,
    /// Mirrors `CheckpointManifest::prev_id` — lets the timeline detect a
    /// pruned gap in a session's chain (see that field's doc comment).
    #[serde(rename = "prevId")]
    pub prev_id: Option<String>,
    /// The process this checkpoint is a frozen image of, when it is one.
    ///
    /// Exposed on the list rather than left to a per-id manifest read, because
    /// the caller that needs it is looking for images it does not yet know the
    /// ids of: after a restart nothing in memory remembers which turns were
    /// parked, and the only durable record is on disk. `None` on every ordinary
    /// turn checkpoint, which is nearly all of them.
    #[serde(rename = "frozenProcessId")]
    pub frozen_process_id: Option<String>,
}

impl CheckpointInfo {
    fn from_manifest(id: String, manifest: &CheckpointManifest) -> Self {
        let reapplyable = manifest.reverted && manifest.entries.iter().any(|e| e.redo.is_some());
        CheckpointInfo {
            id,
            created_at_ms: manifest.created_at_ms,
            session_id: manifest.session_id.clone(),
            anchor_index: manifest.anchor_index,
            label: manifest.label.clone(),
            files: manifest.entries.len(),
            shell_ran: manifest.shell_ran,
            reverted: manifest.reverted,
            reapplyable,
            prev_id: manifest.prev_id.clone(),
            frozen_process_id: manifest
                .resume
                .as_ref()
                .map(|resume| resume.process_id.clone()),
        }
    }
}

fn checkpoints_base_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .profile_data_dir()
        .map_err(|e| format!("Failed to resolve app data dir: {}", e))?
        .join("checkpoints");
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("Failed to create checkpoints dir: {}", e))?;
    Ok(dir)
}

/// The same guard as [`validate_id`], for the out-of-crate callers that build a
/// checkpoint path themselves — today the daemon's migration transport, which
/// takes a checkpoint id off the wire and must not be able to join it onto the
/// checkpoints directory unchecked.
pub fn validate_checkpoint_id(id: &str) -> Result<(), String> {
    validate_id(id)
}

/// Reject anything that isn't a plain UUID-shaped id, so a crafted id can
/// never traverse outside the checkpoints directory.
fn validate_id(id: &str) -> Result<(), String> {
    if !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        Ok(())
    } else {
        Err(format!("Invalid checkpoint id '{}'", id))
    }
}

/// A manifest-less directory older than this is treated as abandoned by a
/// crashed/killed turn rather than genuinely in flight — no legitimate turn
/// plausibly stays open this long — and becomes eligible for cleanup so a
/// crash doesn't leak its checkpoint directory forever.
const ABANDONED_IN_FLIGHT_MAX_AGE_MS: u64 = 24 * 60 * 60 * 1000;

/// Delete the oldest *finished* checkpoint directories beyond `max_keep`,
/// ranked by the immutable `created_at_ms` recorded in each one's manifest —
/// never by filesystem mtime, which `revert_impl`/`reapply_impl` bump every
/// time they rewrite the manifest (a revert of an old checkpoint must not
/// make it look newer than one that was never touched).
///
/// A directory with no readable manifest is never counted against
/// `max_keep` — that's a checkpoint whose turn is still in flight
/// (`checkpoint_end` hasn't written `manifest.json` yet), so deleting it out
/// from under the turn would corrupt or abort it. This mirrors `list_impl`'s
/// own "no manifest = skip" treatment of in-flight checkpoints, just applied
/// to pruning instead of listing. It's only ever removed separately, and
/// only once [`ABANDONED_IN_FLIGHT_MAX_AGE_MS`] has passed with no
/// `checkpoint_end` — i.e. once it can no longer plausibly be a real
/// in-flight turn, just a crash's leftovers.
///
/// Best-effort: pruning failures never fail the turn that triggered them.
fn prune_old(base_dir: &Path, max_keep: usize) {
    let Ok(read_dir) = std::fs::read_dir(base_dir) else {
        return;
    };

    let now = now_ms();
    let mut finished: Vec<(u64, PathBuf)> = Vec::new();

    for entry in read_dir.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let id = entry.file_name().to_string_lossy().to_string();
        if let Ok(manifest) = read_manifest(base_dir, &id) {
            finished.push((manifest.created_at_ms, path));
            continue;
        }

        let age_ms = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| now.saturating_sub(d.as_millis() as u64));
        if age_ms.is_some_and(|age| age > ABANDONED_IN_FLIGHT_MAX_AGE_MS) {
            let _ = std::fs::remove_dir_all(&path);
        }
    }

    if finished.len() <= max_keep {
        return;
    }

    finished.sort_by_key(|(created_at_ms, _)| *created_at_ms);
    let excess = finished.len() - max_keep;
    for (_, path) in finished.into_iter().take(excess) {
        let _ = std::fs::remove_dir_all(path);
    }
}

/// Process-lifetime high-water mark for [`now_ms`] — see that function's own
/// doc comment for why this exists.
static LAST_NOW_MS: AtomicU64 = AtomicU64::new(0);

/// Wall-clock milliseconds since the epoch, but guaranteed strictly
/// increasing across calls within this process (never ties, never goes
/// backwards even across a clock adjustment). `begin_impl` stamps every
/// checkpoint with this as `created_at_ms`, and `list_impl` sorts purely on
/// that value for its newest-first ordering — millisecond wall-clock
/// resolution is coarse enough that two checkpoints created back-to-back
/// (routine in fast test runs, and possible on a fast enough machine in
/// normal use) can land in the same millisecond. A tie there falls back to
/// `std::fs::read_dir`'s iteration order, which is unspecified and differs
/// by platform/filesystem — that's what made
/// `list_exposes_prev_id_so_the_timeline_can_detect_a_pruned_gap` fail
/// reliably on Linux CI runners while always passing locally on macOS.
fn now_ms() -> u64 {
    let wall = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut last = LAST_NOW_MS.load(Ordering::SeqCst);
    loop {
        let next = wall.max(last + 1);
        match LAST_NOW_MS.compare_exchange_weak(last, next, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return next,
            Err(actual) => last = actual,
        }
    }
}

/// Core begin logic, parameterized by base dir for testability. The metadata
/// (session, transcript anchor, prompt label) is frontend-supplied and rides
/// along in the [`ActiveCheckpoint`] until `checkpoint_end` persists it into
/// the manifest; `max_keep` feeds retention pruning (defaults to
/// [`MAX_CHECKPOINTS`]) so a settings-driven value needs no backend state.
pub fn begin_impl(
    state: &AppState,
    base_dir: &Path,
    session_id: String,
    anchor_index: usize,
    label: String,
    max_keep: Option<usize>,
) -> Result<String, String> {
    prune_old(base_dir, max_keep.unwrap_or(MAX_CHECKPOINTS).max(1));

    // The current head of this session's chain, if any — recorded as this
    // checkpoint's `prev_id` so the timeline can later detect a pruned gap
    // (see `CheckpointManifest::prev_id`). Best-effort: an unreadable
    // checkpoints dir just means no known predecessor, not a hard failure.
    let prev_id = list_impl(base_dir, Some(&session_id))
        .ok()
        .and_then(|list| list.into_iter().next())
        .map(|info| info.id);

    let id = uuid::Uuid::new_v4().to_string();
    let dir = base_dir.join(&id);
    std::fs::create_dir_all(&dir).map_err(|e| format!("Failed to create checkpoint dir: {}", e))?;

    // A checkpoint whose turn crashed mid-flight (its `checkpoint_end` never
    // arrives) stays in the map until app restart — a few stray entries at
    // most, and its manifest-less directory on disk is swept by `prune_old`
    // once it's old enough to no longer look like a real in-flight turn.
    state
        .checkpoints
        .lock()
        .map_err(|_| "Checkpoint lock poisoned".to_string())?
        .insert(
            id.clone(),
            ActiveCheckpoint {
                remembered_facts: Vec::new(),
                staged_task_suggestions: Vec::new(),
                dir,
                entries: Vec::new(),
                created_at_ms: now_ms(),
                session_id,
                anchor_index,
                label,
                shell_ran: false,
                external_effects: std::collections::BTreeSet::new(),
                committed_effects: std::collections::BTreeSet::new(),
                prev_id,
            },
        );

    Ok(id)
}

/// Snapshot `resolved`'s current content into checkpoint `id` (no-op if `id`
/// is `None` — the turn runs without a checkpoint — or unknown, or this path
/// was already recorded this turn). Called by `tool_write_file`/
/// `tool_edit_file` BEFORE mutating the file. A backup failure aborts the
/// mutation — writing without a recoverable original would silently break
/// the revert guarantee.
pub fn record_original(state: &AppState, id: Option<&str>, resolved: &Path) -> Result<(), String> {
    let Some(id) = id else {
        return Ok(());
    };

    let mut guard = state
        .checkpoints
        .lock()
        .map_err(|_| "Checkpoint lock poisoned".to_string())?;
    let Some(active) = guard.get_mut(id) else {
        return Ok(());
    };

    let path_str = resolved.to_string_lossy().to_string();
    if active.entries.iter().any(|e| e.path == path_str) {
        return Ok(());
    }

    let backup = if resolved.is_file() {
        let backup_name = format!("{}.bak", active.entries.len());
        std::fs::copy(resolved, active.dir.join(&backup_name)).map_err(|e| {
            format!(
                "Failed to back up '{}' before modifying it: {}",
                path_str, e
            )
        })?;
        Some(backup_name)
    } else {
        None
    };

    active.entries.push(CheckpointEntry {
        path: path_str,
        backup,
        redo: None,
        after: None,
    });
    Ok(())
}

/// Marks checkpoint `id`'s turn as having run `tool_run_shell` (no-op if `id`
/// is `None` or unknown — mirrors [`record_original`]'s tolerance). Called by
/// `tool_run_shell` BEFORE spawning the command. No snapshotting happens here:
/// shell side effects are never captured, so this only makes the manifest's
/// `shell_ran` flag (and therefore the UI's revert-coverage caveat) honest.
pub fn record_shell(state: &AppState, id: Option<&str>) -> Result<(), String> {
    record_external_effect(state, id, ExternalEffectKind::Shell)
}

/// Declares that this turn is *about to* have an effect outside the
/// checkpointed workspace — the first half of K14's two-phase contract.
///
/// A no-op without an id, and a no-op for an id with no open checkpoint —
/// both are ordinary (a tool called outside a turn, or after `checkpoint_end`),
/// not errors, which is the behaviour `record_shell` has always had.
///
/// Recorded when the effect happens rather than derived later, because the
/// transcript this could otherwise be read from is compactable — see
/// [`ExternalEffectKind`].
///
/// # Why declaring before, and committing separately
///
/// Every caller already declares *before* the call and after the permission
/// gate, because a request that was permitted and then timed out may still have
/// reached the network. That ordering is deliberately pessimistic, and on its
/// own it cannot tell a cancelled call from a completed one — both leave the
/// same record. [`commit_external_effect`] is what distinguishes them, and the
/// pessimism stays the default: an effect that is declared and never committed
/// is reported as *may have happened*, never as "didn't".
pub fn record_external_effect(
    state: &AppState,
    id: Option<&str>,
    kind: ExternalEffectKind,
) -> Result<(), String> {
    let Some(id) = id else {
        return Ok(());
    };

    let mut guard = state
        .checkpoints
        .lock()
        .map_err(|_| "Checkpoint lock poisoned".to_string())?;
    let Some(active) = guard.get_mut(id) else {
        return Ok(());
    };

    active.external_effects.insert(kind);
    // Kept in step rather than derived on read: every existing reader — the
    // timeline, the summary, the preview — asks for `shell_ran` by name.
    if kind == ExternalEffectKind::Shell {
        active.shell_ran = true;
    }
    Ok(())
}

/// Commits an effect this app watched finish — the second half of the contract.
///
/// Called only on the success path, so "committed" means *observed to complete*
/// rather than *believed to have completed*. An error path deliberately leaves
/// the declaration standing alone: a failed HTTP call may still have been
/// delivered, and downgrading that to "nothing happened" is the one mistake this
/// whole enumeration exists to avoid.
///
/// Declares as well as commits, so a caller that reaches here can never leave a
/// committed effect that was never declared — the two lists stay a subset
/// relation by construction rather than by discipline.
pub fn commit_external_effect(
    state: &AppState,
    id: Option<&str>,
    kind: ExternalEffectKind,
) -> Result<(), String> {
    record_external_effect(state, id, kind)?;
    let Some(id) = id else {
        return Ok(());
    };
    let mut guard = state
        .checkpoints
        .lock()
        .map_err(|_| "Checkpoint lock poisoned".to_string())?;
    if let Some(active) = guard.get_mut(id) {
        active.committed_effects.insert(kind);
    }
    Ok(())
}

/// Writes the resume state onto an existing checkpoint, turning it into a
/// freeze image (roadmap K13).
///
/// # Why this takes the state rather than gathering it
///
/// The four things K13 names live in four different places — the model in the
/// runtime hub, the workspace in the sandbox, the approvals in the ledger — and
/// this module is a filesystem store with none of those handles. Gathering them
/// here would mean importing three subsystems into the one module that has
/// stayed free of them. The caller already holds all four at the moment it
/// decides to freeze.
///
/// # Freezing twice
///
/// Refused rather than overwritten. A second freeze of the same checkpoint would
/// silently replace the first image's process id and approvals while the entries
/// beneath it still describe the first turn, and a restore would then resume a
/// process into another one's files.
pub fn freeze_impl(base_dir: &Path, id: &str, resume: ResumeState) -> Result<(), String> {
    validate_id(id)?;
    let mut manifest = read_manifest(base_dir, id)?;
    if let Some(existing) = manifest.resume.as_ref() {
        return Err(format!(
            "checkpoint {id} is already a freeze of process {}",
            existing.process_id
        ));
    }
    manifest.resume = Some(resume);
    write_manifest(&base_dir.join(id), &manifest)
}

/// Freezes a checkpoint whose turn is **still running** — the half that makes a
/// restart survivable.
///
/// # Why [`freeze_impl`] could not do this
///
/// It reads the manifest, and a manifest only exists after `checkpoint_end`.
/// So it can only ever freeze a turn that already finished, which is a turn
/// with nothing left to resume. The process K13 wants to freeze is by definition
/// mid-flight: its checkpoint is open, held in `AppState::checkpoints`, and
/// nothing of it is on disk yet. A crash or a quit at that moment loses the
/// image entirely — which is the one moment the image exists for.
///
/// So this writes the manifest early, from the open checkpoint, and **leaves the
/// checkpoint open**. The turn keeps running and its later `checkpoint_end`
/// overwrites what is written here — with `resume: None`, correctly: a turn that
/// reached its own end has nothing to resume, and an entry-less one has its
/// directory removed, taking the stale image with it.
///
/// Writing the entries recorded *so far* is deliberate rather than incidental:
/// the image and the file snapshots have to describe the same instant, and a
/// resume that restored files from a later instant than the conversation would
/// be a state the process was never in.
pub fn freeze_live_impl(
    base_dir: &Path,
    state: &AppState,
    id: &str,
    resume: ResumeState,
) -> Result<(), String> {
    validate_id(id)?;
    let guard = state
        .checkpoints
        .lock()
        .map_err(|_| "Checkpoint lock poisoned".to_string())?;
    let Some(active) = guard.get(id) else {
        // Not an error: the turn may have ended between the park and this call,
        // and a finished turn is not a failed freeze. Same tolerance every other
        // `record_*` here has for an id with no open checkpoint.
        return Ok(());
    };
    // Refused for the same reason `freeze_impl` refuses: a second image would
    // replace the first one's process id while the entries beneath it still
    // describe the first, and a restore would resume one process into another's
    // files.
    if let Ok(existing) = read_manifest(base_dir, id) {
        if let Some(frozen) = existing.resume {
            return Err(format!(
                "checkpoint {id} is already a freeze of process {}",
                frozen.process_id
            ));
        }
    }
    let manifest = CheckpointManifest {
        version: MANIFEST_VERSION,
        created_at_ms: active.created_at_ms,
        session_id: active.session_id.clone(),
        anchor_index: active.anchor_index,
        label: active.label.clone(),
        shell_ran: active.shell_ran,
        external_effects: active.external_effects.iter().copied().collect(),
        committed_effects: Some(active.committed_effects.iter().copied().collect()),
        reverted: false,
        prev_id: active.prev_id.clone(),
        entries: active.entries.clone(),
        remembered_facts: active.remembered_facts.clone(),
        staged_task_suggestions: active.staged_task_suggestions.clone(),
        resume: Some(resume),
    };
    // No `after/` snapshots, unlike `end_impl`. Those record what the turn
    // *produced*, and this turn has not produced it yet — capturing them now
    // would label a mid-turn state as the finished one. `checkpoint_end` fills
    // them in when there is an answer to record.
    write_manifest(&active.dir, &manifest)
}

/// Why an image cannot be restored, one reason per thing that went missing.
///
/// A closed set with stable codes, for [`ExternalEffectKind`]'s reason.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum RestoreBlocker {
    /// The manifest holds no resume state — it is an ordinary turn checkpoint,
    /// not a freeze.
    NotAFreeze,
    /// The K10 workspace this process was running in no longer exists.
    WorkspaceGone,
    /// The model it was running against is not resident on this host.
    ModelNotResident,
    /// An approval outstanding at the freeze has since expired, so resuming
    /// would continue past a permission nobody currently grants.
    ApprovalExpired,
}

impl RestoreBlocker {
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            RestoreBlocker::NotAFreeze => "not-a-freeze",
            RestoreBlocker::WorkspaceGone => "workspace-gone",
            RestoreBlocker::ModelNotResident => "model-not-resident",
            RestoreBlocker::ApprovalExpired => "approval-expired",
        }
    }

    #[must_use]
    pub fn explanation(self) -> &'static str {
        match self {
            RestoreBlocker::NotAFreeze => {
                "This checkpoint is a turn snapshot rather than a frozen process, so there is no process state to resume."
            }
            RestoreBlocker::WorkspaceGone => {
                "The workspace this process was running in no longer exists. Resuming into a fresh one would silently drop whatever it had not committed, so the restore is refused instead."
            }
            RestoreBlocker::ModelNotResident => {
                "The model this process was running against is not loaded on this host. Resuming against a different one would continue the conversation in another model's voice, so the restore is refused instead."
            }
            RestoreBlocker::ApprovalExpired => {
                "An approval this process was waiting on has expired. Resuming would carry on past a permission nobody currently grants, so the restore is refused and the approval must be asked for again."
            }
        }
    }
}

/// Whether a frozen image can be resumed, as a tagged union.
///
/// Not `bool` plus a list, for [`crate::context_cache::PrefixSharing`]'s reason:
/// a caller cannot offer a Resume button without holding the state that says it
/// is safe, and cannot report a refusal without the blockers that caused it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase", tag = "state")]
pub enum Restorability {
    Resumable { process_id: String },
    Blocked { blockers: Vec<RestoreBlocker> },
}

/// What the host can currently offer a restore, supplied by the caller because
/// none of it is knowable from the manifest alone.
#[derive(Debug, Clone, Default)]
pub struct RestoreEnvironment<'a> {
    /// Models resident right now.
    pub resident_models: &'a [String],
    /// `request_id`s whose approval is still valid — an outstanding approval
    /// absent from this list has expired.
    pub live_approvals: &'a [String],
    /// Whether the recorded workspace path exists. Passed in rather than checked
    /// here so this stays a pure function over facts the caller has already
    /// gathered, testable without a filesystem.
    pub workspace_exists: bool,
}

/// Every reason this image cannot be resumed, or the process it resumes.
///
/// Collects *all* blockers rather than returning the first: a user told the
/// workspace is gone, who fixes that and is then told the model is not resident
/// either, has been made to discover one refusal at a time.
pub fn restorability(
    manifest: &CheckpointManifest,
    environment: &RestoreEnvironment<'_>,
) -> Restorability {
    let Some(resume) = manifest.resume.as_ref() else {
        return Restorability::Blocked {
            blockers: vec![RestoreBlocker::NotAFreeze],
        };
    };
    let mut blockers = Vec::new();
    if resume.workspace.is_some() && !environment.workspace_exists {
        blockers.push(RestoreBlocker::WorkspaceGone);
    }
    if let Some(model) = resume.model.as_ref() {
        if !environment
            .resident_models
            .iter()
            .any(|entry| entry == model)
        {
            blockers.push(RestoreBlocker::ModelNotResident);
        }
    }
    if resume.pending_approvals.iter().any(|request| {
        !environment
            .live_approvals
            .iter()
            .any(|live| live == request)
    }) {
        blockers.push(RestoreBlocker::ApprovalExpired);
    }
    if blockers.is_empty() {
        Restorability::Resumable {
            process_id: resume.process_id.clone(),
        }
    } else {
        Restorability::Blocked { blockers }
    }
}

/// What a resume does and does not reproduce (roadmap K13's "determinism
/// statement about what is and is not reproducible").
///
/// Enumerated and shipped beside the restore rather than written in a doc,
/// because the reader who needs it is the person deciding whether to trust a
/// resumed run — and a claim of "resumed exactly" that nobody qualified is worse
/// than no resume at all.
///
/// Every entry here is a thing that is **not** reproduced. There is deliberately
/// no "reproduced" list to balance it: the conversation, the workspace files and
/// the outstanding approvals are reproduced *because the restore refuses when
/// they cannot be*, which [`restorability`] enforces. A second list asserting it
/// would be prose restating a guard.
pub const DETERMINISM_CAVEATS: &[&str] = &[
    "Model sampling is not replayed. The same prompt against the same resident model can produce a different continuation, so a resumed turn is a fresh generation from the frozen point rather than a replay of one.",
    "Prompt-cache state is not part of the image. The first turn after a resume re-evaluates its prompt, which costs time but changes no output.",
    "Wall-clock time moved. Anything the conversation derived from the current date or elapsed time was true at the freeze and may not be now.",
    "External effects that already happened stay happened. A shell command, a network call or an MCP tool invoked before the freeze is not undone by resuming, and is not re-run either.",
    "Anything outside the recorded workspace is whatever it is now. Files elsewhere on disk, other processes, and remote state were not frozen and were free to change.",
];

/// Notes a fact `tool_remember` just added, so reverting this turn can forget it.
///
/// Silently a no-op without a checkpoint id, like [`record_external_effect`]: a
/// `remember` outside a checkpointed turn has nothing to be reverted *by*, and
/// refusing it would break remembering in exactly the sessions that never
/// checkpoint.
pub fn record_remembered_fact(
    state: &AppState,
    id: Option<&str>,
    fact: RememberedFact,
) -> Result<(), String> {
    let Some(id) = id else {
        return Ok(());
    };
    let mut guard = state
        .checkpoints
        .lock()
        .map_err(|_| "Checkpoint lock poisoned".to_string())?;
    let Some(active) = guard.get_mut(id) else {
        return Ok(());
    };
    // Deduplicated by id: `add_fact_impl` returns the *existing* fact when the
    // text already matches one, so remembering the same thing twice in a turn
    // must not queue two deletions of one fact.
    if !active
        .remembered_facts
        .iter()
        .any(|held| held.id == fact.id)
    {
        active.remembered_facts.push(fact);
    }
    Ok(())
}

/// Notes a follow-up chip `spawn_task` just staged, so reverting this turn can
/// withdraw it.
///
/// Silently a no-op without a checkpoint id, like [`record_remembered_fact`]:
/// a `spawn_task` outside a checkpointed turn has nothing to be reverted *by*.
pub fn record_task_suggestion(
    state: &AppState,
    id: Option<&str>,
    suggestion_id: String,
) -> Result<(), String> {
    let Some(id) = id else {
        return Ok(());
    };
    let mut guard = state
        .checkpoints
        .lock()
        .map_err(|_| "Checkpoint lock poisoned".to_string())?;
    let Some(active) = guard.get_mut(id) else {
        return Ok(());
    };
    // Deduplicated for `record_remembered_fact`'s reason: the same id must not
    // queue two withdrawals of one chip.
    if !active.staged_task_suggestions.contains(&suggestion_id) {
        active.staged_task_suggestions.push(suggestion_id);
    }
    Ok(())
}

/// Records both halves of a staged chip: that this turn had a
/// `TaskSuggestion` effect, and which chip to withdraw if it is reverted.
///
/// One command rather than two, so the enumerated effect and the id its
/// compensator needs can never be recorded by halves — a manifest that knew a
/// chip was staged but not which one would have to withdraw nothing, which is
/// the same trap `remembered_facts` documents.
#[tauri::command]
pub fn checkpoint_record_task_suggestion(
    state: tauri::State<'_, AppState>,
    id: Option<String>,
    suggestion_id: String,
) -> Result<(), String> {
    record_external_effect(
        state.inner(),
        id.as_deref(),
        ExternalEffectKind::TaskSuggestion,
    )?;
    record_task_suggestion(state.inner(), id.as_deref(), suggestion_id)
}

/// The chips a checkpoint's turn staged, for the frontend compensator to
/// withdraw on revert and restore on reapply.
///
/// Read back rather than returned by `checkpoint_revert`, because the revert
/// rewrites the manifest and a caller that read it afterwards would find the
/// list it needs already gone. See `checkpointReconciliation`'s own ordering
/// note — this is the same hazard `forget_remembered` avoids by reading the
/// manifest before `revert_impl` touches it.
#[tauri::command]
pub fn checkpoint_staged_task_suggestions(
    app: tauri::AppHandle,
    id: String,
) -> Result<Vec<String>, String> {
    validate_id(&id)?;
    Ok(read_manifest(&checkpoints_base_dir(&app)?, &id)?.staged_task_suggestions)
}

/// Every external effect a manifest records, reconstructing what an older one
/// could not say.
///
/// A manifest written before `external_effects` existed has an empty vec, and
/// an empty vec there means **unrecorded**, not *none*. It does carry
/// `shell_ran`, so that one kind is recovered; the others are simply absent
/// from the record and this function does not invent them.
#[must_use]
fn external_effects_of(manifest: &CheckpointManifest) -> Vec<ExternalEffectKind> {
    let mut effects = manifest.external_effects.clone();
    if manifest.shell_ran && !effects.contains(&ExternalEffectKind::Shell) {
        effects.push(ExternalEffectKind::Shell);
    }
    effects.sort();
    effects.dedup();
    effects
}

/// Parses a raw `manifest.json`, falling back from the versioned v2 struct
/// to the bare v1 `Vec<CheckpointEntry>` shape and synthesizing defaults
/// (creation time from the checkpoint dir's mtime, empty session/label) so
/// pre-upgrade checkpoints keep working with no migration pass.
fn parse_manifest(raw: &str, dir: &Path, id: &str) -> Result<CheckpointManifest, String> {
    if let Ok(manifest) = serde_json::from_str::<CheckpointManifest>(raw) {
        return Ok(manifest);
    }

    let entries: Vec<CheckpointEntry> = serde_json::from_str(raw)
        .map_err(|e| format!("Checkpoint '{}' manifest is corrupt: {}", id, e))?;
    let created_at_ms = std::fs::metadata(dir)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    Ok(CheckpointManifest {
        version: 1,
        created_at_ms,
        session_id: String::new(),
        anchor_index: 0,
        label: String::new(),
        shell_ran: false,
        // A v1 manifest recorded no effects and had no way to. Left empty here
        // and reconstructed by `external_effects_of`, which reads `shell_ran`
        // so the one signal a v1 manifest *does* carry is not lost.
        external_effects: Vec::new(),
        // Nothing watched a v1 turn's calls finish, so there is no completion
        // signal to report — `None`, not an empty list. See `EffectStatus`.
        committed_effects: None,
        // A v1 manifest predates both freezing and fact recording, so neither is
        // recoverable — empty and `None` are what say so.
        remembered_facts: Vec::new(),
        staged_task_suggestions: Vec::new(),
        resume: None,
        reverted: false,
        prev_id: None,
        entries,
    })
}

/// Reads and parses checkpoint `id`'s manifest from its directory.
fn read_manifest(base_dir: &Path, id: &str) -> Result<CheckpointManifest, String> {
    let dir = base_dir.join(id);
    let raw = std::fs::read_to_string(dir.join(MANIFEST_FILE))
        .map_err(|e| format!("Checkpoint '{}' not found or unreadable: {}", id, e))?;
    parse_manifest(&raw, &dir, id)
}

/// Atomic manifest write: sibling temp file + rename, same idiom as
/// `sessions.rs`, so a crash mid-write can never leave a truncated manifest
/// that would make the checkpoint unrevertable.
fn write_manifest(dir: &Path, manifest: &CheckpointManifest) -> Result<(), String> {
    let payload = serde_json::to_string_pretty(manifest)
        .map_err(|e| format!("Failed to serialize checkpoint manifest: {}", e))?;
    let path = dir.join(MANIFEST_FILE);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, payload)
        .map_err(|e| format!("Failed to write checkpoint manifest: {}", e))?;
    std::fs::rename(&tmp, &path)
        .map_err(|e| format!("Failed to finalize checkpoint manifest: {}", e))?;
    Ok(())
}

/// Subdirectory (inside a checkpoint's own dir) holding "after" backups —
/// see [`CheckpointEntry::after`]'s doc comment for why this is captured
/// here, at turn-end, rather than only ever at revert time like `redo/`.
const AFTER_DIR: &str = "after";

/// Core end logic: close checkpoint `id`, persist its manifest (or discard
/// the empty directory), and report what was touched plus the metadata the
/// frontend embeds in the transcript's checkpoint notice.
pub fn end_impl(state: &AppState, id: &str) -> Result<CheckpointSummary, String> {
    let taken = state
        .checkpoints
        .lock()
        .map_err(|_| "Checkpoint lock poisoned".to_string())?
        .remove(id);

    let Some(active) = taken else {
        return Ok(CheckpointSummary {
            id: String::new(),
            files: Vec::new(),
            anchor_index: 0,
            label: String::new(),
            shell_ran: false,
        });
    };

    if active.entries.is_empty() {
        let _ = std::fs::remove_dir_all(&active.dir);
        return Ok(CheckpointSummary {
            id: id.to_string(),
            files: Vec::new(),
            anchor_index: active.anchor_index,
            label: active.label,
            shell_ran: active.shell_ran,
        });
    }

    // Snapshot each touched file's post-turn ("after") content now, while
    // it's still guaranteed to be exactly what this turn produced — see
    // `CheckpointEntry::after`'s doc comment. Best-effort, like `redo`: a
    // copy failure just leaves `after: None` for that entry rather than
    // failing the whole `checkpoint_end` call, since this is purely an
    // enrichment for later preview/compare, not something revert depends on.
    let mut entries = active.entries.clone();
    let after_dir = active.dir.join(AFTER_DIR);
    for (i, entry) in entries.iter_mut().enumerate() {
        let target = Path::new(&entry.path);
        if target.is_file() && std::fs::create_dir_all(&after_dir).is_ok() {
            let after_name = format!("{}.bak", i);
            if std::fs::copy(target, after_dir.join(&after_name)).is_ok() {
                entry.after = Some(after_name);
            }
        }
    }

    let manifest = CheckpointManifest {
        version: MANIFEST_VERSION,
        created_at_ms: active.created_at_ms,
        session_id: active.session_id,
        anchor_index: active.anchor_index,
        label: active.label.clone(),
        shell_ran: active.shell_ran,
        external_effects: active.external_effects.iter().copied().collect(),
        // `Some` even when empty: this code observes commits, so an empty list
        // is a real "nothing completed", not the absence `None` stands for.
        committed_effects: Some(active.committed_effects.iter().copied().collect()),
        reverted: false,
        prev_id: active.prev_id,
        entries: entries.clone(),
        remembered_facts: active.remembered_facts.clone(),
        staged_task_suggestions: active.staged_task_suggestions.clone(),
        // An ordinary turn checkpoint, not a freeze. `freeze_process` is what
        // fills this, and `restorability` refuses anything that has not been.
        resume: None,
    };
    write_manifest(&active.dir, &manifest)?;

    Ok(CheckpointSummary {
        id: id.to_string(),
        files: entries.iter().map(|e| e.path.clone()).collect(),
        anchor_index: active.anchor_index,
        label: active.label,
        shell_ran: active.shell_ran,
    })
}

/// Subdirectory (inside a checkpoint's own dir) holding redo backups —
/// snapshots of each file's post-turn content taken right before `revert`
/// overwrites/deletes it, so `checkpoint_reapply` can play the turn back.
const REDO_DIR: &str = "redo";

/// Core revert logic: restore every file recorded in checkpoint `id`.
/// Returns how many files were restored/removed.
///
/// Before mutating each file, snapshots its current (post-turn) content into
/// `redo/<n>.bak` — best-effort: a failed snapshot leaves `entry.redo` as
/// `None`, which degrades that file to a no-op on a later `checkpoint_reapply`
/// rather than blocking the revert itself. The flipped `reverted` flag and
/// the redo backups are persisted via an atomic manifest rewrite; that write
/// is likewise best-effort so a read-only app-data dir (or any other
/// persistence failure) never prevents the revert from actually happening.
///
/// Idempotent: if `id` is already reverted, this is a no-op that returns
/// `Ok(0)` rather than re-running the snapshot-then-restore steps. Without
/// this guard, a second revert of an already-reverted checkpoint would
/// snapshot the file's *current* (already-restored, pre-turn) content into
/// `redo/<n>.bak`, clobbering the true post-turn content the first revert
/// recorded there and permanently losing the turn's real changes out from
/// under a later `checkpoint_reapply`. Two independent, ordinary-usage call
/// sites can otherwise reach this: `CheckpointTimeline.tsx`'s "Restore to
/// here" re-reverts every checkpoint newest→target unconditionally
/// (including ones the user already reverted individually earlier), and the
/// CLI's `/revert`/`monkey revert` can simply be invoked twice on the same id.
pub fn revert_impl(base_dir: &Path, id: &str) -> Result<u32, String> {
    validate_id(id)?;

    let dir = base_dir.join(id);
    let mut manifest = read_manifest(base_dir, id)?;
    if manifest.reverted {
        return Ok(0);
    }
    let redo_dir = dir.join(REDO_DIR);

    let mut reverted = 0u32;
    let mut errors: Vec<String> = Vec::new();

    for (i, entry) in manifest.entries.iter_mut().enumerate() {
        let target = Path::new(&entry.path);

        // Snapshot the post-turn content before it's overwritten/deleted.
        // Best-effort: a snapshot failure must not block the revert.
        if target.is_file() && std::fs::create_dir_all(&redo_dir).is_ok() {
            let redo_name = format!("{}.bak", i);
            if std::fs::copy(target, redo_dir.join(&redo_name)).is_ok() {
                entry.redo = Some(redo_name);
            }
        }

        let result = match &entry.backup {
            Some(backup_name) => std::fs::copy(dir.join(backup_name), target).map(|_| ()),
            None => match std::fs::remove_file(target) {
                // Already gone — the desired end state, not an error.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                other => other,
            },
        };
        match result {
            Ok(()) => reverted += 1,
            Err(e) => errors.push(format!("{}: {}", entry.path, e)),
        }
    }

    manifest.reverted = true;
    let _ = write_manifest(&dir, &manifest);

    if !errors.is_empty() {
        return Err(format!(
            "Reverted {} of {} files; failures: {}",
            reverted,
            manifest.entries.len(),
            errors.join("; ")
        ));
    }

    Ok(reverted)
}

/// Core reapply ("redo") logic: undoes a previous `revert_impl` on checkpoint
/// `id` by playing its `redo/` backups back over the files revert touched —
/// the inverse operation, restoring the turn's own changes. Returns how many
/// files were reapplied. Persists `reverted: false` the same best-effort way
/// `revert_impl` persists `reverted: true`.
///
/// Per entry: if a redo backup was recorded, it always wins — copying it
/// back over the target recreates a turn-created file exactly as `revert`
/// left it, or re-mutates a turn-modified file back to its post-turn state.
/// Only when there is no redo backup *and* the file was newly created by the
/// turn (`backup: None`) does reapply fall back to deleting the target — a
/// file that never existed before the turn and had nothing to snapshot at
/// revert time (i.e. was already gone) has nothing to recreate. A
/// pre-existing file (`backup: Some`) with no redo backup is left untouched:
/// that's an anomaly (the file should have existed at revert time), and
/// deleting a restored original would be strictly worse than a no-op.
///
/// Idempotent, mirroring `revert_impl`: if `id` isn't currently reverted,
/// this is a no-op returning `Ok(0)` rather than replaying redo backups over
/// files that were never touched by a revert.
pub fn reapply_impl(base_dir: &Path, id: &str) -> Result<u32, String> {
    validate_id(id)?;

    let dir = base_dir.join(id);
    let mut manifest = read_manifest(base_dir, id)?;
    if !manifest.reverted {
        return Ok(0);
    }
    let redo_dir = dir.join(REDO_DIR);

    let mut reapplied = 0u32;
    let mut errors: Vec<String> = Vec::new();

    for entry in &manifest.entries {
        let target = Path::new(&entry.path);
        let result = match (&entry.redo, &entry.backup) {
            (Some(redo_name), _) => std::fs::copy(redo_dir.join(redo_name), target).map(|_| ()),
            (None, None) => match std::fs::remove_file(target) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                other => other,
            },
            (None, Some(_)) => Ok(()),
        };
        match result {
            Ok(()) => reapplied += 1,
            Err(e) => errors.push(format!("{}: {}", entry.path, e)),
        }
    }

    manifest.reverted = false;
    let _ = write_manifest(&dir, &manifest);

    if !errors.is_empty() {
        return Err(format!(
            "Re-applied {} of {} files; failures: {}",
            reapplied,
            manifest.entries.len(),
            errors.join("; ")
        ));
    }

    Ok(reapplied)
}

// ---------------------------------------------------------------------------
// Preview, compare, and rollback-simulation — read-only layers built on top
// of the revert/reapply mechanism above (Checkpoint Preview and State-Aware
// Rollback, ROADMAP.md Phase 1). None of what follows mutates a checkpoint's
// manifest, its backup files, or the live workspace; it only reads what
// `record_original`/`end_impl`/`revert_impl` already captured.
// ---------------------------------------------------------------------------

/// One line in a computed diff, tagged with how it differs between "before"
/// and "after".
#[derive(serde::Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DiffLineKind {
    Context,
    Added,
    Removed,
}

#[derive(serde::Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub text: String,
}

/// A computed line-level diff between two texts. `truncated` is set (with
/// `lines` left empty) when the inputs are too large to diff cheaply — see
/// [`diff_lines`]'s size guard — so callers can show "diff too large to
/// display" instead of hanging or an incomplete render.
#[derive(serde::Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DiffResult {
    pub lines: Vec<DiffLine>,
    pub truncated: bool,
    pub added: usize,
    pub removed: usize,
}

/// Upper bound on `before_lines.len() * after_lines.len()` before
/// [`diff_lines`] gives up and reports `truncated: true` instead of running
/// its O(n*m) LCS table — a large enough file pair (e.g. two ~2000-line
/// files) would otherwise allocate tens of megabytes and spend real time on
/// a diff nobody asked for eagerly (this runs synchronously on a
/// `#[tauri::command]` call, not in the background).
const MAX_DIFF_CELLS: usize = 4_000_000;

/// Files larger than this (either side) skip line-splitting and diffing
/// entirely — a binary-adjacent or generated file can be well under
/// [`MAX_DIFF_CELLS`] in line count while still being multiple megabytes of
/// single-line content (e.g. minified JS, a lockfile with one huge line).
const MAX_DIFF_BYTES: usize = 2_000_000;

/// Computes a line-level diff between `before` and `after` via the standard
/// LCS (longest common subsequence) dynamic-programming table, then
/// backtracks it into a sequence of context/added/removed lines. Pure and
/// deterministic — no filesystem access — so it's directly unit-testable.
pub fn diff_lines(before: &str, after: &str) -> DiffResult {
    let before_lines: Vec<&str> = if before.is_empty() {
        Vec::new()
    } else {
        before.split('\n').collect()
    };
    let after_lines: Vec<&str> = if after.is_empty() {
        Vec::new()
    } else {
        after.split('\n').collect()
    };

    let n = before_lines.len();
    let m = after_lines.len();

    if n.saturating_mul(m) > MAX_DIFF_CELLS {
        return DiffResult {
            lines: Vec::new(),
            truncated: true,
            added: 0,
            removed: 0,
        };
    }

    // lcs[i][j] = length of the longest common subsequence of
    // before_lines[i..] and after_lines[j..].
    let mut lcs = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if before_lines[i] == after_lines[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }

    let mut lines = Vec::new();
    let mut added = 0usize;
    let mut removed = 0usize;
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if before_lines[i] == after_lines[j] {
            lines.push(DiffLine {
                kind: DiffLineKind::Context,
                text: before_lines[i].to_string(),
            });
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            lines.push(DiffLine {
                kind: DiffLineKind::Removed,
                text: before_lines[i].to_string(),
            });
            removed += 1;
            i += 1;
        } else {
            lines.push(DiffLine {
                kind: DiffLineKind::Added,
                text: after_lines[j].to_string(),
            });
            added += 1;
            j += 1;
        }
    }
    while i < n {
        lines.push(DiffLine {
            kind: DiffLineKind::Removed,
            text: before_lines[i].to_string(),
        });
        removed += 1;
        i += 1;
    }
    while j < m {
        lines.push(DiffLine {
            kind: DiffLineKind::Added,
            text: after_lines[j].to_string(),
        });
        added += 1;
        j += 1;
    }

    DiffResult {
        lines,
        truncated: false,
        added,
        removed,
    }
}

/// Where a [`FilePreviewEntry`]'s "after" content came from — callers use
/// this to decide how much to trust it (see [`CheckpointEntry::after`]'s doc
/// comment for the full reasoning).
#[derive(serde::Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SnapshotSource {
    /// Captured at `checkpoint_end`, immediately after the turn's own
    /// mutations — exact, unaffected by anything that happened later.
    Captured,
    /// Captured at revert time (`redo/`) — also exact, just captured later;
    /// only used as a fallback for pre-v3 manifests reverted before this
    /// feature existed.
    Redo,
    /// No stored snapshot exists; fell back to reading the CURRENT live
    /// workspace file. Exact only if nothing has touched this path since the
    /// turn ended — a best-effort fallback for pre-v3 manifests that were
    /// never reverted.
    Live,
    /// No content available from any source.
    Unavailable,
}

/// What kind of change (if any) a checkpoint made to one file, as far as the
/// available snapshots can determine.
#[derive(serde::Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum FileChangeStatus {
    Added,
    Modified,
    Deleted,
    Unchanged,
    /// The before/after state can't be fully determined (always a pre-v3
    /// manifest whose `after` was never captured, combined with no usable
    /// `redo`/live fallback either) — surfaced honestly rather than guessed.
    Unknown,
}

/// One file's preview within a checkpoint: what changed, how certain the
/// "after" side is, and (when computable) the actual diff.
#[derive(serde::Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct FilePreviewEntry {
    pub path: String,
    pub status: FileChangeStatus,
    pub before_bytes: usize,
    pub after_bytes: usize,
    pub after_source: SnapshotSource,
    pub binary: bool,
    pub diff: Option<DiffResult>,
}

/// Reads checkpoint entry `entry`'s pre-turn ("before") content, if any.
fn read_before(dir: &Path, entry: &CheckpointEntry) -> Option<Vec<u8>> {
    entry
        .backup
        .as_ref()
        .and_then(|name| std::fs::read(dir.join(name)).ok())
}

/// Manifest schema version from which `end_impl` always attempts an "after"
/// snapshot for every entry (see [`CheckpointEntry::after`]). Below this
/// version, a missing `entry.after` means "this feature didn't exist yet",
/// not "we looked and found nothing" — [`read_after`] uses this to decide
/// whether a missing snapshot is trustworthy evidence of deletion.
const AFTER_CAPTURE_MANIFEST_VERSION: u8 = 3;

/// Resolves checkpoint entry `entry`'s post-turn ("after") content, in order
/// of preference: the exact snapshot captured at `checkpoint_end` time, then
/// (only if `reverted`) the exact `redo/` snapshot captured at revert time,
/// then (only if NOT reverted) the current live workspace file as a
/// best-effort approximation. See [`SnapshotSource`] for what each source
/// implies about certainty.
///
/// `after_capture_attempted` (true for `manifest.version >=
/// AFTER_CAPTURE_MANIFEST_VERSION`) matters for the "entry.after is None"
/// case: on a v3+ manifest that means `end_impl` looked right at turn-end
/// and there was genuinely nothing there (the file was deleted, e.g. by a
/// shell command later in the same turn) — reported as `Captured` with no
/// bytes, not silently downgraded to a guess. On an older manifest it means
/// the capture step never ran at all, so the caller falls through to the
/// live/redo fallbacks instead.
fn read_after(
    dir: &Path,
    entry: &CheckpointEntry,
    reverted: bool,
    after_capture_attempted: bool,
) -> (Option<Vec<u8>>, SnapshotSource) {
    if let Some(name) = &entry.after {
        if let Ok(bytes) = std::fs::read(dir.join(AFTER_DIR).join(name)) {
            return (Some(bytes), SnapshotSource::Captured);
        }
    }
    if after_capture_attempted {
        // v3+: end_impl definitively looked at turn-end. No stored snapshot
        // means the file genuinely didn't exist then, not "unknown".
        return (None, SnapshotSource::Captured);
    }
    if reverted {
        if let Some(name) = &entry.redo {
            if let Ok(bytes) = std::fs::read(dir.join(REDO_DIR).join(name)) {
                return (Some(bytes), SnapshotSource::Redo);
            }
        }
        // Reverted with no redo backup: the live file now holds the BEFORE
        // state, not after — nothing usable to show as "after".
        return (None, SnapshotSource::Unavailable);
    }
    match std::fs::read(&entry.path) {
        Ok(bytes) => (Some(bytes), SnapshotSource::Live),
        Err(_) => (None, SnapshotSource::Unavailable),
    }
}

/// Builds one file's [`FilePreviewEntry`] from its checkpoint entry.
/// `manifest_version` decides how much [`read_after`]'s fallback chain can
/// trust a missing snapshot — see that function's and
/// [`AFTER_CAPTURE_MANIFEST_VERSION`]'s doc comments.
fn build_file_preview(
    dir: &Path,
    entry: &CheckpointEntry,
    reverted: bool,
    manifest_version: u8,
) -> FilePreviewEntry {
    let before = read_before(dir, entry);
    let after_capture_attempted = manifest_version >= AFTER_CAPTURE_MANIFEST_VERSION;
    let (after, after_source) = read_after(dir, entry, reverted, after_capture_attempted);
    let existed_before = entry.backup.is_some();

    let before_bytes = before.as_ref().map(Vec::len).unwrap_or(0);
    let after_bytes = after.as_ref().map(Vec::len).unwrap_or(0);

    let before_text = before.as_deref().and_then(|b| std::str::from_utf8(b).ok());
    let after_text = after.as_deref().and_then(|b| std::str::from_utf8(b).ok());
    let binary =
        (before.is_some() && before_text.is_none()) || (after.is_some() && after_text.is_none());

    // Only `Captured`/`Redo` are exact snapshots taken AT the relevant
    // moment; `Live`/`Unavailable` are best-effort or empty, so a `None`
    // paired with either of those is genuinely unknown, never a confirmed
    // deletion.
    let after_confident = matches!(
        after_source,
        SnapshotSource::Captured | SnapshotSource::Redo
    );

    let status = match (existed_before, &after) {
        (true, Some(after_bytes_vec)) => {
            if before.as_deref() == Some(after_bytes_vec.as_slice()) {
                FileChangeStatus::Unchanged
            } else {
                FileChangeStatus::Modified
            }
        }
        (true, None) => {
            if after_confident {
                FileChangeStatus::Deleted
            } else {
                FileChangeStatus::Unknown
            }
        }
        (false, Some(_)) => FileChangeStatus::Added,
        (false, None) => {
            // Created and then removed again within the very same turn: the
            // net effect on disk is nothing, but it's a KNOWN nothing when
            // an exact snapshot confirms it (not a guess).
            if after_confident {
                FileChangeStatus::Unchanged
            } else {
                FileChangeStatus::Unknown
            }
        }
    };

    let diff = if binary || status == FileChangeStatus::Unknown {
        None
    } else if before_bytes.max(after_bytes) > MAX_DIFF_BYTES {
        Some(DiffResult {
            lines: Vec::new(),
            truncated: true,
            added: 0,
            removed: 0,
        })
    } else {
        Some(diff_lines(
            before_text.unwrap_or(""),
            after_text.unwrap_or(""),
        ))
    };

    FilePreviewEntry {
        path: entry.path.clone(),
        status,
        before_bytes,
        after_bytes,
        after_source,
        binary,
        diff,
    }
}

/// Full preview of one checkpoint: its metadata plus a per-file breakdown of
/// what changed. Read-only — never touches the live workspace beyond reading
/// files that were already going to be read for the fallback `Live` source.
#[derive(serde::Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointPreview {
    pub id: String,
    pub label: String,
    pub created_at_ms: u64,
    pub session_id: String,
    pub anchor_index: usize,
    pub shell_ran: bool,
    pub reverted: bool,
    pub files: Vec<FilePreviewEntry>,
}

/// Core preview logic, parameterized by base dir for testability.
pub fn preview_impl(base_dir: &Path, id: &str) -> Result<CheckpointPreview, String> {
    validate_id(id)?;
    let dir = base_dir.join(id);
    let manifest = read_manifest(base_dir, id)?;
    let files = manifest
        .entries
        .iter()
        .map(|entry| build_file_preview(&dir, entry, manifest.reverted, manifest.version))
        .collect();
    Ok(CheckpointPreview {
        id: id.to_string(),
        label: manifest.label,
        created_at_ms: manifest.created_at_ms,
        session_id: manifest.session_id,
        anchor_index: manifest.anchor_index,
        shell_ran: manifest.shell_ran,
        reverted: manifest.reverted,
        files,
    })
}

/// Lists a checkpoint's per-file preview (status, diff, provenance) without
/// touching the live workspace beyond reading files for the best-effort
/// `Live` fallback source. Read-only UI plumbing, like `checkpoint_list` —
/// intentionally NOT routed through the permission system.
#[tauri::command]
pub fn checkpoint_preview(app: tauri::AppHandle, id: String) -> Result<CheckpointPreview, String> {
    preview_impl(&checkpoints_base_dir(&app)?, &id)
}

/// One file's side-by-side comparison across two checkpoints (which need not
/// be adjacent, or even from the same session).
#[derive(serde::Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CompareFileEntry {
    pub path: String,
    pub in_a: bool,
    pub in_b: bool,
    pub a: Option<FilePreviewEntry>,
    pub b: Option<FilePreviewEntry>,
    /// Diff between checkpoint A's resulting content and checkpoint B's —
    /// the actual "what differs between these two checkpoints" comparison,
    /// distinct from either checkpoint's own before/after turn diff. `None`
    /// when either side's "after" content isn't available/text.
    pub between: Option<DiffResult>,
}

#[derive(serde::Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointCompareResult {
    pub a: CheckpointPreview,
    pub b: CheckpointPreview,
    pub files: Vec<CompareFileEntry>,
}

/// Core compare logic: the union of every file either checkpoint touched,
/// each with its own per-checkpoint preview plus a direct A-vs-B diff of
/// their resulting content. Entirely read-only — reads each checkpoint's
/// manifest and backups independently; never restores either one.
pub fn compare_impl(
    base_dir: &Path,
    id_a: &str,
    id_b: &str,
) -> Result<CheckpointCompareResult, String> {
    let preview_a = preview_impl(base_dir, id_a)?;
    let preview_b = preview_impl(base_dir, id_b)?;

    let mut paths: Vec<String> = Vec::new();
    for f in preview_a.files.iter().chain(preview_b.files.iter()) {
        if !paths.contains(&f.path) {
            paths.push(f.path.clone());
        }
    }

    let dir_a = base_dir.join(id_a);
    let dir_b = base_dir.join(id_b);
    let manifest_a = read_manifest(base_dir, id_a)?;
    let manifest_b = read_manifest(base_dir, id_b)?;

    let files = paths
        .into_iter()
        .map(|path| {
            let a = preview_a.files.iter().find(|f| f.path == path).cloned();
            let b = preview_b.files.iter().find(|f| f.path == path).cloned();

            let a_text = manifest_a
                .entries
                .iter()
                .find(|e| e.path == path)
                .and_then(|e| {
                    let attempted = manifest_a.version >= AFTER_CAPTURE_MANIFEST_VERSION;
                    let (bytes, _src) = read_after(&dir_a, e, manifest_a.reverted, attempted);
                    bytes.and_then(|b| String::from_utf8(b).ok())
                });
            let b_text = manifest_b
                .entries
                .iter()
                .find(|e| e.path == path)
                .and_then(|e| {
                    let attempted = manifest_b.version >= AFTER_CAPTURE_MANIFEST_VERSION;
                    let (bytes, _src) = read_after(&dir_b, e, manifest_b.reverted, attempted);
                    bytes.and_then(|b| String::from_utf8(b).ok())
                });

            let between = match (&a_text, &b_text) {
                (Some(ta), Some(tb)) if ta.len().max(tb.len()) <= MAX_DIFF_BYTES => {
                    Some(diff_lines(ta, tb))
                }
                (Some(_), Some(_)) => Some(DiffResult {
                    lines: Vec::new(),
                    truncated: true,
                    added: 0,
                    removed: 0,
                }),
                _ => None,
            };

            CompareFileEntry {
                path,
                in_a: a.is_some(),
                in_b: b.is_some(),
                a,
                b,
                between,
            }
        })
        .collect();

    Ok(CheckpointCompareResult {
        a: preview_a,
        b: preview_b,
        files,
    })
}

/// Compares checkpoints `id_a` and `id_b` without restoring either one. Like
/// `checkpoint_preview`, read-only UI plumbing, not permission-gated.
#[tauri::command]
pub fn checkpoint_compare(
    app: tauri::AppHandle,
    id_a: String,
    id_b: String,
) -> Result<CheckpointCompareResult, String> {
    compare_impl(&checkpoints_base_dir(&app)?, &id_a, &id_b)
}

/// What reverting one file entry will actually do to the live workspace,
/// determined by comparing its CURRENT content against what the checkpoint
/// would restore — not just assumed from `backup`/`None` the way
/// `revert_impl` itself does, since a simulation's whole point is to catch
/// the case where reality has drifted from that assumption.
#[derive(serde::Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RestoreAction {
    /// The live file will be overwritten with the checkpoint's pre-turn
    /// content.
    Restore,
    /// The live file (created by this turn) will be deleted.
    Delete,
    /// The live file already matches the pre-turn content — reverting this
    /// entry would be a no-op.
    NoOp,
}

#[derive(serde::Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RestorePlanEntry {
    pub path: String,
    pub action: RestoreAction,
    /// True when the live file no longer matches what THIS checkpoint's turn
    /// itself produced (its captured/live "after" state) — meaning something
    /// else has touched the file since this turn ended, so reverting now
    /// would discard that other change too, not just this turn's. `false`
    /// when the after-state is unknown (a pre-v3 manifest with nothing to
    /// compare against) — absence of evidence isn't evidence of drift.
    pub drifted: bool,
}

/// A read-only "what will happen if I revert this checkpoint right now"
/// report — the rollback-simulation step the UI runs before actually calling
/// `checkpoint_revert`. Never mutates anything.
#[derive(serde::Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RestoreSimulation {
    pub id: String,
    pub already_reverted: bool,
    pub files: Vec<RestorePlanEntry>,
    /// True when this turn had at least one recorded effect that reverting
    /// cannot undo.
    ///
    /// Derived from [`Self::uncompensated`] rather than from `shell_ran`
    /// alone, which is what it used to be — so a turn that only made a network
    /// call no longer reports `false` and reads as "nothing to reconcile".
    pub needs_reconciliation: bool,
    /// Every recorded effect outside the workspace files, each with what
    /// undoes it or the stated reason nothing does.
    ///
    /// This is K14's enumerated set. It is recorded in the manifest when the
    /// effect happens, so — unlike `checkpointReconciliation.ts`'s
    /// transcript-derived list, which is finer-grained but only survives while
    /// the messages do — it is still here after a context compaction.
    pub external_effects: Vec<ExternalEffectRecord>,
}

/// One recorded external effect, how far it got, and its compensation.
#[derive(serde::Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ExternalEffectRecord {
    pub kind: ExternalEffectKind,
    pub status: EffectStatus,
    pub compensation: Compensation,
}

/// How far an effect got through K14's declare-then-commit contract.
///
/// Three states rather than a `committed: bool`, because "we watched it fail"
/// and "nobody was watching" are different facts and only one of them is a
/// reason to worry less. Collapsing them would make every checkpoint written
/// before the commit phase look like a turn whose every call was abandoned.
#[derive(serde::Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum EffectStatus {
    /// Declared and then watched to completion. It definitely happened.
    Committed,
    /// Declared, and this app never saw it finish — cancelled, errored, or the
    /// app went down mid-call. It may or may not have happened, and reverting
    /// has to assume it did.
    Declared,
    /// Written before the commit phase existed, so there is no completion
    /// signal either way. Reads exactly as this whole list read before K14's
    /// second half: recorded, outcome unstated.
    Unobserved,
}

impl RestoreSimulation {
    /// The effects nothing can undo. Every one of them today — see
    /// [`ExternalEffectKind::compensator`] — which is exactly why the type
    /// says so per effect instead of the caller assuming it.
    fn uncompensated(effects: &[ExternalEffectRecord]) -> bool {
        effects
            .iter()
            .any(|effect| matches!(effect.compensation, Compensation::None { .. }))
    }

    fn from_effects(
        id: String,
        already_reverted: bool,
        files: Vec<RestorePlanEntry>,
        manifest: &CheckpointManifest,
    ) -> Self {
        let external_effects: Vec<ExternalEffectRecord> = external_effects_of(manifest)
            .into_iter()
            .map(|kind| ExternalEffectRecord {
                kind,
                status: match manifest.committed_effects.as_deref() {
                    None => EffectStatus::Unobserved,
                    Some(committed) if committed.contains(&kind) => EffectStatus::Committed,
                    Some(_) => EffectStatus::Declared,
                },
                compensation: kind.compensator(),
            })
            .collect();
        RestoreSimulation {
            id,
            already_reverted,
            files,
            needs_reconciliation: Self::uncompensated(&external_effects),
            external_effects,
        }
    }
}

/// Core simulate-restore logic, parameterized by base dir for testability.
/// Read-only: every comparison is against the CURRENT live file, never
/// followed by a write.
pub fn simulate_restore_impl(base_dir: &Path, id: &str) -> Result<RestoreSimulation, String> {
    validate_id(id)?;
    let dir = base_dir.join(id);
    let manifest = read_manifest(base_dir, id)?;

    if manifest.reverted {
        return Ok(RestoreSimulation::from_effects(
            id.to_string(),
            true,
            Vec::new(),
            &manifest,
        ));
    }

    let files = manifest
        .entries
        .iter()
        .map(|entry| {
            let target = Path::new(&entry.path);
            let live = std::fs::read(target).ok();
            let before = read_before(&dir, entry);

            let will_be_noop = match (&entry.backup, &live) {
                (Some(_), Some(live_bytes)) => before.as_deref() == Some(live_bytes.as_slice()),
                (None, None) => true,
                _ => false,
            };
            let action = if will_be_noop {
                RestoreAction::NoOp
            } else if entry.backup.is_some() {
                RestoreAction::Restore
            } else {
                RestoreAction::Delete
            };

            // Drift: does the live file currently match what THIS turn
            // produced? Only meaningful when we actually know the turn's
            // "after" state (never trust a Live-sourced "after" to detect
            // drift against itself — it IS the live file by definition).
            let after_capture_attempted = manifest.version >= AFTER_CAPTURE_MANIFEST_VERSION;
            let (after, after_source) = read_after(&dir, entry, false, after_capture_attempted);
            let drifted = match after_source {
                SnapshotSource::Captured | SnapshotSource::Redo => match (&after, &live) {
                    (Some(a), Some(l)) => a != l,
                    (Some(_), None) => true,
                    (None, Some(_)) => true,
                    (None, None) => false,
                },
                SnapshotSource::Live | SnapshotSource::Unavailable => false,
            };

            RestorePlanEntry {
                path: entry.path.clone(),
                action,
                drifted,
            }
        })
        .collect();

    Ok(RestoreSimulation::from_effects(
        id.to_string(),
        false,
        files,
        &manifest,
    ))
}

/// Simulates reverting checkpoint `id` without actually doing it — the
/// rollback-simulation step the UI runs before `checkpoint_revert`. Like
/// `checkpoint_preview`, read-only and not permission-gated.
#[tauri::command]
pub fn checkpoint_simulate_restore(
    app: tauri::AppHandle,
    id: String,
) -> Result<RestoreSimulation, String> {
    simulate_restore_impl(&checkpoints_base_dir(&app)?, &id)
}

/// Core list logic, parameterized by base dir for testability: scans every
/// checkpoint directory under `base_dir`, reads its manifest (v2, or the v1
/// fallback via [`parse_manifest`]), and returns a [`CheckpointInfo`] per
/// finished checkpoint, newest-first. A directory with no manifest yet (a
/// turn still in flight — `checkpoint_end` hasn't run) or an unreadable/
/// corrupt one is silently skipped rather than failing the whole list.
/// `session_id` optionally restricts the result to one session's checkpoints
/// (used by the timeline's "Restore to here" chain, which is only
/// well-defined within a single session).
pub fn list_impl(base_dir: &Path, session_id: Option<&str>) -> Result<Vec<CheckpointInfo>, String> {
    let read_dir = std::fs::read_dir(base_dir)
        .map_err(|e| format!("Failed to read checkpoints dir: {}", e))?;

    let mut infos: Vec<CheckpointInfo> = Vec::new();
    for entry in read_dir.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let id = entry.file_name().to_string_lossy().to_string();
        let Ok(manifest) = read_manifest(base_dir, &id) else {
            continue;
        };
        if let Some(filter) = session_id {
            if manifest.session_id != filter {
                continue;
            }
        }
        infos.push(CheckpointInfo::from_manifest(id, &manifest));
    }

    infos.sort_by(|a, b| b.created_at_ms.cmp(&a.created_at_ms));
    Ok(infos)
}

/// Lists checkpoints newest-first for the timeline UI, optionally filtered to
/// one session. Read-only UI plumbing — like `checkpoint_revert`,
/// intentionally NOT routed through the permission system.
#[tauri::command]
pub fn checkpoint_list(
    app: tauri::AppHandle,
    session_id: Option<String>,
) -> Result<Vec<CheckpointInfo>, String> {
    list_impl(&checkpoints_base_dir(&app)?, session_id.as_deref())
}

/// Open a new per-turn checkpoint and return its id. `session_id`,
/// `anchor_index` (the turn's user-message index in the transcript) and
/// `label` (prompt prefix) are supplied by the frontend agent loop and end
/// up in the manifest; `max_keep` overrides the default retention cap.
#[tauri::command]
pub fn checkpoint_begin(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    session_id: String,
    anchor_index: usize,
    label: String,
    max_keep: Option<usize>,
) -> Result<String, String> {
    begin_impl(
        state.inner(),
        &checkpoints_base_dir(&app)?,
        session_id,
        anchor_index,
        label,
        max_keep,
    )
}

/// Close checkpoint `id`; returns the touched files (empty = nothing was
/// mutated this turn and the checkpoint was discarded).
#[tauri::command]
pub fn checkpoint_end(
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<CheckpointSummary, String> {
    end_impl(state.inner(), &id)
}

/// RAII guard for one entry in `AppState::checkpoint_locks`: removes `id`
/// from the in-progress set on drop (including on early return via `?`), so
/// a lock can never get stuck if revert/reapply errors out or panics.
struct RevertLockGuard<'a> {
    state: &'a AppState,
    id: String,
}

impl Drop for RevertLockGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut locks) = self.state.checkpoint_locks.lock() {
            locks.remove(&self.id);
        }
    }
}

/// Claims the revert/reapply lock for checkpoint `id`, erroring if another
/// revert or reapply for the same id is already in progress — see
/// `AppState::checkpoint_locks`'s doc comment for why this is needed.
fn acquire_revert_lock<'a>(state: &'a AppState, id: &str) -> Result<RevertLockGuard<'a>, String> {
    let mut locks = state
        .checkpoint_locks
        .lock()
        .map_err(|_| "Checkpoint lock poisoned".to_string())?;
    if !locks.insert(id.to_string()) {
        return Err(format!("Checkpoint '{}' is already being restored", id));
    }
    Ok(RevertLockGuard {
        state,
        id: id.to_string(),
    })
}

/// Restore every file recorded in checkpoint `id` to its pre-turn state.
/// A direct, human-initiated UI action (the transcript's "Revert" button) —
/// like `git_commit`, intentionally NOT routed through the permission system.
#[tauri::command]
pub fn checkpoint_revert(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<u32, String> {
    let _lock = acquire_revert_lock(state.inner(), &id)?;
    let base_dir = checkpoints_base_dir(&app)?;
    // Read before the revert, because `revert_impl` rewrites the manifest.
    let remembered = read_manifest(&base_dir, &id)
        .map(|manifest| manifest.remembered_facts)
        .unwrap_or_default();
    let reverted = revert_impl(&base_dir, &id)?;
    forget_remembered(&app, state.inner(), &remembered)?;
    Ok(reverted)
}

/// Runs the Memory compensator: deletes exactly the facts this turn added.
///
/// Ordered after the file revert so a failure to reach the memory store cannot
/// leave the files half-restored — the files are the part a user notices, and
/// this is additive bookkeeping on top.
///
/// A fact already gone is not an error. The user may have pressed Forget on it
/// themselves, and re-reporting that as a failed revert would make them chase a
/// problem they already fixed.
fn forget_remembered(
    app: &tauri::AppHandle,
    state: &AppState,
    remembered: &[RememberedFact],
) -> Result<(), String> {
    if remembered.is_empty() {
        return Ok(());
    }
    let root = crate::workspace::primary_root_canon(state)
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(|_| crate::memory::GLOBAL_SCOPE_KEY.to_string());
    let path = crate::memory::memories_file_path(app)?;
    let _lock = state
        .memory_lock
        .lock()
        .map_err(|_| "Memory lock poisoned".to_string())?;
    for fact in remembered {
        let _ = crate::memory::delete_fact_impl(&path, &root, &fact.id);
    }
    Ok(())
}

/// Undo a previous revert of checkpoint `id`: plays its `redo/` backups back
/// over the files revert touched, restoring the turn's own changes. Like
/// `checkpoint_revert`, a direct human-initiated UI action, not permission-gated.
#[tauri::command]
pub fn checkpoint_reapply(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<u32, String> {
    let _lock = acquire_revert_lock(state.inner(), &id)?;
    let base_dir = checkpoints_base_dir(&app)?;
    let remembered = read_manifest(&base_dir, &id)
        .map(|manifest| manifest.remembered_facts)
        .unwrap_or_default();
    let reapplied = reapply_impl(&base_dir, &id)?;
    // The other half of the compensator, and the reason the manifest keeps each
    // fact's text: revert deleted them, so reapply has to be able to put them
    // back. An undo that cannot be undone is data loss with a friendly name.
    if !remembered.is_empty() {
        let root = crate::workspace::primary_root_canon(state.inner())
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_else(|_| crate::memory::GLOBAL_SCOPE_KEY.to_string());
        let path = crate::memory::memories_file_path(&app)?;
        let _memory_lock = state
            .memory_lock
            .lock()
            .map_err(|_| "Memory lock poisoned".to_string())?;
        for fact in &remembered {
            // `add_fact_impl` short-circuits on identical text, so a fact the
            // user restored by hand is not duplicated. The new id differs from
            // the recorded one, which is why a second revert re-reads the
            // manifest rather than trusting ids to stay stable.
            let _ = crate::memory::add_fact_impl(&path, &root, &fact.text, "agent", None);
        }
    }
    Ok(reapplied)
}

/// Freeze a **live** turn's checkpoint into a resumable image (roadmap K13).
///
/// Called at the moment a cooperative loop actually parks, which is the tool
/// boundary the acceptance names. Everything about it is [`freeze_live_impl`];
/// this is the command wrapper, holding the same revert lock `checkpoint_freeze`
/// does so a freeze and a revert of one checkpoint cannot both rewrite its
/// manifest with the last writer winning silently.
#[tauri::command]
pub fn checkpoint_freeze_live(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
    resume: ResumeState,
) -> Result<(), String> {
    let _lock = acquire_revert_lock(state.inner(), &id)?;
    freeze_live_impl(&checkpoints_base_dir(&app)?, state.inner(), &id, resume)
}

/// Drops checkpoint `id`'s resume image, leaving the checkpoint itself intact.
///
/// Called once a resume has actually re-entered the loop. Without it the image
/// outlives the thing it describes: the next `freeze_live_impl` on the same
/// checkpoint would be refused as a double freeze, and — worse — a later restart
/// would offer to resume a turn that is already running.
///
/// A checkpoint that is not a freeze is left alone and reported as such rather
/// than silently succeeding, because a caller clearing an image it never wrote
/// has lost track of which checkpoint it is holding.
#[tauri::command]
pub fn checkpoint_clear_freeze(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<bool, String> {
    let _lock = acquire_revert_lock(state.inner(), &id)?;
    let base_dir = checkpoints_base_dir(&app)?;
    validate_id(&id)?;
    let mut manifest = read_manifest(&base_dir, &id)?;
    if manifest.resume.take().is_none() {
        return Ok(false);
    }
    write_manifest(&base_dir.join(&id), &manifest)?;
    Ok(true)
}

/// Freeze a suspended process into checkpoint `id` (roadmap K13).
///
/// The caller supplies the resume state because it is the only party holding all
/// four pieces — see [`freeze_impl`]. Takes the revert lock for the same reason
/// revert does: a freeze and a revert of the same checkpoint both rewrite its
/// manifest, and the last writer would otherwise win silently.
#[tauri::command]
pub fn checkpoint_freeze(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
    resume: ResumeState,
) -> Result<(), String> {
    let _lock = acquire_revert_lock(state.inner(), &id)?;
    freeze_impl(&checkpoints_base_dir(&app)?, &id, resume)
}

/// Whether checkpoint `id` can be resumed here and now, and the determinism
/// caveats that apply if it is.
///
/// The caveats travel with the verdict rather than sitting in a doc: the reader
/// who needs them is whoever is deciding to press Resume.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreReport {
    pub restorability: Restorability,
    pub determinism_caveats: Vec<String>,
    /// Each blocker's explanation, in the same order, for the same reason the
    /// caveats ship here: the reader who needs it is whoever has to fix the
    /// missing thing, and `RestoreBlocker`'s codes are stable identifiers rather
    /// than sentences. Empty when the image is resumable.
    pub blocker_explanations: Vec<String>,
}

#[tauri::command]
pub fn checkpoint_restorability(
    app: tauri::AppHandle,
    id: String,
    resident_models: Vec<String>,
    live_approvals: Vec<String>,
) -> Result<RestoreReport, String> {
    let manifest = read_manifest(&checkpoints_base_dir(&app)?, &id)?;
    // Checked here rather than by the caller: the path is in the manifest, and a
    // caller that had to look it up first could report a stale answer.
    let workspace_exists = manifest
        .resume
        .as_ref()
        .and_then(|resume| resume.workspace.as_ref())
        .is_none_or(|workspace| Path::new(workspace).is_dir());
    let restorability = restorability(
        &manifest,
        &RestoreEnvironment {
            resident_models: &resident_models,
            live_approvals: &live_approvals,
            workspace_exists,
        },
    );
    let blocker_explanations = match &restorability {
        Restorability::Resumable { .. } => Vec::new(),
        Restorability::Blocked { blockers } => blockers
            .iter()
            .map(|blocker| blocker.explanation().to_string())
            .collect(),
    };
    Ok(RestoreReport {
        restorability,
        determinism_caveats: DETERMINISM_CAVEATS
            .iter()
            .map(|c| (*c).to_string())
            .collect(),
        blocker_explanations,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- K13 freeze image -------------------------------------------------

    fn frozen(resume: Option<ResumeState>) -> CheckpointManifest {
        CheckpointManifest {
            remembered_facts: Vec::new(),
            staged_task_suggestions: Vec::new(),
            version: MANIFEST_VERSION,
            created_at_ms: 1,
            session_id: "s-1".to_string(),
            anchor_index: 0,
            label: "freeze".to_string(),
            shell_ran: false,
            external_effects: Vec::new(),
            committed_effects: Some(Vec::new()),
            reverted: false,
            prev_id: None,
            entries: Vec::new(),
            resume,
        }
    }

    fn resume_state() -> ResumeState {
        ResumeState {
            process_id: "p-frozen".to_string(),
            frozen_at_ms: 10,
            model: Some("llama-3.1-8b".to_string()),
            runtime_id: Some("managed-llama".to_string()),
            workspace: Some("/tmp/ws".to_string()),
            pending_approvals: vec!["req-1".to_string()],
        }
    }

    fn environment<'a>(
        resident: &'a [String],
        approvals: &'a [String],
        workspace_exists: bool,
    ) -> RestoreEnvironment<'a> {
        RestoreEnvironment {
            resident_models: resident,
            live_approvals: approvals,
            workspace_exists,
        }
    }

    /// An ordinary turn checkpoint is not a freeze, and must not be offered as
    /// one — the overwhelmingly common manifest takes this branch.
    #[test]
    fn a_turn_checkpoint_is_not_restorable_as_a_process() {
        assert_eq!(
            restorability(&frozen(None), &environment(&[], &[], true)),
            Restorability::Blocked {
                blockers: vec![RestoreBlocker::NotAFreeze]
            }
        );
    }

    #[test]
    fn a_freeze_whose_world_is_intact_resumes() {
        let resident = vec!["llama-3.1-8b".to_string()];
        let approvals = vec!["req-1".to_string()];
        assert_eq!(
            restorability(
                &frozen(Some(resume_state())),
                &environment(&resident, &approvals, true)
            ),
            Restorability::Resumable {
                process_id: "p-frozen".to_string()
            }
        );
    }

    /// Every blocker at once, not the first: a user who fixes the workspace and
    /// is then told the model is missing has been made to discover the refusals
    /// one at a time.
    #[test]
    fn a_refusal_names_every_reason_rather_than_the_first() {
        let blocked = restorability(&frozen(Some(resume_state())), &environment(&[], &[], false));
        let Restorability::Blocked { blockers } = blocked else {
            panic!("expected a refusal");
        };
        assert_eq!(
            blockers,
            vec![
                RestoreBlocker::WorkspaceGone,
                RestoreBlocker::ModelNotResident,
                RestoreBlocker::ApprovalExpired,
            ]
        );
    }

    /// Resuming past an approval that has since expired would continue on a
    /// permission nobody currently grants.
    #[test]
    fn an_expired_approval_alone_blocks_the_restore() {
        let resident = vec!["llama-3.1-8b".to_string()];
        assert_eq!(
            restorability(
                &frozen(Some(resume_state())),
                &environment(&resident, &[], true)
            ),
            Restorability::Blocked {
                blockers: vec![RestoreBlocker::ApprovalExpired]
            }
        );
    }

    /// A manifest written before freezing existed decodes with `resume: None`
    /// rather than failing — and reads as "not a freeze", which it was not.
    #[test]
    fn an_older_manifest_decodes_as_not_a_freeze() {
        let older = serde_json::json!({
            "version": 2,
            "created_at_ms": 1,
            "session_id": "s-old",
            "anchor_index": 0,
            "label": "before freezing existed",
            "shell_ran": false,
            "reverted": false,
            "entries": []
        });
        let manifest: CheckpointManifest =
            serde_json::from_value(older).expect("an older manifest still decodes");
        assert_eq!(manifest.resume, None);
        assert!(matches!(
            restorability(&manifest, &environment(&[], &[], true)),
            Restorability::Blocked { .. }
        ));
    }

    // -- K14's first real compensator --------------------------------------

    /// Recording is deduplicated by id, because `add_fact_impl` returns the
    /// *existing* fact when the text already matches one — remembering the same
    /// thing twice in a turn must not queue two deletions of the one fact.
    #[test]
    fn remembering_the_same_fact_twice_records_it_once() {
        let state = AppState::default();
        let id = "00000000-0000-4000-8000-00000recall1";
        state.checkpoints.lock().unwrap().insert(
            id.to_string(),
            ActiveCheckpoint {
                dir: PathBuf::from("/tmp/unused"),
                entries: Vec::new(),
                created_at_ms: 1,
                session_id: "s".to_string(),
                anchor_index: 0,
                label: String::new(),
                shell_ran: false,
                external_effects: Default::default(),
                committed_effects: Default::default(),
                prev_id: None,
                remembered_facts: Vec::new(),
                staged_task_suggestions: Vec::new(),
            },
        );
        let fact = RememberedFact {
            id: "f-1".to_string(),
            text: "the API lives on port 8080".to_string(),
        };
        record_remembered_fact(&state, Some(id), fact.clone()).unwrap();
        record_remembered_fact(&state, Some(id), fact.clone()).unwrap();
        record_remembered_fact(
            &state,
            Some(id),
            RememberedFact {
                id: "f-2".to_string(),
                text: "and the worker on 8081".to_string(),
            },
        )
        .unwrap();
        let guard = state.checkpoints.lock().unwrap();
        let held = &guard.get(id).unwrap().remembered_facts;
        assert_eq!(
            held.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(),
            vec!["f-1", "f-2"]
        );

        // No checkpoint, and an unknown one, are both no-ops rather than errors:
        // a `remember` outside a checkpointed turn has nothing to be reverted by.
        drop(guard);
        record_remembered_fact(&state, None, fact.clone()).unwrap();
        record_remembered_fact(&state, Some("nope"), fact).unwrap();
    }

    /// Two of the five effects this app can take back, and three it cannot —
    /// which is what K14's acceptance means by `needs_reconciliation` becoming
    /// the exception for an enumerated set rather than the default answer for
    /// everything outside the workspace files.
    ///
    /// A compensated effect says what pressing revert will *do*, not merely that
    /// something can be done. An uncompensated one says why nothing can, in its
    /// own words rather than a shared caveat.
    #[test]
    fn the_two_undoable_effects_are_compensated_and_the_other_three_say_why_not() {
        for kind in [
            ExternalEffectKind::Memory,
            ExternalEffectKind::TaskSuggestion,
        ] {
            let Compensation::Undo { action } = kind.compensator() else {
                panic!("{kind:?} has a real undo in this app and must name it");
            };
            assert!(
                action.len() > 20,
                "{kind:?} must say what reverting will do, not just that it will"
            );
        }
        for kind in [
            ExternalEffectKind::Shell,
            ExternalEffectKind::Network,
            ExternalEffectKind::McpTool,
        ] {
            let Compensation::None { reason } = kind.compensator() else {
                panic!("{kind:?} has no undo in this app and must not claim one");
            };
            assert!(reason.len() > 20, "{kind:?} refuses without a reason");
        }
    }

    /// A turn whose only external effect is a staged chip no longer reports as
    /// unreconcilable — the narrowing is derived from `Compensation::None`, so
    /// adding the second undo needed no second place to update.
    #[test]
    fn a_turn_that_only_proposed_follow_up_work_needs_no_reconciliation() {
        let state = AppState::default();
        let base = TempDir::new("suggest");
        let ws = TempDir::new("ws");

        let file = ws.path.join("f.txt");
        std::fs::write(&file, "v1").unwrap();
        let id = begin(&state, &base.path);
        record_original(&state, Some(&id), &file).unwrap();
        std::fs::write(&file, "v2").unwrap();
        record_external_effect(&state, Some(&id), ExternalEffectKind::TaskSuggestion).unwrap();
        // Recorded twice, to pin that the id list deduplicates like the fact
        // list does — two withdrawals of one chip is not an undo, it is a bug.
        record_task_suggestion(&state, Some(&id), "chip-1".to_string()).unwrap();
        record_task_suggestion(&state, Some(&id), "chip-1".to_string()).unwrap();
        record_task_suggestion(&state, Some(&id), "chip-2".to_string()).unwrap();
        end_impl(&state, &id).unwrap();

        let sim = simulate_restore_impl(&base.path, &id).unwrap();
        assert!(
            !sim.needs_reconciliation,
            "a proposed chip can be withdrawn, so there is nothing to reconcile"
        );
        assert_eq!(
            read_manifest(&base.path, &id)
                .unwrap()
                .staged_task_suggestions,
            vec!["chip-1".to_string(), "chip-2".to_string()]
        );
    }

    /// The text is kept beside the id so a reapply can put the fact back. A
    /// manifest that recorded only ids would make revert a one-way door.
    #[test]
    fn a_remembered_fact_survives_the_manifest_round_trip_with_its_text() {
        let base = TempDir::new("remember");
        let id = "00000000-0000-4000-8000-00000recall2";
        let dir = base.path.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let mut manifest = frozen(None);
        manifest.remembered_facts = vec![RememberedFact {
            id: "f-1".to_string(),
            text: "the API lives on port 8080".to_string(),
        }];
        write_manifest(&dir, &manifest).unwrap();

        let reloaded = read_manifest(&base.path, id).unwrap();
        assert_eq!(reloaded.remembered_facts, manifest.remembered_facts);
    }

    /// An older manifest recorded no facts, and empty there means *unrecorded*
    /// rather than *none* — which is why the compensator runs off this list and
    /// not off `ExternalEffectKind::Memory` being present. A manifest that knows
    /// a fact was remembered but not which one must delete nothing.
    #[test]
    fn an_older_manifest_records_no_facts_and_so_deletes_none() {
        let older = serde_json::json!({
            "version": 2,
            "created_at_ms": 1,
            "session_id": "s-old",
            "anchor_index": 0,
            "label": "before fact recording existed",
            "shell_ran": false,
            "external_effects": ["memory"],
            "reverted": false,
            "entries": []
        });
        let manifest: CheckpointManifest = serde_json::from_value(older).unwrap();
        assert!(manifest.remembered_facts.is_empty());
        assert!(
            external_effects_of(&manifest).contains(&ExternalEffectKind::Memory),
            "the effect is still known — only the specific facts are not"
        );
    }

    /// The whole point of K13: the image survives the process that wrote it.
    /// Written to disk, read back by a different call, and still restorable.
    #[test]
    fn a_freeze_survives_on_disk_and_refuses_to_be_written_twice() {
        let base = TempDir::new("freeze");
        let id = "00000000-0000-4000-8000-0000freeze01";
        let dir = base.path.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        write_manifest(&dir, &frozen(None)).unwrap();

        freeze_impl(&base.path, id, resume_state()).expect("the freeze lands");

        // Read back through the ordinary path — nothing in memory is carried
        // over, which is what "resumed after a restart" actually requires.
        let reloaded = read_manifest(&base.path, id).expect("the manifest reloads");
        assert_eq!(
            reloaded.resume.as_ref().map(|r| r.process_id.as_str()),
            Some("p-frozen")
        );
        let resident = vec!["llama-3.1-8b".to_string()];
        let approvals = vec!["req-1".to_string()];
        assert_eq!(
            restorability(&reloaded, &environment(&resident, &approvals, true)),
            Restorability::Resumable {
                process_id: "p-frozen".to_string()
            }
        );

        // A second freeze would leave the image describing one process while the
        // entries beneath it describe another turn.
        let twice = freeze_impl(&base.path, id, resume_state());
        assert!(
            matches!(twice, Err(ref message) if message.contains("already a freeze")),
            "{twice:?}"
        );
    }

    /// The case a restart actually presents: the turn never ended, so nothing
    /// ever called `checkpoint_end` and no manifest was written by it.
    ///
    /// `freeze_impl` cannot serve this — it reads a manifest, and reading one is
    /// exactly what a mid-flight checkpoint has no way to satisfy. A freeze that
    /// only works after the turn finished is a freeze of a turn with nothing
    /// left to resume.
    #[test]
    fn a_live_turns_image_reaches_disk_while_the_checkpoint_is_still_open() {
        let state = AppState::default();
        let base = TempDir::new("freeze-live");
        let ws = TempDir::new("ws");

        let file = ws.path.join("f.txt");
        std::fs::write(&file, "v1").unwrap();
        let id = begin(&state, &base.path);
        record_original(&state, Some(&id), &file).unwrap();
        std::fs::write(&file, "v2").unwrap();

        // Nothing on disk yet: this is the state a crash would have found.
        assert!(read_manifest(&base.path, &id).is_err());

        freeze_live_impl(&base.path, &state, &id, resume_state()).expect("the live freeze lands");

        let reloaded = read_manifest(&base.path, &id).expect("the image is readable");
        assert_eq!(
            reloaded.resume.as_ref().map(|r| r.process_id.as_str()),
            Some("p-frozen")
        );
        assert_eq!(
            reloaded.entries.len(),
            1,
            "the files recorded so far travel with the image — a resume that \
             restored a later instant's files than the conversation would be a \
             state the process was never in"
        );
        // Still open: the turn has not ended, and freezing must not end it.
        assert!(state.checkpoints.lock().unwrap().contains_key(&id));

        // Refused twice, for `freeze_impl`'s reason.
        let twice = freeze_live_impl(&base.path, &state, &id, resume_state());
        assert!(
            matches!(twice, Err(ref message) if message.contains("already a freeze")),
            "{twice:?}"
        );

        // And when the turn does finish, its own end overwrites the image with
        // `resume: None` — a completed turn has nothing to resume, and an image
        // left behind would offer to restart one that already ran to the end.
        end_impl(&state, &id).unwrap();
        assert!(read_manifest(&base.path, &id).unwrap().resume.is_none());
    }

    /// A turn that parked before touching a file still gets an image, and its
    /// own end still cleans up after it.
    ///
    /// `end_impl` deletes an entry-less checkpoint's directory outright, which is
    /// the right disposal for a stale image too — but only because the deletion
    /// happens when the turn *finished*. A freeze written into that same
    /// directory has to survive until then.
    #[test]
    fn a_freeze_with_no_files_yet_survives_until_the_turn_ends() {
        let state = AppState::default();
        let base = TempDir::new("freeze-empty");
        let id = begin(&state, &base.path);

        freeze_live_impl(&base.path, &state, &id, resume_state()).expect("the live freeze lands");
        assert!(read_manifest(&base.path, &id).unwrap().resume.is_some());

        end_impl(&state, &id).unwrap();
        assert!(
            read_manifest(&base.path, &id).is_err(),
            "the turn ended with nothing recorded, so the directory and the image go with it"
        );
    }

    /// Freezing a checkpoint that already ended is a no-op, not an error.
    ///
    /// The park and the freeze are two steps, and a turn can finish between
    /// them — a stop delivered while the loop was parked, say. Reporting that as
    /// a failure would make an ordinary race look like a broken freeze.
    #[test]
    fn freezing_a_turn_that_already_ended_reports_nothing_to_freeze() {
        let state = AppState::default();
        let base = TempDir::new("freeze-gone");
        assert!(freeze_live_impl(
            &base.path,
            &state,
            "00000000-0000-4000-8000-0000freeze09",
            resume_state()
        )
        .is_ok());
    }

    /// Every blocker states a reason, and every caveat is real prose — an empty
    /// one would make the determinism statement a claim of nothing.
    #[test]
    fn every_blocker_and_caveat_says_something() {
        for blocker in [
            RestoreBlocker::NotAFreeze,
            RestoreBlocker::WorkspaceGone,
            RestoreBlocker::ModelNotResident,
            RestoreBlocker::ApprovalExpired,
        ] {
            assert!(!blocker.code().is_empty());
            assert!(blocker.explanation().len() > 40, "{:?}", blocker);
        }
        assert!(!DETERMINISM_CAVEATS.is_empty());
        for caveat in DETERMINISM_CAVEATS {
            assert!(caveat.len() > 40, "{caveat}");
        }
    }

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            // Nanos alone can collide across parallel test threads — the
            // atomic counter guarantees uniqueness within the process.
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "little_monkey_checkpoint_test_{}_{}_{}_{}",
                tag,
                std::process::id(),
                n,
                nanos
            ));
            std::fs::create_dir_all(&path).unwrap();
            TempDir { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// `begin_impl` with default metadata, for tests that don't care about it.
    fn begin(state: &AppState, base_dir: &Path) -> String {
        begin_impl(
            state,
            base_dir,
            "test-session".to_string(),
            0,
            "test prompt".to_string(),
            None,
        )
        .unwrap()
    }

    #[test]
    fn record_is_a_noop_without_a_checkpoint_id() {
        let state = AppState::default();
        let ws = TempDir::new("ws");
        let file = ws.path.join("a.txt");
        std::fs::write(&file, "hello").unwrap();

        record_original(&state, None, &file).unwrap();
        record_original(&state, Some("0000-unknown-id"), &file).unwrap();

        assert!(state.checkpoints.lock().unwrap().is_empty());
    }

    #[test]
    fn record_shell_sets_the_flag_and_is_a_noop_without_a_checkpoint_id() {
        let state = AppState::default();
        let base = TempDir::new("base");

        // No-op cases: no id, or an id with no active checkpoint.
        record_shell(&state, None).unwrap();
        record_shell(&state, Some("0000-unknown-id")).unwrap();

        let id = begin(&state, &base.path);
        assert!(!state.checkpoints.lock().unwrap()[&id].shell_ran);

        record_shell(&state, Some(&id)).unwrap();
        assert!(
            state.checkpoints.lock().unwrap()[&id].shell_ran,
            "shell_ran must flip to true"
        );

        // The flag must survive into the persisted manifest via checkpoint_end.
        let ws = TempDir::new("ws");
        let file = ws.path.join("a.txt");
        std::fs::write(&file, "x").unwrap();
        record_original(&state, Some(&id), &file).unwrap();
        let summary = end_impl(&state, &id).unwrap();
        assert!(summary.shell_ran);
        assert!(read_manifest(&base.path, &id).unwrap().shell_ran);
    }

    #[test]
    fn end_discards_an_empty_checkpoint() {
        let state = AppState::default();
        let base = TempDir::new("base");

        let id = begin(&state, &base.path);
        let summary = end_impl(&state, &id).unwrap();

        assert_eq!(summary.id, id);
        assert!(summary.files.is_empty());
        assert!(
            !base.path.join(&id).exists(),
            "empty checkpoint dir must be removed"
        );
    }

    #[test]
    fn revert_restores_modified_file_and_deletes_created_file() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let existing = ws.path.join("existing.txt");
        std::fs::write(&existing, "original").unwrap();
        let created = ws.path.join("created.txt");

        let id = begin(&state, &base.path);
        record_original(&state, Some(&id), &existing).unwrap();
        std::fs::write(&existing, "mutated").unwrap();
        record_original(&state, Some(&id), &created).unwrap();
        std::fs::write(&created, "brand new").unwrap();
        let summary = end_impl(&state, &id).unwrap();

        assert_eq!(summary.files.len(), 2);

        let reverted = revert_impl(&base.path, &id).unwrap();
        assert_eq!(reverted, 2);
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "original");
        assert!(
            !created.exists(),
            "file created during the turn must be deleted on revert"
        );
    }

    #[test]
    fn concurrent_checkpoints_record_and_end_independently() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let file_a = ws.path.join("a.txt");
        std::fs::write(&file_a, "a-original").unwrap();
        let file_b = ws.path.join("b.txt");
        std::fs::write(&file_b, "b-original").unwrap();

        // Two turns in flight at once (split pane): each records its own
        // file under its own checkpoint id, in interleaved order.
        let id_a = begin(&state, &base.path);
        let id_b = begin(&state, &base.path);
        record_original(&state, Some(&id_a), &file_a).unwrap();
        std::fs::write(&file_a, "a-mutated").unwrap();
        record_original(&state, Some(&id_b), &file_b).unwrap();
        std::fs::write(&file_b, "b-mutated").unwrap();

        // Ending turn A must not close or steal turn B's checkpoint.
        let summary_a = end_impl(&state, &id_a).unwrap();
        assert_eq!(summary_a.id, id_a);
        assert_eq!(summary_a.files, vec![file_a.to_string_lossy().to_string()]);

        let summary_b = end_impl(&state, &id_b).unwrap();
        assert_eq!(summary_b.id, id_b);
        assert_eq!(summary_b.files, vec![file_b.to_string_lossy().to_string()]);

        // Each revert restores only its own turn's file.
        revert_impl(&base.path, &id_a).unwrap();
        assert_eq!(std::fs::read_to_string(&file_a).unwrap(), "a-original");
        assert_eq!(std::fs::read_to_string(&file_b).unwrap(), "b-mutated");
        revert_impl(&base.path, &id_b).unwrap();
        assert_eq!(std::fs::read_to_string(&file_b).unwrap(), "b-original");
    }

    #[test]
    fn only_the_first_mutation_of_a_path_is_recorded() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let file = ws.path.join("a.txt");
        std::fs::write(&file, "v1").unwrap();

        let id = begin(&state, &base.path);
        record_original(&state, Some(&id), &file).unwrap();
        std::fs::write(&file, "v2").unwrap();
        record_original(&state, Some(&id), &file).unwrap(); // second write same turn
        std::fs::write(&file, "v3").unwrap();
        end_impl(&state, &id).unwrap();

        revert_impl(&base.path, &id).unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "v1",
            "revert must restore the true pre-turn original, not an intermediate version"
        );
    }

    #[test]
    fn revert_rejects_traversal_ids() {
        let base = TempDir::new("base");
        let err = revert_impl(&base.path, "../outside").unwrap_err();
        assert!(
            err.contains("Invalid checkpoint id"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn revert_errors_for_unknown_id() {
        let base = TempDir::new("base");
        let err = revert_impl(&base.path, "0000-does-not-exist").unwrap_err();
        assert!(err.contains("not found"), "unexpected error: {err}");
    }

    #[test]
    fn end_writes_a_v2_manifest_with_metadata_and_echoes_it() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let file = ws.path.join("a.txt");
        std::fs::write(&file, "original").unwrap();

        let before_ms = now_ms();
        let id = begin_impl(
            &state,
            &base.path,
            "session-42".to_string(),
            7,
            "fix the login bug".to_string(),
            None,
        )
        .unwrap();
        record_original(&state, Some(&id), &file).unwrap();
        std::fs::write(&file, "mutated").unwrap();
        let summary = end_impl(&state, &id).unwrap();

        // The summary echoes the metadata the frontend embeds in its notice.
        assert_eq!(summary.anchor_index, 7);
        assert_eq!(summary.label, "fix the login bug");
        assert!(!summary.shell_ran);

        let manifest = read_manifest(&base.path, &id).unwrap();
        assert_eq!(manifest.version, MANIFEST_VERSION);
        assert_eq!(manifest.session_id, "session-42");
        assert_eq!(manifest.anchor_index, 7);
        assert_eq!(manifest.label, "fix the login bug");
        assert!(!manifest.shell_ran);
        assert!(!manifest.reverted);
        assert!(
            manifest.created_at_ms >= before_ms,
            "created_at_ms must be set at begin time"
        );
        assert_eq!(manifest.entries.len(), 1);
    }

    #[test]
    fn v1_manifest_on_disk_still_reverts() {
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let existing = ws.path.join("existing.txt");
        std::fs::write(&existing, "mutated").unwrap();
        let created = ws.path.join("created.txt");
        std::fs::write(&created, "brand new").unwrap();

        // A real pre-upgrade checkpoint: a bare entry array, exactly as the
        // old `end_impl` serialized it.
        let id = "00000000-0000-4000-8000-00000000v1ok";
        let dir = base.path.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("0.bak"), "original").unwrap();
        // Hand-written JSON (no "redo" key at all, unlike anything this
        // version of the code would serialize) so the fallback path is
        // genuinely exercised, not just an equivalent Rust struct.
        let raw = format!(
            r#"[{{"path":{:?},"backup":"0.bak"}},{{"path":{:?},"backup":null}}]"#,
            existing.to_string_lossy(),
            created.to_string_lossy()
        );
        std::fs::write(dir.join(MANIFEST_FILE), raw).unwrap();

        let reverted = revert_impl(&base.path, id).unwrap();
        assert_eq!(reverted, 2);
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "original");
        assert!(
            !created.exists(),
            "file created during the v1 turn must be deleted on revert"
        );
    }

    #[test]
    fn v1_manifest_reads_with_synthesized_defaults() {
        let base = TempDir::new("base");

        let id = "00000000-0000-4000-8000-0000000v1meta";
        let dir = base.path.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let raw = r#"[{"path":"/tmp/a.txt","backup":"0.bak"}]"#;
        std::fs::write(dir.join(MANIFEST_FILE), raw).unwrap();

        let manifest = read_manifest(&base.path, id).unwrap();
        assert_eq!(manifest.version, 1);
        assert!(manifest.session_id.is_empty());
        assert_eq!(manifest.anchor_index, 0);
        assert!(manifest.label.is_empty());
        assert!(!manifest.shell_ran);
        assert!(!manifest.reverted);
        assert!(
            manifest.created_at_ms > 0,
            "created_at_ms must be synthesized from the dir mtime"
        );
        assert_eq!(manifest.entries.len(), 1);
    }

    #[test]
    fn begin_prunes_to_the_supplied_max_keep() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        // Four *finished* checkpoints (each with a real manifest.json),
        // oldest first by created_at_ms.
        let mut ids = Vec::new();
        for n in 0..4 {
            let file = ws.path.join(format!("f{n}.txt"));
            std::fs::write(&file, "x").unwrap();
            let id = begin_impl(
                &state,
                &base.path,
                "s".to_string(),
                0,
                "p".to_string(),
                None,
            )
            .unwrap();
            record_original(&state, Some(&id), &file).unwrap();
            std::fs::write(&file, "y").unwrap();
            end_impl(&state, &id).unwrap();
            ids.push(id);
            std::thread::sleep(std::time::Duration::from_millis(15));
        }

        let id = begin_impl(
            &state,
            &base.path,
            "s".to_string(),
            0,
            "p".to_string(),
            Some(2),
        )
        .unwrap();

        let remaining: Vec<String> = std::fs::read_dir(&base.path)
            .unwrap()
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        // The two oldest were pruned; the two newest plus the just-opened
        // checkpoint remain.
        assert_eq!(remaining.len(), 3, "remaining: {remaining:?}");
        assert!(
            !remaining.contains(&ids[0]),
            "oldest checkpoint must be pruned"
        );
        assert!(
            !remaining.contains(&ids[1]),
            "second-oldest checkpoint must be pruned"
        );
        assert!(remaining.contains(&ids[2]));
        assert!(remaining.contains(&ids[3]));
        assert!(remaining.contains(&id));
    }

    #[test]
    fn prune_never_deletes_an_in_flight_checkpoint_regardless_of_age() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        // Turn A begins and stays open (no checkpoint_end) — its directory
        // has no manifest.json yet.
        let id_a = begin_impl(
            &state,
            &base.path,
            "s".to_string(),
            0,
            "p".to_string(),
            None,
        )
        .unwrap();
        assert!(base.path.join(&id_a).is_dir());

        // Several other turns finish afterwards, each bumping the total
        // count of on-disk directories past a very small max_keep.
        for n in 0..5 {
            let file = ws.path.join(format!("f{n}.txt"));
            std::fs::write(&file, "x").unwrap();
            let id = begin_impl(
                &state,
                &base.path,
                "s".to_string(),
                0,
                "p".to_string(),
                Some(1),
            )
            .unwrap();
            record_original(&state, Some(&id), &file).unwrap();
            end_impl(&state, &id).unwrap();
        }

        // Turn A's directory must have survived every intervening prune_old
        // call, since it never got a manifest and is still open.
        assert!(
            base.path.join(&id_a).is_dir(),
            "in-flight checkpoint dir must never be pruned"
        );
        assert!(state.checkpoints.lock().unwrap().contains_key(&id_a));

        // And a mutation recorded against it afterwards must still succeed.
        let file_a = ws.path.join("a.txt");
        std::fs::write(&file_a, "original").unwrap();
        record_original(&state, Some(&id_a), &file_a).unwrap();
    }

    #[test]
    fn prune_sweeps_an_abandoned_in_flight_checkpoint_once_old_enough() {
        let base = TempDir::new("base");

        // A manifest-less directory whose mtime is far older than the
        // abandoned-in-flight cutoff — simulating a turn that crashed before
        // ever calling checkpoint_end.
        let stale_id = "00000000-0000-4000-8000-0000000stale1";
        let stale_dir = base.path.join(stale_id);
        std::fs::create_dir_all(&stale_dir).unwrap();
        let ancient = std::time::SystemTime::now()
            - std::time::Duration::from_millis(ABANDONED_IN_FLIGHT_MAX_AGE_MS + 60_000);
        set_dir_mtime(&stale_dir, ancient);

        prune_old(&base.path, 20);

        assert!(
            !stale_dir.exists(),
            "an old-enough manifest-less directory must be swept as abandoned"
        );
    }

    /// Backdates `dir`'s mtime for the abandoned-in-flight sweep test above.
    fn set_dir_mtime(dir: &Path, t: std::time::SystemTime) {
        let file = open_dir_handle(dir).expect("open dir for mtime update");
        let times = std::fs::FileTimes::new().set_modified(t);
        file.set_times(times).expect("set directory mtime");
    }

    /// Opens `dir` (a directory, not a regular file) as a [`std::fs::File`]
    /// handle suitable for [`std::fs::File::set_times`].
    ///
    /// Plain `File::open` works for this on Unix, where a directory can be
    /// opened like any other path. On Windows, `CreateFileW` refuses to open
    /// a directory at all unless `FILE_FLAG_BACKUP_SEMANTICS` is passed —
    /// without it this fails with `ERROR_ACCESS_DENIED` (os error 5), which
    /// is exactly the panic Windows CI hit here. Request only
    /// attribute-level access (not a generic read/write handle): opening a
    /// directory with `GENERIC_WRITE` is unreliable, but `FILE_WRITE_ATTRIBUTES`
    /// is both sufficient for `set_times` and reliably grantable on a
    /// directory (this mirrors the approach the `filetime` crate uses for
    /// the same operation).
    #[cfg(windows)]
    fn open_dir_handle(dir: &Path) -> std::io::Result<std::fs::File> {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        const FILE_READ_ATTRIBUTES: u32 = 0x0080;
        const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
        std::fs::OpenOptions::new()
            .read(true)
            .access_mode(FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(dir)
    }

    #[cfg(not(windows))]
    fn open_dir_handle(dir: &Path) -> std::io::Result<std::fs::File> {
        std::fs::File::open(dir)
    }

    #[test]
    fn prune_ranks_by_manifest_created_at_ms_not_filesystem_mtime() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        // A (oldest), B, C (newest) by created_at_ms.
        let make = |n: u64| {
            let file = ws.path.join(format!("f{n}.txt"));
            std::fs::write(&file, "x").unwrap();
            let id = begin_impl(
                &state,
                &base.path,
                "s".to_string(),
                0,
                "p".to_string(),
                None,
            )
            .unwrap();
            record_original(&state, Some(&id), &file).unwrap();
            end_impl(&state, &id).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(15));
            id
        };
        let id_a = make(0);
        let id_b = make(1);
        let id_c = make(2);

        // Reverting A rewrites its manifest.json, which would bump its
        // directory's mtime — but must NOT make it look newer than B.
        revert_impl(&base.path, &id_a).unwrap();

        // With max_keep=2, the correct "keep newest 2 by created_at_ms" is
        // {B, C}; a mtime-based ranking would instead keep {A, C} since A
        // was just touched by revert.
        let _ = begin_impl(
            &state,
            &base.path,
            "s".to_string(),
            0,
            "p".to_string(),
            Some(2),
        )
        .unwrap();

        assert!(
            !base.path.join(&id_a).exists(),
            "reverted-but-genuinely-oldest checkpoint must still be pruned"
        );
        assert!(
            base.path.join(&id_b).exists(),
            "genuinely newer checkpoint must survive pruning"
        );
        assert!(base.path.join(&id_c).exists());
    }

    #[test]
    fn revert_then_reapply_roundtrips_a_turn_created_file() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let created = ws.path.join("created.txt");

        let id = begin(&state, &base.path);
        record_original(&state, Some(&id), &created).unwrap();
        std::fs::write(&created, "brand new").unwrap();
        end_impl(&state, &id).unwrap();

        revert_impl(&base.path, &id).unwrap();
        assert!(
            !created.exists(),
            "revert must delete the turn-created file"
        );
        assert!(
            read_manifest(&base.path, &id).unwrap().reverted,
            "revert must persist reverted: true"
        );

        let reapplied = reapply_impl(&base.path, &id).unwrap();
        assert_eq!(reapplied, 1);
        assert_eq!(
            std::fs::read_to_string(&created).unwrap(),
            "brand new",
            "reapply must recreate the turn-created file with its post-turn content"
        );
        assert!(
            !read_manifest(&base.path, &id).unwrap().reverted,
            "reapply must persist reverted: false"
        );
    }

    #[test]
    fn revert_then_reapply_roundtrips_a_turn_modified_file() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let existing = ws.path.join("existing.txt");
        std::fs::write(&existing, "original").unwrap();

        let id = begin(&state, &base.path);
        record_original(&state, Some(&id), &existing).unwrap();
        std::fs::write(&existing, "mutated").unwrap();
        end_impl(&state, &id).unwrap();

        revert_impl(&base.path, &id).unwrap();
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "original");

        let reapplied = reapply_impl(&base.path, &id).unwrap();
        assert_eq!(reapplied, 1);
        assert_eq!(
            std::fs::read_to_string(&existing).unwrap(),
            "mutated",
            "reapply must restore the turn's mutated content"
        );
    }

    #[test]
    fn revert_is_idempotent_and_does_not_corrupt_the_redo_backup() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let file = ws.path.join("f.txt");
        std::fs::write(&file, "v1").unwrap();

        let id = begin(&state, &base.path);
        record_original(&state, Some(&id), &file).unwrap();
        std::fs::write(&file, "v2").unwrap();
        end_impl(&state, &id).unwrap();

        // First revert: file goes back to "v1", redo/0.bak correctly holds "v2".
        let first = revert_impl(&base.path, &id).unwrap();
        assert_eq!(first, 1);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v1");

        // A second revert of the SAME (already-reverted) checkpoint — e.g.
        // CheckpointTimeline's "Restore to here" re-reverting a checkpoint
        // the user already reverted individually, or `/revert` run twice —
        // must be a no-op, not re-snapshot the current ("v1") content over
        // the true post-turn ("v2") redo backup.
        let second = revert_impl(&base.path, &id).unwrap();
        assert_eq!(
            second, 0,
            "a repeat revert of an already-reverted checkpoint must be a no-op"
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "v1",
            "the file itself must be unaffected"
        );

        let reapplied = reapply_impl(&base.path, &id).unwrap();
        assert_eq!(reapplied, 1);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "v2",
            "reapply after a redundant revert must still restore the turn's true post-turn content"
        );
    }

    #[test]
    fn reapply_is_idempotent() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let file = ws.path.join("f.txt");
        std::fs::write(&file, "v1").unwrap();

        let id = begin(&state, &base.path);
        record_original(&state, Some(&id), &file).unwrap();
        std::fs::write(&file, "v2").unwrap();
        end_impl(&state, &id).unwrap();

        // Reapply before any revert has ever happened: nothing to redo yet.
        let noop = reapply_impl(&base.path, &id).unwrap();
        assert_eq!(
            noop, 0,
            "reapply on a never-reverted checkpoint must be a no-op"
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "v2",
            "must not touch the file"
        );

        revert_impl(&base.path, &id).unwrap();
        reapply_impl(&base.path, &id).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v2");

        // A second reapply after the checkpoint is already re-applied
        // (reverted: false) must likewise be a no-op.
        let second = reapply_impl(&base.path, &id).unwrap();
        assert_eq!(second, 0, "a repeat reapply must be a no-op");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v2");
    }

    #[test]
    fn begin_records_prev_id_as_the_sessions_current_head() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let make = |n: u64| {
            let file = ws.path.join(format!("f{n}.txt"));
            std::fs::write(&file, "x").unwrap();
            let id = begin_impl(
                &state,
                &base.path,
                "s1".to_string(),
                0,
                "p".to_string(),
                None,
            )
            .unwrap();
            record_original(&state, Some(&id), &file).unwrap();
            end_impl(&state, &id).unwrap();
            id
        };
        let id_a = make(0);
        let id_b = make(1);

        let manifest_a = read_manifest(&base.path, &id_a).unwrap();
        assert_eq!(
            manifest_a.prev_id, None,
            "the session's first checkpoint has no predecessor"
        );

        let manifest_b = read_manifest(&base.path, &id_b).unwrap();
        assert_eq!(
            manifest_b.prev_id,
            Some(id_a.clone()),
            "must link to the session's previous checkpoint"
        );

        // A checkpoint in a different session must not be treated as a
        // predecessor.
        let file_c = ws.path.join("c.txt");
        std::fs::write(&file_c, "x").unwrap();
        let id_other_session = begin_impl(
            &state,
            &base.path,
            "s2".to_string(),
            0,
            "p".to_string(),
            None,
        )
        .unwrap();
        record_original(&state, Some(&id_other_session), &file_c).unwrap();
        end_impl(&state, &id_other_session).unwrap();
        assert_eq!(
            read_manifest(&base.path, &id_other_session)
                .unwrap()
                .prev_id,
            None
        );
    }

    #[test]
    fn list_exposes_prev_id_so_the_timeline_can_detect_a_pruned_gap() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let make = |n: u64| {
            let file = ws.path.join(format!("f{n}.txt"));
            std::fs::write(&file, "x").unwrap();
            let id = begin_impl(
                &state,
                &base.path,
                "s1".to_string(),
                0,
                "p".to_string(),
                None,
            )
            .unwrap();
            record_original(&state, Some(&id), &file).unwrap();
            end_impl(&state, &id).unwrap();
            id
        };
        let id_a = make(0);
        let id_b = make(1);
        let id_c = make(2);

        // Simulate B being pruned (or otherwise removed) independently of A/C.
        std::fs::remove_dir_all(base.path.join(&id_b)).unwrap();

        let infos = list_impl(&base.path, Some("s1")).unwrap();
        assert_eq!(infos.len(), 2, "B must no longer be listed");
        // Newest-first: C, then A.
        assert_eq!(infos[0].id, id_c);
        assert_eq!(infos[1].id, id_a);
        // C's recorded predecessor (B) doesn't match the next surviving
        // entry (A) — that mismatch is exactly the pruned-gap signal the
        // timeline's "Restore to here" must key off of.
        assert_eq!(
            infos[0].prev_id,
            Some(id_b),
            "C's prev_id still points at the pruned B"
        );
        assert_ne!(
            infos[0].prev_id,
            Some(infos[1].id.clone()),
            "mismatch signals a gap"
        );
    }

    #[test]
    fn acquire_revert_lock_rejects_a_second_concurrent_claim() {
        let state = AppState::default();
        let id = "some-checkpoint-id";

        let guard = acquire_revert_lock(&state, id).unwrap();
        let err = match acquire_revert_lock(&state, id) {
            Ok(_) => panic!("a second concurrent claim on the same id must be rejected"),
            Err(e) => e,
        };
        assert!(
            err.contains("already being restored"),
            "unexpected error: {err}"
        );

        drop(guard);
        // Once released, a new claim must succeed.
        assert!(acquire_revert_lock(&state, id).is_ok());
    }

    #[test]
    fn reapply_rejects_traversal_ids() {
        let base = TempDir::new("base");
        let err = reapply_impl(&base.path, "../outside").unwrap_err();
        assert!(
            err.contains("Invalid checkpoint id"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn reapply_errors_for_unknown_id() {
        let base = TempDir::new("base");
        let err = reapply_impl(&base.path, "0000-does-not-exist").unwrap_err();
        assert!(err.contains("not found"), "unexpected error: {err}");
    }

    #[test]
    fn list_returns_newest_first_and_skips_in_flight_checkpoints() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let file_a = ws.path.join("a.txt");
        std::fs::write(&file_a, "a").unwrap();
        let id_older = begin_impl(
            &state,
            &base.path,
            "s1".to_string(),
            0,
            "older turn".to_string(),
            None,
        )
        .unwrap();
        record_original(&state, Some(&id_older), &file_a).unwrap();
        end_impl(&state, &id_older).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(5));

        let file_b = ws.path.join("b.txt");
        std::fs::write(&file_b, "b").unwrap();
        let id_newer = begin_impl(
            &state,
            &base.path,
            "s1".to_string(),
            1,
            "newer turn".to_string(),
            None,
        )
        .unwrap();
        record_original(&state, Some(&id_newer), &file_b).unwrap();
        end_impl(&state, &id_newer).unwrap();

        // A checkpoint whose turn is still running (no `checkpoint_end` yet,
        // so no manifest.json on disk) must not appear in the list.
        let _in_flight = begin_impl(
            &state,
            &base.path,
            "s1".to_string(),
            2,
            "still running".to_string(),
            None,
        )
        .unwrap();

        let infos = list_impl(&base.path, None).unwrap();
        assert_eq!(
            infos.len(),
            2,
            "in-flight checkpoint must be skipped: {:?}",
            infos.iter().map(|i| &i.id).collect::<Vec<_>>()
        );
        assert_eq!(infos[0].id, id_newer, "newest checkpoint must sort first");
        assert_eq!(infos[1].id, id_older);
        assert_eq!(infos[0].files, 1);
        assert_eq!(infos[0].label, "newer turn");
        assert!(!infos[0].reverted);
        assert!(!infos[0].reapplyable);
    }

    #[test]
    fn list_filters_by_session_id() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let file_a = ws.path.join("a.txt");
        std::fs::write(&file_a, "a").unwrap();
        let id_s1 = begin_impl(
            &state,
            &base.path,
            "session-1".to_string(),
            0,
            "s1 turn".to_string(),
            None,
        )
        .unwrap();
        record_original(&state, Some(&id_s1), &file_a).unwrap();
        end_impl(&state, &id_s1).unwrap();

        let file_b = ws.path.join("b.txt");
        std::fs::write(&file_b, "b").unwrap();
        let id_s2 = begin_impl(
            &state,
            &base.path,
            "session-2".to_string(),
            0,
            "s2 turn".to_string(),
            None,
        )
        .unwrap();
        record_original(&state, Some(&id_s2), &file_b).unwrap();
        end_impl(&state, &id_s2).unwrap();

        let all = list_impl(&base.path, None).unwrap();
        assert_eq!(all.len(), 2);

        let filtered = list_impl(&base.path, Some("session-1")).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, id_s1);
        assert_eq!(filtered[0].session_id, "session-1");
    }

    #[test]
    fn list_includes_v1_manifests_and_marks_reapplyable_after_revert() {
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let existing = ws.path.join("existing.txt");
        std::fs::write(&existing, "mutated").unwrap();

        // A v1 (bare-array) manifest on disk, same as `v1_manifest_on_disk_still_reverts`.
        let v1_id = "00000000-0000-4000-8000-00000000v1lst";
        let v1_dir = base.path.join(v1_id);
        std::fs::create_dir_all(&v1_dir).unwrap();
        std::fs::write(v1_dir.join("0.bak"), "original").unwrap();
        let raw = format!(
            r#"[{{"path":{:?},"backup":"0.bak"}}]"#,
            existing.to_string_lossy()
        );
        std::fs::write(v1_dir.join(MANIFEST_FILE), raw).unwrap();

        let before_revert = list_impl(&base.path, None).unwrap();
        assert_eq!(before_revert.len(), 1);
        assert_eq!(
            before_revert[0].session_id, "",
            "v1 manifests synthesize an empty session id"
        );
        assert!(!before_revert[0].reverted);
        assert!(
            !before_revert[0].reapplyable,
            "not reverted yet, so nothing to re-apply"
        );

        revert_impl(&base.path, v1_id).unwrap();

        let after_revert = list_impl(&base.path, None).unwrap();
        assert_eq!(after_revert.len(), 1);
        assert!(after_revert[0].reverted);
        assert!(
            after_revert[0].reapplyable,
            "revert_impl recorded a redo backup, so re-apply is now meaningful"
        );
    }

    /// Reproduces (and pins the fix for) the concurrent-write race a code
    /// review flagged against `tool_write_file`/`tool_edit_file`: two
    /// `code`-profile subagents (or any two concurrent mutating tool calls)
    /// resolving to the SAME workspace path can, without serialization,
    /// interleave past `record_original`'s dedup and each other's
    /// `std::fs::write`, silently discarding one write. This mirrors the
    /// review's own throwaway repro exactly — `record_original` followed by
    /// a forced yield (standing in for `request_permission`'s real `.await`)
    /// followed by `std::fs::write` — but now wrapped in `AppState::
    /// file_write_lock` (see `tool_write_file`'s doc comment), the same lock
    /// `tools.rs` now acquires around its own backup+write critical section.
    /// Run across many trials on a genuinely multi-threaded runtime (real OS
    /// parallelism, not just cooperative interleaving) so a regression that
    /// reintroduces the race would show up as either a corrupted/mixed file
    /// or more than one recorded checkpoint entry for the same path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_writes_to_the_same_path_are_serialized_by_file_write_lock() {
        use std::sync::Arc;

        for _ in 0..50 {
            let base = TempDir::new("base");
            let ws = TempDir::new("ws");
            let file = ws.path.join("shared.txt");
            std::fs::write(&file, "original").unwrap();

            let state = Arc::new(AppState::default());
            let checkpoint_id = begin(&state, &base.path);

            let run = |state: Arc<AppState>,
                       checkpoint_id: String,
                       file: PathBuf,
                       content: &'static str| {
                tokio::spawn(async move {
                    // Force genuine interleaving opportunity across the two
                    // tasks before either takes the lock — standing in for
                    // `request_permission`'s real `.await` between path
                    // resolution and the backup+write critical section.
                    tokio::task::yield_now().await;

                    let _guard = state.file_write_lock.lock().unwrap();
                    record_original(&state, Some(&checkpoint_id), &file).unwrap();
                    std::fs::write(&file, content).unwrap();
                })
            };

            let a = run(
                state.clone(),
                checkpoint_id.clone(),
                file.clone(),
                "from writer A",
            );
            let b = run(
                state.clone(),
                checkpoint_id.clone(),
                file.clone(),
                "from writer B",
            );
            a.await.unwrap();
            b.await.unwrap();

            let final_content = std::fs::read_to_string(&file).unwrap();
            assert!(
                final_content == "from writer A" || final_content == "from writer B",
                "file content was corrupted/interleaved rather than a clean win by one writer: {final_content:?}"
            );

            // Only the FIRST writer's pre-mutation backup should ever be
            // recorded for this path — serialization means the second
            // writer's `record_original` call always sees an existing entry
            // and skips (this dedup was always mutex-protected; the fix is
            // that the two writers' backup+write pairs can no longer
            // interleave with each other).
            let entries = state.checkpoints.lock().unwrap()[&checkpoint_id]
                .entries
                .clone();
            let file_str = file.to_string_lossy().to_string();
            assert_eq!(entries.iter().filter(|e| e.path == file_str).count(), 1);
        }
    }

    // -----------------------------------------------------------------
    // diff_lines
    // -----------------------------------------------------------------

    #[test]
    fn diff_lines_reports_no_changes_for_identical_text() {
        let result = diff_lines("a\nb\nc", "a\nb\nc");
        assert_eq!(result.added, 0);
        assert_eq!(result.removed, 0);
        assert!(!result.truncated);
        assert!(result.lines.iter().all(|l| l.kind == DiffLineKind::Context));
    }

    #[test]
    fn diff_lines_detects_a_pure_addition() {
        let result = diff_lines("a\nb", "a\nb\nc");
        assert_eq!(result.added, 1);
        assert_eq!(result.removed, 0);
        assert_eq!(
            result.lines.last(),
            Some(&DiffLine {
                kind: DiffLineKind::Added,
                text: "c".to_string()
            })
        );
    }

    #[test]
    fn diff_lines_detects_a_pure_removal() {
        let result = diff_lines("a\nb\nc", "a\nc");
        assert_eq!(result.added, 0);
        assert_eq!(result.removed, 1);
        assert!(result
            .lines
            .iter()
            .any(|l| l.kind == DiffLineKind::Removed && l.text == "b"));
    }

    #[test]
    fn diff_lines_detects_a_modification_as_remove_plus_add() {
        let result = diff_lines("hello world", "hello there");
        assert_eq!(result.removed, 1);
        assert_eq!(result.added, 1);
    }

    #[test]
    fn diff_lines_treats_empty_before_as_entirely_added() {
        let result = diff_lines("", "x\ny");
        assert_eq!(result.added, 2);
        assert_eq!(result.removed, 0);
    }

    #[test]
    fn diff_lines_treats_empty_after_as_entirely_removed() {
        let result = diff_lines("x\ny", "");
        assert_eq!(result.added, 0);
        assert_eq!(result.removed, 2);
    }

    #[test]
    fn diff_lines_is_a_noop_for_two_empty_strings() {
        let result = diff_lines("", "");
        assert_eq!(result.added, 0);
        assert_eq!(result.removed, 0);
        assert!(result.lines.is_empty());
    }

    #[test]
    fn diff_lines_truncates_when_the_input_is_too_large_to_diff_cheaply() {
        // Comfortably over MAX_DIFF_CELLS (4,000,000): 2100 * 2100 > 4.4M.
        let before = (0..2100)
            .map(|i| format!("before-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let after = (0..2100)
            .map(|i| format!("after-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = diff_lines(&before, &after);
        assert!(
            result.truncated,
            "oversized diff must report truncated: true"
        );
        assert!(result.lines.is_empty());
    }

    // -----------------------------------------------------------------
    // preview_impl / compare_impl
    // -----------------------------------------------------------------

    #[test]
    fn preview_reports_modified_added_and_deleted_files_with_exact_after_snapshots() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let modified = ws.path.join("modified.txt");
        std::fs::write(&modified, "line1\nline2\nline3").unwrap();
        let created = ws.path.join("created.txt");
        let deleted = ws.path.join("deleted.txt");
        std::fs::write(&deleted, "goodbye").unwrap();

        let id = begin(&state, &base.path);
        record_original(&state, Some(&id), &modified).unwrap();
        std::fs::write(&modified, "line1\nCHANGED\nline3").unwrap();
        record_original(&state, Some(&id), &created).unwrap();
        std::fs::write(&created, "brand new").unwrap();
        record_original(&state, Some(&id), &deleted).unwrap();
        std::fs::remove_file(&deleted).unwrap(); // simulates a shell-driven delete mid-turn
        end_impl(&state, &id).unwrap();

        let preview = preview_impl(&base.path, &id).unwrap();
        assert_eq!(preview.files.len(), 3);

        let modified_entry = preview
            .files
            .iter()
            .find(|f| f.path.ends_with("modified.txt"))
            .unwrap();
        assert_eq!(modified_entry.status, FileChangeStatus::Modified);
        assert_eq!(modified_entry.after_source, SnapshotSource::Captured);
        let diff = modified_entry.diff.as_ref().unwrap();
        assert_eq!(diff.added, 1);
        assert_eq!(diff.removed, 1);

        let created_entry = preview
            .files
            .iter()
            .find(|f| f.path.ends_with("created.txt"))
            .unwrap();
        assert_eq!(created_entry.status, FileChangeStatus::Added);
        assert_eq!(created_entry.after_source, SnapshotSource::Captured);
        assert_eq!(created_entry.diff.as_ref().unwrap().added, 1);

        let deleted_entry = preview
            .files
            .iter()
            .find(|f| f.path.ends_with("deleted.txt"))
            .unwrap();
        assert_eq!(
            deleted_entry.status,
            FileChangeStatus::Deleted,
            "a file gone by checkpoint_end time must be confidently reported as Deleted, not guessed at"
        );
        assert_eq!(deleted_entry.after_source, SnapshotSource::Captured);
    }

    #[test]
    fn preview_reports_unchanged_when_content_is_identical() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let file = ws.path.join("same.txt");
        std::fs::write(&file, "same content").unwrap();

        let id = begin(&state, &base.path);
        record_original(&state, Some(&id), &file).unwrap();
        // Rewritten with the exact same bytes — a no-op mutation.
        std::fs::write(&file, "same content").unwrap();
        end_impl(&state, &id).unwrap();

        let preview = preview_impl(&base.path, &id).unwrap();
        assert_eq!(preview.files[0].status, FileChangeStatus::Unchanged);
    }

    #[test]
    fn preview_falls_back_to_live_file_for_a_pre_v3_manifest_never_reverted() {
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let file = ws.path.join("legacy.txt");
        std::fs::write(&file, "post-turn content").unwrap();

        // Hand-written v2 manifest with no "after" key at all, mirroring a
        // checkpoint written before this feature existed.
        let id = "00000000-0000-4000-8000-00000000v2prev";
        let dir = base.path.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("0.bak"), "pre-turn content").unwrap();
        let manifest = format!(
            r#"{{"version":2,"created_at_ms":1,"session_id":"s","anchor_index":0,"label":"l","shell_ran":false,"reverted":false,"prev_id":null,"entries":[{{"path":{:?},"backup":"0.bak"}}]}}"#,
            file.to_string_lossy()
        );
        std::fs::write(dir.join(MANIFEST_FILE), manifest).unwrap();

        let preview = preview_impl(&base.path, id).unwrap();
        assert_eq!(preview.files.len(), 1);
        assert_eq!(preview.files[0].after_source, SnapshotSource::Live);
        assert_eq!(preview.files[0].status, FileChangeStatus::Modified);
    }

    #[test]
    fn preview_reports_unknown_rather_than_guessing_when_nothing_is_available() {
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");
        // File never actually created on disk anywhere — no live fallback,
        // no captured/redo snapshot either (hand-written v1-shaped entry).
        let missing = ws.path.join("never_existed.txt");

        let id = "00000000-0000-4000-8000-00000000nofil";
        let dir = base.path.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let raw = format!(
            r#"[{{"path":{:?},"backup":null}}]"#,
            missing.to_string_lossy()
        );
        std::fs::write(dir.join(MANIFEST_FILE), raw).unwrap();

        let preview = preview_impl(&base.path, id).unwrap();
        assert_eq!(preview.files[0].status, FileChangeStatus::Unknown);
        assert!(preview.files[0].diff.is_none());
    }

    #[test]
    fn compare_returns_the_union_of_files_touched_by_either_checkpoint() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let only_a = ws.path.join("only_a.txt");
        std::fs::write(&only_a, "a-content").unwrap();
        let shared = ws.path.join("shared.txt");
        std::fs::write(&shared, "v1").unwrap();

        let id_a = begin(&state, &base.path);
        record_original(&state, Some(&id_a), &only_a).unwrap();
        std::fs::write(&only_a, "a-mutated").unwrap();
        record_original(&state, Some(&id_a), &shared).unwrap();
        std::fs::write(&shared, "v2").unwrap();
        end_impl(&state, &id_a).unwrap();

        let only_b = ws.path.join("only_b.txt");
        std::fs::write(&only_b, "b-content").unwrap();
        let id_b = begin(&state, &base.path);
        record_original(&state, Some(&id_b), &shared).unwrap();
        std::fs::write(&shared, "v3").unwrap();
        record_original(&state, Some(&id_b), &only_b).unwrap();
        std::fs::write(&only_b, "b-mutated").unwrap();
        end_impl(&state, &id_b).unwrap();

        let compare = compare_impl(&base.path, &id_a, &id_b).unwrap();
        assert_eq!(
            compare.files.len(),
            3,
            "must be the union, not intersection, of touched files"
        );

        let only_a_entry = compare
            .files
            .iter()
            .find(|f| f.path.ends_with("only_a.txt"))
            .unwrap();
        assert!(only_a_entry.in_a && !only_a_entry.in_b);

        let only_b_entry = compare
            .files
            .iter()
            .find(|f| f.path.ends_with("only_b.txt"))
            .unwrap();
        assert!(!only_b_entry.in_a && only_b_entry.in_b);

        let shared_entry = compare
            .files
            .iter()
            .find(|f| f.path.ends_with("shared.txt"))
            .unwrap();
        assert!(shared_entry.in_a && shared_entry.in_b);
        let between = shared_entry
            .between
            .as_ref()
            .expect("both sides have text content");
        // A's resulting content was "v2", B's was "v3" — one line differs.
        assert_eq!(between.added, 1);
        assert_eq!(between.removed, 1);
    }

    // -----------------------------------------------------------------
    // simulate_restore_impl
    // -----------------------------------------------------------------

    #[test]
    fn simulate_restore_plans_a_restore_and_a_delete_with_no_drift() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let existing = ws.path.join("existing.txt");
        std::fs::write(&existing, "original").unwrap();
        let created = ws.path.join("created.txt");

        let id = begin(&state, &base.path);
        record_original(&state, Some(&id), &existing).unwrap();
        std::fs::write(&existing, "mutated").unwrap();
        record_original(&state, Some(&id), &created).unwrap();
        std::fs::write(&created, "brand new").unwrap();
        end_impl(&state, &id).unwrap();

        let sim = simulate_restore_impl(&base.path, &id).unwrap();
        assert!(!sim.already_reverted);
        assert!(!sim.needs_reconciliation, "no shell command ran");

        let existing_plan = sim
            .files
            .iter()
            .find(|f| f.path.ends_with("existing.txt"))
            .unwrap();
        assert_eq!(existing_plan.action, RestoreAction::Restore);
        assert!(
            !existing_plan.drifted,
            "nothing touched the file since the turn ended"
        );

        let created_plan = sim
            .files
            .iter()
            .find(|f| f.path.ends_with("created.txt"))
            .unwrap();
        assert_eq!(created_plan.action, RestoreAction::Delete);
        assert!(!created_plan.drifted);
    }

    #[test]
    fn simulate_restore_reports_no_op_when_the_live_file_already_matches_before() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let file = ws.path.join("f.txt");
        std::fs::write(&file, "original").unwrap();

        let id = begin(&state, &base.path);
        record_original(&state, Some(&id), &file).unwrap();
        std::fs::write(&file, "mutated").unwrap();
        end_impl(&state, &id).unwrap();

        // Something (e.g. the user, or an earlier manual edit) already put
        // the file back to its pre-turn content before the simulation runs.
        std::fs::write(&file, "original").unwrap();

        let sim = simulate_restore_impl(&base.path, &id).unwrap();
        let plan = sim
            .files
            .iter()
            .find(|f| f.path.ends_with("f.txt"))
            .unwrap();
        assert_eq!(plan.action, RestoreAction::NoOp);
    }

    #[test]
    fn simulate_restore_flags_drift_when_something_else_touched_the_file_since() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let file = ws.path.join("f.txt");
        std::fs::write(&file, "original").unwrap();

        let id = begin(&state, &base.path);
        record_original(&state, Some(&id), &file).unwrap();
        std::fs::write(&file, "this-turns-content").unwrap();
        end_impl(&state, &id).unwrap();

        // A DIFFERENT, later change (another turn, or a manual edit) landed
        // on top of this turn's content before the simulation runs.
        std::fs::write(&file, "someone-elses-later-edit").unwrap();

        let sim = simulate_restore_impl(&base.path, &id).unwrap();
        let plan = sim
            .files
            .iter()
            .find(|f| f.path.ends_with("f.txt"))
            .unwrap();
        assert_eq!(plan.action, RestoreAction::Restore);
        assert!(
            plan.drifted,
            "live content no longer matches what this turn produced, so revert would also discard the later edit"
        );
    }

    /// The bug the enumerated set exists for: a turn whose only external
    /// effect was a network call used to report "nothing to reconcile".
    ///
    /// `needs_reconciliation` was `manifest.shell_ran`, and the finer detail
    /// lived only in the transcript — which `contextTrimmer.ts` is allowed to
    /// drop. So after a compaction the rollback simulation, the one surface
    /// that survives, actively said the revert was complete.
    #[test]
    fn a_non_shell_effect_is_enumerated_and_still_needs_reconciliation() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let file = ws.path.join("f.txt");
        std::fs::write(&file, "v1").unwrap();

        let id = begin(&state, &base.path);
        record_original(&state, Some(&id), &file).unwrap();
        std::fs::write(&file, "v2").unwrap();
        record_external_effect(&state, Some(&id), ExternalEffectKind::Network).unwrap();
        record_external_effect(&state, Some(&id), ExternalEffectKind::McpTool).unwrap();
        // Twice, to pin that the set deduplicates rather than growing per call.
        record_external_effect(&state, Some(&id), ExternalEffectKind::Network).unwrap();
        end_impl(&state, &id).unwrap();

        let sim = simulate_restore_impl(&base.path, &id).unwrap();
        assert!(
            sim.needs_reconciliation,
            "an already-sent request cannot be un-sent, whether or not a shell also ran"
        );
        assert_eq!(
            sim.external_effects
                .iter()
                .map(|effect| effect.kind)
                .collect::<Vec<_>>(),
            vec![ExternalEffectKind::Network, ExternalEffectKind::McpTool],
            "each kind appears once, in the enum's own order"
        );
        for effect in &sim.external_effects {
            match effect.compensation {
                Compensation::None { reason } => assert!(
                    !reason.is_empty(),
                    "an effect with no compensator must say why, not just that"
                ),
                Compensation::Undo { action } => assert!(
                    !action.is_empty(),
                    "a compensator must name what reverting will do"
                ),
            }
        }
        assert!(
            !read_manifest(&base.path, &id).unwrap().shell_ran,
            "no shell ran, and recording a network call must not claim one did"
        );
    }

    /// K14's second half: declaring is not the same claim as completing.
    ///
    /// Every declaration is written before the call, deliberately, so a request
    /// that was permitted and then failed is still recorded. That means the list
    /// alone cannot separate "the server ran this" from "we cancelled before it
    /// could" — and reverting a turn wants to know which.
    #[test]
    fn an_effect_that_completed_is_distinguished_from_one_that_only_started() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        // A touched file, because `checkpoint_end` discards a checkpoint that
        // recorded nothing — an effect alone does not keep one alive.
        let file = ws.path.join("f.txt");
        std::fs::write(&file, "v1").unwrap();
        let id = begin(&state, &base.path);
        record_original(&state, Some(&id), &file).unwrap();
        std::fs::write(&file, "v2").unwrap();
        // Network: declared and then watched to finish.
        commit_external_effect(&state, Some(&id), ExternalEffectKind::Network).unwrap();
        // MCP: declared, and the reply never came.
        record_external_effect(&state, Some(&id), ExternalEffectKind::McpTool).unwrap();
        end_impl(&state, &id).unwrap();

        let sim = simulate_restore_impl(&base.path, &id).unwrap();
        let status = |kind| {
            sim.external_effects
                .iter()
                .find(|effect| effect.kind == kind)
                .expect("effect recorded")
                .status
        };
        assert_eq!(status(ExternalEffectKind::Network), EffectStatus::Committed);
        assert_eq!(status(ExternalEffectKind::McpTool), EffectStatus::Declared);
        assert!(
            sim.needs_reconciliation,
            "an unfinished call is still a call that may have landed — the status \
             informs the reader, it does not excuse the effect"
        );
    }

    /// Committing implies declaring, so the two lists cannot drift apart.
    ///
    /// A caller that only ever reaches the success path — one with nothing to
    /// declare *before*, should such a tool ever exist — must not produce a
    /// manifest whose committed set names a kind the effect list has never
    /// heard of, because every reader iterates the declarations.
    #[test]
    fn committing_an_effect_records_it_even_if_nothing_declared_it_first() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let file = ws.path.join("f.txt");
        std::fs::write(&file, "v1").unwrap();
        let id = begin(&state, &base.path);
        record_original(&state, Some(&id), &file).unwrap();
        std::fs::write(&file, "v2").unwrap();
        commit_external_effect(&state, Some(&id), ExternalEffectKind::Shell).unwrap();
        end_impl(&state, &id).unwrap();

        let manifest = read_manifest(&base.path, &id).unwrap();
        assert_eq!(manifest.external_effects, vec![ExternalEffectKind::Shell]);
        assert_eq!(
            manifest.committed_effects,
            Some(vec![ExternalEffectKind::Shell])
        );
        assert!(
            manifest.shell_ran,
            "the flag every older reader asks for by name is kept in step by the \
             declaration that committing performs"
        );
    }

    /// A manifest from before the commit phase says nothing either way, and
    /// that is not the same as saying nothing completed.
    ///
    /// Without the distinction, every checkpoint written before this change
    /// would report its shell command as "started, never seen to finish" — a
    /// downgrade invented by the reader rather than recorded by the writer.
    #[test]
    fn a_manifest_without_the_commit_phase_reports_an_unobserved_outcome() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let file = ws.path.join("f.txt");
        std::fs::write(&file, "v1").unwrap();
        let id = begin(&state, &base.path);
        record_original(&state, Some(&id), &file).unwrap();
        std::fs::write(&file, "v2").unwrap();
        commit_external_effect(&state, Some(&id), ExternalEffectKind::Shell).unwrap();
        end_impl(&state, &id).unwrap();

        let manifest_path = base.path.join(&id).join("manifest.json");
        let raw = std::fs::read_to_string(&manifest_path).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        value.as_object_mut().unwrap().remove("committed_effects");
        std::fs::write(&manifest_path, serde_json::to_string(&value).unwrap()).unwrap();

        let sim = simulate_restore_impl(&base.path, &id).unwrap();
        assert_eq!(
            sim.external_effects
                .iter()
                .map(|effect| effect.status)
                .collect::<Vec<_>>(),
            vec![EffectStatus::Unobserved]
        );
    }

    /// A manifest written before the column existed still reports what it can.
    #[test]
    fn a_manifest_without_the_effect_list_recovers_shell_from_the_old_flag() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let file = ws.path.join("f.txt");
        std::fs::write(&file, "v1").unwrap();
        let id = begin(&state, &base.path);
        record_original(&state, Some(&id), &file).unwrap();
        std::fs::write(&file, "v2").unwrap();
        record_shell(&state, Some(&id)).unwrap();
        end_impl(&state, &id).unwrap();

        // Wind the manifest back to what a build before this change wrote: the
        // flag, and no list.
        let manifest_path = base.path.join(&id).join("manifest.json");
        let raw = std::fs::read_to_string(&manifest_path).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        value.as_object_mut().unwrap().remove("external_effects");
        std::fs::write(&manifest_path, serde_json::to_string(&value).unwrap()).unwrap();

        let sim = simulate_restore_impl(&base.path, &id).unwrap();
        assert_eq!(
            sim.external_effects
                .iter()
                .map(|effect| effect.kind)
                .collect::<Vec<_>>(),
            vec![ExternalEffectKind::Shell],
            "the one signal an older manifest carries must still be reported"
        );
        assert!(sim.needs_reconciliation);
    }

    #[test]
    fn simulate_restore_flags_needs_reconciliation_when_a_shell_command_ran() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let file = ws.path.join("f.txt");
        std::fs::write(&file, "v1").unwrap();

        let id = begin(&state, &base.path);
        record_original(&state, Some(&id), &file).unwrap();
        std::fs::write(&file, "v2").unwrap();
        record_shell(&state, Some(&id)).unwrap();
        end_impl(&state, &id).unwrap();

        let sim = simulate_restore_impl(&base.path, &id).unwrap();
        assert!(
            sim.needs_reconciliation,
            "a shell command's side effects can't be undone by file restore and must be flagged"
        );
    }

    #[test]
    fn simulate_restore_on_an_already_reverted_checkpoint_reports_no_planned_changes() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let file = ws.path.join("f.txt");
        std::fs::write(&file, "v1").unwrap();

        let id = begin(&state, &base.path);
        record_original(&state, Some(&id), &file).unwrap();
        std::fs::write(&file, "v2").unwrap();
        end_impl(&state, &id).unwrap();
        revert_impl(&base.path, &id).unwrap();

        let sim = simulate_restore_impl(&base.path, &id).unwrap();
        assert!(sim.already_reverted);
        assert!(sim.files.is_empty());
    }

    /// Pins the exact JSON wire format every enum in this module serializes
    /// to — the frontend (`src/lib/checkpointPreview.ts`) hand-maintains
    /// matching string-literal union types with no shared codegen, so a
    /// silent drift here (e.g. `RestoreAction::NoOp` serializing to anything
    /// other than `"noOp"`, the least obvious of these under
    /// `rename_all = "camelCase"`) would desync the two without either side
    /// failing to compile.
    #[test]
    fn enum_wire_format_matches_the_hand_maintained_frontend_types() {
        assert_eq!(
            serde_json::to_string(&DiffLineKind::Context).unwrap(),
            "\"context\""
        );
        assert_eq!(
            serde_json::to_string(&DiffLineKind::Added).unwrap(),
            "\"added\""
        );
        assert_eq!(
            serde_json::to_string(&DiffLineKind::Removed).unwrap(),
            "\"removed\""
        );

        assert_eq!(
            serde_json::to_string(&SnapshotSource::Captured).unwrap(),
            "\"captured\""
        );
        assert_eq!(
            serde_json::to_string(&SnapshotSource::Redo).unwrap(),
            "\"redo\""
        );
        assert_eq!(
            serde_json::to_string(&SnapshotSource::Live).unwrap(),
            "\"live\""
        );
        assert_eq!(
            serde_json::to_string(&SnapshotSource::Unavailable).unwrap(),
            "\"unavailable\""
        );

        assert_eq!(
            serde_json::to_string(&FileChangeStatus::Added).unwrap(),
            "\"added\""
        );
        assert_eq!(
            serde_json::to_string(&FileChangeStatus::Modified).unwrap(),
            "\"modified\""
        );
        assert_eq!(
            serde_json::to_string(&FileChangeStatus::Deleted).unwrap(),
            "\"deleted\""
        );
        assert_eq!(
            serde_json::to_string(&FileChangeStatus::Unchanged).unwrap(),
            "\"unchanged\""
        );
        assert_eq!(
            serde_json::to_string(&FileChangeStatus::Unknown).unwrap(),
            "\"unknown\""
        );

        assert_eq!(
            serde_json::to_string(&RestoreAction::Restore).unwrap(),
            "\"restore\""
        );
        assert_eq!(
            serde_json::to_string(&RestoreAction::Delete).unwrap(),
            "\"delete\""
        );
        assert_eq!(
            serde_json::to_string(&RestoreAction::NoOp).unwrap(),
            "\"noOp\"",
            "the two-word Rust variant NoOp must serialize to camelCase noOp, not noop or no_op"
        );

        assert_eq!(
            serde_json::to_string(&EffectStatus::Committed).unwrap(),
            "\"committed\""
        );
        assert_eq!(
            serde_json::to_string(&EffectStatus::Declared).unwrap(),
            "\"declared\""
        );
        assert_eq!(
            serde_json::to_string(&EffectStatus::Unobserved).unwrap(),
            "\"unobserved\""
        );
    }
}

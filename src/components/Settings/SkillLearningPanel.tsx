import { useCallback, useEffect, useState } from "react";
import {
  Brain,
  CheckCircle2,
  FlaskConical,
  Loader2,
  RefreshCw,
  RotateCcw,
  ShieldAlert,
  Trash2,
  Wand2,
  XCircle,
} from "lucide-react";

import {
  skillLearningClient,
  type EvaluationRecord,
  type ImprovementEvidence,
  type LearnedSkillSummary,
  type LearningCandidate,
  type LearningPolicy,
  type LearningSettings,
  type LearningSourceKind,
  type RunEvidence,
  type SkillQualityState,
} from "../../lib/skillLearningClient";
import { nativeSkillsClient, type NativeSkillScope } from "../../lib/nativeSkillsClient";
import { runCandidateEvaluation } from "../../lib/skillLearningEval";
import { draftCandidate } from "../../lib/skillLearningReflection";
import { useNativeSkillsStore } from "../../store/nativeSkillsStore";
import { useSkillLearningFocusStore } from "../../store/skillLearningFocusStore";
import { Button } from "../ui";
import { errorMessage } from "../../lib/errors";

const POLICY_LABELS: Array<{ value: LearningPolicy; label: string; detail: string }> = [
  {
    value: "manual",
    label: "Manual",
    detail: "Automatic candidates are disabled. You can still explicitly save a completed run as a skill.",
  },
  {
    value: "ask",
    label: "Ask",
    detail:
      "Signals are recorded and listed below. A draft is written when you ask for one — or straight away when you explicitly asked the agent to learn a procedure. Nothing installs without your approval.",
  },
  {
    value: "automatic",
    label: "Automatic",
    detail:
      "Additionally installs a candidate that passed a real isolated evaluation, adds no tool access, and declares no new executables or environment variables. Anything else still waits for you.",
  },
];

const SOURCE_LABELS: Record<LearningSourceKind, string> = {
  explicit_user_instruction: "you asked for it",
  manual_run_capture: "you saved the run",
  manual_improvement: "you requested an evidence-backed improvement",
  user_correction: "your correction verified",
  verification_repair: "verification repair",
  successful_novel_procedure: "verified procedure",
  repeated_failure_resolution: "recurring failure resolved",
};

function shortHash(value: string | null | undefined): string {
  return value ? `${value.slice(0, 12)}…` : "—";
}

type CandidateDraft = {
  id: string;
  scope: NativeSkillScope;
  title: string;
  description: string;
  command: string;
  content: string;
  allowedTools: string;
  bins: string;
  env: string;
};

function listField(values: string[]): string {
  return values.join(", ");
}

function parseList(value: string): string[] {
  return value
    .split(/[\n,]/)
    .map((entry) => entry.trim())
    .filter(Boolean);
}

function addedValues(before: string[], after: string[]): string[] {
  const previous = new Set(before);
  return after.filter((value, index) => after.indexOf(value) === index && !previous.has(value));
}

function valuesChanged(before: string[], after: string[]): boolean {
  return before.length !== after.length || before.some((value) => !after.includes(value)) || after.some((value) => !before.includes(value));
}

function resourcePaths(resources: Array<{ path: string }> | undefined): string[] {
  return resources?.map((resource) => resource.path) ?? [];
}

function qualityLabel(state: SkillQualityState): string {
  if (state === "needs_attention") return "Needs attention";
  if (state === "healthy") return "Healthy";
  return "Not enough data";
}

function qualityClass(state: SkillQualityState): string {
  if (state === "needs_attention") return "bg-danger-soft text-danger";
  if (state === "healthy") return "bg-success-soft text-success";
  return "bg-warning-soft text-warning";
}

function boundedDiff(before: string, after: string): Array<{ kind: "context" | "removed" | "added"; text: string }> {
  const left = before.split("\n");
  const right = after.split("\n");
  const lines: Array<{ kind: "context" | "removed" | "added"; text: string }> = [];
  let i = 0;
  let j = 0;
  while (i < left.length || j < right.length) {
    if (left[i] === right[j]) {
      if (i < left.length) lines.push({ kind: "context", text: left[i] });
      i += 1;
      j += 1;
    } else {
      if (i < left.length) lines.push({ kind: "removed", text: left[i++] });
      if (j < right.length) lines.push({ kind: "added", text: right[j++] });
    }
    if (lines.length >= 240) {
      lines.push({ kind: "context", text: "… diff truncated …" });
      break;
    }
  }
  return lines;
}

function candidateDraft(candidate: LearningCandidate): CandidateDraft {
  return {
    id: candidate.candidate_id,
    scope: candidate.scope,
    title: candidate.title,
    description: candidate.description,
    command: candidate.proposed_command,
    content: candidate.proposed_skill_content,
    allowedTools: listField(candidate.allowed_tools),
    bins: listField(candidate.requirements.bins),
    env: listField(candidate.requirements.env),
  };
}

/** The candidate's own permission surface, spelled out rather than hidden
 * behind a generic Install button — a candidate that widens tool access or
 * needs a new executable has to say so on the button's own card. */
function PolicyNotes({ candidate }: { candidate: LearningCandidate }) {
  const policy = candidate.policy;
  if (!policy) return null;
  return (
    <>
      {policy.blocking.length > 0 && (
        <p className="mt-1 flex items-start gap-1 text-danger">
          <ShieldAlert size={12} className="mt-0.5 shrink-0" />
          <span>Refused: {policy.blocking.join("; ")}</span>
        </p>
      )}
      {policy.approval_reasons.length > 0 && (
        <p className="mt-1 text-warning">Needs your approval because {policy.approval_reasons.join("; ")}.</p>
      )}
    </>
  );
}

export function SkillLearningPanel() {
  const [settings, setSettings] = useState<LearningSettings>({ policy: "ask", allow_global_scope: true });
  const [candidates, setCandidates] = useState<LearningCandidate[]>([]);
  const [learned, setLearned] = useState<LearnedSkillSummary[]>([]);
  const [evaluations, setEvaluations] = useState<Record<string, EvaluationRecord[]>>({});
  const [expanded, setExpanded] = useState<string | null>(null);
  const [draft, setDraft] = useState<CandidateDraft | null>(null);
  /** The instructions each installed learned skill carries right now, keyed by
   * command — the "before" half of an update candidate's diff. Loaded from the
   * same discovery the native skill list uses, so it is what a future run
   * would actually read, not a stored copy. */
  const [improvementEvidence, setImprovementEvidence] = useState<Record<string, ImprovementEvidence[]>>({});
  const [selectedImprovementEvidence, setSelectedImprovementEvidence] = useState<Record<string, string[]>>({});
  const [runEvidence, setRunEvidence] = useState<Record<string, RunEvidence>>({});
  const [improvementOpen, setImprovementOpen] = useState<string | null>(null);
  const [qualityDetailsOpen, setQualityDetailsOpen] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const descriptors = useNativeSkillsStore((state) => state.descriptors);
  const refreshNativeSkills = useNativeSkillsStore((state) => state.refresh);
  /** The current installed instructions are the "before" half of an update
   * candidate, read from the shared native-skill registry. */
  const installedBody = Object.fromEntries(descriptors.map((entry) => [entry.command, entry.instructions]));
  const skillFocus = useSkillLearningFocusStore((state) => state.focus);
  const clearFocus = useSkillLearningFocusStore((state) => state.clear);
  const [focusedInstalledKey, setFocusedInstalledKey] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    setError(null);
    try {
      const [nextSettings, nextCandidates, nextLearned] = await Promise.all([
        skillLearningClient.settings(),
        skillLearningClient.listCandidates(),
        skillLearningClient.learnedSkills(),
        refreshNativeSkills(),
      ]);
      setSettings(nextSettings);
      setCandidates(nextCandidates);
      setLearned(nextLearned);
      const withEvaluations = await Promise.all(
        nextCandidates
          .filter((candidate) => candidate.evaluation_ids.length > 0)
          .map(async (candidate) => [candidate.candidate_id, await skillLearningClient.evaluations(candidate.candidate_id)] as const),
      );
      setEvaluations(Object.fromEntries(withEvaluations));
    } catch (reason) {
      setError(errorMessage(reason));
    }
  }, [refreshNativeSkills]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  /** A learning notice names an exact candidate or installed skill — open it,
   * scroll it into view when the async Settings data arrives, then release the
   * focus so a later normal visit is not hijacked. */
  useEffect(() => {
    if (!skillFocus) return;
    if (skillFocus.kind === "candidate") {
      setFocusedInstalledKey(null);
      setExpanded(skillFocus.candidateId);
      clearFocus();
      return;
    }

    const key = `${skillFocus.scope}:${skillFocus.command}`;
    if (!learned.some((summary) => `${summary.scope}:${summary.command}` === key)) return;

    setFocusedInstalledKey(key);
    document.getElementById(`learned-skill-${key}`)?.scrollIntoView({ behavior: "smooth", block: "center" });
    clearFocus();
  }, [skillFocus, learned, clearFocus]);

  const run = async (key: string, operation: () => Promise<unknown>) => {
    setBusy(key);
    setError(null);
    try {
      await operation();
      await refresh();
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setBusy(null);
    }
  };

  const evaluate = (candidate: LearningCandidate) =>
    run(`evaluate:${candidate.candidate_id}`, async () => {
      const controller = new AbortController();
      await runCandidateEvaluation(candidate.candidate_id, controller.signal);
    });

  const open = candidates.filter(
    (candidate) => !["promoted", "rejected", "superseded"].includes(candidate.status),
  );

  return (
    <section className="flex flex-col gap-3 rounded-lg border border-border bg-surface p-3">
      <div className="flex items-start justify-between gap-3">
        <div>
          <h3 className="flex items-center gap-1.5 text-sm font-medium text-foreground">
            <Brain size={14} /> Learned skills
          </h3>
          <p className="text-xs text-faint">
            Reusable procedures derived from this agent's own verified work. A candidate is only ever opened from a real
            run's durable evidence, is staged and evaluated in a disposable copy of the workspace before it can be
            installed, and becomes an ordinary versioned native skill with full rollback when you approve it.
          </p>
        </div>
        <Button variant="ghost" size="sm" onClick={() => void refresh()} disabled={busy !== null}>
          <RefreshCw size={12} /> Refresh
        </Button>
      </div>

      {error && <p className="rounded border border-danger bg-danger-soft px-2 py-1 text-xs text-danger">{error}</p>}

      <label className="flex flex-col gap-1 text-xs">
        <span className="text-muted">Learning policy</span>
        <select
          value={settings.policy}
          disabled={busy !== null}
          onChange={(event) =>
            void run("policy", () =>
              skillLearningClient.setSettings({ ...settings, policy: event.target.value as LearningPolicy }),
            )
          }
          className="h-8 rounded-md border border-border bg-background px-2 text-xs text-foreground"
        >
          {POLICY_LABELS.map((entry) => (
            <option key={entry.value} value={entry.value}>
              {entry.label}
            </option>
          ))}
        </select>
        <span className="text-faint">{POLICY_LABELS.find((entry) => entry.value === settings.policy)?.detail}</span>
      </label>

      <label className="flex items-start gap-2 text-xs">
        <input
          type="checkbox"
          className="mt-0.5"
          checked={settings.allow_global_scope}
          disabled={busy !== null}
          onChange={(event) =>
            void run("scope", () =>
              skillLearningClient.setSettings({ ...settings, allow_global_scope: event.target.checked }),
            )
          }
        />
        <span className="flex flex-col">
          <span className="text-muted">Allow learning in global scope</span>
          <span className="text-faint">
            Off confines every candidate this loop opens to the workspace it was observed in. A workspace candidate is
            never quietly moved into global scope in either case — that is a separate, explicitly approved change.
          </span>
        </span>
      </label>

      <div className="flex flex-col gap-1.5">
        <h4 className="text-xs font-medium text-muted">Candidates ({open.length})</h4>
        {open.length === 0 ? (
          <p className="text-xs text-faint">No candidates are waiting. One appears after a run with real, verified evidence.</p>
        ) : (
          open.map((candidate) => {
            const isOpen = expanded === candidate.candidate_id;
            const editing = draft?.id === candidate.candidate_id;
            const drafted = candidate.proposed_skill_content.length > 0;
            return (
              <div key={candidate.candidate_id} className="rounded-md border border-border bg-background px-2.5 py-2 text-xs">
                <div className="flex flex-wrap items-center gap-2">
                  <span className="font-mono text-foreground">
                    /{candidate.proposed_command || "(not drafted)"}
                  </span>
                  <span className="text-muted">{candidate.title || candidate.signal_summary}</span>
                  <span className="rounded bg-warning-soft px-1 py-0.5 text-[10px] text-warning">{candidate.status}</span>
                  <span className="rounded border border-border px-1 py-0.5 text-[10px] text-faint">{candidate.scope}</span>
                  <span className="text-[10px] text-faint">{SOURCE_LABELS[candidate.source_kind]}</span>
                  <Button
                    variant="ghost"
                    size="sm"
                    className="ml-auto"
                    onClick={() => setExpanded(isOpen ? null : candidate.candidate_id)}
                  >
                    {isOpen ? "Hide" : "Details"}
                  </Button>
                </div>
                <p className="mt-1 text-muted">{candidate.signal_summary}</p>
                {candidate.correction && (
                  <p className="mt-1 text-faint">
                    Updates version {shortHash(candidate.correction.previous_skill_sha256)} used in run{" "}
                    {candidate.correction.previous_run_id}
                    {candidate.correction.failure_signature && ` · repeated failure "${candidate.correction.failure_signature}"`}
                    {candidate.correction.corrected_execution_succeeded && " · the corrected procedure ran and verified"}
                  </p>
                )}
                {candidate.dedup_detail && <p className="mt-1 text-faint">{candidate.dedup_detail}</p>}
                {candidate.evaluation_summary && (
                  <p className="mt-1 flex items-start gap-1">
                    {candidate.evaluation_verdict === "passed" ? (
                      <CheckCircle2 size={12} className="mt-0.5 shrink-0 text-success" />
                    ) : candidate.evaluation_verdict === "failed" ? (
                      <XCircle size={12} className="mt-0.5 shrink-0 text-danger" />
                    ) : (
                      <FlaskConical size={12} className="mt-0.5 shrink-0 text-muted" />
                    )}
                    <span className="text-muted">{candidate.evaluation_summary}</span>
                  </p>
                )}
                <PolicyNotes candidate={candidate} />

                {isOpen && (
                  <div className="mt-2 flex flex-col gap-1.5 border-t border-border pt-2">
                    <p className="text-faint">
                      Evidence: runs {candidate.source_run_ids.join(", ") || "—"} · {candidate.source_event_ids.length}{" "}
                      events · digest {shortHash(candidate.candidate_sha256)}
                      {candidate.parent_skill_sha256 && ` · replaces ${shortHash(candidate.parent_skill_sha256)}`}
                    </p>
                    {candidate.evidence && (
                      <p className="text-faint">
                        Observed: {candidate.evidence.tool_calls.length} tool call(s),{" "}
                        {candidate.evidence.verifications.length} verification round(s),{" "}
                        {candidate.evidence.changed_files.length} file(s) changed
                      </p>
                    )}
                    <p className="text-faint">
                      Tools while active:{" "}
                      {candidate.allowed_tools.length > 0 ? candidate.allowed_tools.join(", ") : "unrestricted"}
                    </p>
                    {candidate.parent_skill_sha256 && (
                      <div className="flex flex-col gap-1">
                        <span className="text-faint">Installed now ({shortHash(candidate.parent_skill_sha256)})</span>
                        <pre className="max-h-40 overflow-auto whitespace-pre-wrap rounded border border-border bg-surface p-2 font-mono text-[11px] text-muted">
                          {candidate.parent_skill_content ?? installedBody[candidate.proposed_command] ?? "(loading the installed version…)"}
                        </pre>
                        <span className="text-faint">Proposed ({shortHash(candidate.candidate_sha256)})</span>
                      </div>
                    )}
                    {candidate.parent_skill_sha256 && candidate.proposed_skill_content && (
                      <div className="flex flex-col gap-1">
                        <span className="text-faint">Instruction diff</span>
                        <pre className="max-h-56 overflow-auto whitespace-pre-wrap rounded border border-border bg-surface p-2 font-mono text-[11px]">
                          {boundedDiff(
                            candidate.parent_skill_content ?? installedBody[candidate.proposed_command] ?? "",
                            candidate.proposed_skill_content,
                          ).map((line, index) => (
                            <span
                              key={`${index}:${line.kind}`}
                              className={`block ${line.kind === "removed" ? "text-danger" : line.kind === "added" ? "text-success" : "text-muted"}`}
                            >
                              {line.kind === "removed" ? "- " : line.kind === "added" ? "+ " : "  "}
                              {line.text}
                            </span>
                          ))}
                        </pre>
                        <div className="grid gap-1 text-faint sm:grid-cols-2">
                          <span>
                            Allowed tools: {candidate.allowed_tools.join(", ") || "unrestricted"}
                            {addedValues(candidate.parent_allowed_tools ?? [], candidate.allowed_tools).length > 0 && (
                              <span className="text-warning"> · + {addedValues(candidate.parent_allowed_tools ?? [], candidate.allowed_tools).join(", ")} ⚠️ widened</span>
                            )}
                            {valuesChanged(candidate.parent_allowed_tools ?? [], candidate.allowed_tools) && addedValues(candidate.parent_allowed_tools ?? [], candidate.allowed_tools).length === 0 && " ⚠️ changed"}
                          </span>
                          <span>
                            Required binaries: {(candidate.requirements.bins ?? []).join(", ") || "none"}
                            {addedValues(candidate.parent_requirements?.bins ?? [], candidate.requirements.bins).length > 0 && (
                              <span className="text-warning"> · + {addedValues(candidate.parent_requirements?.bins ?? [], candidate.requirements.bins).join(", ")} ⚠️ widened</span>
                            )}
                            {valuesChanged(candidate.parent_requirements?.bins ?? [], candidate.requirements.bins) && addedValues(candidate.parent_requirements?.bins ?? [], candidate.requirements.bins).length === 0 && " ⚠️ changed"}
                          </span>
                          <span>
                            Required environment: {(candidate.requirements.env ?? []).join(", ") || "none"}
                            {addedValues(candidate.parent_requirements?.env ?? [], candidate.requirements.env).length > 0 && (
                              <span className="text-warning"> · + {addedValues(candidate.parent_requirements?.env ?? [], candidate.requirements.env).join(", ")} ⚠️ widened</span>
                            )}
                            {valuesChanged(candidate.parent_requirements?.env ?? [], candidate.requirements.env) && addedValues(candidate.parent_requirements?.env ?? [], candidate.requirements.env).length === 0 && " ⚠️ changed"}
                          </span>
                          <span>
                            Scope: {candidate.parent_scope ? `${candidate.parent_scope} → ${candidate.scope}` : candidate.scope}
                            {candidate.parent_scope && candidate.parent_scope !== candidate.scope && " ⚠️ widened from parent"}
                          </span>
                        </div>
                        {(candidate.parent_skill_resource_files?.length ?? 0) > 0 || candidate.proposed_resource_files.length > 0 ? (
                          <div className="mt-1 text-faint">
                            <span>
                              Bundled resources: {resourcePaths(candidate.parent_skill_resource_files).join(", ") || "none"} → {resourcePaths(candidate.proposed_resource_files).join(", ") || "none"}
                            </span>
                            {addedValues(resourcePaths(candidate.parent_skill_resource_files), resourcePaths(candidate.proposed_resource_files)).length > 0 && (
                              <span className="text-success"> · + {addedValues(resourcePaths(candidate.parent_skill_resource_files), resourcePaths(candidate.proposed_resource_files)).join(", ")}</span>
                            )}
                            {addedValues(resourcePaths(candidate.proposed_resource_files), resourcePaths(candidate.parent_skill_resource_files)).length > 0 && (
                              <span className="text-danger"> · − {addedValues(resourcePaths(candidate.proposed_resource_files), resourcePaths(candidate.parent_skill_resource_files)).join(", ")}</span>
                            )}
                          </div>
                        ) : null}
                      </div>
                    )}
                    {editing ? (
                      <div className="flex flex-col gap-1.5">
                        <div className="grid gap-1.5 sm:grid-cols-2">
                          <label className="flex flex-col gap-1">
                            <span className="text-faint">Display name</span>
                            <input
                              value={draft.title}
                              onChange={(event) => setDraft({ ...draft, title: event.target.value })}
                              className="h-8 rounded border border-border bg-surface px-2 text-xs text-foreground"
                            />
                          </label>
                          <label className="flex flex-col gap-1">
                            <span className="text-faint">Slash command</span>
                            <input
                              value={draft.command}
                              onChange={(event) => setDraft({ ...draft, command: event.target.value })}
                              placeholder="my-skill"
                              className="h-8 rounded border border-border bg-surface px-2 font-mono text-xs text-foreground"
                            />
                          </label>
                        </div>
                        <fieldset className="flex flex-col gap-1">
                          <legend className="text-faint">Scope</legend>
                          <div className="flex flex-wrap gap-3 text-xs text-muted">
                            <label className="flex items-center gap-1.5">
                              <input
                                type="radio"
                                name={`skill-scope-${candidate.candidate_id}`}
                                checked={draft.scope === "workspace"}
                                disabled={!candidate.workspace_path}
                                onChange={() => setDraft({ ...draft, scope: "workspace" })}
                              />
                              This workspace
                            </label>
                            <label className="flex items-center gap-1.5">
                              <input
                                type="radio"
                                name={`skill-scope-${candidate.candidate_id}`}
                                checked={draft.scope === "global"}
                                disabled={!settings.allow_global_scope}
                                onChange={() => setDraft({ ...draft, scope: "global" })}
                              />
                              Global
                            </label>
                          </div>
                          <span className="text-faint">
                            {candidate.workspace_path
                              ? "This workspace uses the folder recorded by the original run."
                              : "This run has no recorded workspace, so only global scope is available."}
                          </span>
                        </fieldset>
                        <label className="flex flex-col gap-1">
                          <span className="text-faint">Trigger description</span>
                          <textarea
                            value={draft.description}
                            onChange={(event) => setDraft({ ...draft, description: event.target.value })}
                            rows={2}
                            className="rounded border border-border bg-surface p-2 text-xs text-foreground"
                          />
                        </label>
                        <div className="grid gap-1.5 sm:grid-cols-3">
                          <label className="flex flex-col gap-1">
                            <span className="text-faint">Allowed tools</span>
                            <input
                              value={draft.allowedTools}
                              onChange={(event) => setDraft({ ...draft, allowedTools: event.target.value })}
                              placeholder="read_file, edit_file"
                              className="h-8 rounded border border-border bg-surface px-2 text-xs text-foreground"
                            />
                          </label>
                          <label className="flex flex-col gap-1">
                            <span className="text-faint">Required binaries</span>
                            <input
                              value={draft.bins}
                              onChange={(event) => setDraft({ ...draft, bins: event.target.value })}
                              placeholder="cargo"
                              className="h-8 rounded border border-border bg-surface px-2 text-xs text-foreground"
                            />
                          </label>
                          <label className="flex flex-col gap-1">
                            <span className="text-faint">Required environment</span>
                            <input
                              value={draft.env}
                              onChange={(event) => setDraft({ ...draft, env: event.target.value })}
                              placeholder="API_KEY"
                              className="h-8 rounded border border-border bg-surface px-2 text-xs text-foreground"
                            />
                          </label>
                        </div>
                        <label className="flex flex-col gap-1">
                          <span className="text-faint">Instructions</span>
                          <textarea
                            value={draft.content}
                            onChange={(event) => setDraft({ ...draft, content: event.target.value })}
                            rows={10}
                            className="w-full rounded border border-border bg-surface p-2 font-mono text-[11px] text-foreground"
                          />
                        </label>
                      </div>
                    ) : (
                      <pre className="max-h-64 overflow-auto whitespace-pre-wrap rounded border border-border bg-surface p-2 font-mono text-[11px] text-foreground">
                        {candidate.proposed_skill_content || "(no draft yet — generate one to see the proposed procedure)"}
                      </pre>
                    )}
                    {(evaluations[candidate.candidate_id] ?? []).map((record) => (
                      <div key={record.evaluation_id} className="rounded border border-border bg-surface p-2 text-[11px]">
                        <div className="font-medium text-muted">Evaluation ({record.mode}) · {record.verdict}</div>
                        <div className="grid grid-cols-[minmax(0,1fr)_auto_auto] gap-x-2">
                          <span className="text-faint">Case</span><span className="text-faint">Current</span><span className="text-faint">Proposed</span>
                          {record.cases.flatMap((testCase) => {
                            const baseline = record.reports.find((report) => report.case_id === testCase.case_id && report.arm === "baseline");
                            const proposed = record.reports.find((report) => report.case_id === testCase.case_id && report.arm === "candidate");
                            return [
                              <span key={`${testCase.case_id}:name`} className="text-faint">{testCase.name}</span>,
                              <span key={`${testCase.case_id}:baseline`}>{baseline?.verification_passed === true ? "Passed" : baseline?.verification_passed === false ? "Failed" : "Unknown"}</span>,
                              <span key={`${testCase.case_id}:candidate`}>{proposed?.verification_passed === true ? "Passed" : proposed?.verification_passed === false ? "Failed" : "Unknown"}</span>,
                            ];
                          })}
                        </div>
                        <p className="mt-1 text-faint">{record.summary}</p>
                      </div>
                    ))}
                  </div>
                )}

                <div className="mt-2 flex flex-wrap justify-end gap-1">
                  {!drafted && (
                    <Button
                      variant="secondary"
                      size="sm"
                      disabled={busy !== null}
                      onClick={() =>
                        void run(`draft:${candidate.candidate_id}`, async () => {
                          const outcome = await draftCandidate(candidate.candidate_id);
                          if (outcome.error) throw new Error(outcome.error);
                          if (outcome.declined) {
                            throw new Error(
                              "The reflection pass found nothing reusable in this run and declined to draft a skill.",
                            );
                          }
                          setExpanded(candidate.candidate_id);
                        })
                      }
                    >
                      {busy === `draft:${candidate.candidate_id}` && <Loader2 size={12} className="animate-spin" />}
                      <Wand2 size={12} /> Generate draft
                    </Button>
                  )}
                  <Button
                    variant="ghost"
                    size="sm"
                    disabled={busy !== null || !drafted}
                    onClick={() => void evaluate(candidate)}
                  >
                    {busy === `evaluate:${candidate.candidate_id}` && <Loader2 size={12} className="animate-spin" />}
                    <FlaskConical size={12} /> Evaluate
                  </Button>
                  {editing ? (
                    <Button
                      variant="secondary"
                      size="sm"
                      disabled={busy !== null}
                      onClick={() =>
                        void run(`edit:${candidate.candidate_id}`, async () => {
                          await skillLearningClient.stage(candidate.candidate_id, {
                            scope: draft.scope,
                            title: draft.title,
                            description: draft.description,
                            proposed_command: draft.command,
                            proposed_skill_content: draft.content,
                            proposed_resource_files: candidate.proposed_resource_files,
                            allowed_tools: parseList(draft.allowedTools),
                            requirements: { bins: parseList(draft.bins), env: parseList(draft.env) },
                          });
                          setDraft(null);
                        })
                      }
                    >
                      Save draft
                    </Button>
                  ) : (
                    <Button
                      variant="ghost"
                      size="sm"
                      disabled={busy !== null || !drafted}
                      onClick={() => {
                        setExpanded(candidate.candidate_id);
                        setDraft(candidateDraft(candidate));
                      }}
                    >
                      Edit before install
                    </Button>
                  )}
                  {/* No confirmation dialog here: the approval is the app's own
                      permission prompt, raised in Rust and bound to this exact
                      candidate's digest. A boolean from this component would
                      not authorize anything. */}
                  <Button
                    variant="secondary"
                    size="sm"
                    disabled={busy !== null || editing || !candidate.candidate_sha256}
                    onClick={() =>
                      void run(`promote:${candidate.candidate_id}`, async () => {
                        const outcome = await skillLearningClient.promote(candidate.candidate_id);
                        if (outcome.kind !== "promoted") {
                          throw new Error(
                            `${outcome.kind === "refused" ? "Refused" : "Awaiting approval"}: ${outcome.reasons.join("; ")}`,
                          );
                        }
                      })
                    }
                  >
                    {busy === `promote:${candidate.candidate_id}` && <Loader2 size={12} className="animate-spin" />}
                    Approve &amp; install
                  </Button>
                  <Button
                    variant="ghost"
                    size="sm"
                    disabled={busy !== null}
                    onClick={() =>
                      void run(`reject:${candidate.candidate_id}`, () =>
                        skillLearningClient.reject(candidate.candidate_id, "rejected from Settings"),
                      )
                    }
                  >
                    <Trash2 size={12} /> Reject
                  </Button>
                </div>
              </div>
            );
          })
        )}
      </div>

      <div className="flex flex-col gap-1.5">
        <h4 className="text-xs font-medium text-muted">Installed ({learned.length})</h4>
        {learned.length === 0 ? (
          <p className="text-xs text-faint">Nothing has been promoted yet.</p>
        ) : (
          learned.map((summary) => {
            const quality = summary.quality;
            const improveKey = `${summary.scope}:${summary.command}`;
            const openImprovement = improvementOpen === improveKey;
            const evidence = improvementEvidence[improveKey] ?? [];
            const selected = selectedImprovementEvidence[improveKey] ?? [];
            const hasOpenCandidate = Boolean(quality.open_improvement_candidate_id);
            return (
            <div
              key={`${summary.scope}:${summary.command}:${summary.active_sha256}`}
              id={`learned-skill-${summary.scope}:${summary.command}`}
              className={`rounded-md border bg-background px-2.5 py-2 text-xs transition-colors ${
                focusedInstalledKey === `${summary.scope}:${summary.command}`
                  ? "border-accent bg-accent/10 ring-2 ring-accent/40"
                  : "border-border"
              }`}
            >
              <div className="flex flex-wrap items-center gap-2">
                <span className="font-mono text-foreground">/{summary.command}</span>
                <span className="text-muted">{summary.version}</span>
                <span className="rounded border border-border px-1 py-0.5 text-[10px] text-faint">{summary.scope}</span>
                <span className="rounded bg-success-soft px-1 py-0.5 text-[10px] text-success">learned</span>
                {!summary.enabled && (
                  <span className="rounded bg-warning-soft px-1 py-0.5 text-[10px] text-warning">disabled</span>
                )}
                {summary.deprecated && (
                  <span
                    className="rounded bg-warning-soft px-1 py-0.5 text-[10px] text-warning"
                    title={summary.deprecation_reason ?? undefined}
                  >
                    deprecated
                  </span>
                )}
                <span className="ml-auto font-mono text-[10px] text-faint">{shortHash(summary.active_sha256)}</span>
              </div>
              <p className="mt-1 text-faint">
                From {summary.provenance.source_kind} · runs {summary.provenance.source_run_ids.join(", ") || "—"} ·{" "}
                {summary.provenance.promotion_policy}
                {summary.provenance.approval_id && ` · approval ${summary.provenance.approval_id}`}
                {summary.provenance.evaluation_ids.length > 0 &&
                  ` · evaluations ${summary.provenance.evaluation_ids.join(", ")}`}
              </p>
              <p className="text-faint">
                {summary.uses} use{summary.uses === 1 ? "" : "s"} · {summary.failures} failure
                {summary.failures === 1 ? "" : "s"} · {summary.corrections} correction
                {summary.corrections === 1 ? "" : "s"}
                {summary.last_used_at_unix_ms !== null &&
                  ` · last used ${new Date(summary.last_used_at_unix_ms).toLocaleString()}`}
                {summary.previous_sha256.length > 0 &&
                  ` · previous versions ${summary.previous_sha256.map(shortHash).join(", ")}`}
              </p>
              <div className="mt-2 flex flex-wrap items-center gap-2">
                <button
                  type="button"
                  className={`rounded px-1.5 py-0.5 text-[10px] ${qualityClass(quality.state)}`}
                  title={quality.reasons.join(" ")}
                  onClick={() => setQualityDetailsOpen((current) => (current === improveKey ? null : improveKey))}
                >
                  {qualityLabel(quality.state)}
                </button>
                <span className="text-faint">
                  Verified: {quality.verified_successes}/{quality.verified_successes + quality.verified_failures} · Failures: {quality.verified_failures} · Corrections: {quality.corrections}
                </span>
                {quality.last_used_at_unix_ms !== null && (
                  <span className="text-faint">Last used: {new Date(quality.last_used_at_unix_ms).toLocaleString()}</span>
                )}
              </div>
              <div className="mt-1 rounded border border-border bg-surface px-2 py-1 text-[11px] text-faint">
                <div>{quality.reasons[0] ?? "No quality evidence yet."}</div>
                {qualityDetailsOpen === improveKey && (
                  <div className="mt-2 flex flex-col gap-1 border-t border-border pt-1">
                    <span className="font-medium text-muted">Quality details</span>
                    {quality.reasons.map((reason) => <span key={reason}>• {reason}</span>)}
                    <span>Verified: {quality.verified_successes} passed · {quality.verified_failures} failed</span>
                    {quality.unknown_verification > 0 && <span>! {quality.unknown_verification} unverified run(s) — not counted as success</span>}
                    {quality.cancelled_runs > 0 && <span>! {quality.cancelled_runs} cancelled run(s) — not counted as failure</span>}
                    <span className="mt-1 font-medium text-muted">Recent evidence</span>
                    {quality.recent_runs.length === 0 ? <span>No runs recorded for this version.</span> : quality.recent_runs.map((recentRun) => {
                      const evidenceKey = `${improveKey}:${recentRun.run_id}`;
                      const loadedEvidence = runEvidence[evidenceKey];
                      return (
                        <div key={recentRun.run_id} className="flex flex-col gap-0.5">
                          <span>
                            {recentRun.outcome === "cancelled" ? "—" : recentRun.verification_passed === true ? "✓" : recentRun.verification_passed === false ? "✕" : "!"} {recentRun.run_id.slice(0, 12)} · {recentRun.user_corrected ? "correction" : recentRun.outcome} · {new Date(recentRun.recorded_at_unix_ms).toLocaleString()}
                            {recentRun.failure_signature && ` · ${recentRun.failure_signature}`}
                          </span>
                          {recentRun.evidence_available && (
                            <button
                              type="button"
                              className="self-start text-accent underline"
                              disabled={busy !== null}
                              onClick={() => {
                                if (loadedEvidence) {
                                  setRunEvidence((current) => {
                                    const next = { ...current };
                                    delete next[evidenceKey];
                                    return next;
                                  });
                                  return;
                                }
                                void run(`evidence:${evidenceKey}`, async () => {
                                  const evidence = await skillLearningClient.runEvidence(summary.scope, summary.command, recentRun.run_id);
                                  setRunEvidence((current) => ({ ...current, [evidenceKey]: evidence }));
                                });
                              }}
                            >
                              {loadedEvidence ? "Hide evidence" : "View evidence"}
                            </button>
                          )}
                          {loadedEvidence && (
                            <div className="rounded border border-border bg-background px-1.5 py-1">
                              <span>{loadedEvidence.tool_calls.length} bounded tool call(s) · {loadedEvidence.verifications.length} verification(s)</span>
                              {loadedEvidence.tool_calls.map((call) => (
                                <span key={call.tool_call_id} className="block">
                                  {call.succeeded ? "✓" : "✕"} {call.tool_name}{call.failure_excerpt ? ` · ${call.failure_excerpt}` : ""}
                                </span>
                              ))}
                              {loadedEvidence.verifications.map((verification) => (
                                <span key={verification.event_id} className="block">
                                  {verification.passed ? "✓" : "✕"} {verification.name}: {verification.summary}
                                </span>
                              ))}
                            </div>
                          )}
                        </div>
                      );
                    })}
                    {summary.history.length > 0 && (
                      <>
                        <span className="mt-1 font-medium text-muted">History</span>
                        {summary.history.map((version) => (
                          <span key={version.sha256}>
                            {shortHash(version.sha256)} · {version.version} · {version.sha256 === summary.active_sha256 ? "current" : "previous"} · {version.uses} use{version.uses === 1 ? "" : "s"} · {version.failures} failure{version.failures === 1 ? "" : "s"} · {version.corrections} correction{version.corrections === 1 ? "" : "s"}
                          </span>
                        ))}
                      </>
                    )}
                  </div>
                )}
                {openImprovement && (
                  <div className="mt-2 flex flex-col gap-1.5">
                    <div className="flex items-center justify-between gap-2">
                      <span className="font-medium text-muted">Improvement evidence (max 5)</span>
                      <span>{selected.length}/5 selected</span>
                    </div>
                    {evidence.length === 0 ? (
                      <span>Use this skill in a completed run before improving it.</span>
                    ) : (
                      evidence.map((item) => {
                        const checked = selected.includes(item.run_id);
                        return (
                          <label key={item.run_id} className="flex items-start gap-2">
                            <input
                              type="checkbox"
                              checked={checked}
                              disabled={!checked && selected.length >= 5}
                              onChange={(event) => {
                                const next = event.target.checked
                                  ? [...selected, item.run_id]
                                  : selected.filter((runId) => runId !== item.run_id);
                                setSelectedImprovementEvidence((current) => ({ ...current, [improveKey]: next }));
                              }}
                            />
                            <span>
                              <span className="font-mono text-muted">{item.run_id.slice(0, 12)}</span> · {item.user_corrected ? "correction" : item.outcome === "cancelled" ? "cancelled" : item.verification_passed === false ? "verification failed" : item.verification_passed === true ? "verified success" : item.outcome}
                              <span className="ml-1">· {new Date(item.recorded_at_unix_ms).toLocaleString()}</span>
                              {item.summary && <span className="block">{item.summary}</span>}
                            </span>
                          </label>
                        );
                      })
                    )}
                    <div className="flex justify-end gap-1">
                      <Button
                        variant="secondary"
                        size="sm"
                        disabled={busy !== null || selected.length === 0}
                        onClick={() =>
                          void run(`begin-improvement:${improveKey}`, async () => {
                            const candidate = await skillLearningClient.beginImprovement(summary.scope, summary.command, selected);
                            setExpanded(candidate.candidate_id);
                            setImprovementOpen(null);
                          })
                        }
                      >
                        Begin improvement
                      </Button>
                    </div>
                  </div>
                )}
              </div>
              {/* All four actions go straight to the native skill backend — a
                  learned skill is an ordinary versioned skill once installed,
                  and this panel must not grow a second copy of that logic. */}
              <div className="mt-1.5 flex flex-wrap justify-end gap-1">
                <Button
                  variant={hasOpenCandidate ? "secondary" : "ghost"}
                  size="sm"
                  disabled={busy !== null || quality.improvement_evidence_count === 0}
                  title={quality.improvement_evidence_count === 0 ? "Use this skill in a completed run before improving it." : undefined}
                  onClick={() => {
                    if (hasOpenCandidate) {
                      setExpanded(quality.open_improvement_candidate_id);
                      return;
                    }
                    if (openImprovement) {
                      setImprovementOpen(null);
                      return;
                    }
                    void run(`improvement-evidence:${improveKey}`, async () => {
                      const next = await skillLearningClient.improvementEvidence(summary.scope, summary.command);
                      setImprovementEvidence((current) => ({ ...current, [improveKey]: next }));
                      setSelectedImprovementEvidence((current) => ({
                        ...current,
                        [improveKey]: next.slice(0, Math.min(5, next.length)).map((item) => item.run_id),
                      }));
                      setImprovementOpen(improveKey);
                    });
                  }}
                >
                  <Wand2 size={12} /> {hasOpenCandidate ? "Review improvement" : "Improve skill"}
                </Button>
                <Button
                  variant="ghost"
                  size="sm"
                  disabled={busy !== null || summary.previous_sha256.length === 0}
                  title={
                    summary.previous_sha256.length === 0
                      ? "This is the first installed version — there is nothing to roll back to."
                      : `Roll back to ${shortHash(summary.previous_sha256[0])}`
                  }
                  onClick={() =>
                    void run(`rollback:${summary.command}`, () =>
                      nativeSkillsClient.rollback(summary.scope, summary.command),
                    )
                  }
                >
                  {busy === `rollback:${summary.command}` && <Loader2 size={12} className="animate-spin" />}
                  <RotateCcw size={12} /> Roll back
                </Button>
                <Button
                  variant="ghost"
                  size="sm"
                  disabled={busy !== null}
                  onClick={() =>
                    void run(`enabled:${summary.command}`, () =>
                      nativeSkillsClient.setEnabled(summary.scope, summary.command, !summary.enabled),
                    )
                  }
                >
                  {summary.enabled ? "Disable" : "Enable"}
                </Button>
                <Button
                  variant="ghost"
                  size="sm"
                  disabled={busy !== null || summary.deprecated}
                  onClick={() =>
                    void run(`deprecate:${summary.command}`, () =>
                      skillLearningClient.deprecate(summary.scope, summary.command, "deprecated from Settings"),
                    )
                  }
                >
                  Deprecate
                </Button>
                <Button
                  variant="ghost"
                  size="sm"
                  disabled={busy !== null}
                  onClick={() =>
                    void run(`uninstall:${summary.command}`, () =>
                      nativeSkillsClient.uninstall(summary.scope, summary.command),
                    )
                  }
                >
                  <Trash2 size={12} /> Uninstall
                </Button>
              </div>
            </div>
            );
          })
        )}
      </div>
    </section>
  );
}

export default SkillLearningPanel;

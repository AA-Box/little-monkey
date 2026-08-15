import { useCallback, useEffect, useState } from "react";
import { Brain, CheckCircle2, FlaskConical, Loader2, RefreshCw, ShieldAlert, Trash2, XCircle } from "lucide-react";

import {
  skillLearningClient,
  type EvaluationRecord,
  type LearnedSkillSummary,
  type LearningCandidate,
  type LearningMode,
  type LearningSourceKind,
} from "../../lib/skillLearningClient";
import { runCandidateEvaluation } from "../../lib/skillLearningEval";
import { useNativeSkillsStore } from "../../store/nativeSkillsStore";
import { Button } from "../ui";
import { errorMessage } from "../../lib/errors";

const MODE_LABELS: Array<{ value: LearningMode; label: string; detail: string }> = [
  { value: "off", label: "Off", detail: "No candidates are detected or stored." },
  {
    value: "suggest_only",
    label: "Suggest only",
    detail: "Signals are recorded; a draft is written only when you explicitly ask for one. Nothing installs without your approval.",
  },
  {
    value: "auto_stage",
    label: "Auto-stage",
    detail: "Drafts every detected signal into a staged candidate. Nothing installs without your approval.",
  },
  {
    value: "auto_promote_safe",
    label: "Auto-promote safe improvements",
    detail:
      "Additionally installs a workspace-scoped candidate that passed evaluation, adds no tool access, and declares no new executables or environment variables. Anything else still waits for you.",
  },
];

const SOURCE_LABELS: Record<LearningSourceKind, string> = {
  explicit_user_instruction: "you asked for it",
  user_correction: "your correction verified",
  verification_repair: "verification repair",
  successful_novel_procedure: "verified procedure",
  repeated_failure_resolution: "recurring failure resolved",
};

function shortHash(value: string | null | undefined): string {
  return value ? `${value.slice(0, 12)}…` : "—";
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
  const [mode, setMode] = useState<LearningMode>("suggest_only");
  const [candidates, setCandidates] = useState<LearningCandidate[]>([]);
  const [learned, setLearned] = useState<LearnedSkillSummary[]>([]);
  const [evaluations, setEvaluations] = useState<Record<string, EvaluationRecord[]>>({});
  const [expanded, setExpanded] = useState<string | null>(null);
  const [draft, setDraft] = useState<{ id: string; content: string } | null>(null);
  /** The instructions each installed learned skill carries right now, keyed by
   * command — the "before" half of an update candidate's diff. Loaded from the
   * same discovery the native skill list uses, so it is what a future run
   * would actually read, not a stored copy. */
  const [installedBody, setInstalledBody] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const bumpNativeSkills = useNativeSkillsStore((state) => state.bump);

  const refresh = useCallback(async () => {
    setError(null);
    try {
      const [nextMode, nextCandidates, nextLearned, descriptors] = await Promise.all([
        skillLearningClient.mode(),
        skillLearningClient.listCandidates(),
        skillLearningClient.learnedSkills(),
        skillLearningClient.discover(),
      ]);
      setMode(nextMode);
      setCandidates(nextCandidates);
      setLearned(nextLearned);
      setInstalledBody(Object.fromEntries(descriptors.map((entry) => [entry.command, entry.instructions])));
    } catch (reason) {
      setError(errorMessage(reason));
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const run = async (key: string, operation: () => Promise<unknown>) => {
    setBusy(key);
    setError(null);
    try {
      await operation();
      bumpNativeSkills();
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
      const record = await runCandidateEvaluation(candidate.candidate_id, controller.signal);
      setEvaluations((current) => ({
        ...current,
        [candidate.candidate_id]: [...(current[candidate.candidate_id] ?? []), record],
      }));
    });

  const promote = (candidate: LearningCandidate) => {
    const policy = candidate.policy;
    const lines = [
      `Install /${candidate.proposed_command} (${candidate.scope} scope) as a learned skill?`,
      "",
      `Digest: ${candidate.candidate_sha256}`,
      candidate.allowed_tools.length > 0
        ? `Tools while active: ${candidate.allowed_tools.join(", ")}`
        : "Tools while active: unrestricted",
      candidate.requirements.bins.length > 0 ? `Requires executables: ${candidate.requirements.bins.join(", ")}` : "",
      candidate.requirements.env.length > 0 ? `Requires environment: ${candidate.requirements.env.join(", ")}` : "",
      candidate.parent_skill_sha256 ? `Replaces version ${shortHash(candidate.parent_skill_sha256)} (kept for rollback)` : "",
      policy && policy.approval_reasons.length > 0 ? `\nApproval needed because ${policy.approval_reasons.join("; ")}.` : "",
      candidate.evaluation_verdict === "unevaluated" || candidate.evaluation_verdict === null
        ? "\nThis candidate has not passed an evaluation."
        : "",
    ].filter(Boolean);
    if (!window.confirm(lines.join("\n"))) return;
    void run(`promote:${candidate.candidate_id}`, async () => {
      const outcome = await skillLearningClient.promote(candidate.candidate_id, true);
      if (outcome.kind !== "promoted") {
        throw new Error(`${outcome.kind === "refused" ? "Refused" : "Awaiting approval"}: ${outcome.reasons.join("; ")}`);
      }
    });
  };

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
            run's durable evidence, is staged and evaluated before it can be installed, and becomes an ordinary versioned
            native skill with full rollback when you approve it.
          </p>
        </div>
        <Button variant="ghost" size="sm" onClick={() => void refresh()} disabled={busy !== null}>
          <RefreshCw size={12} /> Refresh
        </Button>
      </div>

      {error && <p className="rounded border border-danger bg-danger-soft px-2 py-1 text-xs text-danger">{error}</p>}

      <label className="flex flex-col gap-1 text-xs">
        <span className="text-muted">Learning mode</span>
        <select
          value={mode}
          disabled={busy !== null}
          onChange={(event) =>
            void run("mode", () => skillLearningClient.setMode(event.target.value as LearningMode))
          }
          className="h-8 rounded-md border border-border bg-background px-2 text-xs text-foreground"
        >
          {MODE_LABELS.map((entry) => (
            <option key={entry.value} value={entry.value}>
              {entry.label}
            </option>
          ))}
        </select>
        <span className="text-faint">{MODE_LABELS.find((entry) => entry.value === mode)?.detail}</span>
      </label>

      <div className="flex flex-col gap-1.5">
        <h4 className="text-xs font-medium text-muted">Candidates ({open.length})</h4>
        {open.length === 0 ? (
          <p className="text-xs text-faint">No candidates are waiting. One appears after a run with real, verified evidence.</p>
        ) : (
          open.map((candidate) => {
            const isOpen = expanded === candidate.candidate_id;
            const editing = draft?.id === candidate.candidate_id;
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
                    <p className="text-faint">
                      Tools while active:{" "}
                      {candidate.allowed_tools.length > 0 ? candidate.allowed_tools.join(", ") : "unrestricted"}
                    </p>
                    {candidate.parent_skill_sha256 && (
                      <div className="flex flex-col gap-1">
                        <span className="text-faint">Installed now ({shortHash(candidate.parent_skill_sha256)})</span>
                        <pre className="max-h-40 overflow-auto whitespace-pre-wrap rounded border border-border bg-surface p-2 font-mono text-[11px] text-muted">
                          {installedBody[candidate.proposed_command] ?? "(loading the installed version…)"}
                        </pre>
                        <span className="text-faint">Proposed ({shortHash(candidate.candidate_sha256)})</span>
                      </div>
                    )}
                    {editing ? (
                      <textarea
                        value={draft.content}
                        onChange={(event) => setDraft({ id: candidate.candidate_id, content: event.target.value })}
                        rows={10}
                        className="w-full rounded border border-border bg-surface p-2 font-mono text-[11px] text-foreground"
                      />
                    ) : (
                      <pre className="max-h-64 overflow-auto whitespace-pre-wrap rounded border border-border bg-surface p-2 font-mono text-[11px] text-foreground">
                        {candidate.proposed_skill_content || "(no draft yet — the reflection pass has not run)"}
                      </pre>
                    )}
                    {(evaluations[candidate.candidate_id] ?? []).map((record) => (
                      <p key={record.evaluation_id} className="text-faint">
                        {record.evaluation_id}: {record.verdict} — {record.summary}
                      </p>
                    ))}
                  </div>
                )}

                <div className="mt-2 flex flex-wrap justify-end gap-1">
                  <Button
                    variant="ghost"
                    size="sm"
                    disabled={busy !== null || !candidate.proposed_command}
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
                            scope: candidate.scope,
                            title: candidate.title,
                            description: candidate.description,
                            proposed_command: candidate.proposed_command,
                            proposed_skill_content: draft.content,
                            proposed_resource_files: candidate.proposed_resource_files,
                            allowed_tools: candidate.allowed_tools,
                            requirements: candidate.requirements,
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
                      disabled={busy !== null || !candidate.proposed_skill_content}
                      onClick={() => {
                        setExpanded(candidate.candidate_id);
                        setDraft({ id: candidate.candidate_id, content: candidate.proposed_skill_content });
                      }}
                    >
                      Edit before install
                    </Button>
                  )}
                  <Button
                    variant="secondary"
                    size="sm"
                    disabled={busy !== null || candidate.status === "detected" || !candidate.candidate_sha256}
                    onClick={() => promote(candidate)}
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
          learned.map((summary) => (
            <div
              key={`${summary.scope}:${summary.command}:${summary.active_sha256}`}
              className="rounded-md border border-border bg-background px-2.5 py-2 text-xs"
            >
              <div className="flex flex-wrap items-center gap-2">
                <span className="font-mono text-foreground">/{summary.command}</span>
                <span className="text-muted">{summary.version}</span>
                <span className="rounded border border-border px-1 py-0.5 text-[10px] text-faint">{summary.scope}</span>
                <span className="rounded bg-success-soft px-1 py-0.5 text-[10px] text-success">learned</span>
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
                {summary.provenance.evaluation_ids.length > 0 &&
                  ` · evaluations ${summary.provenance.evaluation_ids.join(", ")}`}
              </p>
              <p className="text-faint">
                {summary.uses} use{summary.uses === 1 ? "" : "s"} · {summary.failures} failure
                {summary.failures === 1 ? "" : "s"} · {summary.corrections} correction
                {summary.corrections === 1 ? "" : "s"}
                {summary.previous_sha256.length > 0 &&
                  ` · previous versions ${summary.previous_sha256.map(shortHash).join(", ")}`}
              </p>
              <div className="mt-1.5 flex justify-end">
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
              </div>
            </div>
          ))
        )}
        <p className="text-[11px] text-faint">
          Rollback, disable, and uninstall for an installed learned skill live in the native skill list above — a learned
          skill is an ordinary versioned skill once it is installed.
        </p>
      </div>
    </section>
  );
}

export default SkillLearningPanel;

import { useCallback, useEffect, useMemo, useState } from "react";
import { save } from "@tauri-apps/plugin-dialog";
import { writeTextFile } from "@tauri-apps/plugin-fs";
import {
  Check,
  CheckCircle2,
  Copy,
  Download,
  FlaskConical,
  History,
  Play,
  Plus,
  RefreshCw,
  Square,
  Trash2,
  X,
  XCircle,
} from "lucide-react";

import {
  ecosystemClient,
  type WorkflowDefinition,
} from "../../lib/ecosystemClient";
import {
  exportEvalRun,
  exportEvalSuite,
  releaseGateStatus,
  type EvalCase,
  type EvalRun,
  type EvalRunStatus,
  type EvalTarget,
} from "../../lib/evalHarness";
import { nativeSkills, type SlashSkill } from "../../lib/skills";
import { useEvalHarnessStore } from "../../store/evalHarnessStore";
import { useMcpStore } from "../../store/mcpStore";
import { useNativeSkillsStore } from "../../store/nativeSkillsStore";
import { Button, IconButton, StatusPill, type PillTone } from "../ui";
import { errorMessage } from "../../lib/errors";
import { statusTone as sharedStatusTone } from "../../lib/statusTone";

interface EvalHarnessPanelProps {
  onClose: () => void;
}

const FIELD_CLASS = "w-full rounded-md border border-border bg-background px-2 py-1.5 text-sm text-foreground outline-none placeholder:text-faint focus:border-accent focus:ring-1 focus:ring-accent";
const LABEL_CLASS = "space-y-1 text-xs font-medium text-muted";

function stringList(value: string): string[] {
  return [...new Set(value.split(/[\n,]/).map((entry) => entry.trim()).filter(Boolean))];
}

function nullableNumber(value: string): number | null {
  if (!value.trim()) return null;
  const number = Number(value);
  return Number.isFinite(number) ? number : null;
}

function statusTone(status: EvalRunStatus): PillTone {
  // "running" reads as in-flight here; everything non-passed is a failure
  // signal for a suite, including "cancelled".
  return sharedStatusTone(status, { running: "warning", cancelled: "danger" });
}

function formatTime(timestamp: number): string {
  return new Date(timestamp).toLocaleString();
}

function formatTarget(target: EvalTarget): string {
  if (target.kind === "model") return "Active model";
  if (target.kind === "agent") return "Active agent";
  if (target.kind === "skill") return `Skill /${target.command || "not selected"}`;
  if (target.kind === "connector") return `Connector ${target.serverId || "?"}/${target.toolName || "?"}`;
  return `Workflow ${target.workflowId || "not selected"}`;
}

function gateTone(status: ReturnType<typeof releaseGateStatus>["status"]): PillTone {
  if (status === "passed") return "success";
  if (status === "blocked") return "danger";
  if (status === "unverified") return "warning";
  return "neutral";
}

function CaseEditor({ suiteId, testCase, target }: { suiteId: string; testCase: EvalCase; target: EvalTarget }) {
  const updateCase = useEvalHarnessStore((state) => state.updateCase);
  const [jsonText, setJsonText] = useState(testCase.expectations.jsonSubset ? JSON.stringify(testCase.expectations.jsonSubset, null, 2) : "");
  const [jsonError, setJsonError] = useState<string | null>(null);

  useEffect(() => {
    setJsonText(testCase.expectations.jsonSubset ? JSON.stringify(testCase.expectations.jsonSubset, null, 2) : "");
    setJsonError(null);
  }, [testCase.id, testCase.expectations.jsonSubset]);

  const patchExpectations = (patch: Partial<EvalCase["expectations"]>) => {
    updateCase(suiteId, testCase.id, { expectations: { ...testCase.expectations, ...patch } });
  };

  const commitJsonSubset = () => {
    if (!jsonText.trim()) {
      setJsonError(null);
      patchExpectations({ jsonSubset: null });
      return;
    }
    try {
      const parsed = JSON.parse(jsonText) as unknown;
      if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) throw new Error("Expected a JSON object.");
      setJsonError(null);
      patchExpectations({ jsonSubset: parsed as Record<string, unknown> });
    } catch (error) {
      setJsonError(errorMessage(error));
    }
  };

  const connectorCall = target.kind === "connector" && target.serverId && target.toolName
    ? `${target.serverId}/${target.toolName}`
    : null;

  return (
    <div className="space-y-4 pb-8">
      <section className="grid gap-3 rounded-lg border border-border bg-surface p-3 lg:grid-cols-2">
        <label className={LABEL_CLASS}>
          Case name
          <input className={FIELD_CLASS} value={testCase.name} onChange={(event) => updateCase(suiteId, testCase.id, { name: event.target.value })} />
        </label>
        <div className="flex items-end gap-4 pb-1">
          <label className="flex items-center gap-2 text-xs text-muted">
            <input type="checkbox" checked={testCase.enabled} onChange={(event) => updateCase(suiteId, testCase.id, { enabled: event.target.checked })} />
            Enabled
          </label>
          {(target.kind === "connector" || target.kind === "workflow") && (
            <label className="flex items-center gap-2 text-xs text-muted">
              <input type="checkbox" checked={testCase.dryRun} onChange={(event) => updateCase(suiteId, testCase.id, { dryRun: event.target.checked })} />
              Dry-run / replay preview
            </label>
          )}
        </div>
        <label className={`${LABEL_CLASS} lg:col-span-2`}>
          Input {target.kind === "connector" || target.kind === "workflow" ? "(JSON object)" : ""}
          <textarea rows={5} className={`${FIELD_CLASS} resize-y font-mono text-xs`} value={testCase.input} onChange={(event) => updateCase(suiteId, testCase.id, { input: event.target.value })} placeholder={target.kind === "connector" || target.kind === "workflow" ? "{}" : "Prompt or task input"} />
        </label>
        <label className={`${LABEL_CLASS} lg:col-span-2`}>
          Context fixture
          <textarea rows={3} className={`${FIELD_CLASS} resize-y`} value={testCase.context} onChange={(event) => updateCase(suiteId, testCase.id, { context: event.target.value })} placeholder="Optional untrusted context supplied to model, agent, or skill targets" />
        </label>
        <label className={LABEL_CLASS}>
          Retrieval/source labels
          <textarea rows={2} className={`${FIELD_CLASS} resize-y`} value={testCase.retrievalSources.join("\n")} onChange={(event) => updateCase(suiteId, testCase.id, { retrievalSources: stringList(event.target.value) })} placeholder="repo-index\ncustomer-docs" />
          <span className="block font-normal text-faint">Used to cluster failures by source.</span>
        </label>
        <label className={LABEL_CLASS}>
          Allowed tools
          <textarea rows={2} className={`${FIELD_CLASS} resize-y font-mono text-xs`} value={testCase.allowedTools.join("\n")} onChange={(event) => updateCase(suiteId, testCase.id, { allowedTools: stringList(event.target.value) })} placeholder={connectorCall ?? "search\nread_file"} />
          <span className="block font-normal text-faint">Model tools are captured without execution. Connector/workflow entries constrain observed calls.</span>
        </label>
      </section>

      <section className="space-y-3 rounded-lg border border-border bg-surface p-3">
        <div>
          <h3 className="text-sm font-semibold text-foreground">Pass/fail assertions</h3>
          <p className="text-xs text-faint">A case passes only when execution and every configured assertion pass. Empty verifier sets are rejected.</p>
        </div>
        <div className="grid gap-3 lg:grid-cols-2">
          <label className={LABEL_CLASS}>
            Output must contain (one per line)
            <textarea rows={3} className={`${FIELD_CLASS} resize-y`} value={testCase.expectations.contains.join("\n")} onChange={(event) => patchExpectations({ contains: stringList(event.target.value) })} />
          </label>
          <label className={LABEL_CLASS}>
            Output regular expression
            <input className={`${FIELD_CLASS} font-mono text-xs`} value={testCase.expectations.regex ?? ""} onChange={(event) => patchExpectations({ regex: event.target.value || null })} placeholder="^\\{.*\\}$" />
          </label>
          <label className={LABEL_CLASS}>
            Required tool calls
            <textarea rows={2} className={`${FIELD_CLASS} resize-y font-mono text-xs`} value={testCase.expectations.expectedToolCalls.join("\n")} onChange={(event) => patchExpectations({ expectedToolCalls: stringList(event.target.value) })} />
          </label>
          <label className={LABEL_CLASS}>
            Forbidden tool calls
            <textarea rows={2} className={`${FIELD_CLASS} resize-y font-mono text-xs`} value={testCase.expectations.forbiddenToolCalls.join("\n")} onChange={(event) => patchExpectations({ forbiddenToolCalls: stringList(event.target.value) })} />
          </label>
          <label className={`${LABEL_CLASS} lg:col-span-2`}>
            Expected JSON subset
            <textarea rows={4} className={`${FIELD_CLASS} resize-y font-mono text-xs`} value={jsonText} onChange={(event) => setJsonText(event.target.value)} onBlur={commitJsonSubset} placeholder={'{"status":"ok"}'} />
            {jsonError && <span className="block font-normal text-danger">{jsonError}</span>}
          </label>
        </div>
        <div className="grid gap-3 sm:grid-cols-3">
          <label className={LABEL_CLASS}>
            Max latency (ms)
            <input type="number" min={0} className={FIELD_CLASS} value={testCase.expectations.maxLatencyMs ?? ""} onChange={(event) => patchExpectations({ maxLatencyMs: nullableNumber(event.target.value) })} />
          </label>
          <label className={LABEL_CLASS}>
            Max total tokens
            <input type="number" min={0} className={FIELD_CLASS} value={testCase.expectations.maxTotalTokens ?? ""} onChange={(event) => patchExpectations({ maxTotalTokens: nullableNumber(event.target.value) })} />
          </label>
          <label className={LABEL_CLASS}>
            Max cost (µunits)
            <input type="number" min={0} className={FIELD_CLASS} value={testCase.expectations.maxCostMicros ?? ""} onChange={(event) => patchExpectations({ maxCostMicros: nullableNumber(event.target.value) })} />
          </label>
        </div>
      </section>

      <section className="space-y-3 rounded-lg border border-border bg-surface p-3">
        <label className={LABEL_CLASS}>
          Semantic scoring
          <select className={FIELD_CLASS} value={testCase.scoringMode} onChange={(event) => updateCase(suiteId, testCase.id, { scoringMode: event.target.value as EvalCase["scoringMode"] })}>
            <option value="constraints">Constraints only</option>
            <option value="golden">Golden answer</option>
            <option value="judge">Model judge + rubric</option>
          </select>
        </label>
        {testCase.scoringMode === "golden" && (
          <div className="grid gap-3 lg:grid-cols-[1fr_10rem]">
            <label className={LABEL_CLASS}>
              Golden answer
              <textarea rows={4} className={`${FIELD_CLASS} resize-y`} value={testCase.goldenAnswer} onChange={(event) => updateCase(suiteId, testCase.id, { goldenAnswer: event.target.value })} />
            </label>
            <label className={LABEL_CLASS}>
              Similarity threshold
              <input type="number" min={0} max={1} step={0.05} className={FIELD_CLASS} value={testCase.goldenThreshold} onChange={(event) => updateCase(suiteId, testCase.id, { goldenThreshold: Number(event.target.value) })} />
            </label>
          </div>
        )}
        {testCase.scoringMode === "judge" && (
          <div className="grid gap-3 lg:grid-cols-[1fr_10rem]">
            <label className={LABEL_CLASS}>
              Judge rubric
              <textarea rows={4} className={`${FIELD_CLASS} resize-y`} value={testCase.judgeRubric} onChange={(event) => updateCase(suiteId, testCase.id, { judgeRubric: event.target.value })} placeholder="The answer is factually correct, follows the requested format, and cites evidence." />
            </label>
            <label className={LABEL_CLASS}>
              Score threshold
              <input type="number" min={0} max={1} step={0.05} className={FIELD_CLASS} value={testCase.judgeThreshold} onChange={(event) => updateCase(suiteId, testCase.id, { judgeThreshold: Number(event.target.value) })} />
            </label>
          </div>
        )}
      </section>
    </div>
  );
}

function RunReport({ run }: { run: EvalRun }) {
  return (
    <div className="space-y-3">
      <section className="rounded-lg border border-border bg-surface p-3">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <StatusPill tone={statusTone(run.status)}>{run.status}</StatusPill>
          <span className="text-[11px] text-faint">{formatTime(run.startedAt)}</span>
        </div>
        <dl className="mt-3 grid grid-cols-2 gap-2 text-xs">
          <div><dt className="text-faint">Cases</dt><dd className="text-foreground">{run.passCount} passed · {run.failCount} failed</dd></div>
          <div><dt className="text-faint">Latency</dt><dd className="text-foreground">{run.totalLatencyMs.toLocaleString()} ms</dd></div>
          <div><dt className="text-faint">Tokens</dt><dd className="text-foreground">{run.results.some((result) => result.usage !== null) ? run.usage.totalTokens.toLocaleString() : "Not reported"}</dd></div>
          <div><dt className="text-faint">Cost</dt><dd className="text-foreground">{run.costMicros === null ? "Not reported" : `${run.costMicros.toLocaleString()} µ`}</dd></div>
        </dl>
        <p className="mt-2 break-all font-mono text-[10px] text-faint">suite {run.suiteFingerprint} · revision {run.suiteRevision}</p>
      </section>

      {run.failureClusters.length > 0 && (
        <section className="rounded-lg border border-danger/30 bg-danger-soft p-3">
          <h3 className="text-xs font-semibold text-danger">Failure clusters</h3>
          <ul className="mt-2 space-y-1 text-xs text-danger">
            {run.failureClusters.map((cluster) => (
              <li key={cluster.key}><span className="font-medium">{cluster.dimension.replace("_", " ")}:</span> {cluster.label} ({cluster.caseIds.length})</li>
            ))}
          </ul>
        </section>
      )}

      {run.results.map((result) => (
        <details key={result.caseId} open={result.status !== "passed"} className="rounded-lg border border-border bg-surface">
          <summary className="flex cursor-pointer list-none items-center gap-2 px-3 py-2 text-sm font-medium text-foreground">
            {result.status === "passed" ? <CheckCircle2 size={14} className="text-success" /> : result.status === "cancelled" ? <Square size={13} className="text-warning" /> : <XCircle size={14} className="text-danger" />}
            <span className="min-w-0 flex-1 truncate">{result.caseName}</span>
            <span className="text-xs font-normal text-faint">{result.latencyMs} ms</span>
          </summary>
          <div className="space-y-2 border-t border-border px-3 py-2">
            {result.error && <p className="text-xs text-danger">{result.error}</p>}
            <ul className="space-y-1">
              {result.assertions.map((assertion) => (
                <li key={assertion.id} className="flex items-start gap-1.5 text-xs">
                  {assertion.passed ? <Check size={13} className="mt-0.5 shrink-0 text-success" /> : <X size={13} className="mt-0.5 shrink-0 text-danger" />}
                  <span className={assertion.passed ? "text-muted" : "text-danger"}>
                    <span className="font-medium">{assertion.label}.</span> {assertion.evidence}
                  </span>
                </li>
              ))}
            </ul>
            {result.toolCalls.length > 0 && <p className="text-xs text-faint">Tools: {result.toolCalls.join(", ")}</p>}
            {result.output && <pre className="max-h-64 overflow-auto whitespace-pre-wrap break-words rounded-md bg-background p-2 text-[11px] text-muted">{result.output}</pre>}
            <p className="break-all font-mono text-[10px] text-faint">case {result.reproducibility.caseFingerprint} · {result.evidence?.targetLabel ?? "no execution evidence"}</p>
          </div>
        </details>
      ))}
    </div>
  );
}

function defaultTarget(kind: EvalTarget["kind"], skills: SlashSkill[], workflows: WorkflowDefinition[], servers: ReturnType<typeof useMcpStore.getState>["servers"]): EvalTarget {
  if (kind === "model" || kind === "agent") return { kind };
  if (kind === "skill") return { kind, command: skills[0]?.command ?? "" };
  if (kind === "workflow") return { kind, workflowId: workflows[0]?.workflow_id ?? "" };
  const server = servers.find((candidate) => candidate.status === "connected" && candidate.tools.length > 0);
  return { kind, serverId: server?.id ?? "", toolName: server?.tools[0]?.name ?? "" };
}

export function EvalHarnessPanel({ onClose }: EvalHarnessPanelProps) {
  const suites = useEvalHarnessStore((state) => state.suites);
  const runs = useEvalHarnessStore((state) => state.runs);
  const selectedSuiteId = useEvalHarnessStore((state) => state.selectedSuiteId);
  const activeRunId = useEvalHarnessStore((state) => state.activeRunId);
  const storeError = useEvalHarnessStore((state) => state.error);
  const selectSuite = useEvalHarnessStore((state) => state.selectSuite);
  const createSuite = useEvalHarnessStore((state) => state.createSuite);
  const duplicateSuite = useEvalHarnessStore((state) => state.duplicateSuite);
  const updateSuite = useEvalHarnessStore((state) => state.updateSuite);
  const deleteSuite = useEvalHarnessStore((state) => state.deleteSuite);
  const addCase = useEvalHarnessStore((state) => state.addCase);
  const duplicateCase = useEvalHarnessStore((state) => state.duplicateCase);
  const deleteCase = useEvalHarnessStore((state) => state.deleteCase);
  const runSuite = useEvalHarnessStore((state) => state.runSuite);
  const cancelRun = useEvalHarnessStore((state) => state.cancelRun);
  const clearHistory = useEvalHarnessStore((state) => state.clearHistory);
  const clearError = useEvalHarnessStore((state) => state.clearError);
  const servers = useMcpStore((state) => state.servers);
  const refreshMcp = useMcpStore((state) => state.refresh);

  const [selectedCaseId, setSelectedCaseId] = useState<string | null>(null);
  const [selectedRunId, setSelectedRunId] = useState<string | null>(null);
  const nativeSkillDescriptors = useNativeSkillsStore((state) => state.descriptors);
  const refreshNativeSkills = useNativeSkillsStore((state) => state.refresh);
  const skills = useMemo(() => nativeSkills(nativeSkillDescriptors), [nativeSkillDescriptors]);
  const [workflows, setWorkflows] = useState<WorkflowDefinition[]>([]);
  const [targetsBusy, setTargetsBusy] = useState(false);
  const [targetError, setTargetError] = useState<string | null>(null);
  const [exportError, setExportError] = useState<string | null>(null);

  const suite = useMemo(() => suites.find((candidate) => candidate.id === selectedSuiteId) ?? null, [suites, selectedSuiteId]);
  const suiteRuns = useMemo(() => suite ? runs.filter((run) => run.suiteId === suite.id) : [], [runs, suite]);
  const activeRun = useMemo(() => runs.find((run) => run.id === activeRunId) ?? null, [runs, activeRunId]);
  const selectedRun = suiteRuns.find((run) => run.id === selectedRunId) ?? suiteRuns[0] ?? null;
  const selectedCase = suite?.cases.find((testCase) => testCase.id === selectedCaseId) ?? suite?.cases[0] ?? null;
  const gate = suite ? releaseGateStatus(suite, runs) : null;

  useEffect(() => {
    setSelectedCaseId(suite?.cases[0]?.id ?? null);
    setSelectedRunId(null);
  }, [suite?.id]);

  const loadTargets = useCallback(async () => {
    setTargetsBusy(true);
    const errors: string[] = [];
    try {
      await refreshNativeSkills();
    } catch (error) {
      errors.push(`Skills: ${errorMessage(error)}`);
    }
    try {
      setWorkflows(await ecosystemClient.workflows());
    } catch (error) {
      errors.push(`Workflows: ${errorMessage(error)}`);
    }
    try {
      await refreshMcp();
    } catch (error) {
      errors.push(`Connectors: ${errorMessage(error)}`);
    }
    setTargetError(errors.length > 0 ? errors.join(" · ") : null);
    setTargetsBusy(false);
  }, [refreshMcp, refreshNativeSkills]);

  useEffect(() => { void loadTargets(); }, [loadTargets]);

  const exportArtifact = async (defaultPath: string, content: string) => {
    setExportError(null);
    try {
      const destination = await save({ defaultPath, filters: [{ name: "JSON", extensions: ["json"] }] });
      if (destination) await writeTextFile(destination, `${content}\n`);
    } catch (error) {
      setExportError(errorMessage(error));
    }
  };

  const runSelectedSuite = async () => {
    if (!suite) return;
    setSelectedRunId(null);
    try {
      await runSuite(suite.id);
    } catch {
      // The durable store exposes the concrete validation/execution error.
    }
  };

  const connectedServers = servers.filter((server) => server.status === "connected" && server.tools.length > 0);

  return (
    <section className="flex min-h-0 flex-1 flex-col" aria-labelledby="eval-harness-title">
      <header className="flex shrink-0 items-center justify-between gap-3 border-b border-border px-4 py-3">
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <FlaskConical size={17} className="text-accent" />
            <h1 id="eval-harness-title" className="truncate text-base font-semibold text-foreground">Workflow & Agent Test Harness</h1>
          </div>
          <p className="truncate text-xs text-muted">Executable local regression suites with evidence-backed release gates.</p>
        </div>
        <div className="flex shrink-0 items-center gap-2">
          {suite && gate && <StatusPill tone={gateTone(gate.status)}>Gate: {gate.status}</StatusPill>}
          {activeRunId ? (
            <Button size="sm" variant="danger" onClick={cancelRun}><Square size={12} /> Cancel run</Button>
          ) : (
            <Button size="sm" variant="primary" onClick={() => void runSelectedSuite()} disabled={!suite}><Play size={13} /> Run suite</Button>
          )}
          <IconButton size="sm" onClick={onClose} aria-label="Close test harness"><X size={16} /></IconButton>
        </div>
      </header>

      {(storeError || targetError || exportError) && (
        <div role="alert" className="flex items-start justify-between gap-3 border-b border-danger/30 bg-danger-soft px-4 py-2 text-xs text-danger">
          <span>{storeError ?? targetError ?? exportError}</span>
          <button type="button" className="underline" onClick={() => { clearError(); setTargetError(null); setExportError(null); }}>Dismiss</button>
        </div>
      )}

      <div className="flex min-h-0 flex-1 overflow-hidden">
        <aside className="flex w-56 shrink-0 flex-col border-r border-border bg-surface">
          <div className="flex items-center justify-between border-b border-border px-3 py-2">
            <span className="text-xs font-semibold uppercase tracking-wide text-faint">Suites</span>
            <IconButton size="sm" onClick={() => createSuite()} aria-label="Create eval suite"><Plus size={14} /></IconButton>
          </div>
          <div className="min-h-0 flex-1 space-y-1 overflow-y-auto p-2">
            {suites.length === 0 && <p className="px-2 py-4 text-center text-xs text-faint">Create a suite to start testing.</p>}
            {suites.map((candidate) => {
              const latest = runs.find((run) => run.suiteId === candidate.id);
              return (
                <button key={candidate.id} type="button" onClick={() => selectSuite(candidate.id)} className={`w-full rounded-md px-2 py-2 text-left ${candidate.id === suite?.id ? "bg-accent-soft" : "hover:bg-surface-2"}`}>
                  <span className="block truncate text-sm font-medium text-foreground">{candidate.name}</span>
                  <span className="mt-0.5 flex items-center justify-between text-[11px] text-faint">
                    <span>{candidate.cases.filter((entry) => entry.enabled).length} cases · r{candidate.revision}</span>
                    {latest && <span className={latest.status === "passed" ? "text-success" : latest.status === "running" ? "text-warning" : "text-danger"}>{latest.status}</span>}
                  </span>
                </button>
              );
            })}
          </div>
        </aside>

        {!suite ? (
          <div className="flex min-w-0 flex-1 flex-col items-center justify-center gap-2 text-center">
            <FlaskConical size={30} className="text-faint" />
            <p className="text-sm font-medium text-foreground">No eval suite selected</p>
            <Button size="sm" variant="primary" onClick={() => createSuite()}><Plus size={13} /> Create suite</Button>
          </div>
        ) : (
          <>
            <main className="flex min-w-[28rem] flex-1 flex-col overflow-hidden">
              <div className="space-y-3 border-b border-border p-3">
                <div className="flex gap-2">
                  <input aria-label="Suite name" className={`${FIELD_CLASS} min-w-0 flex-1 font-semibold`} value={suite.name} onChange={(event) => updateSuite(suite.id, { name: event.target.value })} />
                  <IconButton size="sm" onClick={() => duplicateSuite(suite.id)} aria-label="Duplicate suite"><Copy size={14} /></IconButton>
                  <IconButton size="sm" variant="danger" onClick={() => deleteSuite(suite.id)} aria-label="Delete suite"><Trash2 size={14} /></IconButton>
                </div>
                <input aria-label="Suite description" className={FIELD_CLASS} value={suite.description} onChange={(event) => updateSuite(suite.id, { description: event.target.value })} placeholder="What regression or release risk does this suite cover?" />
                <div className="grid gap-2 lg:grid-cols-[10rem_1fr_auto]">
                  <select aria-label="Target kind" className={FIELD_CLASS} value={suite.target.kind} onChange={(event) => updateSuite(suite.id, { target: defaultTarget(event.target.value as EvalTarget["kind"], skills, workflows, servers) })}>
                    <option value="model">Active model</option>
                    <option value="agent">Active agent</option>
                    <option value="skill">Installed skill</option>
                    <option value="connector">MCP connector</option>
                    <option value="workflow">Workflow</option>
                  </select>
                  {suite.target.kind === "skill" ? (
                    <select aria-label="Skill target" className={FIELD_CLASS} value={suite.target.command} onChange={(event) => updateSuite(suite.id, { target: { kind: "skill", command: event.target.value } })}>
                      <option value="">Select an eligible enabled skill</option>
                      {skills.map((skill) => <option key={skill.id} value={skill.command}>/{skill.command} · {skill.name}</option>)}
                    </select>
                  ) : suite.target.kind === "workflow" ? (
                    <select aria-label="Workflow target" className={FIELD_CLASS} value={suite.target.workflowId} onChange={(event) => updateSuite(suite.id, { target: { kind: "workflow", workflowId: event.target.value } })}>
                      <option value="">Select a workflow</option>
                      {workflows.map((workflow) => <option key={workflow.workflow_id} value={workflow.workflow_id}>{workflow.name} · v{workflow.workflow_version}</option>)}
                    </select>
                  ) : suite.target.kind === "connector" ? (
                    <div className="grid gap-2 sm:grid-cols-2">
                      <select aria-label="Connector server" className={FIELD_CLASS} value={suite.target.serverId} onChange={(event) => {
                        const server = connectedServers.find((candidate) => candidate.id === event.target.value);
                        updateSuite(suite.id, { target: { kind: "connector", serverId: event.target.value, toolName: server?.tools[0]?.name ?? "" } });
                      }}>
                        <option value="">Select connected server</option>
                        {connectedServers.map((server) => <option key={server.id} value={server.id}>{server.label}</option>)}
                      </select>
                      <select aria-label="Connector tool" className={FIELD_CLASS} value={suite.target.toolName} onChange={(event) => {
                        if (suite.target.kind !== "connector") return;
                        updateSuite(suite.id, { target: { kind: "connector", serverId: suite.target.serverId, toolName: event.target.value } });
                      }}>
                        <option value="">Select tool</option>
                        {connectedServers.find((server) => server.id === (suite.target.kind === "connector" ? suite.target.serverId : ""))?.tools.map((tool) => <option key={tool.name} value={tool.name}>{tool.name}</option>)}
                      </select>
                    </div>
                  ) : (
                    <div className="flex items-center rounded-md border border-border bg-surface px-2 text-xs text-muted">{formatTarget(suite.target)}</div>
                  )}
                  <IconButton size="sm" onClick={() => void loadTargets()} disabled={targetsBusy} aria-label="Refresh eval targets"><RefreshCw size={14} className={targetsBusy ? "animate-spin" : ""} /></IconButton>
                </div>
                <label className="flex items-center gap-2 text-xs text-muted">
                  <input type="checkbox" checked={suite.releaseGate} onChange={(event) => updateSuite(suite.id, { releaseGate: event.target.checked })} />
                  Use the latest passing run of this exact revision as a release gate
                </label>
              </div>

              <div className="flex shrink-0 items-center gap-1 overflow-x-auto border-b border-border px-3 py-2">
                {suite.cases.map((testCase, index) => (
                  <button key={testCase.id} type="button" onClick={() => setSelectedCaseId(testCase.id)} className={`shrink-0 rounded-md px-2 py-1 text-xs ${testCase.id === selectedCase?.id ? "bg-accent-soft text-accent" : "text-muted hover:bg-surface-2"}`}>
                    {testCase.enabled ? "" : "Paused · "}{index + 1}. {testCase.name || "Untitled"}
                  </button>
                ))}
                <IconButton size="sm" onClick={() => setSelectedCaseId(addCase(suite.id))} aria-label="Add eval case"><Plus size={13} /></IconButton>
                {selectedCase && <IconButton size="sm" onClick={() => { const id = duplicateCase(suite.id, selectedCase.id); if (id) setSelectedCaseId(id); }} aria-label="Duplicate eval case"><Copy size={13} /></IconButton>}
                {selectedCase && <IconButton size="sm" variant="danger" disabled={suite.cases.length <= 1} onClick={() => { deleteCase(suite.id, selectedCase.id); setSelectedCaseId(null); }} aria-label="Delete eval case"><Trash2 size={13} /></IconButton>}
              </div>
              <div className="min-h-0 flex-1 overflow-y-auto p-3 [overscroll-behavior:contain]">
                {selectedCase && <CaseEditor suiteId={suite.id} testCase={selectedCase} target={suite.target} />}
              </div>
            </main>

            <aside className="flex w-[22rem] shrink-0 flex-col border-l border-border bg-background">
              <div className="flex items-center justify-between border-b border-border px-3 py-2">
                <div className="flex items-center gap-1.5 text-xs font-semibold uppercase tracking-wide text-faint"><History size={13} /> Results</div>
                <div className="flex items-center gap-1">
                  <IconButton size="sm" disabled={!selectedRun} onClick={() => selectedRun && void exportArtifact(`${suite.name.replace(/[^a-z0-9]+/gi, "-").toLowerCase()}-run.json`, exportEvalRun(selectedRun))} aria-label="Export selected run"><Download size={13} /></IconButton>
                  <IconButton size="sm" onClick={() => void exportArtifact(`${suite.name.replace(/[^a-z0-9]+/gi, "-").toLowerCase()}-suite.json`, exportEvalSuite(suite))} aria-label="Export suite"><Download size={13} /></IconButton>
                </div>
              </div>
              {suiteRuns.length > 0 && (
                <div className="flex shrink-0 gap-1 overflow-x-auto border-b border-border p-2">
                  {suiteRuns.map((run) => (
                    <button key={run.id} type="button" onClick={() => setSelectedRunId(run.id)} className={`shrink-0 rounded px-2 py-1 text-[11px] ${run.id === selectedRun?.id ? "bg-accent-soft text-accent" : "bg-surface text-muted"}`}>
                      r{run.suiteRevision} · {run.status}
                    </button>
                  ))}
                  {!activeRunId && <button type="button" className="shrink-0 px-2 text-[11px] text-faint underline" onClick={() => clearHistory(suite.id)}>Clear</button>}
                </div>
              )}
              <div className="min-h-0 flex-1 overflow-y-auto p-3 [overscroll-behavior:contain]">
                {activeRun?.suiteId === suite.id && activeRun.results.length === 0 ? (
                  <div className="flex h-full flex-col items-center justify-center gap-2 text-center text-xs text-muted"><RefreshCw size={22} className="animate-spin text-accent" />Executing first case…</div>
                ) : selectedRun ? (
                  <RunReport run={selectedRun} />
                ) : (
                  <div className="flex h-full flex-col items-center justify-center gap-2 text-center">
                    <History size={24} className="text-faint" />
                    <p className="text-sm font-medium text-foreground">No run evidence yet</p>
                    <p className="max-w-56 text-xs text-muted">Run the suite to collect pass/fail assertions, outputs, usage, cost, latency, and reproducibility metadata.</p>
                  </div>
                )}
              </div>
            </aside>
          </>
        )}
      </div>
    </section>
  );
}

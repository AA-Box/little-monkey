import { useState } from "react";
import { ChevronDown, ChevronRight, Play, Plus, RotateCcw, Trash2, X } from "lucide-react";

import { useT } from "../../lib/i18n";
import type { FixtureSourceType, RedTeamFixture } from "../../lib/redTeamFixtures";
import { useRedTeamStore, type CustomFixtureDraft } from "../../store/redTeamStore";
import type { PermissionMode } from "../../store/permissionStore";
import { Button, IconButton, StatusPill, type PillTone } from "../ui";

interface RedTeamLabPanelProps {
  onClose: () => void;
}

const TEST_MODES: PermissionMode[] = ["manual", "acceptEdits", "smart", "auto", "bypass"];

const MODE_LABEL_KEY: Record<PermissionMode, string> = {
  manual: "ModeSelector.modeManualLabel",
  acceptEdits: "ModeSelector.modeAcceptEditsLabel",
  smart: "ModeSelector.modeSmartLabel",
  plan: "ModeSelector.modePlanLabel",
  auto: "ModeSelector.modeAutoLabel",
  bypass: "ModeSelector.modeBypassLabel",
};

const SOURCE_TYPES: FixtureSourceType[] = [
  "webpage",
  "email",
  "mcp_tool_output",
  "repo_file",
  "connector_payload",
  "pdf_document",
  "screenshot_ocr",
  "knowledge_source",
  "web_search_result",
  "subagent_output",
];

const SOURCE_LABEL_KEY: Record<FixtureSourceType, string> = {
  webpage: "RedTeamLab.source.webpage",
  email: "RedTeamLab.source.email",
  mcp_tool_output: "RedTeamLab.source.mcpToolOutput",
  repo_file: "RedTeamLab.source.repoFile",
  connector_payload: "RedTeamLab.source.connectorPayload",
  pdf_document: "RedTeamLab.source.pdfDocument",
  screenshot_ocr: "RedTeamLab.source.screenshotOcr",
  knowledge_source: "RedTeamLab.source.knowledgeSource",
  web_search_result: "RedTeamLab.source.webSearchResult",
  subagent_output: "RedTeamLab.source.subagentOutput",
};

const EMPTY_DRAFT: CustomFixtureDraft = {
  title: "",
  sourceType: "webpage",
  simulatedToolName: "web_fetch",
  isMcp: false,
  content: "",
  rawControlToken: "",
  triggeredActionTool: "run_shell",
  triggeredActionArgsJson: "{}",
  triggeredActionDescription: "",
  expectedOutcome: "requires_approval",
};

function inputClass(extra = ""): string {
  return `h-8 min-w-0 rounded-md border border-border bg-surface px-2.5 text-sm text-foreground placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent ${extra}`;
}

export function RedTeamLabPanel({ onClose }: RedTeamLabPanelProps) {
  const { t } = useT();
  const fixtures = useRedTeamStore((s) => s.fixtures);
  const results = useRedTeamStore((s) => s.results);
  const mode = useRedTeamStore((s) => s.mode);
  const running = useRedTeamStore((s) => s.running);
  const formError = useRedTeamStore((s) => s.formError);
  const setMode = useRedTeamStore((s) => s.setMode);
  const runAll = useRedTeamStore((s) => s.runAll);
  const runOne = useRedTeamStore((s) => s.runOne);
  const clearResults = useRedTeamStore((s) => s.clearResults);
  const addFixture = useRedTeamStore((s) => s.addFixture);
  const removeFixture = useRedTeamStore((s) => s.removeFixture);

  const [expandedId, setExpandedId] = useState<string | null>(null);
  const [addFormOpen, setAddFormOpen] = useState(false);
  const [draft, setDraft] = useState<CustomFixtureDraft>(EMPTY_DRAFT);

  const total = fixtures.length;
  const ran = Object.keys(results).length;
  const passed = Object.values(results).filter((r) => r.pass).length;
  const failed = ran - passed;

  const handleAddFixture = () => {
    const ok = addFixture(draft);
    if (ok) {
      setDraft(EMPTY_DRAFT);
      setAddFormOpen(false);
    }
  };

  return (
    <section className="flex min-h-0 flex-1 flex-col" aria-labelledby="red-team-lab-title">
      <header className="flex shrink-0 items-center justify-between gap-3 border-b border-border px-4 py-3">
        <div className="min-w-0">
          <h1 id="red-team-lab-title" className="text-base font-semibold text-foreground">
            {t("RedTeamLab.title")}
          </h1>
          <p className="truncate text-xs text-muted">{t("RedTeamLab.subtitle")}</p>
        </div>
        <div className="flex shrink-0 items-center gap-2">
          <label className="flex items-center gap-1.5 text-xs text-muted">
            {t("RedTeamLab.modeLabel")}
            <select
              value={mode}
              onChange={(event) => setMode(event.target.value as PermissionMode)}
              className={inputClass("px-2 py-0")}
            >
              {TEST_MODES.map((m) => (
                <option key={m} value={m}>
                  {t(MODE_LABEL_KEY[m])}
                </option>
              ))}
            </select>
          </label>
          <Button variant="secondary" size="sm" onClick={() => clearResults()} disabled={ran === 0}>
            <RotateCcw size={14} /> {t("RedTeamLab.clearResults")}
          </Button>
          <Button variant="primary" size="sm" onClick={() => runAll()} disabled={running}>
            <Play size={14} /> {running ? t("RedTeamLab.running") : t("RedTeamLab.runAll")}
          </Button>
          <IconButton size="sm" onClick={onClose} aria-label={t("RedTeamLab.close")}>
            <X size={16} />
          </IconButton>
        </div>
      </header>

      {ran > 0 && (
        <div className="flex shrink-0 items-center gap-3 border-b border-border bg-surface px-4 py-2 text-xs">
          <span className="text-muted">{t("RedTeamLab.summary", { total, ran })}</span>
          <StatusPill tone="success">{t("RedTeamLab.summaryPassed", { count: passed })}</StatusPill>
          {failed > 0 && <StatusPill tone="danger">{t("RedTeamLab.summaryFailed", { count: failed })}</StatusPill>}
        </div>
      )}

      <div className="min-h-0 flex-1 overflow-y-auto">
        <table className="w-full border-collapse text-left text-sm">
          <thead className="sticky top-0 bg-background text-xs uppercase tracking-wider text-faint">
            <tr>
              <th className="w-8 border-b border-border px-2 py-2" />
              <th className="border-b border-border px-3 py-2">{t("RedTeamLab.columnSource")}</th>
              <th className="border-b border-border px-3 py-2">{t("RedTeamLab.columnFixture")}</th>
              <th className="border-b border-border px-3 py-2">{t("RedTeamLab.columnTriggeredAction")}</th>
              <th className="border-b border-border px-3 py-2">{t("RedTeamLab.columnExpected")}</th>
              <th className="border-b border-border px-3 py-2">{t("RedTeamLab.columnActual")}</th>
              <th className="border-b border-border px-3 py-2">{t("RedTeamLab.columnResult")}</th>
              <th className="w-24 border-b border-border px-3 py-2" />
            </tr>
          </thead>
          <tbody>
            {fixtures.map((fixture) => {
              const result = results[fixture.id];
              const expanded = expandedId === fixture.id;
              const tone: PillTone = !result ? "neutral" : result.pass ? "success" : "danger";
              return (
                <FixtureRows
                  key={fixture.id}
                  fixture={fixture}
                  result={result}
                  expanded={expanded}
                  tone={tone}
                  onToggleExpand={() => setExpandedId(expanded ? null : fixture.id)}
                  onRun={() => runOne(fixture.id)}
                  onRemove={() => removeFixture(fixture.id)}
                />
              );
            })}
          </tbody>
        </table>
      </div>

      <div className="shrink-0 border-t border-border bg-surface">
        <button
          type="button"
          onClick={() => setAddFormOpen((prev) => !prev)}
          className="flex w-full items-center gap-2 px-4 py-2 text-left text-sm text-foreground hover:bg-surface-2"
        >
          <Plus size={14} className="text-faint" />
          {t("RedTeamLab.addFixtureToggle")}
          {addFormOpen ? <ChevronDown size={14} className="ml-auto text-faint" /> : <ChevronRight size={14} className="ml-auto text-faint" />}
        </button>

        {addFormOpen && (
          <div className="flex flex-col gap-2 border-t border-border px-4 py-3">
            {formError && <p className="text-xs text-danger">{formError}</p>}
            <div className="flex flex-wrap gap-2">
              <input
                type="text"
                value={draft.title}
                onChange={(e) => setDraft({ ...draft, title: e.target.value })}
                placeholder={t("RedTeamLab.form.titlePlaceholder")}
                className={inputClass("flex-[2]")}
              />
              <select
                value={draft.sourceType}
                onChange={(e) => setDraft({ ...draft, sourceType: e.target.value as FixtureSourceType })}
                className={inputClass("flex-1")}
              >
                {SOURCE_TYPES.map((s) => (
                  <option key={s} value={s}>
                    {t(SOURCE_LABEL_KEY[s])}
                  </option>
                ))}
              </select>
            </div>
            <div className="flex flex-wrap gap-2">
              <input
                type="text"
                value={draft.simulatedToolName}
                onChange={(e) => setDraft({ ...draft, simulatedToolName: e.target.value })}
                placeholder={t("RedTeamLab.form.simulatedToolPlaceholder")}
                className={inputClass("flex-1 font-mono")}
              />
              <label className="flex items-center gap-1.5 text-xs text-muted">
                <input
                  type="checkbox"
                  checked={draft.isMcp}
                  onChange={(e) => setDraft({ ...draft, isMcp: e.target.checked })}
                />
                {t("RedTeamLab.form.isMcpLabel")}
              </label>
              <select
                value={draft.expectedOutcome}
                onChange={(e) =>
                  setDraft({ ...draft, expectedOutcome: e.target.value as RedTeamFixture["expectedOutcome"] })
                }
                className={inputClass("")}
              >
                <option value="requires_approval">{t("RedTeamLab.form.expectedRequiresApproval")}</option>
                <option value="blocked">{t("RedTeamLab.form.expectedBlocked")}</option>
              </select>
            </div>
            <textarea
              value={draft.content}
              onChange={(e) => setDraft({ ...draft, content: e.target.value })}
              placeholder={t("RedTeamLab.form.contentPlaceholder")}
              rows={3}
              className="w-full resize-y rounded-md border border-border bg-surface px-2.5 py-1.5 font-mono text-xs text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
            />
            <input
              type="text"
              value={draft.rawControlToken}
              onChange={(e) => setDraft({ ...draft, rawControlToken: e.target.value })}
              placeholder={t("RedTeamLab.form.rawControlTokenPlaceholder")}
              className={inputClass("w-full font-mono")}
            />
            <div className="flex flex-wrap gap-2">
              <input
                type="text"
                value={draft.triggeredActionTool}
                onChange={(e) => setDraft({ ...draft, triggeredActionTool: e.target.value })}
                placeholder={t("RedTeamLab.form.triggeredToolPlaceholder")}
                className={inputClass("flex-1 font-mono")}
              />
              <input
                type="text"
                value={draft.triggeredActionDescription}
                onChange={(e) => setDraft({ ...draft, triggeredActionDescription: e.target.value })}
                placeholder={t("RedTeamLab.form.triggeredDescriptionPlaceholder")}
                className={inputClass("flex-[2]")}
              />
            </div>
            <input
              type="text"
              value={draft.triggeredActionArgsJson}
              onChange={(e) => setDraft({ ...draft, triggeredActionArgsJson: e.target.value })}
              placeholder={t("RedTeamLab.form.triggeredArgsPlaceholder")}
              className={inputClass("w-full font-mono")}
            />
            <div className="flex justify-end">
              <Button variant="primary" size="sm" onClick={handleAddFixture}>
                <Plus size={14} /> {t("RedTeamLab.form.submit")}
              </Button>
            </div>
          </div>
        )}
      </div>
    </section>
  );
}

interface FixtureRowsProps {
  fixture: RedTeamFixture;
  result: ReturnType<typeof useRedTeamStore.getState>["results"][string] | undefined;
  expanded: boolean;
  tone: PillTone;
  onToggleExpand: () => void;
  onRun: () => void;
  onRemove: () => void;
}

function FixtureRows({ fixture, result, expanded, tone, onToggleExpand, onRun, onRemove }: FixtureRowsProps) {
  const { t } = useT();
  return (
    <>
      <tr className="border-b border-border/60 align-top hover:bg-surface-2/40">
        <td className="px-2 py-2">
          <button
            type="button"
            onClick={onToggleExpand}
            aria-label={expanded ? t("RedTeamLab.collapseRow") : t("RedTeamLab.expandRow")}
            className="text-faint hover:text-foreground"
          >
            {expanded ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
          </button>
        </td>
        <td className="px-3 py-2 text-xs text-muted">{t(SOURCE_LABEL_KEY[fixture.sourceType])}</td>
        <td className="max-w-xs px-3 py-2">
          <span className="block truncate font-medium text-foreground">{fixture.title}</span>
          {!fixture.builtin && <StatusPill tone="neutral">{t("RedTeamLab.customBadge")}</StatusPill>}
        </td>
        <td className="max-w-xs px-3 py-2 text-xs text-muted">
          <code className="font-mono">{fixture.triggeredAction.tool}</code> — {fixture.triggeredAction.description}
        </td>
        <td className="px-3 py-2 text-xs text-muted">
          {fixture.expectedOutcome === "blocked" ? t("RedTeamLab.expectedBlocked") : t("RedTeamLab.expectedRequiresApproval")}
        </td>
        <td className="px-3 py-2 text-xs text-muted">
          {result ? (
            result.gate.decision === "auto_approved" ? (
              t("RedTeamLab.actualAutoApproved")
            ) : result.gate.decision === "blocked" ? (
              t("RedTeamLab.actualBlocked")
            ) : (
              t("RedTeamLab.actualRequiresPrompt")
            )
          ) : (
            t("RedTeamLab.actualNotRun")
          )}
        </td>
        <td className="px-3 py-2">
          <StatusPill tone={tone}>
            {!result ? t("RedTeamLab.resultPending") : result.pass ? t("RedTeamLab.resultPass") : t("RedTeamLab.resultFail")}
          </StatusPill>
        </td>
        <td className="px-3 py-2">
          <div className="flex items-center justify-end gap-1">
            <IconButton size="sm" onClick={onRun} aria-label={t("RedTeamLab.runOne")}>
              <Play size={13} />
            </IconButton>
            {!fixture.builtin && (
              <IconButton size="sm" onClick={onRemove} aria-label={t("RedTeamLab.removeFixture")}>
                <Trash2 size={13} />
              </IconButton>
            )}
          </div>
        </td>
      </tr>
      {expanded && (
        <tr className="border-b border-border/60 bg-surface-2/30">
          <td />
          <td colSpan={7} className="px-3 py-3 text-xs text-muted">
            <div className="flex flex-col gap-2">
              <div>
                <span className="font-semibold text-foreground">{t("RedTeamLab.detailContent")}</span>
                <pre className="mt-1 max-h-32 overflow-auto whitespace-pre-wrap rounded-md border border-border bg-background p-2 font-mono text-[11px]">
                  {fixture.content}
                </pre>
              </div>
              {result && (
                <>
                  <div>
                    <span className="font-semibold text-foreground">{t("RedTeamLab.detailContainment")}</span>{" "}
                    {result.containment.reason}
                  </div>
                  <div>
                    <span className="font-semibold text-foreground">{t("RedTeamLab.detailGate")}</span>{" "}
                    {result.gate.reason}
                  </div>
                  {result.failureReason && (
                    <div className="text-danger">
                      <span className="font-semibold">{t("RedTeamLab.detailFailureReason")}</span> {result.failureReason}
                    </div>
                  )}
                </>
              )}
            </div>
          </td>
        </tr>
      )}
    </>
  );
}

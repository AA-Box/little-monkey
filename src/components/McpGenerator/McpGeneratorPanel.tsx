import { useMemo, useState } from "react";
import {
  AlertTriangle,
  CheckCircle2,
  Download,
  FlaskConical,
  Loader2,
  Play,
  Plus,
  Trash2,
  X,
  XCircle,
} from "lucide-react";

import { useT } from "../../lib/i18n";
import { validateServerSpec, type McpParamType, type McpSourceKind } from "../../lib/mcpGenerator";
import { useMcpGeneratorStore } from "../../store/mcpGeneratorStore";
import { Button, IconButton, StatusPill, type PillTone } from "../ui";

interface McpGeneratorPanelProps {
  onClose: () => void;
}

const PARAM_TYPES: McpParamType[] = ["string", "number", "boolean", "array", "object"];
const SOURCE_KINDS: McpSourceKind[] = ["api", "cli", "script", "workflow"];

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

export function McpGeneratorPanel({ onClose }: McpGeneratorPanelProps) {
  const { t } = useT();
  const store = useMcpGeneratorStore();
  const [localError, setLocalError] = useState<string | null>(null);

  const selected = useMemo(
    () => store.entries.find((entry) => entry.id === store.selectedEntryId) ?? null,
    [store.entries, store.selectedEntryId],
  );

  const draftIssues = useMemo(() => validateServerSpec(store.draft), [store.draft]);

  return (
    <section className="flex h-full min-h-0 flex-col" aria-labelledby="mcp-generator-title">
      <header className="flex shrink-0 items-start justify-between gap-3 border-b border-border px-5 py-4">
        <div>
          <h2 id="mcp-generator-title" className="text-sm font-semibold text-foreground">
            {t("McpGenerator.title")}
          </h2>
          <p className="mt-1 max-w-2xl text-xs leading-5 text-muted">{t("McpGenerator.subtitle")}</p>
        </div>
        <IconButton size="sm" aria-label={t("McpGenerator.close")} title={t("McpGenerator.close")} onClick={onClose}>
          <X size={15} />
        </IconButton>
      </header>

      {(store.error || localError) && (
        <div role="alert" className="mx-5 mt-3 whitespace-pre-wrap rounded-md border border-danger/40 bg-danger/10 p-3 text-xs text-danger">
          {store.error ?? localError}
        </div>
      )}

      <div className="grid min-h-0 flex-1 gap-4 overflow-hidden p-5 xl:grid-cols-[minmax(20rem,1fr)_minmax(0,1.3fr)]">
        {/* Left: the generator form + past generated servers */}
        <div className="flex min-h-0 flex-col gap-3 overflow-y-auto">
          <div className="rounded-lg border border-border bg-surface p-3">
            <h3 className="text-xs font-semibold text-foreground">{t("McpGenerator.formHeading")}</h3>

            <label className="mt-2 block text-xs text-muted">
              {t("McpGenerator.nameLabel")}
              <input
                className="mt-1 w-full rounded-md border border-border bg-background px-2.5 py-1.5 text-sm text-foreground outline-none focus:border-accent"
                placeholder={t("McpGenerator.namePlaceholder")}
                value={store.draft.name}
                onChange={(event) => store.updateDraft({ name: event.target.value })}
              />
            </label>

            <label className="mt-2 block text-xs text-muted">
              {t("McpGenerator.descriptionLabel")}
              <textarea
                className="mt-1 w-full rounded-md border border-border bg-background px-2.5 py-1.5 text-sm text-foreground outline-none focus:border-accent"
                rows={2}
                placeholder={t("McpGenerator.descriptionPlaceholder")}
                value={store.draft.description}
                onChange={(event) => store.updateDraft({ description: event.target.value })}
              />
            </label>

            <div className="mt-2 grid grid-cols-2 gap-2">
              <label className="block text-xs text-muted">
                {t("McpGenerator.sourceKindLabel")}
                <select
                  className="mt-1 w-full rounded-md border border-border bg-background px-2.5 py-1.5 text-sm text-foreground outline-none focus:border-accent"
                  value={store.draft.sourceKind}
                  onChange={(event) => store.updateDraft({ sourceKind: event.target.value as McpSourceKind })}
                >
                  {SOURCE_KINDS.map((kind) => (
                    <option key={kind} value={kind}>
                      {t(`McpGenerator.sourceKind.${kind}`)}
                    </option>
                  ))}
                </select>
              </label>
              <label className="block text-xs text-muted">
                {t("McpGenerator.targetLabel")}
                <input
                  className="mt-1 w-full rounded-md border border-border bg-background px-2.5 py-1.5 text-sm text-foreground outline-none focus:border-accent"
                  placeholder={t("McpGenerator.targetPlaceholder")}
                  value={store.draft.target}
                  onChange={(event) => store.updateDraft({ target: event.target.value })}
                />
              </label>
            </div>

            <div className="mt-3 flex items-center justify-between">
              <h4 className="text-xs font-semibold text-foreground">{t("McpGenerator.toolsHeading")}</h4>
              <Button size="sm" onClick={() => store.addTool()}>
                <Plus size={13} /> {t("McpGenerator.addToolButton")}
              </Button>
            </div>

            <div className="mt-2 space-y-3">
              {store.draft.tools.map((tool, toolIndex) => (
                <div key={toolIndex} className="rounded-md border border-border bg-background p-2.5">
                  <div className="flex items-start gap-2">
                    <div className="flex-1 space-y-1.5">
                      <input
                        className="w-full rounded-md border border-border bg-surface px-2 py-1 font-mono text-xs text-foreground outline-none focus:border-accent"
                        placeholder={t("McpGenerator.toolNamePlaceholder")}
                        value={tool.name}
                        onChange={(event) => store.updateTool(toolIndex, { name: event.target.value })}
                      />
                      <input
                        className="w-full rounded-md border border-border bg-surface px-2 py-1 text-xs text-foreground outline-none focus:border-accent"
                        placeholder={t("McpGenerator.toolDescriptionPlaceholder")}
                        value={tool.description}
                        onChange={(event) => store.updateTool(toolIndex, { description: event.target.value })}
                      />
                      <label className="flex items-center gap-1.5 text-[11px] text-muted">
                        <input
                          type="checkbox"
                          checked={tool.requiresAuth}
                          onChange={(event) => store.updateTool(toolIndex, { requiresAuth: event.target.checked })}
                        />
                        {t("McpGenerator.requiresAuthLabel")}
                      </label>
                    </div>
                    <IconButton
                      size="sm"
                      aria-label={t("McpGenerator.removeToolButton")}
                      title={t("McpGenerator.removeToolButton")}
                      onClick={() => store.removeTool(toolIndex)}
                    >
                      <Trash2 size={13} />
                    </IconButton>
                  </div>

                  <div className="mt-2 space-y-1.5 border-t border-border pt-2">
                    {tool.params.map((param, paramIndex) => (
                      <div key={paramIndex} className="flex items-center gap-1.5">
                        <input
                          className="w-24 min-w-0 flex-1 rounded-md border border-border bg-surface px-1.5 py-1 font-mono text-[11px] text-foreground outline-none focus:border-accent"
                          placeholder={t("McpGenerator.paramNamePlaceholder")}
                          value={param.name}
                          onChange={(event) => store.updateParam(toolIndex, paramIndex, { name: event.target.value })}
                        />
                        <select
                          className="rounded-md border border-border bg-surface px-1 py-1 text-[11px] text-foreground outline-none focus:border-accent"
                          value={param.type}
                          onChange={(event) =>
                            store.updateParam(toolIndex, paramIndex, { type: event.target.value as McpParamType })
                          }
                        >
                          {PARAM_TYPES.map((type) => (
                            <option key={type} value={type}>
                              {type}
                            </option>
                          ))}
                        </select>
                        <label className="flex items-center gap-1 text-[10px] text-muted">
                          <input
                            type="checkbox"
                            checked={param.required}
                            onChange={(event) =>
                              store.updateParam(toolIndex, paramIndex, { required: event.target.checked })
                            }
                          />
                          {t("McpGenerator.requiredLabel")}
                        </label>
                        <IconButton
                          size="sm"
                          aria-label={t("McpGenerator.removeParamButton")}
                          title={t("McpGenerator.removeParamButton")}
                          onClick={() => store.removeParam(toolIndex, paramIndex)}
                        >
                          <X size={12} />
                        </IconButton>
                      </div>
                    ))}
                    <Button size="sm" onClick={() => store.addParam(toolIndex)}>
                      <Plus size={12} /> {t("McpGenerator.addParamButton")}
                    </Button>
                  </div>
                </div>
              ))}
            </div>

            {draftIssues.length > 0 && (
              <ul className="mt-3 list-disc space-y-0.5 rounded-md border border-warning/40 bg-warning/5 p-2.5 pl-6 text-[11px] text-warning">
                {draftIssues.map((issue) => (
                  <li key={issue}>{issue}</li>
                ))}
              </ul>
            )}

            <div className="mt-3 flex justify-end gap-2">
              <Button size="sm" onClick={() => store.resetDraft()}>
                {t("McpGenerator.resetButton")}
              </Button>
              <Button
                size="sm"
                variant="primary"
                disabled={store.generating || draftIssues.length > 0}
                onClick={() => {
                  setLocalError(null);
                  void store.generate().catch((error) => setLocalError(errorText(error)));
                }}
              >
                {store.generating ? <Loader2 className="animate-spin" size={13} /> : <Play size={13} />}
                {t("McpGenerator.generateButton")}
              </Button>
            </div>
          </div>

          <div className="rounded-lg border border-border bg-surface p-3">
            <h3 className="text-xs font-semibold text-foreground">{t("McpGenerator.generatedHeading")}</h3>
            {store.entries.length === 0 ? (
              <p className="mt-2 rounded-md border border-dashed border-border p-4 text-center text-xs text-faint">
                {t("McpGenerator.emptyGenerated")}
              </p>
            ) : (
              <div className="mt-2 space-y-1.5">
                {store.entries.map((entry) => (
                  <button
                    key={entry.id}
                    type="button"
                    onClick={() => store.selectEntry(entry.id)}
                    className={`w-full rounded-md border p-2 text-left transition-colors ${
                      entry.id === store.selectedEntryId
                        ? "border-accent bg-accent/10"
                        : "border-border bg-background hover:border-border-strong"
                    }`}
                  >
                    <div className="flex items-center justify-between gap-2">
                      <p className="truncate text-xs font-medium text-foreground">{entry.spec.name}</p>
                      {readyPill(entry.ready, entry.simulation !== null, t)}
                    </div>
                    <p className="mt-0.5 truncate text-[11px] text-muted">
                      {t("McpGenerator.toolCount", { count: entry.spec.tools.length })}
                    </p>
                  </button>
                ))}
              </div>
            )}
          </div>
        </div>

        {/* Right: generated code preview + simulator report */}
        <div className="flex min-h-0 flex-col overflow-hidden rounded-lg border border-border bg-surface">
          {!selected ? (
            <p className="p-8 text-center text-xs text-faint">{t("McpGenerator.noSelectionHint")}</p>
          ) : (
            <div className="flex min-h-0 flex-1 flex-col overflow-y-auto p-4">
              <div className="flex flex-wrap items-start justify-between gap-2">
                <div>
                  <h3 className="text-sm font-semibold text-foreground">{selected.spec.name}</h3>
                  <p className="mt-1 text-[11px] text-muted">{selected.spec.description}</p>
                </div>
                {readyPill(selected.ready, selected.simulation !== null, t)}
              </div>

              <div className="mt-3 flex flex-wrap gap-2">
                <Button
                  size="sm"
                  disabled={store.simulating}
                  onClick={() => store.runSimulator(selected.id)}
                >
                  {store.simulating ? <Loader2 className="animate-spin" size={13} /> : <FlaskConical size={13} />}
                  {t("McpGenerator.runSimulatorButton")}
                </Button>
                <Button
                  size="sm"
                  variant="primary"
                  disabled={!selected.ready || store.saving}
                  title={selected.ready ? undefined : t("McpGenerator.saveBlockedHint")}
                  onClick={() => {
                    setLocalError(null);
                    void store.saveToDisk(selected.id).catch((error) => setLocalError(errorText(error)));
                  }}
                >
                  {store.saving ? <Loader2 className="animate-spin" size={13} /> : <Download size={13} />}
                  {t("McpGenerator.saveButton")}
                </Button>
                <Button size="sm" variant="danger" onClick={() => store.removeEntry(selected.id)}>
                  <Trash2 size={13} /> {t("McpGenerator.deleteEntryButton")}
                </Button>
              </div>

              {selected.savedPath && (
                <p className="mt-2 break-all rounded-md border border-success/40 bg-success-soft p-2 text-[11px] text-success">
                  {t("McpGenerator.savedAt", { path: selected.savedPath })}
                </p>
              )}

              {!selected.ready && selected.simulation && (
                <p className="mt-2 flex items-start gap-1.5 rounded-md border border-danger/40 bg-danger/10 p-2.5 text-[11px] text-danger">
                  <AlertTriangle size={13} className="mt-0.5 shrink-0" />
                  {t("McpGenerator.notSimulatorCleanWarning")}
                </p>
              )}
              {!selected.simulation && (
                <p className="mt-2 flex items-start gap-1.5 rounded-md border border-warning/40 bg-warning/5 p-2.5 text-[11px] text-warning">
                  <AlertTriangle size={13} className="mt-0.5 shrink-0" />
                  {t("McpGenerator.notSimulatedYetWarning")}
                </p>
              )}

              {selected.simulation && (
                <div className="mt-3">
                  <h4 className="text-xs font-semibold text-foreground">
                    {t("McpGenerator.simulationHeading", {
                      passed: selected.simulation.passCount,
                      total: selected.simulation.results.length,
                    })}
                  </h4>
                  <div className="mt-2 overflow-x-auto rounded-md border border-border">
                    <table className="w-full min-w-[36rem] border-collapse text-left text-[11px]">
                      <thead className="bg-surface-2 text-faint">
                        <tr>
                          <th className="px-2 py-1.5 font-medium">{t("McpGenerator.tableTool")}</th>
                          <th className="px-2 py-1.5 font-medium">{t("McpGenerator.tableFixture")}</th>
                          <th className="px-2 py-1.5 font-medium">{t("McpGenerator.tableOutcome")}</th>
                          <th className="px-2 py-1.5 font-medium">{t("McpGenerator.tableReason")}</th>
                        </tr>
                      </thead>
                      <tbody>
                        {selected.simulation.results.map((result) => (
                          <tr key={result.fixture.id} className="border-t border-border">
                            <td className="px-2 py-1.5 font-mono text-foreground">{result.fixture.toolName}</td>
                            <td className="px-2 py-1.5 text-muted">{result.fixture.label}</td>
                            <td className="px-2 py-1.5">
                              {result.outcome === "pass" ? (
                                <span className="inline-flex items-center gap-1 text-success">
                                  <CheckCircle2 size={12} /> {t("McpGenerator.pass")}
                                </span>
                              ) : (
                                <span className="inline-flex items-center gap-1 text-danger">
                                  <XCircle size={12} /> {t("McpGenerator.fail")}
                                </span>
                              )}
                              {result.injectionDetected && (
                                <span className="ml-1.5 inline-flex items-center gap-1 text-warning" title={t("McpGenerator.injectionDetectedHint")}>
                                  <AlertTriangle size={11} />
                                </span>
                              )}
                            </td>
                            <td className="px-2 py-1.5 text-faint">{result.reason}</td>
                          </tr>
                        ))}
                      </tbody>
                    </table>
                  </div>
                </div>
              )}

              {selected.code && (
                <div className="mt-3 min-h-0 flex-1">
                  <h4 className="text-xs font-semibold text-foreground">{t("McpGenerator.codeHeading")}</h4>
                  <pre className="mt-2 max-h-96 overflow-auto rounded-md border border-border bg-background p-3 text-[11px] leading-5 text-foreground">
                    <code>{selected.code}</code>
                  </pre>
                </div>
              )}
            </div>
          )}
        </div>
      </div>
    </section>
  );
}

function readyPill(ready: boolean, simulated: boolean, t: (key: string) => string) {
  const tone: PillTone = ready ? "success" : simulated ? "danger" : "warning";
  const label = ready
    ? t("McpGenerator.statusReady")
    : simulated
      ? t("McpGenerator.statusNotClean")
      : t("McpGenerator.statusNotSimulated");
  return <StatusPill tone={tone}>{label}</StatusPill>;
}

export default McpGeneratorPanel;

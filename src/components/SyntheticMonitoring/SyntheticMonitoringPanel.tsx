import { useEffect, useMemo, useState, type FormEvent } from "react";
import {
  AlertTriangle,
  Loader2,
  Pencil,
  Play,
  Plus,
  RefreshCw,
  Sparkles,
  Trash2,
  X,
} from "lucide-react";

import { useT } from "../../lib/i18n";
import { artifactDataUrl, readDurableArtifact } from "../../lib/durableArtifacts";
import type {
  MonitorAssertionType,
  MonitorRun,
  MonitorTargetEnv,
  SyntheticMonitor,
} from "../../lib/syntheticMonitoring";
import { useSyntheticMonitoringStore, type CreateMonitorInput } from "../../store/syntheticMonitoringStore";
import { Button, IconButton, StatusPill, type PillTone } from "../ui";
import { errorMessage } from "../../lib/errors";

interface SyntheticMonitoringPanelProps {
  onClose: () => void;
}

type WaitForKind = "none" | "selector" | "text";

interface FormState {
  name: string;
  url: string;
  targetEnv: MonitorTargetEnv;
  intervalMinutes: string;
  waitForKind: WaitForKind;
  waitForValue: string;
  waitTimeoutSeconds: string;
  clickSelector: string;
  assertionType: MonitorAssertionType;
  assertionValue: string;
}

const EMPTY_FORM: FormState = {
  name: "",
  url: "",
  targetEnv: "production",
  intervalMinutes: "5",
  waitForKind: "none",
  waitForValue: "",
  waitTimeoutSeconds: "15",
  clickSelector: "",
  assertionType: "textPresent",
  assertionValue: "",
};

function formToInput(form: FormState): CreateMonitorInput {
  const intervalMinutes = Number(form.intervalMinutes) || 5;
  const waitTimeoutMs = (Number(form.waitTimeoutSeconds) || 15) * 1_000;
  return {
    name: form.name,
    url: form.url,
    targetEnv: form.targetEnv,
    intervalMinutes,
    waitForSelector: form.waitForKind === "selector" ? form.waitForValue : null,
    waitForText: form.waitForKind === "text" ? form.waitForValue : null,
    waitTimeoutMs,
    clickSelector: form.clickSelector || null,
    assertion: { type: form.assertionType, value: form.assertionValue },
  };
}

function monitorToForm(monitor: SyntheticMonitor): FormState {
  return {
    name: monitor.name,
    url: monitor.url,
    targetEnv: monitor.targetEnv,
    intervalMinutes: String(monitor.intervalMinutes),
    waitForKind: monitor.waitForSelector ? "selector" : monitor.waitForText ? "text" : "none",
    waitForValue: monitor.waitForSelector ?? monitor.waitForText ?? "",
    waitTimeoutSeconds: String(Math.round(monitor.waitTimeoutMs / 1_000)),
    clickSelector: monitor.clickSelector ?? "",
    assertionType: monitor.assertion.type,
    assertionValue: monitor.assertion.value,
  };
}

function runStatusTone(status: MonitorRun["status"]): PillTone {
  if (status === "pass") return "success";
  if (status === "fail") return "danger";
  return "warning";
}

function errorText(error: unknown): string {
  return errorMessage(error);
}

function RunEvidenceThumbnail({ artifactId, alt }: { artifactId: string | null; alt: string }) {
  const [dataUrl, setDataUrl] = useState<string | null>(null);

  useEffect(() => {
    let current = true;
    setDataUrl(null);
    if (!artifactId) return;
    void readDurableArtifact(artifactId)
      .then((content) => {
        if (current) setDataUrl(artifactDataUrl("image/png", content.contentBase64));
      })
      .catch(() => {});
    return () => {
      current = false;
    };
  }, [artifactId]);

  if (!artifactId) return null;
  if (!dataUrl) {
    return <div className="flex h-40 w-full items-center justify-center rounded-md border border-dashed border-border text-faint"><Loader2 className="animate-spin" size={16} /></div>;
  }
  return <img src={dataUrl} alt={alt} className="max-h-64 w-full rounded-md border border-border object-contain" />;
}

export function SyntheticMonitoringPanel({ onClose }: SyntheticMonitoringPanelProps) {
  const { t } = useT();
  const monitors = useSyntheticMonitoringStore((s) => s.monitors);
  const runsByMonitor = useSyntheticMonitoringStore((s) => s.runsByMonitor);
  const runningMonitorIds = useSyntheticMonitoringStore((s) => s.runningMonitorIds);
  const selectedMonitorId = useSyntheticMonitoringStore((s) => s.selectedMonitorId);
  const storeError = useSyntheticMonitoringStore((s) => s.error);
  const selectMonitor = useSyntheticMonitoringStore((s) => s.selectMonitor);
  const addMonitor = useSyntheticMonitoringStore((s) => s.addMonitor);
  const updateMonitor = useSyntheticMonitoringStore((s) => s.updateMonitor);
  const deleteMonitor = useSyntheticMonitoringStore((s) => s.deleteMonitor);
  const toggleMonitor = useSyntheticMonitoringStore((s) => s.toggleMonitor);
  const runMonitorNow = useSyntheticMonitoringStore((s) => s.runMonitorNow);
  const clearError = useSyntheticMonitoringStore((s) => s.clearError);

  const [form, setForm] = useState<FormState>(EMPTY_FORM);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [formError, setFormError] = useState<string | null>(null);
  const [selectedRunId, setSelectedRunId] = useState<string | null>(null);

  const selectedMonitor = useMemo(
    () => monitors.find((monitor) => monitor.id === selectedMonitorId) ?? null,
    [monitors, selectedMonitorId],
  );
  const runs = selectedMonitor ? runsByMonitor[selectedMonitor.id] ?? [] : [];
  const selectedRun = useMemo(() => runs.find((run) => run.id === selectedRunId) ?? runs[0] ?? null, [runs, selectedRunId]);

  useEffect(() => {
    setSelectedRunId(null);
  }, [selectedMonitorId]);

  function resetForm() {
    setForm(EMPTY_FORM);
    setEditingId(null);
    setFormError(null);
  }

  function startEdit(monitor: SyntheticMonitor) {
    setForm(monitorToForm(monitor));
    setEditingId(monitor.id);
    setFormError(null);
  }

  function handleSubmit(event: FormEvent) {
    event.preventDefault();
    setFormError(null);
    try {
      const input = formToInput(form);
      if (editingId) {
        updateMonitor(editingId, input);
        if (useSyntheticMonitoringStore.getState().error) {
          setFormError(useSyntheticMonitoringStore.getState().error);
          clearError();
          return;
        }
      } else {
        addMonitor(input);
      }
      resetForm();
    } catch (error) {
      setFormError(errorText(error));
    }
  }

  return (
    <section className="flex h-full min-h-0 flex-col" aria-labelledby="synthetic-monitoring-title">
      <header className="flex shrink-0 items-start justify-between gap-3 border-b border-border px-5 py-4">
        <div>
          <h2 id="synthetic-monitoring-title" className="text-sm font-semibold text-foreground">
            {t("SyntheticMonitoring.title")}
          </h2>
          <p className="mt-1 max-w-2xl text-xs leading-5 text-muted">{t("SyntheticMonitoring.subtitle")}</p>
        </div>
        <IconButton size="sm" aria-label={t("SyntheticMonitoring.close")} title={t("SyntheticMonitoring.close")} onClick={onClose}>
          <X size={15} />
        </IconButton>
      </header>

      {storeError && (
        <div role="alert" className="mx-5 mt-3 rounded-md border border-danger/40 bg-danger/10 p-3 text-xs text-danger">
          {storeError}
        </div>
      )}

      <div className="grid min-h-0 flex-1 gap-4 overflow-hidden p-5 xl:grid-cols-[minmax(20rem,.9fr)_minmax(0,1.3fr)]">
        <div className="flex min-h-0 flex-col gap-4 overflow-y-auto">
          <form onSubmit={handleSubmit} className="shrink-0 space-y-2.5 rounded-lg border border-border bg-surface p-3">
            <h3 className="text-xs font-semibold text-foreground">
              {editingId ? t("SyntheticMonitoring.editMonitorHeading") : t("SyntheticMonitoring.addMonitorHeading")}
            </h3>

            <label className="block text-xs text-muted">
              {t("SyntheticMonitoring.nameLabel")}
              <input
                className="mt-1 w-full rounded-md border border-border bg-background px-2.5 py-1.5 text-sm text-foreground outline-none focus:border-accent focus:ring-1 focus:ring-accent"
                placeholder={t("SyntheticMonitoring.namePlaceholder")}
                value={form.name}
                onChange={(event) => setForm((prev) => ({ ...prev, name: event.target.value }))}
              />
            </label>

            <label className="block text-xs text-muted">
              {t("SyntheticMonitoring.urlLabel")}
              <input
                className="mt-1 w-full rounded-md border border-border bg-background px-2.5 py-1.5 font-mono text-xs text-foreground outline-none focus:border-accent focus:ring-1 focus:ring-accent"
                placeholder={t("SyntheticMonitoring.urlPlaceholder")}
                value={form.url}
                onChange={(event) => setForm((prev) => ({ ...prev, url: event.target.value }))}
              />
            </label>

            <div className="grid grid-cols-2 gap-2">
              <label className="block text-xs text-muted">
                {t("SyntheticMonitoring.targetEnvLabel")}
                <select
                  className="mt-1 w-full rounded-md border border-border bg-background px-2 py-1.5 text-xs text-foreground outline-none focus:border-accent"
                  value={form.targetEnv}
                  onChange={(event) => setForm((prev) => ({ ...prev, targetEnv: event.target.value as MonitorTargetEnv }))}
                >
                  <option value="local">{t("SyntheticMonitoring.targetEnvLocal")}</option>
                  <option value="staging">{t("SyntheticMonitoring.targetEnvStaging")}</option>
                  <option value="production">{t("SyntheticMonitoring.targetEnvProduction")}</option>
                </select>
              </label>
              <label className="block text-xs text-muted">
                {t("SyntheticMonitoring.intervalLabel")}
                <input
                  type="number"
                  min={1}
                  className="mt-1 w-full rounded-md border border-border bg-background px-2 py-1.5 text-xs text-foreground outline-none focus:border-accent"
                  value={form.intervalMinutes}
                  onChange={(event) => setForm((prev) => ({ ...prev, intervalMinutes: event.target.value }))}
                />
              </label>
            </div>

            <div className="grid grid-cols-[7rem_minmax(0,1fr)] gap-2">
              <label className="block text-xs text-muted">
                {t("SyntheticMonitoring.waitForLabel")}
                <select
                  className="mt-1 w-full rounded-md border border-border bg-background px-2 py-1.5 text-xs text-foreground outline-none focus:border-accent"
                  value={form.waitForKind}
                  onChange={(event) => setForm((prev) => ({ ...prev, waitForKind: event.target.value as WaitForKind }))}
                >
                  <option value="none">{t("SyntheticMonitoring.waitForNone")}</option>
                  <option value="selector">{t("SyntheticMonitoring.waitForSelectorOption")}</option>
                  <option value="text">{t("SyntheticMonitoring.waitForTextOption")}</option>
                </select>
              </label>
              <label className="block text-xs text-muted">
                &nbsp;
                <input
                  disabled={form.waitForKind === "none"}
                  className="mt-1 w-full rounded-md border border-border bg-background px-2.5 py-1.5 font-mono text-xs text-foreground outline-none focus:border-accent focus:ring-1 focus:ring-accent disabled:opacity-40"
                  placeholder={t("SyntheticMonitoring.waitForValuePlaceholder")}
                  value={form.waitForValue}
                  onChange={(event) => setForm((prev) => ({ ...prev, waitForValue: event.target.value }))}
                />
              </label>
            </div>

            <div className="grid grid-cols-2 gap-2">
              <label className="block text-xs text-muted">
                {t("SyntheticMonitoring.waitTimeoutLabel")}
                <input
                  type="number"
                  min={1}
                  className="mt-1 w-full rounded-md border border-border bg-background px-2 py-1.5 text-xs text-foreground outline-none focus:border-accent"
                  value={form.waitTimeoutSeconds}
                  onChange={(event) => setForm((prev) => ({ ...prev, waitTimeoutSeconds: event.target.value }))}
                />
              </label>
              <label className="block text-xs text-muted">
                {t("SyntheticMonitoring.clickSelectorLabel")}
                <input
                  className="mt-1 w-full rounded-md border border-border bg-background px-2.5 py-1.5 font-mono text-xs text-foreground outline-none focus:border-accent focus:ring-1 focus:ring-accent"
                  placeholder={t("SyntheticMonitoring.clickSelectorPlaceholder")}
                  value={form.clickSelector}
                  onChange={(event) => setForm((prev) => ({ ...prev, clickSelector: event.target.value }))}
                />
              </label>
            </div>

            <div className="grid grid-cols-[9rem_minmax(0,1fr)] gap-2">
              <label className="block text-xs text-muted">
                {t("SyntheticMonitoring.assertionLabel")}
                <select
                  className="mt-1 w-full rounded-md border border-border bg-background px-2 py-1.5 text-xs text-foreground outline-none focus:border-accent"
                  value={form.assertionType}
                  onChange={(event) => setForm((prev) => ({ ...prev, assertionType: event.target.value as MonitorAssertionType }))}
                >
                  <option value="selectorPresent">{t("SyntheticMonitoring.assertionSelectorPresent")}</option>
                  <option value="textPresent">{t("SyntheticMonitoring.assertionTextPresent")}</option>
                  <option value="urlPrefix">{t("SyntheticMonitoring.assertionUrlPrefix")}</option>
                </select>
              </label>
              <label className="block text-xs text-muted">
                &nbsp;
                <input
                  className="mt-1 w-full rounded-md border border-border bg-background px-2.5 py-1.5 font-mono text-xs text-foreground outline-none focus:border-accent focus:ring-1 focus:ring-accent"
                  placeholder={t("SyntheticMonitoring.assertionValuePlaceholder")}
                  value={form.assertionValue}
                  onChange={(event) => setForm((prev) => ({ ...prev, assertionValue: event.target.value }))}
                />
              </label>
            </div>

            {formError && <p className="text-xs text-danger">{formError}</p>}

            <div className="flex justify-end gap-2 pt-1">
              {editingId && (
                <Button type="button" size="sm" onClick={resetForm}>
                  {t("SyntheticMonitoring.cancelEditButton")}
                </Button>
              )}
              <Button type="submit" size="sm" variant="primary">
                {editingId ? <Pencil size={13} /> : <Plus size={13} />}
                {editingId ? t("SyntheticMonitoring.updateButton") : t("SyntheticMonitoring.saveButton")}
              </Button>
            </div>
          </form>

          <div className="min-h-0 flex-1 overflow-y-auto rounded-lg border border-border bg-surface p-3">
            <h3 className="text-xs font-semibold text-foreground">{t("SyntheticMonitoring.monitorsHeading")}</h3>
            <div className="mt-2 space-y-1.5">
              {monitors.length === 0 && (
                <p className="rounded-md border border-dashed border-border p-5 text-center text-xs text-faint">
                  {t("SyntheticMonitoring.emptyMonitors")}
                </p>
              )}
              {monitors.map((monitor) => {
                const latestRun = (runsByMonitor[monitor.id] ?? [])[0] ?? null;
                const running = Boolean(runningMonitorIds[monitor.id]);
                return (
                  <div
                    key={monitor.id}
                    className={`rounded-md border p-2.5 transition-colors ${
                      monitor.id === selectedMonitorId ? "border-accent bg-accent/10" : "border-border bg-background hover:border-border-strong"
                    }`}
                  >
                    <button type="button" className="w-full text-left" onClick={() => selectMonitor(monitor.id)}>
                      <div className="flex items-center justify-between gap-2">
                        <p className="truncate text-xs font-medium text-foreground">{monitor.name}</p>
                        {latestRun && <StatusPill tone={runStatusTone(latestRun.status)}>{t(`SyntheticMonitoring.status${statusSuffix(latestRun.status)}`)}</StatusPill>}
                      </div>
                      <p className="mt-0.5 truncate font-mono text-[11px] text-faint">{monitor.url}</p>
                      <p className="mt-1 text-[11px] text-muted">
                        {monitor.lastRunAtMs ? t("SyntheticMonitoring.lastRunLabel", { time: new Date(monitor.lastRunAtMs).toLocaleString() }) : t("SyntheticMonitoring.neverRunLabel")}
                        {" · "}
                        {t("SyntheticMonitoring.everyIntervalLabel", { minutes: monitor.intervalMinutes })}
                      </p>
                    </button>
                    <div className="mt-2 flex flex-wrap items-center gap-1.5">
                      <Button size="sm" disabled={running} onClick={() => void runMonitorNow(monitor.id)}>
                        {running ? <Loader2 className="animate-spin" size={12} /> : <Play size={12} />}
                        {running ? t("SyntheticMonitoring.runningButton") : t("SyntheticMonitoring.runNowButton")}
                      </Button>
                      <Button size="sm" onClick={() => toggleMonitor(monitor.id)}>
                        {monitor.enabled ? t("SyntheticMonitoring.enableToggleOn") : t("SyntheticMonitoring.enableToggleOff")}
                      </Button>
                      <IconButton size="sm" aria-label={t("SyntheticMonitoring.editButton")} title={t("SyntheticMonitoring.editButton")} onClick={() => startEdit(monitor)}>
                        <Pencil size={13} />
                      </IconButton>
                      <IconButton size="sm" aria-label={t("SyntheticMonitoring.deleteButton")} title={t("SyntheticMonitoring.deleteButton")} onClick={() => deleteMonitor(monitor.id)}>
                        <Trash2 size={13} />
                      </IconButton>
                    </div>
                  </div>
                );
              })}
            </div>
          </div>
        </div>

        <div className="min-h-0 overflow-y-auto rounded-lg border border-border bg-surface p-4">
          {!selectedMonitor ? (
            <p className="p-8 text-center text-xs text-faint">{t("SyntheticMonitoring.noSelectionState")}</p>
          ) : (
            <div className="grid min-h-0 gap-4 lg:grid-cols-[minmax(12rem,.8fr)_minmax(0,1.2fr)]">
              <div>
                <div className="flex items-center justify-between gap-2">
                  <h3 className="text-xs font-semibold text-foreground">{t("SyntheticMonitoring.runHistoryHeading")}</h3>
                  <IconButton size="sm" aria-label="Refresh" onClick={() => void runMonitorNow(selectedMonitor.id)}>
                    <RefreshCw size={13} />
                  </IconButton>
                </div>
                <div className="mt-2 space-y-1.5">
                  {runs.length === 0 && (
                    <p className="rounded-md border border-dashed border-border p-4 text-center text-xs text-faint">
                      {t("SyntheticMonitoring.emptyRunHistory")}
                    </p>
                  )}
                  {runs.map((run) => (
                    <button
                      key={run.id}
                      type="button"
                      onClick={() => setSelectedRunId(run.id)}
                      className={`w-full rounded-md border p-2 text-left text-[11px] transition-colors ${
                        (selectedRun?.id ?? runs[0]?.id) === run.id ? "border-accent bg-accent/10" : "border-border bg-background hover:border-border-strong"
                      }`}
                    >
                      <div className="flex items-center justify-between gap-2">
                        <StatusPill tone={runStatusTone(run.status)}>{t(`SyntheticMonitoring.status${statusSuffix(run.status)}`)}</StatusPill>
                        <span className="text-faint">{t("SyntheticMonitoring.latencyLabel", { ms: run.latencyMs })}</span>
                      </div>
                      <p className="mt-1 text-faint">{new Date(run.startedAtMs).toLocaleString()}</p>
                    </button>
                  ))}
                </div>
              </div>

              <div className="min-w-0 space-y-3">
                <div>
                  <h3 className="text-sm font-semibold text-foreground">{selectedMonitor.name}</h3>
                  <p className="mt-1 break-all font-mono text-[11px] text-faint">{selectedMonitor.url}</p>
                </div>

                {selectedRun ? (
                  <>
                    <div>
                      <h4 className="text-xs font-semibold text-foreground">{t("SyntheticMonitoring.evidenceHeading")}</h4>
                      <div className="mt-1.5">
                        {selectedRun.evidence.screenshotArtifactId ? (
                          <RunEvidenceThumbnail artifactId={selectedRun.evidence.screenshotArtifactId} alt={t("SyntheticMonitoring.evidenceScreenshotAlt")} />
                        ) : (
                          <p className="rounded-md border border-dashed border-border p-3 text-center text-[11px] text-faint">
                            {t("SyntheticMonitoring.evidenceNoneState")}
                          </p>
                        )}
                      </div>
                    </div>

                    {selectedRun.failureReason && (
                      <div className="rounded-md border border-danger/40 bg-danger/10 p-3 text-xs text-danger">
                        <p className="flex items-center gap-1.5 font-medium"><AlertTriangle size={13} /> {t("SyntheticMonitoring.failureReasonHeading")}</p>
                        <p className="mt-1 whitespace-pre-wrap break-words">{selectedRun.failureReason}</p>
                      </div>
                    )}

                    {selectedRun.status !== "pass" && (
                      <div className="rounded-md border border-accent/30 bg-accent/5 p-3 text-xs">
                        <p className="flex items-center gap-1.5 font-medium text-foreground"><Sparkles size={13} className="text-accent" /> {t("SyntheticMonitoring.diagnosisHeading")}</p>
                        <p className="mt-1 whitespace-pre-wrap break-words text-muted">
                          {selectedRun.diagnosis ?? t("SyntheticMonitoring.diagnosisNoneState")}
                        </p>
                      </div>
                    )}
                  </>
                ) : (
                  <p className="rounded-md border border-dashed border-border p-4 text-center text-xs text-faint">
                    {t("SyntheticMonitoring.emptyRunHistory")}
                  </p>
                )}
              </div>
            </div>
          )}
        </div>
      </div>
    </section>
  );
}

function statusSuffix(status: MonitorRun["status"]): string {
  switch (status) {
    case "pass": return "Pass";
    case "fail": return "Fail";
    case "error": return "Error";
  }
}

export default SyntheticMonitoringPanel;

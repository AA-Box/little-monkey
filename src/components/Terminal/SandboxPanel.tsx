import { useCallback, useMemo, useState } from "react";
import { AlertTriangle, Box, PlayCircle, RefreshCw, Trash2, X } from "lucide-react";

import { useT } from "../../lib/i18n";
import { useSandboxStore } from "../../store/sandboxStore";
import { Button, IconButton, StatusPill } from "../ui";
import type { PillTone } from "../ui";

interface SandboxPanelProps {
  initialCommand?: string;
  onClose: () => void;
}

function statusTone(passed: boolean, timedOut: boolean): PillTone {
  if (timedOut) return "warning";
  return passed ? "success" : "danger";
}

export function SandboxPanel({ initialCommand, onClose }: SandboxPanelProps) {
  const { t } = useT();
  const activeSummary = useSandboxStore((state) => state.activeSummary);
  const stdoutText = useSandboxStore((state) => state.stdoutText);
  const stderrText = useSandboxStore((state) => state.stderrText);
  const diff = useSandboxStore((state) => state.diff);
  const selectedFiles = useSandboxStore((state) => state.selectedFiles);
  const preview = useSandboxStore((state) => state.preview);
  const busy = useSandboxStore((state) => state.busy);
  const error = useSandboxStore((state) => state.error);
  const run = useSandboxStore((state) => state.run);
  const loadLogs = useSandboxStore((state) => state.loadLogs);
  const loadDiff = useSandboxStore((state) => state.loadDiff);
  const toggleFile = useSandboxStore((state) => state.toggleFile);
  const setSelectedFiles = useSandboxStore((state) => state.setSelectedFiles);
  const preparePromote = useSandboxStore((state) => state.preparePromote);
  const cancelPromotePreview = useSandboxStore((state) => state.cancelPromotePreview);
  const executePromote = useSandboxStore((state) => state.executePromote);
  const discard = useSandboxStore((state) => state.discard);
  const clearMessages = useSandboxStore((state) => state.clearMessages);

  const [command, setCommand] = useState(initialCommand ?? "");
  const [allowNetwork, setAllowNetwork] = useState(false);
  const [confirmation, setConfirmation] = useState("");
  const [discarding, setDiscarding] = useState(false);

  const runBusy = busy.run;
  const logsBusy = busy.logs;
  const diffBusy = busy.diff;
  const prepareBusy = busy.preparePromote;
  const executeBusy = busy.executePromote;

  const runCommand = useCallback(async () => {
    if (!command.trim() || runBusy) return;
    try {
      await run(command, { allowNetwork });
    } catch {
      // The store owns the visible error text.
    }
  }, [allowNetwork, command, run, runBusy]);

  const onLoadDiff = useCallback(() => {
    if (!activeSummary) return;
    void loadDiff(activeSummary.runId);
  }, [activeSummary, loadDiff]);

  const onPreparePromote = useCallback(async () => {
    if (!activeSummary || selectedFiles.length === 0) return;
    setConfirmation("");
    try {
      await preparePromote(activeSummary.runId, selectedFiles);
    } catch {
      // The store owns the visible error text.
    }
  }, [activeSummary, preparePromote, selectedFiles]);

  const onExecutePromote = useCallback(async () => {
    try {
      await executePromote(confirmation);
      setConfirmation("");
    } catch {
      // The store owns the visible error text.
    }
  }, [confirmation, executePromote]);

  const onDiscard = useCallback(async () => {
    if (!activeSummary) return;
    try {
      await discard(activeSummary.runId);
    } finally {
      setDiscarding(false);
    }
  }, [activeSummary, discard]);

  const allSelected = useMemo(
    () => diff.length > 0 && diff.every((entry) => selectedFiles.includes(entry.path)),
    [diff, selectedFiles],
  );

  return (
    <div
      className="absolute inset-0 z-30 flex items-center justify-center bg-black/50 p-3"
      role="dialog"
      aria-modal="true"
      aria-labelledby="sandbox-panel-title"
    >
      <div className="flex max-h-full w-full max-w-2xl flex-col overflow-hidden rounded-xl border border-border bg-background shadow-xl">
        <div className="flex items-start gap-3 border-b border-border p-4">
          <div className="flex h-9 w-9 shrink-0 items-center justify-center rounded-full bg-accent-soft text-accent">
            <Box size={18} />
          </div>
          <div className="min-w-0 flex-1">
            <h3 id="sandbox-panel-title" className="text-sm font-semibold text-foreground">
              {t("SandboxPanel.title")}
            </h3>
            <p className="mt-1 text-xs text-muted">{t("SandboxPanel.description")}</p>
          </div>
          <IconButton size="sm" variant="ghost" onClick={onClose} aria-label={t("SandboxPanel.closeButton")}>
            <X size={14} />
          </IconButton>
        </div>

        <div className="min-h-0 flex-1 overflow-y-auto p-4">
          {error && (
            <div className="mb-3 flex items-center justify-between gap-3 rounded-md border border-danger bg-danger-soft px-3 py-1.5 text-xs text-danger">
              <span className="min-w-0 break-words">{error}</span>
              <button type="button" onClick={clearMessages} className="shrink-0 underline">
                {t("SandboxPanel.dismiss")}
              </button>
            </div>
          )}

          <label className="block text-xs font-medium text-muted">
            {t("SandboxPanel.commandLabel")}
            <input
              value={command}
              onChange={(event) => setCommand(event.target.value)}
              placeholder={t("SandboxPanel.commandPlaceholder")}
              className="mt-1 w-full rounded-md border border-border bg-surface px-2 py-1.5 font-mono text-xs text-foreground outline-none focus:border-accent"
            />
          </label>
          <div className="mt-2 flex flex-wrap items-center gap-3">
            <label className="flex items-center gap-1.5 text-xs text-muted">
              <input
                type="checkbox"
                checked={allowNetwork}
                onChange={(event) => setAllowNetwork(event.target.checked)}
              />
              {t("SandboxPanel.allowNetwork")}
            </label>
            <Button size="sm" variant="primary" onClick={() => void runCommand()} disabled={!command.trim() || runBusy}>
              <PlayCircle size={13} /> {runBusy ? t("SandboxPanel.running") : t("SandboxPanel.runButton")}
            </Button>
          </div>

          {!activeSummary ? (
            <p className="mt-6 text-center text-xs text-faint">{t("SandboxPanel.noActiveRun")}</p>
          ) : (
            <div className="mt-4 space-y-4">
              <div className="flex flex-wrap items-center gap-2">
                <StatusPill tone={statusTone(activeSummary.passed, activeSummary.timedOut)}>
                  {activeSummary.timedOut
                    ? t("SandboxPanel.timedOut")
                    : activeSummary.passed
                      ? t("SandboxPanel.passed")
                      : t("SandboxPanel.failed")}
                </StatusPill>
                <span className="text-[11px] text-faint">{t(`SandboxPanel.isolation.${activeSummary.isolation}`)}</span>
                <span className="text-[11px] text-faint">
                  {t("SandboxPanel.exitCode")}: {activeSummary.exitCode ?? "—"}
                </span>
                <span className="text-[11px] text-faint">
                  {t("SandboxPanel.duration")}: {activeSummary.durationMs}ms
                </span>
                <span className="text-[11px] text-faint">
                  {t("SandboxPanel.filesCopied", { count: activeSummary.filesCopied })}
                </span>
              </div>

              <div>
                <Button size="sm" variant="secondary" onClick={() => void loadLogs(activeSummary)} disabled={logsBusy}>
                  <RefreshCw size={12} /> {t("SandboxPanel.loadLogs")}
                </Button>
                {(stdoutText !== null || stderrText !== null) && (
                  <div className="mt-2 grid gap-2 sm:grid-cols-2">
                    <div>
                      <p className="mb-1 text-[11px] font-semibold text-muted">{t("SandboxPanel.stdoutHeading")}</p>
                      <pre className="max-h-40 overflow-auto whitespace-pre-wrap break-words rounded-md bg-[#0d1117] p-2 font-mono text-[11px] leading-4 text-[#d1d5db]">
                        {stdoutText || t("SandboxPanel.emptyOutput")}
                      </pre>
                    </div>
                    <div>
                      <p className="mb-1 text-[11px] font-semibold text-muted">{t("SandboxPanel.stderrHeading")}</p>
                      <pre className="max-h-40 overflow-auto whitespace-pre-wrap break-words rounded-md bg-[#0d1117] p-2 font-mono text-[11px] leading-4 text-[#d1d5db]">
                        {stderrText || t("SandboxPanel.emptyOutput")}
                      </pre>
                    </div>
                  </div>
                )}
              </div>

              <div>
                <div className="flex items-center justify-between">
                  <h4 className="text-xs font-semibold text-foreground">{t("SandboxPanel.diffHeading")}</h4>
                  <Button size="sm" variant="secondary" onClick={onLoadDiff} disabled={diffBusy}>
                    <RefreshCw size={12} /> {t("SandboxPanel.loadDiff")}
                  </Button>
                </div>
                {diff.length === 0 ? (
                  <p className="mt-2 text-xs text-faint">{t("SandboxPanel.diffEmpty")}</p>
                ) : (
                  <>
                    <div className="mt-2 flex items-center gap-2 text-[11px] text-muted">
                      <button
                        type="button"
                        className="underline"
                        onClick={() => setSelectedFiles(allSelected ? [] : diff.map((entry) => entry.path))}
                      >
                        {allSelected ? t("SandboxPanel.selectNone") : t("SandboxPanel.selectAll")}
                      </button>
                    </div>
                    <ul className="mt-1 max-h-40 space-y-1 overflow-y-auto rounded-md border border-border p-2">
                      {diff.map((entry) => (
                        <li key={entry.path} className="flex items-center gap-2 text-xs">
                          <input
                            type="checkbox"
                            checked={selectedFiles.includes(entry.path)}
                            onChange={() => toggleFile(entry.path)}
                          />
                          <span className="min-w-0 flex-1 truncate font-mono text-foreground">{entry.path}</span>
                          <StatusPill tone={entry.status === "added" ? "success" : "warning"}>
                            {entry.status === "added" ? t("SandboxPanel.diffAdded") : t("SandboxPanel.diffModified")}
                          </StatusPill>
                        </li>
                      ))}
                    </ul>
                    <div className="mt-2">
                      <Button
                        size="sm"
                        variant="primary"
                        onClick={() => void onPreparePromote()}
                        disabled={selectedFiles.length === 0 || prepareBusy}
                      >
                        {t("SandboxPanel.preparePromote")}
                      </Button>
                    </div>
                  </>
                )}
              </div>

              {preview && (
                <div className="rounded-md border border-warning/40 bg-warning-soft p-3">
                  <h4 className="flex items-center gap-1.5 text-xs font-semibold text-warning">
                    <AlertTriangle size={13} /> {t("SandboxPanel.promoteTitle")}
                  </h4>
                  <p className="mt-1 text-[11px] text-warning">{t("SandboxPanel.promoteDescription")}</p>
                  <p className="mt-2 text-[11px] font-semibold text-foreground">{t("SandboxPanel.promoteFileList")}</p>
                  <ul className="mt-1 max-h-28 overflow-y-auto text-[11px] text-foreground">
                    {preview.files.map((file) => (
                      <li key={file.path} className="truncate font-mono">
                        {file.path} · {file.sizeBytes}B
                      </li>
                    ))}
                  </ul>
                  <label className="mt-3 block text-xs text-muted">
                    {t("SandboxPanel.confirmPrompt")}{" "}
                    <code className="select-all rounded bg-background px-1 py-0.5 text-foreground">
                      {preview.confirmationPhrase}
                    </code>
                    <input
                      autoFocus
                      autoComplete="off"
                      spellCheck={false}
                      value={confirmation}
                      onChange={(event) => setConfirmation(event.target.value)}
                      placeholder={t("SandboxPanel.confirmPlaceholder")}
                      className="mt-1 w-full rounded-md border border-border bg-background px-2 py-1.5 font-mono text-xs text-foreground outline-none focus:border-accent"
                    />
                  </label>
                  <div className="mt-3 flex justify-end gap-2">
                    <Button variant="ghost" size="sm" onClick={cancelPromotePreview}>
                      {t("SandboxPanel.cancelButton")}
                    </Button>
                    <Button
                      variant="danger"
                      size="sm"
                      disabled={
                        executeBusy ||
                        confirmation !== preview.confirmationPhrase ||
                        Date.now() > preview.expiresAtMs
                      }
                      onClick={() => void onExecutePromote()}
                    >
                      {t("SandboxPanel.confirmButton")}
                    </Button>
                  </div>
                </div>
              )}

              <div className="border-t border-border pt-3">
                {discarding ? (
                  <div className="flex items-center justify-between gap-2 rounded-md border border-danger/40 bg-danger-soft p-2 text-xs text-danger">
                    <span>{t("SandboxPanel.discardConfirm")}</span>
                    <div className="flex gap-2">
                      <Button size="sm" variant="ghost" onClick={() => setDiscarding(false)}>
                        {t("SandboxPanel.cancelButton")}
                      </Button>
                      <Button size="sm" variant="danger" onClick={() => void onDiscard()}>
                        {t("SandboxPanel.discardButton")}
                      </Button>
                    </div>
                  </div>
                ) : (
                  <Button size="sm" variant="ghost" onClick={() => setDiscarding(true)}>
                    <Trash2 size={12} /> {t("SandboxPanel.discardButton")}
                  </Button>
                )}
              </div>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}

export default SandboxPanel;

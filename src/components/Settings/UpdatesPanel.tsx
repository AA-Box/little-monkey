import { useEffect, useState } from "react";
import {
  AlertTriangle,
  CheckCircle2,
  CircleSlash,
  HelpCircle,
  Loader2,
  MinusCircle,
  RefreshCw,
  ShieldCheck,
  Trash2,
  Undo2,
} from "lucide-react";

import { Button } from "../ui";
import { useT } from "../../lib/i18n";
import { errorMessage } from "../../lib/errors";
import { downloadProgress } from "../../lib/appUpdater";
import {
  loadIntegrityReport,
  sortComponents,
  type IntegrityReport,
  type IntegrityStatus,
} from "../../lib/selfIntegrity";
import { useUpdateStore } from "../../store/updateStore";
import { invoke, isTauri } from "@tauri-apps/api/core";

interface InstallInfo {
  kind: "macBundle" | "windowsDir" | "linuxFile";
  root: string;
  /** False for a Linux install owned by a package manager: the in-app updater
   * replaces an AppImage, not a `.deb`/`.rpm`. */
  selfUpdatable: boolean;
}

/**
 * Updates & integrity (roadmap K22 / ROADMAP #8).
 *
 * Three things live here because they are the same question asked at three
 * moments: is this build the one it claims to be (the startup integrity
 * verdict), is there a newer one (the update check, including the failures the
 * background checker keeps silent), and can I get the last one back (rollback).
 */

const STATUS_STYLE: Record<IntegrityStatus, string> = {
  mismatch: "border-danger/40 bg-danger/10 text-danger",
  unverified: "border-warning/40 bg-warning/10 text-warning",
  verified: "border-success/30 bg-success/5 text-success",
  absent: "border-border bg-surface-2 text-muted",
  unsupported: "border-border bg-surface-2 text-faint",
};

const STATUS_LABEL_KEYS: Record<IntegrityStatus, string> = {
  mismatch: "UpdatesPanel.statusMismatch",
  unverified: "UpdatesPanel.statusUnverified",
  verified: "UpdatesPanel.statusVerified",
  absent: "UpdatesPanel.statusAbsent",
  unsupported: "UpdatesPanel.statusUnsupported",
};

const COMPONENT_LABEL_KEYS: Record<string, string> = {
  app: "UpdatesPanel.componentApp",
  llama: "UpdatesPanel.componentLlama",
  "llama-tts": "UpdatesPanel.componentLlamaTts",
  sd: "UpdatesPanel.componentSd",
};

function StatusIcon({ status }: { status: IntegrityStatus }) {
  if (status === "mismatch") return <AlertTriangle size={15} />;
  if (status === "unverified") return <HelpCircle size={15} />;
  if (status === "verified") return <CheckCircle2 size={15} />;
  if (status === "absent") return <MinusCircle size={15} />;
  return <CircleSlash size={15} />;
}

/** Bytes as a short human string. Snapshots are hundreds of megabytes, so the
 * unit matters more than the precision. */
export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value < 10 ? value.toFixed(1) : Math.round(value)} ${units[unit]}`;
}

export function UpdatesPanel() {
  const { t } = useT();
  const status = useUpdateStore((s) => s.status);
  const version = useUpdateStore((s) => s.version);
  const notes = useUpdateStore((s) => s.notes);
  const downloadedBytes = useUpdateStore((s) => s.downloadedBytes);
  const contentLength = useUpdateStore((s) => s.contentLength);
  const lastCheckedAt = useUpdateStore((s) => s.lastCheckedAt);
  const lastError = useUpdateStore((s) => s.lastError);
  const rollback = useUpdateStore((s) => s.rollback);
  const rollbackError = useUpdateStore((s) => s.rollbackError);
  const rollbackBusy = useUpdateStore((s) => s.rollbackBusy);
  const check = useUpdateStore((s) => s.check);
  const applyUpdate = useUpdateStore((s) => s.applyUpdate);
  const loadRollback = useUpdateStore((s) => s.loadRollback);
  const applyRollback = useUpdateStore((s) => s.applyRollback);
  const discardRollback = useUpdateStore((s) => s.discardRollback);

  const [report, setReport] = useState<IntegrityReport | null>(null);
  const [reportError, setReportError] = useState<string | null>(null);
  const [reportLoading, setReportLoading] = useState(true);
  const [confirmRollback, setConfirmRollback] = useState(false);
  const [install, setInstall] = useState<InstallInfo | null>(null);

  useEffect(() => {
    let live = true;
    void (async () => {
      try {
        const loaded = await loadIntegrityReport();
        if (live) setReport(loaded);
      } catch (error) {
        if (live) setReportError(errorMessage(error));
      } finally {
        if (live) setReportLoading(false);
      }
    })();
    void loadRollback();
    if (isTauri()) {
      void invoke<InstallInfo>("update_install_info")
        .then((info) => {
          if (live) setInstall(info);
        })
        .catch(() => {
          // A shape we cannot classify is not worth an error banner; the
          // update controls behave the same either way.
        });
    }
    return () => {
      live = false;
    };
  }, [loadRollback]);

  const checking = status === "checking" || status === "downloading";
  const progress = downloadProgress(downloadedBytes, contentLength);

  const statusText = () => {
    if (status === "checking") return t("UpdatesPanel.stateChecking");
    if (status === "downloading") {
      return progress === null
        ? t("UpdatesPanel.stateDownloading")
        : t("UpdatesPanel.stateDownloadingPercent", { percent: Math.round(progress * 100) });
    }
    if (status === "ready") return t("UpdatesPanel.stateReady", { version: version ?? "" });
    if (status === "applying") return t("UpdatesPanel.stateApplying");
    return lastCheckedAt === null
      ? t("UpdatesPanel.stateNeverChecked")
      : t("UpdatesPanel.stateUpToDate");
  };

  return (
    <section className="flex flex-col gap-4" aria-labelledby="updates-heading">
      <div className="flex items-start gap-3">
        <span className="rounded-lg border border-accent/30 bg-accent/10 p-2 text-accent">
          <ShieldCheck size={20} />
        </span>
        <div>
          <h3 id="updates-heading" className="text-sm font-semibold text-foreground">
            {t("UpdatesPanel.title")}
          </h3>
          <p className="mt-1 text-xs leading-5 text-muted">{t("UpdatesPanel.description")}</p>
        </div>
      </div>

      {report?.refused && (
        <div
          role="alert"
          className="rounded-lg border border-danger/40 bg-danger/10 p-3 text-xs leading-5 text-danger"
        >
          <span className="font-semibold">{t("UpdatesPanel.refusedTitle")}</span>
          <p className="mt-1">{t("UpdatesPanel.refusedDescription")}</p>
        </div>
      )}

      <div className="rounded-lg border border-border bg-surface p-3">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <div className="min-w-0">
            <p className="text-xs font-semibold text-foreground">{statusText()}</p>
            <p className="mt-1 text-[11px] text-faint">
              {lastCheckedAt === null
                ? t("UpdatesPanel.neverChecked")
                : t("UpdatesPanel.lastChecked", {
                    when: new Date(lastCheckedAt).toLocaleString(),
                  })}
            </p>
          </div>
          <div className="flex flex-wrap gap-2">
            <Button variant="secondary" disabled={checking} onClick={() => void check("manual")}>
              {checking ? (
                <Loader2 size={14} className="animate-spin" />
              ) : (
                <RefreshCw size={14} />
              )}
              {t("UpdatesPanel.checkNow")}
            </Button>
            {status === "ready" && (
              <Button variant="primary" onClick={() => void applyUpdate()}>
                {t("UpdatesPanel.applyUpdate")}
              </Button>
            )}
          </div>
        </div>
        {notes && status === "ready" && (
          <p className="mt-2 whitespace-pre-line text-[11px] leading-4 text-muted">{notes}</p>
        )}
        {install && !install.selfUpdatable && (
          <p className="mt-2 rounded border border-border bg-surface-2 p-2 text-[11px] leading-4 text-muted">
            {t("UpdatesPanel.packageManagedInstall")}
          </p>
        )}
        {lastError && (
          // The background checker is silent by design; this is the one place
          // a failing check is visible instead of invisible.
          <p className="mt-2 rounded border border-warning/40 bg-warning/10 p-2 text-[11px] leading-4 text-warning">
            {t("UpdatesPanel.lastCheckFailed", { error: lastError })}
          </p>
        )}
      </div>

      <div className="rounded-lg border border-border bg-surface p-3">
        <h4 className="text-xs font-semibold text-foreground">{t("UpdatesPanel.rollbackTitle")}</h4>
        {rollback ? (
          <>
            <p className="mt-1 text-[11px] leading-4 text-muted">
              {t("UpdatesPanel.rollbackAvailable", {
                version: rollback.version,
                size: formatBytes(rollback.sizeBytes),
                when: new Date(rollback.createdAtMs).toLocaleString(),
              })}
            </p>
            <div className="mt-2 flex flex-wrap gap-2">
              {confirmRollback ? (
                <>
                  <Button
                    variant="danger"
                    disabled={rollbackBusy}
                    onClick={() => void applyRollback()}
                  >
                    {rollbackBusy ? (
                      <Loader2 size={12} className="animate-spin" />
                    ) : (
                      <Undo2 size={12} />
                    )}
                    {t("UpdatesPanel.rollbackConfirm", { version: rollback.version })}
                  </Button>
                  <Button variant="ghost" onClick={() => setConfirmRollback(false)}>
                    {t("UpdatesPanel.cancel")}
                  </Button>
                </>
              ) : (
                <>
                  <Button
                    variant="secondary"
                    disabled={rollbackBusy}
                    onClick={() => setConfirmRollback(true)}
                  >
                    <Undo2 size={12} />
                    {t("UpdatesPanel.rollbackAction", { version: rollback.version })}
                  </Button>
                  <Button
                    variant="ghost"
                    disabled={rollbackBusy}
                    onClick={() => void discardRollback()}
                  >
                    <Trash2 size={12} />
                    {t("UpdatesPanel.rollbackDiscard")}
                  </Button>
                </>
              )}
            </div>
            <p className="mt-2 text-[11px] leading-4 text-faint">
              {t("UpdatesPanel.rollbackRestartNote")}
            </p>
          </>
        ) : (
          <p className="mt-1 text-[11px] leading-4 text-muted">{t("UpdatesPanel.rollbackNone")}</p>
        )}
        {rollbackError && (
          <p className="mt-2 rounded border border-warning/40 bg-warning/10 p-2 text-[11px] leading-4 text-warning">
            {t("UpdatesPanel.rollbackFailed", { error: rollbackError })}
          </p>
        )}
      </div>

      <div className="rounded-lg border border-border bg-surface p-3">
        <h4 className="text-xs font-semibold text-foreground">
          {t("UpdatesPanel.integrityTitle")}
        </h4>
        <p className="mt-1 text-[11px] leading-4 text-muted">
          {t("UpdatesPanel.integrityDescription")}
        </p>
        {reportLoading && (
          <p className="mt-2 flex items-center gap-2 text-[11px] text-faint">
            <Loader2 size={12} className="animate-spin" />
            {t("UpdatesPanel.integrityLoading")}
          </p>
        )}
        {reportError && (
          <p className="mt-2 rounded border border-warning/40 bg-warning/10 p-2 text-[11px] leading-4 text-warning">
            {reportError}
          </p>
        )}
        <ul className="mt-2 flex flex-col gap-2">
          {sortComponents(report?.components ?? []).map((component) => (
            <li
              key={`${component.kind}-${component.id}`}
              className="flex items-start gap-3 rounded border border-border bg-surface-2 p-2"
            >
              <span
                className={`mt-0.5 inline-flex shrink-0 items-center gap-1 rounded-full border px-2 py-1 text-[10px] font-semibold uppercase tracking-wide ${STATUS_STYLE[component.status]}`}
              >
                <StatusIcon status={component.status} />
                {t(STATUS_LABEL_KEYS[component.status])}
              </span>
              <div className="min-w-0 flex-1">
                <p className="text-xs font-semibold text-foreground">
                  {COMPONENT_LABEL_KEYS[component.id]
                    ? t(COMPONENT_LABEL_KEYS[component.id])
                    : component.id}
                </p>
                <p className="mt-0.5 text-[11px] leading-4 text-muted">{component.detail}</p>
                {component.path && (
                  <p className="mt-0.5 truncate text-[10px] text-faint" title={component.path}>
                    {component.path}
                  </p>
                )}
              </div>
            </li>
          ))}
        </ul>
      </div>
    </section>
  );
}

export default UpdatesPanel;

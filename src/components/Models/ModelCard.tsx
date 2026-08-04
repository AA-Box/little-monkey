import { AlertTriangle, Download, Play, Square, Trash2, X } from "lucide-react";
import type { ModelInfo } from "../../lib/modelRegistry";
import { formatBytes, formatSizeGb } from "../../lib/modelRegistry";
import { useT } from "../../lib/i18n";
import { Button, IconButton, StatusPill } from "../ui";
import type { PillTone } from "../ui";

/** Status of the managed `llama-server` process, mirrors Rust `LlamaState.status`. */
export type LlamaStatus = "stopped" | "starting" | "ready" | "error";

export interface DownloadProgress {
  downloaded: number;
  total: number;
}

export interface ModelCardProps {
  model: ModelInfo;
  /** Whether this is the currently-selected/loaded model in the store. */
  isActive: boolean;
  /** Status of the llama-server process. Only meaningful when `isActive` is true. */
  llamaStatus: LlamaStatus;
  /** Present while `model.file` is being downloaded. */
  downloadProgress?: DownloadProgress;
  onInstall: () => void;
  onCancelDownload: () => void;
  onDelete: () => void;
  onStart: () => void;
  onStop: () => void;
}

const ACTIVE_PILL: Record<LlamaStatus, { tone: PillTone; labelKey: string } | null> = {
  ready: { tone: "success", labelKey: "ModelCard.statusActive" },
  starting: { tone: "warning", labelKey: "ModelCard.statusStarting" },
  error: { tone: "danger", labelKey: "ModelCard.statusError" },
  stopped: { tone: "neutral", labelKey: "ModelCard.statusSelected" },
};

export function ModelCard({
  model,
  isActive,
  llamaStatus,
  downloadProgress,
  onInstall,
  onCancelDownload,
  onDelete,
  onStart,
  onStop,
}: ModelCardProps) {
  const { t } = useT();
  const isDownloading = !model.installed && downloadProgress !== undefined;
  const isStarting = isActive && llamaStatus === "starting";
  const isRunning = isActive && llamaStatus === "ready";
  const isErrored = isActive && llamaStatus === "error";
  const busy = isStarting || isRunning;

  const progressPct =
    isDownloading && downloadProgress && downloadProgress.total > 0
      ? Math.min(100, Math.round((downloadProgress.downloaded / downloadProgress.total) * 100))
      : 0;

  const activePill = isActive ? ACTIVE_PILL[llamaStatus] : null;

  return (
    <div
      className={`flex items-center justify-between gap-3 rounded-lg border border-border bg-background p-3 transition-colors hover:border-border-strong ${
        isActive ? "border-l-2 border-l-accent pl-2.5" : ""
      }`}
    >
      <div className="min-w-0">
        <div className="flex flex-wrap items-center gap-2">
          <h3 className="truncate text-sm font-medium text-foreground">{model.name}</h3>
          {model.tool_calling && (
            <span className="inline-flex items-center rounded-full bg-surface-2 px-2 py-0.5 text-[10px] font-medium uppercase tracking-wide text-faint">
              {t("ModelCard.toolCallingBadge")}
            </span>
          )}
          {activePill && <StatusPill tone={activePill.tone}>{t(activePill.labelKey)}</StatusPill>}
        </div>
        <p className="mt-0.5 truncate font-mono text-xs text-muted">
          {model.repo ? `${model.repo} · ` : ""}
          {formatSizeGb(model.size_gb)}
        </p>
        {isErrored && (
          <p className="mt-1 flex items-center gap-1 text-xs text-danger">
            <AlertTriangle size={12} className="shrink-0" />
            {t("ModelCard.startFailedMessage")}
          </p>
        )}
      </div>

      <div className="flex shrink-0 items-center gap-2">
        {isDownloading && downloadProgress ? (
          <div className="flex items-center gap-2">
            <div className="flex flex-col items-end gap-1">
              <div className="h-1.5 w-28 overflow-hidden rounded-full bg-surface-2">
                <div
                  className="h-full rounded-full bg-accent transition-[width] duration-300"
                  style={{ width: `${progressPct}%` }}
                />
              </div>
              <span className="text-xs text-muted">
                {t("ModelCard.downloadProgressLabel", {
                  downloaded: formatBytes(downloadProgress.downloaded),
                  total: formatBytes(downloadProgress.total),
                  pct: progressPct,
                })}
              </span>
            </div>
            <IconButton
              variant="ghost"
              size="sm"
              aria-label={t("ModelCard.cancelDownloadAriaLabel")}
              title={t("ModelCard.cancelDownloadAriaLabel")}
              onClick={onCancelDownload}
            >
              <X size={14} />
            </IconButton>
          </div>
        ) : !model.installed ? (
          <Button variant="secondary" size="sm" onClick={onInstall}>
            <Download size={14} />
            {t("ModelCard.pullButton")}
          </Button>
        ) : (
          <>
            {isStarting && (
              <Button variant="danger" size="sm" disabled>
                <Square size={14} />
                {t("ModelCard.statusStarting")}
              </Button>
            )}

            {isRunning && (
              <Button variant="danger" size="sm" onClick={onStop}>
                <Square size={14} />
                {t("ModelCard.stopButton")}
              </Button>
            )}

            {!busy && (
              <Button variant="primary" size="sm" onClick={onStart}>
                <Play size={14} />
                {t("ModelCard.startButton")}
              </Button>
            )}

            <IconButton
              variant="ghost"
              size="sm"
              aria-label={t("ModelCard.deleteModelAriaLabel")}
              title={busy ? t("ModelCard.stopBeforeDeleteTitle") : t("ModelCard.deleteWeightsTitle")}
              onClick={onDelete}
              disabled={busy}
            >
              <Trash2 size={14} />
            </IconButton>
          </>
        )}
      </div>
    </div>
  );
}

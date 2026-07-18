/**
 * Claude-Desktop-style working-tree diff panel: a right-docked pane listing
 * every changed file (staged + unstaged + untracked, via `git_changed_files`)
 * with a HEAD-vs-disk diff of the selected file below (`git_file_diff`).
 * Read-only — committing stays in the workspace panel's git bar.
 */
import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { FileDiff, RefreshCw, X } from "lucide-react";

import { useT } from "../../lib/i18n";
import { IconButton } from "../ui";
import { DiffViewer } from "./DiffViewer";

interface GitChangedFile {
  path: string;
  status: string;
}

interface GitFileDiff {
  original: string;
  current: string;
  binary: boolean;
  oversize: boolean;
}

interface DiffPanelProps {
  onClose: () => void;
}

const STATUS_CLASSES: Record<string, string> = {
  A: "text-success",
  M: "text-warning",
  D: "text-danger",
  R: "text-accent",
};

export function DiffPanel({ onClose }: DiffPanelProps) {
  const { t } = useT();
  const [files, setFiles] = useState<GitChangedFile[]>([]);
  const [loading, setLoading] = useState(true);
  const [selectedPath, setSelectedPath] = useState<string | null>(null);
  const [diff, setDiff] = useState<GitFileDiff | null>(null);
  const [diffLoading, setDiffLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      const changed = await invoke<GitChangedFile[]>("git_changed_files");
      setFiles(changed);
      setError(null);
      // Keep the current selection when it still exists; otherwise select
      // the first file so the panel opens straight onto a diff.
      setSelectedPath((current) =>
        current !== null && changed.some((file) => file.path === current)
          ? current
          : (changed[0]?.path ?? null),
      );
    } catch (err) {
      setError(String(err));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  useEffect(() => {
    if (selectedPath === null) {
      setDiff(null);
      return;
    }
    let stale = false;
    setDiffLoading(true);
    invoke<GitFileDiff>("git_file_diff", { path: selectedPath })
      .then((result) => {
        if (!stale) setDiff(result);
      })
      .catch((err) => {
        if (!stale) setError(String(err));
      })
      .finally(() => {
        if (!stale) setDiffLoading(false);
      });
    return () => {
      stale = true;
    };
  }, [selectedPath]);

  return (
    <aside
      className="flex w-[560px] shrink-0 flex-col overflow-hidden border-l border-border bg-surface"
      aria-label={t("DiffPanel.title")}
    >
      <div
        data-tauri-drag-region
        className="flex h-11 shrink-0 items-center justify-between border-b border-border px-3"
      >
        <span className="pointer-events-none flex items-center gap-2 text-[11px] font-semibold uppercase tracking-wider text-faint">
          <FileDiff size={14} /> {t("DiffPanel.title")}
        </span>
        <span className="flex items-center gap-1">
          <IconButton
            size="sm"
            onClick={() => void refresh()}
            disabled={loading}
            aria-label={t("DiffPanel.refresh")}
            title={t("DiffPanel.refresh")}
          >
            <RefreshCw size={14} className={loading ? "animate-spin" : undefined} />
          </IconButton>
          <IconButton size="sm" onClick={onClose} aria-label={t("DiffPanel.close")} title={t("DiffPanel.close")}>
            <X size={15} />
          </IconButton>
        </span>
      </div>

      {error !== null && (
        <div className="shrink-0 border-b border-border bg-danger/10 px-3 py-2 text-xs text-danger">
          {error}
        </div>
      )}

      {files.length === 0 ? (
        <div className="flex flex-1 items-center justify-center px-6 text-center text-sm text-muted">
          {loading ? t("DiffPanel.loading") : t("DiffPanel.empty")}
        </div>
      ) : (
        <>
          <div role="listbox" aria-label={t("DiffPanel.title")} className="max-h-56 shrink-0 overflow-y-auto border-b border-border py-1">
            {files.map((file) => (
              <div
                key={file.path}
                role="option"
                aria-selected={file.path === selectedPath}
                tabIndex={0}
                onClick={() => setSelectedPath(file.path)}
                onKeyDown={(event) => {
                  if (event.key === "Enter" || event.key === " ") setSelectedPath(file.path);
                }}
                className={`mx-1 flex cursor-pointer items-center gap-2 rounded-md px-2 py-1.5 text-xs ${
                  file.path === selectedPath
                    ? "bg-surface-2 text-foreground"
                    : "text-muted hover:bg-surface-2/60 hover:text-foreground"
                }`}
              >
                <span
                  className={`w-3 shrink-0 text-center font-mono font-semibold ${STATUS_CLASSES[file.status] ?? "text-faint"}`}
                >
                  {file.status}
                </span>
                <span dir="rtl" className="min-w-0 flex-1 truncate text-left">
                  {file.path}
                </span>
              </div>
            ))}
          </div>
          <div className="min-h-0 flex-1 overflow-auto p-3">
            {diffLoading ? (
              <div className="flex h-full items-center justify-center text-sm text-faint">
                {t("DiffPanel.loading")}
              </div>
            ) : diff === null ? (
              <div className="flex h-full items-center justify-center text-sm text-muted">
                {t("DiffPanel.selectHint")}
              </div>
            ) : diff.binary ? (
              <div className="flex h-full items-center justify-center text-sm text-muted">
                {t("DiffPanel.binaryFile")}
              </div>
            ) : diff.oversize ? (
              <div className="flex h-full items-center justify-center text-sm text-muted">
                {t("DiffPanel.oversizeFile")}
              </div>
            ) : (
              <DiffViewer
                fileName={selectedPath ?? undefined}
                oldValue={diff.original}
                newValue={diff.current}
                oldTitle={t("DiffPanel.oldTitle")}
                newTitle={t("DiffPanel.newTitle")}
              />
            )}
          </div>
        </>
      )}
    </aside>
  );
}

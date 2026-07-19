import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { ChevronRight, FileText, Folder, FolderOpen, ListTodo, RefreshCw } from "lucide-react";
import { IconButton } from "../ui";
import { primaryRoot, useWorkspaceStore } from "../../store/workspaceStore";
import { useSessionStore } from "../../store/sessionStore";
import { buildSelectedFilesSideTaskSeed, useSideTaskStore } from "../../store/sideTaskStore";
import { useT } from "../../lib/i18n";

/** Shape returned per-entry by the Rust `tool_list_dir` command. */
interface DirEntry {
  name: string;
  is_dir: boolean;
  size: number;
}

export interface FileTreeProps {
  className?: string;
  /** Called whenever the user successfully opens a file preview. */
  onSelectFile?: (path: string, content: string) => void;
}

const INDENT_PX = 12;
const BASE_INDENT_PX = 6;
const ICON_SIZE = 16;
const MAX_PREVIEW_CHARS = 200_000;

function formatError(err: unknown): string {
  if (typeof err === "string") return err;
  if (err instanceof Error) return err.message;
  try {
    return JSON.stringify(err);
  } catch {
    return String(err);
  }
}

function formatSize(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return "";
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let value = bytes / 1024;
  let unitIndex = 0;
  while (value >= 1024 && unitIndex < units.length - 1) {
    value /= 1024;
    unitIndex += 1;
  }
  return `${value >= 10 ? Math.round(value) : value.toFixed(1)} ${units[unitIndex]}`;
}

function sortEntries(entries: DirEntry[]): DirEntry[] {
  return [...entries].sort((a, b) => {
    if (a.is_dir !== b.is_dir) return a.is_dir ? -1 : 1;
    return a.name.localeCompare(b.name, undefined, { sensitivity: "base" });
  });
}

function joinPath(parent: string, name: string): string {
  return parent === "." ? name : `${parent}/${name}`;
}

function TreeNode({
  entry,
  path,
  depth,
  selectedPath,
  onSelectFile,
}: {
  entry: DirEntry;
  path: string;
  depth: number;
  selectedPath: string | null;
  onSelectFile: (path: string) => void;
}) {
  const [expanded, setExpanded] = useState(false);
  const [children, setChildren] = useState<DirEntry[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const { t } = useT();

  async function loadChildren() {
    setLoading(true);
    setError(null);
    try {
      const result = await invoke<DirEntry[]>("tool_list_dir", { path });
      setChildren(sortEntries(result));
    } catch (err) {
      setError(formatError(err));
    } finally {
      setLoading(false);
    }
  }

  function handleActivate() {
    if (entry.is_dir) {
      if (!expanded && children === null && !loading) {
        void loadChildren();
      }
      setExpanded((prev) => !prev);
    } else {
      onSelectFile(path);
    }
  }

  const isSelected = !entry.is_dir && selectedPath === path;
  const indent = depth * INDENT_PX + BASE_INDENT_PX;
  const childIndent = (depth + 1) * INDENT_PX + BASE_INDENT_PX;

  return (
    <div>
      <div
        role="treeitem"
        aria-selected={isSelected}
        aria-expanded={entry.is_dir ? expanded : undefined}
        tabIndex={0}
        onClick={handleActivate}
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            handleActivate();
          }
        }}
        style={{ paddingLeft: `${indent}px` }}
        className={`flex cursor-pointer items-center gap-1.5 rounded py-1 pr-2 text-sm outline-none transition-colors hover:bg-surface-2 focus-visible:ring-1 focus-visible:ring-accent ${
          isSelected ? "bg-accent-soft text-accent" : "text-foreground"
        }`}
      >
        {entry.is_dir ? (
          <ChevronRight
            size={ICON_SIZE}
            className={`shrink-0 text-faint transition-transform duration-150 ${expanded ? "rotate-90" : ""}`}
          />
        ) : (
          <span className="shrink-0" style={{ width: ICON_SIZE, height: ICON_SIZE }} />
        )}
        {entry.is_dir ? (
          expanded ? (
            <FolderOpen size={ICON_SIZE} className="shrink-0 text-faint" />
          ) : (
            <Folder size={ICON_SIZE} className="shrink-0 text-faint" />
          )
        ) : (
          <FileText size={ICON_SIZE} className="shrink-0 text-faint" />
        )}
        <span className={`truncate ${entry.is_dir ? "" : "font-mono text-xs"}`}>{entry.name}</span>
        {!entry.is_dir && entry.size > 0 && (
          <span className="ml-auto shrink-0 pl-2 font-mono text-[10px] text-faint">
            {formatSize(entry.size)}
          </span>
        )}
      </div>
      {entry.is_dir && expanded && (
        <div role="group">
          {loading && (
            <div style={{ paddingLeft: `${childIndent}px` }} className="py-1 text-xs text-faint">
              {t("FileTree.loadingChildren")}
            </div>
          )}
          {error && (
            <div style={{ paddingLeft: `${childIndent}px` }} className="py-1 pr-2 text-xs text-danger">
              {error}
            </div>
          )}
          {children && children.length === 0 && (
            <div style={{ paddingLeft: `${childIndent}px` }} className="py-1 text-xs text-faint">
              {t("FileTree.emptyFolder")}
            </div>
          )}
          {children &&
            children.map((child) => (
              <TreeNode
                key={child.name}
                entry={child}
                path={joinPath(path, child.name)}
                depth={depth + 1}
                selectedPath={selectedPath}
                onSelectFile={onSelectFile}
              />
            ))}
        </div>
      )}
    </div>
  );
}

export function FileTree({ className = "", onSelectFile }: FileTreeProps) {
  const primary = useWorkspaceStore((s) => primaryRoot(s.roots));
  const activeChatSessionId = useSessionStore((state) => state.activeSessionId);
  const [rootLabel, setRootLabel] = useState<string | null>(null);
  const [rootEntries, setRootEntries] = useState<DirEntry[] | null>(null);
  const [rootError, setRootError] = useState<string | null>(null);
  const [rootLoading, setRootLoading] = useState(false);

  const [selectedPath, setSelectedPath] = useState<string | null>(null);
  const [previewContent, setPreviewContent] = useState<string | null>(null);
  const [previewError, setPreviewError] = useState<string | null>(null);
  const [previewLoading, setPreviewLoading] = useState(false);
  const [previewTruncated, setPreviewTruncated] = useState(false);
  const { t } = useT();

  async function loadRoot() {
    setRootLoading(true);
    setRootError(null);
    try {
      if (!primary) {
        setRootLabel(null);
        setRootEntries(null);
        return;
      }
      setRootLabel(primary.label);
      const entries = await invoke<DirEntry[]>("tool_list_dir", { path: "." });
      setRootEntries(sortEntries(entries));
    } catch (err) {
      setRootError(formatError(err));
      setRootEntries(null);
    } finally {
      setRootLoading(false);
    }
  }

  useEffect(() => {
    void loadRoot();
    // Runs once per mount; the parent remounts this component (via a `key`
    // tied to the workspace store's `rootsVersion`) whenever the primary
    // root actually changes, which is what drives a refresh.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  async function handleSelectFile(path: string) {
    setSelectedPath(path);
    setPreviewLoading(true);
    setPreviewError(null);
    try {
      const content = await invoke<string>("tool_read_file", { path });
      const truncated = content.length > MAX_PREVIEW_CHARS;
      setPreviewContent(truncated ? content.slice(0, MAX_PREVIEW_CHARS) : content);
      setPreviewTruncated(truncated);
      onSelectFile?.(path, content);
    } catch (err) {
      setPreviewContent(null);
      setPreviewTruncated(false);
      setPreviewError(formatError(err));
    } finally {
      setPreviewLoading(false);
    }
  }

  function startSideTaskFromSelection(): void {
    if (!selectedPath || previewContent === null || previewLoading || previewError) return;
    useSideTaskStore.getState().openComposer(buildSelectedFilesSideTaskSeed({
      sessionId: activeChatSessionId,
      files: [{ path: selectedPath, content: previewContent }],
    }));
  }

  return (
    <div className={`flex h-full min-h-0 flex-col ${className}`}>
      <div className="flex shrink-0 items-center justify-between border-b border-border px-3 py-2">
        <span className="truncate text-xs font-semibold uppercase tracking-wide text-faint">
          {rootLabel ?? t("FileTree.filesHeaderFallback")}
        </span>
        <IconButton
          size="sm"
          aria-label={t("FileTree.refreshFileTree")}
          onClick={() => void loadRoot()}
          disabled={rootLoading}
        >
          <RefreshCw size={14} className={rootLoading ? "animate-spin" : ""} />
        </IconButton>
      </div>

      <div role="tree" className="min-h-0 flex-1 overflow-y-auto px-1 py-2">
        {rootLoading && !rootEntries && !rootError && (
          <p className="px-2 py-1 text-xs text-faint">{t("FileTree.loadingWorkspace")}</p>
        )}
        {!rootLoading && !rootLabel && !rootError && (
          <p className="px-2 py-1 text-xs text-faint">{t("FileTree.noWorkspaceOpen")}</p>
        )}
        {rootError && <p className="px-2 py-1 text-xs text-danger">{rootError}</p>}
        {rootEntries && rootEntries.length === 0 && (
          <p className="px-2 py-1 text-xs text-faint">{t("FileTree.workspaceEmpty")}</p>
        )}
        {rootEntries &&
          rootEntries.map((entry) => (
            <TreeNode
              key={entry.name}
              entry={entry}
              path={entry.name}
              depth={0}
              selectedPath={selectedPath}
              onSelectFile={(p) => void handleSelectFile(p)}
            />
          ))}
      </div>

      {selectedPath && (
        <div className="max-h-72 shrink-0 overflow-auto border-t border-border bg-surface-2">
          <div className="sticky top-0 flex items-center justify-between border-b border-border bg-surface-2 px-3 py-1.5">
            <span className="truncate font-mono text-[11px] text-muted">{selectedPath}</span>
            <div className="ml-2 flex shrink-0 items-center gap-1">
              <IconButton
                size="sm"
                variant="ghost"
                onClick={startSideTaskFromSelection}
                disabled={previewContent === null || previewLoading || Boolean(previewError)}
                aria-label="Start side task from selected file"
                title="Start side task from selected file"
              >
                <ListTodo size={13} />
              </IconButton>
              <button
                type="button"
                onClick={() => {
                  setSelectedPath(null);
                  setPreviewContent(null);
                  setPreviewError(null);
                }}
                className="shrink-0 cursor-pointer rounded px-1.5 py-0.5 text-xs text-faint transition-colors hover:bg-surface hover:text-foreground"
              >
                {t("FileTree.closeButton")}
              </button>
            </div>
          </div>
          <div className="p-3">
            {previewLoading && <p className="text-xs text-faint">{t("FileTree.loadingPreview")}</p>}
            {previewError && <p className="text-xs text-danger">{previewError}</p>}
            {previewContent !== null && !previewLoading && !previewError && (
              <>
                <pre className="whitespace-pre-wrap break-words font-mono text-[11px] leading-relaxed text-muted">
                  {previewContent}
                </pre>
                {previewTruncated && (
                  <p className="mt-2 text-[11px] italic text-faint">
                    {t("FileTree.previewTruncated", { maxSize: formatSize(MAX_PREVIEW_CHARS) })}
                  </p>
                )}
              </>
            )}
          </div>
        </div>
      )}
    </div>
  );
}

export default FileTree;

import { useCallback, useEffect, useMemo, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { ChevronDown, ChevronRight, Download, FileText, FolderOpen, Play, Plus, Search, Square, Trash2, X } from "lucide-react";
import { Button, IconButton, StatusPill } from "../ui";
import type { PillTone } from "../ui";
import { useT } from "../../lib/i18n";
import { useModelStore } from "../../store/modelStore";
import {
  CURATED_EMBEDDING_SPECS,
  useStackStore,
  type EmbeddingBackend,
  type KnowledgeStack,
  type StackQueryResult,
} from "../../store/stackStore";

const EMBED_STATUS_TONE: Record<string, PillTone> = {
  ready: "success",
  starting: "warning",
  error: "danger",
  stopped: "neutral",
};

/** Format a Unix-ms timestamp as a short relative-ish absolute string. */
function formatIndexedAt(ms: number | null): string | null {
  if (ms == null) return null;
  return new Date(ms).toLocaleString();
}

/**
 * "Knowledge" settings tab (RAG slice 1): create/rename/delete stacks,
 * manage each stack's folder/file sources, download + run the local
 * embedding model, reindex with live progress, and a test-search box —
 * verifiable end-to-end before any agent wiring (`search_docs`, doc-chat
 * mode) exists.
 */
export function KnowledgePanel() {
  const { t } = useT();
  const stacks = useStackStore((s) => s.stacks);
  const indexProgress = useStackStore((s) => s.indexProgress);
  const reindexError = useStackStore((s) => s.reindexError);
  const refresh = useStackStore((s) => s.refresh);
  const createStack = useStackStore((s) => s.create);
  const removeStack = useStackStore((s) => s.remove);
  const renameStack = useStackStore((s) => s.rename);
  const addSource = useStackStore((s) => s.addSource);
  const removeSource = useStackStore((s) => s.removeSource);
  const reindex = useStackStore((s) => s.reindex);
  const cancelIndex = useStackStore((s) => s.cancelIndex);
  const queryStack = useStackStore((s) => s.query);

  const embedStatus = useStackStore((s) => s.embedStatus);
  const embedModelPath = useStackStore((s) => s.embedModelPath);
  const embedError = useStackStore((s) => s.embedError);
  const refreshEmbedStatus = useStackStore((s) => s.refreshEmbedStatus);
  const startEmbedServer = useStackStore((s) => s.startEmbedServer);
  const stopEmbedServer = useStackStore((s) => s.stopEmbedServer);

  const curatedModels = useModelStore((s) => s.curated);
  const installedModels = useModelStore((s) => s.installed);
  const downloadProgress = useModelStore((s) => s.downloadProgress);
  const refreshModels = useModelStore((s) => s.refresh);
  const downloadModel = useModelStore((s) => s.download);

  useEffect(() => {
    void refresh();
    void refreshModels();
    void refreshEmbedStatus();
  }, [refresh, refreshModels, refreshEmbedStatus]);

  const embeddingModels = useMemo(
    () => (curatedModels.length > 0 ? curatedModels : []).filter((m) => m.kind === "embedding"),
    [curatedModels],
  );
  const installedEmbeddingModels = useMemo(
    () => installedModels.filter((m) => m.kind === "embedding"),
    [installedModels],
  );

  const [expandedId, setExpandedId] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [newName, setNewName] = useState("");
  const [newBackend, setNewBackend] = useState<EmbeddingBackend>("llama");
  const [newModelId, setNewModelId] = useState<string>("");
  const [newOllamaTag, setNewOllamaTag] = useState("");
  const [createError, setCreateError] = useState<string | null>(null);
  const [selectedEmbedModelPath, setSelectedEmbedModelPath] = useState<string>("");

  const handleCreate = useCallback(async () => {
    const name = newName.trim();
    if (!name) return;
    setCreateError(null);
    try {
      if (newBackend === "llama") {
        const spec = CURATED_EMBEDDING_SPECS[newModelId];
        if (!newModelId || !spec) {
          setCreateError(t("KnowledgePanel.selectModelError"));
          return;
        }
        const stack = await createStack(name, {
          backend: "llama",
          model_id_or_tag: newModelId,
          dim: spec.dim,
          query_prefix: spec.queryPrefix,
          doc_prefix: spec.docPrefix,
        });
        setExpandedId(stack.id);
      } else {
        const tag = newOllamaTag.trim();
        if (!tag) {
          setCreateError(t("KnowledgePanel.enterOllamaTagError"));
          return;
        }
        const known = CURATED_EMBEDDING_SPECS[tag];
        const stack = await createStack(name, {
          backend: "ollama",
          model_id_or_tag: tag,
          dim: known?.dim ?? 1024,
          query_prefix: known?.queryPrefix ?? "",
          doc_prefix: known?.docPrefix ?? "",
        });
        setExpandedId(stack.id);
      }
      setNewName("");
      setNewOllamaTag("");
      setCreating(false);
    } catch (err) {
      setCreateError(err instanceof Error ? err.message : String(err));
    }
  }, [newName, newBackend, newModelId, newOllamaTag, createStack, t]);

  const handleDelete = useCallback(
    async (stack: KnowledgeStack) => {
      if (!window.confirm(t("KnowledgePanel.confirmDelete", { name: stack.name }))) return;
      await removeStack(stack.id);
      if (expandedId === stack.id) setExpandedId(null);
    },
    [removeStack, expandedId, t],
  );

  const handleRename = useCallback(
    async (stack: KnowledgeStack) => {
      const name = window.prompt(t("KnowledgePanel.renamePrompt"), stack.name);
      if (!name || !name.trim() || name.trim() === stack.name) return;
      await renameStack(stack.id, name.trim());
    },
    [renameStack, t],
  );

  const handleAddFolder = useCallback(
    async (stackId: string) => {
      const selected = await open({ directory: true, multiple: false });
      if (!selected || Array.isArray(selected)) return;
      await addSource(stackId, selected, "folder");
    },
    [addSource],
  );

  const handleAddFile = useCallback(
    async (stackId: string) => {
      const selected = await open({ multiple: false });
      if (!selected || Array.isArray(selected)) return;
      await addSource(stackId, selected, "file");
    },
    [addSource],
  );

  return (
    <div className="flex flex-col gap-3 p-2">
      <section className="rounded-lg border border-border bg-background p-3">
        <div className="flex items-center justify-between gap-2">
          <div>
            <h3 className="text-sm font-medium text-foreground">{t("KnowledgePanel.embedServerHeading")}</h3>
            <p className="mt-0.5 text-xs text-faint">{t("KnowledgePanel.embedServerDescription")}</p>
          </div>
          <StatusPill tone={EMBED_STATUS_TONE[embedStatus] ?? "neutral"}>
            {t(`KnowledgePanel.embedStatus_${embedStatus}`)}
          </StatusPill>
        </div>

        <div className="mt-2.5 flex flex-wrap items-center gap-2">
          <select
            value={selectedEmbedModelPath}
            onChange={(event) => setSelectedEmbedModelPath(event.target.value)}
            className="h-8 min-w-0 flex-1 rounded-md border border-border bg-surface px-2 text-sm text-foreground focus:outline-none focus:ring-2 focus:ring-accent"
          >
            <option value="">{t("KnowledgePanel.selectDownloadedModelPlaceholder")}</option>
            {installedEmbeddingModels.map((model) => (
              <option key={model.id} value={model.path ?? ""}>
                {model.name}
              </option>
            ))}
          </select>
          {embedStatus === "ready" || embedStatus === "starting" ? (
            <Button
              variant="danger"
              size="sm"
              onClick={() => void stopEmbedServer()}
              disabled={embedStatus === "starting"}
            >
              <Square size={14} />
              {t("KnowledgePanel.embedServerStopButton")}
            </Button>
          ) : (
            <Button
              variant="primary"
              size="sm"
              onClick={() => void startEmbedServer(selectedEmbedModelPath)}
              disabled={!selectedEmbedModelPath}
            >
              <Play size={14} />
              {t("KnowledgePanel.embedServerStartButton")}
            </Button>
          )}
        </div>
        {embedModelPath && embedStatus === "ready" && (
          <p className="mt-1.5 truncate font-mono text-xs text-muted">{embedModelPath}</p>
        )}
        {embedError && <p className="mt-1.5 text-xs text-danger">{embedError}</p>}

        {embeddingModels.length > 0 && (
          <div className="mt-3 border-t border-border pt-2.5">
            <p className="mb-1.5 text-xs text-muted">{t("KnowledgePanel.embeddingModelsHeading")}</p>
            <div className="flex flex-col gap-1.5">
              {embeddingModels.map((model) => {
                const installed = installedEmbeddingModels.some((m) => m.id === model.id);
                const progress = downloadProgress[model.file];
                return (
                  <div
                    key={model.id}
                    className="flex items-center justify-between gap-2 rounded-md border border-border px-2.5 py-1.5"
                  >
                    <span className="truncate text-xs text-foreground">{model.name}</span>
                    {installed ? (
                      <StatusPill tone="success">{t("KnowledgePanel.installedLabel")}</StatusPill>
                    ) : progress ? (
                      <span className="text-xs text-muted">
                        {progress.total > 0 ? Math.round((progress.downloaded / progress.total) * 100) : 0}%
                      </span>
                    ) : (
                      <Button variant="secondary" size="sm" onClick={() => void downloadModel(model)}>
                        <Download size={12} />
                        {t("KnowledgePanel.downloadButton")}
                      </Button>
                    )}
                  </div>
                );
              })}
            </div>
          </div>
        )}
      </section>

      <section className="flex flex-col gap-2">
        <div className="flex items-center justify-between">
          <h3 className="text-sm font-medium text-foreground">{t("KnowledgePanel.stacksHeading")}</h3>
          {!creating && (
            <Button variant="secondary" size="sm" onClick={() => setCreating(true)}>
              <Plus size={14} />
              {t("KnowledgePanel.createButton")}
            </Button>
          )}
        </div>

        {creating && (
          <div className="flex flex-col gap-2 rounded-lg border border-border bg-background p-3">
            <input
              type="text"
              autoFocus
              value={newName}
              onChange={(event) => setNewName(event.target.value)}
              placeholder={t("KnowledgePanel.namePlaceholder")}
              className="h-8 rounded-md border border-border bg-surface px-2.5 text-sm text-foreground placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
            />
            <div className="flex items-center gap-2">
              <select
                value={newBackend}
                onChange={(event) => setNewBackend(event.target.value as EmbeddingBackend)}
                className="h-8 rounded-md border border-border bg-surface px-2 text-sm text-foreground focus:outline-none focus:ring-2 focus:ring-accent"
              >
                <option value="llama">{t("KnowledgePanel.backendLlama")}</option>
                <option value="ollama">{t("KnowledgePanel.backendOllama")}</option>
              </select>
              {newBackend === "llama" ? (
                <select
                  value={newModelId}
                  onChange={(event) => setNewModelId(event.target.value)}
                  className="h-8 min-w-0 flex-1 rounded-md border border-border bg-surface px-2 text-sm text-foreground focus:outline-none focus:ring-2 focus:ring-accent"
                >
                  <option value="">{t("KnowledgePanel.modelLabel")}</option>
                  {embeddingModels.map((model) => (
                    <option key={model.id} value={model.id}>
                      {model.name}
                    </option>
                  ))}
                </select>
              ) : (
                <input
                  type="text"
                  value={newOllamaTag}
                  onChange={(event) => setNewOllamaTag(event.target.value)}
                  placeholder={t("KnowledgePanel.ollamaTagPlaceholder")}
                  className="h-8 min-w-0 flex-1 rounded-md border border-border bg-surface px-2.5 text-sm text-foreground placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
                />
              )}
            </div>
            {createError && <p className="text-xs text-danger">{createError}</p>}
            <div className="flex justify-end gap-2">
              <Button
                variant="ghost"
                size="sm"
                onClick={() => {
                  setCreating(false);
                  setCreateError(null);
                }}
              >
                {t("KnowledgePanel.cancelButton")}
              </Button>
              <Button variant="primary" size="sm" onClick={() => void handleCreate()} disabled={!newName.trim()}>
                {t("KnowledgePanel.createSubmit")}
              </Button>
            </div>
          </div>
        )}

        {stacks.length === 0 && !creating ? (
          <p className="p-3 text-center text-sm text-faint">{t("KnowledgePanel.emptyState")}</p>
        ) : (
          stacks.map((stack) => (
            <StackRow
              key={stack.id}
              stack={stack}
              expanded={expandedId === stack.id}
              onToggle={() => setExpandedId(expandedId === stack.id ? null : stack.id)}
              onDelete={() => void handleDelete(stack)}
              onRename={() => void handleRename(stack)}
              onAddFolder={() => void handleAddFolder(stack.id)}
              onAddFile={() => void handleAddFile(stack.id)}
              onRemoveSource={(path) => void removeSource(stack.id, path)}
              onReindex={() => void reindex(stack.id)}
              onCancelIndex={() => void cancelIndex(stack.id)}
              progress={indexProgress[stack.id]}
              error={reindexError[stack.id]}
              onQuery={queryStack}
            />
          ))
        )}
      </section>
    </div>
  );
}

interface StackRowProps {
  stack: KnowledgeStack;
  expanded: boolean;
  onToggle: () => void;
  onDelete: () => void;
  onRename: () => void;
  onAddFolder: () => void;
  onAddFile: () => void;
  onRemoveSource: (path: string) => void;
  onReindex: () => void;
  onCancelIndex: () => void;
  progress?: { files_done: number; files_total: number; chunks: number; phase: string };
  error?: string;
  onQuery: (stackIds: string[], query: string, k?: number) => Promise<StackQueryResult[]>;
}

function StackRow({
  stack,
  expanded,
  onToggle,
  onDelete,
  onRename,
  onAddFolder,
  onAddFile,
  onRemoveSource,
  onReindex,
  onCancelIndex,
  progress,
  error,
  onQuery,
}: StackRowProps) {
  const { t } = useT();
  const [searchText, setSearchText] = useState("");
  const [searching, setSearching] = useState(false);
  const [searchError, setSearchError] = useState<string | null>(null);
  const [results, setResults] = useState<StackQueryResult[] | null>(null);

  const isIndexing = progress != null && progress.phase !== "done";
  const indexedAt = formatIndexedAt(stack.indexed_at);

  const phaseLabel = (() => {
    if (!progress) return null;
    if (progress.phase === "walking") return t("KnowledgePanel.phaseWalking");
    if (progress.phase === "chunking") {
      return t("KnowledgePanel.phaseChunking", { done: progress.files_done, total: progress.files_total });
    }
    if (progress.phase === "embedding") return t("KnowledgePanel.phaseEmbedding", { chunks: progress.chunks });
    return t("KnowledgePanel.phaseDone");
  })();

  const handleSearch = useCallback(async () => {
    const query = searchText.trim();
    if (!query) return;
    setSearching(true);
    setSearchError(null);
    try {
      const hits = await onQuery([stack.id], query);
      setResults(hits);
    } catch (err) {
      setSearchError(err instanceof Error ? err.message : String(err));
    } finally {
      setSearching(false);
    }
  }, [searchText, stack.id, onQuery]);

  return (
    <div className="rounded-lg border border-border bg-background">
      <button
        type="button"
        onClick={onToggle}
        className="flex w-full items-center justify-between gap-2 px-3 py-2.5 text-left"
      >
        <div className="flex min-w-0 items-center gap-2">
          {expanded ? <ChevronDown size={14} className="shrink-0 text-faint" /> : <ChevronRight size={14} className="shrink-0 text-faint" />}
          <div className="min-w-0">
            <p className="truncate text-sm font-medium text-foreground">{stack.name}</p>
            <p className="truncate text-xs text-faint">
              {indexedAt
                ? t("KnowledgePanel.indexedAt", { when: indexedAt, count: stack.chunk_count })
                : t("KnowledgePanel.neverIndexed")}
            </p>
          </div>
        </div>
        <span className="shrink-0 font-mono text-[11px] text-faint">{stack.embedding.model_id_or_tag}</span>
      </button>

      {expanded && (
        <div className="flex flex-col gap-3 border-t border-border p-3">
          <div className="flex items-center justify-end gap-2">
            <Button variant="ghost" size="sm" onClick={onRename}>
              {t("KnowledgePanel.renameButton")}
            </Button>
            <Button variant="ghost" size="sm" onClick={onDelete}>
              <Trash2 size={13} />
              {t("KnowledgePanel.deleteButton")}
            </Button>
          </div>

          <div>
            <div className="mb-1.5 flex items-center justify-between">
              <p className="text-xs font-medium text-muted">{t("KnowledgePanel.sourcesHeading")}</p>
              <div className="flex gap-1.5">
                <Button variant="secondary" size="sm" onClick={onAddFolder}>
                  <FolderOpen size={12} />
                  {t("KnowledgePanel.addFolderButton")}
                </Button>
                <Button variant="secondary" size="sm" onClick={onAddFile}>
                  <FileText size={12} />
                  {t("KnowledgePanel.addFileButton")}
                </Button>
              </div>
            </div>
            {stack.sources.length === 0 ? (
              <p className="text-xs text-faint">{t("KnowledgePanel.noSourcesHint")}</p>
            ) : (
              <ul className="flex flex-col gap-1">
                {stack.sources.map((source) => (
                  <li
                    key={source.path}
                    className="flex items-center justify-between gap-2 rounded-md bg-surface-2 px-2 py-1"
                  >
                    <span className="truncate font-mono text-xs text-foreground">{source.path}</span>
                    <IconButton
                      variant="ghost"
                      size="sm"
                      aria-label={t("KnowledgePanel.removeSourceAriaLabel")}
                      onClick={() => onRemoveSource(source.path)}
                    >
                      <X size={12} />
                    </IconButton>
                  </li>
                ))}
              </ul>
            )}
          </div>

          <div>
            <div className="flex items-center gap-2">
              {isIndexing ? (
                <Button variant="danger" size="sm" onClick={onCancelIndex}>
                  {t("KnowledgePanel.cancelIndexButton")}
                </Button>
              ) : (
                <Button variant="primary" size="sm" onClick={onReindex} disabled={stack.sources.length === 0}>
                  {t("KnowledgePanel.reindexButton")}
                </Button>
              )}
              {phaseLabel && <span className="text-xs text-muted">{phaseLabel}</span>}
            </div>
            {error && <p className="mt-1.5 text-xs text-danger">{error}</p>}
          </div>

          <div className="border-t border-border pt-2.5">
            <p className="mb-1.5 text-xs font-medium text-muted">{t("KnowledgePanel.testSearchHeading")}</p>
            {stack.indexed_at == null ? (
              <p className="text-xs text-faint">{t("KnowledgePanel.notIndexedForSearch")}</p>
            ) : (
              <>
                <div className="flex items-center gap-2">
                  <input
                    type="text"
                    value={searchText}
                    onChange={(event) => setSearchText(event.target.value)}
                    onKeyDown={(event) => {
                      if (event.key === "Enter") void handleSearch();
                    }}
                    placeholder={t("KnowledgePanel.searchPlaceholder")}
                    className="h-8 min-w-0 flex-1 rounded-md border border-border bg-surface px-2.5 text-sm text-foreground placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
                  />
                  <Button variant="secondary" size="sm" onClick={() => void handleSearch()} disabled={searching || !searchText.trim()}>
                    <Search size={13} />
                    {searching ? t("KnowledgePanel.searchingLabel") : t("KnowledgePanel.searchButton")}
                  </Button>
                </div>
                {searchError && <p className="mt-1.5 text-xs text-danger">{searchError}</p>}
                {results && (
                  <ul className="mt-2 flex flex-col gap-1.5">
                    {results.length === 0 ? (
                      <li className="text-xs text-faint">{t("KnowledgePanel.noResults")}</li>
                    ) : (
                      results.map((hit, i) => (
                        <li key={`${hit.source_path}-${i}`} className="rounded-md bg-surface-2 p-2">
                          <div className="flex items-center justify-between gap-2">
                            <span className="truncate font-mono text-[11px] text-muted">{hit.source_path}</span>
                            <span className="shrink-0 text-[11px] text-faint">
                              {t("KnowledgePanel.scoreLabel", { score: hit.score.toFixed(3) })}
                            </span>
                          </div>
                          <p className="mt-1 line-clamp-3 text-xs text-foreground">{hit.text}</p>
                        </li>
                      ))
                    )}
                  </ul>
                )}
              </>
            )}
          </div>
        </div>
      )}
    </div>
  );
}

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { KeyboardEvent as ReactKeyboardEvent } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open as openFileDialog } from "@tauri-apps/plugin-dialog";
import {
  ArrowLeft,
  Bot,
  Camera,
  Check,
  Clipboard,
  FileText,
  Folder,
  ListPlus,
  Loader2,
  MessageSquare,
  Plug,
  Search,
  ShieldAlert,
  Sparkles,
  SquareLibrary,
  Wand2,
  Workflow,
  X,
  type LucideIcon,
} from "lucide-react";

import { useShallow } from "zustand/react/shallow";

import { useT } from "../../lib/i18n";
import { isImagePath, readImageAsDataUrl } from "../../lib/imageAttachment";
import { companionClient } from "../../lib/companionClient";
import {
  searchPaletteItems,
  type PaletteItem,
  type PaletteItemKind,
} from "../../lib/paletteSearch";
import {
  cancelSearchKnowledge,
  runApprovePending,
  runCreateTask,
  runQuickAction,
  runSearchKnowledge,
  runStartWorkflow,
  type CapturedContext,
  type QuickActionId,
} from "../../lib/paletteActions";
import { useSessionStore, sessionDisplayTitle } from "../../store/sessionStore";
import { useModelStore } from "../../store/modelStore";
import { useMcpStore } from "../../store/mcpStore";
import { useRecipeStore, type Recipe } from "../../store/recipeStore";
import { usePromptStore, selectSnippets } from "../../store/promptStore";
import { useStackStore } from "../../store/stackStore";
import { usePermissionStore, type PermissionMode } from "../../store/permissionStore";
import {
  DEFAULT_PROVIDER_MODEL_FILTER,
  useSettingsStore,
} from "../../store/settingsStore";
import { primaryRoot, useWorkspaceStore } from "../../store/workspaceStore";
import type { KnowledgeInspectorResponse } from "../../store/knowledgeV2Store";
import { Button, IconButton, StatusPill } from "../ui";
import type { SettingsTab } from "../Settings";
import { visibleProviderModelsForProvider } from "../../lib/providerModelSelection";

interface CommandPaletteProps {
  onClose: () => void;
  onOpenSettingsTab: (tab: SettingsTab) => void;
}

/** Extends the pure-search `PaletteItem` with the icon and the actual
 * side-effecting handler for this render — never persisted, never passed to
 * `searchPaletteItems` for anything but ranking (extra fields are ignored by
 * it). */
interface RichPaletteItem extends PaletteItem {
  icon: LucideIcon;
  onSelect: () => void | Promise<void>;
}

type PendingAction =
  | { view: "quickAction"; action: QuickActionId }
  | { view: "workflow"; recipe: Recipe }
  | { view: "knowledge" }
  | { view: "createTask" }
  | { view: "approval" };

const MODE_LABEL_KEYS: Record<PermissionMode, string> = {
  manual: "ModeSelector.modeManualLabel",
  acceptEdits: "ModeSelector.modeAcceptEditsLabel",
  smart: "ModeSelector.modeSmartLabel",
  plan: "ModeSelector.modePlanLabel",
  auto: "ModeSelector.modeAutoLabel",
  bypass: "ModeSelector.modeBypassLabel",
};

const KIND_ICONS: Record<PaletteItemKind, LucideIcon> = {
  quickAction: Sparkles,
  approval: ShieldAlert,
  session: MessageSquare,
  model: Bot,
  recipe: Workflow,
  snippet: SquareLibrary,
  connector: Plug,
  file: FileText,
};

function message(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function truncate(value: string, max: number): string {
  const trimmed = value.trim();
  return trimmed.length > max ? `${trimmed.slice(0, max)}…` : trimmed;
}

/** Mirrors `ModelSwitcher.tsx`'s own label logic exactly, so the palette's
 * scope indicator never drifts from what the model pill in chat shows. */
function activeModelLabel(model: ReturnType<typeof useModelStore.getState>): string | null {
  if (model.activeProvider === "local" && model.active) return model.active.name;
  if (model.activeProvider === "ollama" && model.activeOllamaModel) return model.activeOllamaModel;
  if (model.activeProvider === "provider" && model.activeProviderModel) return model.activeProviderModel;
  return null;
}

/**
 * Raycast-style global command surface (ROADMAP.md, Phase 1). Renders
 * inside the main window (see `App.tsx`) so it can reuse the companion
 * overlay's own capture-grant commands for context capture, and reuses
 * every command's *existing* execution path (`runAgentTurn`, `runRecipeNow`,
 * `knowledgeV2Store.query`, `permissionStore.respond`, `recipeStore.save`)
 * for every action — see `paletteActions.ts`'s module doc.
 */
export function CommandPalette({ onClose, onOpenSettingsTab }: CommandPaletteProps) {
  const { t } = useT();
  const inputRef = useRef<HTMLInputElement>(null);

  const sessions = useSessionStore((s) => s.sessions);
  const activeSessionId = useSessionStore((s) => s.activeSessionId);
  const switchSession = useSessionStore((s) => s.switchSession);

  const modelState = useModelStore();
  const providerModelFilters = useSettingsStore((s) => s.providerModelFilters);
  const installedChatModels = useMemo(
    () => modelState.installed.filter((model) => model.kind === "chat"),
    [modelState.installed],
  );

  const mcpServers = useMcpStore((s) => s.servers);
  const recipes = useRecipeStore((s) => s.recipes);
  // `useShallow` is load-bearing here — see `PersonaSelector.tsx`'s identical
  // note: `selectSnippets` filters `entries` into a new array on every call,
  // and without a shallow-equality wrapper that defeats `useSyncExternalStore`'s
  // "did the snapshot actually change" check, causing an infinite render loop.
  const snippets = usePromptStore(useShallow(selectSnippets));
  const stacks = useStackStore((s) => s.stacks);
  const refreshStacks = useStackStore((s) => s.refresh);

  const pending = usePermissionStore((s) => s.pending);
  const permissionMode = usePermissionStore((s) => s.mode);
  const roots = useWorkspaceStore((s) => s.roots);

  const [query, setQuery] = useState("");
  const [activeIndex, setActiveIndex] = useState(0);
  const [capturedContext, setCapturedContext] = useState<CapturedContext | null>(null);
  const [pendingAction, setPendingAction] = useState<PendingAction | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [workspaceFiles, setWorkspaceFiles] = useState<{ path: string; is_dir: boolean }[]>([]);

  const [translateLanguage, setTranslateLanguage] = useState("");
  const [askQuestion, setAskQuestion] = useState("");
  const [taskName, setTaskName] = useState("");
  const [taskPrompt, setTaskPrompt] = useState("");
  const [taskSaved, setTaskSaved] = useState<Recipe | null>(null);
  const [knowledgeStackId, setKnowledgeStackId] = useState("");
  const [knowledgeQuery, setKnowledgeQuery] = useState("");
  const [knowledgeResult, setKnowledgeResult] = useState<KnowledgeInspectorResponse | null>(null);
  const [knowledgeRunId, setKnowledgeRunId] = useState<string | null>(null);

  useEffect(() => inputRef.current?.focus(), []);
  useEffect(() => {
    void refreshStacks().catch(() => undefined);
  }, [refreshStacks]);
  useEffect(() => {
    if (roots.length === 0) return;
    invoke<{ entries: { path: string; is_dir: boolean }[] }>("list_workspace_paths")
      .then((result) => setWorkspaceFiles(result.entries.filter((entry) => !entry.is_dir)))
      .catch(() => setWorkspaceFiles([]));
  }, [roots.length]);
  useEffect(() => {
    if (!knowledgeStackId && stacks.length > 0) setKnowledgeStackId(stacks[0].id);
  }, [knowledgeStackId, stacks]);
  useEffect(() => {
    if (capturedContext?.text && !taskPrompt) setTaskPrompt(capturedContext.text);
    if (capturedContext?.text && !knowledgeQuery) setKnowledgeQuery(truncate(capturedContext.text, 200));
  }, [capturedContext]); // eslint-disable-line react-hooks/exhaustive-deps

  // Global Escape always closes the palette outright, even from a sub-view —
  // matches PermissionModal's own capture-first Escape handling so a
  // pending permission prompt (which renders on top of everything,
  // including this) still gets first claim on the key.
  useEffect(() => {
    function handleKeyDown(event: KeyboardEvent) {
      if (event.key !== "Escape" || event.defaultPrevented) return;
      event.preventDefault();
      onClose();
    }
    window.addEventListener("keydown", handleKeyDown, true);
    return () => window.removeEventListener("keydown", handleKeyDown, true);
  }, [onClose]);

  const modelLabel = activeModelLabel(modelState);
  const connectedToolCount = mcpServers
    .filter((server) => server.enabled && server.status === "connected")
    .reduce((total, server) => total + server.tools.length, 0);
  const workspaceLabel = primaryRoot(roots)?.label ?? t("CommandPalette.noWorkspace");

  const runQuick = useCallback(
    async (action: QuickActionId, extra = "") => {
      setBusy(true);
      setError(null);
      try {
        await runQuickAction(action, capturedContext, extra);
        onClose();
      } catch (caught) {
        setError(message(caught));
      } finally {
        setBusy(false);
      }
    },
    [capturedContext, onClose],
  );

  const runWorkflow = useCallback(
    async (recipe: Recipe) => {
      setBusy(true);
      setError(null);
      try {
        await runStartWorkflow(recipe, capturedContext);
        onClose();
      } catch (caught) {
        setError(message(caught));
      } finally {
        setBusy(false);
      }
    },
    [capturedContext, onClose],
  );

  const runKnowledgeSearch = useCallback(async () => {
    setBusy(true);
    setError(null);
    try {
      const outcome = await runSearchKnowledge(knowledgeStackId, knowledgeQuery);
      setKnowledgeRunId(outcome.runId);
      setKnowledgeResult(outcome.response);
    } catch (caught) {
      setError(message(caught));
    } finally {
      setBusy(false);
    }
  }, [knowledgeQuery, knowledgeStackId]);

  const saveTask = useCallback(async () => {
    setBusy(true);
    setError(null);
    try {
      const recipe = await runCreateTask(taskName, taskPrompt);
      setTaskSaved(recipe);
    } catch (caught) {
      setError(message(caught));
    } finally {
      setBusy(false);
    }
  }, [taskName, taskPrompt]);

  const decidePending = useCallback(
    async (allow: boolean) => {
      setBusy(true);
      setError(null);
      try {
        await runApprovePending(allow);
        onClose();
      } catch (caught) {
        setError(message(caught));
      } finally {
        setBusy(false);
      }
    },
    [onClose],
  );

  const captureClipboard = useCallback(async () => {
    setError(null);
    try {
      const text = await navigator.clipboard.readText();
      if (!text.trim()) throw new Error(t("CommandPalette.clipboardEmpty"));
      // Little Monkey has no OS-level "read the currently selected text"
      // capability on any platform (the companion overlay's own "Paste"
      // button — the only other outside-app capture in this app — has the
      // same limitation), so the clipboard is the real mechanism behind
      // both the "selected text" and "clipboard text" roadmap inputs.
      setCapturedContext({ source: "clipboard", text, imageDataUrl: null });
    } catch (caught) {
      setError(message(caught));
    }
  }, [t]);

  const captureFilePath = useCallback(async (path: string) => {
    setError(null);
    setBusy(true);
    try {
      if (isImagePath(path)) {
        const imageDataUrl = await readImageAsDataUrl(path);
        setCapturedContext({ source: "file", text: null, imageDataUrl, path });
      } else {
        const text = await invoke<string>("tool_read_file", { path });
        setCapturedContext({ source: "file", text, imageDataUrl: null, path });
      }
    } catch (caught) {
      setError(message(caught));
    } finally {
      setBusy(false);
    }
  }, []);

  const captureFileDialog = useCallback(async () => {
    const selected = await openFileDialog({ multiple: false, directory: false });
    if (!selected || Array.isArray(selected)) return;
    await captureFilePath(selected);
  }, [captureFilePath]);

  const captureScreenshot = useCallback(async () => {
    setError(null);
    setBusy(true);
    try {
      const grant = await companionClient.grant("screen", 15 * 60_000, "command-palette");
      const artifact = await companionClient.captureScreen(grant.grantId);
      const imageDataUrl = await companionClient.imageDataUrl(artifact.blob.id, artifact.mediaType);
      setCapturedContext({ source: "screenshot", text: null, imageDataUrl });
    } catch (caught) {
      setError(message(caught));
    } finally {
      setBusy(false);
    }
  }, []);

  const items: RichPaletteItem[] = useMemo(() => {
    const list: RichPaletteItem[] = [];

    if (pending) {
      list.push({
        id: "approval:pending",
        kind: "approval",
        title: t("CommandPalette.action.approvePending"),
        subtitle: pending.tool,
        keywords: ["approve", "allow", "permission", "deny"],
        sensitive: true,
        icon: KIND_ICONS.approval,
        onSelect: () => setPendingAction({ view: "approval" }),
      });
    }

    const quickActionDefs: { id: QuickActionId; title: string; subtitle: string; icon: LucideIcon; keywords: string[] }[] = [
      { id: "summarize", title: t("CommandPalette.action.summarize"), subtitle: t("CommandPalette.action.summarizeHint"), icon: Sparkles, keywords: ["tldr", "condense"] },
      { id: "rewrite", title: t("CommandPalette.action.rewrite"), subtitle: t("CommandPalette.action.rewriteHint"), icon: Wand2, keywords: ["edit", "polish", "improve"] },
      { id: "translate", title: t("CommandPalette.action.translate"), subtitle: t("CommandPalette.action.translateHint"), icon: SquareLibrary, keywords: ["language"] },
      { id: "askModel", title: t("CommandPalette.action.askModel"), subtitle: t("CommandPalette.action.askModelHint"), icon: MessageSquare, keywords: ["chat", "question", "ask"] },
    ];
    for (const def of quickActionDefs) {
      list.push({
        id: `quickAction:${def.id}`,
        kind: "quickAction",
        title: def.title,
        subtitle: def.subtitle,
        keywords: def.keywords,
        sensitive: true,
        icon: def.icon,
        onSelect: () => {
          setError(null);
          setPendingAction({ view: "quickAction", action: def.id });
        },
      });
    }
    list.push({
      id: "quickAction:startWorkflow",
      kind: "quickAction",
      title: t("CommandPalette.action.startWorkflow"),
      subtitle: t("CommandPalette.action.startWorkflowHint"),
      keywords: ["recipe", "task", "automation", "run"],
      sensitive: true,
      icon: Workflow,
      onSelect: () => setQuery(""),
    });
    list.push({
      id: "quickAction:searchKnowledge",
      kind: "quickAction",
      title: t("CommandPalette.action.searchKnowledge"),
      subtitle: t("CommandPalette.action.searchKnowledgeHint"),
      keywords: ["knowledge", "stack", "rag", "docs"],
      sensitive: true,
      icon: SquareLibrary,
      onSelect: () => setPendingAction({ view: "knowledge" }),
    });
    list.push({
      id: "quickAction:createTask",
      kind: "quickAction",
      title: t("CommandPalette.action.createTask"),
      subtitle: t("CommandPalette.action.createTaskHint"),
      keywords: ["task", "recipe", "schedule", "save"],
      sensitive: true,
      icon: ListPlus,
      onSelect: () => setPendingAction({ view: "createTask" }),
    });

    for (const session of sessions) {
      if (session.archived) continue;
      list.push({
        id: `session:${session.id}`,
        kind: "session",
        title: sessionDisplayTitle(session),
        subtitle: session.id === activeSessionId ? t("CommandPalette.activeSession") : undefined,
        keywords: session.workspacePath ? [session.workspacePath] : [],
        sensitive: false,
        icon: KIND_ICONS.session,
        onSelect: () => {
          switchSession(session.id);
          onClose();
        },
      });
    }

    for (const model of installedChatModels) {
      list.push({
        id: `model:local:${model.id}`,
        kind: "model",
        title: model.name,
        subtitle: t("CommandPalette.model.local"),
        keywords: [model.repo],
        sensitive: false,
        icon: KIND_ICONS.model,
        onSelect: () => {
          modelState.start(model).catch((caught: unknown) => console.error(caught));
          onClose();
        },
      });
    }
    for (const ollamaModel of modelState.ollamaModels) {
      list.push({
        id: `model:ollama:${ollamaModel.name}`,
        kind: "model",
        title: ollamaModel.name,
        subtitle: t("CommandPalette.model.ollama"),
        sensitive: false,
        icon: KIND_ICONS.model,
        onSelect: () => {
          modelState.useOllamaModel(ollamaModel.name);
          onClose();
        },
      });
    }
    for (const provider of modelState.providers) {
      if (!provider.has_key) continue;
      const providerModels = visibleProviderModelsForProvider(
        provider.id,
        modelState.providerModels[provider.id] ?? [],
        providerModelFilters[provider.id] ?? DEFAULT_PROVIDER_MODEL_FILTER,
        modelState,
      );
      for (const providerModel of providerModels) {
        list.push({
          id: `model:provider:${provider.id}:${providerModel.id}`,
          kind: "model",
          title: providerModel.id,
          subtitle: provider.label,
          sensitive: false,
          icon: KIND_ICONS.model,
          onSelect: () => {
            modelState.useProviderModel(provider.id, providerModel.id);
            onClose();
          },
        });
      }
    }

    for (const discovered of recipes) {
      if (!discovered.recipe) continue;
      const recipe = discovered.recipe;
      list.push({
        id: `recipe:${discovered.path}`,
        kind: "recipe",
        title: recipe.name,
        subtitle: recipe.description ?? t("CommandPalette.recipe.defaultSubtitle"),
        keywords: ["workflow", "task"],
        sensitive: true,
        icon: KIND_ICONS.recipe,
        onSelect: () => setPendingAction({ view: "workflow", recipe }),
      });
    }

    for (const snippet of snippets) {
      list.push({
        id: `snippet:${snippet.id}`,
        kind: "snippet",
        title: snippet.name,
        subtitle: snippet.description ?? `/${snippet.command}`,
        sensitive: false,
        icon: KIND_ICONS.snippet,
        onSelect: () => setCapturedContext({ source: "snippet", text: snippet.content, imageDataUrl: null }),
      });
    }

    for (const server of mcpServers) {
      list.push({
        id: `connector:${server.id}`,
        kind: "connector",
        title: server.label,
        subtitle: t(`CommandPalette.connector.status.${server.status}`),
        keywords: ["mcp", "connector", "tool"],
        sensitive: false,
        icon: KIND_ICONS.connector,
        onSelect: () => {
          onOpenSettingsTab("connectors");
          onClose();
        },
      });
    }

    for (const file of workspaceFiles.slice(0, 500)) {
      const base = file.path.split(/[\\/]/).pop() ?? file.path;
      list.push({
        id: `file:${file.path}`,
        kind: "file",
        title: base,
        subtitle: file.path,
        keywords: [file.path],
        sensitive: false,
        icon: KIND_ICONS.file,
        onSelect: () => captureFilePath(file.path),
      });
    }

    return list;
  }, [
    pending,
    sessions,
    activeSessionId,
    installedChatModels,
    modelState,
    providerModelFilters,
    recipes,
    snippets,
    mcpServers,
    workspaceFiles,
    switchSession,
    onOpenSettingsTab,
    onClose,
    captureFilePath,
    t,
  ]);

  const results = useMemo(() => searchPaletteItems(items, query, 60), [items, query]);

  useEffect(() => setActiveIndex(0), [query, items.length]);

  const activate = useCallback(
    async (item: RichPaletteItem) => {
      setError(null);
      try {
        await item.onSelect();
      } catch (caught) {
        setError(message(caught));
      }
    },
    [],
  );

  function handleSearchKeyDown(event: ReactKeyboardEvent<HTMLInputElement>) {
    if (event.key === "ArrowDown") {
      event.preventDefault();
      setActiveIndex((index) => Math.min(index + 1, results.length - 1));
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      setActiveIndex((index) => Math.max(index - 1, 0));
    } else if (event.key === "Enter") {
      event.preventDefault();
      const target = results[activeIndex]?.item as RichPaletteItem | undefined;
      if (target) void activate(target);
    }
  }

  function backToSearch() {
    setPendingAction(null);
    setError(null);
    setKnowledgeResult(null);
    setTaskSaved(null);
  }

  return (
    <div
      className="fixed inset-0 z-40 flex items-start justify-center bg-black/40 p-4 pt-[12vh] backdrop-blur-[2px]"
      role="dialog"
      aria-modal="true"
      aria-labelledby="command-palette-title"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget) onClose();
      }}
    >
      <div className="flex max-h-[72vh] w-full max-w-2xl min-h-0 flex-col overflow-hidden rounded-xl border border-border bg-background shadow-2xl">
        <header className="flex shrink-0 items-center gap-2 border-b border-border px-4 py-3">
          <Search size={16} className="shrink-0 text-faint" />
          <span id="command-palette-title" className="sr-only">{t("CommandPalette.title")}</span>
          <input
            ref={inputRef}
            value={query}
            disabled={pendingAction !== null}
            onChange={(event) => setQuery(event.target.value)}
            onKeyDown={handleSearchKeyDown}
            placeholder={t("CommandPalette.placeholder")}
            className="h-8 min-w-0 flex-1 bg-transparent text-sm outline-none placeholder:text-faint disabled:opacity-50"
          />
          {busy && <Loader2 size={15} className="animate-spin text-faint" />}
          <IconButton size="sm" onClick={onClose} aria-label={t("CommandPalette.close")}>
            <X size={16} />
          </IconButton>
        </header>

        <div className="flex shrink-0 flex-wrap items-center gap-1.5 border-b border-border bg-surface px-4 py-2 text-[11px] text-muted">
          <span className="font-medium text-faint">{t("CommandPalette.scopeLabel")}</span>
          <StatusPill tone="neutral">{t("CommandPalette.scopeWorkspace", { workspace: workspaceLabel })}</StatusPill>
          <StatusPill tone={modelLabel ? "success" : "warning"}>
            {modelLabel ? t("CommandPalette.scopeModel", { model: modelLabel }) : t("CommandPalette.scopeNoModel")}
          </StatusPill>
          <StatusPill tone="neutral">{t("CommandPalette.scopeTools", { count: connectedToolCount })}</StatusPill>
          <StatusPill tone={permissionMode === "bypass" ? "danger" : permissionMode === "auto" ? "warning" : "neutral"}>
            {t("CommandPalette.scopePrivacy", { mode: t(MODE_LABEL_KEYS[permissionMode]) })}
          </StatusPill>
        </div>

        {pendingAction === null && (
          <div className="flex shrink-0 flex-wrap items-center gap-2 border-b border-border px-4 py-2">
            <span className="text-[11px] font-medium text-faint">{t("CommandPalette.captureLabel")}</span>
            <Button size="sm" variant="secondary" onClick={() => void captureClipboard()}>
              <Clipboard size={13} />{t("CommandPalette.captureClipboard")}
            </Button>
            <Button size="sm" variant="secondary" onClick={() => void captureFileDialog()}>
              <Folder size={13} />{t("CommandPalette.captureFile")}
            </Button>
            <Button size="sm" variant="secondary" onClick={() => void captureScreenshot()}>
              <Camera size={13} />{t("CommandPalette.captureScreenshot")}
            </Button>
            {capturedContext && (
              <span className="ml-auto flex items-center gap-2 rounded-full bg-surface-2 px-2.5 py-1 text-[11px] text-foreground">
                <Check size={12} className="text-success" />
                {t(`CommandPalette.capturedFrom.${capturedContext.source}`)}
                <button
                  type="button"
                  onClick={() => setCapturedContext(null)}
                  aria-label={t("CommandPalette.removeCapture")}
                  className="text-faint hover:text-foreground"
                >
                  <X size={12} />
                </button>
              </span>
            )}
          </div>
        )}

        {error && (
          <p role="alert" className="mx-4 mt-2 shrink-0 rounded-md border border-danger/40 bg-danger-soft p-2 text-xs text-danger">
            {error}
          </p>
        )}

        {pendingAction === null ? (
          <ul className="min-h-0 flex-1 overflow-y-auto py-1 [overscroll-behavior:contain]" aria-live="polite">
            {results.length === 0 && (
              <li className="px-4 py-8 text-center text-sm text-faint">{t("CommandPalette.noResults")}</li>
            )}
            {results.map((result, index) => {
              const item = result.item as RichPaletteItem;
              const Icon = item.icon;
              const isActive = index === activeIndex;
              return (
                <li key={item.id}>
                  <button
                    type="button"
                    onMouseEnter={() => setActiveIndex(index)}
                    onClick={() => void activate(item)}
                    className={`flex w-full items-center gap-3 px-4 py-2 text-left ${
                      isActive ? "bg-surface-2" : "hover:bg-surface-2"
                    }`}
                  >
                    <Icon size={15} className="shrink-0 text-faint" />
                    <span className="min-w-0 flex-1">
                      <span className="block truncate text-sm text-foreground">{item.title}</span>
                      {item.subtitle && <span className="block truncate text-xs text-muted">{item.subtitle}</span>}
                    </span>
                    <StatusPill tone="neutral">{t(`CommandPalette.kind.${item.kind}`)}</StatusPill>
                  </button>
                </li>
              );
            })}
          </ul>
        ) : (
          <div className="min-h-0 flex-1 overflow-y-auto p-4">
            <button
              type="button"
              onClick={backToSearch}
              className="mb-3 inline-flex items-center gap-1.5 text-xs font-medium text-muted hover:text-foreground"
            >
              <ArrowLeft size={13} />{t("CommandPalette.back")}
            </button>

            {capturedContext && (
              <div className="mb-3 rounded-md border border-border bg-surface p-2.5 text-xs">
                <p className="mb-1 font-medium text-faint">{t("CommandPalette.contextPreviewLabel")}</p>
                {capturedContext.imageDataUrl ? (
                  <img src={capturedContext.imageDataUrl} alt="" className="max-h-32 rounded border border-border object-contain" />
                ) : (
                  <p className="max-h-24 overflow-y-auto whitespace-pre-wrap break-words font-mono text-muted">
                    {truncate(capturedContext.text ?? "", 600)}
                  </p>
                )}
              </div>
            )}

            {pendingAction.view === "approval" && pending && (
              <div className="space-y-3">
                <p className="text-sm font-medium text-foreground">{t("PermissionModal.wantsToRunTool")} <span className="font-mono">{pending.tool}</span></p>
                <div className="max-h-40 overflow-auto whitespace-pre-wrap break-all rounded-md border border-border bg-surface-2 p-2.5 font-mono text-xs text-muted">
                  {pending.detail}
                </div>
                <div className="flex justify-end gap-2">
                  <Button variant="secondary" disabled={busy} onClick={() => void decidePending(false)}>{t("PermissionModal.denyButton")}</Button>
                  <Button variant="primary" disabled={busy} onClick={() => void decidePending(true)}>{t("PermissionModal.allowOnceButton")}</Button>
                </div>
              </div>
            )}

            {pendingAction.view === "quickAction" && (
              <div className="space-y-3">
                {pendingAction.action === "translate" && (
                  <label className="block text-xs text-muted">
                    {t("CommandPalette.translateLanguageLabel")}
                    <input
                      autoFocus
                      value={translateLanguage}
                      onChange={(event) => setTranslateLanguage(event.target.value)}
                      placeholder={t("CommandPalette.translateLanguagePlaceholder")}
                      className="mt-1 w-full rounded border border-border bg-background px-2 py-1.5 text-sm text-foreground"
                    />
                  </label>
                )}
                {pendingAction.action === "askModel" && (
                  <label className="block text-xs text-muted">
                    {t("CommandPalette.askQuestionLabel")}
                    <textarea
                      autoFocus
                      value={askQuestion}
                      onChange={(event) => setAskQuestion(event.target.value)}
                      placeholder={t("CommandPalette.askQuestionPlaceholder")}
                      className="mt-1 min-h-20 w-full resize-y rounded border border-border bg-background px-2 py-1.5 text-sm text-foreground"
                    />
                  </label>
                )}
                <p className="text-xs text-muted">{t("CommandPalette.willRunNotice")}</p>
                <div className="flex justify-end">
                  <Button
                    variant="primary"
                    disabled={busy || (pendingAction.action === "askModel" && !askQuestion.trim())}
                    onClick={() =>
                      void runQuick(
                        pendingAction.action,
                        pendingAction.action === "translate" ? translateLanguage : pendingAction.action === "askModel" ? askQuestion : "",
                      )
                    }
                  >
                    {t("CommandPalette.runButton")}
                  </Button>
                </div>
              </div>
            )}

            {pendingAction.view === "workflow" && (
              <div className="space-y-3">
                <div className="rounded-md border border-border bg-surface p-2.5 text-xs">
                  <p className="font-medium text-foreground">{pendingAction.recipe.name}</p>
                  {pendingAction.recipe.description && <p className="mt-1 text-muted">{pendingAction.recipe.description}</p>}
                  <p className="mt-1 text-faint">{t("CommandPalette.recipe.permissionMode", { mode: pendingAction.recipe.permission_mode })}</p>
                </div>
                <p className="text-xs text-muted">{t("CommandPalette.willRunNotice")}</p>
                <div className="flex justify-end">
                  <Button variant="primary" disabled={busy} onClick={() => void runWorkflow(pendingAction.recipe)}>
                    {t("CommandPalette.runButton")}
                  </Button>
                </div>
              </div>
            )}

            {pendingAction.view === "knowledge" && (
              <div className="space-y-3">
                <label className="block text-xs text-muted">
                  {t("CommandPalette.knowledgeStackLabel")}
                  <select
                    value={knowledgeStackId}
                    onChange={(event) => setKnowledgeStackId(event.target.value)}
                    className="mt-1 w-full rounded border border-border bg-background px-2 py-1.5 text-sm text-foreground"
                  >
                    {stacks.length === 0 && <option value="">{t("CommandPalette.knowledgeNoStacks")}</option>}
                    {stacks.map((stack) => (
                      <option key={stack.id} value={stack.id}>{stack.name}</option>
                    ))}
                  </select>
                </label>
                <label className="block text-xs text-muted">
                  {t("CommandPalette.knowledgeQueryLabel")}
                  <input
                    value={knowledgeQuery}
                    onChange={(event) => setKnowledgeQuery(event.target.value)}
                    onKeyDown={(event) => event.key === "Enter" && void runKnowledgeSearch()}
                    className="mt-1 w-full rounded border border-border bg-background px-2 py-1.5 text-sm text-foreground"
                  />
                </label>
                <div className="flex justify-end gap-2">
                  {knowledgeRunId && busy && (
                    <Button variant="secondary" onClick={() => void cancelSearchKnowledge(knowledgeRunId)}>
                      {t("CommandPalette.cancelButton")}
                    </Button>
                  )}
                  <Button variant="primary" disabled={busy || !knowledgeStackId} onClick={() => void runKnowledgeSearch()}>
                    {t("CommandPalette.searchButton")}
                  </Button>
                </div>
                {knowledgeResult && (
                  <ul className="space-y-2">
                    {knowledgeResult.search.hits.length === 0 && (
                      <li className="text-xs text-faint">{t("CommandPalette.noResults")}</li>
                    )}
                    {knowledgeResult.search.hits.map((hit) => (
                      <li key={hit.chunk.chunk_id} className="rounded-md border border-border bg-surface p-2 text-xs">
                        <p className="truncate font-mono text-faint">{hit.chunk.citation.canonical_uri}</p>
                        <p className="mt-1 whitespace-pre-wrap text-muted">{truncate(hit.chunk.text, 320)}</p>
                      </li>
                    ))}
                  </ul>
                )}
              </div>
            )}

            {pendingAction.view === "createTask" && (
              <div className="space-y-3">
                {taskSaved ? (
                  <p className="rounded-md border border-success/40 bg-success-soft p-2.5 text-sm text-success">
                    {t("CommandPalette.taskSaved", { name: taskSaved.name })}
                  </p>
                ) : (
                  <>
                    <label className="block text-xs text-muted">
                      {t("CommandPalette.taskNameLabel")}
                      <input
                        autoFocus
                        value={taskName}
                        onChange={(event) => setTaskName(event.target.value)}
                        className="mt-1 w-full rounded border border-border bg-background px-2 py-1.5 text-sm text-foreground"
                      />
                    </label>
                    <label className="block text-xs text-muted">
                      {t("CommandPalette.taskPromptLabel")}
                      <textarea
                        value={taskPrompt}
                        onChange={(event) => setTaskPrompt(event.target.value)}
                        className="mt-1 min-h-24 w-full resize-y rounded border border-border bg-background px-2 py-1.5 text-sm text-foreground"
                      />
                    </label>
                    <p className="text-xs text-muted">
                      {t("CommandPalette.taskModelNotice", { model: modelLabel ?? t("CommandPalette.scopeNoModel") })}
                    </p>
                    <div className="flex justify-end">
                      <Button variant="primary" disabled={busy} onClick={() => void saveTask()}>
                        {t("CommandPalette.saveButton")}
                      </Button>
                    </div>
                  </>
                )}
              </div>
            )}
          </div>
        )}

        <footer className="flex shrink-0 items-center justify-between border-t border-border bg-surface px-4 py-1.5 text-[11px] text-faint">
          <span>{t("CommandPalette.hintNavigate")}</span>
          <span>{t("CommandPalette.hintClose")}</span>
        </footer>
      </div>
    </div>
  );
}

export default CommandPalette;

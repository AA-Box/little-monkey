import { useCallback, useEffect, useState } from "react";
import { invoke, isTauri } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { PanelRight, PanelRightClose, SquareTerminal, X } from "lucide-react";

import { ChatSessionList, ChatWindow, CompareView, CrewView } from "./components/Chat";
import { AppMenu } from "./components/AppMenu";
import { ErrorBoundary } from "./components/ErrorBoundary";
import { RunCenter } from "./components/Runs";
import { BrowserWorkbench } from "./components/Browser";
import { IssueToPrPanel } from "./components/IssueToPr";
import { SideTaskDrawer } from "./components/SideTasks";
import { GlobalSearch } from "./components/Search";
import { AgentInbox } from "./components/Inbox";
import { TerminalPanel } from "./components/Terminal";
import { SettingsModal } from "./components/Settings";
import type { SettingsTab } from "./components/Settings";
import { useRunStore } from "./store/runStore";
import { useSideTaskStore } from "./store/sideTaskStore";
import { ArtifactPane, FileTree, DiffViewer, PermissionModal, ApprovalChainModal, SessionGrantBanner } from "./components/Workspace";
import { IconButton, Button } from "./components/ui";
import { useSessionStore } from "./store/sessionStore";
import { primaryRoot, useWorkspaceStore } from "./store/workspaceStore";
import { useModelStore } from "./store/modelStore";
import { useMcpStore } from "./store/mcpStore";
import { useArtifactStore } from "./store/artifactStore";
import { usePermissionStore } from "./store/permissionStore";
import { useShortcutStore } from "./store/shortcutStore";
import { useRecipeStore, subscribeToRecipeChanges } from "./store/recipeStore";
import { subscribeToLocalAppsChanges, subscribeToLocalAppRunRequests } from "./store/localAppsStore";
import { hydrateAutomations } from "./store/automationsStore";
import { startScheduler } from "./lib/scheduler";
import { startBackupScheduler } from "./lib/backupScheduler";
import { useT } from "./lib/i18n";
import {
  detectShortcutPlatform,
  shortcutIdForEvent,
  shouldHandleGlobalShortcut,
  type ShortcutIdForScope,
} from "./lib/shortcuts";
import { onRunCancellationRequested } from "./lib/runProtocol";
import { cancelRegisteredRun } from "./lib/runCancellationRegistry";
import { recoverDaemonDesktopTurns } from "./lib/agentLoop";

/** A file currently previewed in the Workspace panel, with a baseline snapshot
 * (captured the moment it was opened) so edits made by the agent afterwards
 * can be diffed against it. */
interface SelectedFile {
  path: string;
  original: string;
  current: string;
}

function formatError(err: unknown): string {
  if (err instanceof Error) return err.message;
  if (typeof err === "string") return err;
  try {
    return JSON.stringify(err);
  } catch {
    return String(err);
  }
}

function App() {
  const { t } = useT();
  const activeSessionId = useSessionStore((s) => s.activeSessionId);
  const activeComparisonId = useSessionStore((s) =>
    s.sessions.find((session) => session.id === s.activeSessionId)?.comparisonBranch?.comparisonId ?? null
  );
  const activeCrewSessionId = useSessionStore((s) =>
    s.sessions.find((session) => session.id === s.activeSessionId)?.crewRun ? s.activeSessionId : null
  );
  const newSession = useSessionStore((s) => s.newSession);
  const splitSessionId = useSessionStore((s) => s.splitSessionId);
  const splitTitle = useSessionStore((s) =>
    s.splitSessionId === null ? null : s.sessions.find((x) => x.id === s.splitSessionId)?.title ?? null
  );
  const splitCrewSessionId = useSessionStore((s) =>
    s.splitSessionId !== null && s.sessions.find((session) => session.id === s.splitSessionId)?.crewRun
      ? s.splitSessionId
      : null
  );
  const closeSplit = useSessionStore((s) => s.closeSplit);
  const rootsVersion = useWorkspaceStore((s) => s.rootsVersion);
  const refreshRoots = useWorkspaceStore((s) => s.refreshRoots);
  const refreshRecent = useWorkspaceStore((s) => s.refreshRecent);
  const refreshModels = useModelStore((s) => s.refresh);
  const refreshOllama = useModelStore((s) => s.refreshOllama);
  const refreshProviders = useModelStore((s) => s.refreshProviders);
  const refreshRecipes = useRecipeStore((s) => s.refresh);
  const refreshMcp = useMcpStore((s) => s.refresh);
  const connectMcp = useMcpStore((s) => s.connect);
  const activeArtifact = useArtifactStore((s) => s.active);
  const permissionPending = usePermissionStore((s) => s.pending !== null);

  const [workspacePanelOpen, setWorkspacePanelOpen] = useState(true);
  const [selectedFile, setSelectedFile] = useState<SelectedFile | null>(null);
  const [diffLoading, setDiffLoading] = useState(false);
  const [diffError, setDiffError] = useState<string | null>(null);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [runCenterOpen, setRunCenterOpen] = useState(false);
  const [browserWorkbenchOpen, setBrowserWorkbenchOpen] = useState(false);
  const [issueToPrOpen, setIssueToPrOpen] = useState(false);
  const [globalSearchOpen, setGlobalSearchOpen] = useState(false);
  const [agentInboxOpen, setAgentInboxOpen] = useState(false);
  const [terminalOpen, setTerminalOpen] = useState(false);
  // Tab Settings should jump to the moment it opens — set alongside
  // `settingsOpen` by anything that deep-links into a specific tab (right
  // now just `PersonaSelector`'s "Manage prompts…" row); left `undefined`
  // for the normal "open on whatever tab was last active" path (AppMenu).
  // Reset back to `undefined` on close (see the `SettingsModal` below) so a
  // one-off deep link doesn't stick around and hijack every later normal
  // open too.
  const [settingsInitialTab, setSettingsInitialTab] = useState<SettingsTab | undefined>(undefined);
  // A sequence number makes repeated requests for the same tab observable
  // even while Settings is already open and the user has navigated away.
  const [settingsTabRequest, setSettingsTabRequest] = useState(0);

  const openSettingsTab = useCallback((tab: SettingsTab) => {
    setRunCenterOpen(false);
    setBrowserWorkbenchOpen(false);
    setIssueToPrOpen(false);
    setGlobalSearchOpen(false);
    setAgentInboxOpen(false);
    setSettingsInitialTab(tab);
    setSettingsTabRequest((request) => request + 1);
    setSettingsOpen(true);
  }, []);

  const handleManagePrompts = useCallback(() => {
    openSettingsTab("prompts");
  }, [openSettingsTab]);

  // App-wide accelerators. The same definitions are rendered by the
  // Keyboard Shortcuts Settings panel, so a displayed binding always has a
  // live handler behind it with native modifiers on macOS, Windows, and Linux.
  useEffect(() => {
    function handleKeyDown(event: KeyboardEvent) {
      // Read at event time rather than closing over a render-time snapshot:
      // edits made in Settings take effect on the very next keydown. Recording
      // must suspend dispatch because this capture listener runs before the
      // recorder's target-level key handler can cancel an existing shortcut.
      const { overrides, recordingId } = useShortcutStore.getState();
      if (!shouldHandleGlobalShortcut(event, permissionPending, recordingId !== null)) return;
      const shortcut = shortcutIdForEvent(event, "global", detectShortcutPlatform(), overrides);
      if (!shortcut) return;

      // Session-scoped commands (pin/rename/fork/archive/open-in-X) act on
      // whichever session is active — read fresh rather than via a selector
      // so a session switch made without a re-render of this effect is still
      // picked up on the very next keydown.
      const session = useSessionStore.getState();
      const activeSession = session.sessions.find((s) => s.id === session.activeSessionId) ?? null;
      const resolveWorkspacePath = () =>
        activeSession?.workspacePath ?? primaryRoot(useWorkspaceStore.getState().roots)?.path ?? null;
      const openInEditor = (editor: "cursor" | "vscode") => {
        const path = resolveWorkspacePath();
        if (!path) return;
        void invoke("open_in_editor", { path, editor }).catch((err) => console.error(err));
      };

      const actions: Record<ShortcutIdForScope<"global">, () => void> = {
        newSession: () => {
          setRunCenterOpen(false);
          setBrowserWorkbenchOpen(false);
          setGlobalSearchOpen(false);
          setAgentInboxOpen(false);
          setSettingsOpen(false);
          setSettingsInitialTab(undefined);
          newSession();
        },
        openSettings: () => {
          setRunCenterOpen(false);
          setBrowserWorkbenchOpen(false);
          setGlobalSearchOpen(false);
          setAgentInboxOpen(false);
          setSettingsInitialTab(undefined);
          setSettingsOpen(true);
        },
        openShortcuts: () => openSettingsTab("shortcuts"),
        toggleWorkspacePanel: () => setWorkspacePanelOpen((open) => !open),
        sessionTogglePin: () => activeSession && session.togglePin(activeSession.id),
        sessionToggleUnread: () => activeSession && session.toggleUnread(activeSession.id),
        sessionRename: () => activeSession && session.requestRename(activeSession.id),
        sessionFork: () => activeSession && session.forkSession(activeSession.id),
        sessionArchive: () =>
          activeSession &&
          (activeSession.archived ? session.unarchiveSession : session.archiveSession)(activeSession.id),
        sessionOpenWindow: () => {
          if (!activeSession) return;
          void invoke("open_session_window", { sessionId: activeSession.id }).catch((err) => console.error(err));
        },
        sessionOpenCursor: () => openInEditor("cursor"),
        sessionOpenVsCode: () => openInEditor("vscode"),
        sessionRevealFinder: () => {
          const path = resolveWorkspacePath();
          if (!path) return;
          void invoke("reveal_in_finder", { path }).catch((err) => console.error(err));
        },
      };

      event.preventDefault();
      actions[shortcut]();
    }

    // Capture keeps native-style app commands available even inside inputs
    // that stop bubbling for their own local Enter/Escape handling.
    window.addEventListener("keydown", handleKeyDown, true);
    return () => window.removeEventListener("keydown", handleKeyDown, true);
  }, [newSession, openSettingsTab, permissionPending]);

  useEffect(() => {
    void refreshRoots();
    void refreshRecent();
    void refreshModels();
    void refreshOllama();
    void refreshProviders();
  }, [refreshRoots, refreshRecent, refreshModels, refreshOllama, refreshProviders]);

  useEffect(() => {
    void refreshRecipes();
    void subscribeToRecipeChanges();
  }, [refreshRecipes]);

  // The scheduler tick loop (design doc slice 3) must run in exactly one
  // place — every window shares the same `automations.json`/recipes, so
  // running it in a secondary session window too (see
  // `system::open_session_window`) would fire each due entry twice. Tauri
  // defaults the first window's label to "main" (see tauri.conf.json's
  // unlabeled window entry); secondary windows are labeled `session-<id>`.
  // `getCurrentWindow()` throws outside the Tauri shell (plain-browser dev
  // has no `window.__TAURI_INTERNALS__`) — guarded the same way every other
  // store's boot hydration already guards its own Tauri-only calls.
  useEffect(() => {
    void hydrateAutomations();
    if (isTauri() && getCurrentWindow().label === "main") {
      startScheduler();
      return startBackupScheduler();
    }
    return undefined;
  }, []);

  // A stop requested from Run Center (including another app window) is
  // recorded by Rust first, then bridged back to the exact active desktop
  // AbortController. The turn finalizer records Cancelling/Cancelled only
  // after the model/tool cancellation path has actually been triggered.
  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | null = null;
    void onRunCancellationRequested(({ runId }) => cancelRegisteredRun(runId)).then((cleanup) => {
      if (disposed) cleanup();
      else unlisten = cleanup;
    });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  // Rebind persisted chat placeholders to the daemon's shared run ledger
  // after a WebView/app restart. Only the main window owns recovery so two
  // session windows never race to render the same durable event stream.
  useEffect(() => {
    if (isTauri() && getCurrentWindow().label === "main") {
      recoverDaemonDesktopTurns();
    }
  }, []);

  // Local App Builder (ROADMAP.md, Phase 3): a published app's static page
  // triggers a run over HTTP, but only the desktop app's own frontend loop
  // can actually execute a recipe (`recipeRunner.ts`'s `runRecipeNow`) — see
  // `local_apps.rs`'s module doc. Main-window-only, same reasoning as the
  // scheduler and daemon-recovery effects above: every window shares the
  // same local API server, so running this in a secondary window too would
  // race to handle the same run request twice.
  useEffect(() => {
    void subscribeToLocalAppsChanges();
    if (isTauri() && getCurrentWindow().label === "main") {
      void subscribeToLocalAppRunRequests();
    }
  }, []);

  // Eager connect-on-startup for MCP servers: load the configured list, then
  // kick off `mcp_connect` for every enabled-but-not-yet-connected one so
  // their tools are ready before the user's first turn, without any Rust
  // setup-hook plumbing (mirrors the design doc's "frontend triggers
  // mcp_connect_all on mount" note). Failures surface per-server via the
  // McpPanel status pill (mcp://status -> "error"), not here.
  useEffect(() => {
    void (async () => {
      await refreshMcp();
      const toConnect = useMcpStore.getState().servers.filter((s) => s.enabled && s.status !== "connected");
      await Promise.all(toConnect.map((s) => connectMcp(s.id).catch(() => {})));
    })();
  }, [refreshMcp, connectMcp]);

  // The primary root changing (attach/detach of the whole workspace, not
  // secondary folders) invalidates whatever file was being previewed.
  useEffect(() => {
    setSelectedFile(null);
  }, [rootsVersion]);

  // Clicking "Preview" on a code fence should actually reveal the pane, even
  // if the user had collapsed it — otherwise `artifactStore.open()` would
  // silently do nothing visible.
  useEffect(() => {
    if (activeArtifact) setWorkspacePanelOpen(true);
  }, [activeArtifact]);

  const handleSelectFile = useCallback((path: string, content: string) => {
    setSelectedFile({ path, original: content, current: content });
  }, []);

  const handleRefreshDiff = useCallback(async () => {
    if (!selectedFile) return;
    setDiffLoading(true);
    setDiffError(null);
    try {
      const content = await invoke<string>("tool_read_file", { path: selectedFile.path });
      setSelectedFile((prev) => (prev ? { ...prev, current: content } : prev));
    } catch (err) {
      setDiffError(formatError(err));
    } finally {
      setDiffLoading(false);
    }
  }, [selectedFile]);

  return (
    <div className="flex h-screen w-screen bg-background text-foreground">
      {/* Left sidebar: chat session list, extending to the very top of the
          window (the title bar is overlaid — see tauri.conf.json). The top
          strip stays empty as a drag region and clears the macOS traffic
          lights. Workspace folder picking now lives in the WorkspaceBar
          above the chat input (see ChatWindow). */}
      <aside className="flex w-72 shrink-0 flex-col border-r border-border bg-surface">
        <div data-tauri-drag-region className="h-11 shrink-0" />
        <div className="min-h-0 flex-1 overflow-y-auto [overscroll-behavior:contain]">
          <ChatSessionList />
        </div>
        <AppMenu
          onOpenSettings={() => {
            setRunCenterOpen(false);
            setBrowserWorkbenchOpen(false);
            setIssueToPrOpen(false);
            setGlobalSearchOpen(false);
            setAgentInboxOpen(false);
            setSettingsOpen(true);
          }}
          onOpenRunCenter={() => {
            setSettingsOpen(false);
            setBrowserWorkbenchOpen(false);
            setIssueToPrOpen(false);
            setGlobalSearchOpen(false);
            setAgentInboxOpen(false);
            setSettingsInitialTab(undefined);
            setRunCenterOpen(true);
          }}
          onOpenGlobalSearch={() => {
            setSettingsOpen(false);
            setRunCenterOpen(false);
            setBrowserWorkbenchOpen(false);
            setIssueToPrOpen(false);
            setAgentInboxOpen(false);
            setSettingsInitialTab(undefined);
            setGlobalSearchOpen(true);
          }}
          onOpenBrowserWorkbench={() => {
            setSettingsOpen(false);
            setRunCenterOpen(false);
            setIssueToPrOpen(false);
            setGlobalSearchOpen(false);
            setAgentInboxOpen(false);
            setSettingsInitialTab(undefined);
            setBrowserWorkbenchOpen(true);
          }}
          onOpenIssueToPr={() => {
            setSettingsOpen(false);
            setRunCenterOpen(false);
            setBrowserWorkbenchOpen(false);
            setGlobalSearchOpen(false);
            setAgentInboxOpen(false);
            setSettingsInitialTab(undefined);
            setIssueToPrOpen(true);
          }}
          onOpenAgentInbox={() => {
            setSettingsOpen(false);
            setRunCenterOpen(false);
            setBrowserWorkbenchOpen(false);
            setGlobalSearchOpen(false);
            setIssueToPrOpen(false);
            setSettingsInitialTab(undefined);
            setAgentInboxOpen(true);
          }}
          onOpenTerminal={() => setTerminalOpen(true)}
          onOpenSideTasks={() => useSideTaskStore.getState().openDrawer()}
        />
      </aside>

      {/* Center: chat, with a drag-region strip standing in for the title bar */}
      <div className="flex min-h-0 min-w-0 flex-1 flex-col">
        <div data-tauri-drag-region className="flex h-11 shrink-0 items-center justify-end px-2">
          <IconButton
            size="sm"
            variant={terminalOpen ? "secondary" : "ghost"}
            onClick={() => setTerminalOpen((open) => !open)}
            disabled={!primaryRoot(useWorkspaceStore.getState().roots)}
            aria-label={terminalOpen ? t("App.closeTerminal") : t("App.openTerminal")}
            title={terminalOpen ? t("App.closeTerminal") : t("App.openTerminal")}
          >
            <SquareTerminal size={15} />
          </IconButton>
        </div>
        <SessionGrantBanner />
        {/* Per-pane boundary so one pane crashing doesn't take down the other
            (or the sidebar/workspace). `resetKey` clears a shown error on
            session switch — the replacement session gets a fresh render. */}
        <ErrorBoundary resetKey={globalSearchOpen ? "global-search" : agentInboxOpen ? "agent-inbox" : runCenterOpen ? "run-center" : issueToPrOpen ? "issue-to-pr" : browserWorkbenchOpen ? `browser-${activeSessionId}` : activeComparisonId ?? activeCrewSessionId ?? activeSessionId}>
          {globalSearchOpen ? (
            <GlobalSearch
              onClose={() => setGlobalSearchOpen(false)}
              onOpenRun={(runId) => {
                setGlobalSearchOpen(false);
                setRunCenterOpen(true);
                void useRunStore.getState().selectRun(runId);
              }}
            />
          ) : agentInboxOpen ? (
            <AgentInbox
              onClose={() => setAgentInboxOpen(false)}
              onOpenRunCenter={(runId) => {
                setAgentInboxOpen(false);
                setRunCenterOpen(true);
                void useRunStore.getState().selectRun(runId);
              }}
            />
          ) : runCenterOpen ? (
            <RunCenter onClose={() => setRunCenterOpen(false)} />
          ) : issueToPrOpen ? (
            <IssueToPrPanel
              onClose={() => setIssueToPrOpen(false)}
              onOpenRunCapsule={(runId) => {
                setIssueToPrOpen(false);
                setRunCenterOpen(true);
                void useRunStore.getState().selectRun(runId);
              }}
            />
          ) : browserWorkbenchOpen ? (
            <BrowserWorkbench
              key={activeSessionId}
              taskId={activeSessionId}
              chatSessionId={activeSessionId}
              onClose={() => setBrowserWorkbenchOpen(false)}
            />
          ) : activeComparisonId ? (
            <CompareView groupId={activeComparisonId} />
          ) : activeCrewSessionId ? (
            <CrewView sessionId={activeCrewSessionId} />
          ) : (
            <ChatWindow
              sessionId={activeSessionId}
              onManagePrompts={handleManagePrompts}
              onOpenSettingsTab={openSettingsTab}
            />
          )}
        </ErrorBoundary>
        {terminalOpen && (
          <TerminalPanel chatSessionId={activeSessionId} onClose={() => setTerminalOpen(false)} />
        )}
      </div>

      {/* Split pane: a second, fully independent chat opened via the session
          menu's "Open in > Split view" — Claude-Desktop-style, inside the
          same window. Its top strip doubles as the pane header: session
          title + close, still draggable like the other title-bar strips. */}
      {!globalSearchOpen && !agentInboxOpen && !runCenterOpen && !issueToPrOpen && !browserWorkbenchOpen && activeComparisonId === null && activeCrewSessionId === null && splitSessionId !== null && (
        <div className="flex min-h-0 min-w-0 flex-1 flex-col border-l border-border">
          <div data-tauri-drag-region className="flex h-11 shrink-0 items-center justify-between gap-2 border-b border-border px-3">
            <span className="pointer-events-none min-w-0 truncate text-sm font-medium text-foreground">
              {splitTitle}
            </span>
            <IconButton size="sm" onClick={closeSplit} aria-label={t("App.closeSplitPane")}>
              <X size={16} />
            </IconButton>
          </div>
          <ErrorBoundary resetKey={splitSessionId}>
            {splitCrewSessionId ? (
              <CrewView sessionId={splitCrewSessionId} />
            ) : (
              <ChatWindow
                sessionId={splitSessionId}
                onManagePrompts={handleManagePrompts}
                onOpenSettingsTab={openSettingsTab}
              />
            )}
          </ErrorBoundary>
        </div>
      )}

      {/* Side Tasks: a collapsible panel that coexists with whatever the
          main pane is showing (unlike RunCenter/BrowserWorkbench, which
          replace it) — ROADMAP.md's "Side Tasks" item asks for parallel work
          that stays visible next to the main chat, not a full-screen swap.
          Keyed by the active session so a manually-started ("+ New") task is
          attributed to whichever chat is actually on screen. */}
      <SideTaskDrawer sessionId={activeSessionId} />

      {/* Right: collapsible workspace panel (file tree + diff preview),
          also extending to the top of the window */}
      <aside
        className={`flex shrink-0 flex-col border-l border-border bg-surface transition-[width] duration-200 ${
          workspacePanelOpen ? "w-96" : "w-12"
        }`}
      >
        <div data-tauri-drag-region className="flex h-11 shrink-0 items-center justify-between border-b border-border px-3">
          {workspacePanelOpen && (
            <span className="text-[11px] font-semibold uppercase tracking-wider text-faint">
              {t("App.workspacePanelTitle")}
            </span>
          )}
          <IconButton
            size="sm"
            onClick={() => setWorkspacePanelOpen((prev) => !prev)}
            aria-label={workspacePanelOpen ? t("App.collapseWorkspacePanel") : t("App.expandWorkspacePanel")}
            className="ml-auto"
          >
            {workspacePanelOpen ? <PanelRightClose size={16} /> : <PanelRight size={16} />}
          </IconButton>
        </div>

        {workspacePanelOpen && (activeArtifact ? (
          <ArtifactPane />
        ) : (
          <div className="flex min-h-0 flex-1 flex-col">
            <div className="min-h-0 flex-[3] border-b border-border">
              <FileTree key={rootsVersion} onSelectFile={handleSelectFile} />
            </div>
            <div className="flex min-h-0 flex-[2] flex-col">
              <div className="flex h-9 shrink-0 items-center justify-between border-b border-border px-3">
                <span className="text-[11px] font-semibold uppercase tracking-wider text-faint">
                  {t("App.diffPanelTitle")}
                </span>
                <Button
                  variant="ghost"
                  size="sm"
                  onClick={() => void handleRefreshDiff()}
                  disabled={!selectedFile || diffLoading}
                >
                  {diffLoading ? t("App.diffRefreshing") : t("App.diffRefresh")}
                </Button>
              </div>
              {diffError && <p className="px-3 pt-1.5 text-xs text-danger">{diffError}</p>}
              <div className="min-h-0 flex-1 overflow-auto p-2 [overscroll-behavior:contain]">
                {selectedFile ? (
                  <DiffViewer
                    fileName={selectedFile.path}
                    oldValue={selectedFile.original}
                    newValue={selectedFile.current}
                    oldTitle={t("App.diffOldTitleOpened")}
                    newTitle={t("App.diffNewTitleCurrent")}
                  />
                ) : (
                  <p className="p-3 text-sm text-faint">
                    {t("App.diffEmptyStateHint")}
                  </p>
                )}
              </div>
            </div>
          </div>
        ))}
      </aside>

      <PermissionModal />
      <ApprovalChainModal />
      <SettingsModal
        open={settingsOpen}
        onClose={() => {
          setSettingsOpen(false);
          // Consume the deep-link tab on close so it only affects the
          // opening it was requested for — otherwise it would stick around
          // and force every later normal open (e.g. the plain gear icon)
          // back onto that tab instead of "whatever was last active".
          setSettingsInitialTab(undefined);
        }}
        initialTab={settingsInitialTab}
        initialTabRequest={settingsTabRequest}
      />
    </div>
  );
}

export default App;

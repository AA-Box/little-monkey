import { useCallback, useEffect, useState } from "react";
import { invoke, isTauri } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { FileDiff, FolderTree, Globe2, ListTodo, PanelRight, PanelRightClose, SquareTerminal, X } from "lucide-react";

import { ChatSessionList, ChatWindow, CompareView, CrewView, PrivacyFirewallGate } from "./components/Chat";
import { AppMenu } from "./components/AppMenu";
import { ErrorBoundary } from "./components/ErrorBoundary";
import { RunCenter } from "./components/Runs";
import { BrowserPane, BrowserWorkbench } from "./components/Browser";
import { useBrowserPaneStore } from "./store/browserPaneStore";
import { useApprovalChainStore } from "./store/approvalChainStore";
import { IssueToPrPanel } from "./components/IssueToPr";
import { SecurityAutofixPanel } from "./components/SecurityAutofix";
import { TrustScorecardsPanel } from "./components/TrustScorecards";
import { SopCompilerPanel } from "./components/SopCompiler";
import { McpGeneratorPanel } from "./components/McpGenerator";
import { ConnectorBuilderPanel } from "./components/ConnectorBuilder";
import { MigrationAgentPanel } from "./components/MigrationAgent";
import { SideTaskDrawer } from "./components/SideTasks";
import { GlobalSearch } from "./components/Search";
import { CommandPalette } from "./components/Palette";
import { AgentInbox } from "./components/Inbox";
import { RedTeamLabPanel } from "./components/RedTeamLab";
import { KnowledgeGraphExplorerPanel } from "./components/KnowledgeGraphExplorer";
import { SpreadsheetCopilotPanel } from "./components/SpreadsheetCopilot";
import { EvidenceBoardPanel } from "./components/EvidenceBoard";
import { GoldenDatasetBuilderPanel } from "./components/GoldenDatasetBuilder";
import { DailyBriefPanel } from "./components/DailyBrief";
import { DataNotebookPanel } from "./components/DataNotebook";
import { SyntheticMonitoringPanel } from "./components/SyntheticMonitoring";
import { CrossRepoIntelligencePanel } from "./components/CrossRepoIntelligence";
import { WorkCanvasPanel } from "./components/WorkCanvas";
import { PmCopilotPanel } from "./components/PmCopilot";
import { DeepResearchWorkspacePanel } from "./components/DeepResearchWorkspace";
import { BriefStudioPanel } from "./components/BriefStudio";
import { CrossRepoChangePlannerPanel } from "./components/CrossRepoChangePlanner";
import { VisualEditModePanel } from "./components/VisualEditMode";
import { TerminalPanel } from "./components/Terminal";
import { useTerminalStore } from "./store/terminalStore";
import { DebatePanel } from "./components/Debate";
import { DatabaseAdminGuardrailsPanel } from "./components/DatabaseAdminGuardrails";
import { ApiContractDiffLabPanel } from "./components/ApiContractDiffLab";
import { SettingsModal } from "./components/Settings";
import type { SettingsTab } from "./components/Settings";
import { OnboardingWizard } from "./components/Onboarding";
import { useRunStore } from "./store/runStore";
import { useSideTaskStore, selectRunningSideTaskCount } from "./store/sideTaskStore";
import { ArtifactPane, FileTree, DiffPanel, DiffViewer, PermissionModal, ApprovalChainModal, ReviewPanel, SessionGrantBanner } from "./components/Workspace";
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
import { useOnboardingStore } from "./store/onboardingStore";
import { startScheduler } from "./lib/scheduler";
import { startBackupScheduler } from "./lib/backupScheduler";
import { startSyntheticMonitoringScheduler } from "./store/syntheticMonitoringStore";
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
import { paletteClient } from "./lib/paletteClient";

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
  const switchSession = useSessionStore((s) => s.switchSession);
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
  const hasCompletedOnboarding = useOnboardingStore((s) => s.hasCompletedOnboarding);
  const restartOnboarding = useOnboardingStore((s) => s.restartOnboarding);

  const [workspacePanelOpen, setWorkspacePanelOpen] = useState(true);
  const [selectedFile, setSelectedFile] = useState<SelectedFile | null>(null);
  const [diffLoading, setDiffLoading] = useState(false);
  const [diffError, setDiffError] = useState<string | null>(null);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [runCenterOpen, setRunCenterOpen] = useState(false);
  const [browserWorkbenchOpen, setBrowserWorkbenchOpen] = useState(false);
  const [issueToPrOpen, setIssueToPrOpen] = useState(false);
  const [securityAutofixOpen, setSecurityAutofixOpen] = useState(false);
  const [trustScorecardsOpen, setTrustScorecardsOpen] = useState(false);
  const [sopCompilerOpen, setSopCompilerOpen] = useState(false);
  const [mcpGeneratorOpen, setMcpGeneratorOpen] = useState(false);
  const [connectorBuilderOpen, setConnectorBuilderOpen] = useState(false);
  const [migrationAgentOpen, setMigrationAgentOpen] = useState(false);
  const [globalSearchOpen, setGlobalSearchOpen] = useState(false);
  const [commandPaletteOpen, setCommandPaletteOpen] = useState(false);
  const [agentInboxOpen, setAgentInboxOpen] = useState(false);
  const [redTeamLabOpen, setRedTeamLabOpen] = useState(false);
  const [knowledgeGraphOpen, setKnowledgeGraphOpen] = useState(false);
  const [spreadsheetCopilotOpen, setSpreadsheetCopilotOpen] = useState(false);
  const [evidenceBoardOpen, setEvidenceBoardOpen] = useState(false);
  const [goldenDatasetBuilderOpen, setGoldenDatasetBuilderOpen] = useState(false);
  const [dailyBriefOpen, setDailyBriefOpen] = useState(false);
  const [dataNotebookOpen, setDataNotebookOpen] = useState(false);
  const [syntheticMonitoringOpen, setSyntheticMonitoringOpen] = useState(false);
  const [crossRepoIntelligenceOpen, setCrossRepoIntelligenceOpen] = useState(false);
  const [workCanvasOpen, setWorkCanvasOpen] = useState(false);
  const [pmCopilotOpen, setPmCopilotOpen] = useState(false);
  const [deepResearchOpen, setDeepResearchOpen] = useState(false);
  const [briefStudioOpen, setBriefStudioOpen] = useState(false);
  const [crossRepoPlannerOpen, setCrossRepoPlannerOpen] = useState(false);
  const [visualEditModeOpen, setVisualEditModeOpen] = useState(false);
  const [terminalOpen, setTerminalOpen] = useState(false);
  // Title-bar slot the primary ChatWindow portals its Compare/Crew pickers
  // into — callback-ref state (not a plain ref) so ChatWindow re-renders
  // once the element mounts and the portal can attach.
  const [chatHeaderActionsEl, setChatHeaderActionsEl] = useState<HTMLDivElement | null>(null);
  const terminalDock = useTerminalStore((state) => state.dock);
  const browserPaneOpen = useBrowserPaneStore((state) => state.open);
  const setBrowserPaneOpen = useBrowserPaneStore((state) => state.setOpen);
  const approvalChainPending = useApprovalChainStore((s) => s.pending !== null);
  const [diffPanelOpen, setDiffPanelOpen] = useState(false);
  /** Changed-file count behind the top-bar Diff badge; polled, best-effort. */
  const [changedFileCount, setChangedFileCount] = useState(0);
  const runningSideTaskCount = useSideTaskStore(selectRunningSideTaskCount);

  // Top-bar Diff badge: slow poll of the changed-file count, refreshed
  // immediately whenever the panel toggles. Badge only — the panel itself
  // fetches its own list.
  useEffect(() => {
    if (!isTauri()) return;
    let cancelled = false;
    const refresh = async () => {
      if (!primaryRoot(useWorkspaceStore.getState().roots)) return;
      try {
        const files = await invoke<unknown[]>("git_changed_files");
        if (!cancelled) setChangedFileCount(files.length);
      } catch {
        // Badge is cosmetic — ignore failures.
      }
    };
    void refresh();
    const timer = window.setInterval(() => void refresh(), 15_000);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [diffPanelOpen]);
  /** The right sidebar region shows at most ONE thing at a time (Claude-
   * Desktop-style): nothing (default), the picker menu, the workspace
   * panel, or the side-tasks drawer. A right-docked terminal overrides
   * whatever is selected here. */
  const [rightPanel, setRightPanel] = useState<"none" | "menu" | "workspace" | "sideTasks" | "review">("none");
  const [debateOpen, setDebateOpen] = useState(false);
  const [dbAdminGuardrailsOpen, setDbAdminGuardrailsOpen] = useState(false);
  const [apiContractDiffLabOpen, setApiContractDiffLabOpen] = useState(false);
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
    setSecurityAutofixOpen(false);
    setTrustScorecardsOpen(false);
    setSopCompilerOpen(false);
    setMcpGeneratorOpen(false);
    setConnectorBuilderOpen(false);
    setMigrationAgentOpen(false);
    setGlobalSearchOpen(false);
    setCommandPaletteOpen(false);
    setAgentInboxOpen(false);
    setRedTeamLabOpen(false);
    setKnowledgeGraphOpen(false);
    setSpreadsheetCopilotOpen(false);
    setEvidenceBoardOpen(false);
    setGoldenDatasetBuilderOpen(false);
    setDebateOpen(false);
    setDbAdminGuardrailsOpen(false);
    setDailyBriefOpen(false);
    setApiContractDiffLabOpen(false);
    setDataNotebookOpen(false);
    setSyntheticMonitoringOpen(false);
    setCrossRepoIntelligenceOpen(false);
    setWorkCanvasOpen(false);
    setPmCopilotOpen(false);
    setDeepResearchOpen(false);
    setBriefStudioOpen(false);
    setCrossRepoPlannerOpen(false);
    setVisualEditModeOpen(false);
    setSettingsInitialTab(tab);
    setSettingsTabRequest((request) => request + 1);
    setSettingsOpen(true);
  }, []);

  const handleManagePrompts = useCallback(() => {
    openSettingsTab("prompts");
  }, [openSettingsTab]);

  // Opens the Global Command Palette over whatever's currently shown —
  // triggered by the in-window shortcut below (Cmd/Ctrl+Shift+K, only while
  // focused) and by the OS-level global shortcut (works even unfocused; see
  // `src-tauri/src/command_palette.rs`, which shows/focuses this window and
  // emits `palette://open` for the listener further down).
  const openCommandPalette = useCallback(() => {
    setSettingsOpen(false);
    setRunCenterOpen(false);
    setBrowserWorkbenchOpen(false);
    setGlobalSearchOpen(false);
    setSettingsInitialTab(undefined);
    setCommandPaletteOpen(true);
  }, []);

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
          setIssueToPrOpen(false);
          setMigrationAgentOpen(false);
          setSecurityAutofixOpen(false);
          setGlobalSearchOpen(false);
          setAgentInboxOpen(false);
          setRedTeamLabOpen(false);
          setKnowledgeGraphOpen(false);
          setSpreadsheetCopilotOpen(false);
          setEvidenceBoardOpen(false);
          setGoldenDatasetBuilderOpen(false);
          setDebateOpen(false);
          setDbAdminGuardrailsOpen(false);
          setDailyBriefOpen(false);
          setApiContractDiffLabOpen(false);
          setDataNotebookOpen(false);
          setSyntheticMonitoringOpen(false);
          setCrossRepoIntelligenceOpen(false);
          setWorkCanvasOpen(false);
          setPmCopilotOpen(false);
          setDeepResearchOpen(false);
          setBriefStudioOpen(false);
          setCrossRepoPlannerOpen(false);
          setVisualEditModeOpen(false);
          setSettingsOpen(false);
          setSettingsInitialTab(undefined);
          newSession();
        },
        openSettings: () => {
          setRunCenterOpen(false);
          setBrowserWorkbenchOpen(false);
          setIssueToPrOpen(false);
          setMigrationAgentOpen(false);
          setSecurityAutofixOpen(false);
          setGlobalSearchOpen(false);
          setAgentInboxOpen(false);
          setRedTeamLabOpen(false);
          setKnowledgeGraphOpen(false);
          setSpreadsheetCopilotOpen(false);
          setEvidenceBoardOpen(false);
          setGoldenDatasetBuilderOpen(false);
          setDebateOpen(false);
          setDbAdminGuardrailsOpen(false);
          setDailyBriefOpen(false);
          setApiContractDiffLabOpen(false);
          setDataNotebookOpen(false);
          setSyntheticMonitoringOpen(false);
          setCrossRepoIntelligenceOpen(false);
          setWorkCanvasOpen(false);
          setPmCopilotOpen(false);
          setDeepResearchOpen(false);
          setBriefStudioOpen(false);
          setCrossRepoPlannerOpen(false);
          setVisualEditModeOpen(false);
          setSettingsInitialTab(undefined);
          setSettingsOpen(true);
        },
        openShortcuts: () => openSettingsTab("shortcuts"),
        toggleWorkspacePanel: () => setWorkspacePanelOpen((open) => !open),
        openCommandPalette: () => openCommandPalette(),
        toggleRightSidebar: () => setRightPanel((panel) => (panel === "none" ? "menu" : "none")),
        openTerminal: () => setTerminalOpen((open) => !open),
        openBrowser: () => {
          setDiffPanelOpen(false);
          const browserPane = useBrowserPaneStore.getState();
          browserPane.setOpen(!browserPane.open);
        },
        openReview: () => {
          useBrowserPaneStore.getState().setOpen(false);
          setDiffPanelOpen(false);
          setRightPanel((panel) => (panel === "review" ? "none" : "review"));
        },
        openFiles: () => {
          useBrowserPaneStore.getState().setOpen(false);
          setDiffPanelOpen(false);
          setRightPanel((panel) => (panel === "workspace" ? "none" : "workspace"));
        },
        openSideTasksPanel: () => {
          useBrowserPaneStore.getState().setOpen(false);
          setDiffPanelOpen(false);
          useSideTaskStore.getState().openDrawer();
          setRightPanel("sideTasks");
        },
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
  }, [newSession, openCommandPalette, openSettingsTab, permissionPending]);

  // The OS-level global shortcut (works even when Little Monkey isn't the
  // focused app) is registered in Rust and, on press, shows/focuses this
  // window and emits this event — see `command_palette::show_palette`.
  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | null = null;
    void paletteClient.onOpen(openCommandPalette).then((cleanup) => {
      if (disposed) cleanup();
      else unlisten = cleanup;
    }).catch(() => undefined);
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [openCommandPalette]);

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
      const stopBackupScheduler = startBackupScheduler();
      const stopSyntheticMonitoringScheduler = startSyntheticMonitoringScheduler();
      return () => {
        stopBackupScheduler();
        stopSyntheticMonitoringScheduler();
      };
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

  // Work Canvas "Open" action for a file-reference node: re-reads the file
  // fresh (same `tool_read_file` path `FileTree.tsx`'s own click handler
  // uses) and drops it into the same Workspace-panel preview/diff pane, so a
  // canvas node never carries a stale copy of the file — only ever a live
  // pointer back to it. Swallows a read failure into the diff pane's own
  // error slot rather than throwing, since the referenced file may have
  // since moved or been deleted.
  const handleOpenFileFromCanvas = useCallback(async (path: string) => {
    setWorkspacePanelOpen(true);
    setDiffError(null);
    try {
      const content = await invoke<string>("tool_read_file", { path });
      setSelectedFile({ path, original: content, current: content });
    } catch (err) {
      setDiffError(formatError(err));
    }
  }, []);

  // First-run onboarding (ROADMAP.md Phase 6): a Tauri-only, full-screen
  // wizard that replaces the entire shell below until it's finished or
  // explicitly skipped. Plain-browser dev (`vite` without the Tauri shell)
  // never shows it — same `isTauri()` guard every other Tauri-only boot
  // effect above already uses — so it can't block local frontend-only
  // development. Placed after every hook above so hook call order never
  // depends on this condition.
  if (isTauri() && !hasCompletedOnboarding) {
    return <OnboardingWizard />;
  }

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
            setSecurityAutofixOpen(false);
            setTrustScorecardsOpen(false);
            setSopCompilerOpen(false);
            setMcpGeneratorOpen(false);
            setConnectorBuilderOpen(false);
            setMigrationAgentOpen(false);
            setGlobalSearchOpen(false);
            setCommandPaletteOpen(false);
            setAgentInboxOpen(false);
            setRedTeamLabOpen(false);
            setKnowledgeGraphOpen(false);
            setSpreadsheetCopilotOpen(false);
            setEvidenceBoardOpen(false);
            setGoldenDatasetBuilderOpen(false);
            setDebateOpen(false);
            setDbAdminGuardrailsOpen(false);
            setDailyBriefOpen(false);
setApiContractDiffLabOpen(false);
setDataNotebookOpen(false);
setSyntheticMonitoringOpen(false);
setCrossRepoIntelligenceOpen(false);
setWorkCanvasOpen(false);
setPmCopilotOpen(false);
setDeepResearchOpen(false);
setBriefStudioOpen(false);
setCrossRepoPlannerOpen(false);
setVisualEditModeOpen(false);
            setSettingsOpen(true);
          }}
          onOpenRunCenter={() => {
            setSettingsOpen(false);
            setBrowserWorkbenchOpen(false);
            setIssueToPrOpen(false);
            setSecurityAutofixOpen(false);
            setTrustScorecardsOpen(false);
            setSopCompilerOpen(false);
            setMcpGeneratorOpen(false);
            setConnectorBuilderOpen(false);
            setMigrationAgentOpen(false);
            setGlobalSearchOpen(false);
            setCommandPaletteOpen(false);
            setAgentInboxOpen(false);
            setRedTeamLabOpen(false);
            setKnowledgeGraphOpen(false);
            setSpreadsheetCopilotOpen(false);
            setEvidenceBoardOpen(false);
            setGoldenDatasetBuilderOpen(false);
            setDebateOpen(false);
            setDbAdminGuardrailsOpen(false);
            setDailyBriefOpen(false);
setApiContractDiffLabOpen(false);
setDataNotebookOpen(false);
setSyntheticMonitoringOpen(false);
setCrossRepoIntelligenceOpen(false);
setWorkCanvasOpen(false);
setPmCopilotOpen(false);
setDeepResearchOpen(false);
setBriefStudioOpen(false);
setCrossRepoPlannerOpen(false);
setVisualEditModeOpen(false);
            setSettingsInitialTab(undefined);
            setRunCenterOpen(true);
          }}
          onOpenGlobalSearch={() => {
            setSettingsOpen(false);
            setRunCenterOpen(false);
            setBrowserWorkbenchOpen(false);
            setIssueToPrOpen(false);
            setSecurityAutofixOpen(false);
            setAgentInboxOpen(false);
            setCommandPaletteOpen(false);
            setIssueToPrOpen(false);
            setTrustScorecardsOpen(false);
            setSopCompilerOpen(false);
            setMcpGeneratorOpen(false);
            setConnectorBuilderOpen(false);
            setMigrationAgentOpen(false);
            setAgentInboxOpen(false);
            setRedTeamLabOpen(false);
            setKnowledgeGraphOpen(false);
            setSpreadsheetCopilotOpen(false);
            setEvidenceBoardOpen(false);
            setGoldenDatasetBuilderOpen(false);
            setDebateOpen(false);
            setDbAdminGuardrailsOpen(false);
            setDailyBriefOpen(false);
setApiContractDiffLabOpen(false);
setDataNotebookOpen(false);
setSyntheticMonitoringOpen(false);
setCrossRepoIntelligenceOpen(false);
setWorkCanvasOpen(false);
setPmCopilotOpen(false);
setDeepResearchOpen(false);
setBriefStudioOpen(false);
setCrossRepoPlannerOpen(false);
setVisualEditModeOpen(false);
            setSettingsInitialTab(undefined);
            setGlobalSearchOpen(true);
          }}
          onOpenBrowserWorkbench={() => {
            setSettingsOpen(false);
            setRunCenterOpen(false);
            setIssueToPrOpen(false);
            setSecurityAutofixOpen(false);
            setTrustScorecardsOpen(false);
            setSopCompilerOpen(false);
            setMcpGeneratorOpen(false);
            setConnectorBuilderOpen(false);
            setMigrationAgentOpen(false);
            setGlobalSearchOpen(false);
            setCommandPaletteOpen(false);
            setAgentInboxOpen(false);
            setRedTeamLabOpen(false);
            setKnowledgeGraphOpen(false);
            setSpreadsheetCopilotOpen(false);
            setEvidenceBoardOpen(false);
            setGoldenDatasetBuilderOpen(false);
            setDebateOpen(false);
            setDbAdminGuardrailsOpen(false);
            setDailyBriefOpen(false);
setApiContractDiffLabOpen(false);
setDataNotebookOpen(false);
setSyntheticMonitoringOpen(false);
setCrossRepoIntelligenceOpen(false);
setWorkCanvasOpen(false);
setPmCopilotOpen(false);
setDeepResearchOpen(false);
setBriefStudioOpen(false);
setCrossRepoPlannerOpen(false);
setVisualEditModeOpen(false);
            setSettingsInitialTab(undefined);
            setBrowserWorkbenchOpen(true);
          }}
          onOpenCommandPalette={openCommandPalette}
          onOpenIssueToPr={() => {
            setSettingsOpen(false);
            setRunCenterOpen(false);
            setBrowserWorkbenchOpen(false);
            setTrustScorecardsOpen(false);
            setSopCompilerOpen(false);
            setMcpGeneratorOpen(false);
            setConnectorBuilderOpen(false);
            setMigrationAgentOpen(false);
            setGlobalSearchOpen(false);
            setCommandPaletteOpen(false);
            setAgentInboxOpen(false);
            setRedTeamLabOpen(false);
            setKnowledgeGraphOpen(false);
            setSpreadsheetCopilotOpen(false);
            setEvidenceBoardOpen(false);
            setDebateOpen(false);
            setDbAdminGuardrailsOpen(false);
            setDailyBriefOpen(false);
            setApiContractDiffLabOpen(false);
            setDataNotebookOpen(false);
            setSyntheticMonitoringOpen(false);
            setCrossRepoIntelligenceOpen(false);
            setWorkCanvasOpen(false);
            setBriefStudioOpen(false);
            setVisualEditModeOpen(false);
            setSecurityAutofixOpen(false);
            setSettingsInitialTab(undefined);
            setIssueToPrOpen(true);
          }}
          onOpenSecurityAutofix={() => {
            setSettingsOpen(false);
            setRunCenterOpen(false);
            setBrowserWorkbenchOpen(false);
            setIssueToPrOpen(false);
            setGlobalSearchOpen(false);
            setAgentInboxOpen(false);
            setSettingsInitialTab(undefined);
            setSecurityAutofixOpen(true);
          }}
          onOpenTrustScorecards={() => {
            setSettingsOpen(false);
            setRunCenterOpen(false);
            setBrowserWorkbenchOpen(false);
            setIssueToPrOpen(false);
            setGlobalSearchOpen(false);
            setSopCompilerOpen(false);
            setMcpGeneratorOpen(false);
            setCommandPaletteOpen(false);
            setAgentInboxOpen(false);
            setRedTeamLabOpen(false);
            setKnowledgeGraphOpen(false);
            setEvidenceBoardOpen(false);
            setGoldenDatasetBuilderOpen(false);
            setDebateOpen(false);
            setDbAdminGuardrailsOpen(false);
            setDailyBriefOpen(false);
            setApiContractDiffLabOpen(false);
            setVisualEditModeOpen(false);
            setSecurityAutofixOpen(false);
            setSettingsInitialTab(undefined);
            setTrustScorecardsOpen(true);
          }}
          onOpenSopCompiler={() => {
            setSettingsOpen(false);
            setRunCenterOpen(false);
            setBrowserWorkbenchOpen(false);
            setIssueToPrOpen(false);
            setGlobalSearchOpen(false);
            setTrustScorecardsOpen(false);
            setMcpGeneratorOpen(false);
            setConnectorBuilderOpen(false);
            setMigrationAgentOpen(false);
            setCommandPaletteOpen(false);
            setAgentInboxOpen(false);
            setRedTeamLabOpen(false);
            setKnowledgeGraphOpen(false);
            setSpreadsheetCopilotOpen(false);
            setEvidenceBoardOpen(false);
            setGoldenDatasetBuilderOpen(false);
            setDebateOpen(false);
            setDbAdminGuardrailsOpen(false);
            setDailyBriefOpen(false);
            setApiContractDiffLabOpen(false);
            setDataNotebookOpen(false);
            setCrossRepoIntelligenceOpen(false);
            setVisualEditModeOpen(false);
            setSettingsInitialTab(undefined);
            setSopCompilerOpen(true);
          }}
          onOpenMcpGenerator={() => {
            setSettingsOpen(false);
            setRunCenterOpen(false);
            setBrowserWorkbenchOpen(false);
            setIssueToPrOpen(false);
            setGlobalSearchOpen(false);
            setSopCompilerOpen(false);
            setConnectorBuilderOpen(false);
            setMigrationAgentOpen(false);
            setCommandPaletteOpen(false);
            setAgentInboxOpen(false);
            setRedTeamLabOpen(false);
            setKnowledgeGraphOpen(false);
            setEvidenceBoardOpen(false);
            setGoldenDatasetBuilderOpen(false);
            setDebateOpen(false);
            setDailyBriefOpen(false);
            setDataNotebookOpen(false);
            setCrossRepoIntelligenceOpen(false);
            setVisualEditModeOpen(false);
            setSettingsInitialTab(undefined);
            setMcpGeneratorOpen(true);
          }}
          onOpenMigrationAgent={() => {
            setSettingsOpen(false);
            setRunCenterOpen(false);
            setBrowserWorkbenchOpen(false);
            setIssueToPrOpen(false);
            setGlobalSearchOpen(false);
            setSopCompilerOpen(false);
            setMcpGeneratorOpen(false);
            setAgentInboxOpen(false);
            setKnowledgeGraphOpen(false);
            setSpreadsheetCopilotOpen(false);
            setEvidenceBoardOpen(false);
            setDebateOpen(false);
            setDailyBriefOpen(false);
            setSettingsInitialTab(undefined);
            setMigrationAgentOpen(true);
          }}
          onOpenConnectorBuilder={() => {
            setSettingsOpen(false);
            setRunCenterOpen(false);
            setBrowserWorkbenchOpen(false);
            setIssueToPrOpen(false);
            setGlobalSearchOpen(false);
            setSopCompilerOpen(false);
            setMcpGeneratorOpen(false);
            setAgentInboxOpen(false);
            setKnowledgeGraphOpen(false);
            setEvidenceBoardOpen(false);
            setDebateOpen(false);
            setDailyBriefOpen(false);
            setApiContractDiffLabOpen(false);
            setSettingsInitialTab(undefined);
            setConnectorBuilderOpen(true);
          }}
          onOpenApiContractDiffLab={() => {
            setSettingsOpen(false);
            setRunCenterOpen(false);
            setBrowserWorkbenchOpen(false);
            setIssueToPrOpen(false);
            setTrustScorecardsOpen(false);
            setSopCompilerOpen(false);
            setMcpGeneratorOpen(false);
            setGlobalSearchOpen(false);
            setAgentInboxOpen(false);
            setRedTeamLabOpen(false);
            setKnowledgeGraphOpen(false);
            setEvidenceBoardOpen(false);
            setDebateOpen(false);
            setDbAdminGuardrailsOpen(false);
            setDailyBriefOpen(false);
            setSettingsInitialTab(undefined);
            setApiContractDiffLabOpen(true);
          }}
          onOpenAgentInbox={() => {
            setSettingsOpen(false);
            setRunCenterOpen(false);
            setBrowserWorkbenchOpen(false);
            setIssueToPrOpen(false);
            setTrustScorecardsOpen(false);
            setSopCompilerOpen(false);
            setMcpGeneratorOpen(false);
            setMigrationAgentOpen(false);
            setGlobalSearchOpen(false);
            setCommandPaletteOpen(false);
            setRedTeamLabOpen(false);
            setKnowledgeGraphOpen(false);
            setEvidenceBoardOpen(false);
            setDebateOpen(false);
            setDbAdminGuardrailsOpen(false);
            setDailyBriefOpen(false);
            setApiContractDiffLabOpen(false);
            setDataNotebookOpen(false);
            setSyntheticMonitoringOpen(false);
            setCrossRepoIntelligenceOpen(false);
            setWorkCanvasOpen(false);
            setPmCopilotOpen(false);
            setDeepResearchOpen(false);
            setBriefStudioOpen(false);
            setCrossRepoPlannerOpen(false);
            setVisualEditModeOpen(false);
            setSecurityAutofixOpen(false);
            setSettingsInitialTab(undefined);
            setAgentInboxOpen(true);
          }}
          onOpenRedTeamLab={() => {
            setSettingsOpen(false);
            setRunCenterOpen(false);
            setBrowserWorkbenchOpen(false);
            setIssueToPrOpen(false);
            setTrustScorecardsOpen(false);
            setSopCompilerOpen(false);
            setMcpGeneratorOpen(false);
            setConnectorBuilderOpen(false);
            setGlobalSearchOpen(false);
            setCommandPaletteOpen(false);
            setAgentInboxOpen(false);
            setKnowledgeGraphOpen(false);
            setSpreadsheetCopilotOpen(false);
            setEvidenceBoardOpen(false);
            setGoldenDatasetBuilderOpen(false);
            setDebateOpen(false);
            setDbAdminGuardrailsOpen(false);
            setDailyBriefOpen(false);
            setApiContractDiffLabOpen(false);
            setVisualEditModeOpen(false);
            setSettingsInitialTab(undefined);
            setRedTeamLabOpen(true);
          }}
          onOpenKnowledgeGraph={() => {
            setSettingsOpen(false);
            setRunCenterOpen(false);
            setBrowserWorkbenchOpen(false);
            setIssueToPrOpen(false);
            setSopCompilerOpen(false);
            setMcpGeneratorOpen(false);
            setConnectorBuilderOpen(false);
            setGlobalSearchOpen(false);
            setCommandPaletteOpen(false);
            setAgentInboxOpen(false);
            setSpreadsheetCopilotOpen(false);
            setRedTeamLabOpen(false);
            setEvidenceBoardOpen(false);
            setGoldenDatasetBuilderOpen(false);
            setDebateOpen(false);
            setDailyBriefOpen(false);
            setDataNotebookOpen(false);
            setVisualEditModeOpen(false);
            setSettingsInitialTab(undefined);
            setKnowledgeGraphOpen(true);
          }}
          onOpenSpreadsheetCopilot={() => {
            setSettingsOpen(false);
            setRunCenterOpen(false);
            setBrowserWorkbenchOpen(false);
            setIssueToPrOpen(false);
            setSopCompilerOpen(false);
            setMcpGeneratorOpen(false);
            setGlobalSearchOpen(false);
            setAgentInboxOpen(false);
            setKnowledgeGraphOpen(false);
            setGoldenDatasetBuilderOpen(false);
            setEvidenceBoardOpen(false);
            setDebateOpen(false);
            setDbAdminGuardrailsOpen(false);
            setDailyBriefOpen(false);
            setApiContractDiffLabOpen(false);
            setSettingsInitialTab(undefined);
            setSpreadsheetCopilotOpen(true);
          }}
          onOpenEvidenceBoard={() => {
            setSettingsOpen(false);
            setRunCenterOpen(false);
            setBrowserWorkbenchOpen(false);
            setIssueToPrOpen(false);
            setSopCompilerOpen(false);
            setMcpGeneratorOpen(false);
            setConnectorBuilderOpen(false);
            setGlobalSearchOpen(false);
            setCommandPaletteOpen(false);
            setAgentInboxOpen(false);
            setRedTeamLabOpen(false);
            setKnowledgeGraphOpen(false);
            setSpreadsheetCopilotOpen(false);
            setDebateOpen(false);
            setDbAdminGuardrailsOpen(false);
            setDailyBriefOpen(false);
            setDataNotebookOpen(false);
            setVisualEditModeOpen(false);
            setSettingsInitialTab(undefined);
            setEvidenceBoardOpen(true);
          }}
          onOpenGoldenDatasetBuilder={() => {
            setSettingsOpen(false);
            setRunCenterOpen(false);
            setBrowserWorkbenchOpen(false);
            setIssueToPrOpen(false);
            setSopCompilerOpen(false);
            setMcpGeneratorOpen(false);
            setGlobalSearchOpen(false);
            setAgentInboxOpen(false);
            setKnowledgeGraphOpen(false);
            setEvidenceBoardOpen(false);
            setDebateOpen(false);
            setDailyBriefOpen(false);
            setApiContractDiffLabOpen(false);
            setSettingsInitialTab(undefined);
            setGoldenDatasetBuilderOpen(true);
          }}
          onOpenDailyBrief={() => {
            setSettingsOpen(false);
            setRunCenterOpen(false);
            setBrowserWorkbenchOpen(false);
            setIssueToPrOpen(false);
            setTrustScorecardsOpen(false);
            setSopCompilerOpen(false);
            setMcpGeneratorOpen(false);
            setConnectorBuilderOpen(false);
            setMigrationAgentOpen(false);
            setGlobalSearchOpen(false);
            setCommandPaletteOpen(false);
            setAgentInboxOpen(false);
            setSyntheticMonitoringOpen(false);
            setWorkCanvasOpen(false);
            setPmCopilotOpen(false);
            setDeepResearchOpen(false);
            setBriefStudioOpen(false);
            setCrossRepoPlannerOpen(false);
            setVisualEditModeOpen(false);
            setRedTeamLabOpen(false);
            setKnowledgeGraphOpen(false);
            setSpreadsheetCopilotOpen(false);
            setEvidenceBoardOpen(false);
            setDebateOpen(false);
            setDataNotebookOpen(false);
            setCrossRepoIntelligenceOpen(false);
            setSettingsInitialTab(undefined);
            setDailyBriefOpen(true);
          }}
          onOpenDataNotebook={() => {
            setSettingsOpen(false);
            setRunCenterOpen(false);
            setBrowserWorkbenchOpen(false);
            setIssueToPrOpen(false);
            setTrustScorecardsOpen(false);
            setSopCompilerOpen(false);
            setMcpGeneratorOpen(false);
            setGlobalSearchOpen(false);
            setCommandPaletteOpen(false);
            setAgentInboxOpen(false);
            setRedTeamLabOpen(false);
            setKnowledgeGraphOpen(false);
            setEvidenceBoardOpen(false);
            setGoldenDatasetBuilderOpen(false);
            setDebateOpen(false);
            setDailyBriefOpen(false);
            setSyntheticMonitoringOpen(false);
            setWorkCanvasOpen(false);
            setPmCopilotOpen(false);
            setDeepResearchOpen(false);
            setBriefStudioOpen(false);
            setCrossRepoPlannerOpen(false);
            setVisualEditModeOpen(false);
            setCrossRepoIntelligenceOpen(false);
            setSettingsInitialTab(undefined);
            setDataNotebookOpen(true);
          }}
          onOpenSyntheticMonitoring={() => {
            setSettingsOpen(false);
            setRunCenterOpen(false);
            setBrowserWorkbenchOpen(false);
            setIssueToPrOpen(false);
            setTrustScorecardsOpen(false);
            setSopCompilerOpen(false);
            setMcpGeneratorOpen(false);
            setGlobalSearchOpen(false);
            setCommandPaletteOpen(false);
            setAgentInboxOpen(false);
            setRedTeamLabOpen(false);
            setKnowledgeGraphOpen(false);
            setEvidenceBoardOpen(false);
            setDebateOpen(false);
            setDbAdminGuardrailsOpen(false);
            setApiContractDiffLabOpen(false);
            setDailyBriefOpen(false);
            setDataNotebookOpen(false);
            setWorkCanvasOpen(false);
            setPmCopilotOpen(false);
            setDeepResearchOpen(false);
            setBriefStudioOpen(false);
            setCrossRepoPlannerOpen(false);
            setVisualEditModeOpen(false);
            setCrossRepoIntelligenceOpen(false);
            setSettingsInitialTab(undefined);
            setSyntheticMonitoringOpen(true);
          }}
          onOpenCrossRepoIntelligence={() => {
            setSettingsOpen(false);
            setRunCenterOpen(false);
            setBrowserWorkbenchOpen(false);
            setIssueToPrOpen(false);
            setTrustScorecardsOpen(false);
            setSopCompilerOpen(false);
            setMcpGeneratorOpen(false);
            setGlobalSearchOpen(false);
            setCommandPaletteOpen(false);
            setAgentInboxOpen(false);
            setRedTeamLabOpen(false);
            setKnowledgeGraphOpen(false);
            setEvidenceBoardOpen(false);
            setDebateOpen(false);
            setDailyBriefOpen(false);
            setPmCopilotOpen(false);
            setDeepResearchOpen(false);
            setBriefStudioOpen(false);
            setCrossRepoPlannerOpen(false);
            setVisualEditModeOpen(false);
            setWorkCanvasOpen(false);
            setSyntheticMonitoringOpen(false);
            setDataNotebookOpen(false);
            setSettingsInitialTab(undefined);
            setCrossRepoIntelligenceOpen(true);
          }}
          onOpenWorkCanvas={() => {
            setSettingsOpen(false);
            setRunCenterOpen(false);
            setBrowserWorkbenchOpen(false);
            setIssueToPrOpen(false);
            setTrustScorecardsOpen(false);
            setSopCompilerOpen(false);
            setMcpGeneratorOpen(false);
            setGlobalSearchOpen(false);
            setCommandPaletteOpen(false);
            setAgentInboxOpen(false);
            setRedTeamLabOpen(false);
            setKnowledgeGraphOpen(false);
            setEvidenceBoardOpen(false);
            setDebateOpen(false);
            setDailyBriefOpen(false);
            setPmCopilotOpen(false);
            setDeepResearchOpen(false);
            setBriefStudioOpen(false);
            setCrossRepoPlannerOpen(false);
            setVisualEditModeOpen(false);
            setCrossRepoIntelligenceOpen(false);
            setSyntheticMonitoringOpen(false);
            setDataNotebookOpen(false);
            setSettingsInitialTab(undefined);
            setWorkCanvasOpen(true);
          }}
          onOpenPmCopilot={() => {
            setSettingsOpen(false);
            setRunCenterOpen(false);
            setBrowserWorkbenchOpen(false);
            setIssueToPrOpen(false);
            setTrustScorecardsOpen(false);
            setSopCompilerOpen(false);
            setMcpGeneratorOpen(false);
            setGlobalSearchOpen(false);
            setCommandPaletteOpen(false);
            setAgentInboxOpen(false);
            setRedTeamLabOpen(false);
            setKnowledgeGraphOpen(false);
            setEvidenceBoardOpen(false);
            setDebateOpen(false);
            setDailyBriefOpen(false);
            setWorkCanvasOpen(false);
            setDeepResearchOpen(false);
            setBriefStudioOpen(false);
            setCrossRepoPlannerOpen(false);
            setVisualEditModeOpen(false);
            setCrossRepoIntelligenceOpen(false);
            setSyntheticMonitoringOpen(false);
            setDataNotebookOpen(false);
            setSettingsInitialTab(undefined);
            setPmCopilotOpen(true);
          }}
          onOpenDeepResearch={() => {
            setSettingsOpen(false);
            setRunCenterOpen(false);
            setBrowserWorkbenchOpen(false);
            setIssueToPrOpen(false);
            setTrustScorecardsOpen(false);
            setSopCompilerOpen(false);
            setMcpGeneratorOpen(false);
            setGlobalSearchOpen(false);
            setCommandPaletteOpen(false);
            setAgentInboxOpen(false);
            setRedTeamLabOpen(false);
            setKnowledgeGraphOpen(false);
            setEvidenceBoardOpen(false);
            setDebateOpen(false);
            setDailyBriefOpen(false);
            setBriefStudioOpen(false);
            setCrossRepoPlannerOpen(false);
            setVisualEditModeOpen(false);
            setPmCopilotOpen(false);
            setWorkCanvasOpen(false);
            setCrossRepoIntelligenceOpen(false);
            setSyntheticMonitoringOpen(false);
            setDataNotebookOpen(false);
            setSettingsInitialTab(undefined);
            setDeepResearchOpen(true);
          }}
          onOpenBriefStudio={() => {
            setSettingsOpen(false);
            setRunCenterOpen(false);
            setBrowserWorkbenchOpen(false);
            setIssueToPrOpen(false);
            setTrustScorecardsOpen(false);
            setSopCompilerOpen(false);
            setMcpGeneratorOpen(false);
            setGlobalSearchOpen(false);
            setCommandPaletteOpen(false);
            setAgentInboxOpen(false);
            setRedTeamLabOpen(false);
            setKnowledgeGraphOpen(false);
            setEvidenceBoardOpen(false);
            setDebateOpen(false);
            setDailyBriefOpen(false);
            setCrossRepoPlannerOpen(false);
            setVisualEditModeOpen(false);
            setDeepResearchOpen(false);
            setPmCopilotOpen(false);
            setWorkCanvasOpen(false);
            setCrossRepoIntelligenceOpen(false);
            setSyntheticMonitoringOpen(false);
            setDataNotebookOpen(false);
            setSettingsInitialTab(undefined);
            setBriefStudioOpen(true);
          }}
          onOpenCrossRepoChangePlanner={() => {
            setSettingsOpen(false);
            setRunCenterOpen(false);
            setBrowserWorkbenchOpen(false);
            setIssueToPrOpen(false);
            setTrustScorecardsOpen(false);
            setSopCompilerOpen(false);
            setMcpGeneratorOpen(false);
            setGlobalSearchOpen(false);
            setCommandPaletteOpen(false);
            setAgentInboxOpen(false);
            setRedTeamLabOpen(false);
            setKnowledgeGraphOpen(false);
            setEvidenceBoardOpen(false);
            setDebateOpen(false);
            setDailyBriefOpen(false);
            setVisualEditModeOpen(false);
            setBriefStudioOpen(false);
            setDeepResearchOpen(false);
            setPmCopilotOpen(false);
            setWorkCanvasOpen(false);
            setCrossRepoIntelligenceOpen(false);
            setSyntheticMonitoringOpen(false);
            setDataNotebookOpen(false);
            setSettingsInitialTab(undefined);
            setCrossRepoPlannerOpen(true);
          }}
          onOpenVisualEditMode={() => {
            setSettingsOpen(false);
            setRunCenterOpen(false);
            setBrowserWorkbenchOpen(false);
            setIssueToPrOpen(false);
            setTrustScorecardsOpen(false);
            setSopCompilerOpen(false);
            setMcpGeneratorOpen(false);
            setGlobalSearchOpen(false);
            setCommandPaletteOpen(false);
            setAgentInboxOpen(false);
            setRedTeamLabOpen(false);
            setKnowledgeGraphOpen(false);
            setEvidenceBoardOpen(false);
            setDebateOpen(false);
            setDailyBriefOpen(false);
            setCrossRepoPlannerOpen(false);
            setBriefStudioOpen(false);
            setDeepResearchOpen(false);
            setPmCopilotOpen(false);
            setWorkCanvasOpen(false);
            setCrossRepoIntelligenceOpen(false);
            setSyntheticMonitoringOpen(false);
            setDataNotebookOpen(false);
            setSettingsInitialTab(undefined);
            setVisualEditModeOpen(true);
          }}
          onOpenTerminal={() => setTerminalOpen(true)}
          onOpenSideTasks={() => {
            useSideTaskStore.getState().openDrawer();
            setRightPanel("sideTasks");
          }}
          onOpenDebate={() => {
            setSettingsOpen(false);
            setRunCenterOpen(false);
            setBrowserWorkbenchOpen(false);
            setGlobalSearchOpen(false);
            setCommandPaletteOpen(false);
            setAgentInboxOpen(false);
            setIssueToPrOpen(false);
            setTrustScorecardsOpen(false);
            setSopCompilerOpen(false);
            setMcpGeneratorOpen(false);
            setConnectorBuilderOpen(false);
            setRedTeamLabOpen(false);
            setKnowledgeGraphOpen(false);
            setSpreadsheetCopilotOpen(false);
            setEvidenceBoardOpen(false);
            setGoldenDatasetBuilderOpen(false);
            setDailyBriefOpen(false);
            setDbAdminGuardrailsOpen(false);
            setApiContractDiffLabOpen(false);
            setDataNotebookOpen(false);
            setSyntheticMonitoringOpen(false);
            setWorkCanvasOpen(false);
            setPmCopilotOpen(false);
            setDeepResearchOpen(false);
            setBriefStudioOpen(false);
            setCrossRepoPlannerOpen(false);
            setCrossRepoIntelligenceOpen(false);
            setVisualEditModeOpen(false);
            setSecurityAutofixOpen(false);
            setSettingsInitialTab(undefined);
            setDebateOpen(true);
          }}
          onOpenDbAdminGuardrails={() => {
            setSettingsOpen(false);
            setRunCenterOpen(false);
            setBrowserWorkbenchOpen(false);
            setGlobalSearchOpen(false);
            setAgentInboxOpen(false);
            setIssueToPrOpen(false);
            setTrustScorecardsOpen(false);
            setSopCompilerOpen(false);
            setMcpGeneratorOpen(false);
            setRedTeamLabOpen(false);
            setKnowledgeGraphOpen(false);
            setEvidenceBoardOpen(false);
            setDailyBriefOpen(false);
            setDebateOpen(false);
            setSettingsInitialTab(undefined);
            setDbAdminGuardrailsOpen(true);
          }}
          onRestartOnboarding={() => {
            setSettingsOpen(false);
            setRunCenterOpen(false);
            setBrowserWorkbenchOpen(false);
            setIssueToPrOpen(false);
            setMigrationAgentOpen(false);
            setGlobalSearchOpen(false);
            setAgentInboxOpen(false);
            setSyntheticMonitoringOpen(false);
            setDebateOpen(false);
            setDbAdminGuardrailsOpen(false);
            setDataNotebookOpen(false);
            setTerminalOpen(false);
            restartOnboarding();
          }}
        />
      </aside>

      {/* Center: chat, with a drag-region strip standing in for the title bar */}
      <div className="flex min-h-0 min-w-0 flex-1 flex-col">
        <div data-tauri-drag-region className="flex h-11 shrink-0 items-center justify-end px-2">
          {/* Portal target for ChatWindow's Compare/Crew pickers — hugs the
              strip's left edge (right of the session sidebar); empty when
              another view is shown. */}
          <div ref={setChatHeaderActionsEl} className="mr-auto flex items-center gap-1.5" />
          <IconButton
            size="sm"
            variant={diffPanelOpen ? "active" : "ghost"}
            className="relative"
            onClick={() => {
              setBrowserPaneOpen(false);
              setDiffPanelOpen((open) => !open);
            }}
            disabled={!primaryRoot(useWorkspaceStore.getState().roots)}
            aria-label={diffPanelOpen ? t("App.closeDiff") : t("App.openDiff")}
            title={diffPanelOpen ? t("App.closeDiff") : t("App.openDiff")}
          >
            <FileDiff size={15} />
            {changedFileCount > 0 && (
              <span className="absolute -right-0.5 -top-0.5 flex h-3.5 min-w-3.5 items-center justify-center rounded-full bg-accent px-0.5 text-[9px] font-semibold leading-none text-accent-foreground">
                {changedFileCount > 9 ? "9+" : changedFileCount}
              </span>
            )}
          </IconButton>
          <IconButton
            size="sm"
            variant={terminalOpen ? "active" : "ghost"}
            onClick={() => setTerminalOpen((open) => !open)}
            disabled={!primaryRoot(useWorkspaceStore.getState().roots)}
            aria-label={terminalOpen ? t("App.closeTerminal") : t("App.openTerminal")}
            title={terminalOpen ? t("App.closeTerminal") : t("App.openTerminal")}
          >
            <SquareTerminal size={15} />
          </IconButton>
          <IconButton
            size="sm"
            variant={browserPaneOpen ? "active" : "ghost"}
            onClick={() => {
              setDiffPanelOpen(false);
              setBrowserPaneOpen(!browserPaneOpen);
            }}
            aria-label={browserPaneOpen ? t("App.closeBrowser") : t("App.openBrowser")}
            title={browserPaneOpen ? t("App.closeBrowser") : t("App.openBrowser")}
          >
            <Globe2 size={15} />
          </IconButton>
          <IconButton
            size="sm"
            variant={rightPanel === "sideTasks" ? "active" : "ghost"}
            className="relative"
            onClick={() => {
              if (rightPanel === "sideTasks") {
                setRightPanel("none");
              } else {
                setBrowserPaneOpen(false);
                setDiffPanelOpen(false);
                useSideTaskStore.getState().openDrawer();
                setRightPanel("sideTasks");
              }
            }}
            aria-label={rightPanel === "sideTasks" ? t("App.closeTasks") : t("App.openTasks")}
            title={rightPanel === "sideTasks" ? t("App.closeTasks") : t("App.openTasks")}
          >
            <ListTodo size={15} />
            {runningSideTaskCount > 0 && (
              <span className="absolute -right-0.5 -top-0.5 flex h-3.5 min-w-3.5 items-center justify-center rounded-full bg-accent px-0.5 text-[9px] font-semibold leading-none text-accent-foreground">
                {runningSideTaskCount > 9 ? "9+" : runningSideTaskCount}
              </span>
            )}
          </IconButton>
          <IconButton
            size="sm"
            variant={rightPanel !== "none" ? "active" : "ghost"}
            onClick={() => setRightPanel((panel) => (panel === "none" ? "menu" : "none"))}
            aria-label={rightPanel !== "none" ? t("App.closeRightSidebar") : t("App.openRightSidebar")}
            title={rightPanel !== "none" ? t("App.closeRightSidebar") : t("App.openRightSidebar")}
          >
            <PanelRight size={15} />
          </IconButton>
        </div>
        <SessionGrantBanner />
        {/* Per-pane boundary so one pane crashing doesn't take down the other
            (or the sidebar/workspace). `resetKey` clears a shown error on
            session switch — the replacement session gets a fresh render. */}
        <ErrorBoundary resetKey={globalSearchOpen ? "global-search" : agentInboxOpen ? "agent-inbox" : redTeamLabOpen ? "red-team-lab" : knowledgeGraphOpen ? "knowledge-graph" : spreadsheetCopilotOpen ? "spreadsheet-copilot" : evidenceBoardOpen ? "evidence-board" : goldenDatasetBuilderOpen ? "golden-dataset-builder" : dailyBriefOpen ? "daily-brief" : dataNotebookOpen ? "data-notebook" : syntheticMonitoringOpen ? "synthetic-monitoring" : workCanvasOpen ? "work-canvas" : pmCopilotOpen ? "pm-copilot" : deepResearchOpen ? "deep-research" : briefStudioOpen ? "brief-studio" : crossRepoPlannerOpen ? "cross-repo-planner" : crossRepoIntelligenceOpen ? "cross-repo-intelligence" : visualEditModeOpen ? "visual-edit-mode" : runCenterOpen ? "run-center" : debateOpen ? "debate" : dbAdminGuardrailsOpen ? "db-admin-guardrails" : issueToPrOpen ? "issue-to-pr" : securityAutofixOpen ? "security-autofix" : trustScorecardsOpen ? "trust-scorecards" : sopCompilerOpen ? "sop-compiler" : mcpGeneratorOpen ? "mcp-generator" : connectorBuilderOpen ? "connector-builder" : migrationAgentOpen ? "migration-agent" : apiContractDiffLabOpen ? "api-contract-diff-lab" : browserWorkbenchOpen ? `browser-${activeSessionId}` : activeComparisonId ?? activeCrewSessionId ?? activeSessionId}>
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
          ) : redTeamLabOpen ? (
            <RedTeamLabPanel onClose={() => setRedTeamLabOpen(false)} />
          ) : knowledgeGraphOpen ? (
            <KnowledgeGraphExplorerPanel onClose={() => setKnowledgeGraphOpen(false)} />
          ) : spreadsheetCopilotOpen ? (
            <SpreadsheetCopilotPanel onClose={() => setSpreadsheetCopilotOpen(false)} />
          ) : evidenceBoardOpen ? (
            <EvidenceBoardPanel sessionId={activeSessionId} onClose={() => setEvidenceBoardOpen(false)} />
          ) : goldenDatasetBuilderOpen ? (
            <GoldenDatasetBuilderPanel onClose={() => setGoldenDatasetBuilderOpen(false)} />
          ) : dailyBriefOpen ? (
            <DailyBriefPanel
              onClose={() => setDailyBriefOpen(false)}
              onOpenRunCenter={(runId) => {
                setDailyBriefOpen(false);
                setApiContractDiffLabOpen(false);
                setRunCenterOpen(true);
                void useRunStore.getState().selectRun(runId);
              }}
              onOpenAgentInbox={() => {
                setDailyBriefOpen(false);
                setApiContractDiffLabOpen(false);
                setAgentInboxOpen(true);
              }}
              onOpenSettingsTab={openSettingsTab}
            />
          ) : dataNotebookOpen ? (
            <DataNotebookPanel onClose={() => setDataNotebookOpen(false)} />
          ) : syntheticMonitoringOpen ? (
            <SyntheticMonitoringPanel onClose={() => setSyntheticMonitoringOpen(false)} />
          ) : workCanvasOpen ? (
            <WorkCanvasPanel
              onClose={() => setWorkCanvasOpen(false)}
              onOpenSession={(sessionId) => {
                setWorkCanvasOpen(false);
                switchSession(sessionId);
              }}
              onOpenRun={(runId) => {
                setWorkCanvasOpen(false);
                setRunCenterOpen(true);
                void useRunStore.getState().selectRun(runId);
              }}
              onOpenFile={(path) => void handleOpenFileFromCanvas(path)}
            />
          ) : pmCopilotOpen ? (
            <PmCopilotPanel onClose={() => setPmCopilotOpen(false)} />
          ) : deepResearchOpen ? (
            <DeepResearchWorkspacePanel onClose={() => setDeepResearchOpen(false)} />
          ) : briefStudioOpen ? (
            <BriefStudioPanel onClose={() => setBriefStudioOpen(false)} />
          ) : crossRepoPlannerOpen ? (
            <CrossRepoChangePlannerPanel onClose={() => setCrossRepoPlannerOpen(false)} />
          ) : crossRepoIntelligenceOpen ? (
            <CrossRepoIntelligencePanel onClose={() => setCrossRepoIntelligenceOpen(false)} />
          ) : visualEditModeOpen ? (
            <VisualEditModePanel onClose={() => setVisualEditModeOpen(false)} />
          ) : runCenterOpen ? (
            <RunCenter onClose={() => setRunCenterOpen(false)} />
          ) : debateOpen ? (
            <DebatePanel onClose={() => setDebateOpen(false)} />
          ) : dbAdminGuardrailsOpen ? (
            <DatabaseAdminGuardrailsPanel onClose={() => setDbAdminGuardrailsOpen(false)} />
          ) : issueToPrOpen ? (
            <IssueToPrPanel
              onClose={() => setIssueToPrOpen(false)}
              onOpenRunCapsule={(runId) => {
                setIssueToPrOpen(false);
                setRunCenterOpen(true);
                void useRunStore.getState().selectRun(runId);
              }}
            />
          ) : securityAutofixOpen ? (
            <SecurityAutofixPanel
              onClose={() => setSecurityAutofixOpen(false)}
              onOpenRunCapsule={(runId) => {
                setSecurityAutofixOpen(false);
                setRunCenterOpen(true);
                void useRunStore.getState().selectRun(runId);
              }}
            />
          ) : trustScorecardsOpen ? (
            <TrustScorecardsPanel onClose={() => setTrustScorecardsOpen(false)} />
          ) : sopCompilerOpen ? (
            <SopCompilerPanel
              onClose={() => setSopCompilerOpen(false)}
              onOpenSkillProposals={() => {
                setSopCompilerOpen(false);
                openSettingsTab("prompts");
              }}
            />
          ) : mcpGeneratorOpen ? (
            <McpGeneratorPanel onClose={() => setMcpGeneratorOpen(false)} />
          ) : connectorBuilderOpen ? (
            <ConnectorBuilderPanel onClose={() => setConnectorBuilderOpen(false)} />
          ) : migrationAgentOpen ? (
            <MigrationAgentPanel
              onClose={() => setMigrationAgentOpen(false)}
              onOpenRunCapsule={(runId) => {
                setMigrationAgentOpen(false);
                setRunCenterOpen(true);
                void useRunStore.getState().selectRun(runId);
              }}
            />
          ) : apiContractDiffLabOpen ? (
            <ApiContractDiffLabPanel onClose={() => setApiContractDiffLabOpen(false)} />
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
              headerActionsSlot={chatHeaderActionsEl}
            />
          )}
        </ErrorBoundary>
        {terminalOpen && terminalDock === "bottom" && (
          <TerminalPanel chatSessionId={activeSessionId} onClose={() => setTerminalOpen(false)} />
        )}
      </div>


      {/* Split pane: a second, fully independent chat opened via the session
          menu's "Open in > Split view" — Claude-Desktop-style, inside the
          same window. Its top strip doubles as the pane header: session
          title + close, still draggable like the other title-bar strips. */}
      {!globalSearchOpen && !agentInboxOpen && !redTeamLabOpen && !knowledgeGraphOpen && !spreadsheetCopilotOpen && !evidenceBoardOpen && !goldenDatasetBuilderOpen && !dailyBriefOpen && !dataNotebookOpen && !syntheticMonitoringOpen && !workCanvasOpen && !pmCopilotOpen && !deepResearchOpen && !briefStudioOpen && !crossRepoPlannerOpen && !crossRepoIntelligenceOpen && !visualEditModeOpen && !runCenterOpen && !debateOpen && !dbAdminGuardrailsOpen && !issueToPrOpen && !securityAutofixOpen && !trustScorecardsOpen && !sopCompilerOpen && !mcpGeneratorOpen && !connectorBuilderOpen && !migrationAgentOpen && !apiContractDiffLabOpen && !browserWorkbenchOpen && activeComparisonId === null && activeCrewSessionId === null && splitSessionId !== null && (
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

      {/* Right region: hidden by default; shows exactly one of — the
          right-docked terminal (overrides everything), the picker menu, the
          side-tasks drawer, or the workspace panel. Claude-Desktop-style:
          contents swap in place, they never stack beside each other. */}
      {browserPaneOpen ? (
        <BrowserPane
          obscured={settingsOpen || commandPaletteOpen || permissionPending || approvalChainPending}
        />
      ) : diffPanelOpen ? (
        <DiffPanel onClose={() => setDiffPanelOpen(false)} />
      ) : terminalOpen && terminalDock === "right" ? (
        <TerminalPanel chatSessionId={activeSessionId} onClose={() => setTerminalOpen(false)} />
      ) : rightPanel === "sideTasks" ? (
        <SideTaskDrawer sessionId={activeSessionId} />
      ) : rightPanel === "review" ? (
        <aside className="flex w-[40rem] max-w-[60vw] shrink-0 flex-col border-l border-border bg-surface">
          <div data-tauri-drag-region className="flex h-11 shrink-0 items-center justify-between border-b border-border px-3">
            <span className="text-[11px] font-semibold uppercase tracking-wider text-faint">
              {t("App.rightPanelReview")}
            </span>
            <IconButton size="sm" onClick={() => setRightPanel("none")} aria-label={t("App.closeRightSidebar")} className="ml-auto">
              <X size={16} />
            </IconButton>
          </div>
          <ReviewPanel />
        </aside>
      ) : rightPanel === "menu" ? (
        <aside className="flex w-72 shrink-0 flex-col border-l border-border bg-surface">
          <div data-tauri-drag-region className="flex h-11 shrink-0 items-center justify-between border-b border-border px-3">
            <span className="text-[11px] font-semibold uppercase tracking-wider text-faint">
              {t("App.rightPanelMenuTitle")}
            </span>
            <IconButton size="sm" onClick={() => setRightPanel("none")} aria-label={t("App.closeRightSidebar")} className="ml-auto">
              <X size={16} />
            </IconButton>
          </div>
          <div className="flex flex-col gap-1 p-3">
            <button
              type="button"
              onClick={() => setRightPanel("workspace")}
              className="flex items-center gap-2.5 rounded-lg px-3 py-2.5 text-left text-sm text-foreground hover:bg-surface-2 focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-accent"
            >
              <FolderTree size={16} className="shrink-0 text-faint" /> {t("App.rightPanelWorkspace")}
            </button>
            <button
              type="button"
              disabled={!primaryRoot(useWorkspaceStore.getState().roots)}
              onClick={() => setRightPanel("review")}
              className="flex items-center gap-2.5 rounded-lg px-3 py-2.5 text-left text-sm text-foreground hover:bg-surface-2 focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-accent disabled:opacity-50"
            >
              <FileDiff size={16} className="shrink-0 text-faint" /> {t("App.rightPanelReview")}
            </button>
            <button
              type="button"
              disabled={!primaryRoot(useWorkspaceStore.getState().roots)}
              onClick={() => {
                useTerminalStore.getState().setDock("right");
                setTerminalOpen(true);
                setRightPanel("none");
              }}
              className="flex items-center gap-2.5 rounded-lg px-3 py-2.5 text-left text-sm text-foreground hover:bg-surface-2 focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-accent disabled:opacity-50"
            >
              <SquareTerminal size={16} className="shrink-0 text-faint" /> {t("App.rightPanelTerminal")}
            </button>
            <button
              type="button"
              onClick={() => {
                useSideTaskStore.getState().openDrawer();
                setRightPanel("sideTasks");
              }}
              className="flex items-center gap-2.5 rounded-lg px-3 py-2.5 text-left text-sm text-foreground hover:bg-surface-2 focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-accent"
            >
              <ListTodo size={16} className="shrink-0 text-faint" /> {t("App.rightPanelSideTasks")}
            </button>
          </div>
        </aside>
      ) : rightPanel === "workspace" ? (
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
      ) : null}

      {commandPaletteOpen && (
        <CommandPalette
          onClose={() => setCommandPaletteOpen(false)}
          onOpenSettingsTab={openSettingsTab}
        />
      )}
      <PermissionModal />
      <ApprovalChainModal />
      <PrivacyFirewallGate />
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

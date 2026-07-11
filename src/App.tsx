import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { PanelRight, PanelRightClose, X } from "lucide-react";

import { ChatSessionList, ChatWindow } from "./components/Chat";
import { AppMenu } from "./components/AppMenu";
import { SettingsModal } from "./components/Settings";
import type { SettingsTab } from "./components/Settings";
import { FileTree, DiffViewer, PermissionModal, SessionGrantBanner } from "./components/Workspace";
import { IconButton, Button } from "./components/ui";
import { useSessionStore } from "./store/sessionStore";
import { useWorkspaceStore } from "./store/workspaceStore";
import { useModelStore } from "./store/modelStore";
import { useMcpStore } from "./store/mcpStore";
import { useT } from "./lib/i18n";

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
  const splitSessionId = useSessionStore((s) => s.splitSessionId);
  const splitTitle = useSessionStore((s) =>
    s.splitSessionId === null ? null : s.sessions.find((x) => x.id === s.splitSessionId)?.title ?? null
  );
  const closeSplit = useSessionStore((s) => s.closeSplit);
  const rootsVersion = useWorkspaceStore((s) => s.rootsVersion);
  const refreshRoots = useWorkspaceStore((s) => s.refreshRoots);
  const refreshRecent = useWorkspaceStore((s) => s.refreshRecent);
  const refreshModels = useModelStore((s) => s.refresh);
  const refreshOllama = useModelStore((s) => s.refreshOllama);
  const refreshProviders = useModelStore((s) => s.refreshProviders);
  const refreshMcp = useMcpStore((s) => s.refresh);
  const connectMcp = useMcpStore((s) => s.connect);

  const [workspacePanelOpen, setWorkspacePanelOpen] = useState(true);
  const [selectedFile, setSelectedFile] = useState<SelectedFile | null>(null);
  const [diffLoading, setDiffLoading] = useState(false);
  const [diffError, setDiffError] = useState<string | null>(null);
  const [settingsOpen, setSettingsOpen] = useState(false);
  // Tab Settings should jump to the moment it opens — set alongside
  // `settingsOpen` by anything that deep-links into a specific tab (right
  // now just `PersonaSelector`'s "Manage prompts…" row); left `undefined`
  // for the normal "open on whatever tab was last active" path (AppMenu).
  const [settingsInitialTab, setSettingsInitialTab] = useState<SettingsTab | undefined>(undefined);

  const handleManagePrompts = useCallback(() => {
    setSettingsInitialTab("prompts");
    setSettingsOpen(true);
  }, []);

  useEffect(() => {
    void refreshRoots();
    void refreshRecent();
    void refreshModels();
    void refreshOllama();
    void refreshProviders();
  }, [refreshRoots, refreshRecent, refreshModels, refreshOllama, refreshProviders]);

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
        <div className="min-h-0 flex-1 overflow-y-auto">
          <ChatSessionList />
        </div>
        <AppMenu onOpenSettings={() => setSettingsOpen(true)} />
      </aside>

      {/* Center: chat, with a drag-region strip standing in for the title bar */}
      <div className="flex min-w-0 flex-1 flex-col">
        <div data-tauri-drag-region className="h-11 shrink-0" />
        <SessionGrantBanner />
        <ChatWindow sessionId={activeSessionId} onManagePrompts={handleManagePrompts} />
      </div>

      {/* Split pane: a second, fully independent chat opened via the session
          menu's "Open in > Split view" — Claude-Desktop-style, inside the
          same window. Its top strip doubles as the pane header: session
          title + close, still draggable like the other title-bar strips. */}
      {splitSessionId !== null && (
        <div className="flex min-w-0 flex-1 flex-col border-l border-border">
          <div data-tauri-drag-region className="flex h-11 shrink-0 items-center justify-between gap-2 border-b border-border px-3">
            <span className="pointer-events-none min-w-0 truncate text-sm font-medium text-foreground">
              {splitTitle}
            </span>
            <IconButton size="sm" onClick={closeSplit} aria-label={t("App.closeSplitPane")}>
              <X size={16} />
            </IconButton>
          </div>
          <ChatWindow sessionId={splitSessionId} onManagePrompts={handleManagePrompts} />
        </div>
      )}

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

        {workspacePanelOpen && (
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
              <div className="min-h-0 flex-1 overflow-auto p-2">
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
        )}
      </aside>

      <PermissionModal />
      <SettingsModal open={settingsOpen} onClose={() => setSettingsOpen(false)} initialTab={settingsInitialTab} />
    </div>
  );
}

export default App;

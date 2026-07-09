import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { PanelRight, PanelRightClose } from "lucide-react";

import { ChatSessionList, ChatWindow } from "./components/Chat";
import { AppMenu } from "./components/AppMenu";
import { SettingsModal } from "./components/Settings";
import { FileTree, DiffViewer, PermissionModal, SessionGrantBanner } from "./components/Workspace";
import { IconButton, Button } from "./components/ui";
import { useWorkspaceStore } from "./store/workspaceStore";
import { useModelStore } from "./store/modelStore";
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
  const rootsVersion = useWorkspaceStore((s) => s.rootsVersion);
  const refreshRoots = useWorkspaceStore((s) => s.refreshRoots);
  const refreshRecent = useWorkspaceStore((s) => s.refreshRecent);
  const refreshModels = useModelStore((s) => s.refresh);
  const refreshOllama = useModelStore((s) => s.refreshOllama);
  const refreshProviders = useModelStore((s) => s.refreshProviders);

  const [workspacePanelOpen, setWorkspacePanelOpen] = useState(true);
  const [selectedFile, setSelectedFile] = useState<SelectedFile | null>(null);
  const [diffLoading, setDiffLoading] = useState(false);
  const [diffError, setDiffError] = useState<string | null>(null);
  const [settingsOpen, setSettingsOpen] = useState(false);

  useEffect(() => {
    void refreshRoots();
    void refreshRecent();
    void refreshModels();
    void refreshOllama();
    void refreshProviders();
  }, [refreshRoots, refreshRecent, refreshModels, refreshOllama, refreshProviders]);

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
        <ChatWindow />
      </div>

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
      <SettingsModal open={settingsOpen} onClose={() => setSettingsOpen(false)} />
    </div>
  );
}

export default App;

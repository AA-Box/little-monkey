import { useCallback, useEffect, useRef, useState } from "react";
import { Terminal as XTerm } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import {
  AlertTriangle,
  Box,
  Maximize2,
  Minimize2,
  PanelBottom,
  PanelRight,
  Paperclip,
  Plus,
  RefreshCw,
  ShieldCheck,
  TerminalSquare,
  X,
} from "lucide-react";

import { useT } from "../../lib/i18n";
import { useTerminalStore, buildTerminalEvidence } from "../../store/terminalStore";
import { primaryRoot, useWorkspaceStore } from "../../store/workspaceStore";
import { Button, IconButton } from "../ui";
import { SandboxPanel } from "./SandboxPanel";

/** Reads the app's themed color custom properties into concrete values for
 * xterm's theme object (xterm cannot consume `var(...)` references). The
 * cursor colors are explicit — without them xterm's defaults can collapse
 * into the background in a light theme, leaving the cursor invisible. */
function xtermTheme(): {
  background: string;
  foreground: string;
  cursor: string;
  cursorAccent: string;
  selectionBackground: string;
} {
  const styles = getComputedStyle(document.documentElement);
  const background = styles.getPropertyValue("--c-background").trim() || "#ffffff";
  const foreground = styles.getPropertyValue("--c-foreground").trim() || "#1a1a1a";
  return {
    background,
    foreground,
    cursor: foreground,
    cursorAccent: background,
    selectionBackground: styles.getPropertyValue("--c-accent-soft").trim() || "#b3d4fc",
  };
}

interface TerminalPanelProps {
  chatSessionId: string;
  onClose: () => void;
}

export function TerminalPanel({ chatSessionId, onClose }: TerminalPanelProps) {
  const { t } = useT();
  const roots = useWorkspaceStore((state) => state.roots);
  const sessions = useTerminalStore((state) => state.sessions);
  const activeSessionId = useTerminalStore((state) => state.activeSessionId);
  const initialized = useTerminalStore((state) => state.initialized);
  const busy = useTerminalStore((state) => state.busy);
  const error = useTerminalStore((state) => state.error);
  const initialize = useTerminalStore((state) => state.initialize);
  const createSession = useTerminalStore((state) => state.createSession);
  const setActive = useTerminalStore((state) => state.setActive);
  const write = useTerminalStore((state) => state.write);
  const restart = useTerminalStore((state) => state.restart);
  const close = useTerminalStore((state) => state.close);
  const resize = useTerminalStore((state) => state.resize);
  const queueEvidence = useTerminalStore((state) => state.queueEvidence);
  const clearError = useTerminalStore((state) => state.clearError);
  const dock = useTerminalStore((state) => state.dock);
  const setDock = useTerminalStore((state) => state.setDock);
  const panelSize = useTerminalStore((state) => state.panelSize);
  const setPanelSize = useTerminalStore((state) => state.setPanelSize);

  const defaultRoot = primaryRoot(roots);
  const [selectedWorkspaceId, setSelectedWorkspaceId] = useState(defaultRoot?.id ?? "");
  const [evidencePreview, setEvidencePreview] = useState<ReturnType<typeof buildTerminalEvidence> | null>(null);
  const [attachedNotice, setAttachedNotice] = useState(false);
  const [sandboxOpen, setSandboxOpen] = useState(false);
  const [expanded, setExpanded] = useState(false);

  const hostRef = useRef<HTMLDivElement>(null);
  const termRef = useRef<XTerm | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  /** Length of `active.output` already written to the emulator — new store
   * chunks are appended as deltas. When the bounded tail slides (256KB cap)
   * the prefix assumption breaks; the emulator is then reset and replayed. */
  const writtenRef = useRef(0);

  const active = sessions.find((session) => session.id === activeSessionId) ?? null;
  const activeId = active?.id ?? null;
  const activeStatus = active?.status ?? null;

  useEffect(() => {
    void initialize();
  }, [initialize]);

  useEffect(() => {
    if (!selectedWorkspaceId && defaultRoot) setSelectedWorkspaceId(defaultRoot.id);
  }, [defaultRoot, selectedWorkspaceId]);

  const startTerminal = useCallback(async () => {
    if (!selectedWorkspaceId) return;
    try {
      await createSession(selectedWorkspaceId);
    } catch {
      // The store owns the visible error text.
    }
  }, [createSession, selectedWorkspaceId]);

  // Auto-start, once per workspace, so opening the panel (or switching its
  // workspace selector) behaves like VSCode's integrated terminal (the panel
  // only mounts when `terminalOpen` — see App.tsx — so "open" and "start"
  // are already the same user action). Terminal actions are user-initiated
  // and carry no permission gate (see terminal.rs's module doc — the
  // `run_shell` gate belongs to the agent's shell tool, not the user's own
  // typing). A failed attempt is not retried automatically for that
  // workspace — the button remains as a manual fallback. Tracked by
  // workspace id (not a single mount-scoped flag) so switching the
  // workspace selector without closing the panel still auto-starts for the
  // newly selected one, instead of silently requiring the manual button.
  const autoStartedWorkspacesRef = useRef<Set<string>>(new Set());
  useEffect(() => {
    if (!initialized || busy || !selectedWorkspaceId) return;
    if (autoStartedWorkspacesRef.current.has(selectedWorkspaceId)) return;
    if (sessions.some((session) => session.workspace_id === selectedWorkspaceId)) return;
    autoStartedWorkspacesRef.current.add(selectedWorkspaceId);
    void startTerminal();
  }, [initialized, busy, selectedWorkspaceId, sessions, startTerminal]);

  // One emulator instance per visible session: (re)created when the active
  // session changes, torn down with the panel. Keystrokes go straight to the
  // PTY (`terminal_write`); the user's real shell handles line editing,
  // history, and completions — there is no separate command input.
  useEffect(() => {
    const host = hostRef.current;
    if (!host || !activeId) return;

    const term = new XTerm({
      convertEol: false,
      cursorBlink: true,
      fontSize: 12,
      fontFamily:
        'ui-monospace, "SF Mono", "JetBrains Mono", Menlo, Consolas, monospace',
      theme: xtermTheme(),
      allowProposedApi: true,
      scrollback: 5000,
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(host);
    fit.fit();
    termRef.current = term;
    fitRef.current = fit;
    writtenRef.current = 0;

    const replay = useTerminalStore.getState().sessions.find((session) => session.id === activeId)?.output ?? "";
    if (replay) {
      term.write(replay);
      writtenRef.current = replay.length;
    }

    const data = term.onData((chunk) => {
      void write(activeId, chunk);
    });

    // Keep the kernel PTY dimensions in lockstep with the emulator grid.
    const observer = new ResizeObserver(() => {
      fit.fit();
      void resize(activeId, term.rows, term.cols);
    });
    observer.observe(host);
    void resize(activeId, term.rows, term.cols);

    term.focus();

    return () => {
      observer.disconnect();
      data.dispose();
      term.dispose();
      termRef.current = null;
      fitRef.current = null;
      writtenRef.current = 0;
    };
  }, [activeId, resize, write]);

  // Streams store output into the emulator as deltas (the store remains the
  // single source of truth so evidence capture and session replay keep
  // working). A slid bounded tail (or restart) resets and replays.
  const output = active?.output ?? "";
  useEffect(() => {
    const term = termRef.current;
    if (!term) return;
    if (output.length >= writtenRef.current) {
      const delta = output.slice(writtenRef.current);
      if (delta) term.write(delta);
      writtenRef.current = output.length;
    } else {
      term.reset();
      term.write(output);
      writtenRef.current = output.length;
    }
  }, [output]);

  // Re-fit when the expanded height toggles (height transition ends fast;
  // ResizeObserver above also fires, this makes the refit immediate).
  useEffect(() => {
    fitRef.current?.fit();
  }, [expanded, dock, panelSize]);

  // Drag-to-resize: the handle sits on the edge facing the chat (top edge
  // when bottom-docked, left edge when right-docked). Pointer capture keeps
  // the drag alive when the cursor leaves the thin handle strip.
  const onResizeStart = useCallback((event: React.PointerEvent<HTMLDivElement>) => {
    event.preventDefault();
    const startCoord = dock === "bottom" ? event.clientY : event.clientX;
    const startSize = dock === "bottom" ? panelSize.bottom : panelSize.right;
    const target = event.currentTarget;
    target.setPointerCapture(event.pointerId);
    const move = (moveEvent: PointerEvent) => {
      const delta = startCoord - (dock === "bottom" ? moveEvent.clientY : moveEvent.clientX);
      setPanelSize(dock, startSize + delta);
    };
    const up = () => {
      target.removeEventListener("pointermove", move);
      target.removeEventListener("pointerup", up);
      target.removeEventListener("pointercancel", up);
    };
    target.addEventListener("pointermove", move);
    target.addEventListener("pointerup", up);
    target.addEventListener("pointercancel", up);
  }, [dock, panelSize, setPanelSize]);

  const prepareEvidence = useCallback(() => {
    if (!active) return;
    const selected = termRef.current?.getSelection().trim() ?? "";
    const evidence = buildTerminalEvidence(active, selected || undefined);
    if (!evidence.content) return;
    setEvidencePreview(evidence);
  }, [active]);

  const confirmEvidence = useCallback(() => {
    if (!evidencePreview) return;
    queueEvidence(chatSessionId, evidencePreview);
    setEvidencePreview(null);
    setAttachedNotice(true);
    window.setTimeout(() => setAttachedNotice(false), 2500);
  }, [chatSessionId, evidencePreview, queueEvidence]);

  return (
    <section
      className={`flex flex-col overflow-hidden border-border bg-background ${
        expanded
          ? "fixed inset-0 z-40 border"
          : dock === "bottom"
            ? "relative min-h-44 shrink-0 rounded-t-xl border shadow-sm"
            : "relative h-full min-w-56 shrink-0 border-l"
      }`}
      style={expanded ? undefined : dock === "bottom" ? { height: panelSize.bottom } : { width: panelSize.right }}
      aria-label={t("TerminalPanel.title")}
    >
      {!expanded && (
        <div
          role="separator"
          aria-orientation={dock === "bottom" ? "horizontal" : "vertical"}
          onPointerDown={onResizeStart}
          className={`absolute z-10 ${
            dock === "bottom"
              ? "inset-x-0 top-0 h-1.5 cursor-ns-resize"
              : "inset-y-0 left-0 w-1.5 cursor-ew-resize"
          } bg-transparent transition-colors hover:bg-accent/40 active:bg-accent/60`}
        />
      )}
      {/* Title row keeps only the always-reachable controls (new tab, dock
          toggle, expand, close) so a narrow right-docked column can never
          push them off-screen; per-session actions live on their own
          horizontally scrollable row below. */}
      <div className="flex min-h-10 shrink-0 items-center gap-1.5 bg-surface px-3 py-1.5">
        <span className="truncate whitespace-nowrap text-sm font-medium text-foreground">{t("TerminalPanel.title")}</span>
        <IconButton
          size="sm"
          variant="ghost"
          onClick={() => void startTerminal()}
          disabled={!selectedWorkspaceId || busy}
          aria-label={t("TerminalPanel.newTerminal")}
          title={t("TerminalPanel.newTerminal")}
        >
          <Plus size={14} />
        </IconButton>
        <div className="flex-1" />
        {roots.length > 1 && (
          <select
            value={selectedWorkspaceId}
            onChange={(event) => setSelectedWorkspaceId(event.target.value)}
            aria-label={t("TerminalPanel.workspaceLabel")}
            className="hidden max-w-40 rounded-md border border-border bg-background px-2 py-1 text-xs text-foreground sm:block"
          >
            {roots.map((root) => <option key={root.id} value={root.id}>{root.label}</option>)}
          </select>
        )}
        {active && (
          <>
            <IconButton
              size="sm"
              variant="ghost"
              onClick={prepareEvidence}
              disabled={!active.output.trim()}
              aria-label={t("TerminalPanel.attach")}
              title={t("TerminalPanel.attach")}
            >
              <Paperclip size={14} />
            </IconButton>
            <IconButton
              size="sm"
              variant="ghost"
              onClick={() => setSandboxOpen(true)}
              aria-label={t("SandboxPanel.openButton")}
              title={t("SandboxPanel.openButton")}
            >
              <Box size={14} />
            </IconButton>
          </>
        )}
        <IconButton
          size="sm"
          variant="ghost"
          onClick={() => setDock(dock === "bottom" ? "right" : "bottom")}
          aria-label={t(dock === "bottom" ? "TerminalPanel.dockRight" : "TerminalPanel.dockBottom")}
          title={t(dock === "bottom" ? "TerminalPanel.dockRight" : "TerminalPanel.dockBottom")}
        >
          {dock === "bottom" ? <PanelRight size={14} /> : <PanelBottom size={14} />}
        </IconButton>
        <IconButton
          size="sm"
          variant="ghost"
          onClick={() => setExpanded((value) => !value)}
          aria-label={t(expanded ? "TerminalPanel.collapsePanel" : "TerminalPanel.expandPanel")}
          title={t(expanded ? "TerminalPanel.collapsePanel" : "TerminalPanel.expandPanel")}
        >
          {expanded ? <Minimize2 size={14} /> : <Maximize2 size={14} />}
        </IconButton>
        <IconButton size="sm" variant="ghost" onClick={onClose} aria-label={t("TerminalPanel.closePanel")}>
          <X size={14} />
        </IconButton>
      </div>


      {/* Session tabs get a dedicated strip: sharing the header row with the
          Kill/Restart/Attach controls let flexbox squeeze the tab list into
          an unreadable sliver as soon as a couple of terminals were open. */}
      {sessions.length > 1 && (
        <div className="flex shrink-0 items-center gap-1 overflow-x-auto border-t border-border bg-surface px-3 py-1 [scrollbar-width:thin]">
          {sessions.map((session, index) => (
            <div
              key={session.id}
              className={`group inline-flex max-w-44 shrink-0 items-center rounded-md border transition-colors ${
                session.id === activeSessionId
                  ? "border-accent bg-accent-soft text-foreground"
                  : "border-transparent text-muted hover:bg-surface-2 hover:text-foreground"
              }`}
            >
              <button
                type="button"
                onClick={() => setActive(session.id)}
                className="inline-flex min-w-0 items-center gap-1.5 py-1 pl-2 text-xs focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-accent"
              >
                <span className={`h-1.5 w-1.5 shrink-0 rounded-full ${session.status === "running" ? "bg-success" : session.status === "error" ? "bg-danger" : "bg-faint"}`} />
                <span className="truncate">{t("TerminalPanel.tabLabel", { count: index + 1 })}</span>
              </button>
              <button
                type="button"
                aria-label={t("TerminalPanel.closeTab")}
                onClick={() => void close(session.id)}
                className="mr-1 rounded-sm p-0.5 text-faint opacity-70 hover:bg-background hover:text-danger group-hover:opacity-100 focus-visible:opacity-100 focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-accent"
              >
                <X size={11} />
              </button>
            </div>
          ))}
        </div>
      )}

      {error && (
        <div className="flex items-center justify-between gap-3 border-b border-danger bg-danger-soft px-3 py-1.5 text-xs text-danger">
          <span className="min-w-0 truncate">{error}</span>
          <button type="button" onClick={clearError} className="shrink-0 underline">{t("TerminalPanel.dismiss")}</button>
        </div>
      )}

      {active?.output_truncated && (
        <div className="border-b border-warning/30 bg-warning-soft px-3 py-1 text-[11px] text-warning">
          {t("TerminalPanel.outputTruncated")}
        </div>
      )}

      {!initialized || (busy && sessions.length === 0) ? (
        <div className="flex flex-1 items-center justify-center text-sm text-faint">{t("TerminalPanel.loading")}</div>
      ) : !active ? (
        <div className="flex flex-1 flex-col items-center justify-center gap-3 px-5 text-center">
          <div className="flex h-11 w-11 items-center justify-center rounded-xl bg-surface-2 text-faint">
            <TerminalSquare size={22} />
          </div>
          <div>
            <p className="text-sm font-medium text-foreground">{t("TerminalPanel.emptyTitle")}</p>
            <p className="mt-1 max-w-md text-xs text-muted">{t("TerminalPanel.emptyDescription")}</p>
          </div>
          {roots.length > 1 && (
            <select
              value={selectedWorkspaceId}
              onChange={(event) => setSelectedWorkspaceId(event.target.value)}
              aria-label={t("TerminalPanel.workspaceLabel")}
              className="rounded-md border border-border bg-background px-2 py-1.5 text-sm text-foreground sm:hidden"
            >
              {roots.map((root) => <option key={root.id} value={root.id}>{root.label}</option>)}
            </select>
          )}
          <Button variant="primary" size="sm" onClick={() => void startTerminal()} disabled={!selectedWorkspaceId || busy}>
            <Plus size={14} /> {t("TerminalPanel.startTerminal")}
          </Button>
        </div>
      ) : (
        <div className="relative min-h-0 flex-1">
          <div ref={hostRef} className="absolute inset-0 px-2 py-1" aria-label={t("TerminalPanel.outputLabel")} />
          {activeStatus !== "running" && (
            <div className="absolute inset-x-0 bottom-0 flex items-center justify-between gap-2 border-t border-border bg-surface px-3 py-1.5 text-xs text-muted">
              <span className="min-w-0 truncate">{t("TerminalPanel.restartToContinue")}</span>
              <Button size="sm" variant="secondary" onClick={() => active && void restart(active.id)} disabled={busy}>
                <RefreshCw size={12} /> {t("TerminalPanel.restart")}
              </Button>
            </div>
          )}
        </div>
      )}

      {attachedNotice && (
        <div className="pointer-events-none absolute bottom-12 right-3 rounded-md border border-success/30 bg-success-soft px-2.5 py-1.5 text-xs text-success shadow-lg">
          {t("TerminalPanel.attachedNotice")}
        </div>
      )}

      {evidencePreview && (
        <div className="absolute inset-0 z-20 flex items-center justify-center bg-black/50 p-3" role="dialog" aria-modal="true" aria-labelledby="terminal-evidence-title">
          <div className="flex max-h-full w-full max-w-xl flex-col rounded-xl border border-border bg-background shadow-xl">
            <div className="flex items-start gap-3 border-b border-border p-4">
              <div className="flex h-9 w-9 shrink-0 items-center justify-center rounded-full bg-warning-soft text-warning">
                <ShieldCheck size={18} />
              </div>
              <div>
                <h3 id="terminal-evidence-title" className="text-sm font-semibold text-foreground">{t("TerminalPanel.approvalTitle")}</h3>
                <p className="mt-1 text-xs text-muted">{t("TerminalPanel.approvalDescription")}</p>
              </div>
            </div>
            {evidencePreview.truncated && (
              <div className="flex items-center gap-2 border-b border-warning/30 bg-warning-soft px-4 py-2 text-xs text-warning">
                <AlertTriangle size={13} /> {t("TerminalPanel.evidenceTruncated")}
              </div>
            )}
            <pre className="min-h-0 flex-1 overflow-auto whitespace-pre-wrap break-words bg-surface-2 p-3 font-mono text-xs leading-5 text-foreground">
              {evidencePreview.content}
            </pre>
            <div className="flex flex-col-reverse gap-2 border-t border-border p-3 sm:flex-row sm:justify-end">
              <Button variant="ghost" onClick={() => setEvidencePreview(null)}>{t("TerminalPanel.cancel")}</Button>
              <Button variant="primary" onClick={confirmEvidence}><Paperclip size={13} /> {t("TerminalPanel.confirmAttach")}</Button>
            </div>
          </div>
        </div>
      )}

      {sandboxOpen && (
        <SandboxPanel initialCommand="" onClose={() => setSandboxOpen(false)} />
      )}
    </section>
  );
}

export default TerminalPanel;

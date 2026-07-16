import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import {
  AlertTriangle,
  Box,
  Paperclip,
  Plus,
  RefreshCw,
  Search,
  ShieldCheck,
  Square,
  TerminalSquare,
  X,
} from "lucide-react";

import { useT } from "../../lib/i18n";
import { useTerminalStore, buildTerminalEvidence, readableTerminalOutput } from "../../store/terminalStore";
import { primaryRoot, useWorkspaceStore } from "../../store/workspaceStore";
import { Button, IconButton, StatusPill } from "../ui";
import type { PillTone } from "../ui";
import { SandboxPanel } from "./SandboxPanel";

const MAX_HIGHLIGHT_MATCHES = 500;

function statusTone(status: "running" | "exited" | "killed" | "error"): PillTone {
  if (status === "running") return "success";
  if (status === "error") return "danger";
  if (status === "killed") return "warning";
  return "neutral";
}

function highlightedOutput(text: string, query: string): { content: ReactNode; count: number; limited: boolean } {
  const needle = query.trim().toLocaleLowerCase();
  if (!needle) return { content: text, count: 0, limited: false };

  const lower = text.toLocaleLowerCase();
  const parts: ReactNode[] = [];
  let cursor = 0;
  let count = 0;
  while (cursor < text.length) {
    const index = lower.indexOf(needle, cursor);
    if (index < 0 || count >= MAX_HIGHLIGHT_MATCHES) break;
    if (index > cursor) parts.push(text.slice(cursor, index));
    parts.push(
      <mark key={`${index}:${count}`} className="rounded-sm bg-warning-soft px-0.5 text-warning">
        {text.slice(index, index + needle.length)}
      </mark>,
    );
    count += 1;
    cursor = index + needle.length;
  }
  if (cursor < text.length) parts.push(text.slice(cursor));
  return { content: parts, count, limited: count >= MAX_HIGHLIGHT_MATCHES && lower.indexOf(needle, cursor) >= 0 };
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
  const histories = useTerminalStore((state) => state.historyByWorkspace);
  const initialized = useTerminalStore((state) => state.initialized);
  const busy = useTerminalStore((state) => state.busy);
  const error = useTerminalStore((state) => state.error);
  const initialize = useTerminalStore((state) => state.initialize);
  const createSession = useTerminalStore((state) => state.createSession);
  const setActive = useTerminalStore((state) => state.setActive);
  const execute = useTerminalStore((state) => state.execute);
  const interrupt = useTerminalStore((state) => state.interrupt);
  const kill = useTerminalStore((state) => state.kill);
  const restart = useTerminalStore((state) => state.restart);
  const close = useTerminalStore((state) => state.close);
  const resize = useTerminalStore((state) => state.resize);
  const loadHistory = useTerminalStore((state) => state.loadHistory);
  const queueEvidence = useTerminalStore((state) => state.queueEvidence);
  const clearError = useTerminalStore((state) => state.clearError);

  const defaultRoot = primaryRoot(roots);
  const [selectedWorkspaceId, setSelectedWorkspaceId] = useState(defaultRoot?.id ?? "");
  const [command, setCommand] = useState("");
  const [search, setSearch] = useState("");
  const [historyIndex, setHistoryIndex] = useState<number | null>(null);
  const [evidencePreview, setEvidencePreview] = useState<ReturnType<typeof buildTerminalEvidence> | null>(null);
  const [attachedNotice, setAttachedNotice] = useState(false);
  const [sandboxOpen, setSandboxOpen] = useState(false);
  const outputRef = useRef<HTMLPreElement>(null);
  const followOutputRef = useRef(true);
  const panelRef = useRef<HTMLDivElement>(null);

  const active = sessions.find((session) => session.id === activeSessionId) ?? null;
  const readableOutput = useMemo(() => readableTerminalOutput(active?.output ?? ""), [active?.output]);
  const searchResult = useMemo(() => highlightedOutput(readableOutput, search), [readableOutput, search]);
  const history = active ? histories[active.workspace_id] ?? [] : [];

  useEffect(() => {
    void initialize();
  }, [initialize]);

  useEffect(() => {
    if (!selectedWorkspaceId && defaultRoot) setSelectedWorkspaceId(defaultRoot.id);
  }, [defaultRoot, selectedWorkspaceId]);

  useEffect(() => {
    if (active) void loadHistory(active.workspace_id);
  }, [active?.workspace_id, loadHistory]);

  useEffect(() => {
    const node = outputRef.current;
    if (node && followOutputRef.current) node.scrollTop = node.scrollHeight;
  }, [active?.output]);

  // Keep the kernel PTY dimensions aligned with the visible panel. This is
  // best-effort and intentionally approximate because the line-oriented UI
  // uses the app's monospace font rather than a canvas terminal renderer.
  useEffect(() => {
    const node = panelRef.current;
    if (!node || !active) return;
    const observer = new ResizeObserver((entries) => {
      const rect = entries[0]?.contentRect;
      if (!rect) return;
      const cols = Math.max(20, Math.floor(rect.width / 8));
      const rows = Math.max(2, Math.floor(rect.height / 18));
      void resize(active.id, rows, cols);
    });
    observer.observe(node);
    return () => observer.disconnect();
  }, [active?.id, resize]);

  const startTerminal = useCallback(async () => {
    if (!selectedWorkspaceId) return;
    try {
      await createSession(selectedWorkspaceId);
      setCommand("");
      setHistoryIndex(null);
    } catch {
      // The store owns the visible error text.
    }
  }, [createSession, selectedWorkspaceId]);

  const submitCommand = useCallback(async () => {
    if (!active || active.status !== "running" || !command.trim() || busy) return;
    const pending = command;
    try {
      await execute(active.id, pending);
      setCommand("");
      setHistoryIndex(null);
      followOutputRef.current = true;
    } catch {
      // The command remains in the field after denial/failure for review.
    }
  }, [active, busy, command, execute]);

  const moveHistory = useCallback((direction: -1 | 1) => {
    if (history.length === 0) return;
    const current = historyIndex ?? history.length;
    const next = Math.min(history.length, Math.max(0, current + direction));
    setHistoryIndex(next === history.length ? null : next);
    setCommand(next === history.length ? "" : history[next] ?? "");
  }, [history, historyIndex]);

  const prepareEvidence = useCallback(() => {
    if (!active) return;
    const selection = window.getSelection();
    const selected = selection?.anchorNode && outputRef.current?.contains(selection.anchorNode)
      ? selection.toString().trim()
      : "";
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
      ref={panelRef}
      className="relative flex h-[min(42vh,24rem)] min-h-56 shrink-0 flex-col border-t border-border bg-surface"
      aria-label={t("TerminalPanel.title")}
    >
      <div className="flex min-h-10 shrink-0 items-center gap-1 border-b border-border px-2">
        <TerminalSquare size={15} className="shrink-0 text-faint" />
        <div className="flex min-w-0 flex-1 items-center gap-1 overflow-x-auto py-1 [scrollbar-width:thin]">
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
        <IconButton size="sm" variant="ghost" onClick={onClose} aria-label={t("TerminalPanel.closePanel")}>
          <X size={14} />
        </IconButton>
      </div>

      {error && (
        <div className="flex items-center justify-between gap-3 border-b border-danger bg-danger-soft px-3 py-1.5 text-xs text-danger">
          <span className="min-w-0 truncate">{error}</span>
          <button type="button" onClick={clearError} className="shrink-0 underline">{t("TerminalPanel.dismiss")}</button>
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
        <>
          <div className="flex flex-wrap items-center gap-1.5 border-b border-border px-2 py-1.5">
            <StatusPill tone={statusTone(active.status)}>{t(`TerminalPanel.status.${active.status}`)}</StatusPill>
            <span className="min-w-0 flex-1 truncate font-mono text-[11px] text-faint" title={active.workspace_path}>
              {active.workspace_path}
            </span>
            <div className="relative flex min-w-36 flex-1 items-center sm:max-w-64">
              <Search size={12} className="pointer-events-none absolute left-2 text-faint" />
              <input
                value={search}
                onChange={(event) => setSearch(event.target.value)}
                placeholder={t("TerminalPanel.searchPlaceholder")}
                aria-label={t("TerminalPanel.searchLabel")}
                className="w-full rounded-md border border-border bg-background py-1 pl-7 pr-12 text-xs text-foreground outline-none focus:border-accent"
              />
              {search.trim() && (
                <span className="pointer-events-none absolute right-2 text-[10px] text-faint">
                  {searchResult.limited ? `${searchResult.count}+` : searchResult.count}
                </span>
              )}
            </div>
            <Button size="sm" variant="ghost" onClick={() => void interrupt(active.id)} disabled={active.status !== "running"}>
              ^C
            </Button>
            <Button size="sm" variant="ghost" onClick={() => void kill(active.id)} disabled={active.status !== "running"}>
              <Square size={11} className="fill-current" /> {t("TerminalPanel.kill")}
            </Button>
            <Button size="sm" variant="ghost" onClick={() => void restart(active.id)} disabled={busy}>
              <RefreshCw size={12} /> {t("TerminalPanel.restart")}
            </Button>
            <Button size="sm" variant="secondary" onClick={prepareEvidence} disabled={!readableOutput.trim()}>
              <Paperclip size={12} /> {t("TerminalPanel.attach")}
            </Button>
            <Button size="sm" variant="secondary" onClick={() => setSandboxOpen(true)}>
              <Box size={12} /> {t("SandboxPanel.openButton")}
            </Button>
          </div>

          {active.output_truncated && (
            <div className="border-b border-warning/30 bg-warning-soft px-3 py-1 text-[11px] text-warning">
              {t("TerminalPanel.outputTruncated")}
            </div>
          )}

          <pre
            ref={outputRef}
            tabIndex={0}
            onScroll={(event) => {
              const node = event.currentTarget;
              followOutputRef.current = node.scrollHeight - node.scrollTop - node.clientHeight < 32;
            }}
            className="min-h-0 flex-1 select-text overflow-auto whitespace-pre-wrap break-words bg-[#0d1117] px-3 py-2 font-mono text-xs leading-5 text-[#d1d5db] outline-none focus-visible:ring-1 focus-visible:ring-inset focus-visible:ring-accent [overscroll-behavior:contain]"
            aria-label={t("TerminalPanel.outputLabel")}
          >
            {searchResult.content || t("TerminalPanel.awaitingOutput")}
          </pre>

          <div className="flex shrink-0 items-center gap-2 border-t border-border bg-background px-2 py-2">
            <span className="select-none font-mono text-sm text-success">$</span>
            <input
              value={command}
              onChange={(event) => setCommand(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter" && !event.shiftKey) {
                  event.preventDefault();
                  void submitCommand();
                } else if (event.key === "ArrowUp") {
                  event.preventDefault();
                  moveHistory(-1);
                } else if (event.key === "ArrowDown") {
                  event.preventDefault();
                  moveHistory(1);
                }
              }}
              disabled={active.status !== "running" || busy}
              placeholder={active.status === "running" ? t("TerminalPanel.commandPlaceholder") : t("TerminalPanel.restartToContinue")}
              aria-label={t("TerminalPanel.commandLabel")}
              className="min-w-0 flex-1 bg-transparent font-mono text-sm text-foreground outline-none placeholder:text-faint disabled:opacity-50"
              autoComplete="off"
              spellCheck={false}
            />
            <Button size="sm" variant="primary" onClick={() => void submitCommand()} disabled={active.status !== "running" || busy || !command.trim()}>
              {t("TerminalPanel.run")}
            </Button>
          </div>
        </>
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
            <pre className="min-h-0 flex-1 overflow-auto whitespace-pre-wrap break-words bg-[#0d1117] p-3 font-mono text-xs leading-5 text-[#d1d5db]">
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
        <SandboxPanel initialCommand={command} onClose={() => setSandboxOpen(false)} />
      )}
    </section>
  );
}

export default TerminalPanel;

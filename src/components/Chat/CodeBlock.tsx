import { useState, type ReactNode } from "react";
import { isTauri } from "@tauri-apps/api/core";
import { Prism as SyntaxHighlighter } from "react-syntax-highlighter";
import oneDark from "react-syntax-highlighter/dist/esm/styles/prism/one-dark";
import oneLight from "react-syntax-highlighter/dist/esm/styles/prism/one-light";
import { Check, Copy, Play, TerminalSquare } from "lucide-react";

import { useT } from "../../lib/i18n";
import { useAppliedTheme } from "../../lib/theme";
import { readableTerminalOutput, useTerminalStore } from "../../store/terminalStore";
import { primaryRoot, useWorkspaceStore } from "../../store/workspaceStore";

/** Fence language token -> display label shown in a `CodeBlock`'s header
 * (e.g. `bash` -> `Bash`). Falls back to capitalizing the raw token, and to
 * "Text" when a fence has no language at all. */
function displayLangLabel(lang: string): string {
  if (!lang) return "Text";
  return lang.charAt(0).toUpperCase() + lang.slice(1);
}

/** Fence languages whose body is a shell command the integrated terminal can
 * actually run. Anything else (a diff, a JSON blob, Python) gets no terminal
 * buttons — running it would only produce a shell syntax error. */
const SHELL_LANGS = new Set(["bash", "sh", "zsh", "shell", "console", "terminal", "fish"]);

/** What a block's Run produced: where its output begins inside the terminal
 * session's bounded output tail, or why it never got that far. A failure has
 * to be shown here — the store's own error surface is the terminal panel,
 * which "Run in terminal" deliberately does not open, so swallowing it leaves
 * a button that looks like it did nothing. */
type RunState =
  | { kind: "output"; sessionId: string; offset: number }
  | { kind: "error"; message: string };

/**
 * The terminal actions on a shell code block: type the command into the app's
 * terminal panel without running it, or run it in the block's own session and
 * show the output underneath.
 *
 * Both are the user running their own command — the same authority as typing
 * it into the terminal panel by hand, which is why neither carries the
 * `run_shell` permission gate (see `terminal.rs`'s module doc). The command
 * stays fully visible in the block above the buttons, so a click is a
 * deliberate act on text the user can read, not a hidden execution.
 */
function ShellActions({ body, onRun }: { body: string; onRun: (state: RunState) => void }) {
  const { t } = useT();
  const roots = useWorkspaceStore((state) => state.roots);
  const [busy, setBusy] = useState(false);
  const workspaceId = primaryRoot(roots)?.id ?? "";
  const command = body.trim();

  const withSession = async (act: (sessionId: string, outputLength: number) => Promise<void>) => {
    if (busy) return;
    if (!workspaceId) {
      onRun({ kind: "error", message: t("CodeBlock.noWorkspace") });
      return;
    }
    setBusy(true);
    try {
      const session = await useTerminalStore.getState().ensureSession(workspaceId);
      // Re-read rather than trusting the returned snapshot: output may have
      // arrived while the session was being started.
      const live = useTerminalStore.getState().sessions.find((entry) => entry.id === session.id);
      await act(session.id, (live ?? session).output.length);
    } catch (error) {
      onRun({ kind: "error", message: String(error) });
    } finally {
      setBusy(false);
    }
  };

  const handleRun = () =>
    withSession(async (sessionId, offset) => {
      onRun({ kind: "output", sessionId, offset });
      // `executeScript`, not `execute`: a fence can hold several command lines
      // (a plan the model wrote out, or a plain multi-line snippet), and the
      // PTY takes one submission at a time.
      await useTerminalStore.getState().executeScript(sessionId, command);
    });

  const handleOpen = () =>
    withSession(async (sessionId) => {
      useTerminalStore.getState().setActive(sessionId);
      // The panel writes it into the shell once it is open on this session —
      // unsubmitted, so the user presses Enter themselves.
      useTerminalStore.getState().requestCommand(command);
    });

  return (
    <>
      <button
        type="button"
        onClick={() => void handleRun()}
        disabled={busy}
        aria-label={t("CodeBlock.runInTerminal")}
        title={t("CodeBlock.runInTerminal")}
        className="flex cursor-pointer items-center justify-center rounded-md p-1 text-muted transition-colors hover:bg-foreground/10 hover:text-foreground disabled:cursor-not-allowed disabled:opacity-50"
      >
        <Play size={13} />
      </button>
      <button
        type="button"
        onClick={() => void handleOpen()}
        disabled={busy}
        aria-label={t("CodeBlock.openInTerminal")}
        title={t("CodeBlock.openInTerminal")}
        className="flex cursor-pointer items-center justify-center rounded-md p-1 text-muted transition-colors hover:bg-foreground/10 hover:text-foreground disabled:cursor-not-allowed disabled:opacity-50"
      >
        <TerminalSquare size={13} />
      </button>
    </>
  );
}

/** Live tail of everything the block's own run has produced so far. Reads the
 * same store the terminal panel does, so the output shown here and the output
 * in the panel are the same bytes. */
function RunOutput({ run }: { run: RunState }) {
  const { t } = useT();
  const output = useTerminalStore((state) => (run.kind === "output"
    ? state.sessions.find((session) => session.id === run.sessionId)?.output ?? ""
    : ""));
  // The store keeps only a bounded tail (256KB); once it slides past this
  // block's offset the prefix assumption is gone, so show the whole tail
  // rather than slicing at a byte count that no longer means anything.
  const text = run.kind === "error"
    ? run.message
    : readableTerminalOutput(output.length < run.offset ? output : output.slice(run.offset));

  return (
    <div className="border-t border-border bg-background px-3 py-2">
      <pre
        className={`max-h-64 overflow-auto whitespace-pre-wrap break-all font-mono text-[11px] ${
          run.kind === "error" ? "text-danger" : "text-foreground"
        }`}
        role={run.kind === "error" ? "alert" : undefined}
      >
        {text.trimEnd() || t("CodeBlock.running")}
      </pre>
    </div>
  );
}

/**
 * Renders a single fenced code block in a chat message: a header bar (language
 * label, optional extra action, terminal actions for a shell fence, copy
 * button) over syntax-highlighted source — shared by both previewable fences
 * (html/svg/mermaid, which also get a Preview button via `headerExtra`) and
 * plain ones, so every code block in the transcript looks and behaves the same
 * way. A shell fence that was run keeps its output attached underneath.
 *
 * Deliberately its own file, loaded via `lazy()` from `MessageBubble.tsx`
 * rather than imported at the top level there: `react-syntax-highlighter`'s
 * Prism bundle is the same heavy dependency `ArtifactPane.tsx` already keeps
 * out of the main entry chunk via `lazyComponents.tsx` — `MessageBubble.tsx`
 * is a core, always-loaded chat component, so a top-level import here would
 * pull that weight straight into the entry bundle and blow the CI bundle
 * budget (`scripts/check-bundle-budget.mjs`) the same way it did before this
 * file was split out.
 */
export default function CodeBlock({ lang, body, headerExtra }: { lang: string; body: string; headerExtra?: ReactNode }) {
  const { t } = useT();
  const [copied, setCopied] = useState(false);
  const [run, setRun] = useState<RunState | null>(null);
  // The block always sits on the chat's own `surface-2` (the same grey user
  // bubbles and the composer use) rather than One Dark's `#282c34`, so it never
  // punches a hole through the transcript in either theme; only the syntax
  // token palette follows the theme.
  const dark = useAppliedTheme() === "dark";
  // The integrated terminal only exists in the desktop app — in the browser
  // build there is no PTY to run anything in.
  const shell = SHELL_LANGS.has(lang.toLowerCase()) && body.trim().length > 0 && isTauri();

  const handleCopy = async () => {
    try {
      await navigator.clipboard.writeText(body);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      // Clipboard permission denied/unavailable — nothing more to do than
      // silently leave the button unclicked; there's no destructive fallback.
    }
  };

  return (
    <div className="my-2 overflow-hidden rounded-lg border border-border bg-surface-2 not-prose">
      <div className="flex items-center justify-between gap-2 border-b border-border bg-foreground/[0.04] px-3 py-1.5">
        <span className="font-mono text-[11px] uppercase tracking-wide text-muted">{displayLangLabel(lang)}</span>
        <div className="flex items-center gap-1">
          {headerExtra}
          {shell && <ShellActions body={body} onRun={setRun} />}
          <button
            type="button"
            onClick={() => void handleCopy()}
            aria-label={copied ? t("MessageBubble.copiedLabel") : t("MessageBubble.copyButton")}
            title={copied ? t("MessageBubble.copiedLabel") : t("MessageBubble.copyButton")}
            className="flex cursor-pointer items-center justify-center rounded-md p-1 text-muted transition-colors hover:bg-foreground/10 hover:text-foreground"
          >
            {copied ? <Check size={13} /> : <Copy size={13} />}
          </button>
        </div>
      </div>
      {/* `wrapLongLines` because the alternative is a horizontal scrollbar
          macOS hides until you hover it: a long command or a one-line JSON
          object then reads as text clipped at the border with no hint that
          the rest is reachable. `break-word` catches the tokens wrapping
          alone can't break — a long unbroken path or URL. */}
      <SyntaxHighlighter
        language={lang || "text"}
        style={dark ? oneDark : oneLight}
        wrapLongLines
        customStyle={{ margin: 0, padding: "0.75rem", background: "transparent", fontSize: "12px" }}
        codeTagProps={{ style: { whiteSpace: "pre-wrap", wordBreak: "break-word" } }}
      >
        {body}
      </SyntaxHighlighter>
      {run && <RunOutput run={run} />}
    </div>
  );
}

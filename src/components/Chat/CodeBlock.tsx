import { useState, type ReactNode } from "react";
import { Prism as SyntaxHighlighter } from "react-syntax-highlighter";
import oneDark from "react-syntax-highlighter/dist/esm/styles/prism/one-dark";
import oneLight from "react-syntax-highlighter/dist/esm/styles/prism/one-light";
import { Check, Copy } from "lucide-react";

import { useT } from "../../lib/i18n";
import { useAppliedTheme } from "../../lib/theme";

/** Fence language token -> display label shown in a `CodeBlock`'s header
 * (e.g. `bash` -> `Bash`). Falls back to capitalizing the raw token, and to
 * "Text" when a fence has no language at all. */
function displayLangLabel(lang: string): string {
  if (!lang) return "Text";
  return lang.charAt(0).toUpperCase() + lang.slice(1);
}

/**
 * Renders a single fenced code block in a chat message: a header bar (language
 * label, optional extra action, copy button) over syntax-highlighted source —
 * shared by both previewable fences (html/svg/mermaid, which also get a
 * Preview button via `headerExtra`) and plain ones, so every code block in
 * the transcript looks and behaves the same way.
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
  // The block always sits on the chat's own `surface-2` (the same grey user
  // bubbles and the composer use) rather than One Dark's `#282c34`, so it never
  // punches a hole through the transcript in either theme; only the syntax
  // token palette follows the theme.
  const dark = useAppliedTheme() === "dark";

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
      <SyntaxHighlighter
        language={lang || "text"}
        style={dark ? oneDark : oneLight}
        customStyle={{ margin: 0, padding: "0.75rem", background: "transparent", fontSize: "12px" }}
      >
        {body}
      </SyntaxHighlighter>
    </div>
  );
}

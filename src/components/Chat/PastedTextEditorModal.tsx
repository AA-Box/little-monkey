import { useEffect, useRef, useState } from "react";
import { FileText, X } from "lucide-react";

import { formatEstimatedTokens, formatPastedTextSize } from "../../lib/pastedText";

export interface PastedTextEditorModalProps {
  name: string;
  content: string;
  onClose: () => void;
  onSave: (content: string) => void;
}

/**
 * Local-only editor for a large clipboard paste. Opening, typing, saving and
 * closing this dialog never talks to a model; the text remains ordinary
 * composer state until the user explicitly sends the turn.
 */
export default function PastedTextEditorModal({ name, content, onClose, onSave }: PastedTextEditorModalProps) {
  const [draft, setDraft] = useState(content);
  const textareaRef = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        onClose();
      }
    };
    document.addEventListener("keydown", onKeyDown);
    return () => document.removeEventListener("keydown", onKeyDown);
  }, [onClose]);

  useEffect(() => {
    textareaRef.current?.focus();
  }, []);

  const save = () => {
    onSave(draft);
    onClose();
  };

  return (
    <div
      className="fixed inset-0 z-[100] flex items-center justify-center bg-black/35 p-4"
      role="dialog"
      aria-modal="true"
      aria-label={`Edit ${name}`}
      onMouseDown={(event) => {
        if (event.target === event.currentTarget) onClose();
      }}
    >
      <div className="flex h-[min(78vh,760px)] w-full max-w-4xl flex-col overflow-hidden rounded-2xl border border-border bg-background shadow-2xl">
        <div className="flex items-center gap-3 border-b border-border px-4 py-3">
          <FileText size={18} className="shrink-0 text-accent" />
          <div className="min-w-0 flex-1">
            <div className="truncate text-sm font-semibold text-foreground">{name}</div>
            <div className="mt-0.5 text-[11px] text-faint">
              {formatPastedTextSize(draft)} · {formatEstimatedTokens(draft)} estimated when sent
            </div>
          </div>
          <button
            type="button"
            onClick={onClose}
            aria-label="Close pasted text editor"
            className="flex h-8 w-8 shrink-0 cursor-pointer items-center justify-center rounded-md text-muted hover:bg-surface-2 hover:text-foreground"
          >
            <X size={15} />
          </button>
        </div>

        <div className="min-h-0 flex-1 p-4">
          <textarea
            ref={textareaRef}
            value={draft}
            onChange={(event) => setDraft(event.target.value)}
            spellCheck={false}
            className="h-full w-full resize-none rounded-xl border border-border bg-surface px-4 py-3 font-mono text-sm leading-relaxed text-foreground outline-none focus:border-accent focus:ring-1 focus:ring-accent"
          />
        </div>

        <div className="flex items-center justify-between gap-3 border-t border-border px-4 py-3">
          <p className="max-w-xl text-[11px] leading-relaxed text-faint">
            Creating and editing this attachment is fully local and uses no AI or tokens. Sending it still uses the normal model context; the token count above is an estimate, not a tokenizer charge.
          </p>
          <div className="flex shrink-0 items-center gap-2">
            <button
              type="button"
              onClick={onClose}
              className="cursor-pointer rounded-lg border border-border px-3 py-2 text-xs font-medium text-muted hover:bg-surface-2 hover:text-foreground"
            >
              Close
            </button>
            <button
              type="button"
              onClick={save}
              disabled={draft === content}
              className="cursor-pointer rounded-lg bg-accent px-3 py-2 text-xs font-medium text-accent-foreground hover:bg-accent-hover disabled:cursor-not-allowed disabled:opacity-50"
            >
              Save changes
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}

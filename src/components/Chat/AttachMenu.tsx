import { useEffect, useRef, useState } from "react";
import { FolderPlus, Paperclip, Plus } from "lucide-react";

import { IconButton } from "../ui";
import { useT } from "../../lib/i18n";

export interface AttachMenuProps {
  /** Invoked when the user picks "Add files" - the caller owns the actual file dialog. */
  onAddFiles: () => void;
  /** Invoked when the user picks "Add folder" - the caller owns the actual folder dialog. */
  onAddFolder: () => void;
}

/**
 * "+" attach button + dropdown rendered in ChatWindow's input area. Purely
 * presentational: it has no idea how files/folders actually get picked (no
 * Tauri imports at all) - it just calls back into the caller, which owns
 * that logic. Mirrors the floating-panel idiom used by ModeSelector /
 * MentionAutocomplete (absolute, closes on outside pointerdown).
 */
export function AttachMenu({ onAddFiles, onAddFolder }: AttachMenuProps) {
  const [open, setOpen] = useState(false);
  const containerRef = useRef<HTMLDivElement>(null);
  const { t } = useT();

  useEffect(() => {
    if (!open) return;
    function handlePointerDown(event: PointerEvent) {
      if (containerRef.current && !containerRef.current.contains(event.target as Node)) {
        setOpen(false);
      }
    }
    window.addEventListener("pointerdown", handlePointerDown);
    return () => window.removeEventListener("pointerdown", handlePointerDown);
  }, [open]);

  function handleAddFiles() {
    onAddFiles();
    setOpen(false);
  }

  function handleAddFolder() {
    onAddFolder();
    setOpen(false);
  }

  return (
    <div ref={containerRef} className="relative inline-block">
      <IconButton
        type="button"
        variant="ghost"
        size="sm"
        aria-label={t("AttachMenu.addAttachmentAriaLabel")}
        aria-haspopup="true"
        aria-expanded={open}
        onClick={() => setOpen((prev) => !prev)}
      >
        <Plus size={16} />
      </IconButton>

      {open && (
        <div className="absolute bottom-full left-0 z-20 mb-1 w-48 rounded-lg border border-border bg-background py-1 shadow-lg">
          <button
            type="button"
            onClick={handleAddFiles}
            className="flex w-full cursor-pointer items-center gap-2 px-3 py-2 text-left text-sm hover:bg-surface-2"
          >
            <Paperclip size={14} className="shrink-0 text-faint" />
            {t("AttachMenu.addFilesOrPhotos")}
          </button>
          <button
            type="button"
            onClick={handleAddFolder}
            className="flex w-full cursor-pointer items-center gap-2 px-3 py-2 text-left text-sm hover:bg-surface-2"
          >
            <FolderPlus size={14} className="shrink-0 text-faint" />
            {t("AttachMenu.addFolder")}
          </button>
        </div>
      )}
    </div>
  );
}

export default AttachMenu;

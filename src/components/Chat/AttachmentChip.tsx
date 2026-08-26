import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { FileText, Folder, FolderOpen, X } from "lucide-react";
import { useT } from "../../lib/i18n";
import { ContextMenu } from "../ui";

export interface AttachmentChipProps {
  /** Display name for the attachment (typically the final path segment). */
  name: string;
  /** Whether this attachment refers to a directory rather than a single file. */
  isDir: boolean;
  /** Invoked when the user clicks the remove ("x") control on the chip. */
  onRemove: () => void;
  /** Optional primary action for virtual/editable attachments. */
  onOpen?: () => void;
  /** Small secondary metadata, e.g. an estimated token count. */
  detail?: string;
  /** Whether the remove ("x") control is rendered at all. Defaults to
   * `true`; pass `false` to show an inert, non-removable chip (e.g. while a
   * workspace picker is locked). */
  removable?: boolean;
  /** A `data:` URL for an image attachment — renders as a small thumbnail in
   * place of the file/folder icon when present. Undefined for every
   * non-image attachment, and briefly undefined for an image one until its
   * pick-time read finishes (see `ChatWindow.tsx`'s `handleAddFiles`). */
  previewUrl?: string;
  /** Filesystem path this chip stands for, which unlocks the right-click
   * "Show in Finder" menu. Omitted for virtual attachments (terminal
   * evidence — see `AttachmentRef.content`) whose `path` names no file on
   * disk and would open the OS file manager on nothing. */
  revealPath?: string;
}

/**
 * Small pill representing one pending attachment (file or folder) in the
 * chat composer's input pill, above the textarea. The caller owns the
 * attachments list and what removing/opening one means. A real filesystem
 * attachment may also expose the one-item right-click "Show in Finder" menu
 * through the same `reveal_in_finder` command the workspace bar and session
 * menu use.
 */
export function AttachmentChip({
  name,
  isDir,
  onRemove,
  onOpen,
  detail,
  removable = true,
  previewUrl,
  revealPath,
}: AttachmentChipProps) {
  const { t } = useT();
  const Icon = isDir ? Folder : FileText;
  const [menuOpen, setMenuOpen] = useState(false);
  const chipRef = useRef<HTMLSpanElement>(null);

  useEffect(() => {
    if (!menuOpen) return;
    function handlePointerDown(event: PointerEvent) {
      if (!chipRef.current?.contains(event.target as Node)) setMenuOpen(false);
    }
    function handleKeyDown(event: KeyboardEvent) {
      if (event.key === "Escape") setMenuOpen(false);
    }
    document.addEventListener("pointerdown", handlePointerDown);
    document.addEventListener("keydown", handleKeyDown);
    return () => {
      document.removeEventListener("pointerdown", handlePointerDown);
      document.removeEventListener("keydown", handleKeyDown);
    };
  }, [menuOpen]);

  const showInFinder = () => {
    setMenuOpen(false);
    if (!revealPath) return;
    void invoke("reveal_in_finder", { path: revealPath }).catch((err) => console.error(err));
  };

  const keyboardInteractive = Boolean(onOpen || revealPath);

  return (
    <span
      ref={chipRef}
      tabIndex={keyboardInteractive ? 0 : undefined}
      role={onOpen ? "button" : undefined}
      aria-label={onOpen ? `Open ${name}` : undefined}
      onClick={onOpen}
      onContextMenu={
        revealPath
          ? (event) => {
              event.preventDefault();
              setMenuOpen(true);
            }
          : undefined
      }
      onKeyDown={
        keyboardInteractive
          ? (event) => {
              if (onOpen && (event.key === "Enter" || event.key === " ")) {
                event.preventDefault();
                onOpen();
                return;
              }
              if (revealPath && (event.key === "ContextMenu" || (event.key === "F10" && event.shiftKey))) {
                event.preventDefault();
                setMenuOpen(true);
              }
            }
          : undefined
      }
      className={`relative inline-flex items-center gap-1.5 rounded-md border border-border bg-surface-2 px-2 py-1 text-xs ${onOpen ? "cursor-pointer hover:bg-surface" : ""}`}
    >
      {previewUrl ? (
        <img src={previewUrl} alt="" className="h-4 w-4 shrink-0 rounded-sm object-cover" />
      ) : (
        <Icon size={12} className="shrink-0 text-faint" />
      )}
      <span className="flex min-w-0 flex-col">
        <span className="max-w-[10rem] truncate">{name}</span>
        {detail && <span className="max-w-[10rem] truncate text-[10px] leading-tight text-faint">{detail}</span>}
      </span>
      {removable && (
        <button
          type="button"
          onClick={(event) => {
            event.stopPropagation();
            onRemove();
          }}
          aria-label={t("AttachmentChip.removeAttachment")}
          className="shrink-0 cursor-pointer text-faint hover:text-danger"
        >
          <X size={10} />
        </button>
      )}
      {menuOpen && revealPath && (
        <ContextMenu
          className="left-0 top-full mt-1"
          entries={[
            {
              label: t("AttachmentChip.showInFinder"),
              icon: <FolderOpen size={14} className="text-faint" />,
              onClick: showInFinder,
            },
          ]}
        />
      )}
    </span>
  );
}

export default AttachmentChip;

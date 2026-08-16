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
 * chat composer's input pill, above the textarea. Purely presentational -
 * the caller owns the attachments list and what removing one means - except
 * for the one-item right-click menu, which reveals `revealPath` in the OS
 * file manager through the same `reveal_in_finder` command the workspace bar
 * and session menu use.
 */
export function AttachmentChip({ name, isDir, onRemove, removable = true, previewUrl, revealPath }: AttachmentChipProps) {
  const { t } = useT();
  const Icon = isDir ? Folder : FileText;
  const [menuOpen, setMenuOpen] = useState(false);
  const chipRef = useRef<HTMLSpanElement>(null);

  // Same dismissal contract as the app's other floating menus (WorkspaceBar,
  // SessionMenu): a pointerdown anywhere outside closes, as does Escape.
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

  return (
    <span
      ref={chipRef}
      // Focusable only when there is a menu to open: the context-menu key
      // (and Shift+F10) is the keyboard's only route to a right-click, and
      // it needs a focused element to fire on.
      tabIndex={revealPath ? 0 : undefined}
      onContextMenu={
        revealPath
          ? (event) => {
              event.preventDefault();
              setMenuOpen(true);
            }
          : undefined
      }
      onKeyDown={
        revealPath
          ? (event) => {
              if (event.key === "ContextMenu" || (event.key === "F10" && event.shiftKey)) {
                event.preventDefault();
                setMenuOpen(true);
              }
            }
          : undefined
      }
      className="relative inline-flex items-center gap-1.5 rounded-md border border-border bg-surface-2 px-2 py-1 text-xs"
    >
      {previewUrl ? (
        <img src={previewUrl} alt="" className="h-4 w-4 shrink-0 rounded-sm object-cover" />
      ) : (
        <Icon size={12} className="shrink-0 text-faint" />
      )}
      <span className="max-w-[10rem] truncate">{name}</span>
      {removable && (
        <button
          type="button"
          onClick={onRemove}
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

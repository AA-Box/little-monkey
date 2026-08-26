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
 * Small pill representing one pending attachment in the chat composer. The
 * outer wrapper is deliberately non-button UI: editable virtual attachments
 * get a real primary <button> and the remove affordance is a sibling button,
 * avoiding nested interactive controls while preserving full keyboard access.
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

  const body = (
    <>
      {previewUrl ? (
        <img src={previewUrl} alt="" className="h-4 w-4 shrink-0 rounded-sm object-cover" />
      ) : (
        <Icon size={12} className="shrink-0 text-faint" />
      )}
      <span className="flex min-w-0 flex-col">
        <span className="max-w-[10rem] truncate">{name}</span>
        {detail && <span className="max-w-[10rem] truncate text-[10px] leading-tight text-faint">{detail}</span>}
      </span>
    </>
  );

  return (
    <span
      ref={chipRef}
      tabIndex={revealPath && !onOpen ? 0 : undefined}
      onContextMenu={
        revealPath
          ? (event) => {
              event.preventDefault();
              setMenuOpen(true);
            }
          : undefined
      }
      onKeyDown={
        revealPath && !onOpen
          ? (event) => {
              if (event.key === "ContextMenu" || (event.key === "F10" && event.shiftKey)) {
                event.preventDefault();
                setMenuOpen(true);
              }
            }
          : undefined
      }
      className="relative inline-flex items-center gap-1 rounded-md border border-border bg-surface-2 px-1 py-1 text-xs"
    >
      {onOpen ? (
        <button
          type="button"
          onClick={onOpen}
          aria-label={`Open ${name}`}
          className="flex min-w-0 cursor-pointer items-center gap-1.5 rounded px-1 text-left hover:bg-surface"
        >
          {body}
        </button>
      ) : (
        <span className="flex min-w-0 items-center gap-1.5 px-1">{body}</span>
      )}
      {removable && (
        <button
          type="button"
          onClick={onRemove}
          aria-label={t("AttachmentChip.removeAttachment")}
          className="shrink-0 cursor-pointer rounded p-0.5 text-faint hover:bg-surface hover:text-danger"
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

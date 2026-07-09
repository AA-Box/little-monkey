import { FileText, Folder, X } from "lucide-react";
import { useT } from "../../lib/i18n";

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
}

/**
 * Small pill representing one pending attachment (file or folder) in the
 * chat composer's input pill, above the textarea. Purely presentational -
 * the caller owns the attachments list and what removing one means.
 */
export function AttachmentChip({ name, isDir, onRemove, removable = true, previewUrl }: AttachmentChipProps) {
  const { t } = useT();
  const Icon = isDir ? Folder : FileText;

  return (
    <span className="inline-flex items-center gap-1.5 rounded-md border border-border bg-surface-2 px-2 py-1 text-xs">
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
    </span>
  );
}

export default AttachmentChip;

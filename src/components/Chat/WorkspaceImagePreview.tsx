import { memo, useEffect, useState } from "react";
import { ImageOff, LoaderCircle } from "lucide-react";

import { loadWorkspaceImage } from "../../lib/imageGeneration";
import { useT } from "../../lib/i18n";

/**
 * Inline preview of a workspace image file in the chat transcript — used by
 * `MessageBubble.tsx`'s Markdown `img` override for workspace-relative image
 * references. New generated-image cards load from private artifact storage;
 * their legacy fallback reuses the loader below directly.
 *
 * Loads the file through the read-only `workspace_read_image` Tauri command
 * (see `imageGeneration.ts::loadWorkspaceImage`) into a `data:` URL rather
 * than pointing an `<img>` at the filesystem: the webview can't load
 * arbitrary file paths directly, and going through the command keeps the
 * read inside the same workspace sandbox every other file read uses.
 *
 * `refreshKey` re-triggers the load for the same path when a caller knows its
 * bytes changed. Outside Tauri (browser mode)
 * `loadWorkspaceImage` resolves `null` and the component renders nothing —
 * there is no workspace filesystem to read, and an error chip for every
 * image would just be noise.
 */
function WorkspaceImagePreview({ path, refreshKey = 0, alt }: { path: string; refreshKey?: number; alt?: string }) {
  const { t } = useT();
  const [dataUrl, setDataUrl] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    let stale = false;
    setLoading(true);
    setError(null);
    loadWorkspaceImage(path)
      .then((url) => {
        if (stale) return;
        setDataUrl(url);
        setLoading(false);
      })
      .catch((caught: unknown) => {
        if (stale) return;
        setError(caught instanceof Error ? caught.message : String(caught));
        setLoading(false);
      });
    return () => {
      stale = true;
    };
  }, [path, refreshKey]);

  if (loading) {
    return (
      <div className="flex items-center gap-1.5 rounded-lg border border-border bg-surface-2 px-3 py-2 text-xs text-faint">
        <LoaderCircle size={12} className="animate-spin" />
        {t("WorkspaceImage.loading")}
      </div>
    );
  }

  if (error) {
    return (
      <div className="flex items-center gap-1.5 rounded-lg border border-border bg-surface-2 px-3 py-2 text-xs text-muted">
        <ImageOff size={12} className="shrink-0 text-faint" />
        <span className="min-w-0 break-words">{t("WorkspaceImage.error", { error })}</span>
      </div>
    );
  }

  if (!dataUrl) return null;

  return (
    <img
      src={dataUrl}
      alt={alt || path}
      title={path}
      className="max-h-96 max-w-full rounded-lg border border-border bg-white object-contain"
    />
  );
}

export default memo(WorkspaceImagePreview);

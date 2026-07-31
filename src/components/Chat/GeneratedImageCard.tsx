import { memo, useEffect, useState } from "react";
import { save } from "@tauri-apps/plugin-dialog";
import { writeFile } from "@tauri-apps/plugin-fs";
import {
  Check,
  Copy,
  Download,
  ImageOff,
  LoaderCircle,
  MoreHorizontal,
  Pencil,
} from "lucide-react";

import {
  loadGeneratedImage,
  loadWorkspaceImage,
  parseGeneratedImageReceipt,
} from "../../lib/imageGeneration";
import { useT } from "../../lib/i18n";
import { errorMessage } from "../../lib/errors";

interface GeneratedImageCardProps {
  path: string;
  prompt: string;
  result?: string;
  failed: boolean;
  onEdit?: (path: string, prompt: string, artifactId?: string) => void | Promise<void>;
}

function fileName(path: string): string {
  return path.split(/[\\/]/).filter(Boolean).pop() || "generated-image.png";
}

function dataUrlBytes(dataUrl: string): Uint8Array {
  const encoded = dataUrl.slice(dataUrl.indexOf(",") + 1);
  const binary = atob(encoded);
  return Uint8Array.from(binary, (character) => character.charCodeAt(0));
}

/**
 * ChatGPT-style image generation surface for the `generate_image` tool.
 * The square is reserved as soon as the tool call appears, so the completed
 * PNG replaces the loader without moving the rest of the conversation.
 */
function GeneratedImageCard({ path, prompt, result, failed, onEdit }: GeneratedImageCardProps) {
  const { t } = useT();
  const pending = result === undefined;
  const receipt = parseGeneratedImageReceipt(result);
  const artifactId = receipt?.artifactId;
  const suggestedName = receipt?.suggestedName ?? fileName(path);
  const [dataUrl, setDataUrl] = useState<string | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [detailsOpen, setDetailsOpen] = useState(false);
  const [action, setAction] = useState<"idle" | "copying" | "copied" | "saving" | "saved">("idle");
  const [actionError, setActionError] = useState<string | null>(null);

  useEffect(() => {
    if (pending || failed) return;
    let stale = false;
    setLoadError(null);
    const image = artifactId ? loadGeneratedImage(artifactId) : loadWorkspaceImage(path);
    void image
      .then((url) => {
        if (!stale) setDataUrl(url);
      })
      .catch((caught: unknown) => {
        if (!stale) setLoadError(errorMessage(caught));
      });
    return () => {
      stale = true;
    };
  }, [artifactId, failed, path, pending]);

  useEffect(() => {
    if (action !== "copied" && action !== "saved") return;
    const timeout = window.setTimeout(() => setAction("idle"), 1800);
    return () => window.clearTimeout(timeout);
  }, [action]);

  const copyImage = async () => {
    if (!dataUrl) return;
    setAction("copying");
    setActionError(null);
    try {
      const response = await fetch(dataUrl);
      const blob = await response.blob();
      if (typeof ClipboardItem === "undefined" || !navigator.clipboard?.write) {
        throw new Error(t("GeneratedImage.clipboardUnavailable"));
      }
      await navigator.clipboard.write([new ClipboardItem({ [blob.type]: blob })]);
      setAction("copied");
    } catch (caught) {
      setAction("idle");
      setActionError(errorMessage(caught));
    }
  };

  const downloadPng = async () => {
    if (!dataUrl) return;
    setAction("saving");
    setActionError(null);
    try {
      const destination = await save({
        defaultPath: suggestedName,
        filters: [{ name: "PNG", extensions: ["png"] }],
      });
      if (!destination) {
        setAction("idle");
        return;
      }
      await writeFile(destination, dataUrlBytes(dataUrl));
      setAction("saved");
    } catch (caught) {
      setAction("idle");
      setActionError(errorMessage(caught));
    }
  };

  if (pending) {
    return (
      <section className="w-full max-w-[30rem]" aria-live="polite" aria-busy="true">
        <p className="text-sm font-medium text-muted">{t("GeneratedImage.generatingLabel")}</p>
        <p className="mt-2 text-sm text-muted">{t("GeneratedImage.generatingDetail")}</p>
        <div className="relative mt-5 aspect-square w-full overflow-hidden rounded-[1.75rem] border border-border/60 bg-surface-2">
          <div
            aria-hidden
            className="absolute inset-[7%] animate-pulse text-faint motion-reduce:animate-none"
            style={{
              backgroundImage: "radial-gradient(circle, currentColor 1.6px, transparent 1.8px)",
              backgroundSize: "27px 27px",
              maskImage: "radial-gradient(circle at 50% 43%, black 12%, rgba(0,0,0,.9) 28%, transparent 68%)",
              WebkitMaskImage: "radial-gradient(circle at 50% 43%, black 12%, rgba(0,0,0,.9) 28%, transparent 68%)",
            }}
          />
          <div aria-hidden className="absolute inset-0 bg-[radial-gradient(circle_at_50%_42%,transparent_0%,transparent_32%,var(--color-surface-2)_76%)]" />
          <div className="absolute inset-x-0 bottom-8 flex items-center justify-center gap-2 text-xs text-faint">
            <LoaderCircle size={14} className="animate-spin motion-reduce:animate-none" />
            <span>{t("GeneratedImage.renderingPng")}</span>
          </div>
        </div>
      </section>
    );
  }

  if (failed || loadError) {
    return (
      <section className="w-full max-w-[30rem] rounded-2xl border border-danger/40 bg-danger-soft p-4" role="alert">
        <div className="flex items-start gap-3">
          <ImageOff size={18} className="mt-0.5 shrink-0 text-danger" />
          <div className="min-w-0">
            <p className="text-sm font-medium text-danger">{t("GeneratedImage.failed")}</p>
            <p className="mt-1 break-words text-xs leading-relaxed text-muted">{loadError ?? result}</p>
          </div>
        </div>
      </section>
    );
  }

  return (
    <figure className="w-full max-w-[30rem]">
      <div className="group/image relative aspect-square w-full overflow-hidden rounded-[1.75rem] border border-border bg-white shadow-sm">
        {dataUrl ? (
          <img
            src={dataUrl}
            alt={prompt || t("WorkspaceImage.generatedAlt")}
            className="h-full w-full object-contain"
          />
        ) : (
          <div className="flex h-full items-center justify-center text-faint" role="status">
            <LoaderCircle size={20} className="animate-spin motion-reduce:animate-none" />
            <span className="sr-only">{t("WorkspaceImage.loading")}</span>
          </div>
        )}

        {dataUrl && (
          <div className="absolute inset-x-0 bottom-0 flex items-end justify-between bg-gradient-to-t from-black/45 via-black/5 to-transparent px-3 pb-3 pt-14 opacity-100 transition-opacity duration-200 sm:opacity-0 sm:group-hover/image:opacity-100 sm:group-focus-within/image:opacity-100">
            <button
              type="button"
              onClick={() => void onEdit?.(path, prompt, artifactId)}
              disabled={!onEdit}
              className="flex min-h-11 cursor-pointer items-center gap-2 rounded-full bg-white/88 px-4 text-sm font-medium text-neutral-800 shadow-sm backdrop-blur transition-colors duration-200 hover:bg-white focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-white disabled:cursor-not-allowed disabled:opacity-50"
            >
              <Pencil size={14} />
              {t("GeneratedImage.editButton")}
            </button>
            <button
              type="button"
              onClick={() => void downloadPng()}
              disabled={action === "saving"}
              aria-label={t("GeneratedImage.downloadButton")}
              title={t("GeneratedImage.downloadButton")}
              className="flex h-11 w-11 cursor-pointer items-center justify-center rounded-full bg-white/88 text-neutral-800 shadow-sm backdrop-blur transition-colors duration-200 hover:bg-white focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-white disabled:cursor-wait disabled:opacity-60"
            >
              {action === "saving" ? <LoaderCircle size={17} className="animate-spin" /> : <Download size={17} />}
            </button>
          </div>
        )}
      </div>

      <figcaption className="mt-2 flex min-h-11 items-center gap-1 text-faint">
        <button
          type="button"
          onClick={() => void copyImage()}
          disabled={!dataUrl || action === "copying"}
          aria-label={t("GeneratedImage.copyButton")}
          title={t("GeneratedImage.copyButton")}
          className="flex h-11 w-11 cursor-pointer items-center justify-center rounded-full transition-colors duration-200 hover:bg-surface-2 hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent disabled:cursor-not-allowed disabled:opacity-45"
        >
          {action === "copied" ? <Check size={17} className="text-success" /> : <Copy size={17} />}
        </button>
        <button
          type="button"
          onClick={() => setDetailsOpen((open) => !open)}
          aria-expanded={detailsOpen}
          aria-label={t("GeneratedImage.moreButton")}
          title={t("GeneratedImage.moreButton")}
          className="flex h-11 w-11 cursor-pointer items-center justify-center rounded-full transition-colors duration-200 hover:bg-surface-2 hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent"
        >
          <MoreHorizontal size={18} />
        </button>
        {(action === "copied" || action === "saved") && (
          <span className="ml-1 text-xs text-success" role="status">
            {t(action === "copied" ? "GeneratedImage.copied" : "GeneratedImage.saved")}
          </span>
        )}
      </figcaption>

      {(detailsOpen || actionError) && (
        <div className="rounded-xl border border-border bg-surface-2 px-3 py-2 text-xs leading-relaxed text-muted">
          {detailsOpen && <p className="break-all">{t("GeneratedImage.readyAs", { name: suggestedName })}</p>}
          {actionError && <p className="mt-1 break-words text-danger" role="alert">{actionError}</p>}
        </div>
      )}
    </figure>
  );
}

export default memo(GeneratedImageCard);

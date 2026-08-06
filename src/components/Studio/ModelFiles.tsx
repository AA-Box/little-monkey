import { open } from "@tauri-apps/plugin-dialog";
import { FolderOpen, Plus, Trash2 } from "lucide-react";

import { Button, IconButton } from "../ui";
import { useT } from "../../lib/i18n";
import { describeWeightFile } from "../../lib/weightFileHints";
import {
  COMPONENT_SLOTS,
  type ComponentSlot,
  type ModelComponent,
} from "../../lib/studioClient";

/** The last segment of a path, which is the part anyone recognizes. */
function basename(path: string): string {
  return path.split(/[/\\]/).pop() ?? path;
}

function blankComponent(): ModelComponent {
  return {
    // An all-in-one checkpoint is the common case and the one whose name says
    // nothing, so it is what a fresh row starts as.
    slot: "checkpoint",
    source: { kind: "local_file", path: "" },
    sizeBytes: 0,
  };
}

/**
 * The weight files a model is made of: the checkpoint or diffusion model, plus
 * whatever CLIP, text encoder or VAE the architecture wants beside it.
 *
 * Shared by the add form and the generation page, because "this model is
 * missing its VAE" is discovered while generating, not while filling in a form,
 * and sending the user back to a different tab to fix it is the whole problem.
 *
 * Each slot is prefilled from the file's own name and every one stays an open
 * select. That distinction is the point: a wrong slot does not fail here, it
 * fails deep inside the engine as a tensor-shape error that reads like a
 * corrupt download — so this suggests only where the name names a component,
 * and leaves the row alone otherwise rather than inventing an answer.
 */
export function ModelFiles({
  components,
  onChange,
  /** Whether a row may be fetched from Hugging Face rather than picked off
   *  disk. The generation page is for pointing at a file you already have. */
  allowDownload = true,
}: {
  components: ModelComponent[];
  onChange: (next: ModelComponent[]) => void;
  allowDownload?: boolean;
}) {
  const { t } = useT();

  const patch = (index: number, next: Partial<ModelComponent>) => {
    const updated = components.map((component, at) =>
      at === index ? { ...component, ...next } : component,
    );
    const source = updated[index]?.source;
    const path =
      source?.kind === "local_file"
        ? source.path
        : source?.kind === "hugging_face"
          ? source.file
          : "";
    // A named component wins over whatever the row defaulted to; a file that
    // names nothing leaves the row alone. An explicit slot change always wins.
    const hint = path.trim() ? describeWeightFile(path).slot : null;
    if (hint && next.slot === undefined) {
      updated[index] = { ...updated[index], slot: hint };
    }
    onChange(updated);
  };

  /** The native picker, so a path is chosen rather than typed. */
  const browse = async (index: number) => {
    const picked = await open({ multiple: false, directory: false });
    if (typeof picked !== "string") return;
    patch(index, { source: { kind: "local_file", path: picked } });
  };

  return (
    <div className="grid gap-2">
      {components.map((component, index) => (
        <div key={index} className="grid gap-1.5 rounded bg-background/60 p-2">
          <div className="flex items-center gap-1.5">
            {/* What the part *is*, in words. The engine flag stays beside it
                because it is the unambiguous version, but nobody should have
                to know that `--clip_l` is a text encoder to fill this in. */}
            <select
              className="min-w-0 flex-1 rounded border border-border bg-background px-1.5 py-1 text-[11px] text-foreground"
              value={component.slot}
              aria-label={t("Studio.add.slot")}
              onChange={(event) => patch(index, { slot: event.target.value as ComponentSlot })}
            >
              {COMPONENT_SLOTS.map((entry) => (
                <option key={entry.slot} value={entry.slot}>
                  {t(`Studio.slot.${entry.slot}`)} ({entry.flag})
                </option>
              ))}
            </select>
            {allowDownload && (
              <select
                className="rounded border border-border bg-background px-1.5 py-1 text-[11px] text-foreground"
                value={component.source.kind}
                aria-label={t("Studio.add.source")}
                onChange={(event) =>
                  patch(index, {
                    source:
                      event.target.value === "local_file"
                        ? { kind: "local_file", path: "" }
                        : { kind: "hugging_face", repo: "", file: "" },
                  })
                }
              >
                <option value="local_file">{t("Studio.add.onDisk")}</option>
                <option value="hugging_face">{t("Studio.add.download")}</option>
              </select>
            )}
            <IconButton
              size="sm"
              aria-label={t("Studio.add.removeFile")}
              onClick={() => onChange(components.filter((_, at) => at !== index))}
            >
              <Trash2 size={12} />
            </IconButton>
          </div>

          {component.source.kind === "local_file" ? (
            // The file picker, not a path field. An absolute path is something
            // to point at, never something to type — and the full one is on
            // the tooltip for when it matters.
            <button
              type="button"
              title={component.source.path || t("Studio.add.browse")}
              className="flex w-full cursor-pointer items-center gap-1.5 rounded border border-border bg-background px-2 py-1 text-left text-[11px] text-foreground"
              onClick={() => void browse(index)}
            >
              <FolderOpen size={12} className="shrink-0 text-muted" />
              <span
                className={`min-w-0 flex-1 truncate ${
                  component.source.path ? "font-mono" : "text-faint"
                }`}
              >
                {component.source.path
                  ? basename(component.source.path)
                  : t("Studio.add.choose")}
              </span>
            </button>
          ) : (
            <div className="grid gap-1.5">
              <input
                className="rounded border border-border bg-background px-2 py-1 font-mono text-[11px] text-foreground"
                value={component.source.repo}
                placeholder="Comfy-Org/Wan_2.2_ComfyUI_Repackaged"
                onChange={(event) =>
                  patch(index, {
                    source: {
                      kind: "hugging_face",
                      repo: event.target.value,
                      file: component.source.kind === "hugging_face" ? component.source.file : "",
                    },
                  })
                }
              />
              <input
                className="rounded border border-border bg-background px-2 py-1 font-mono text-[11px] text-foreground"
                value={component.source.file}
                placeholder="split_files/vae/wan2.2_vae.safetensors"
                onChange={(event) =>
                  patch(index, {
                    source: {
                      kind: "hugging_face",
                      repo: component.source.kind === "hugging_face" ? component.source.repo : "",
                      file: event.target.value,
                    },
                  })
                }
              />
            </div>
          )}
        </div>
      ))}
      <Button
        size="sm"
        variant="secondary"
        onClick={() => onChange([...components, blankComponent()])}
      >
        <Plus size={13} />
        {t("Studio.add.addFile")}
      </Button>
    </div>
  );
}

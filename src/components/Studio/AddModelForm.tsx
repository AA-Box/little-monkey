import { useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { FolderOpen, Plus, Trash2 } from "lucide-react";

import { Button, IconButton } from "../ui";
import { useT } from "../../lib/i18n";
import { describeWeightFile } from "../../lib/weightFileHints";
import {
  ALL_TASKS,
  COMPONENT_SLOTS,
  emptyModelSpec,
  studioClient,
  type ComponentSlot,
  type GenerationModelSpec,
  type GenerationTask,
  type ModelComponent,
} from "../../lib/studioClient";

/** A slug the backend will accept as a directory name. */
function slugify(value: string): string {
  return value
    .toLowerCase()
    .replace(/[^a-z0-9._-]+/g, "-")
    .replace(/^[.-]+/, "")
    .slice(0, 128);
}

function blankComponent(): ModelComponent {
  return {
    slot: "checkpoint",
    source: { kind: "local_file", path: "" },
    sizeBytes: 0,
  };
}

/**
 * Adds a model to the user's library.
 *
 * Every slot is prefilled from the file's own name and every one stays an open
 * select. The distinction matters: a wrong slot does not fail here, it fails
 * deep inside the engine as a tensor-shape error that reads like a corrupt
 * download — so the form suggests only where the name names a component, and
 * leaves the row alone otherwise rather than inventing an answer.
 */
export function AddModelForm({ onSaved }: { onSaved: () => void }) {
  const { t } = useT();
  const [spec, setSpec] = useState<GenerationModelSpec>(emptyModelSpec);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const patch = (next: Partial<GenerationModelSpec>) =>
    setSpec((current) => ({ ...current, ...next }));

  const patchComponent = (index: number, next: Partial<ModelComponent>) =>
    setSpec((current) => {
      const components = current.components.map((component, at) =>
        at === index ? { ...component, ...next } : component,
      );
      // The first file named is what the model gets called. Both fields stay
      // editable — this fills a blank, it never overwrites a choice.
      const source = components[index]?.source;
      const path =
        source?.kind === "local_file"
          ? source.path
          : source?.kind === "hugging_face"
            ? source.file
            : "";
      if (!path.trim()) return { ...current, components };
      const hint = describeWeightFile(path);
      // A named component wins over whatever the row defaulted to; a file that
      // names nothing leaves the row alone.
      if (hint.slot && next.slot === undefined) {
        components[index] = { ...components[index], slot: hint.slot };
      }
      // Naming only ever fills a blank, and only from the first file.
      if (index !== 0 || (current.name && current.family)) {
        return { ...current, components };
      }
      return {
        ...current,
        components,
        name: current.name || hint.name,
        family: current.family || hint.family,
      };
    });

  /** The native picker, so a path is chosen rather than typed. */
  const browse = async (index: number) => {
    const picked = await open({ multiple: false, directory: false });
    if (typeof picked !== "string") return;
    patchComponent(index, { source: { kind: "local_file", path: picked } });
  };

  const toggleTask = (task: GenerationTask) =>
    setSpec((current) => ({
      ...current,
      tasks: current.tasks.includes(task)
        ? current.tasks.filter((entry) => entry !== task)
        : [...current.tasks, task],
    }));

  const save = async () => {
    setError(null);
    setBusy(true);
    try {
      await studioClient.addModel({
        ...spec,
        id: spec.id.trim() || slugify(spec.name),
      });
      setSpec(emptyModelSpec());
      onSaved();
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : String(reason));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="grid gap-3 rounded border border-border p-3">
      <p className="text-xs font-medium">{t("Studio.add.title")}</p>
      <p className="text-[11px] text-faint">{t("Studio.add.slotHint")}</p>
      {spec.tasks.includes("text_to_speech") && (
        <p className="text-[11px] text-faint">{t("Studio.add.speechHint")}</p>
      )}

      {error && (
        <p className="rounded border border-danger/40 bg-danger/10 px-2 py-1 text-[11px] text-danger">
          {error}
        </p>
      )}

      <label className="grid gap-1 text-[11px] text-muted">
        {t("Studio.add.name")}
        <input
          className="rounded border border-border bg-background px-2 py-1 text-xs text-foreground"
          value={spec.name}
          placeholder="Wan 2.2 TI2V 5B"
          onChange={(event) => patch({ name: event.target.value })}
        />
      </label>

      <label className="grid gap-1 text-[11px] text-muted">
        {t("Studio.add.family")}
        <input
          className="rounded border border-border bg-background px-2 py-1 text-xs text-foreground"
          value={spec.family}
          placeholder="Wan"
          onChange={(event) => patch({ family: event.target.value })}
        />
      </label>

      <fieldset className="grid gap-1 text-[11px] text-muted">
        <legend>{t("Studio.add.tasks")}</legend>
        <div className="flex flex-wrap gap-1.5">
          {ALL_TASKS.map((task) => (
            <Button
              key={task}
              size="sm"
              variant={spec.tasks.includes(task) ? "primary" : "secondary"}
              onClick={() => toggleTask(task)}
            >
              {t(`Studio.task.${task}`)}
            </Button>
          ))}
        </div>
      </fieldset>

      <div className="grid gap-2">
        <span className="text-[11px] text-muted">{t("Studio.add.files")}</span>
        {spec.components.map((component, index) => (
          <div key={index} className="grid gap-1.5 rounded bg-background/60 p-2">
            <div className="flex items-center gap-1.5">
              <select
                className="rounded border border-border bg-background px-1.5 py-1 font-mono text-[11px] text-foreground"
                value={component.slot}
                onChange={(event) =>
                  patchComponent(index, { slot: event.target.value as ComponentSlot })
                }
              >
                {COMPONENT_SLOTS.map((entry) => (
                  <option key={entry.slot} value={entry.slot}>
                    {entry.flag}
                  </option>
                ))}
              </select>
              <select
                className="rounded border border-border bg-background px-1.5 py-1 text-[11px] text-foreground"
                value={component.source.kind}
                onChange={(event) =>
                  patchComponent(index, {
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
              <IconButton
                size="sm"
                aria-label={t("Studio.add.removeFile")}
                onClick={() =>
                  patch({
                    components: spec.components.filter((_, at) => at !== index),
                  })
                }
              >
                <Trash2 size={12} />
              </IconButton>
            </div>

            {component.source.kind === "local_file" ? (
              <div className="flex items-center gap-1.5">
                <input
                  className="min-w-0 flex-1 rounded border border-border bg-background px-2 py-1 font-mono text-[11px] text-foreground"
                  value={component.source.path}
                  placeholder="/Users/you/models/wan2.2_ti2v_5B_fp16.safetensors"
                  onChange={(event) =>
                    patchComponent(index, {
                      source: { kind: "local_file", path: event.target.value },
                    })
                  }
                />
                <IconButton
                  size="sm"
                  variant="secondary"
                  aria-label={t("Studio.add.browse")}
                  title={t("Studio.add.browse")}
                  onClick={() => void browse(index)}
                >
                  <FolderOpen size={12} />
                </IconButton>
              </div>
            ) : (
              <div className="grid gap-1.5 sm:grid-cols-2">
                <input
                  className="rounded border border-border bg-background px-2 py-1 font-mono text-[11px] text-foreground"
                  value={component.source.repo}
                  placeholder="Comfy-Org/Wan_2.2_ComfyUI_Repackaged"
                  onChange={(event) =>
                    patchComponent(index, {
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
                    patchComponent(index, {
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
          onClick={() => patch({ components: [...spec.components, blankComponent()] })}
        >
          <Plus size={13} />
          {t("Studio.add.addFile")}
        </Button>
      </div>

      {/* Canvas size, steps, guidance and sampler are per-generation and live
          on the Image and Video tabs. What stays here is what the architecture
          fixes rather than the run: how the family rounds a clip's length, and
          how fast it plays. */}
      <div className="grid gap-2 sm:grid-cols-2">
        <label className="grid gap-1 text-[11px] text-muted">
          {t("Studio.add.fps")}
          <input
            type="number"
            min={1}
            max={60}
            className="rounded border border-border bg-background px-2 py-1 text-xs text-foreground"
            value={spec.defaults.fps}
            onChange={(event) =>
              patch({ defaults: { ...spec.defaults, fps: Number(event.target.value) } })
            }
          />
        </label>
        <label className="grid gap-1 text-[11px] text-muted">
          {t("Studio.add.frameGrid")}
          <select
            className="rounded border border-border bg-background px-2 py-1 text-xs text-foreground"
            value={spec.defaults.frameGrid}
            onChange={(event) =>
              patch({
                defaults: {
                  ...spec.defaults,
                  frameGrid: event.target.value as typeof spec.defaults.frameGrid,
                },
              })
            }
          >
            <option value="down_to4n_plus1">{t("Studio.add.grid4n1")}</option>
            <option value="up_to17k_plus5">{t("Studio.add.grid17k5")}</option>
          </select>
        </label>
      </div>

      <label className="grid gap-1 text-[11px] text-muted">
        {t("Studio.add.engineArgs")}
        <input
          className="rounded border border-border bg-background px-2 py-1 font-mono text-[11px] text-foreground"
          value={spec.extraLaunchArgs.join(" ")}
          placeholder="--diffusion-fa --offload-to-cpu"
          onChange={(event) =>
            patch({
              extraLaunchArgs: event.target.value.split(/\s+/).filter(Boolean),
            })
          }
        />
      </label>

      <Button
        variant="primary"
        disabled={busy || !spec.name.trim() || spec.components.length === 0}
        onClick={() => void save()}
      >
        {t("Studio.add.save")}
      </Button>
    </div>
  );
}

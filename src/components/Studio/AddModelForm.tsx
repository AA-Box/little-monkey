import { useState } from "react";

import { Button } from "../ui";
import { ModelFiles } from "./ModelFiles";
import { useT } from "../../lib/i18n";
import { describeWeightFile } from "../../lib/weightFileHints";
import {
  ALL_TASKS,
  emptyModelSpec,
  studioClient,
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

/** The path a component reads from, whichever source it uses. */
function componentPath(component: ModelComponent | undefined): string {
  const source = component?.source;
  if (source?.kind === "local_file") return source.path;
  if (source?.kind === "hugging_face") return source.file;
  return "";
}

/**
 * Adds a model to the user's library.
 *
 * The first file named says most of what the entry needs: its name, its
 * architecture family, and — through that family — what it can make and how it
 * counts frames. All of it lands in a visible control the user can overwrite,
 * because these guesses are cheap to correct on screen and expensive to
 * discover wrong several minutes into a load.
 */
export function AddModelForm({ onSaved }: { onSaved: () => void }) {
  const { t } = useT();
  const [spec, setSpec] = useState<GenerationModelSpec>(emptyModelSpec);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const patch = (next: Partial<GenerationModelSpec>) =>
    setSpec((current) => ({ ...current, ...next }));

  /** Fills every blank the first file can speak for. Only blanks: once the
   *  user has typed a name or picked a task, that is the answer. */
  const patchComponents = (components: ModelComponent[]) =>
    setSpec((current) => {
      const path = componentPath(components[0]);
      if (!path.trim()) return { ...current, components };
      const hint = describeWeightFile(path);
      // Frame rate and frame grid ride along with the tasks rather than being
      // reapplied on every edit, so a user who tuned them keeps them.
      const profile = current.tasks.length === 0 ? hint.profile : null;
      return {
        ...current,
        components,
        name: current.name || hint.name,
        family: current.family || hint.family,
        tasks: profile?.tasks ?? current.tasks,
        defaults: profile
          ? { ...current.defaults, fps: profile.fps, frameGrid: profile.frameGrid }
          : current.defaults,
      };
    });

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
          placeholder={t("Studio.add.namePlaceholder")}
          onChange={(event) => patch({ name: event.target.value })}
        />
      </label>

      <label className="grid gap-1 text-[11px] text-muted">
        {t("Studio.add.family")}
        <input
          className="rounded border border-border bg-background px-2 py-1 text-xs text-foreground"
          value={spec.family}
          placeholder={t("Studio.add.familyPlaceholder")}
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
        <ModelFiles components={spec.components} onChange={patchComponents} />
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
          placeholder={t("Studio.add.engineArgsPlaceholder")}
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

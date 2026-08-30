import { useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { FolderOpen, Trash2 } from "lucide-react";

import { Button, IconButton } from "../ui";
import { ModelFiles } from "./ModelFiles";
import { useT } from "../../lib/i18n";
import { describeWeightFile } from "../../lib/weightFileHints";
import {
  ALL_TASKS,
  emptyModelSpec,
  formatLaunchArgs,
  hasLaunchFlag,
  launchArgValue,
  parseLaunchArgs,
  setLaunchArg,
  setLaunchFlag,
  studioClient,
  type GenerationEngineKind,
  type GenerationModelSpec,
  type GenerationTask,
  type ModelComponent,
} from "../../lib/studioClient";

/** Engine flags that name a *directory* of extra weights rather than one file.
 *  The engine rescans these per run, so pointing at a folder is enough — the
 *  upscaler picker fills itself from whatever is found. */
export const DIRECTORY_FLAGS = [
  {
    flag: "--hires-upscalers-dir",
    labelKey: "Studio.add.upscalersDir",
    hintKey: "Studio.add.upscalersDirHint",
  },
  {
    flag: "--embd-dir",
    labelKey: "Studio.add.embeddingsDir",
    hintKey: "Studio.add.embeddingsDirHint",
  },
] as const;

/**
 * Launch-time switches worth a control of their own.
 *
 * Every one is verified present in the pinned engine's own `--help`, and every
 * one is a *launch* flag — so it belongs to the model entry rather than to a
 * run. That is a real limitation for `--circular`, which is a creative choice
 * someone would reasonably want per image; it is here because the engine takes
 * it at startup, not because that is the better place for it.
 *
 * Deliberately short. The engine has 136 flags and most are either already sent
 * per run (samplers, steps, guidance, schedulers) or so specialised that a
 * checkbox would be noise beside the args field that already reaches them.
 */
export const ENGINE_TOGGLES = [
  {
    flag: "--vae-tiling",
    labelKey: "Studio.add.vaeTiling",
    hintKey: "Studio.add.vaeTilingHint",
  },
  {
    flag: "--offload-to-cpu",
    labelKey: "Studio.add.offloadToCpu",
    hintKey: "Studio.add.offloadToCpuHint",
  },
  {
    flag: "--diffusion-fa",
    labelKey: "Studio.add.flashAttention",
    hintKey: "Studio.add.flashAttentionHint",
  },
  {
    flag: "--circular",
    labelKey: "Studio.add.seamless",
    hintKey: "Studio.add.seamlessHint",
  },
] as const;

/** A slug the backend will accept as a directory name. */
function slugify(value: string): string {
  return value
    .toLowerCase()
    .replace(/[^a-z0-9._-]+/g, "-")
    .replace(/^[.-]+/, "")
    .slice(0, 128);
}

const DEFAULT_MFLUX_REPOSITORY = "black-forest-labs/FLUX.1-dev";

function mfluxLicense(repo: string) {
  const normalized = repo.trim() || DEFAULT_MFLUX_REPOSITORY;
  return {
    id: `mflux:${normalized}`,
    name: "MFLUX model terms",
    url: `https://huggingface.co/${normalized}`,
    excludedTerritories: [],
    acceptanceRequired: true,
  };
}

/** The path a component reads from, whichever source it uses. */
function componentPath(component: ModelComponent | undefined): string {
  const source = component?.source;
  if (source?.kind === "local_file") return source.path;
  if (source?.kind === "hugging_face") return source.file;
  return "";
}

/**
 * Adds a model to the user's library or edits an existing library entry.
 *
 * The first file named says most of what the entry needs: its name, its
 * architecture family, and — through that family — what it can make and how it
 * counts frames. All of it lands in a visible control the user can overwrite,
 * because these guesses are cheap to correct on screen and expensive to
 * discover wrong several minutes into a load.
 */
interface AddModelFormProps {
  onSaved: () => void;
  /** When present, the form edits this library entry rather than creating one. */
  initialSpec?: GenerationModelSpec;
  editing?: boolean;
}

export function AddModelForm({ onSaved, initialSpec, editing = false }: AddModelFormProps) {
  const { t } = useT();
  const [spec, setSpec] = useState<GenerationModelSpec>(() => initialSpec ?? emptyModelSpec());
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [huggingFaceToken, setHuggingFaceToken] = useState("");
  const mfluxSelected = spec.engine === "mflux_image";
  const repositorySource = spec.source.kind === "hugging_face_repo" ? spec.source : null;
  const directorySource = spec.source.kind === "local_directory" ? spec.source : null;

  const patch = (next: Partial<GenerationModelSpec>) =>
    setSpec((current) => ({ ...current, ...next }));

  /** A complete repository/directory is the MFLUX model shape; component rows
   *  are the bundled engine shape. Keep this decision next to the source
   *  picker so the engine cannot drift from the model the user selected. */
  const chooseModelSource = (kind: GenerationModelSpec["source"]["kind"]) => {
    if (kind === "components") {
      patch({
        engine: "stable_diffusion_cpp",
        source: { kind: "components" },
        quantizationBits: null,
      });
      return;
    }
    const imageTasks = spec.tasks.filter(
      (task) => task === "text_to_image" || task === "image_to_image",
    );
    if (kind === "hugging_face_repo") {
      const repo = repositorySource?.repo?.trim() || DEFAULT_MFLUX_REPOSITORY;
      patch({
        engine: "mflux_image",
        components: [],
        source: {
          kind,
          repo,
          revision: repositorySource?.revision ?? null,
        },
        quantizationBits: spec.quantizationBits ?? 8,
        family: spec.family || "dev",
        defaults: { ...spec.defaults, sampleMethod: "linear" },
        tasks: imageTasks.length ? imageTasks : ["text_to_image", "image_to_image"],
        license: mfluxLicense(repo),
      });
      return;
    }
    patch({
      engine: "mflux_image",
      components: [],
      source: {
        kind,
        path: directorySource?.path ?? "",
      },
      quantizationBits: spec.quantizationBits ?? 8,
      family: spec.family || "dev",
      defaults: { ...spec.defaults, sampleMethod: "linear" },
      tasks: imageTasks.length ? imageTasks : ["text_to_image", "image_to_image"],
      license: {
        id: "",
        name: "",
        url: "",
        excludedTerritories: [],
        acceptanceRequired: false,
      },
    });
  };

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
      if (mfluxSelected && spec.source.kind === "hugging_face_repo") {
        await studioClient.setHuggingFaceToken(
          spec.id.trim() || slugify(spec.name),
          huggingFaceToken,
        );
      }
      setSpec(emptyModelSpec());
      setHuggingFaceToken("");
      onSaved();
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : String(reason));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="grid gap-3 rounded border border-border p-3">
      <p className="text-xs font-medium">{t(editing ? "Studio.add.editTitle" : "Studio.add.title")}</p>
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

      <label className="grid gap-1 text-[11px] text-muted">
        {t("Studio.add.mfluxSource")}
        <select
          className="rounded border border-border bg-background px-2 py-1 text-xs text-foreground"
          aria-label={t("Studio.add.mfluxSource")}
          value={spec.source.kind}
          onChange={(event) =>
            chooseModelSource(event.target.value as GenerationModelSpec["source"]["kind"])
          }
        >
          <option value="components">{t("Studio.add.componentFiles")}</option>
          <option value="hugging_face_repo">{t("Studio.add.mfluxRepository")}</option>
          <option value="local_directory">{t("Studio.add.mfluxDirectory")}</option>
        </select>
        <span className="text-faint">{t("Studio.add.engineAutoHint")}</span>
      </label>

      {spec.source.kind === "components" && <div className="grid gap-2">
        <span className="text-[11px] text-muted">{t("Studio.add.files")}</span>
        <ModelFiles components={spec.components} onChange={patchComponents} />
      </div>}

      {spec.source.kind !== "components" && (
        <div className="grid gap-2 rounded border border-border p-2">
          {spec.source.kind === "local_directory" ? (
            <span className="flex items-center gap-2">
              <span className="min-w-0 flex-1 truncate font-mono text-[11px] text-foreground">
                {directorySource?.path || <span className="text-faint">{t("Studio.add.noFolder")}</span>}
              </span>
              <Button
                size="sm"
                variant="secondary"
                onClick={async () => {
                  const picked = await open({ directory: true, multiple: false });
                  if (typeof picked === "string") {
                    patch({ source: { kind: "local_directory", path: picked } });
                  }
                }}
              >
                <FolderOpen size={13} />
                {t("Studio.add.chooseFolder")}
              </Button>
            </span>
          ) : (
            <label className="grid gap-1 text-[11px] text-muted">
              {t("Studio.add.mfluxRepository")}
              <input
                className="rounded border border-border bg-background px-2 py-1 font-mono text-xs text-foreground"
                value={repositorySource?.repo ?? ""}
                placeholder={t("Studio.add.mfluxRepositoryPlaceholder")}
                onChange={(event) =>
                  patch({
                    source: {
                      kind: "hugging_face_repo",
                      repo: event.target.value,
                      revision: repositorySource?.revision ?? null,
                    },
                    license: mfluxLicense(event.target.value),
                  })
                }
              />
            </label>
          )}
          {spec.source.kind === "hugging_face_repo" && (
            <label className="grid gap-1 text-[11px] text-muted">
              {t("Studio.add.mfluxToken")}
              <input
                type="password"
                autoComplete="off"
                className="rounded border border-border bg-background px-2 py-1 font-mono text-xs text-foreground"
                value={huggingFaceToken}
                placeholder={t("Studio.add.mfluxTokenPlaceholder")}
                onChange={(event) => setHuggingFaceToken(event.target.value)}
              />
              <span className="text-faint">{t("Studio.add.mfluxTokenHint")}</span>
            </label>
          )}
          <label className="grid gap-1 text-[11px] text-muted">
            {t("Studio.add.mfluxQuantization")}
            <select
              className="rounded border border-border bg-background px-2 py-1 text-xs text-foreground"
              value={spec.quantizationBits ?? 8}
              onChange={(event) => patch({ quantizationBits: Number(event.target.value) })}
            >
              {[8, 6, 5, 4, 3].map((bits) => (
                <option key={bits} value={bits}>{bits}-bit</option>
              ))}
            </select>
          </label>
        </div>
      )}

      {/* Canvas size, steps, guidance and sampler are per-generation and live
          on the Image and Video tabs. What stays here is what the architecture
          fixes rather than the run: how the family rounds a clip's length, and
          how fast it plays. */}
      {!mfluxSelected && <div className="grid gap-2 sm:grid-cols-2">
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
      </div>}

      {/* Both of these are directories, not weight files, which is why neither is
          a component slot: a slot resolves to one file. They write into the
          engine-args field below rather than carrying their own state, so a path
          typed there by hand and one picked here can never disagree about what
          actually launches. */}
      {!mfluxSelected && DIRECTORY_FLAGS.map(({ flag, labelKey, hintKey }) => {
        const current = launchArgValue(spec.extraLaunchArgs, flag);
        return (
          <label key={flag} className="grid gap-1 text-[11px] text-muted">
            {t(labelKey)}
            <span className="flex items-center gap-2">
              <span className="min-w-0 flex-1 truncate font-mono text-[11px] text-foreground">
                {current ?? <span className="text-faint">{t("Studio.add.noFolder")}</span>}
              </span>
              <Button
                size="sm"
                variant="secondary"
                onClick={async () => {
                  const picked = await open({ directory: true, multiple: false });
                  if (typeof picked === "string") {
                    patch({ extraLaunchArgs: setLaunchArg(spec.extraLaunchArgs, flag, picked) });
                  }
                }}
              >
                <FolderOpen size={13} />
                {t("Studio.add.chooseFolder")}
              </Button>
              {current && (
                <IconButton
                  size="sm"
                  aria-label={t("Studio.add.clearFolder")}
                  onClick={() => patch({ extraLaunchArgs: setLaunchArg(spec.extraLaunchArgs, flag, null) })}
                >
                  <Trash2 size={12} />
                </IconButton>
              )}
            </span>
            <span className="text-faint">{t(hintKey)}</span>
          </label>
        );
      })}

      {/* Which program renders this model. Above the launch switches because it
          decides which of them mean anything: the toggles below are
          stable-diffusion.cpp flags, and the MLX service ignores what it does
          not recognize rather than failing to start. */}
      <label className="grid gap-1 text-[11px] text-muted">
        {t("Studio.add.engine")}
        <select
          className="rounded border border-border bg-background px-2 py-1 text-[11px] text-foreground"
          aria-label={t("Studio.add.engine")}
          value={spec.engine}
          onChange={(event) => {
            const engine = event.target.value as GenerationEngineKind;
            if (engine === "mflux_image") {
              const imageTasks = spec.tasks.filter(
                (task) => task === "text_to_image" || task === "image_to_image",
              );
              patch({
                engine,
                components: [],
                source: {
                  kind: "hugging_face_repo",
                  repo: DEFAULT_MFLUX_REPOSITORY,
                  revision: null,
                },
                quantizationBits: 8,
                family: spec.family || "dev",
                defaults: { ...spec.defaults, sampleMethod: "linear" },
                tasks: imageTasks.length ? imageTasks : ["text_to_image", "image_to_image"],
                license: mfluxLicense(DEFAULT_MFLUX_REPOSITORY),
              });
            } else {
              patch({
                engine,
                source: { kind: "components" },
                quantizationBits: null,
              });
            }
          }}
        >
          <option value="stable_diffusion_cpp">{t("Studio.add.engineBundled")}</option>
          <option value="mlx_video">{t("Studio.add.engineMlxVideo")}</option>
          <option value="mflux_image">{t("Studio.add.engineMfluxImage")}</option>
        </select>
        <span className="text-faint">
          {t(
            spec.engine === "mlx_video"
              ? "Studio.add.engineMlxVideoHint"
              : spec.engine === "mflux_image"
                ? "Studio.add.engineMfluxImageHint"
                : "Studio.add.engineHint",
          )}
        </span>
      </label>

      {/* Launch-time switches. Reachable by typing them into the field below
          since the quote-aware parser landed, so this is discoverability rather
          than new capability — which is exactly why they are toggles over the
          same `extraLaunchArgs` and not a second place to store settings. */}
      {!mfluxSelected && <div className="grid gap-1.5">
        <span className="text-[11px] text-muted">{t("Studio.add.engineOptions")}</span>
        {ENGINE_TOGGLES.map(({ flag, labelKey, hintKey }) => (
          <label key={flag} className="flex items-start gap-2 text-[11px]">
            <input
              type="checkbox"
              className="mt-0.5"
              checked={hasLaunchFlag(spec.extraLaunchArgs, flag)}
              onChange={(event) =>
                patch({
                  extraLaunchArgs: setLaunchFlag(
                    spec.extraLaunchArgs,
                    flag,
                    event.target.checked,
                  ),
                })
              }
            />
            <span className="grid gap-0.5">
              <span className="text-foreground">{t(labelKey)}</span>
              <span className="text-faint">{t(hintKey)}</span>
            </span>
          </label>
        ))}
      </div>}

      {!mfluxSelected && <label className="grid gap-1 text-[11px] text-muted">
        {t("Studio.add.engineArgs")}
        <input
          className="rounded border border-border bg-background px-2 py-1 font-mono text-[11px] text-foreground"
          value={formatLaunchArgs(spec.extraLaunchArgs)}
          placeholder={t("Studio.add.engineArgsPlaceholder")}
          onChange={(event) => patch({ extraLaunchArgs: parseLaunchArgs(event.target.value) })}
        />
        <span className="text-faint">{t("Studio.add.engineArgsHint")}</span>
      </label>}

      <Button
        variant="primary"
        disabled={
          busy ||
          !spec.name.trim() ||
          (mfluxSelected
            ? (spec.source.kind === "hugging_face_repo"
                ? !spec.source.repo.trim()
                : spec.source.kind === "local_directory"
                  ? !spec.source.path.trim()
                  : true)
            : spec.components.length === 0)
        }
        onClick={() => void save()}
      >
        {t(editing ? "Studio.add.saveChanges" : "Studio.add.save")}
      </Button>
    </div>
  );
}

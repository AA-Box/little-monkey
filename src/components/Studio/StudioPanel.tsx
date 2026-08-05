import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  Download,
  Loader2,
  Shuffle,
  Sparkles,
  Square,
  Trash2,
  Upload,
  Wand2,
} from "lucide-react";

import { Button, IconButton, StatusPill, Tabs } from "../ui";
import { AddModelForm } from "./AddModelForm";
import { LoraStack } from "./LoraStack";
import { useT } from "../../lib/i18n";
import {
  componentFileName,
  editTaskFor,
  formatBytes,
  isSpeechTask,
  isVideoTask,
  needsInitImage,
  normalizeDimension,
  normalizeVideoFrames,
  ASPECT_PRESETS,
  SAMPLERS,
  SCHEDULERS,
  studioClient,
  UPSCALERS,
  type GenerationEngineStatus,
  type GenerationEntry,
  type GenerationModel,
  type GenerationTask,
  type HiresSettings,
  type LoraSelection,
} from "../../lib/studioClient";

/** The canvas and sampling controls for one run. Seeded from the model but
 *  owned by the tab, because they are choices about this generation rather
 *  than facts about the model. */
interface RunSettings {
  width: number;
  height: number;
  steps: number;
  cfgScale: number;
  sampler: string;
  scheduler: string;
  clipSkip: number;
  eta: number | null;
  /** How far an init image is redrawn. Ignored by the text-only tasks. */
  strength: number;
  /** Null means the second pass is off. */
  hires: HiresSettings | null;
}

/** The file extension a saved asset should carry, from its media type. */
function extensionFor(mediaType: string): string {
  const subtype = mediaType.split("/")[1] ?? "bin";
  return subtype === "jpeg" ? "jpg" : subtype.replace(/[^a-z0-9]/gi, "");
}

/** A slider paired with the number it sets, because a slider alone cannot be
 *  typed into and a number alone cannot be swept. */
function SliderField({
  label,
  value,
  min,
  max,
  step,
  hint,
  onChange,
}: {
  label: string;
  value: number;
  min: number;
  max: number;
  step: number;
  hint?: string;
  onChange: (value: number) => void;
}) {
  return (
    <label className="grid gap-1 text-[11px] text-muted">
      <span className="flex items-center justify-between gap-2">
        {label}
        {hint && <span className="font-mono text-faint">{hint}</span>}
      </span>
      <span className="flex items-center gap-2">
        <input
          type="range"
          min={min}
          max={max}
          step={step}
          value={value}
          onChange={(event) => onChange(Number(event.target.value))}
          className="min-w-0 flex-1"
        />
        <input
          type="number"
          min={min}
          max={max}
          step={step}
          value={value}
          onChange={(event) => onChange(Number(event.target.value))}
          className="w-20 shrink-0 rounded border border-border bg-background px-2 py-1 text-xs text-foreground"
        />
      </span>
    </label>
  );
}

/** The hires pass at its usual starting point, so the toggle has something
 *  sensible to switch on. */
const DEFAULT_HIRES: HiresSettings = {
  scale: 2,
  steps: 20,
  denoisingStrength: 0.5,
  upscaler: "Latent",
};

function errorText(reason: unknown): string {
  return reason instanceof Error ? reason.message : String(reason);
}

/** A model's own file basenames, for the "what will this download" list. */
function componentNames(model: GenerationModel): string[] {
  return model.components.map(componentFileName);
}

export type StudioMode = "models" | "image" | "video" | "audio";

/** Which tasks each making-tab covers. The models tab makes nothing, so it
 *  has no entry and its model list is never filtered. */
const MODE_TASKS: Record<Exclude<StudioMode, "models">, GenerationTask[]> = {
  image: ["text_to_image", "image_to_image"],
  video: ["text_to_video", "image_to_video"],
  audio: ["text_to_speech"],
};

const tasksFor = (mode: StudioMode): GenerationTask[] =>
  mode === "models" ? [] : MODE_TASKS[mode];

/** Studio talks to the engine over Tauri commands, which only exist inside the
 *  desktop window. In a plain browser tab every call throws a bare TypeError
 *  about `invoke`, which says nothing useful — detect it and say the real
 *  thing instead. */
const IN_DESKTOP_APP =
  typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

export function StudioPanel() {
  const { t } = useT();
  const [mode, setMode] = useState<StudioMode>("models");
  const [status, setStatus] = useState<GenerationEngineStatus | null>(null);
  const [models, setModels] = useState<GenerationModel[]>([]);
  const [gallery, setGallery] = useState<GenerationEntry[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [task, setTask] = useState<GenerationTask>("text_to_image");
  const [prompt, setPrompt] = useState("");
  const [negativePrompt, setNegativePrompt] = useState("");
  const [seconds, setSeconds] = useState(3);
  // A string rather than a number so "empty means random" is expressible, the
  // way every other generation tool spells it.
  const [seed, setSeed] = useState("");
  const [initImage, setInitImage] = useState<string | null>(null);
  const [speakerFile, setSpeakerFile] = useState("");
  const [language, setLanguage] = useState("");
  const [loras, setLoras] = useState<LoraSelection[]>([]);
  const [settings, setSettings] = useState<RunSettings | null>(null);
  const [adding, setAdding] = useState(false);
  const [busy, setBusy] = useState(false);
  const [downloadingId, setDownloadingId] = useState<string | null>(null);
  const [phase, setPhase] = useState<string | null>(null);
  const [percent, setPercent] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [previews, setPreviews] = useState<Record<string, string>>({});
  const fileInput = useRef<HTMLInputElement>(null);

  // A segment shows only the models that can do something in it, so the
  // picker never offers a video model under Image.
  const visible = useMemo(
    () =>
      mode === "models"
        ? models
        : models.filter((model) => model.tasks.some((entry) => tasksFor(mode).includes(entry))),
    [models, mode],
  );
  const selected = useMemo(
    () => visible.find((model) => model.id === selectedId) ?? null,
    [visible, selectedId],
  );

  const refresh = useCallback(async () => {
    try {
      const [engine, list, entries] = await Promise.all([
        studioClient.engineStatus(),
        studioClient.models(),
        studioClient.gallery(),
      ]);
      setStatus(engine);
      setModels(list);
      setGallery([...entries].reverse());
      const usable = list.filter((model) =>
        mode === "models" || model.tasks.some((entry) => tasksFor(mode).includes(entry)),
      );
      setSelectedId((current) => {
        if (current && usable.some((model) => model.id === current)) return current;
        // Prefer something already downloaded so the panel opens on a model
        // the user can actually run.
        return (usable.find((model) => model.installed) ?? usable[0])?.id ?? null;
      });
    } catch (reason) {
      setError(errorText(reason));
    }
  }, [mode]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  useEffect(() => {
    const unlisten = studioClient.onProgress((payload) => {
      setPhase(
        payload.phase === "running" && payload.queuePosition > 0
          ? t("Studio.phase.queued", { position: String(payload.queuePosition) })
          : payload.step !== null && payload.totalSteps !== null
            ? t("Studio.phase.step", {
                step: String(payload.step),
                total: String(payload.totalSteps),
              })
            : t(`Studio.phase.${payload.phase}`),
      );
      // Weight loading reports no step count, so the bar stays indeterminate
      // until the first sampling step rather than sitting at a false zero.
      setPercent(payload.percent);
    });
    return () => {
      void unlisten.then((stop) => stop());
    };
  }, [t]);

  // The controls follow whichever model is selected, so switching models
  // offers that model's own starting point rather than the last one's.
  useEffect(() => {
    if (!selected) return;
    setSettings({
      width: selected.defaults.width,
      height: selected.defaults.height,
      steps: selected.defaults.steps,
      cfgScale: selected.defaults.cfgScale,
      sampler: selected.defaults.sampleMethod,
      scheduler: "",
      clipSkip: -1,
      eta: null,
      strength: 0.75,
      hires: null,
    });
  }, [selected?.id]);

  // Keep the task valid for whichever model is selected: switching from a
  // video model to an image-only one must not leave a video task armed.
  useEffect(() => {
    const allowed = selected
      ? selected.tasks.filter((entry) => tasksFor(mode).includes(entry))
      : tasksFor(mode);
    if (mode !== "models" && !allowed.includes(task)) {
      setTask(allowed[0] ?? tasksFor(mode)[0]);
    }
  }, [selected, task, mode]);

  const loadPreview = useCallback(
    async (entry: GenerationEntry) => {
      if (previews[entry.artifactId]) return;
      try {
        const dataUrl = await studioClient.mediaDataUrl(entry.artifactId);
        setPreviews((current) => ({ ...current, [entry.artifactId]: dataUrl }));
      } catch (reason) {
        setError(errorText(reason));
      }
    },
    [previews],
  );

  useEffect(() => {
    const newest = gallery[0];
    if (newest) void loadPreview(newest);
  }, [gallery, loadPreview]);

  const download = async (model: GenerationModel) => {
    setError(null);
    if (model.license.acceptanceRequired && !model.licenseAccepted) {
      return;
    }
    setDownloadingId(model.id);
    try {
      await studioClient.downloadModel(model.id);
      await refresh();
    } catch (reason) {
      setError(errorText(reason));
    } finally {
      setDownloadingId(null);
    }
  };

  const acceptLicense = async (model: GenerationModel) => {
    try {
      await studioClient.acceptLicense(model.license.id);
      await refresh();
    } catch (reason) {
      setError(errorText(reason));
    }
  };

  const pickImage = async (file: File) => {
    const reader = new FileReader();
    reader.onload = () => {
      const result = String(reader.result ?? "");
      const comma = result.indexOf(",");
      setInitImage(comma < 0 ? null : result.slice(comma + 1));
    };
    reader.readAsDataURL(file);
  };

  const generate = async () => {
    if (!selected || !settings) return;
    setError(null);
    setBusy(true);
    setPercent(null);
    setPhase(t("Studio.phase.submitted"));
    try {
      const entry = await studioClient.run({
        modelId: selected.id,
        task,
        prompt,
        negativePrompt,
        width: normalizeDimension(settings.width),
        height: normalizeDimension(settings.height),
        steps: settings.steps,
        cfgScale: settings.cfgScale,
        sampleMethod: settings.sampler,
        scheduler: settings.scheduler,
        clipSkip: settings.clipSkip,
        eta: settings.eta,
        strength: needsInitImage(task) ? settings.strength : null,
        hires: settings.hires,
        // Blank asks the engine for a fresh seed rather than pinning one.
        seed: seed.trim() === "" ? -1 : Number(seed),
        videoFrames: isVideoTask(task)
          ? normalizeVideoFrames(selected.defaults.frameGrid, seconds * selected.defaults.fps)
          : 1,
        fps: isVideoTask(task) ? selected.defaults.fps : 1,
        speakerFile: isSpeechTask(task) ? speakerFile.trim() || null : null,
        language: isSpeechTask(task) ? language.trim() || null : null,
        initImageBase64: needsInitImage(task) ? initImage : null,
        // Blank rows are a half-typed path, not a LoRA the user meant.
        loras: loras.filter((lora) => lora.path.trim().length > 0),
      });
      setGallery((current) => [entry, ...current]);
      void loadPreview(entry);
    } catch (reason) {
      setError(errorText(reason));
    } finally {
      setBusy(false);
      setPhase(null);
      setPercent(null);
    }
  };

  /** Sends a finished asset back in as the next run's starting frame, which is
   *  how a generation model edits: regenerate from what you already have. */
  const editEntry = async (entry: GenerationEntry) => {
    if (!selected) return;
    const next = editTaskFor(selected);
    if (!next) {
      setError(t("Studio.result.noEditTask", { name: selected.name }));
      return;
    }
    try {
      const dataUrl = previews[entry.artifactId] ?? (await studioClient.mediaDataUrl(entry.artifactId));
      setPreviews((current) => ({ ...current, [entry.artifactId]: dataUrl }));
      setInitImage(dataUrl.slice(dataUrl.indexOf(",") + 1));
      setTask(next);
      setPrompt(entry.prompt);
    } catch (reason) {
      setError(errorText(reason));
    }
  };

  if (!IN_DESKTOP_APP) {
    return (
      <div className="flex min-h-0 flex-1 items-center justify-center p-8 text-center">
        <div className="max-w-md">
          <h2 className="text-sm font-medium">{t("Studio.browserOnly.title")}</h2>
          <p className="mt-2 text-xs text-muted">{t("Studio.browserOnly.body")}</p>
        </div>
      </div>
    );
  }

  if (status && !status.supported) {
    return (
      <div className="flex min-h-0 flex-1 items-center justify-center p-8 text-center">
        <div className="max-w-md">
          <h2 className="text-sm font-medium">{t("Studio.unsupported.title")}</h2>
          <p className="mt-2 text-xs text-muted">{t("Studio.unsupported.body")}</p>
        </div>
      </div>
    );
  }

  const effectiveFrames = selected
    ? normalizeVideoFrames(selected.defaults.frameGrid, seconds * selected.defaults.fps)
    : 0;
  const canGenerate =
    !!selected &&
    !!settings &&
    selected.installed &&
    prompt.trim().length > 0 &&
    !busy &&
    (!needsInitImage(task) || !!initImage);

  return (
    <div className="flex min-h-0 flex-1 flex-col overflow-y-auto p-4">
      <Tabs
        active={mode}
        onChange={(next) => setMode(next as StudioMode)}
        tabs={[
          { id: "models", label: t("Studio.tab.models") },
          { id: "image", label: t("Studio.tab.image") },
          { id: "video", label: t("Studio.tab.video") },
          { id: "audio", label: t("Studio.tab.audio") },
        ]}
      />
      <header className="mb-4 mt-3">
        <h1 className="text-sm font-medium">{t(`Studio.${mode}.title`)}</h1>
        <p className="mt-1 text-xs text-muted">{t(`Studio.${mode}.subtitle`)}</p>
      </header>

      {error && (
        <p className="mb-3 rounded border border-danger/40 bg-danger/10 px-3 py-2 text-xs text-danger">
          {error}
        </p>
      )}

      {mode === "models" ? (
      <section className="mb-4">
        <div className="mb-2 flex items-center justify-between">
          <h2 className="text-xs font-medium text-muted">{t("Studio.models")}</h2>
          <Button size="sm" variant="secondary" onClick={() => setAdding((open) => !open)}>
            {adding ? t("Studio.add.cancel") : t("Studio.add.open")}
          </Button>
        </div>
        {adding && (
          <div className="mb-2">
            <AddModelForm
              onSaved={() => {
                setAdding(false);
                void refresh();
              }}
            />
          </div>
        )}
        {visible.length === 0 && !adding && (
          <p className="mb-2 text-xs text-faint">{t("Studio.emptyLibrary")}</p>
        )}
        <div className="grid gap-2">
          {visible.map((model) => {
            const blockedByLicense =
              model.license.acceptanceRequired && !model.licenseAccepted;
            return (
              <div
                key={model.id}
                className={`rounded border p-3 ${
                  model.id === selectedId ? "border-accent" : "border-border"
                }`}
              >
                <button
                  type="button"
                  className="flex w-full cursor-pointer items-start justify-between gap-3 text-left"
                  onClick={() => setSelectedId(model.id)}
                >
                  <span className="min-w-0">
                    <span className="block text-xs font-medium">{model.name}</span>
                    <span className="mt-0.5 block text-[11px] text-faint">
                      {model.family} · {formatBytes(model.totalBytes)}
                      {" · "}
                      {model.tasks.map((entry) => t(`Studio.task.${entry}`)).join(", ")}
                    </span>
                  </span>
                  <span className="flex shrink-0 items-center gap-2">
                    <IconButton
                      size="sm"
                      aria-label={t("Studio.forget")}
                      onClick={(event) => {
                        event.stopPropagation();
                        void studioClient.removeModel(model.id).then(refresh).catch((reason) =>
                          setError(errorText(reason)),
                        );
                      }}
                    >
                      <Trash2 size={12} />
                    </IconButton>
                    {model.installed ? (
                      <StatusPill tone="success">{t("Studio.installed")}</StatusPill>
                    ) : (
                      <StatusPill tone="neutral">
                        {t("Studio.download", { size: formatBytes(model.missingBytes) })}
                      </StatusPill>
                    )}
                  </span>
                </button>

                {!model.fitsInMemory && (
                  <p className="mt-2 text-[11px] text-warning">
                    {t("Studio.tooLarge", { needed: formatBytes(model.minRamBytes) })}
                  </p>
                )}

                {blockedByLicense && (
                  <div className="mt-2 rounded bg-background/60 p-2">
                    <p className="text-[11px] text-warning">
                      {t("Studio.license.restricted", {
                        name: model.license.name,
                        territories: model.license.excludedTerritories.join(", "),
                      })}
                    </p>
                    <div className="mt-2 flex items-center gap-2">
                      <a
                        className="text-[11px] text-accent underline"
                        href={model.license.url}
                        target="_blank"
                        rel="noreferrer"
                      >
                        {t("Studio.license.read")}
                      </a>
                      <Button size="sm" onClick={() => void acceptLicense(model)}>
                        {t("Studio.license.accept")}
                      </Button>
                    </div>
                  </div>
                )}

                {!model.installed && !blockedByLicense && (
                  <div className="mt-2 flex items-center gap-2">
                    <Button
                      size="sm"
                      disabled={downloadingId !== null}
                      onClick={() => void download(model)}
                    >
                      {downloadingId === model.id ? (
                        <Loader2 size={13} className="animate-spin" />
                      ) : (
                        <Download size={13} />
                      )}
                      {t("Studio.download", { size: formatBytes(model.missingBytes) })}
                    </Button>
                    {downloadingId === model.id && (
                      <IconButton
                        size="sm"
                        aria-label={t("Studio.cancelDownload")}
                        onClick={() => void studioClient.cancelDownload(model.id)}
                      >
                        <Square size={12} />
                      </IconButton>
                    )}
                    <span className="truncate text-[11px] text-faint">
                      {componentNames(model).join(", ")}
                    </span>
                  </div>
                )}
              </div>
            );
          })}
        </div>
      </section>
      ) : (
        <section className="mb-3">
          <label className="grid gap-1 text-[11px] text-muted">
            {t("Studio.models")}
            <select
              className="rounded border border-border bg-background px-2 py-1 text-xs text-foreground"
              value={selectedId ?? ""}
              onChange={(event) => setSelectedId(event.target.value || null)}
            >
              {visible.length === 0 && <option value="">{t("Studio.noneForTab")}</option>}
              {visible.map((model) => (
                <option key={model.id} value={model.id}>
                  {model.name}
                  {model.installed ? "" : ` — ${t("Studio.notDownloaded")}`}
                </option>
              ))}
            </select>
          </label>
        </section>
      )}

      {mode !== "models" && selected && (
        <section className="mb-4 grid gap-3">
          <div className="flex flex-wrap gap-1.5">
            {selected.tasks
              .filter((entry) => tasksFor(mode).includes(entry))
              .map((entry) => (
              <Button
                key={entry}
                size="sm"
                variant={entry === task ? "primary" : "secondary"}
                onClick={() => setTask(entry)}
              >
                {t(`Studio.task.${entry}`)}
              </Button>
            ))}
          </div>

          <textarea
            className="min-h-20 w-full rounded border border-border bg-background p-2 text-xs"
            placeholder={t("Studio.promptPlaceholder")}
            value={prompt}
            onChange={(event) => setPrompt(event.target.value)}
          />
          {!isSpeechTask(task) && (
            <input
              className="w-full rounded border border-border bg-background p-2 text-xs"
              placeholder={t("Studio.negativePlaceholder")}
              value={negativePrompt}
              onChange={(event) => setNegativePrompt(event.target.value)}
            />
          )}

          {isSpeechTask(task) && (
            <div className="grid gap-3 sm:grid-cols-[2fr_1fr]">
              <label className="grid gap-1 text-[11px] text-muted">
                {t("Studio.speakerFile")}
                <input
                  className="rounded border border-border bg-background px-2 py-1 font-mono text-[11px] text-foreground"
                  placeholder="/Users/you/voices/narrator.wav"
                  value={speakerFile}
                  onChange={(event) => setSpeakerFile(event.target.value)}
                />
                <span className="text-faint">{t("Studio.speakerHint")}</span>
              </label>
              <label className="grid gap-1 text-[11px] text-muted">
                {t("Studio.language")}
                <input
                  className="rounded border border-border bg-background px-2 py-1 font-mono text-[11px] text-foreground"
                  placeholder="en"
                  maxLength={2}
                  value={language}
                  onChange={(event) =>
                    setLanguage(event.target.value.replace(/[^a-zA-Z]/g, ""))
                  }
                />
                <span className="text-faint">{t("Studio.languageHint")}</span>
              </label>
            </div>
          )}

          {needsInitImage(task) && (
            <div className="flex items-center gap-2">
              <input
                ref={fileInput}
                type="file"
                accept="image/png,image/jpeg,image/webp"
                className="hidden"
                onChange={(event) => {
                  const file = event.target.files?.[0];
                  if (file) void pickImage(file);
                }}
              />
              <Button size="sm" variant="secondary" onClick={() => fileInput.current?.click()}>
                <Upload size={13} />
                {t("Studio.chooseImage")}
              </Button>
              {initImage && (
                <>
                  <StatusPill tone="success">{t("Studio.imageReady")}</StatusPill>
                  <IconButton
                    size="sm"
                    aria-label={t("Studio.clearImage")}
                    onClick={() => setInitImage(null)}
                  >
                    <Trash2 size={12} />
                  </IconButton>
                </>
              )}
            </div>
          )}

          {isVideoTask(task) && (
            <label className="flex items-center gap-3 text-xs">
              <span className="w-24 shrink-0 text-muted">{t("Studio.duration")}</span>
              <input
                type="range"
                min={1}
                max={15}
                step={1}
                value={seconds}
                onChange={(event) => setSeconds(Number(event.target.value))}
                className="flex-1"
              />
              {/* The backend snaps length to its 4n+1 grid, so show the frame
                  count the clip will really have, not the slider's ask. */}
              <span className="w-28 shrink-0 text-right font-mono text-[11px] text-faint">
                {t("Studio.frames", {
                  frames: String(effectiveFrames),
                  fps: String(selected.defaults.fps),
                })}
              </span>
            </label>
          )}

          {/* Every one of these is a per-run choice, not a property of the
              model — the library entry only supplies the starting values. */}
          {settings && !isSpeechTask(task) && (
            <div className="grid gap-3 rounded border border-border p-3">
              <span className="text-xs font-medium">{t("Studio.settings")}</span>

              <div className="grid gap-1 text-[11px] text-muted">
                {t("Studio.aspect")}
                <div className="flex flex-wrap gap-1.5">
                  {ASPECT_PRESETS.map((preset) => (
                    <Button
                      key={preset.id}
                      size="sm"
                      variant={
                        settings.width === preset.width && settings.height === preset.height
                          ? "primary"
                          : "secondary"
                      }
                      onClick={() =>
                        setSettings({ ...settings, width: preset.width, height: preset.height })
                      }
                    >
                      {t(`Studio.aspect.${preset.id}`)}
                      <span className="font-mono text-[10px] opacity-70">
                        {preset.width}×{preset.height}
                      </span>
                    </Button>
                  ))}
                </div>
              </div>

              <div className="grid gap-3 sm:grid-cols-2">
                <SliderField
                  label={t("Studio.width")}
                  value={settings.width}
                  min={64}
                  max={2048}
                  step={32}
                  onChange={(width) => setSettings({ ...settings, width })}
                />
                <SliderField
                  label={t("Studio.height")}
                  value={settings.height}
                  min={64}
                  max={2048}
                  step={32}
                  // The engine aligns edges up to a multiple of 32, so show the
                  // canvas that will really be rendered.
                  hint={`${normalizeDimension(settings.width)}×${normalizeDimension(settings.height)}`}
                  onChange={(height) => setSettings({ ...settings, height })}
                />
              </div>

              <label className="grid gap-1 text-[11px] text-muted">
                {t("Studio.sampler")}
                <select
                  className="rounded border border-border bg-background px-2 py-1 font-mono text-xs text-foreground"
                  value={settings.sampler}
                  onChange={(event) => setSettings({ ...settings, sampler: event.target.value })}
                >
                  {/* A model may name a sampler this build does not list; keep
                      it selectable rather than silently switching it. */}
                  {(SAMPLERS.includes(settings.sampler)
                    ? SAMPLERS
                    : [settings.sampler, ...SAMPLERS]
                  ).map((entry) => (
                    <option key={entry} value={entry}>
                      {entry}
                    </option>
                  ))}
                </select>
              </label>

              <div className="grid gap-3 sm:grid-cols-2">
                <SliderField
                  label={t("Studio.steps")}
                  value={settings.steps}
                  min={1}
                  max={150}
                  step={1}
                  onChange={(steps) => setSettings({ ...settings, steps })}
                />
                <SliderField
                  label={t("Studio.guidance")}
                  value={settings.cfgScale}
                  min={0}
                  max={30}
                  step={0.1}
                  onChange={(cfgScale) => setSettings({ ...settings, cfgScale })}
                />
              </div>

              {needsInitImage(task) && (
                <SliderField
                  label={t("Studio.denoise")}
                  value={settings.strength}
                  min={0}
                  max={1}
                  step={0.01}
                  onChange={(strength) => setSettings({ ...settings, strength })}
                />
              )}

              <label className="grid gap-1 text-[11px] text-muted">
                {t("Studio.seed")}
                <span className="flex items-center gap-2">
                  <input
                    className="min-w-0 flex-1 rounded border border-border bg-background px-2 py-1 font-mono text-xs text-foreground"
                    placeholder={t("Studio.seedPlaceholder")}
                    value={seed}
                    inputMode="numeric"
                    onChange={(event) => setSeed(event.target.value.replace(/[^\d-]/g, ""))}
                  />
                  <IconButton
                    size="sm"
                    aria-label={t("Studio.seedShuffle")}
                    onClick={() => setSeed(String(Math.floor(Math.random() * 2_147_483_647)))}
                  >
                    <Shuffle size={12} />
                  </IconButton>
                </span>
              </label>

              <details className="grid gap-2">
                <summary className="cursor-pointer text-[11px] text-muted">
                  {t("Studio.advanced")}
                </summary>
                <div className="mt-2 grid gap-3 sm:grid-cols-2">
                  <label className="grid gap-1 text-[11px] text-muted">
                    {t("Studio.scheduler")}
                    <select
                      className="rounded border border-border bg-background px-2 py-1 font-mono text-xs text-foreground"
                      value={settings.scheduler}
                      onChange={(event) =>
                        setSettings({ ...settings, scheduler: event.target.value })
                      }
                    >
                      <option value="">{t("Studio.engineDefault")}</option>
                      {SCHEDULERS.map((entry) => (
                        <option key={entry} value={entry}>
                          {entry}
                        </option>
                      ))}
                    </select>
                  </label>
                  <SliderField
                    label={t("Studio.clipSkip")}
                    value={settings.clipSkip}
                    min={-1}
                    max={12}
                    step={1}
                    hint={settings.clipSkip < 0 ? t("Studio.engineDefault") : undefined}
                    onChange={(clipSkip) => setSettings({ ...settings, clipSkip })}
                  />
                </div>
              </details>

              <div className="grid gap-2 rounded bg-background/60 p-2">
                <label className="flex items-center gap-2 text-[11px] font-medium">
                  <input
                    type="checkbox"
                    checked={settings.hires !== null}
                    onChange={(event) =>
                      setSettings({
                        ...settings,
                        hires: event.target.checked ? DEFAULT_HIRES : null,
                      })
                    }
                  />
                  {t("Studio.upscale")}
                  {settings.hires && (
                    <span className="font-mono text-[10px] font-normal text-faint">
                      {t("Studio.upscaleTo", {
                        target: `${normalizeDimension(Math.round(settings.width * settings.hires.scale))}×${normalizeDimension(Math.round(settings.height * settings.hires.scale))}`,
                      })}
                    </span>
                  )}
                </label>
                {settings.hires && (
                  <>
                    <div className="flex flex-wrap gap-1.5">
                      {[1.5, 2, 3, 4].map((scale) => (
                        <Button
                          key={scale}
                          size="sm"
                          variant={settings.hires?.scale === scale ? "primary" : "secondary"}
                          onClick={() =>
                            setSettings({ ...settings, hires: { ...DEFAULT_HIRES, ...settings.hires, scale } })
                          }
                        >
                          {scale}x
                        </Button>
                      ))}
                    </div>
                    <label className="grid gap-1 text-[11px] text-muted">
                      {t("Studio.upscaler")}
                      {/* A free field with suggestions, not a closed list: an
                          ESRGAN model in --hires-upscalers-dir is selectable
                          under its own name, which no fixed list can know. */}
                      <input
                        list="studio-upscalers"
                        className="rounded border border-border bg-background px-2 py-1 font-mono text-xs text-foreground"
                        value={settings.hires.upscaler}
                        onChange={(event) =>
                          setSettings({
                            ...settings,
                            hires: { ...DEFAULT_HIRES, ...settings.hires, upscaler: event.target.value },
                          })
                        }
                      />
                      <datalist id="studio-upscalers">
                        {UPSCALERS.map((entry) => (
                          <option key={entry} value={entry} />
                        ))}
                      </datalist>
                      <span className="text-faint">{t("Studio.upscalerHint")}</span>
                    </label>
                    <div className="grid gap-3 sm:grid-cols-2">
                      <SliderField
                        label={t("Studio.hiresSteps")}
                        value={settings.hires.steps}
                        min={0}
                        max={150}
                        step={1}
                        onChange={(steps) =>
                          setSettings({
                            ...settings,
                            hires: { ...DEFAULT_HIRES, ...settings.hires, steps },
                          })
                        }
                      />
                      <SliderField
                        label={t("Studio.denoise")}
                        value={settings.hires.denoisingStrength}
                        min={0}
                        max={1}
                        step={0.01}
                        onChange={(denoisingStrength) =>
                          setSettings({
                            ...settings,
                            hires: { ...DEFAULT_HIRES, ...settings.hires, denoisingStrength },
                          })
                        }
                      />
                    </div>
                  </>
                )}
              </div>
            </div>
          )}

          {!isSpeechTask(task) && (
            <LoraStack
              loras={loras}
              onChange={setLoras}
              showHighNoise={selected.components.some(
                (component) => component.slot === "high_noise_diffusion_model",
              )}
            />
          )}

          <div className="flex items-center gap-2">
            <Button variant="primary" disabled={!canGenerate} onClick={() => void generate()}>
              {busy ? <Loader2 size={14} className="animate-spin" /> : <Sparkles size={14} />}
              {t("Studio.generate")}
            </Button>
            {phase && <span className="text-[11px] text-muted">{phase}</span>}
            {busy && (
              <span className="flex min-w-0 flex-1 items-center gap-2">
                <span className="h-1 min-w-16 flex-1 overflow-hidden rounded-full bg-surface-2">
                  <span
                    className={`block h-full bg-accent ${
                      percent === null ? "w-1/3 animate-pulse" : "transition-[width]"
                    }`}
                    style={percent === null ? undefined : { width: `${percent}%` }}
                  />
                </span>
                {percent !== null && (
                  <span className="shrink-0 font-mono text-[11px] text-muted">{percent}%</span>
                )}
              </span>
            )}
            {status?.loadedModelId && (
              <Button
                size="sm"
                variant="secondary"
                onClick={() => void studioClient.unloadEngine().then(refresh)}
              >
                {t("Studio.unload")}
              </Button>
            )}
          </div>
        </section>
      )}

      {mode !== "models" && (
      <section>
        <h2 className="mb-2 text-xs font-medium text-muted">{t("Studio.gallery")}</h2>
        {gallery.length === 0 ? (
          <p className="text-xs text-faint">{t("Studio.galleryEmpty")}</p>
        ) : (
          <div className="grid gap-3 sm:grid-cols-2">
            {gallery
              .filter((entry) => tasksFor(mode).includes(entry.task))
              .map((entry, index) => {
              const preview = previews[entry.artifactId];
              return (
                <figure
                  key={entry.entryId}
                  // The newest result is the one the user is waiting on, so it
                  // gets the full width rather than sharing a row.
                  className={`rounded border border-border p-2 ${index === 0 ? "sm:col-span-2" : ""}`}
                >
                  {preview ? (
                    entry.mediaType.startsWith("video/") ? (
                      <video
                        controls
                        loop
                        src={preview}
                        className="w-full rounded bg-black"
                      />
                    ) : entry.mediaType.startsWith("audio/") ? (
                      <audio controls src={preview} className="w-full" />
                    ) : (
                      <img src={preview} alt={entry.prompt} className="w-full rounded" />
                    )
                  ) : (
                    <Button size="sm" variant="secondary" onClick={() => void loadPreview(entry)}>
                      {t("Studio.loadPreview")}
                    </Button>
                  )}
                  <figcaption className="mt-2 text-[11px] text-faint">
                    <span className="line-clamp-2 block text-muted">{entry.prompt}</span>
                    {entry.modelId}
                    {entry.width > 0 && ` · ${entry.width}×${entry.height}`}
                    {entry.frameCount > 1 &&
                      ` · ${(entry.durationMs / 1000).toFixed(1)}s`}
                  </figcaption>
                  {preview && (
                    <div className="mt-2 flex flex-wrap items-center gap-1.5">
                      {/* Editing a generated asset means generating from it —
                          the result becomes the next run's starting frame. */}
                      {!entry.mediaType.startsWith("audio/") && selected && editTaskFor(selected) && (
                        <Button size="sm" variant="secondary" onClick={() => void editEntry(entry)}>
                          <Wand2 size={12} />
                          {t("Studio.result.edit")}
                        </Button>
                      )}
                      <a
                        className="inline-flex items-center gap-1 rounded border border-border px-2 py-1 text-[11px] text-muted hover:text-foreground"
                        href={preview}
                        download={`${entry.entryId}.${extensionFor(entry.mediaType)}`}
                      >
                        <Download size={12} />
                        {t("Studio.result.save")}
                      </a>
                    </div>
                  )}
                </figure>
              );
            })}
          </div>
        )}
      </section>
      )}
    </div>
  );
}

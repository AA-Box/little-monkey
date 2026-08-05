import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { confirm } from "@tauri-apps/plugin-dialog";
import {
  ChevronDown,
  Download,
  Loader2,
  RectangleHorizontal,
  RectangleVertical,
  Shuffle,
  Sparkles,
  Square,
  Trash2,
  Upload,
  Wand2,
} from "lucide-react";

import { Button, IconButton, Listbox, StatusPill, Tabs } from "../ui";
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
          className="w-16 shrink-0 rounded border border-border bg-background px-1.5 py-1 text-center text-xs text-foreground"
        />
      </span>
    </label>
  );
}

function Select({
  value,
  onChange,
  children,
  mono,
}: {
  value: string;
  onChange: (value: string) => void;
  children: React.ReactNode;
  mono?: boolean;
}) {
  return (
    <span className="relative block">
      <select
        className={`w-full appearance-none rounded-md border border-border bg-background py-1.5 pl-2.5 pr-7 text-xs text-foreground outline-none focus:ring-1 focus:ring-accent ${
          mono ? "font-mono" : ""
        }`}
        value={value}
        onChange={(event) => onChange(event.target.value)}
      >
        {children}
      </select>
      <ChevronDown
        size={12}
        className="pointer-events-none absolute right-2 top-1/2 -translate-y-1/2 text-muted"
      />
    </span>
  );
}

/** The shape each preset makes, which is the whole of what the button says. */
const ASPECT_ICONS = {
  portrait: RectangleVertical,
  landscape: RectangleHorizontal,
  square: Square,
} as const;

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
  const [mode, setMode] = useState<StudioMode>("image");
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
  // The engine names the job in every progress event; without keeping it there
  // is nothing to cancel.
  const [jobId, setJobId] = useState<string | null>(null);
  const stopped = useRef(false);
  const [error, setError] = useState<string | null>(null);
  const [previews, setPreviews] = useState<Record<string, string>>({});
  const [lightbox, setLightbox] = useState<GenerationEntry | null>(null);
  const fileInput = useRef<HTMLInputElement>(null);
  const lightboxRef = useRef<HTMLDialogElement>(null);

  // `showModal` is the only way into the top layer, so open and close are
  // driven from state rather than the `open` attribute.
  useEffect(() => {
    const dialog = lightboxRef.current;
    if (!dialog) return;
    if (lightbox) dialog.showModal();
    else dialog.close();
  }, [lightbox]);

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
  // This tab's results, newest first: the newest fills the canvas, the rest is
  // the strip under it.
  const shownGallery = useMemo(
    () => gallery.filter((entry) => tasksFor(mode).includes(entry.task)),
    [gallery, mode],
  );
  const shown = shownGallery[0] ?? null;
  const shownHistory = shownGallery.slice(1);

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
      setJobId(payload.jobId || null);
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

  // The canvas has to have something in it the moment a tab opens.
  useEffect(() => {
    if (shown) void loadPreview(shown);
  }, [shown, loadPreview]);

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
    stopped.current = false;
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
      if (!stopped.current) setError(errorText(reason));
    } finally {
      setBusy(false);
      setPhase(null);
      setPercent(null);
      setJobId(null);
      // The engine holds the model after a run, so whether it is loaded is
      // only knowable by asking again — and Free memory is what that answer
      // turns on.
      void refresh();
    }
  };

  /**
   * Stops the run in flight.
   *
   * The engine drops a queued job but cannot interrupt one already sampling
   * (`cancel_generating: false` in its own capabilities), so stopping a
   * running generation means stopping the engine running it. That also
   * releases its weights, which is what the user wanted from a stop anyway.
   */
  const stop = async () => {
    stopped.current = true;
    setPhase(t("Studio.phase.stopping"));
    try {
      if (!jobId || !(await studioClient.cancel(jobId))) {
        await studioClient.unloadEngine();
      }
    } catch (reason) {
      setError(errorText(reason));
    }
  };

  /** Deletes a generation and its bytes. Irreversible — the store keeps no
   *  history — so it asks first. */
  const removeEntry = async (entry: GenerationEntry) => {
    if (!(await confirm(t("Studio.result.deleteConfirm"), { kind: "warning" }))) return;
    setLightbox(null);
    try {
      await studioClient.deleteEntry(entry.entryId);
      setGallery((current) => current.filter((item) => item.entryId !== entry.entryId));
      setPreviews((current) => {
        const next = { ...current };
        delete next[entry.artifactId];
        return next;
      });
    } catch (reason) {
      setError(errorText(reason));
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
    <div
      className={`flex min-h-0 flex-1 flex-col p-4 ${
        mode === "models" ? "overflow-y-auto" : "overflow-hidden"
      }`}
    >
      <Tabs
        active={mode}
        onChange={(next) => setMode(next as StudioMode)}
        tabs={[
          { id: "image", label: t("Studio.tab.image") },
          { id: "video", label: t("Studio.tab.video") },
          { id: "audio", label: t("Studio.tab.audio") },
          { id: "models", label: t("Studio.tab.models") },
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
            {/* A native popup is drawn by the platform and sized to its widest
                option, never to the control, so this one is ours. */}
            <Listbox
              value={selectedId ?? ""}
              ariaLabel={t("Studio.models")}
              placeholder={t("Studio.noneForTab")}
              onChange={(next) => setSelectedId(next || null)}
              options={visible.map((model) => ({
                value: model.id,
                label: model.name,
                detail: [
                  model.family,
                  formatBytes(model.totalBytes),
                  model.installed ? "" : t("Studio.notDownloaded"),
                ]
                  .filter(Boolean)
                  .join(" · "),
              }))}
            />
          </label>
        </section>
      )}

      {/* The shape every local generation tool settles on: a narrow rail of
          controls that scrolls on its own, and a canvas that keeps the prompt,
          the button and the result together where the work happens. */}
      {mode !== "models" && (
      <div className="flex min-h-0 flex-1 gap-4 overflow-hidden p-1">
        {/* `overflow-y: auto` computes overflow-x to auto as well, so the rail
            clips sideways too and its controls need room for a focus ring. */}
        {selected && (
        <aside className="grid w-72 shrink-0 content-start gap-3 overflow-y-auto px-1 pb-4">
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

          {isSpeechTask(task) && (
            <div className="grid gap-3">
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
            <details open className="rounded border border-border p-3">
              <summary className="cursor-pointer text-xs font-medium">
                {t("Studio.settings")}
              </summary>
              <div className="mt-3 grid gap-3">

              <div className="grid gap-1 text-[11px] text-muted">
                {t("Studio.aspect")}
                <div className="flex gap-1.5">
                  {ASPECT_PRESETS.map((preset) => {
                    const Icon = ASPECT_ICONS[preset.id as keyof typeof ASPECT_ICONS];
                    return (
                      <IconButton
                        key={preset.id}
                        size="sm"
                        variant={
                          settings.width === preset.width && settings.height === preset.height
                            ? "active"
                            : "secondary"
                        }
                        aria-label={t(`Studio.aspect.${preset.id}`)}
                        title={`${t(`Studio.aspect.${preset.id}`)} · ${preset.width}×${preset.height}`}
                        onClick={() =>
                          setSettings({ ...settings, width: preset.width, height: preset.height })
                        }
                      >
                        <Icon size={13} />
                      </IconButton>
                    );
                  })}
                </div>
              </div>

              <div className="grid gap-3">
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
                <Select
                  mono
                  value={settings.sampler}
                  onChange={(sampler) => setSettings({ ...settings, sampler })}
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
                </Select>
              </label>

              <div className="grid gap-3">
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
                <div className="mt-2 grid gap-3">
                  <label className="grid gap-1 text-[11px] text-muted">
                    {t("Studio.scheduler")}
                    <Select
                      mono
                      value={settings.scheduler}
                      onChange={(scheduler) => setSettings({ ...settings, scheduler })}
                    >
                      <option value="">{t("Studio.engineDefault")}</option>
                      {SCHEDULERS.map((entry) => (
                        <option key={entry} value={entry}>
                          {entry}
                        </option>
                      ))}
                    </Select>
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
                    <div className="grid gap-3">
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
            </details>
          )}

          {!isSpeechTask(task) && (
            <details className="rounded border border-border p-3">
              <summary className="cursor-pointer text-xs font-medium">
                {t("Studio.lora.title")}
              </summary>
              <div className="mt-3">
                <LoraStack
                  loras={loras}
                  onChange={setLoras}
                  showHighNoise={selected.components.some(
                    (component) => component.slot === "high_noise_diffusion_model",
                  )}
                />
              </div>
            </details>
          )}

        </aside>
        )}

        <section className="flex min-h-0 flex-1 flex-col gap-2">
          {/* Each button sits with the field it belongs to and stretches to
              its height, so the row reads as one control rather than two. */}
          <div className="flex shrink-0 items-stretch gap-2">
            <textarea
              className="min-h-16 min-w-0 flex-1 resize-none rounded border border-border bg-background p-2 text-xs"
              placeholder={t("Studio.promptPlaceholder")}
              value={prompt}
              onChange={(event) => setPrompt(event.target.value)}
            />
            <Button
              variant="primary"
              className="h-auto shrink-0 flex-col px-4"
              disabled={!canGenerate}
              onClick={() => void generate()}
            >
              {busy ? <Loader2 size={14} className="animate-spin" /> : <Sparkles size={14} />}
              {t("Studio.generate")}
            </Button>
          </div>

          {!isSpeechTask(task) && (
            <div className="flex shrink-0 items-stretch gap-2">
              <input
                className="min-w-0 flex-1 rounded border border-border bg-background p-2 text-xs"
                placeholder={t("Studio.negativePlaceholder")}
                value={negativePrompt}
                onChange={(event) => setNegativePrompt(event.target.value)}
              />
              <Button
                variant="secondary"
                className="h-auto shrink-0"
                disabled={!status?.loadedModelId}
                title={status?.loadedModelId ?? t("Studio.unloadIdle")}
                onClick={() => void studioClient.unloadEngine().then(refresh)}
              >
                {t("Studio.unload")}
              </Button>
            </div>
          )}

          {busy && (
            <div className="flex shrink-0 items-center gap-2">
              <Button size="sm" variant="secondary" onClick={() => void stop()}>
                <Square size={12} />
                {t("Studio.stop")}
              </Button>
              {phase && <span className="shrink-0 text-[11px] text-muted">{phase}</span>}
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
            </div>
          )}

          {/* The canvas: whatever was made last, as large as the pane allows.
              It is the thing being worked on, so it gets the space.

              `absolute inset-0` is load-bearing. A percentage max-height only
              resolves against a definite height, and a centred grid or flex
              item is sized by its content — so `max-h-full` on the image was
              silently doing nothing and a tall result rendered at full size
              behind `overflow-hidden`, which looks exactly like a blank pane. */}
          <div className="relative min-h-0 flex-1 overflow-hidden rounded border border-border bg-background/40">
            {!shown ? (
              <p className="absolute inset-0 grid place-items-center text-xs text-faint">
                {busy ? t("Studio.phase.running") : t("Studio.galleryEmpty")}
              </p>
            ) : !previews[shown.artifactId] ? (
              // Something was made but its bytes are not here yet. Saying
              // "nothing generated yet" in this state is a lie, and it is the
              // lie that reads as "it finished and showed me nothing".
              <div className="absolute inset-0 grid place-items-center">
                <Button size="sm" variant="secondary" onClick={() => void loadPreview(shown)}>
                  <Loader2 size={13} className="animate-spin" />
                  {t("Studio.loadPreview")}
                </Button>
              </div>
            ) : shown.mediaType.startsWith("audio/") ? (
              <div className="absolute inset-0 grid place-items-center p-2">
                <audio controls src={previews[shown.artifactId]} className="w-full max-w-md" />
              </div>
            ) : (
              <button
                type="button"
                className="absolute inset-0 flex cursor-zoom-in items-center justify-center p-2"
                title={t("Studio.result.expand")}
                onClick={() => setLightbox(shown)}
              >
                {shown.mediaType.startsWith("video/") ? (
                  <video
                    controls
                    loop
                    src={previews[shown.artifactId]}
                    className="max-h-full max-w-full rounded bg-black object-contain"
                  />
                ) : (
                  <img
                    src={previews[shown.artifactId]}
                    alt={shown.prompt}
                    className="max-h-full max-w-full rounded object-contain"
                  />
                )}
              </button>
            )}
          </div>

          {/* Everything made before, as a strip rather than a wall. */}
          {shownHistory.length > 0 && (
            <div className="flex shrink-0 gap-2 overflow-x-auto pb-1">
              {shownHistory.map((entry) => {
                const preview = previews[entry.artifactId];
                return (
                  <button
                    key={entry.entryId}
                    type="button"
                    title={entry.prompt}
                    className="h-16 w-16 shrink-0 overflow-hidden rounded border border-border transition hover:border-accent"
                    onClick={() => (preview ? setLightbox(entry) : void loadPreview(entry))}
                  >
                    {!preview ? (
                      <span className="grid h-full place-items-center text-[10px] text-faint">
                        {t("Studio.loadPreview")}
                      </span>
                    ) : entry.mediaType.startsWith("video/") ? (
                      <video src={preview} muted className="h-full w-full bg-black object-cover" />
                    ) : entry.mediaType.startsWith("audio/") ? (
                      <span className="grid h-full place-items-center text-[10px] text-faint">
                        ♪
                      </span>
                    ) : (
                      <img src={preview} alt={entry.prompt} className="h-full w-full object-cover" />
                    )}
                  </button>
                );
              })}
            </div>
          )}
        </section>
      </div>
      )}

      {/* Full size on demand. `<dialog>` brings the top layer, the backdrop and
          Esc-to-close with it, so none of that is reimplemented here. */}
      <dialog
        ref={lightboxRef}
        onClose={() => setLightbox(null)}
        // A native dialog does not close on a backdrop click. The backdrop is
        // the dialog's own box outside its content, so this is that click.
        onClick={(event) => {
          if (event.target === lightboxRef.current) setLightbox(null);
        }}
        className="max-h-[92vh] max-w-[92vw] rounded-lg border border-border bg-surface p-3 text-foreground backdrop:bg-black/70"
      >
        {lightbox && previews[lightbox.artifactId] && (
          <div className="grid gap-2">
            {lightbox.mediaType.startsWith("video/") ? (
              <video
                controls
                loop
                autoPlay
                src={previews[lightbox.artifactId]}
                className="max-h-[74vh] rounded bg-black object-contain"
              />
            ) : (
              <img
                src={previews[lightbox.artifactId]}
                alt={lightbox.prompt}
                className="max-h-[74vh] rounded object-contain"
              />
            )}
            <p className="max-w-prose text-[11px] text-muted">{lightbox.prompt}</p>
            <div className="flex flex-wrap items-center gap-1.5 text-[11px] text-faint">
              <span className="mr-auto">
                {lightbox.modelId}
                {lightbox.width > 0 && ` · ${lightbox.width}×${lightbox.height}`}
                {lightbox.frameCount > 1 && ` · ${(lightbox.durationMs / 1000).toFixed(1)}s`}
              </span>
              {/* Editing a generated asset means generating from it — the
                  result becomes the next run's starting frame. */}
              {selected && editTaskFor(selected) && (
                <Button
                  size="sm"
                  variant="secondary"
                  onClick={() => {
                    void editEntry(lightbox);
                    setLightbox(null);
                  }}
                >
                  <Wand2 size={12} />
                  {t("Studio.result.edit")}
                </Button>
              )}
              <a
                className="inline-flex items-center gap-1 rounded border border-border px-2 py-1 text-muted hover:text-foreground"
                href={previews[lightbox.artifactId]}
                download={`${lightbox.entryId}.${extensionFor(lightbox.mediaType)}`}
              >
                <Download size={12} />
                {t("Studio.result.save")}
              </a>
              <Button size="sm" variant="danger" onClick={() => void removeEntry(lightbox)}>
                <Trash2 size={12} />
                {t("Studio.result.delete")}
              </Button>
              <Button size="sm" variant="secondary" onClick={() => setLightbox(null)}>
                {t("Studio.result.close")}
              </Button>
            </div>
          </div>
        )}
      </dialog>
    </div>
  );
}

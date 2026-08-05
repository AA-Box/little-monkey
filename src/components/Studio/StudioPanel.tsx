import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Download, Loader2, Sparkles, Square, Trash2, Upload } from "lucide-react";

import { Button, IconButton, StatusPill, Tabs } from "../ui";
import { AddModelForm } from "./AddModelForm";
import { LoraStack } from "./LoraStack";
import { useT } from "../../lib/i18n";
import {
  componentFileName,
  formatBytes,
  isVideoTask,
  needsInitImage,
  normalizeDimension,
  normalizeVideoFrames,
  studioClient,
  type GenerationEngineStatus,
  type GenerationEntry,
  type GenerationModel,
  type GenerationTask,
  type LoraSelection,
} from "../../lib/studioClient";

function errorText(reason: unknown): string {
  return reason instanceof Error ? reason.message : String(reason);
}

/** A model's own file basenames, for the "what will this download" list. */
function componentNames(model: GenerationModel): string[] {
  return model.components.map(componentFileName);
}

export type StudioMode = "image" | "video" | "audio";

const MODE_TASKS: Record<StudioMode, GenerationTask[]> = {
  image: ["text_to_image", "image_to_image"],
  video: ["text_to_video", "image_to_video"],
  audio: ["text_to_speech"],
};

export function StudioPanel() {
  const { t } = useT();
  const [mode, setMode] = useState<StudioMode>("image");
  const [status, setStatus] = useState<GenerationEngineStatus | null>(null);
  const [models, setModels] = useState<GenerationModel[]>([]);
  const [gallery, setGallery] = useState<GenerationEntry[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [task, setTask] = useState<GenerationTask>(MODE_TASKS[mode][0]);
  const [prompt, setPrompt] = useState("");
  const [negativePrompt, setNegativePrompt] = useState("");
  const [seconds, setSeconds] = useState(3);
  const [seed, setSeed] = useState(-1);
  const [initImage, setInitImage] = useState<string | null>(null);
  const [loras, setLoras] = useState<LoraSelection[]>([]);
  const [adding, setAdding] = useState(false);
  const [busy, setBusy] = useState(false);
  const [downloadingId, setDownloadingId] = useState<string | null>(null);
  const [phase, setPhase] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [previews, setPreviews] = useState<Record<string, string>>({});
  const fileInput = useRef<HTMLInputElement>(null);

  // A segment shows only the models that can do something in it, so the
  // picker never offers a video model under Image.
  const visible = useMemo(
    () => models.filter((model) => model.tasks.some((entry) => MODE_TASKS[mode].includes(entry))),
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
        model.tasks.some((entry) => MODE_TASKS[mode].includes(entry)),
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
          : t(`Studio.phase.${payload.phase}`),
      );
    });
    return () => {
      void unlisten.then((stop) => stop());
    };
  }, [t]);

  // Keep the task valid for whichever model is selected: switching from a
  // video model to an image-only one must not leave a video task armed.
  useEffect(() => {
    const allowed = selected
      ? selected.tasks.filter((entry) => MODE_TASKS[mode].includes(entry))
      : MODE_TASKS[mode];
    if (!allowed.includes(task)) setTask(allowed[0] ?? MODE_TASKS[mode][0]);
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
    if (!selected) return;
    setError(null);
    setBusy(true);
    setPhase(t("Studio.phase.submitted"));
    try {
      const entry = await studioClient.run({
        modelId: selected.id,
        task,
        prompt,
        negativePrompt,
        width: normalizeDimension(selected.defaults.width),
        height: normalizeDimension(selected.defaults.height),
        steps: selected.defaults.steps,
        cfgScale: selected.defaults.cfgScale,
        seed,
        videoFrames: isVideoTask(task)
          ? normalizeVideoFrames(selected.defaults.frameGrid, seconds * selected.defaults.fps)
          : 1,
        fps: isVideoTask(task) ? selected.defaults.fps : 1,
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
    }
  };

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
                      {model.family} · {formatBytes(model.components.reduce((sum, c) => sum + c.sizeBytes, 0))}
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

      {selected && (
        <section className="mb-4 grid gap-3">
          <div className="flex flex-wrap gap-1.5">
            {selected.tasks
              .filter((entry) => MODE_TASKS[mode].includes(entry))
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
          <input
            className="w-full rounded border border-border bg-background p-2 text-xs"
            placeholder={t("Studio.negativePlaceholder")}
            value={negativePrompt}
            onChange={(event) => setNegativePrompt(event.target.value)}
          />

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

          <LoraStack
            loras={loras}
            onChange={setLoras}
            showHighNoise={selected.components.some(
              (component) => component.slot === "high_noise_diffusion_model",
            )}
          />

          <label className="flex items-center gap-3 text-xs">
            <span className="w-24 shrink-0 text-muted">{t("Studio.seed")}</span>
            <input
              type="number"
              value={seed}
              onChange={(event) => setSeed(Number(event.target.value))}
              className="w-32 rounded border border-border bg-background px-2 py-1 text-xs"
            />
            <span className="text-[11px] text-faint">{t("Studio.seedHint")}</span>
          </label>

          <div className="flex items-center gap-2">
            <Button variant="primary" disabled={!canGenerate} onClick={() => void generate()}>
              {busy ? <Loader2 size={14} className="animate-spin" /> : <Sparkles size={14} />}
              {t("Studio.generate")}
            </Button>
            {phase && <span className="text-[11px] text-muted">{phase}</span>}
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

      <section>
        <h2 className="mb-2 text-xs font-medium text-muted">{t("Studio.gallery")}</h2>
        {gallery.length === 0 ? (
          <p className="text-xs text-faint">{t("Studio.galleryEmpty")}</p>
        ) : (
          <div className="grid gap-3 sm:grid-cols-2">
            {gallery
              .filter((entry) => MODE_TASKS[mode].includes(entry.task))
              .map((entry) => {
              const preview = previews[entry.artifactId];
              return (
                <figure key={entry.entryId} className="rounded border border-border p-2">
                  {preview ? (
                    entry.mediaType.startsWith("video/") ? (
                      <video
                        controls
                        loop
                        src={preview}
                        className="w-full rounded bg-black"
                      />
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
                    {entry.modelId} · {entry.width}×{entry.height}
                    {entry.frameCount > 1 &&
                      ` · ${(entry.durationMs / 1000).toFixed(1)}s`}
                  </figcaption>
                </figure>
              );
            })}
          </div>
        )}
      </section>
    </div>
  );
}

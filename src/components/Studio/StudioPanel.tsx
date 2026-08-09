import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { confirm, open } from "@tauri-apps/plugin-dialog";
import {
  ArrowDown,
  ArrowLeft,
  ArrowRight,
  ArrowUp,
  ChevronDown,
  Download,
  Loader2,
  Plus,
  RectangleHorizontal,
  RectangleVertical,
  Redo2,
  Shuffle,
  Sparkles,
  Square,
  Trash2,
  Undo2,
  Upload,
  Wand2,
} from "lucide-react";

import { Button, IconButton, Listbox, StatusPill } from "../ui";
import { AddBackendForm } from "./AddBackendForm";
import { AddModelForm } from "./AddModelForm";
import { LoraStack } from "./LoraStack";
import { MaskCanvas } from "./MaskCanvas";
import { ModelFiles } from "./ModelFiles";
import { SettingsCard } from "./SettingsCard";
import type { StudioMode } from "./StudioNav";
import { ToolPanel } from "./ToolPanel";
import { useT } from "../../lib/i18n";
import { describeWeightFile } from "../../lib/weightFileHints";
import { PREPROCESSORS, runPreprocessor, type Preprocessor } from "../../lib/preprocess";
import { NO_MARGINS, runOutpaint, type Margins } from "../../lib/outpaint";
import { pickImageBase64 } from "../../lib/imageAttachment";
import {
  componentFileName,
  editTaskFor,
  formatBytes,
  isSpeechTask,
  isVideoTask,
  needsInitImage,
  normalizeDimension,
  normalizeVideoFrames,
  toSpec,
  ASPECT_PRESETS,
  MAX_BATCH_COUNT,
  MAX_REF_IMAGES,
  SAMPLERS,
  SCHEDULERS,
  studioClient,
  UPSCALERS,
  engineSupports,
  type EngineCapabilities,
  type GenerationEngineStatus,
  type GenerationEntry,
  type GenerationModel,
  type GenerationTask,
  type HiresSettings,
  type LoraAsset,
  type LoraSelection,
  type ModelComponent,
  availableConditioning,
  choosableSlots,
  COMPONENT_SLOTS,
  type ComponentOverride,
  type ComponentSlot,
  type ConditioningImage,
  type PartAsset,
  backendModels,
  isRemoteModelId,
  type RemoteBackend,
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
        <span className="truncate">{label}</span>
        {hint && <span className="shrink-0 font-mono text-faint">{hint}</span>}
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

/** A thumbnail of a chosen conditioning image, small enough to sit inline in
 *  the controls column. */
function Thumbnail({ base64, alt }: { base64: string; alt: string }) {
  return (
    <img
      src={`data:image/png;base64,${base64}`}
      alt={alt}
      className="h-12 w-12 shrink-0 rounded border border-border object-cover"
    />
  );
}

/** One conditioning image plus how strongly it applies. Shared by the control
 *  image and the IP-Adapter reference, which differ only in wording — the two
 *  are read by different weights but chosen the same way. */
function ConditioningImageField({
  label,
  hint,
  value,
  onPick,
  onClear,
  strength,
  onStrength,
  strengthLabel,
  onPreprocess,
}: {
  label: string;
  hint: string;
  value: string | null;
  onPick: () => void;
  onClear: () => void;
  strength: number;
  onStrength: (value: number) => void;
  strengthLabel: string;
  /** Offered only where a hint map is what the slot wants. ControlNet takes an
   *  edge or depth map rather than a photograph; IP-Adapter and PhotoMaker take
   *  the picture itself, and running an edge detector over those would throw
   *  away the very thing they read. */
  onPreprocess?: (kind: Preprocessor) => void;
}) {
  const { t } = useT();
  return (
    <SettingsCard title={label} hint={hint}>
      <div className="flex items-center gap-2">
        {value && <Thumbnail base64={value} alt={label} />}
        <Button size="sm" variant="secondary" onClick={onPick}>
          <Upload size={13} />
          {t("Studio.chooseImage")}
        </Button>
        {value && (
          <IconButton size="sm" aria-label={t("Studio.clearImage")} onClick={onClear}>
            <Trash2 size={12} />
          </IconButton>
        )}
      </div>
      {value && onPreprocess && (
        <label className="grid gap-1">
          <span className="text-[11px] font-medium text-muted">
            {t("Studio.preprocess.label")}
          </span>
          <Listbox
            ariaLabel={t("Studio.preprocess.label")}
            value=""
            placeholder={t("Studio.preprocess.placeholder")}
            options={PREPROCESSORS.filter((kind) => kind !== "none").map((kind) => ({
              value: kind,
              label: t(`Studio.preprocess.${kind}`),
            }))}
            onChange={(kind) => onPreprocess(kind as Preprocessor)}
          />
        </label>
      )}
      {value && (
        <SliderField
          label={strengthLabel}
          value={strength}
          min={0}
          max={1}
          step={0.05}
          onChange={onStrength}
        />
      )}
    </SettingsCard>
  );
}

/** The reference-image list for the identity- and edit-conditioned models,
 *  which take several rather than one. */
function ReferenceImages({
  images,
  onAdd,
  onRemove,
  numbered,
  onNumberedChange,
}: {
  images: string[];
  onAdd: () => void;
  onRemove: (index: number) => void;
  numbered: boolean;
  onNumberedChange: (numbered: boolean) => void;
}) {
  const { t } = useT();
  const full = images.length >= MAX_REF_IMAGES;
  // One reference has nothing to be told apart from, so the control only earns
  // its space once there are two. The badges are the point of showing it: they
  // are the numbers the prompt refers to, so the setting's effect is visible
  // rather than something the user has to take on trust.
  const canNumber = images.length > 1;
  return (
    <SettingsCard title={t("Studio.reference.title")} hint={t("Studio.reference.hint")}>
      <div className="flex flex-wrap items-center gap-2">
        {images.map((image, index) => (
          <span key={`${index}-${image.slice(0, 16)}`} className="relative">
            <Thumbnail
              base64={image}
              alt={
                canNumber && numbered
                  ? t("Studio.reference.numberedAlt", { index: String(index + 1) })
                  : t("Studio.reference.title")
              }
            />
            {canNumber && numbered && (
              <span
                aria-hidden="true"
                className="absolute -bottom-1 -left-1 flex h-4 min-w-4 items-center justify-center rounded-full border border-border bg-surface-2 px-1 font-mono text-[10px] font-medium text-foreground"
              >
                {index + 1}
              </span>
            )}
            <IconButton
              size="sm"
              className="absolute -right-1 -top-1"
              aria-label={t("Studio.reference.remove")}
              onClick={() => onRemove(index)}
            >
              <Trash2 size={10} />
            </IconButton>
          </span>
        ))}
        <Button size="sm" variant="secondary" disabled={full} onClick={onAdd}>
          <Plus size={13} />
          {t("Studio.reference.add")}
        </Button>
      </div>
      {canNumber && (
        <label className="flex min-h-11 items-center gap-2 text-xs text-muted">
          <input
            type="checkbox"
            checked={numbered}
            onChange={(event) => onNumberedChange(event.target.checked)}
            className="h-4 w-4 rounded border-border accent-[var(--color-accent)]"
          />
          {t("Studio.reference.numbered")}
        </label>
      )}
      {/* Only the two stateful lines stay visible — that the list is full, or
          what numbering did. The plain explanation is on the card's info icon,
          where it is not re-read on every glance. */}
      {full || (canNumber && numbered) ? (
        <p className="text-[11px] text-faint">
          {full
            ? t("Studio.reference.full", { max: String(MAX_REF_IMAGES) })
            : t("Studio.reference.numberedHint")}
        </p>
      ) : null}
    </SettingsCard>
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

/** Which tasks each section covers. The two library sections generate
 *  nothing, so neither has an entry and neither filters the model list. */
const MODE_TASKS: Record<Exclude<StudioMode, "models" | "tools">, GenerationTask[]> = {
  image: ["text_to_image", "image_to_image"],
  video: ["text_to_video", "image_to_video"],
  audio: ["text_to_speech"],
};

/** How far one press extends. Multiples of 64 so a picture that was already a
 *  valid size stays one — the engine works in 64-pixel blocks, and an odd
 *  margin would have it resize the result behind the user's back. */
const OUTPAINT_STEPS = [64, 128, 256] as const;

/** One point in the extension history: the picture and the size the form was
 *  set to while it was the picture. Both, because an extension moves the
 *  requested size with it and restoring one without the other hands the engine
 *  a mismatch. */
interface OutpaintState {
  image: string;
  width: number;
  height: number;
}

const OUTPAINT_SIDES = [
  { side: "left", labelKey: "Studio.outpaint.left", icon: ArrowLeft },
  { side: "right", labelKey: "Studio.outpaint.right", icon: ArrowRight },
  { side: "top", labelKey: "Studio.outpaint.up", icon: ArrowUp },
  { side: "bottom", labelKey: "Studio.outpaint.down", icon: ArrowDown },
] as const satisfies readonly { side: keyof Margins; labelKey: string; icon: unknown }[];

const tasksFor = (mode: StudioMode): GenerationTask[] =>
  mode === "models" || mode === "tools" ? [] : MODE_TASKS[mode];

/** Studio talks to the engine over Tauri commands, which only exist inside the
 *  desktop window. In a plain browser tab every call throws a bare TypeError
 *  about `invoke`, which says nothing useful — detect it and say the real
 *  thing instead. */
const IN_DESKTOP_APP =
  typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

interface Props {
  /** Which section is showing. Owned by `App`, because the control that
   *  switches it is the sidebar nav rather than anything in here. */
  mode: StudioMode;
  /** Sidebar node to render the settings rail into. Null until the sidebar has
   *  mounted, and in Chat, where there is no rail to show. */
  railSlot: HTMLElement | null;
}

export function StudioPanel({ mode, railSlot }: Props) {
  const { t } = useT();
  const [status, setStatus] = useState<GenerationEngineStatus | null>(null);
  /** What the running engine says it supports. Null until one has run — the
   *  pickers fall back to the compiled-in lists until then. */
  const [capabilities, setCapabilities] = useState<EngineCapabilities | null>(null);
  const [models, setModels] = useState<GenerationModel[]>([]);
  const [backends, setBackends] = useState<RemoteBackend[]>([]);
  const [addingBackend, setAddingBackend] = useState(false);
  const [gallery, setGallery] = useState<GenerationEntry[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [task, setTask] = useState<GenerationTask>("text_to_image");
  const [prompt, setPrompt] = useState("");
  const [negativePrompt, setNegativePrompt] = useState("");
  const [seconds, setSeconds] = useState(3);
  // A string rather than a number so "empty means random" is expressible, the
  // way every other generation tool spells it.
  const [seed, setSeed] = useState("");
  /** Images sampled from one prompt. The engine runs them serially, so this
   *  multiplies the wait as well as the output. */
  const [batchCount, setBatchCount] = useState(1);
  const [initImage, setInitImage] = useState<string | null>(null);
  const [adPrompt, setAdPrompt] = useState("");
  const [adNegativePrompt, setAdNegativePrompt] = useState("");
  /** Inpainting: white is repainted, black is kept. Only ever set while an
   *  init image exists, because it is a mask *over* that image. */
  const [maskImage, setMaskImage] = useState<string | null>(null);
  const [outpaintStep, setOutpaintStep] = useState<number>(OUTPAINT_STEPS[1]);
  const [extending, setExtending] = useState(false);
  /** One entry per extension, so a mis-aimed arrow is undoable, and one per
   *  undo so it is redoable. The mask is not kept in either: it was generated
   *  for the step being stepped over, and the image is handed back unmasked.
   *
   *  A fresh extension drops the redo stack — the branch it belonged to is
   *  gone, and offering to "redo" onto a different image would paste the wrong
   *  picture back. */
  const [outpaintHistory, setOutpaintHistory] = useState<OutpaintState[]>([]);
  const [outpaintFuture, setOutpaintFuture] = useState<OutpaintState[]>([]);
  /** Structure to follow — already a depth map, pose skeleton or edge map. The
   *  engine runs no detector, so a plain photo is followed as if it were one. */
  const [controlImage, setControlImage] = useState<string | null>(null);
  const [controlStrength, setControlStrength] = useState(0.9);
  /** Style/content to borrow, read through the IP-Adapter. */
  const [ipAdapterImage, setIpAdapterImage] = useState<string | null>(null);
  const [ipAdapterStrength, setIpAdapterStrength] = useState(1);
  /** Subjects to keep consistent, for the identity-conditioned architectures. */
  const [refImages, setRefImages] = useState<string[]>([]);
  /** Whether each reference gets its own index, so a prompt can address them
   *  individually ("the jacket from image 2"). Only meaningful past one. */
  const [numberRefImages, setNumberRefImages] = useState(false);
  const [speakerFile, setSpeakerFile] = useState("");
  const [language, setLanguage] = useState("");
  const [loras, setLoras] = useState<LoraSelection[]>([]);
  const [loraLibrary, setLoraLibrary] = useState<LoraAsset[]>([]);
  const [parts, setParts] = useState<PartAsset[]>([]);
  /** Which library part fills a slot for this run. A slot with no entry is
   *  left as the model has it — its own file, or nothing. */
  const [overrides, setOverrides] = useState<ComponentOverride[]>([]);
  /** A model's parts, being edited in the Models tab. Keyed by model id so an
   *  edit survives the list re-rendering around it. */
  const [partsDraft, setPartsDraft] = useState<Record<string, ModelComponent[]>>({});
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
  // LoRAs, slot swaps and the hires pass are all `sd-server` features applied
  // to weight files the app holds. A remote backend runs somebody else's
  // process against weights this app never sees, so those controls are hidden
  // rather than shown and then silently dropped on the way out.
  const remote = isRemoteModelId(selected?.id ?? null);
  // This tab's results, newest first: the newest fills the canvas, the rest is
  // the strip under it.
  const shownGallery = useMemo(
    () => gallery.filter((entry) => tasksFor(mode).includes(entry.task)),
    [gallery, mode],
  );
  const shown = shownGallery[0] ?? null;
  const shownHistory = shownGallery.slice(1);

  // Which conditioning images this run can actually use. Read off the slots
  // that will be *loaded* — the model's own plus anything overridden for this
  // run — because a ControlNet chosen from the library counts exactly as much
  // as one the model entry names. A remote backend has none of them: its
  // conditioning fields are dropped on the way out, so offering the inputs
  // would promise something the backend is never sent.
  const conditioning = useMemo(() => {
    if (remote || !selected) return new Set<ConditioningImage>();
    return availableConditioning(
      [
        ...selected.components.map((component) => component.slot),
        ...overrides.map((override) => override.slot),
      ],
      // The second gate, and an independent one: weights decide whether the
      // model can read the image, the engine's own flags decide whether this
      // build accepts the field at all.
      capabilities,
    );
  }, [remote, selected, overrides, capabilities]);

  // ADetailer re-renders each region its detector finds. The detector is a
  // launch flag, so this asks the same question the conditioning memo does —
  // the model's own slots plus anything picked for this run — but it is not a
  // conditioning *image*, so it cannot ride that set.
  const hasDetector = useMemo(
    () =>
      !remote &&
      [...(selected?.components ?? []), ...overrides].some(
        (component) => component.slot === "ad_model",
      ),
    [remote, selected, overrides],
  );

  // Inpainting. Offered only for the still-image edit task — a mask over the
  // first frame of a clip describes one frame out of thirty-three — and only
  // where the engine takes a `mask_image` at all, so an older build is never
  // sent a field it rejects.
  const canMask =
    task === "image_to_image" && !remote && engineSupports(capabilities, "mask_image");

  // The lists the pickers offer: the running engine's own, falling back to the
  // pinned build's while nothing is running. An engine answering with an empty
  // list is treated as not having answered — an empty sampler picker is never
  // the right thing to show.
  const samplers = capabilities?.samplers.length ? capabilities.samplers : SAMPLERS;
  const schedulers = capabilities?.schedulers.length ? capabilities.schedulers : SCHEDULERS;
  const upscalers = capabilities?.upscalers.length ? capabilities.upscalers : UPSCALERS;

  // One chooser per slot the library has a part for. Not per slot the *model*
  // has: a checkpoint that needs a separate VAE does not name one, so keying
  // off the model is exactly the case that offers nothing.
  const choosable = useMemo(
    () =>
      choosableSlots(parts).map((slot) => ({
        slot,
        options: parts.filter((part) => part.slot === slot),
        own: selected?.components.find((component) => component.slot === slot) ?? null,
      })),
    [parts, selected],
  );

  const refresh = useCallback(async () => {
    try {
      const [engine, library, entries, assets, loose, remotes, reported] = await Promise.all([
        studioClient.engineStatus(),
        studioClient.models(),
        studioClient.gallery(),
        studioClient.loras(),
        studioClient.parts(),
        studioClient.backends(),
        // An engine that is up but not answering must not take the whole panel
        // down with it: the lists have a fallback, so a failed ask is the same
        // as no engine at all.
        studioClient.capabilities().catch(() => null),
      ]);
      // A backend's models join the library list rather than sitting beside it:
      // the picker, the task filter and the run path then need no notion of a
      // backend at all, and the one place that does — which controls make sense
      // for the selection — asks the id.
      const list = [...library, ...backendModels(remotes)];
      setStatus(engine);
      setCapabilities(reported);
      setBackends(remotes);
      setModels(list);
      setGallery([...entries].reverse());
      setLoraLibrary(assets);
      setParts(loose);
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
    setOverrides([]);
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

  /** Adds a loose part from a file picker. Its slot is read off the file name
   *  and stays editable in the list — that guess is the one that matters, and
   *  a wrong one fails inside the engine rather than here. */
  const addPart = async () => {
    setError(null);
    const picked = await open({ multiple: false, directory: false });
    if (typeof picked !== "string") return;
    const hint = describeWeightFile(picked);
    try {
      setParts(
        await studioClient.addPart({
          slot: hint.slot ?? "vae",
          name: hint.name || (picked.split(/[/\\]/).pop() ?? picked),
          path: picked,
        }),
      );
    } catch (reason) {
      setError(errorText(reason));
    }
  };

  /** Adds a LoRA to the library from a file picker, named after the file. The
   *  name is only a label, so a wrong guess costs nothing. */
  const addLora = async () => {
    setError(null);
    const picked = await open({ multiple: false, directory: false });
    if (typeof picked !== "string") return;
    try {
      const basename = picked.split(/[/\\]/).pop() ?? picked;
      setLoraLibrary(
        await studioClient.addLora({
          name: describeWeightFile(picked).name || basename,
          path: picked,
        }),
      );
    } catch (reason) {
      setError(errorText(reason));
    }
  };

  /**
   * Saves an edit to a model's file list.
   *
   * The same entry the library holds — a model whose VAE was missing is the
   * same model once it is not. Any engine still holding the old file set is
   * dropped: it was launched from the list that just changed, and the backend
   * keys a warm engine on exactly that.
   */
  const saveParts = async (model: GenerationModel, components: ModelComponent[]) => {
    setError(null);
    try {
      await studioClient.addModel({ ...toSpec(model), components });
      if (status?.loadedModelId === model.id) await studioClient.unloadEngine();
      setPartsDraft((current) => {
        const next = { ...current };
        delete next[model.id];
        return next;
      });
      await refresh();
    } catch (reason) {
      setError(errorText(reason));
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

  /** Reads a chosen file as bare base64 and hands it to whichever image slot
   *  asked. One reader for the init image, the control image, the IP-Adapter
   *  reference and the reference list — they differ only in where the bytes
   *  land. */
  /** Replaces a conditioning image with its hint map, in place.
   *
   *  Destructive on purpose: the run sends one image, so keeping the original
   *  beside the processed one would only raise the question of which is used.
   *  Re-picking the file is the undo. */
  const preprocessInto = async (
    current: string | null,
    kind: Preprocessor,
    receive: (base64: string) => void,
  ) => {
    if (!current) return;
    try {
      receive(await runPreprocessor(current, kind));
    } catch (cause) {
      setError(String(cause));
    }
  };

  const pickImage = async (receive: (base64: string) => void) => {
    try {
      const base64 = await pickImageBase64();
      if (base64) receive(base64);
    } catch (cause) {
      setError(String(cause));
    }
  };

  /** Extends the source image on one side and marks the new ground for the
   *  model to fill.
   *
   *  Outpainting is inpainting on a bigger canvas, so this replaces the source,
   *  supplies the mask, and moves the requested size to match — all three, or
   *  the engine is handed a mask that does not line up with what it is given.
   *  Repeat to keep going, which is what dragging a frame outward amounts to. */
  const extend = async (side: keyof Margins) => {
    if (!initImage) return;
    setExtending(true);
    setError(null);
    try {
      const result = await runOutpaint(initImage, { ...NO_MARGINS, [side]: outpaintStep });
      if (settings)
        setOutpaintHistory((current) => [
          ...current,
          { image: initImage, width: settings.width, height: settings.height },
        ]);
      setOutpaintFuture([]);
      setInitImage(result.initImageBase64);
      setMaskImage(result.maskImageBase64);
      // Null only before a model is chosen, and the button that got here is
      // not reachable then — so leaving it null is right rather than
      // fabricating a settings object out of two dimensions.
      setSettings((current) =>
        current ? { ...current, width: result.width, height: result.height } : current,
      );
    } catch (cause) {
      setError(String(cause));
    } finally {
      setExtending(false);
    }
  };

  /** Moves one step along the extension history, in either direction.
   *
   *  Undo and redo are the same move with the stacks swapped, so they are one
   *  function: take the top of one stack, put where you were on the other, and
   *  restore. Writing them apart is how the two drift. */
  const stepExtension = (direction: "undo" | "redo") => {
    if (!initImage || !settings) return;
    const from = direction === "undo" ? outpaintHistory : outpaintFuture;
    const target = from[from.length - 1];
    if (!target) return;
    const here: OutpaintState = {
      image: initImage,
      width: settings.width,
      height: settings.height,
    };
    const drop = (current: OutpaintState[]) => current.slice(0, -1);
    const push = (current: OutpaintState[]) => [...current, here];
    if (direction === "undo") {
      setOutpaintHistory(drop);
      setOutpaintFuture(push);
    } else {
      setOutpaintFuture(drop);
      setOutpaintHistory(push);
    }
    setInitImage(target.image);
    setMaskImage(null);
    setSettings((current) =>
      current ? { ...current, width: target.width, height: target.height } : current,
    );
  };

  /** Dropping the source image drops the mask with it: a mask addresses that
   *  image's pixels, so keeping it would silently repaint the wrong region of
   *  whatever came next. Both extension stacks go too — they hold states of an
   *  image that is no longer loaded. */
  const changeInitImage = (next: string | null) => {
    setInitImage(next);
    setMaskImage(null);
    setOutpaintHistory([]);
    setOutpaintFuture([]);
  };

  const generate = async () => {
    if (!selected || !settings) return;
    setError(null);
    stopped.current = false;
    setBusy(true);
    setPercent(null);
    setPhase(t("Studio.phase.submitted"));
    try {
      const entries = await studioClient.run({
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
        // A clip and an utterance are one artifact per run whatever the
        // control last said, and the backend normalizes it to 1 regardless.
        batchCount: isVideoTask(task) || isSpeechTask(task) ? 1 : batchCount,
        videoFrames: isVideoTask(task)
          ? normalizeVideoFrames(selected.defaults.frameGrid, seconds * selected.defaults.fps)
          : 1,
        fps: isVideoTask(task) ? selected.defaults.fps : 1,
        speakerFile: isSpeechTask(task) ? speakerFile.trim() || null : null,
        language: isSpeechTask(task) ? language.trim() || null : null,
        initImageBase64: needsInitImage(task) ? initImage : null,
        // Each conditioning input is sent only where its own control was
        // offered, so a mask painted before switching task and a control image
        // chosen before switching model cannot follow the run somewhere they
        // mean nothing. The backend refuses them again on its own side.
        maskImageBase64: canMask && initImage ? maskImage : null,
        // Blank inherits the main prompt inside the engine, so an untouched
        // field is sent as null rather than as an empty string it would have
        // to interpret.
        adPrompt: hasDetector && adPrompt.trim() ? adPrompt.trim() : null,
        adNegativePrompt:
          hasDetector && adNegativePrompt.trim() ? adNegativePrompt.trim() : null,
        controlImageBase64: conditioning.has("control") ? controlImage : null,
        controlStrength: conditioning.has("control") && controlImage ? controlStrength : null,
        ipAdapterImageBase64: conditioning.has("ip_adapter") ? ipAdapterImage : null,
        ipAdapterStrength:
          conditioning.has("ip_adapter") && ipAdapterImage ? ipAdapterStrength : null,
        refImagesBase64: conditioning.has("reference") ? refImages : [],
        // Only meaningful when there is more than one reference to tell apart,
        // which is also the only time the control is shown.
        increaseRefIndex:
          conditioning.has("reference") && refImages.length > 1 && numberRefImages,
        // Blank rows are a half-typed path, not a LoRA the user meant.
        loras: loras.filter((lora) => lora.path.trim().length > 0),
        componentOverrides: overrides,
      });
      // Newest first, and within one batch the engine's own order — which
      // reversing the run's entries preserves once they are prepended.
      setGallery((current) => [...[...entries].reverse(), ...current]);
      // Only the last is previewed: it is the one the gallery shows first, and
      // decoding eight images to data URLs to show one is eight times the work.
      const newest = entries[entries.length - 1];
      if (newest) void loadPreview(newest);
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
      changeInitImage(dataUrl.slice(dataUrl.indexOf(",") + 1));
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

  // Tools share nothing with generation — no model, no prompt, no sampler —
  // so the section is its own panel rather than another branch threaded
  // through this one. After every hook above it, so the hook order is the same
  // whichever section is showing.
  if (mode === "tools") return <ToolPanel railSlot={railSlot} />;

  return (
    <div
      className={`flex min-h-0 flex-1 flex-col p-4 ${
        mode === "models" ? "overflow-y-auto" : "overflow-hidden"
      }`}
    >
      <header className="mb-4">
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
                    {/* A backend's models are not library entries and are not
                        forgotten one at a time — the backend below owns them. */}
                    {!isRemoteModelId(model.id) && (
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
                    )}
                    {model.installed ? (
                      <StatusPill tone="success">{t("Studio.installed")}</StatusPill>
                    ) : (
                      <StatusPill tone="neutral">
                        {t("Studio.download", { size: formatBytes(model.missingBytes) })}
                      </StatusPill>
                    )}
                  </span>
                </button>

                {/* Adding and swapping files happens here and only here. The
                    generation tabs pick from what this produces. A backend's
                    models have no files here to swap. */}
                <details className="mt-2" hidden={isRemoteModelId(model.id)}>
                  <summary className="cursor-pointer text-[11px] text-muted">
                    {t("Studio.parts")}
                    <span className="ml-1.5 text-faint">
                      {model.components
                        .map((component) => t(`Studio.slot.${component.slot}`))
                        .join(", ")}
                    </span>
                  </summary>
                  <div className="mt-2 grid gap-2 [&>*]:min-w-0">
                    <ModelFiles
                      components={partsDraft[model.id] ?? model.components}
                      onChange={(components) =>
                        setPartsDraft((current) => ({ ...current, [model.id]: components }))
                      }
                    />
                    {partsDraft[model.id] && (
                      <div className="flex items-center gap-1.5">
                        <Button
                          size="sm"
                          variant="primary"
                          onClick={() => void saveParts(model, partsDraft[model.id])}
                        >
                          {t("Studio.partsSave")}
                        </Button>
                        <Button
                          size="sm"
                          variant="secondary"
                          onClick={() =>
                            setPartsDraft((current) => {
                              const next = { ...current };
                              delete next[model.id];
                              return next;
                            })
                          }
                        >
                          {t("Studio.add.cancel")}
                        </Button>
                      </div>
                    )}
                  </div>
                </details>

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

        {/* Remote backends. Nothing here is bundled: a ComfyUI is a server the
            user installed and runs, and a hosted endpoint is somebody else's.
            Both are reached over HTTP, which is what fills the gaps the managed
            engine cannot — architectures it has no support for, and machines
            with no GPU at all. */}
        <div className="mt-6">
          <div className="mb-2 flex items-center justify-between">
            <h2 className="text-xs font-medium text-muted">{t("Studio.backends")}</h2>
            <Button
              size="sm"
              variant="secondary"
              onClick={() => setAddingBackend((open) => !open)}
            >
              {addingBackend ? t("Studio.add.cancel") : t("Studio.backendAdd")}
            </Button>
          </div>
          {addingBackend && (
            <div className="mb-2">
              <AddBackendForm
                onSaved={() => {
                  setAddingBackend(false);
                  void refresh();
                }}
              />
            </div>
          )}
          {backends.length === 0 && !addingBackend && (
            <p className="text-xs text-faint">{t("Studio.backendsEmpty")}</p>
          )}
          <div className="grid gap-2">
            {backends.map((backend) => (
              <div
                key={backend.id}
                className="flex items-start justify-between gap-3 rounded border border-border p-3"
              >
                <span className="min-w-0">
                  <span className="block text-xs font-medium">{backend.label}</span>
                  <span className="mt-0.5 block truncate text-[11px] text-faint">
                    {t(
                      backend.kind === "comfy_ui"
                        ? "Studio.backend.kindComfy"
                        : "Studio.backend.kindOpenAi",
                    )}
                    {backend.baseUrl ? ` · ${backend.baseUrl}` : ""}
                    {` · ${t("Studio.backendModelCount", {
                      count: String(backend.models.length),
                    })}`}
                  </span>
                </span>
                <IconButton
                  size="sm"
                  aria-label={t("Studio.forget")}
                  onClick={() =>
                    void studioClient
                      .removeBackend(backend.id)
                      .then(refresh)
                      .catch((reason) => setError(errorText(reason)))
                  }
                >
                  <Trash2 size={12} />
                </IconButton>
              </div>
            ))}
          </div>
        </div>

        {/* The loose files: CLIPs, text encoders, VAEs. A model entry has to
            be a whole model, so the pieces shared between models are added
            here and chosen per generation. */}
        <div className="mt-6">
          <div className="mb-2 flex items-center justify-between">
            <h2 className="text-xs font-medium text-muted">{t("Studio.partsLibrary")}</h2>
            <Button size="sm" variant="secondary" onClick={() => void addPart()}>
              <Plus size={13} />
              {t("Studio.partsAdd")}
            </Button>
          </div>
          {parts.length === 0 ? (
            <p className="text-xs text-faint">{t("Studio.partsLibraryEmpty")}</p>
          ) : (
            <div className="grid gap-1.5">
              {parts.map((part) => (
                <div
                  key={part.path}
                  className="flex items-center gap-2 rounded border border-border p-2"
                >
                  {/* The slot stays editable: it is read off the file name,
                      and a wrong one fails inside the engine rather than here. */}
                  <select
                    className="shrink-0 rounded border border-border bg-background px-1.5 py-1 text-[11px] text-foreground"
                    aria-label={t("Studio.add.slot")}
                    value={part.slot}
                    onChange={(event) =>
                      void studioClient
                        .addPart({ ...part, slot: event.target.value as ComponentSlot })
                        .then(setParts)
                        .catch((reason) => setError(errorText(reason)))
                    }
                  >
                    {COMPONENT_SLOTS.map((entry) => (
                      <option key={entry.slot} value={entry.slot}>
                        {t(`Studio.slot.${entry.slot}`)}
                      </option>
                    ))}
                  </select>
                  <span className="min-w-0 flex-1">
                    <span className="block truncate text-xs">{part.name}</span>
                    <span className="block truncate text-[11px] text-faint">{part.path}</span>
                  </span>
                  <IconButton
                    size="sm"
                    aria-label={t("Studio.partsForget")}
                    onClick={() =>
                      void studioClient
                        .removePart(part.path)
                        .then(setParts)
                        .catch((reason) => setError(errorText(reason)))
                    }
                  >
                    <Trash2 size={12} />
                  </IconButton>
                </div>
              ))}
            </div>
          )}
        </div>

        {/* LoRAs are a library of their own: they fill no slot, launch no
            engine, and are picked per run rather than loaded with a model. */}
        <div className="mt-6">
          <div className="mb-2 flex items-center justify-between">
            <h2 className="text-xs font-medium text-muted">{t("Studio.lora.library")}</h2>
            <Button size="sm" variant="secondary" onClick={() => void addLora()}>
              <Plus size={13} />
              {t("Studio.lora.addToLibrary")}
            </Button>
          </div>
          {loraLibrary.length === 0 ? (
            <p className="text-xs text-faint">{t("Studio.lora.libraryEmpty")}</p>
          ) : (
            <div className="grid gap-1.5">
              {loraLibrary.map((asset) => (
                <div
                  key={asset.path}
                  className="flex items-center gap-2 rounded border border-border p-2"
                >
                  <span className="min-w-0 flex-1">
                    <span className="block truncate text-xs">{asset.name}</span>
                    <span className="block truncate text-[11px] text-faint">{asset.path}</span>
                  </span>
                  <IconButton
                    size="sm"
                    aria-label={t("Studio.lora.forget")}
                    onClick={() =>
                      void studioClient
                        .removeLora(asset.path)
                        .then(setLoraLibrary)
                        .catch((reason) => setError(errorText(reason)))
                    }
                  >
                    <Trash2 size={12} />
                  </IconButton>
                </div>
              ))}
            </div>
          )}
        </div>
      </section>
      ) : null}

      {/* The canvas keeps the prompt, the button and the result together where
          the work happens. The rail of controls that used to sit beside it is
          portalled into the sidebar below, so this gets the full width. */}
      {mode !== "models" && (
      <div className="flex min-h-0 flex-1 gap-4 overflow-hidden p-1">
        {/* The rail renders into the sidebar rather than here. It stays part of
            this component — every control below reads state this component owns
            — and only its DOM position moves.
            `[&>*]:min-w-0`: a grid item's default minimum is its own
            min-content, so one wide control anywhere in the rail would size the
            whole column to itself and every box would overflow together. */}
        {railSlot ? createPortal(
        <div className="grid content-start gap-3 [&>*]:min-w-0">
          {/* Outside the `selected` guard below: with nothing chosen this is the
              one control that has to be reachable, or there is no way to choose. */}
          <SettingsCard title={t("Studio.models")}>
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
          </SettingsCard>

          {selected && (
          <>
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
                  placeholder={t("Studio.speakerPlaceholder")}
                  value={speakerFile}
                  onChange={(event) => setSpeakerFile(event.target.value)}
                />
                <span className="text-faint">{t("Studio.speakerHint")}</span>
              </label>
              <label className="grid gap-1 text-[11px] text-muted">
                {t("Studio.language")}
                <input
                  className="rounded border border-border bg-background px-2 py-1 font-mono text-[11px] text-foreground"
                  placeholder={t("Studio.languagePlaceholder")}
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
              <Button
                size="sm"
                variant="secondary"
                onClick={() => void pickImage(changeInitImage)}
              >
                <Upload size={13} />
                {t("Studio.chooseImage")}
              </Button>
              {initImage && (
                <>
                  <StatusPill tone="success">{t("Studio.imageReady")}</StatusPill>
                  <IconButton
                    size="sm"
                    aria-label={t("Studio.clearImage")}
                    onClick={() => changeInitImage(null)}
                  >
                    <Trash2 size={12} />
                  </IconButton>
                </>
              )}
            </div>
          )}

          {canMask && initImage && (
            <SettingsCard title={t("Studio.outpaint.title")} hint={t("Studio.outpaint.hint")}>
              {/* Two groups, each of which refuses to wrap inside itself: a
                  narrow panel breaks between the sizes and the arrows rather
                  than leaving one arrow stranded on a line of its own. */}
              <div className="flex flex-wrap items-center gap-x-3 gap-y-1.5">
                <div className="flex items-center gap-1.5">
                  {OUTPAINT_STEPS.map((step) => (
                    <Button
                      key={step}
                      size="sm"
                      variant={outpaintStep === step ? "primary" : "secondary"}
                      onClick={() => setOutpaintStep(step)}
                    >
                      {step}
                    </Button>
                  ))}
                  <span className="text-[11px] text-faint">px</span>
                </div>
                <div className="flex items-center gap-1.5">
                  {OUTPAINT_SIDES.map(({ side, labelKey, icon: Icon }) => (
                    <IconButton
                      key={side}
                      size="sm"
                      aria-label={t(labelKey)}
                      title={t(labelKey)}
                      disabled={extending}
                      onClick={() => void extend(side)}
                    >
                      <Icon size={13} />
                    </IconButton>
                  ))}
                  <IconButton
                    size="sm"
                    aria-label={t("Studio.outpaint.undo")}
                    title={t("Studio.outpaint.undo")}
                    disabled={extending || outpaintHistory.length === 0}
                    onClick={() => stepExtension("undo")}
                  >
                    <Undo2 size={13} />
                  </IconButton>
                  <IconButton
                    size="sm"
                    aria-label={t("Studio.outpaint.redo")}
                    title={t("Studio.outpaint.redo")}
                    disabled={extending || outpaintFuture.length === 0}
                    onClick={() => stepExtension("redo")}
                  >
                    <Redo2 size={13} />
                  </IconButton>
                </div>
              </div>
            </SettingsCard>
          )}

          {canMask && initImage && (
            <div className="grid gap-2">
              <span className="text-xs text-muted">{t("Studio.mask.title")}</span>
              <MaskCanvas imageBase64={initImage} value={maskImage} onChange={setMaskImage} />
            </div>
          )}

          {hasDetector && (
            <SettingsCard title={t("Studio.adetailer.title")} hint={t("Studio.adetailer.hint")}>
              <input
                className="w-full rounded border border-border bg-background px-2 py-1 text-xs text-foreground"
                placeholder={t("Studio.adetailer.promptPlaceholder")}
                aria-label={t("Studio.adetailer.prompt")}
                value={adPrompt}
                onChange={(event) => setAdPrompt(event.target.value)}
              />
              <input
                className="w-full rounded border border-border bg-background px-2 py-1 text-xs text-foreground"
                placeholder={t("Studio.adetailer.negativePlaceholder")}
                aria-label={t("Studio.adetailer.negative")}
                value={adNegativePrompt}
                onChange={(event) => setAdNegativePrompt(event.target.value)}
              />
            </SettingsCard>
          )}

          {conditioning.has("control") && (
            <ConditioningImageField
              label={t("Studio.control.title")}
              hint={t("Studio.control.hint")}
              value={controlImage}
              onPick={() => void pickImage(setControlImage)}
              onClear={() => setControlImage(null)}
              onPreprocess={(kind) => void preprocessInto(controlImage, kind, setControlImage)}
              strength={controlStrength}
              onStrength={setControlStrength}
              strengthLabel={t("Studio.control.strength")}
            />
          )}

          {conditioning.has("ip_adapter") && (
            <ConditioningImageField
              label={t("Studio.ipAdapter.title")}
              hint={t("Studio.ipAdapter.hint")}
              value={ipAdapterImage}
              onPick={() => void pickImage(setIpAdapterImage)}
              onClear={() => setIpAdapterImage(null)}
              strength={ipAdapterStrength}
              onStrength={setIpAdapterStrength}
              strengthLabel={t("Studio.ipAdapter.strength")}
            />
          )}

          {conditioning.has("reference") && (
            <ReferenceImages
              images={refImages}
              onAdd={() =>
                void pickImage((base64) =>
                  setRefImages((current) =>
                    current.length >= MAX_REF_IMAGES ? current : [...current, base64],
                  ),
                )
              }
              onRemove={(index) =>
                setRefImages((current) => current.filter((_, at) => at !== index))
              }
              numbered={numberRefImages}
              onNumberedChange={setNumberRefImages}
            />
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
            <>
            <details open className="rounded border border-border p-3">
              <summary className="cursor-pointer text-xs font-medium">
                {t("Studio.settings")}
              </summary>
              <div className="mt-3 grid gap-3 [&>*]:min-w-0">

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
                  {(samplers.includes(settings.sampler)
                    ? samplers
                    : [settings.sampler, ...samplers]
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

              {/* Images only: a clip and an utterance are one artifact per
                  run, so a batch control on them would promise nothing. */}
              {!isVideoTask(task) && !isSpeechTask(task) && (
                <SliderField
                  label={t("Studio.batch")}
                  value={batchCount}
                  min={1}
                  max={MAX_BATCH_COUNT}
                  step={1}
                  onChange={setBatchCount}
                />
              )}

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

              <details className="grid gap-2" hidden={remote}>
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
                      {schedulers.map((entry) => (
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
                        {upscalers.map((entry) => (
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

            {/* Its own card: the seed is the one setting people reach for
                between runs — reroll, or paste one back to reproduce a result —
                so it does not belong buried under the sampler. */}
            <SettingsCard title={t("Studio.seed")}>
              <span className="flex items-center gap-2">
                <input
                  className="min-w-0 flex-1 rounded border border-border bg-background px-2 py-1 font-mono text-xs text-foreground"
                  placeholder={t("Studio.seedPlaceholder")}
                  aria-label={t("Studio.seed")}
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
            </SettingsCard>
            </>
          )}

          {!isSpeechTask(task) && !remote && (
            <details className="rounded border border-border p-3">
              <summary className="cursor-pointer text-xs font-medium">
                {t("Studio.lora.title")}
              </summary>
              <div className="mt-3 grid [&>*]:min-w-0">
                <LoraStack
                  loras={loras}
                  library={loraLibrary}
                  onChange={setLoras}
                  showHighNoise={selected.components.some(
                    (component) => component.slot === "high_noise_diffusion_model",
                  )}
                />
              </div>
            </details>
          )}

          {/* Choosing only. Files are added in the Models tab, and a slot the
              model does not already load is a different model rather than a
              setting — so this offers the alternatives the library holds and
              nothing else. Hidden entirely when there are none, because a
              column of fixed dropdowns is just noise. */}
          {choosable.length > 0 && !remote && (
            <details open className="rounded border border-border p-3">
              <summary className="cursor-pointer text-xs font-medium">
                {t("Studio.parts")}
              </summary>
              <div className="mt-3 grid gap-2 [&>*]:min-w-0">
                <p className="text-[11px] text-faint">{t("Studio.partsHint")}</p>
                {choosable.map(({ slot, options, own }) => (
                  <label key={slot} className="grid gap-1 text-[11px] text-muted">
                    {t(`Studio.slot.${slot}`)}
                    <select
                      className="min-w-0 rounded border border-border bg-background px-1.5 py-1 text-[11px] text-foreground"
                      value={overrides.find((entry) => entry.slot === slot)?.path ?? ""}
                      onChange={(event) =>
                        setOverrides((current) => [
                          ...current.filter((entry) => entry.slot !== slot),
                          ...(event.target.value ? [{ slot, path: event.target.value }] : []),
                        ])
                      }
                    >
                      {/* Empty is "leave the model alone" — which means its own
                          file when it has one, and nothing when it does not. */}
                      <option value="">
                        {own ? componentFileName(own) : t("Studio.parts.none")}
                        {own ? ` (${t("Studio.parts.own")})` : ""}
                      </option>
                      {options.map((part) => (
                        <option key={part.path} value={part.path}>
                          {part.name}
                        </option>
                      ))}
                    </select>
                  </label>
                ))}
              </div>
            </details>
          )}
          </>
          )}
        </div>,
        railSlot,
        ) : null}

        {/* `min-w-0`: a flex item's floor is its content, so without this a
            wide result pushes the whole row past the pane. */}
        <section className="flex min-h-0 min-w-0 flex-1 flex-col gap-2">
          {/* Each button sits with the field it belongs to and stretches to
              its height, so the row reads as one control rather than two. */}
          <div className="flex shrink-0 items-stretch gap-2">
            <textarea
              className="min-h-16 min-w-0 flex-1 resize-none rounded border border-border bg-background p-2 text-xs"
              placeholder={t("Studio.promptPlaceholder")}
              // A placeholder is not a label: it disappears on the first
              // keystroke and screen readers may never announce it.
              aria-label={isSpeechTask(task) ? t("Studio.speechText") : t("Studio.prompt")}
              // The engine parses `(word:1.3)` inside the prompt itself —
              // verified against the pinned build, which exports
              // `parse_prompt_attention`. It costs nothing to expose and is
              // invisible otherwise. Speech has no sampler to weight.
              title={isSpeechTask(task) ? undefined : t("Studio.promptWeighting")}
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
                aria-label={t("Studio.negativePrompt")}
                title={t("Studio.promptWeighting")}
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

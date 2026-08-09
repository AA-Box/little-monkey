/**
 * The inpainting mask painter: brush over the source image to mark what the
 * model should repaint.
 *
 * The mask is a plain PNG the same pixel size as the source, black where the
 * image is kept and white where it is redrawn. `sd-server` decodes it with
 * `target_channels = 1`, so stb converts whatever the canvas exports down to
 * grayscale — no need to produce a single-channel file here. It is also handed
 * the source image's dimensions as the expected size, which is why the canvas
 * is the image's *natural* size and only its CSS box is scaled to fit.
 *
 * Deliberately small: a round brush, a size, and an eraser. No selection tools,
 * no feathering, no layers — the engine's own mask blur is what softens an
 * edge, and everything past a brush is an image editor rather than the smallest
 * thing that makes inpainting usable.
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { Eraser, Paintbrush, Redo2, Trash2, Undo2, ZoomIn, ZoomOut } from "lucide-react";

import { Button, IconButton } from "../ui";
import { useT } from "../../lib/i18n";

/** Brush diameter in image pixels, so a stroke covers the same area on a 512px
 *  source as on a 2048px one rather than shrinking with the zoom. */
const MIN_BRUSH = 8;
const MAX_BRUSH = 256;
const DEFAULT_BRUSH = 64;

/** Zoom stops. 1 fits the column; past that the container scrolls, which is
 *  what gives panning for free rather than a drag mode that would fight the
 *  brush for the same pointer. */
const ZOOM_STOPS = [1, 2, 3, 4] as const;

/** Steps between stops, clamping at each end. Written against the list so a
 *  stop added or removed needs no other change. */
export const nextStop = (current: number) =>
  ZOOM_STOPS.find((stop) => stop > current) ?? ZOOM_STOPS[ZOOM_STOPS.length - 1];
export const previousStop = (current: number) =>
  [...ZOOM_STOPS].reverse().find((stop) => stop < current) ?? ZOOM_STOPS[0];

/** How many strokes back undo reaches. Each entry is a full PNG of the mask —
 *  cheap for the mostly-black images a mask is, but not free on a 2048px
 *  source, so the tail is dropped rather than kept forever.
 *  ponytail: whole-canvas snapshots, switch to dirty-rect diffs if a long
 *  session on a large image gets heavy. */
const MAX_UNDO = 24;

interface Props {
  /** Bare base64 (no data URL prefix) of the image being painted over. */
  imageBase64: string;
  /** Bare base64 PNG of the current mask, or null when nothing is painted. */
  value: string | null;
  onChange: (maskBase64: string | null) => void;
}

export function MaskCanvas({ imageBase64, value, onChange }: Props) {
  const { t } = useT();
  const canvas = useRef<HTMLCanvasElement | null>(null);
  const painting = useRef(false);
  /** Previous pointer position in image pixels, so a fast drag draws a
   *  connected stroke instead of a dotted line of separate circles. */
  const last = useRef<{ x: number; y: number } | null>(null);
  const [brush, setBrush] = useState(DEFAULT_BRUSH);
  const [erasing, setErasing] = useState(false);
  const [size, setSize] = useState<{ width: number; height: number } | null>(null);
  /** The mask as it stood before each stroke, and — once undone — the strokes
   *  taken back. `null` is a legal entry: it is the empty mask, which is where
   *  the first stroke and every clear start from. */
  const [past, setPast] = useState<(string | null)[]>([]);
  const [future, setFuture] = useState<(string | null)[]>([]);
  /** The mask at `pointerdown`, held until the stroke commits: what undo has to
   *  put back is where the canvas was before the brush touched it, not where it
   *  is once the stroke has been painted. */
  const before = useRef<string | null>(null);

  const [zoom, setZoom] = useState(1);

  const source = `data:image/png;base64,${imageBase64}`;

  /** The mask the parent is holding, read inside the load effect without making
   *  it a dependency — see the effect's own note on why `onChange` is excluded
   *  for the same reason. */
  const incoming = useRef(value);
  incoming.current = value;

  // Sizing the canvas to the source resets it, which is correct: a mask drawn
  // over one image means nothing over another.
  useEffect(() => {
    let cancelled = false;
    const image = new Image();
    image.onload = () => {
      if (cancelled) return;
      const element = canvas.current;
      setSize({ width: image.naturalWidth, height: image.naturalHeight });
      if (!element) return;
      element.width = image.naturalWidth;
      element.height = image.naturalHeight;
      const context = element.getContext("2d");
      if (!context) return;
      context.fillStyle = "#000";
      context.fillRect(0, 0, element.width, element.height);
      // The history describes strokes over the old image. Over this one they
      // are meaningless, so undo starts empty rather than able to paste a mask
      // drawn for something else.
      setPast([]);
      setFuture([]);
      setZoom(1);

      // Unless the parent supplied a mask *for this image*. Extending the
      // picture hands over a new source and the mask marking the new margin in
      // the same update, and clearing unconditionally threw that mask away — so
      // the margin the user asked to have filled reached the engine unmarked,
      // and the run repainted everything or nothing rather than the new ground.
      const supplied = incoming.current;
      if (!supplied) {
        onChange(null);
        return;
      }
      const mask = new Image();
      mask.onload = () => {
        if (cancelled) return;
        context.drawImage(mask, 0, 0, element.width, element.height);
      };
      mask.src = `data:image/png;base64,${supplied}`;
    };
    image.src = source;
    return () => {
      cancelled = true;
    };
    // `onChange` is excluded on purpose: it is a fresh closure every render and
    // including it would clear the mask on every keystroke elsewhere in the form.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [source]);

  /** Canvas coordinates for a pointer event, in image pixels. The element is
   *  CSS-scaled to fit its column, so the ratio between the two has to be
   *  applied or the stroke lands somewhere other than the cursor. */
  const at = (event: React.PointerEvent<HTMLCanvasElement>) => {
    const element = event.currentTarget;
    const box = element.getBoundingClientRect();
    return {
      x: ((event.clientX - box.left) / box.width) * element.width,
      y: ((event.clientY - box.top) / box.height) * element.height,
    };
  };

  const stroke = (from: { x: number; y: number } | null, to: { x: number; y: number }) => {
    const context = canvas.current?.getContext("2d");
    if (!context) return;
    // Erasing paints black rather than clearing to transparent: the mask has to
    // stay a fully opaque black-and-white image, and a cleared pixel would
    // decode as whatever the alpha flattens to.
    context.strokeStyle = erasing ? "#000" : "#fff";
    context.fillStyle = context.strokeStyle;
    context.lineCap = "round";
    context.lineJoin = "round";
    context.lineWidth = brush;
    context.beginPath();
    if (from) {
      context.moveTo(from.x, from.y);
      context.lineTo(to.x, to.y);
      context.stroke();
    } else {
      context.arc(to.x, to.y, brush / 2, 0, Math.PI * 2);
      context.fill();
    }
  };

  /** Read the mask out once per stroke rather than per pointer move: encoding a
   *  2048px PNG on every `pointermove` is what turns painting into a slideshow. */
  const commit = useCallback(() => {
    const element = canvas.current;
    if (!element) return;
    const previous = before.current;
    setPast((current) => [...current, previous].slice(-MAX_UNDO));
    // A new stroke is a new branch: what was undone is no longer ahead of us.
    setFuture([]);
    onChange(element.toDataURL("image/png").split(",")[1] ?? null);
  }, [onChange]);

  /** Repaints the canvas to hold exactly `mask` — black everywhere when it is
   *  null, which is what an empty mask is. */
  const paint = (mask: string | null) => {
    const element = canvas.current;
    const context = element?.getContext("2d");
    if (!element || !context) return;
    context.fillStyle = "#000";
    context.fillRect(0, 0, element.width, element.height);
    if (!mask) return;
    const image = new Image();
    image.onload = () => context.drawImage(image, 0, 0, element.width, element.height);
    image.src = `data:image/png;base64,${mask}`;
  };

  /** Moves one step through the stroke history. Undo and redo are the same move
   *  with the stacks swapped — where you are goes onto the other stack, so the
   *  step is reversible in either direction. */
  const step = (direction: "undo" | "redo") => {
    const from = direction === "undo" ? past : future;
    if (from.length === 0) return;
    const target = from[from.length - 1] ?? null;
    const here = value;
    const drop = (current: (string | null)[]) => current.slice(0, -1);
    const push = (current: (string | null)[]) => [...current, here];
    if (direction === "undo") {
      setPast(drop);
      setFuture(push);
    } else {
      setFuture(drop);
      setPast(push);
    }
    paint(target);
    onChange(target);
  };

  const clear = () => {
    const element = canvas.current;
    const context = element?.getContext("2d");
    if (!element || !context) return;
    // Undoable like any stroke: clearing a mask by accident is exactly the
    // thing worth taking back.
    setPast((current) => [...current, value].slice(-MAX_UNDO));
    setFuture([]);
    context.fillStyle = "#000";
    context.fillRect(0, 0, element.width, element.height);
    onChange(null);
  };

  return (
    <div className="flex flex-col gap-2">
      {/* `overflow-auto` rather than a drag-to-pan mode: past 1× the content is
          wider than the box and the browser's own scrolling moves it, which
          costs no code and never competes with the brush for the pointer. */}
      <div className="max-h-80 overflow-auto rounded-md border border-border">
        <div className="relative w-fit" style={{ width: `${zoom * 100}%` }}>
        <img src={source} alt="" className="block w-full select-none" draggable={false} />
        <canvas
          ref={canvas}
          className="absolute inset-0 h-full w-full cursor-crosshair opacity-50 mix-blend-screen"
          onPointerDown={(event) => {
            event.currentTarget.setPointerCapture(event.pointerId);
            painting.current = true;
            before.current = value;
            const point = at(event);
            last.current = point;
            stroke(null, point);
          }}
          onPointerMove={(event) => {
            if (!painting.current) return;
            const point = at(event);
            stroke(last.current, point);
            last.current = point;
          }}
          onPointerUp={() => {
            if (!painting.current) return;
            painting.current = false;
            last.current = null;
            commit();
          }}
          // A cancelled pointer still ends the stroke — and still commits it.
          // The paint is already on the canvas either way, so skipping the
          // commit here would leave the exported mask behind what is drawn.
          onPointerCancel={() => {
            if (!painting.current) return;
            painting.current = false;
            last.current = null;
            commit();
          }}
        />
        </div>
      </div>

      <div className="flex items-center gap-1.5 text-xs">
        <IconButton
          size="sm"
          aria-label={t("Studio.mask.zoomOut")}
          title={t("Studio.mask.zoomOut")}
          disabled={zoom <= ZOOM_STOPS[0]}
          onClick={() => setZoom(previousStop)}
        >
          <ZoomOut size={12} />
        </IconButton>
        <IconButton
          size="sm"
          aria-label={t("Studio.mask.zoomIn")}
          title={t("Studio.mask.zoomIn")}
          disabled={zoom >= ZOOM_STOPS[ZOOM_STOPS.length - 1]}
          onClick={() => setZoom(nextStop)}
        >
          <ZoomIn size={12} />
        </IconButton>
        <span className="font-mono text-[11px] text-faint">{zoom}×</span>
        {/* Pushed to the far end of the zoom row rather than sharing the brush
            row below: it is the one destructive control here, and a labelled
            button beside the slider is what left the slider no width. */}
        <Button
          size="sm"
          variant="secondary"
          className="ml-auto"
          onClick={clear}
          disabled={!value}
        >
          <Trash2 size={12} />
          {t("Studio.mask.clear")}
        </Button>
      </div>

      <div className="flex items-center gap-2 text-xs">
        <IconButton
          size="sm"
          className="shrink-0"
          aria-label={t(erasing ? "Studio.mask.paint" : "Studio.mask.erase")}
          aria-pressed={erasing}
          onClick={() => setErasing((current) => !current)}
        >
          {erasing ? <Paintbrush size={12} /> : <Eraser size={12} />}
        </IconButton>
        <IconButton
          size="sm"
          className="shrink-0"
          aria-label={t("Studio.mask.undo")}
          title={t("Studio.mask.undo")}
          disabled={past.length === 0}
          onClick={() => step("undo")}
        >
          <Undo2 size={12} />
        </IconButton>
        <IconButton
          size="sm"
          className="shrink-0"
          aria-label={t("Studio.mask.redo")}
          title={t("Studio.mask.redo")}
          disabled={future.length === 0}
          onClick={() => step("redo")}
        >
          <Redo2 size={12} />
        </IconButton>
        {/* `min-w-0` on both: a range input's intrinsic width is around 130px
            and a flex item does not shrink past its content by default, so the
            slider held the row wider than the panel and pushed its own readout
            off the edge. */}
        <label className="flex min-w-0 flex-1 items-center gap-2">
          <span className="shrink-0 text-muted">{t("Studio.mask.brush")}</span>
          <input
            type="range"
            min={MIN_BRUSH}
            max={MAX_BRUSH}
            step={4}
            value={brush}
            onChange={(event) => setBrush(Number(event.target.value))}
            className="min-w-0 flex-1"
          />
          <span className="w-7 shrink-0 text-right font-mono text-[11px] text-faint">{brush}</span>
        </label>
      </div>
      <p className="text-[11px] text-faint">
        {size
          ? t("Studio.mask.hint", { width: String(size.width), height: String(size.height) })
          : t("Studio.mask.loading")}
      </p>
    </div>
  );
}

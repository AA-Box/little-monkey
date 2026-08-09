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
import {
  Eraser,
  Paintbrush,
  Redo2,
  Trash2,
  Undo2,
  ZoomIn,
  ZoomOut,
} from "lucide-react";

import { Button, IconButton } from "../ui";
import { useT } from "../../lib/i18n";

/** Brush diameter in image pixels, so a stroke covers the same area on a 512px
 *  source as on a 2048px one rather than shrinking with the zoom. */
const MIN_BRUSH = 8;
const MAX_BRUSH = 256;
const DEFAULT_BRUSH = 64;

/** How many strokes can be taken back. Bounded because each entry is a whole
 *  PNG of the mask; twelve covers the mis-stroke this exists for without
 *  holding a session's worth of images in memory. */
const MAX_HISTORY = 12;
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
  const [zoom, setZoom] = useState(1);
  /** One entry per undoable state, oldest first. `empty` distinguishes an
   *  all-black canvas — which means "repaint nothing" and must be reported as
   *  no mask at all — from one the user painted black over deliberately. */
  const [history, setHistory] = useState<{ data: string; empty: boolean }[]>([]);
  const [historyAt, setHistoryAt] = useState(0);

  const source = `data:image/png;base64,${imageBase64}`;

  /** The mask the parent is currently holding, read inside the load effect
   *  without making it a dependency — see the effect's own note. */
  const incoming = useRef(value);
  incoming.current = value;

  // Sizing the canvas to the source resets it, which is correct: a mask drawn
  // over one image means nothing over another.
  //
  // Unless the parent supplied one *for this image*. Extending the picture
  // hands over a new source and the mask marking the new margin in the same
  // update, and clearing unconditionally threw that mask away — so the margin
  // the user asked to be filled arrived at the engine unmarked, and the run
  // repainted everything or nothing instead of the new ground.
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
      setZoom(1);

      const supplied = incoming.current;
      if (!supplied) {
        setHistory([{ data: element.toDataURL("image/png"), empty: true }]);
        setHistoryAt(0);
        onChange(null);
        return;
      }
      const mask = new Image();
      mask.onload = () => {
        if (cancelled) return;
        context.drawImage(mask, 0, 0, element.width, element.height);
        setHistory([{ data: element.toDataURL("image/png"), empty: false }]);
        setHistoryAt(0);
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
    const data = element.toDataURL("image/png");
    onChange(data.split(",")[1] ?? null);
    // Anything that was undone is dropped: painting after an undo forks from
    // there, so keeping the old redo tail would offer a future that no longer
    // follows from what is on the canvas.
    setHistory((current) => {
      const kept = [...current.slice(0, historyAt + 1), { data, empty: false }];
      // Stored as PNG data URLs rather than raw pixels: a 2048px `ImageData`
      // is 16 MB, and a mostly-black mask compresses to a few kilobytes.
      const trimmed = kept.slice(-MAX_HISTORY);
      setHistoryAt(trimmed.length - 1);
      return trimmed;
    });
  }, [onChange, historyAt]);

  /** Puts one history entry back on the canvas and tells the parent what the
   *  mask now is — `null` for the blank state, so an all-black mask is never
   *  sent as if it were a real selection. */
  const restore = (index: number) => {
    const entry = history[index];
    const element = canvas.current;
    const context = element?.getContext("2d");
    if (!entry || !element || !context) return;
    const image = new Image();
    image.onload = () => {
      context.fillStyle = "#000";
      context.fillRect(0, 0, element.width, element.height);
      context.drawImage(image, 0, 0, element.width, element.height);
      setHistoryAt(index);
      onChange(entry.empty ? null : entry.data.split(",")[1] ?? null);
    };
    image.src = entry.data;
  };

  const clear = () => {
    const element = canvas.current;
    const context = element?.getContext("2d");
    if (!element || !context) return;
    context.fillStyle = "#000";
    context.fillRect(0, 0, element.width, element.height);
    onChange(null);
    // Clearing is itself undoable — it is the one action that can throw away
    // several minutes of painting in a single click.
    setHistory((current) => {
      const kept = [
        ...current.slice(0, historyAt + 1),
        { data: element.toDataURL("image/png"), empty: true },
      ].slice(-MAX_HISTORY);
      setHistoryAt(kept.length - 1);
      return kept;
    });
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
          aria-label={t("Studio.mask.undo")}
          title={t("Studio.mask.undo")}
          disabled={historyAt <= 0}
          onClick={() => restore(historyAt - 1)}
        >
          <Undo2 size={12} />
        </IconButton>
        <IconButton
          size="sm"
          aria-label={t("Studio.mask.redo")}
          title={t("Studio.mask.redo")}
          disabled={historyAt >= history.length - 1}
          onClick={() => restore(historyAt + 1)}
        >
          <Redo2 size={12} />
        </IconButton>
        <IconButton
          size="sm"
          aria-label={t("Studio.mask.zoomOut")}
          title={t("Studio.mask.zoomOut")}
          disabled={zoom <= ZOOM_STOPS[0]}
          onClick={() => setZoom((current) => previousStop(current))}
        >
          <ZoomOut size={12} />
        </IconButton>
        <IconButton
          size="sm"
          aria-label={t("Studio.mask.zoomIn")}
          title={t("Studio.mask.zoomIn")}
          disabled={zoom >= ZOOM_STOPS[ZOOM_STOPS.length - 1]}
          onClick={() => setZoom((current) => nextStop(current))}
        >
          <ZoomIn size={12} />
        </IconButton>
        <span className="w-8 shrink-0 text-right font-mono text-[11px] text-faint">{zoom}×</span>
      </div>

      <div className="flex items-center gap-2 text-xs">
        <IconButton
          size="sm"
          aria-label={t(erasing ? "Studio.mask.paint" : "Studio.mask.erase")}
          aria-pressed={erasing}
          onClick={() => setErasing((current) => !current)}
        >
          {erasing ? <Paintbrush size={12} /> : <Eraser size={12} />}
        </IconButton>
        <label className="flex flex-1 items-center gap-2">
          <span className="shrink-0 text-muted">{t("Studio.mask.brush")}</span>
          <input
            type="range"
            min={MIN_BRUSH}
            max={MAX_BRUSH}
            step={4}
            value={brush}
            onChange={(event) => setBrush(Number(event.target.value))}
            className="flex-1"
          />
          <span className="w-10 shrink-0 text-right font-mono text-[11px] text-faint">{brush}</span>
        </label>
        <Button size="sm" variant="secondary" onClick={clear} disabled={!value}>
          <Trash2 size={12} />
          {t("Studio.mask.clear")}
        </Button>
      </div>
      <p className="text-[11px] text-faint">
        {size
          ? t("Studio.mask.hint", { width: String(size.width), height: String(size.height) })
          : t("Studio.mask.loading")}
      </p>
    </div>
  );
}

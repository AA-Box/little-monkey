/**
 * Outpainting: extending an image past its own borders.
 *
 * This is the capability behind Draw Things' moveable canvas — generate, drag
 * the frame, fill what is now empty. The infinite canvas is one interface to
 * it; the operation underneath is what actually matters, and the engine already
 * does it. `sd-server` inpaints whatever a mask marks white, so extending an
 * image is composition, not a new engine feature and not a rewrite of Studio:
 * paste the original onto a larger canvas, mark the new margin white, and send
 * the pair down the img2img path that already exists.
 *
 * Two details decide whether the result looks joined or obviously pasted, and
 * both live here rather than in the component:
 *
 * - **The margin is filled by replicating the border pixels**, not with black.
 *   The sampler starts from these pixels, and starting from black biases it
 *   toward a dark halo along the seam — the single most common outpainting
 *   artefact. Stretched border colour is wrong in detail but right in tone,
 *   which is what the denoiser needs to work against.
 * - **The mask feathers inward**, so the model is allowed to repaint a thin
 *   band of the *original* too. A hard black-to-white edge asks it to match the
 *   existing pixels exactly at the boundary, which it cannot do, and the failure
 *   shows up as a visible seam.
 *
 * Pure over [`Bitmap`], so both are tested without a DOM.
 */
import type { Bitmap } from "./preprocess";

/** How far to grow each side, in pixels. */
export interface Margins {
  left: number;
  right: number;
  top: number;
  bottom: number;
}

/** What the engine gets: a larger image, and the mask marking what is new. */
export interface Outpaint {
  image: Bitmap;
  mask: Bitmap;
}

/** Width of the band inside the original that the model may also repaint. */
export const DEFAULT_FEATHER = 12;

export const NO_MARGINS: Margins = { left: 0, right: 0, top: 0, bottom: 0 };

function blank(width: number, height: number): Bitmap {
  return { data: new Uint8ClampedArray(width * height * 4), width, height };
}

const clamp = (value: number, low: number, high: number) =>
  Math.min(high, Math.max(low, value));

/**
 * Places `image` on a canvas grown by `margins` and returns it with the mask
 * that marks the new area.
 *
 * Margins are clamped at zero — a negative one would mean cropping, which is a
 * different operation with a different mask and does not belong here.
 */
export function expandForOutpaint(
  image: Bitmap,
  margins: Margins,
  feather: number = DEFAULT_FEATHER,
): Outpaint {
  const left = Math.max(0, Math.round(margins.left));
  const right = Math.max(0, Math.round(margins.right));
  const top = Math.max(0, Math.round(margins.top));
  const bottom = Math.max(0, Math.round(margins.bottom));

  const width = image.width + left + right;
  const height = image.height + top + bottom;
  const out = blank(width, height);
  const mask = blank(width, height);

  // A feather wider than the image would reach past the far edge and mark the
  // whole thing repaintable, quietly turning an extension into a regeneration.
  const band = clamp(
    Math.round(feather),
    0,
    Math.max(0, Math.floor(Math.min(image.width, image.height) / 2)),
  );

  for (let y = 0; y < height; y += 1) {
    for (let x = 0; x < width; x += 1) {
      // Where this pixel sits in the original, clamped to its edge. Inside the
      // original this is the pixel itself; outside, it is the nearest border
      // pixel, which is what replicates the edge outward.
      const sourceX = clamp(x - left, 0, image.width - 1);
      const sourceY = clamp(y - top, 0, image.height - 1);
      const from = (sourceY * image.width + sourceX) * 4;
      const to = (y * width + x) * 4;
      out.data[to] = image.data[from];
      out.data[to + 1] = image.data[from + 1];
      out.data[to + 2] = image.data[from + 2];
      out.data[to + 3] = 255;

      const localX = x - left;
      const localY = y - top;
      const outside =
        localX < 0 || localY < 0 || localX >= image.width || localY >= image.height;

      // Distance from the nearest *seam*, counting only sides that are actually
      // being extended. Feathering an edge with no margin would repaint a strip
      // of a border nothing is being joined to — damage, not blending, and the
      // reason this is per-side rather than one inset from the whole frame.
      const seams: number[] = [];
      if (left > 0) seams.push(localX);
      if (right > 0) seams.push(image.width - 1 - localX);
      if (top > 0) seams.push(localY);
      if (bottom > 0) seams.push(image.height - 1 - localY);
      const inset = seams.length > 0 ? Math.min(...seams) : Number.POSITIVE_INFINITY;

      let value: number;
      if (outside) {
        value = 255; // New ground: repaint freely.
      } else if (band > 0 && inset >= 0 && inset < band) {
        // Ramp from fully repaintable at the seam to fully kept `band` pixels
        // in, so the model blends rather than being asked to match exactly.
        value = Math.round(255 * (1 - inset / band));
      } else {
        value = 0; // Untouched original.
      }
      mask.data[to] = value;
      mask.data[to + 1] = value;
      mask.data[to + 2] = value;
      mask.data[to + 3] = 255;
    }
  }

  return { image: out, mask };
}

export function hasMargins(margins: Margins): boolean {
  return margins.left > 0 || margins.right > 0 || margins.top > 0 || margins.bottom > 0;
}

/** What the generation form needs after an extension: a new source image, the
 *  mask marking what to fill, and the size the run must now request. */
export interface OutpaintResult {
  initImageBase64: string;
  maskImageBase64: string;
  width: number;
  height: number;
}

function toBitmap(canvas: HTMLCanvasElement): CanvasRenderingContext2D {
  const context = canvas.getContext("2d");
  if (!context) throw new Error("This system cannot process images");
  return context;
}

function encode(bitmap: Bitmap): string {
  const canvas = document.createElement("canvas");
  canvas.width = bitmap.width;
  canvas.height = bitmap.height;
  toBitmap(canvas).putImageData(
    new ImageData(bitmap.data, bitmap.width, bitmap.height),
    0,
    0,
  );
  return canvas.toDataURL("image/png").split(",")[1] ?? "";
}

/**
 * Decodes, extends, and re-encodes — the one part that needs a DOM.
 *
 * PNG both ways: a mask is flat black and white, and JPEG ringing around its
 * edges would be read as instructions, softening the boundary the mask exists
 * to define.
 */
export async function runOutpaint(
  base64: string,
  margins: Margins,
  feather: number = DEFAULT_FEATHER,
): Promise<OutpaintResult> {
  const source = new Image();
  await new Promise<void>((resolve, reject) => {
    source.onload = () => resolve();
    source.onerror = () => reject(new Error("That image could not be read"));
    source.src = `data:image/png;base64,${base64}`;
  });

  const canvas = document.createElement("canvas");
  canvas.width = source.naturalWidth;
  canvas.height = source.naturalHeight;
  const context = toBitmap(canvas);
  context.drawImage(source, 0, 0);
  const pixels = context.getImageData(0, 0, canvas.width, canvas.height);

  const { image, mask } = expandForOutpaint(
    { data: pixels.data, width: pixels.width, height: pixels.height },
    margins,
    feather,
  );
  return {
    initImageBase64: encode(image),
    maskImageBase64: encode(mask),
    width: image.width,
    height: image.height,
  };
}

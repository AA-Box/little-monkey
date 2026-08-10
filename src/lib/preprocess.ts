/**
 * Control-image preprocessors.
 *
 * ControlNet does not take a photograph — it takes a *hint map*: an edge
 * drawing, a depth field, a pose skeleton. Until now the app asked the user to
 * produce that themselves, which is the difference between ControlNet being
 * usable and being theoretical: nobody has a Canny edge map of their own
 * holiday photo lying around.
 *
 * The engine has no preprocessor of its own — verified against the pinned
 * `sd-server`'s option list, which has `--control-image` and nothing that makes
 * one. So this does it here.
 *
 * # Why in the webview
 *
 * The frontend already runs in a browser engine, so `<canvas>` decodes PNG,
 * JPEG and WebP for free and `getImageData` hands over raw pixels. That makes
 * an edge detector a pure array transform and costs no dependency, no sidecar
 * process and no binary anyone has to publish. `MaskCanvas` already works this
 * way.
 *
 * Everything here is a pure function over [`Bitmap`] so it is tested without a
 * DOM; the canvas only decodes and re-encodes at the edges (see
 * `runPreprocessor`).
 *
 * Depth and pose are deliberately absent: both need a real model, which is what
 * the sidecar tool tier is for (`studio_tools.rs`). Edges need no model at all.
 */

/** Raw RGBA pixels, the shape `CanvasRenderingContext2D.getImageData` returns. */
export interface Bitmap {
  data: Uint8ClampedArray;
  width: number;
  height: number;
}

export const PREPROCESSORS = ["none", "canny", "grayscale", "invert"] as const;
export type Preprocessor = (typeof PREPROCESSORS)[number];

/** Luminance at one pixel, by the usual perceptual weights. */
function luma(image: Bitmap, index: number): number {
  const offset = index * 4;
  return (
    0.299 * image.data[offset] +
    0.587 * image.data[offset + 1] +
    0.114 * image.data[offset + 2]
  );
}

function blank(width: number, height: number): Bitmap {
  return { data: new Uint8ClampedArray(width * height * 4), width, height };
}

/** Writes one grey level into every channel, opaque. */
function put(image: Bitmap, index: number, value: number): void {
  const offset = index * 4;
  image.data[offset] = value;
  image.data[offset + 1] = value;
  image.data[offset + 2] = value;
  image.data[offset + 3] = 255;
}

export function grayscale(image: Bitmap): Bitmap {
  const out = blank(image.width, image.height);
  for (let index = 0; index < image.width * image.height; index += 1) {
    put(out, index, luma(image, index));
  }
  return out;
}

export function invert(image: Bitmap): Bitmap {
  const out = blank(image.width, image.height);
  for (let index = 0; index < image.width * image.height; index += 1) {
    const offset = index * 4;
    out.data[offset] = 255 - image.data[offset];
    out.data[offset + 1] = 255 - image.data[offset + 1];
    out.data[offset + 2] = 255 - image.data[offset + 2];
    out.data[offset + 3] = 255;
  }
  return out;
}

/**
 * Separable 5-tap Gaussian over the luminance plane.
 *
 * Canny without a blur is a noise detector — every sensor grain becomes an
 * edge, and the hint map ends up denser than the photograph. Separable because
 * two 1-D passes are O(2n) per pixel against O(n²) for the square kernel, and
 * this runs on images up to a few megapixels inside a click handler.
 */
function blurLuma(image: Bitmap): Float32Array {
  const { width, height } = image;
  const kernel = [1, 4, 6, 4, 1];
  const weight = 16;
  const source = new Float32Array(width * height);
  for (let index = 0; index < width * height; index += 1) {
    source[index] = luma(image, index);
  }

  const horizontal = new Float32Array(width * height);
  for (let y = 0; y < height; y += 1) {
    for (let x = 0; x < width; x += 1) {
      let total = 0;
      for (let tap = -2; tap <= 2; tap += 1) {
        // Clamped at the border rather than wrapped: wrapping invents an edge
        // down every side of the image.
        const sampleX = Math.min(width - 1, Math.max(0, x + tap));
        total += source[y * width + sampleX] * kernel[tap + 2];
      }
      horizontal[y * width + x] = total / weight;
    }
  }

  const blurred = new Float32Array(width * height);
  for (let y = 0; y < height; y += 1) {
    for (let x = 0; x < width; x += 1) {
      let total = 0;
      for (let tap = -2; tap <= 2; tap += 1) {
        const sampleY = Math.min(height - 1, Math.max(0, y + tap));
        total += horizontal[sampleY * width + x] * kernel[tap + 2];
      }
      blurred[y * width + x] = total / weight;
    }
  }
  return blurred;
}

/**
 * Canny edge detection: blur, Sobel, non-maximum suppression, then hysteresis.
 *
 * The last two steps are what separate this from a plain Sobel filter, and both
 * matter to the result ControlNet sees. Suppression thins a gradient ridge to
 * the single pixel at its peak, so lines come out one pixel wide instead of
 * smeared bands. Hysteresis keeps a weak pixel only when it is connected to a
 * strong one, which is what stops a shadow gradient becoming a contour while
 * still letting a real line survive the stretch where it fades.
 *
 * Thresholds are 0–255 on gradient magnitude. The defaults suit photographs;
 * raising them drops detail, lowering them keeps texture.
 */
export function canny(image: Bitmap, lowThreshold = 40, highThreshold = 110): Bitmap {
  const { width, height } = image;
  const out = blank(width, height);
  if (width < 3 || height < 3) return out;

  const blurred = blurLuma(image);
  const magnitude = new Float32Array(width * height);
  const direction = new Uint8Array(width * height);

  for (let y = 1; y < height - 1; y += 1) {
    for (let x = 1; x < width - 1; x += 1) {
      const index = y * width + x;
      const topLeft = blurred[index - width - 1];
      const top = blurred[index - width];
      const topRight = blurred[index - width + 1];
      const left = blurred[index - 1];
      const right = blurred[index + 1];
      const bottomLeft = blurred[index + width - 1];
      const bottom = blurred[index + width];
      const bottomRight = blurred[index + width + 1];

      const gx =
        topRight + 2 * right + bottomRight - (topLeft + 2 * left + bottomLeft);
      const gy =
        bottomLeft + 2 * bottom + bottomRight - (topLeft + 2 * top + topRight);
      magnitude[index] = Math.hypot(gx, gy);

      // Quantized to the four neighbour axes, which is all suppression needs.
      let angle = (Math.atan2(gy, gx) * 180) / Math.PI;
      if (angle < 0) angle += 180;
      if (angle < 22.5 || angle >= 157.5) direction[index] = 0;
      else if (angle < 67.5) direction[index] = 1;
      else if (angle < 112.5) direction[index] = 2;
      else direction[index] = 3;
    }
  }

  // Non-maximum suppression: keep a pixel only where it is the ridge peak
  // along the gradient.
  const thin = new Float32Array(width * height);
  for (let y = 1; y < height - 1; y += 1) {
    for (let x = 1; x < width - 1; x += 1) {
      const index = y * width + x;
      const value = magnitude[index];
      let before: number;
      let after: number;
      switch (direction[index]) {
        case 0:
          before = magnitude[index - 1];
          after = magnitude[index + 1];
          break;
        case 1:
          before = magnitude[index - width + 1];
          after = magnitude[index + width - 1];
          break;
        case 2:
          before = magnitude[index - width];
          after = magnitude[index + width];
          break;
        default:
          before = magnitude[index - width - 1];
          after = magnitude[index + width + 1];
      }
      thin[index] = value >= before && value >= after ? value : 0;
    }
  }

  // Hysteresis: strong pixels seed, weak pixels join only via a connected path.
  // Iterative with an explicit stack rather than recursion, because a long
  // contour in a large image would otherwise overflow it.
  const STRONG = 2;
  const WEAK = 1;
  const label = new Uint8Array(width * height);
  const stack: number[] = [];
  for (let index = 0; index < width * height; index += 1) {
    if (thin[index] >= highThreshold) {
      label[index] = STRONG;
      stack.push(index);
    } else if (thin[index] >= lowThreshold) {
      label[index] = WEAK;
    }
  }
  while (stack.length > 0) {
    const index = stack.pop() as number;
    const x = index % width;
    const y = (index - x) / width;
    for (let dy = -1; dy <= 1; dy += 1) {
      for (let dx = -1; dx <= 1; dx += 1) {
        const nx = x + dx;
        const ny = y + dy;
        if (nx < 0 || ny < 0 || nx >= width || ny >= height) continue;
        const neighbour = ny * width + nx;
        if (label[neighbour] === WEAK) {
          label[neighbour] = STRONG;
          stack.push(neighbour);
        }
      }
    }
  }

  // White lines on black is the convention every Canny ControlNet was trained
  // on; handing it the inverse produces confidently wrong structure.
  for (let index = 0; index < width * height; index += 1) {
    put(out, index, label[index] === STRONG ? 255 : 0);
  }
  return out;
}

export function applyPreprocessor(image: Bitmap, kind: Preprocessor): Bitmap {
  switch (kind) {
    case "canny":
      return canny(image);
    case "grayscale":
      return grayscale(image);
    case "invert":
      return invert(image);
    default:
      return image;
  }
}

/**
 * Decodes base64, runs the preprocessor, and re-encodes as PNG base64.
 *
 * PNG rather than the source format: an edge map is flat black and white, and
 * putting that through JPEG adds ringing artefacts around every line — noise
 * the ControlNet then faithfully reproduces.
 *
 * Lives beside the pure functions rather than in the component because it is
 * the one part that needs a DOM, and keeping it here means the component never
 * touches a canvas.
 */
export async function runPreprocessor(
  base64: string,
  kind: Preprocessor,
): Promise<string> {
  if (kind === "none") return base64;
  const image = new Image();
  await new Promise<void>((resolve, reject) => {
    image.onload = () => resolve();
    image.onerror = () => reject(new Error("That image could not be read"));
    image.src = `data:image/png;base64,${base64}`;
  });

  const canvas = document.createElement("canvas");
  canvas.width = image.naturalWidth;
  canvas.height = image.naturalHeight;
  const context = canvas.getContext("2d");
  if (!context) throw new Error("This system cannot process images");
  context.drawImage(image, 0, 0);

  const source = context.getImageData(0, 0, canvas.width, canvas.height);
  const processed = applyPreprocessor(
    { data: source.data, width: source.width, height: source.height },
    kind,
  );
  context.putImageData(
    new ImageData(processed.data, processed.width, processed.height),
    0,
    0,
  );
  return canvas.toDataURL("image/png").split(",")[1] ?? base64;
}

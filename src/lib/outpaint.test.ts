import { describe, expect, it } from "vitest";

import { expandForOutpaint, hasMargins, NO_MARGINS } from "./outpaint";
import type { Bitmap } from "./preprocess";

/** A bitmap from a per-pixel colour, so fixtures read as a picture. */
function fromRgb(
  width: number,
  height: number,
  rgb: (x: number, y: number) => [number, number, number],
): Bitmap {
  const data = new Uint8ClampedArray(width * height * 4);
  for (let y = 0; y < height; y += 1) {
    for (let x = 0; x < width; x += 1) {
      const offset = (y * width + x) * 4;
      const [r, g, b] = rgb(x, y);
      data[offset] = r;
      data[offset + 1] = g;
      data[offset + 2] = b;
      data[offset + 3] = 255;
    }
  }
  return { data, width, height };
}

const at = (image: Bitmap, x: number, y: number): [number, number, number] => {
  const offset = (y * image.width + x) * 4;
  return [image.data[offset], image.data[offset + 1], image.data[offset + 2]];
};
const grey = (image: Bitmap, x: number, y: number) => image.data[(y * image.width + x) * 4];

/** Solid red, big enough that a feather band fits well inside it. */
const red = fromRgb(40, 40, () => [255, 0, 0]);

describe("expandForOutpaint", () => {
  it("grows the canvas by exactly the margins asked for", () => {
    const { image, mask } = expandForOutpaint(red, { left: 5, right: 7, top: 3, bottom: 11 });
    expect([image.width, image.height]).toEqual([40 + 12, 40 + 14]);
    // The engine rejects a mask that is not the same size as the image.
    expect([mask.width, mask.height]).toEqual([image.width, image.height]);
  });

  it("keeps the original pixels where the original was", () => {
    const gradient = fromRgb(20, 20, (x, y) => [x * 10, y * 10, 0]);
    const { image } = expandForOutpaint(gradient, { left: 6, right: 0, top: 4, bottom: 0 });
    for (const [x, y] of [
      [0, 0],
      [10, 10],
      [19, 19],
    ] as const) {
      expect(at(image, x + 6, y + 4), `${x},${y}`).toEqual(at(gradient, x, y));
    }
  });

  /** Filling with black biases the sampler toward a dark halo along the seam —
   *  the most common outpainting artefact. */
  it("fills the new margin by replicating the border, not with black", () => {
    const { image } = expandForOutpaint(red, { left: 8, right: 8, top: 8, bottom: 8 });
    expect(at(image, 0, 0)).toEqual([255, 0, 0]);
    expect(at(image, image.width - 1, image.height - 1)).toEqual([255, 0, 0]);
    expect(at(image, 2, 20)).toEqual([255, 0, 0]);
  });

  it("replicates each edge outward rather than smearing one colour everywhere", () => {
    // Left half blue, right half green: the two margins must differ.
    const split = fromRgb(20, 20, (x) => (x < 10 ? [0, 0, 255] : [0, 255, 0]));
    const { image } = expandForOutpaint(split, { left: 5, right: 5, top: 0, bottom: 0 });
    expect(at(image, 0, 10)).toEqual([0, 0, 255]);
    expect(at(image, image.width - 1, 10)).toEqual([0, 255, 0]);
  });

  it("marks the new ground white and the untouched original black", () => {
    const { mask } = expandForOutpaint(red, { left: 8, right: 8, top: 8, bottom: 8 }, 4);
    expect(grey(mask, 0, 0)).toBe(255);
    expect(grey(mask, 2, 20)).toBe(255);
    // Deep inside the original, well past the feather band.
    expect(grey(mask, 28, 28)).toBe(0);
  });

  /** A hard black-to-white edge asks the model to match the existing pixels
   *  exactly at the boundary. It cannot, and the failure is a visible seam. */
  it("feathers inward, so the seam blends instead of butting up against itself", () => {
    const { mask } = expandForOutpaint(red, { left: 10, right: 0, top: 0, bottom: 0 }, 8);
    // Walking right from the seam, the mask must fall monotonically to black.
    const walk = Array.from({ length: 10 }, (_, step) => grey(mask, 10 + step, 20));
    expect(walk[0]).toBe(255);
    expect(walk[9]).toBe(0);
    for (let index = 1; index < walk.length; index += 1) {
      expect(walk[index], `step ${index}`).toBeLessThanOrEqual(walk[index - 1]);
    }
  });

  /** Feathering an edge with no margin repaints a strip of a border nothing is
   *  being joined to — damage rather than blending. Only sides actually being
   *  extended have a seam. */
  it("feathers only the sides being extended", () => {
    const { mask } = expandForOutpaint(red, { left: 10, right: 0, top: 0, bottom: 0 }, 8);
    // Left edge of the original is a seam, so it is feathered.
    expect(grey(mask, 10, 20)).toBe(255);
    // Right, top and bottom are not being extended, so they stay untouched.
    expect(grey(mask, mask.width - 1, 20)).toBe(0);
    expect(grey(mask, 25, 0)).toBe(0);
    expect(grey(mask, 25, mask.height - 1)).toBe(0);
  });

  it("keeps a feather wider than the image from turning an extension into a regeneration", () => {
    const small = fromRgb(10, 10, () => [128, 128, 128]);
    const { mask } = expandForOutpaint(small, { left: 4, right: 0, top: 0, bottom: 0 }, 500);
    // The far side must still be kept, or the whole picture is repainted.
    expect(grey(mask, mask.width - 1, 5)).toBe(0);
  });

  it("does nothing for no margins, and asks for no repainting", () => {
    const { image, mask } = expandForOutpaint(red, NO_MARGINS, 0);
    expect([image.width, image.height]).toEqual([40, 40]);
    expect(Array.from(mask.data).filter((_, index) => index % 4 !== 3).every((v) => v === 0)).toBe(
      true,
    );
  });

  /** A negative margin means cropping, which needs a different mask. Clamped
   *  rather than honoured, so it cannot silently shrink the picture. */
  it("clamps a negative margin to zero instead of cropping", () => {
    const { image } = expandForOutpaint(red, { left: -20, right: 5, top: 0, bottom: 0 });
    expect(image.width).toBe(45);
  });

  it("leaves the result fully opaque, since the mask carries the meaning", () => {
    const { image, mask } = expandForOutpaint(red, { left: 3, right: 3, top: 3, bottom: 3 });
    for (const layer of [image, mask]) {
      for (let index = 3; index < layer.data.length; index += 4) {
        expect(layer.data[index]).toBe(255);
      }
    }
  });
});

describe("hasMargins", () => {
  it("is false only when nothing would grow", () => {
    expect(hasMargins(NO_MARGINS)).toBe(false);
    expect(hasMargins({ ...NO_MARGINS, bottom: 1 })).toBe(true);
  });
});

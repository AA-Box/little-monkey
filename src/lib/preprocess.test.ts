import { describe, expect, it } from "vitest";

import {
  applyPreprocessor,
  canny,
  grayscale,
  invert,
  PREPROCESSORS,
  type Bitmap,
} from "./preprocess";
import { en } from "./i18n/locales/en";

/** A bitmap from a per-pixel grey level, so fixtures read as a picture. */
function fromGrey(width: number, height: number, grey: (x: number, y: number) => number): Bitmap {
  const data = new Uint8ClampedArray(width * height * 4);
  for (let y = 0; y < height; y += 1) {
    for (let x = 0; x < width; x += 1) {
      const offset = (y * width + x) * 4;
      const value = grey(x, y);
      data[offset] = value;
      data[offset + 1] = value;
      data[offset + 2] = value;
      data[offset + 3] = 255;
    }
  }
  return { data, width, height };
}

const at = (image: Bitmap, x: number, y: number) => image.data[(y * image.width + x) * 4];

describe("canny", () => {
  /** Black left half, white right half: one straight vertical edge. */
  const verticalEdge = fromGrey(32, 32, (x) => (x < 16 ? 0 : 255));

  it("finds the edge and leaves the flat regions empty", () => {
    const edges = canny(verticalEdge);
    const litColumns = new Set<number>();
    for (let y = 4; y < 28; y += 1) {
      for (let x = 0; x < 32; x += 1) {
        if (at(edges, x, y) > 0) litColumns.add(x);
      }
    }
    expect(litColumns.size).toBeGreaterThan(0);
    // Every lit pixel sits at the transition, not out in the flat areas.
    for (const column of litColumns) {
      expect(Math.abs(column - 16)).toBeLessThanOrEqual(2);
    }
  });

  it("draws white lines on black, which is what a Canny ControlNet was trained on", () => {
    const edges = canny(verticalEdge);
    expect(at(edges, 0, 16)).toBe(0);
    expect(at(edges, 31, 16)).toBe(0);
    const litSomewhere = Array.from({ length: 32 }, (_, x) => at(edges, x, 16)).some(
      (value) => value === 255,
    );
    expect(litSomewhere).toBe(true);
  });

  it("thins the ridge to roughly one pixel rather than a smeared band", () => {
    const edges = canny(verticalEdge);
    const litInRow = Array.from({ length: 32 }, (_, x) => at(edges, x, 16)).filter(
      (value) => value > 0,
    ).length;
    expect(litInRow).toBeLessThanOrEqual(2);
  });

  it("finds nothing in a flat image, so noise-free input yields no false structure", () => {
    const flat = canny(fromGrey(24, 24, () => 128));
    expect(Array.from(flat.data).some((value, index) => index % 4 !== 3 && value > 0)).toBe(
      false,
    );
  });

  it("keeps a faint edge that a plain threshold would drop, via hysteresis", () => {
    // A step too small to pass the high threshold on its own, joined to a
    // strong one: hysteresis is what carries the weak stretch.
    const mixed = fromGrey(40, 20, (x, y) => {
      if (y < 10) return x < 20 ? 0 : 255;
      return x < 20 ? 118 : 138;
    });
    const edges = canny(mixed);
    const weakHalfLit = Array.from({ length: 20 }, (_, offset) =>
      Array.from({ length: 40 }, (_, x) => at(edges, x, 10 + offset)).some((v) => v > 0),
    ).some(Boolean);
    expect(weakHalfLit).toBe(true);
  });

  it("survives an image too small to have an interior", () => {
    expect(() => canny(fromGrey(2, 2, () => 255))).not.toThrow();
  });
});

describe("grayscale and invert", () => {
  it("weights green most, matching perceived brightness", () => {
    const green = { data: new Uint8ClampedArray([0, 255, 0, 255]), width: 1, height: 1 };
    const blue = { data: new Uint8ClampedArray([0, 0, 255, 255]), width: 1, height: 1 };
    expect(at(grayscale(green), 0, 0)).toBeGreaterThan(at(grayscale(blue), 0, 0));
  });

  it("inverts each channel and keeps the image opaque", () => {
    const image = { data: new Uint8ClampedArray([10, 20, 30, 255]), width: 1, height: 1 };
    const flipped = invert(image);
    expect(Array.from(flipped.data)).toEqual([245, 235, 225, 255]);
  });
});

/** The picker builds its key from the kind, so the i18n key-lint — which only
 *  sees literal call sites — cannot check these. Pinned here instead. */
describe("labels", () => {
  it("every preprocessor has one", () => {
    for (const kind of PREPROCESSORS.filter((entry) => entry !== "none")) {
      expect(en[`Studio.preprocess.${kind}` as keyof typeof en], kind).toBeTruthy();
    }
  });
});

describe("applyPreprocessor", () => {
  it("returns the image untouched for 'none'", () => {
    const image = fromGrey(4, 4, () => 90);
    expect(applyPreprocessor(image, "none")).toBe(image);
  });

  it("routes each kind to its own transform", () => {
    const image = fromGrey(8, 8, (x) => (x < 4 ? 0 : 255));
    expect(at(applyPreprocessor(image, "invert"), 0, 0)).toBe(255);
    expect(at(applyPreprocessor(image, "grayscale"), 7, 0)).toBe(255);
  });
});

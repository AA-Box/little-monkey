import { describe, expect, it } from "vitest";

import {
  clampToolNumber,
  missingRequired,
  toolDefaults,
  type ToolInput,
  type ToolManifest,
} from "./studioTools";

function input(key: string, overrides: Partial<ToolInput> = {}): ToolInput {
  return {
    key,
    label: key,
    kind: "text",
    required: false,
    options: [],
    ...overrides,
  };
}

function manifest(inputs: ToolInput[]): ToolManifest {
  return {
    schemaVersion: 1,
    id: "face-swap",
    name: "Face Swap",
    inputs,
  };
}

describe("toolDefaults", () => {
  it("prefers a declared default over the kind's empty value", () => {
    const values = toolDefaults(
      manifest([input("scale", { kind: "number", min: 1, max: 4, default: 2 })]),
    );
    expect(values.scale).toBe(2);
  });

  it("selects a choice's first option so a select never reports nothing while showing something", () => {
    const values = toolDefaults(
      manifest([
        input("restorer", {
          kind: "choice",
          options: [
            { value: "gfpgan", label: "GFPGAN" },
            { value: "codeformer", label: "CodeFormer" },
          ],
        }),
      ]),
    );
    expect(values.restorer).toBe("gfpgan");
  });

  it("leaves an image with no value, because there is no empty image to pre-fill", () => {
    const values = toolDefaults(manifest([input("source", { kind: "image" })]));
    expect(values.source).toBeUndefined();
  });

  it("starts a number at its declared minimum rather than at zero", () => {
    const values = toolDefaults(manifest([input("scale", { kind: "number", min: 1 })]));
    expect(values.scale).toBe(1);
  });
});

describe("missingRequired", () => {
  it("names a required input that was never filled", () => {
    const spec = manifest([
      input("source", { kind: "image", required: true, label: "Source image" }),
    ]);
    expect(missingRequired(spec, {})).toEqual(["Source image"]);
  });

  it("counts a cleared text box as empty", () => {
    const spec = manifest([input("prompt", { required: true, label: "Prompt" })]);
    expect(missingRequired(spec, { prompt: "   " })).toEqual(["Prompt"]);
  });

  it("accepts false as an answer for a required toggle", () => {
    const spec = manifest([input("restore", { kind: "toggle", required: true })]);
    expect(missingRequired(spec, { restore: false })).toEqual([]);
  });

  it("ignores optional inputs", () => {
    const spec = manifest([input("note")]);
    expect(missingRequired(spec, {})).toEqual([]);
  });
});

describe("clampToolNumber", () => {
  const scale = input("scale", { kind: "number", min: 1, max: 4 });

  it("holds a typed value inside the declared range", () => {
    expect(clampToolNumber(scale, "9")).toBe(4);
    expect(clampToolNumber(scale, "-3")).toBe(1);
    expect(clampToolNumber(scale, "2.5")).toBe(2.5);
  });

  it("falls back to the minimum rather than to zero, which a range may exclude", () => {
    expect(clampToolNumber(scale, "")).toBe(1);
    expect(clampToolNumber(scale, "abc")).toBe(1);
  });

  it("leaves an unbounded number alone", () => {
    expect(clampToolNumber(input("free", { kind: "number" }), "1000")).toBe(1000);
  });
});

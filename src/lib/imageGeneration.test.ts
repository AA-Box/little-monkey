import { describe, expect, it } from "vitest";

import {
  clampRasterDimensions,
  DEFAULT_RASTER_SIZE,
  isWorkspaceImageSrc,
  MAX_RASTER_DIMENSION,
  parseGeneratedImageReceipt,
  svgDimensions,
} from "./imageGeneration";

// Only the DOM-free helpers are exercised here — `rasterizeSvgToPng` needs a
// real canvas (vitest runs under `environment: "node"`), so its behavior is
// covered by the webview at runtime, not this suite.

describe("svgDimensions", () => {
  it("reads explicit width/height attributes, with or without px units", () => {
    expect(svgDimensions('<svg width="640" height="480"></svg>')).toEqual({ width: 640, height: 480 });
    expect(svgDimensions("<svg width='320px' height='200px'></svg>")).toEqual({ width: 320, height: 200 });
  });

  it("falls back to the viewBox rect when width/height are absent", () => {
    expect(svgDimensions('<svg viewBox="0 0 1200 800"><rect/></svg>')).toEqual({ width: 1200, height: 800 });
    expect(svgDimensions('<svg viewBox="-10, -10, 100, 50"></svg>')).toEqual({ width: 100, height: 50 });
  });

  it("derives the missing dimension from the viewBox aspect ratio when only one is explicit", () => {
    expect(svgDimensions('<svg width="600" viewBox="0 0 300 150"></svg>')).toEqual({ width: 600, height: 300 });
    expect(svgDimensions('<svg height="100" viewBox="0 0 400 200"></svg>')).toEqual({ width: 200, height: 100 });
  });

  it("ignores percentage dimensions and uses the viewBox instead", () => {
    expect(svgDimensions('<svg width="100%" height="100%" viewBox="0 0 500 250"></svg>')).toEqual({
      width: 500,
      height: 250,
    });
  });

  it("uses the default size when nothing usable is declared", () => {
    expect(svgDimensions("<svg></svg>")).toEqual(DEFAULT_RASTER_SIZE);
    expect(svgDimensions("not svg at all")).toEqual(DEFAULT_RASTER_SIZE);
    expect(svgDimensions('<svg width="0" height="0"></svg>')).toEqual(DEFAULT_RASTER_SIZE);
  });

  it("only ever reads the root svg tag, not nested elements", () => {
    const svg = '<svg viewBox="0 0 100 100"><svg width="9999" height="9999"/></svg>';
    expect(svgDimensions(svg)).toEqual({ width: 100, height: 100 });
  });
});

describe("clampRasterDimensions", () => {
  it("passes already-small dimensions through unchanged", () => {
    expect(clampRasterDimensions({ width: 800, height: 600 })).toEqual({ width: 800, height: 600 });
  });

  it("scales down preserving aspect ratio so neither side exceeds the max", () => {
    const clamped = clampRasterDimensions({ width: MAX_RASTER_DIMENSION * 2, height: MAX_RASTER_DIMENSION });
    expect(clamped.width).toBe(MAX_RASTER_DIMENSION);
    expect(clamped.height).toBe(MAX_RASTER_DIMENSION / 2);
  });

  it("never scales up and never returns a dimension below 1", () => {
    expect(clampRasterDimensions({ width: 10, height: 10 })).toEqual({ width: 10, height: 10 });
    const tiny = clampRasterDimensions({ width: 0.2, height: MAX_RASTER_DIMENSION * 100 });
    expect(tiny.width).toBeGreaterThanOrEqual(1);
    expect(tiny.height).toBe(MAX_RASTER_DIMENSION);
  });
});

describe("isWorkspaceImageSrc", () => {
  it("accepts workspace-relative paths with previewable image extensions", () => {
    expect(isWorkspaceImageSrc("chart.png")).toBe(true);
    expect(isWorkspaceImageSrc("out/images/plot.JPEG")).toBe(true);
    expect(isWorkspaceImageSrc("a/b/c.webp")).toBe(true);
    expect(isWorkspaceImageSrc("diagram.svg")).toBe(true);
  });

  it("rejects URLs, data/blob srcs, and absolute paths", () => {
    expect(isWorkspaceImageSrc("https://example.com/x.png")).toBe(false);
    expect(isWorkspaceImageSrc("data:image/png;base64,AAAA")).toBe(false);
    expect(isWorkspaceImageSrc("blob:abc")).toBe(false);
    expect(isWorkspaceImageSrc("/etc/x.png")).toBe(false);
    expect(isWorkspaceImageSrc("\\\\server\\x.png")).toBe(false);
  });

  it("rejects non-image extensions and empty srcs", () => {
    expect(isWorkspaceImageSrc("src/index.ts")).toBe(false);
    expect(isWorkspaceImageSrc("README.md")).toBe(false);
    expect(isWorkspaceImageSrc("")).toBe(false);
    expect(isWorkspaceImageSrc(undefined)).toBe(false);
  });
});

describe("parseGeneratedImageReceipt", () => {
  const valid = JSON.stringify({
    artifactId: "a".repeat(64),
    mediaType: "image/png",
    width: 1024,
    height: 768,
    size: 12345,
    suggestedName: "little-monkey.png",
  });

  it("accepts a valid durable generated-image reference", () => {
    expect(parseGeneratedImageReceipt(valid)).toEqual({
      artifactId: "a".repeat(64),
      mediaType: "image/png",
      width: 1024,
      height: 768,
      size: 12345,
      suggestedName: "little-monkey.png",
    });
  });

  it("rejects legacy text, errors, malformed ids, and non-PNG media", () => {
    expect(parseGeneratedImageReceipt("Saved image to images/logo.png")).toBeNull();
    expect(parseGeneratedImageReceipt('{"error":"failed"}')).toBeNull();
    expect(parseGeneratedImageReceipt(valid.replace("a".repeat(64), "../escape"))).toBeNull();
    expect(parseGeneratedImageReceipt(valid.replace("image/png", "image/svg+xml"))).toBeNull();
  });
});

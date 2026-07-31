import { afterEach, describe, expect, it } from "vitest";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";

// The release check is executable ESM rather than application TypeScript.
// @ts-expect-error The adjacent .mjs intentionally has no declaration file.
import { analyzeBundle } from "../../scripts/check-bundle-budget.mjs";

const fixtureDirectories: string[] = [];

afterEach(() => {
  for (const directory of fixtureDirectories.splice(0)) {
    rmSync(directory, { recursive: true, force: true });
  }
});

function fixture(files: Record<string, string>): string {
  const dist = mkdtempSync(join(tmpdir(), "little-monkey-bundle-budget-"));
  fixtureDirectories.push(dist);
  for (const [file, contents] of Object.entries(files)) {
    const path = join(dist, file);
    mkdirSync(dirname(path), { recursive: true });
    writeFileSync(path, contents);
  }
  return dist;
}

const generousLimits = {
  entryRaw: 10_000,
  entryGzip: 10_000,
  initialRaw: 10_000,
  initialGzip: 10_000,
  chunkRaw: 10_000,
  chunkGzip: 10_000,
  cssRaw: 10_000,
  cssGzip: 10_000,
};

describe("bundle budget analysis", () => {
  it("counts transitive static imports as initial and keeps dynamic imports out", () => {
    const dist = fixture({
      "assets/entry.js": "entry".repeat(20),
      "assets/shared.js": "shared".repeat(15),
      "assets/lazy.js": "lazy".repeat(100),
      "assets/app.css": "style".repeat(10),
    });
    const manifest = {
      "src/main.tsx": {
        file: "assets/entry.js",
        isEntry: true,
        imports: ["_shared.js"],
        dynamicImports: ["src/lazy.tsx"],
        css: ["assets/app.css"],
      },
      "_shared.js": { file: "assets/shared.js" },
      "src/lazy.tsx": { file: "assets/lazy.js", isDynamicEntry: true },
    };

    const result = analyzeBundle({ dist, manifest, limits: generousLimits });

    expect(result.failures).toEqual([]);
    expect(result.report.initial.chunks).toBe(2);
    expect(result.measurements.initialRaw).toBe(
      result.measurements.entry.raw + result.measurements.initialJs[1].raw,
    );
    expect(result.measurements.initialJs.map(({ file }: { file: string }) => file)).not.toContain("assets/lazy.js");
    expect(result.report.largestChunks[0].file).toBe("assets/lazy.js");
  });

  it("enforces async chunk and stylesheet ceilings too", () => {
    const dist = fixture({
      "assets/entry.js": "entry",
      "assets/lazy.js": "x".repeat(80),
      "assets/lazy.css": "y".repeat(40),
    });
    const manifest = {
      "src/main.tsx": { file: "assets/entry.js", isEntry: true },
      "src/lazy.tsx": {
        file: "assets/lazy.js",
        isDynamicEntry: true,
        css: ["assets/lazy.css"],
      },
    };

    const result = analyzeBundle({
      dist,
      manifest,
      limits: { ...generousLimits, chunkRaw: 64, cssRaw: 32 },
    });

    expect(result.failures).toContain("assets/lazy.js raw: 0.1 KiB > 0.1 KiB");
    expect(result.failures).toContain("assets/lazy.css raw: 0.0 KiB > 0.0 KiB");
  });

  it("rejects malformed import graphs instead of silently undercounting", () => {
    const dist = fixture({ "assets/entry.js": "entry" });
    const manifest = {
      "src/main.tsx": {
        file: "assets/entry.js",
        isEntry: true,
        imports: ["_missing.js"],
      },
    };

    expect(() => analyzeBundle({ dist, manifest, limits: generousLimits })).toThrow(
      "Manifest import _missing.js is missing.",
    );
  });
});

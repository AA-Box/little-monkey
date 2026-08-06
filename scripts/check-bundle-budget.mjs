import { gzipSync } from "node:zlib";
import { readFileSync } from "node:fs";
import { isAbsolute, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

export const DEFAULT_LIMITS = Object.freeze({
  entryRaw: 1_500 * 1024,
  entryGzip: 450 * 1024,
  initialRaw: 2_000 * 1024,
  initialGzip: 600 * 1024,
  chunkRaw: 900 * 1024,
  chunkGzip: 300 * 1024,
  cssRaw: 150 * 1024,
  cssGzip: 30 * 1024,
});

function assetSize(dist, file) {
  const path = resolve(dist, file);
  const relativePath = relative(dist, path);
  if (relativePath === ".." || relativePath.startsWith(`..${sep}`) || isAbsolute(relativePath)) {
    throw new Error(`Manifest asset escapes dist: ${file}`);
  }
  // One read, not statSync + readFileSync: the raw size is the buffer length,
  // so there is no window in which the file can change between the two calls.
  const contents = readFileSync(path);
  return { file, raw: contents.length, gzip: gzipSync(contents).length };
}

function kib(bytes) {
  return `${(bytes / 1024).toFixed(1)} KiB`;
}

function check(label, value, limit, failures) {
  if (value > limit) failures.push(`${label}: ${kib(value)} > ${kib(limit)}`);
}

/**
 * Measures one Vite manifest. This pure-ish boundary accepts a fixture
 * manifest and dist directory so the graph traversal and every limit can be
 * covered without invoking Vite in unit tests.
 */
export function analyzeBundle({ dist, manifest, limits: limitOverrides = {} }) {
  const limits = { ...DEFAULT_LIMITS, ...limitOverrides };
  const entryKeys = Object.keys(manifest).filter((key) => manifest[key]?.isEntry);
  if (entryKeys.length === 0) throw new Error("Vite manifest has no entry chunk.");
  if (entryKeys.length > 1) {
    throw new Error(`Vite manifest has multiple entry chunks: ${entryKeys.join(", ")}`);
  }
  const entryKey = entryKeys[0];

  const initialKeys = new Set();
  function visit(key) {
    if (initialKeys.has(key)) return;
    const item = manifest[key];
    if (!item) throw new Error(`Manifest import ${key} is missing.`);
    initialKeys.add(key);
    for (const dependency of item.imports ?? []) visit(dependency);
  }
  visit(entryKey);

  const entry = assetSize(dist, manifest[entryKey].file);
  const initialJs = [...initialKeys]
    .map((key) => manifest[key])
    .filter((item) => item.file.endsWith(".js"))
    .map((item) => assetSize(dist, item.file));
  const initialCss = [...new Set(
    [...initialKeys].flatMap((key) => manifest[key].css ?? []),
  )].map((file) => assetSize(dist, file));
  const allJs = [...new Set(
    Object.values(manifest)
      .map((item) => item.file)
      .filter((file) => file.endsWith(".js")),
  )].map((file) => assetSize(dist, file));
  const allCss = [...new Set(
    Object.values(manifest).flatMap((item) => item.css ?? []),
  )].map((file) => assetSize(dist, file));

  const initialRaw = initialJs.reduce((sum, item) => sum + item.raw, 0);
  const initialGzip = initialJs.reduce((sum, item) => sum + item.gzip, 0);
  const failures = [];
  check("entry raw", entry.raw, limits.entryRaw, failures);
  check("entry gzip", entry.gzip, limits.entryGzip, failures);
  check("initial JS raw", initialRaw, limits.initialRaw, failures);
  check("initial JS gzip", initialGzip, limits.initialGzip, failures);
  // The entry is deliberately excluded: it has its own `entryRaw`/`entryGzip`
  // limits, checked above. Including it here as well meant the generic
  // per-chunk cap always bound first (900 < 1500 raw, 300 < 450 gzip), so the
  // entry's own budget could never fail and those two constants were dead. The
  // entry is one file the app always loads; a lazily-loaded chunk is a cost
  // paid only by whoever opens that surface, which is why the two get
  // different ceilings in the first place.
  for (const chunk of allJs) {
    if (chunk.file === entry.file) continue;
    check(`${chunk.file} raw`, chunk.raw, limits.chunkRaw, failures);
    check(`${chunk.file} gzip`, chunk.gzip, limits.chunkGzip, failures);
  }
  for (const stylesheet of allCss) {
    check(`${stylesheet.file} raw`, stylesheet.raw, limits.cssRaw, failures);
    check(`${stylesheet.file} gzip`, stylesheet.gzip, limits.cssGzip, failures);
  }

  const report = {
    entry: { file: entry.file, raw: kib(entry.raw), gzip: kib(entry.gzip) },
    initial: {
      chunks: initialJs.length,
      raw: kib(initialRaw),
      gzip: kib(initialGzip),
    },
    initialCss: initialCss.map((item) => ({
      file: item.file,
      raw: kib(item.raw),
      gzip: kib(item.gzip),
    })),
    largestChunks: [...allJs]
      .sort((left, right) => right.raw - left.raw)
      .slice(0, 8)
      .map((item) => ({ file: item.file, raw: kib(item.raw), gzip: kib(item.gzip) })),
  };

  return {
    report,
    failures,
    measurements: { entry, initialJs, initialCss, allJs, allCss, initialRaw, initialGzip },
  };
}

export function checkBundleBudget({
  root = process.cwd(),
  dist = resolve(root, "dist"),
  manifestPath = resolve(dist, ".vite", "manifest.json"),
  limits,
} = {}) {
  const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
  return analyzeBundle({ dist, manifest, limits });
}

function parseDistArgument(args) {
  if (args.length === 0) return undefined;
  if (args.length === 2 && args[0] === "--dist") return resolve(args[1]);
  throw new Error("Usage: node scripts/check-bundle-budget.mjs [--dist <directory>]");
}

function runCli() {
  try {
    const dist = parseDistArgument(process.argv.slice(2));
    const result = checkBundleBudget(dist ? { dist, manifestPath: resolve(dist, ".vite", "manifest.json") } : {});
    console.log(JSON.stringify(result.report, null, 2));
    if (result.failures.length) {
      console.error(`Bundle budget failed:\n- ${result.failures.join("\n- ")}`);
      process.exitCode = 1;
    }
  } catch (error) {
    console.error(`Bundle budget check could not run: ${error instanceof Error ? error.message : String(error)}`);
    process.exitCode = 1;
  }
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  runCli();
}

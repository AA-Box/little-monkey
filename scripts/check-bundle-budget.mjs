import { gzipSync } from "node:zlib";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";

const root = process.cwd();
const dist = resolve(root, "dist");
const manifestPath = resolve(dist, ".vite", "manifest.json");
const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));

const LIMITS = {
  entryRaw: 1_500 * 1024,
  entryGzip: 450 * 1024,
  initialRaw: 2_000 * 1024,
  initialGzip: 600 * 1024,
  chunkRaw: 900 * 1024,
  chunkGzip: 300 * 1024,
  cssRaw: 150 * 1024,
  cssGzip: 30 * 1024,
};

function assetSize(file) {
  const path = resolve(dist, file);
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

const entryKey = Object.keys(manifest).find((key) => manifest[key]?.isEntry);
if (!entryKey) throw new Error("Vite manifest has no entry chunk.");

const initialKeys = new Set();
function visit(key) {
  if (initialKeys.has(key)) return;
  const item = manifest[key];
  if (!item) throw new Error(`Manifest import ${key} is missing.`);
  initialKeys.add(key);
  for (const dependency of item.imports ?? []) visit(dependency);
}
visit(entryKey);

const entry = assetSize(manifest[entryKey].file);
const initialJs = [...initialKeys]
  .map((key) => manifest[key])
  .filter((item) => item.file.endsWith(".js"))
  .map((item) => assetSize(item.file));
const initialCss = [...new Set(
  [...initialKeys].flatMap((key) => manifest[key].css ?? []),
)].map(assetSize);
const allJs = [...new Set(
  Object.values(manifest)
    .map((item) => item.file)
    .filter((file) => file.endsWith(".js")),
)].map(assetSize);

const failures = [];
check("entry raw", entry.raw, LIMITS.entryRaw, failures);
check("entry gzip", entry.gzip, LIMITS.entryGzip, failures);
check("initial JS raw", initialJs.reduce((sum, item) => sum + item.raw, 0), LIMITS.initialRaw, failures);
check("initial JS gzip", initialJs.reduce((sum, item) => sum + item.gzip, 0), LIMITS.initialGzip, failures);
for (const chunk of allJs) {
  check(`${chunk.file} raw`, chunk.raw, LIMITS.chunkRaw, failures);
  check(`${chunk.file} gzip`, chunk.gzip, LIMITS.chunkGzip, failures);
}
for (const stylesheet of initialCss) {
  check(`${stylesheet.file} raw`, stylesheet.raw, LIMITS.cssRaw, failures);
  check(`${stylesheet.file} gzip`, stylesheet.gzip, LIMITS.cssGzip, failures);
}

const report = {
  entry: { file: entry.file, raw: kib(entry.raw), gzip: kib(entry.gzip) },
  initial: {
    chunks: initialJs.length,
    raw: kib(initialJs.reduce((sum, item) => sum + item.raw, 0)),
    gzip: kib(initialJs.reduce((sum, item) => sum + item.gzip, 0)),
  },
  largestChunks: [...allJs]
    .sort((left, right) => right.raw - left.raw)
    .slice(0, 8)
    .map((item) => ({ file: item.file, raw: kib(item.raw), gzip: kib(item.gzip) })),
};
console.log(JSON.stringify(report, null, 2));

if (failures.length) {
  console.error(`Bundle budget failed:\n- ${failures.join("\n- ")}`);
  process.exitCode = 1;
}

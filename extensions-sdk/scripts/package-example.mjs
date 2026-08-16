import { createHash } from "node:crypto";
import { mkdir, readFile, realpath, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const sdkRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const allowedExamples = new Set(["simple-tool", "mock-channel", "mock-stt-provider"]);
const [example, revision = "local"] = process.argv.slice(2);

if (!allowedExamples.has(example)) {
  throw new Error(
    "usage: node extensions-sdk/scripts/package-example.mjs " +
      "<simple-tool|mock-channel|mock-stt-provider> [source-revision]",
  );
}

const crateRoot = path.join(sdkRoot, "examples", example);
const targetRoot = process.env.CARGO_TARGET_DIR
  ? path.resolve(process.env.CARGO_TARGET_DIR)
  : path.join(sdkRoot, "target");
const artifactName = `little_monkey_example_${example.replaceAll("-", "_")}.wasm`;
const builtComponent = path.join(targetRoot, "wasm32-wasip2", "release", artifactName);
const output = path.join(sdkRoot, "dist", example);
const component = path.join(output, "component.wasm");

const bytes = await readFile(builtComponent);
await rm(output, { recursive: true, force: true });
await mkdir(output, { recursive: true });
await writeFile(component, bytes);
const digest = createHash("sha256").update(bytes).digest("hex");
const sourcePath = await realpath(output);
const manifest = JSON.parse(
  await readFile(path.join(crateRoot, "extension.template.json"), "utf8"),
);

function fill(value) {
  if (Array.isArray(value)) return value.map(fill);
  if (value && typeof value === "object") {
    return Object.fromEntries(Object.entries(value).map(([key, child]) => [key, fill(child)]));
  }
  if (value === "__COMPONENT_SHA256__") return digest;
  if (value === "__SOURCE_PATH__") return sourcePath;
  if (value === "__SOURCE_REVISION__") return revision;
  return value;
}

await writeFile(
  path.join(output, "extension.json"),
  `${JSON.stringify(fill(manifest), null, 2)}\n`,
  "utf8",
);
process.stdout.write(`${output}\n`);

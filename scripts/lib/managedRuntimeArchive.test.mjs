import assert from "node:assert/strict";
import test from "node:test";

import { managedRuntimeArchiveExtractor } from "./managedRuntimeArchive.mjs";

const archive = "/tmp/sd-runtime.zip";
const extractRoot = "/tmp/extract";

test("extracts ZIP managed-runtime assets with unzip on POSIX", () => {
  assert.deepEqual(managedRuntimeArchiveExtractor(archive, extractRoot, "linux"), [
    "unzip",
    ["-q", archive, "-d", extractRoot],
  ]);
  assert.deepEqual(managedRuntimeArchiveExtractor(archive, extractRoot, "darwin"), [
    "unzip",
    ["-q", archive, "-d", extractRoot],
  ]);
});

test("keeps Windows ZIP and tarball assets on tar", () => {
  assert.deepEqual(managedRuntimeArchiveExtractor(archive, extractRoot, "win32"), [
    "tar",
    ["-xf", archive, "-C", extractRoot],
  ]);
  assert.deepEqual(
    managedRuntimeArchiveExtractor("/tmp/llama-runtime.tar.gz", extractRoot, "linux"),
    ["tar", ["-xf", "/tmp/llama-runtime.tar.gz", "-C", extractRoot]],
  );
});

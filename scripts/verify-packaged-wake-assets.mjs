#!/usr/bin/env node
// Reads a file listing taken from a built installer and asserts that the wake
// word can actually work once that installer has been installed.
//
// The listing, not the repository: `pnpm test:wake-assets` already proves the
// staged directory is byte-exact, and the bundle job's question is a different
// one — whether the bundler carried it. Those diverge for real reasons (a
// resource glob that stops matching, a target whose bundler treats directories
// differently, a file added to the model but not to the packaged path), and
// every one of them ships an application whose Talk panel reports the model
// unavailable on a machine that has never seen a network.
//
// The required names come from `stage-wake-word-model.mjs`, so a model file
// added there is required here without anybody remembering to add it: a list of
// filenames repeated in YAML is a list that drifts.
//
// Any tool's listing will do as long as it prints one path per line —
// `dpkg-deb -c`, `find` over a .app, `7z l` over an NSIS installer. Windows
// separators are normalized, and surrounding columns (permissions, sizes,
// dates) are ignored because only the path is matched.
//
//   node scripts/verify-packaged-wake-assets.mjs <listing-file>
//   … | node scripts/verify-packaged-wake-assets.mjs

import { readFileSync } from "node:fs";

import { MODEL_FILES } from "./stage-wake-word-model.mjs";

/**
 * What is wrong with this installer's listing, or `null` if nothing is.
 *
 * Returns a sentence rather than throwing or exiting so the same rules can be
 * asserted directly in `scripts/lib/wakeWordAssets.test.mjs`; the CLI below is
 * only an exit code around it.
 */
export function packagedWakeAssetProblem(rawListing) {
  const listing = rawListing.replaceAll("\\", "/");
  const required = Object.keys(MODEL_FILES);
  const missing = required.filter((file) => !listing.includes(`local-wake-word/${file}`));

  // A listing that matched nothing at all is far more likely to be a tool this
  // script could not read than an installer with no resources in it, and the
  // two failures need different fixes.
  if (missing.length === required.length) {
    return (
      `found none of the ${required.length} model files in this listing`
      + ` (${listing.length} bytes, ${listing.split("\n").length} lines). Either the`
      + " bundler packaged no resources, or this is not a list of paths."
    );
  }
  if (missing.length > 0) {
    return `the installer is missing ${missing.length} required model file(s): ${missing.join(", ")}`;
  }

  // The packaged resource is a directory glob, so "the installer carries no
  // test audio" is only true while nothing stages test audio into that
  // directory. The fixtures live in their own unpackaged directory precisely so
  // this holds; this is the assertion that keeps it holding.
  const testAudio = listing
    .split("\n")
    .filter((line) => /local-wake-word\/.*\.wav/i.test(line));
  if (testAudio.length > 0) {
    return `test audio was packaged into the installer:\n${testAudio.join("\n")}`;
  }
  return null;
}

// `import.meta.main` is Node 24; this repository's scripts still run under
// older locals, so compare the entry path instead.
if (process.argv[1]?.endsWith("verify-packaged-wake-assets.mjs")) {
  const listingPath = process.argv[2];
  const problem = packagedWakeAssetProblem(readFileSync(listingPath ?? 0, "utf8"));
  if (problem) {
    console.error(`[verify-packaged-wake-assets] ${listingPath ?? "piped listing"}: ${problem}`);
    process.exit(1);
  }
  const required = Object.keys(MODEL_FILES);
  console.log(
    `[verify-packaged-wake-assets] all ${required.length} model files packaged, no test audio:\n`
      + required.map((file) => `  local-wake-word/${file}`).join("\n"),
  );
}

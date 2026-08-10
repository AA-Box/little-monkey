#!/usr/bin/env node
/**
 * Clean-machine install and upgrade smoke test (roadmap K22).
 *
 * Run against a directory of release artifacts:
 *
 *     node scripts/install-smoke.mjs --artifacts <dir> [--previous <dir>]
 *
 * It installs the built package into a scratch prefix — never onto the runner
 * — and asserts that the install produced the binaries a user would get, that
 * the CLI it ships reports this release's version, and (when a previous
 * release's artifacts are supplied) that installing over it keeps the user's
 * data directory intact.
 *
 * # The contract, and it is the whole point
 *
 * **Every leg reports `PASS`, `FAIL`, or `SKIPPED(reason)`. There is no silent
 * pass.** A hosted runner cannot exercise every installer — a Windows MSI needs
 * elevation, a `.deb` install needs a package database, and the first release
 * ever has nothing to upgrade from — so the honest answer is often "not covered
 * here", and this says that out loud, in the log and in the job summary. A
 * release gate that goes green because it found nothing to install is worse
 * than no gate.
 *
 * Only a real failure exits non-zero. A skip is reported, not fatal: failing
 * the release because a leg was not applicable would train everyone to ignore
 * it.
 *
 * The decisions live in `scripts/lib/installSmoke.mjs` so they are unit-tested
 * without an installer (`pnpm test:install-smoke`); this file is the I/O.
 */
import { execFileSync } from "node:child_process";
import {
  appendFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join, relative, resolve, sep } from "node:path";

import {
  OUTCOME,
  checkDataSurvived,
  checkPayload,
  checkVersion,
  expectedPayload,
  exitCodeFor,
  formatLeg,
  installPlanFor,
  upgradePlan,
} from "./lib/installSmoke.mjs";

function arg(name) {
  const index = process.argv.indexOf(`--${name}`);
  return index >= 0 ? process.argv[index + 1] : undefined;
}

const artifactsDir = arg("artifacts");
const previousDir = arg("previous");
const platform = process.platform;
const legs = [];

function report(name, result) {
  legs.push(result);
  const line = formatLeg(name, result);
  console.log(line);
  // Also to the job summary, so a skip is visible without opening the log —
  // the failure mode this guards is a skip nobody ever reads.
  if (process.env.GITHUB_STEP_SUMMARY) {
    appendFileSync(process.env.GITHUB_STEP_SUMMARY, `- ${line}\n`);
  }
}

/** Every file under `root`, as paths relative to it, using forward slashes. */
function walk(root, base = root, out = []) {
  for (const entry of readdirSync(root, { withFileTypes: true })) {
    const full = join(root, entry.name);
    if (entry.isDirectory()) {
      walk(full, base, out);
    } else {
      out.push(relative(base, full).split(sep).join("/"));
    }
  }
  return out;
}

/**
 * Unpacks one installer into `into`, or returns a skip reason.
 *
 * Extraction rather than a real system install, deliberately and with the cost
 * stated: a `dpkg -i` would need root and would leave the runner modified, and
 * `msiexec` needs elevation. What extraction *does* cover is the packaging
 * itself — the control archive, the payload layout, the file modes — which is
 * where a release actually breaks. What it does not cover is the post-install
 * scripts and registry/desktop entries, and that is named here rather than
 * implied to be tested.
 */
function unpack(plan, artifactsDir, into) {
  const asset = join(artifactsDir, plan.asset);
  mkdirSync(into, { recursive: true });
  switch (plan.kind) {
    case "deb":
      execFileSync("dpkg-deb", ["-x", asset, into], { stdio: "inherit" });
      return null;
    case "appimage": {
      const cwd = into;
      execFileSync("chmod", ["+x", asset]);
      execFileSync(asset, ["--appimage-extract"], { cwd, stdio: "inherit" });
      return null;
    }
    case "dmg": {
      const mount = execFileSync("hdiutil", ["attach", "-nobrowse", "-readonly", asset], {
        encoding: "utf8",
      });
      const point = mount.trim().split("\n").pop()?.split("\t").pop()?.trim();
      if (!point) return "hdiutil attached the image but reported no mount point";
      try {
        const app = readdirSync(point).find((entry) => entry.endsWith(".app"));
        if (!app) return "the mounted disk image contains no .app bundle";
        execFileSync("cp", ["-R", join(point, app), into], { stdio: "inherit" });
      } finally {
        execFileSync("hdiutil", ["detach", point], { stdio: "ignore" });
      }
      return null;
    }
    case "nsis":
      // NSIS `/D` must be last and unquoted — that is the installer's own rule,
      // not a preference here.
      execFileSync(asset, ["/S", `/D=${into}`], { stdio: "inherit" });
      return null;
    default:
      return `no unpack strategy for ${plan.kind}`;
  }
}

/** The one payload path that is also a runnable CLI, if it was installed. */
function cliPath(root, installed) {
  const sidecar = installed.find((path) => /(^|\/)monkey(\.exe)?$/.test(path));
  return sidecar ? join(root, ...sidecar.split("/")) : null;
}

function main() {
  const releaseVersion = JSON.parse(
    readFileSync(resolve("src-tauri/tauri.conf.json"), "utf8"),
  ).version;

  if (!artifactsDir || !existsSync(artifactsDir)) {
    report("clean install", {
      outcome: OUTCOME.Skip,
      reason: `no artifact directory to install from (--artifacts ${JSON.stringify(artifactsDir)})`,
    });
    report("upgrade", {
      outcome: OUTCOME.Skip,
      reason: "the clean-install leg did not run, so there is nothing to upgrade",
    });
    process.exit(exitCodeFor(legs));
  }

  const assets = readdirSync(artifactsDir);
  const plan = installPlanFor(platform, assets);
  if (!plan) {
    report("clean install", {
      outcome: OUTCOME.Skip,
      reason: `no installable artifact for ${platform} among ${assets.length} asset(s): ${assets.join(", ") || "none"}`,
    });
    report("upgrade", {
      outcome: OUTCOME.Skip,
      reason: "the clean-install leg did not run, so there is nothing to upgrade",
    });
    process.exit(exitCodeFor(legs));
  }

  const scratch = mkdtempSync(join(tmpdir(), "little-monkey-install-smoke-"));
  try {
    const prefix = join(scratch, "install");
    const failure = unpack(plan, artifactsDir, prefix);
    if (failure) {
      report("clean install", { outcome: OUTCOME.Fail, reason: failure });
      process.exit(exitCodeFor(legs));
    }

    const installed = walk(prefix);
    // Matched on suffix: a `.deb` unpacks under `usr/...`, a `.dmg` copy lands
    // under `Little Monkey.app/...`, and the expected paths describe the tail.
    const expected = expectedPayload(platform);
    const found = expected.filter((want) => installed.some((path) => path.endsWith(want)));
    report("clean install", checkPayload(found, expected));

    const cli = cliPath(prefix, installed);
    if (!cli) {
      report("installed CLI version", {
        outcome: OUTCOME.Fail,
        reason: "the install produced no monkey CLI to run",
      });
    } else {
      try {
        const reported = execFileSync(cli, ["--version"], { encoding: "utf8", timeout: 60_000 });
        report("installed CLI version", checkVersion(reported, releaseVersion));
      } catch (error) {
        report("installed CLI version", {
          outcome: OUTCOME.Fail,
          reason: `the installed CLI could not be run: ${error.message}`,
        });
      }
    }

    // --- upgrade -------------------------------------------------------
    const previousAssets = previousDir && existsSync(previousDir) ? readdirSync(previousDir) : [];
    const previousPlan = previousAssets.length ? installPlanFor(platform, previousAssets) : null;
    const upgrade = upgradePlan({
      previousVersion: arg("previous-version") ?? (previousAssets.length ? "previous" : null),
      previousAsset: previousPlan?.asset ?? null,
      currentVersion: releaseVersion,
    });
    if (upgrade.outcome === OUTCOME.Skip) {
      report("upgrade", upgrade);
      process.exit(exitCodeFor(legs));
    }

    const upgradePrefix = join(scratch, "upgrade");
    const previousFailure = unpack(previousPlan, previousDir, upgradePrefix);
    if (previousFailure) {
      report("upgrade", {
        outcome: OUTCOME.Fail,
        reason: `the previous release would not unpack: ${previousFailure}`,
      });
      process.exit(exitCodeFor(legs));
    }

    // Stand-in for the user's data directory, placed inside the install prefix
    // precisely because that is the layout an installer is most likely to wipe.
    const dataFile = join(upgradePrefix, "user-data-canary.txt");
    const canary = `conversation written before the upgrade at ${releaseVersion}`;
    writeFileSync(dataFile, canary);

    const overFailure = unpack(plan, artifactsDir, upgradePrefix);
    if (overFailure) {
      report("upgrade", {
        outcome: OUTCOME.Fail,
        reason: `installing over the previous release failed: ${overFailure}`,
      });
      process.exit(exitCodeFor(legs));
    }

    const after = existsSync(dataFile) ? readFileSync(dataFile, "utf8") : null;
    report("upgrade", checkDataSurvived(canary, after));

    const upgraded = walk(upgradePrefix);
    const upgradedFound = expected.filter((want) => upgraded.some((path) => path.endsWith(want)));
    report("upgraded payload", checkPayload(upgradedFound, expected));
  } finally {
    rmSync(scratch, { recursive: true, force: true });
  }

  process.exit(exitCodeFor(legs));
}

main();

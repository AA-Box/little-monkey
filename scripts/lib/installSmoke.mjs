/**
 * Decision logic for the clean-machine install/upgrade smoke test (roadmap K22).
 *
 * Split from the runner (`scripts/install-smoke.mjs`) so the parts that decide
 * *what to do* are testable without an installer, a runner, or root — the same
 * `scripts/lib/*.mjs` + `node --test` split `machO` and `mlxPackage` already
 * use. The runner does the I/O; everything below is pure.
 *
 * # The rule this file exists to enforce
 *
 * **A leg that cannot run reports SKIPPED with its reason. It never reports a
 * pass.** A release gate that goes green because it found nothing to install is
 * worse than no gate: it is a gate that will keep being green after it breaks.
 * So every outcome here is one of `pass`, `fail`, or `skip` *with* a reason
 * string, and there is no fourth state a caller could read as success.
 */

/** What one leg of the smoke test concluded. */
export const OUTCOME = Object.freeze({
  Pass: "pass",
  Fail: "fail",
  Skip: "skip",
});

/**
 * Which installer this platform's clean-install leg uses, and how.
 *
 * `null` for a platform whose installer cannot be exercised on a hosted runner
 * — the caller turns that into a SKIPPED with the reason attached, rather than
 * quietly doing nothing.
 */
export function installPlanFor(platform, assets) {
  const named = (suffix) => assets.find((asset) => asset.toLowerCase().endsWith(suffix));

  if (platform === "linux") {
    // `.deb` first: `dpkg-deb -x` unpacks the real package payload with no root
    // and no package database, which exercises the packaging (control archive,
    // paths, permissions) rather than a tarball we made ourselves.
    const deb = named(".deb");
    if (deb) {
      return { kind: "deb", asset: deb, command: "dpkg-deb", args: ["-x"] };
    }
    const appImage = named(".appimage");
    if (appImage) {
      return { kind: "appimage", asset: appImage, command: "appimage", args: ["--appimage-extract"] };
    }
    return null;
  }

  if (platform === "darwin") {
    const dmg = named(".dmg");
    if (dmg) {
      return { kind: "dmg", asset: dmg, command: "hdiutil", args: ["attach", "-nobrowse", "-readonly"] };
    }
    return null;
  }

  if (platform === "win32") {
    // NSIS supports a silent install into a chosen prefix, which is what makes
    // this runnable unattended. The MSI is not used: `msiexec` needs elevation
    // for a per-machine install and the per-user path differs enough that a
    // green here would not describe what a user gets.
    const setup = named("-setup.exe");
    if (setup) {
      return { kind: "nsis", asset: setup, command: "setup", args: ["/S"] };
    }
    return null;
  }

  return null;
}

/**
 * The files a clean install must produce, by platform.
 *
 * The app binary **and** the `monkey` CLI sidecar: the sidecar is the half a
 * packaging mistake actually loses (it is staged by a separate build step), and
 * it is also the only half this test can *run* — the app itself is a GUI.
 */
export function expectedPayload(platform) {
  switch (platform) {
    case "linux":
      return ["usr/bin/little-monkey", "usr/lib/little-monkey/monkey"];
    case "darwin":
      return ["Contents/MacOS/little-monkey", "Contents/Resources/monkey"];
    case "win32":
      return ["little-monkey.exe", "monkey.exe"];
    default:
      return [];
  }
}

/**
 * Turns a set of found/missing payload paths into an outcome.
 *
 * Missing files are a **fail**, never a skip: the installer ran, so this is a
 * real answer about a real install.
 */
export function checkPayload(found, expected) {
  const missing = expected.filter((path) => !found.includes(path));
  if (missing.length > 0) {
    return {
      outcome: OUTCOME.Fail,
      reason: `the install is missing ${missing.length} expected file(s): ${missing.join(", ")}`,
    };
  }
  return { outcome: OUTCOME.Pass, reason: `all ${expected.length} expected files installed` };
}

/**
 * Whether the reported version matches the version being released.
 *
 * Compared on the *normalised* string because installers spell it differently:
 * a Windows product version is `1.2.0.0`, a tag may carry a leading `v`, and a
 * CLI prints `little-monkey 1.2.0`. A mismatch after normalising is a genuine
 * "the installer shipped the wrong build", which is the whole point of asking.
 */
export function normalizeVersion(raw) {
  const match = String(raw ?? "").match(/(\d+)\.(\d+)\.(\d+)/);
  if (!match) return null;
  return `${match[1]}.${match[2]}.${match[3]}`;
}

export function checkVersion(reported, expected) {
  const got = normalizeVersion(reported);
  const want = normalizeVersion(expected);
  if (!want) {
    return {
      outcome: OUTCOME.Skip,
      reason: `no release version to compare against (got ${JSON.stringify(expected)})`,
    };
  }
  if (!got) {
    return {
      outcome: OUTCOME.Fail,
      reason: `the installed binary reported no parseable version (${JSON.stringify(reported)})`,
    };
  }
  if (got !== want) {
    return {
      outcome: OUTCOME.Fail,
      reason: `the installed binary reports ${got}, but this release is ${want}`,
    };
  }
  return { outcome: OUTCOME.Pass, reason: `the installed binary reports ${got}` };
}

/**
 * Decides the upgrade leg, which needs a previous release to upgrade *from*.
 *
 * The first release ever, and any run where the previous release published no
 * asset for this platform, legitimately have nothing to test — and say so.
 */
export function upgradePlan({ previousVersion, previousAsset, currentVersion }) {
  if (!previousVersion) {
    return {
      outcome: OUTCOME.Skip,
      reason: "no previous release exists to upgrade from",
    };
  }
  if (!previousAsset) {
    return {
      outcome: OUTCOME.Skip,
      reason: `the previous release (${previousVersion}) published no installer for this platform`,
    };
  }
  if (normalizeVersion(previousVersion) === normalizeVersion(currentVersion)) {
    return {
      outcome: OUTCOME.Skip,
      reason: `the previous release is the same version (${previousVersion}) — nothing to upgrade`,
    };
  }
  return {
    outcome: null,
    from: previousVersion,
    to: currentVersion,
    asset: previousAsset,
  };
}

/**
 * Whether a file written under the user's data directory survived the upgrade.
 *
 * The one property an upgrade test is really for. An installer that replaces
 * the binaries but wipes `<app_data>` loses every conversation, and nothing
 * else in this pipeline would notice.
 */
export function checkDataSurvived(before, after) {
  if (before !== after) {
    return {
      outcome: OUTCOME.Fail,
      reason: "user data did not survive the upgrade — the installer replaced or removed it",
    };
  }
  return { outcome: OUTCOME.Pass, reason: "user data survived the upgrade unchanged" };
}

/**
 * Renders one leg's result as a single, greppable line.
 *
 * `SKIPPED(reason)` rather than silence is the contract: a human scanning a
 * release log must be able to see that a leg did not run *and why*, and a
 * machine must be able to count skips.
 */
export function formatLeg(name, { outcome, reason }) {
  switch (outcome) {
    case OUTCOME.Pass:
      return `PASS ${name}: ${reason}`;
    case OUTCOME.Fail:
      return `FAIL ${name}: ${reason}`;
    case OUTCOME.Skip:
      return `SKIPPED ${name}: ${reason}`;
    default:
      // An unnamed outcome is itself a bug, and reporting it as a pass is the
      // failure this whole module is shaped to prevent.
      return `FAIL ${name}: the leg produced no outcome (${JSON.stringify(outcome)})`;
  }
}

/**
 * The exit code for a whole run: non-zero only for a real failure.
 *
 * A skip does not fail the release. It is *reported*, which is the difference
 * between "we know this is not covered here" and "we think this is covered".
 */
export function exitCodeFor(legs) {
  return legs.some((leg) => leg.outcome === OUTCOME.Fail) ? 1 : 0;
}

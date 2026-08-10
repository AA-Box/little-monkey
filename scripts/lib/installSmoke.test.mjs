/**
 * The install/upgrade smoke test's decision logic (roadmap K22).
 *
 * Run with: pnpm test:install-smoke
 *
 * These assert the property the whole thing exists for: **a leg that cannot run
 * says SKIPPED with its reason, and never reports a pass.** A release gate that
 * goes green because it found nothing to install is worse than no gate, so the
 * "nothing to do" paths get more attention here than the happy one.
 */
import assert from "node:assert/strict";
import { test } from "node:test";

import {
  OUTCOME,
  checkDataSurvived,
  checkPayload,
  checkVersion,
  exitCodeFor,
  expectedPayload,
  formatLeg,
  installPlanFor,
  normalizeVersion,
  upgradePlan,
} from "./installSmoke.mjs";

test("each platform picks the installer a hosted runner can actually exercise", () => {
  const linux = installPlanFor("linux", ["app_1.2.0_amd64.deb", "app_1.2.0.AppImage"]);
  assert.equal(linux.kind, "deb", "the .deb exercises real packaging without root");

  // Falls back rather than giving up, because an AppImage-only build is a
  // configuration this repo supports.
  assert.equal(installPlanFor("linux", ["app.AppImage"]).kind, "appimage");

  assert.equal(installPlanFor("darwin", ["app_aarch64.dmg"]).kind, "dmg");
  assert.equal(installPlanFor("win32", ["app_1.2.0_x64-setup.exe"]).kind, "nsis");

  // The MSI is deliberately not used — it needs elevation — so an MSI-only set
  // has no plan, which the caller turns into a SKIPPED with a reason.
  assert.equal(installPlanFor("win32", ["app_en-US.msi"]), null);
  assert.equal(installPlanFor("linux", []), null);
  assert.equal(installPlanFor("freebsd", ["app.pkg"]), null);
});

test("a missing payload file is a failure, not a skip", () => {
  // The installer ran, so this is a real answer about a real install.
  const expected = expectedPayload("linux");
  const result = checkPayload(["usr/bin/little-monkey"], expected);
  assert.equal(result.outcome, OUTCOME.Fail);
  assert.match(result.reason, /usr\/lib\/little-monkey\/monkey/);

  assert.equal(checkPayload(expected, expected).outcome, OUTCOME.Pass);
});

test("every platform names the CLI sidecar, which is the half packaging loses", () => {
  for (const platform of ["linux", "darwin", "win32"]) {
    const payload = expectedPayload(platform);
    assert.ok(payload.length > 0, `${platform} expects nothing`);
    assert.ok(
      payload.some((path) => /monkey(\.exe)?$/.test(path)),
      `${platform} does not check for the CLI sidecar`,
    );
  }
});

test("versions are compared after normalising the ways installers spell them", () => {
  assert.equal(normalizeVersion("1.2.0"), "1.2.0");
  // A Windows product version, a tag, and a CLI banner.
  assert.equal(normalizeVersion("1.2.0.0"), "1.2.0");
  assert.equal(normalizeVersion("v1.2.0"), "1.2.0");
  assert.equal(normalizeVersion("little-monkey 1.2.0 (abc1234)"), "1.2.0");
  assert.equal(normalizeVersion("not a version"), null);
  assert.equal(normalizeVersion(undefined), null);

  assert.equal(checkVersion("little-monkey 1.2.0", "1.2.0").outcome, OUTCOME.Pass);
  // The failure worth catching: the installer shipped a different build.
  const wrong = checkVersion("little-monkey 1.1.0", "1.2.0");
  assert.equal(wrong.outcome, OUTCOME.Fail);
  assert.match(wrong.reason, /1\.1\.0.*1\.2\.0/);

  // A binary that printed nothing parseable is a failure — it ran and answered
  // wrong — while having no release version to compare against is a skip.
  assert.equal(checkVersion("", "1.2.0").outcome, OUTCOME.Fail);
  assert.equal(checkVersion("1.2.0", undefined).outcome, OUTCOME.Skip);
});

test("the upgrade leg skips with a reason rather than passing vacuously", () => {
  const first = upgradePlan({ previousVersion: null, previousAsset: null, currentVersion: "1.0.0" });
  assert.equal(first.outcome, OUTCOME.Skip);
  assert.match(first.reason, /no previous release/);

  const noAsset = upgradePlan({
    previousVersion: "1.1.0",
    previousAsset: null,
    currentVersion: "1.2.0",
  });
  assert.equal(noAsset.outcome, OUTCOME.Skip);
  assert.match(noAsset.reason, /published no installer/);

  const same = upgradePlan({
    previousVersion: "1.2.0",
    previousAsset: "app.deb",
    currentVersion: "1.2.0.0",
  });
  assert.equal(same.outcome, OUTCOME.Skip, "re-running one release is not an upgrade");

  const real = upgradePlan({
    previousVersion: "1.1.0",
    previousAsset: "app_1.1.0_amd64.deb",
    currentVersion: "1.2.0",
  });
  assert.equal(real.outcome, null, "a runnable leg has no outcome yet — it has to be run");
  assert.equal(real.asset, "app_1.1.0_amd64.deb");
});

test("losing user data across an upgrade is a failure", () => {
  const written = "a conversation";
  assert.equal(checkDataSurvived(written, written).outcome, OUTCOME.Pass);
  const wiped = checkDataSurvived(written, null);
  assert.equal(wiped.outcome, OUTCOME.Fail);
  assert.match(wiped.reason, /did not survive/);
});

test("a skip is reported loudly and an unnamed outcome is never a pass", () => {
  assert.match(
    formatLeg("upgrade", { outcome: OUTCOME.Skip, reason: "no previous release" }),
    /^SKIPPED upgrade: no previous release$/,
  );
  assert.match(formatLeg("clean install", { outcome: OUTCOME.Pass, reason: "ok" }), /^PASS /);
  assert.match(formatLeg("clean install", { outcome: OUTCOME.Fail, reason: "bad" }), /^FAIL /);

  // The guard against a fourth state creeping in: anything unrecognised is a
  // failure, because reporting it as a pass is exactly what this must not do.
  assert.match(formatLeg("mystery", { outcome: undefined, reason: "" }), /^FAIL /);
});

test("only a real failure fails the release", () => {
  assert.equal(exitCodeFor([{ outcome: OUTCOME.Pass }, { outcome: OUTCOME.Skip }]), 0);
  assert.equal(exitCodeFor([{ outcome: OUTCOME.Skip }, { outcome: OUTCOME.Skip }]), 0);
  assert.equal(exitCodeFor([{ outcome: OUTCOME.Pass }, { outcome: OUTCOME.Fail }]), 1);
});

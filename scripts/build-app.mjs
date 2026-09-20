#!/usr/bin/env node
// `tauri build`, signed with a real identity when this machine has one.
//
// Ad-hoc signing (`signingIdentity: "-"`, the repo default) gives the bundle a
// designated requirement built from its cdhash, and the cdhash changes with
// every byte of the binary. TCC stores its decisions against that requirement,
// so each build that actually changed something arrives as a different program
// and the microphone grant from ten minutes ago no longer applies — then the
// permission has to be given again to test the change that needed it.
//
// A Developer ID certificate is stable, so the requirement is stable, and one
// grant covers every later build. Nothing here signs anything itself: Tauri's
// bundler reads APPLE_SIGNING_IDENTITY, and this only fills it in when the
// machine has an identity and the environment has not already chosen one. With
// neither, the build is ad-hoc exactly as before — no developer is required to
// own a certificate.
import { execFileSync, spawnSync } from "node:child_process";

function developerIdIdentity() {
  try {
    const listed = execFileSync("/usr/bin/security", ["find-identity", "-v", "-p", "codesigning"], {
      encoding: "utf8",
    });
    // `  1) <sha1> "Developer ID Application: Name (TEAMID)"`
    const found = listed.match(/"(Developer ID Application:[^"]+)"/);
    return found?.[1] ?? null;
  } catch {
    // No `security`, no keychain, or nothing in it. Ad-hoc is still a build.
    return null;
  }
}

const identity = process.env.APPLE_SIGNING_IDENTITY
  ?? (process.platform === "darwin" ? developerIdIdentity() : null);
console.log(identity ? `Signing with ${identity}` : "Signing ad-hoc: no Developer ID identity found");

const result = spawnSync(
  "pnpm",
  ["tauri", "build", ...process.argv.slice(2)],
  { stdio: "inherit", env: identity ? { ...process.env, APPLE_SIGNING_IDENTITY: identity } : process.env },
);
process.exit(result.status ?? 1);

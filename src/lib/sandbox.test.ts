/**
 * Two things, and the second is the one a linter cannot see.
 *
 * `SandboxPanel` renders its pre-run warning through a *computed* key,
 * `SandboxPanel.enforcement.${enforcement}`. `i18n:lint` scans for literal keys,
 * so a missing entry here would ship as the raw key string rendered inside a
 * security warning — the worst place for it. Enumerating the variants in a test
 * is what makes that reachable.
 */
import { describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
  isTauri: () => true,
}));

import { sandboxEnforcement, type SandboxEnforcement } from "./sandbox";
import { sandboxLocale } from "./i18n/locales/sandbox";

describe("the enforcement probe", () => {
  it("asks the backend rather than inferring from the platform", async () => {
    // Inferring from `navigator.platform` is the tempting shortcut and is wrong:
    // on macOS the answer depends on whether `sandbox-exec` is actually present,
    // which only the backend can see.
    invokeMock.mockResolvedValue("os_enforced");
    await expect(sandboxEnforcement()).resolves.toBe("os_enforced");
    expect(invokeMock).toHaveBeenCalledWith("sandbox_enforcement_probe");
  });
});

describe("the pre-run warning strings", () => {
  /** Every state the panel warns about — `os_enforced` renders no banner. */
  const WARNED: readonly Exclude<SandboxEnforcement, "os_enforced">[] = [
    "process_contained",
    "process_only",
    "unavailable",
  ];

  it("has a string for every state that renders a banner", () => {
    for (const state of WARNED) {
      const key = `SandboxPanel.enforcement.${state}`;
      expect(sandboxLocale[key], key).toBeTruthy();
    }
  });

  it("says what is not protected rather than only that something is missing", () => {
    // A warning that names the mechanism but not the consequence tells a user
    // nothing they can act on. The process-only case is the one people actually
    // hit, and the consequence is that absolute paths still reach real files.
    expect(sandboxLocale["SandboxPanel.enforcement.process_only"]).toMatch(/absolute path/i);
    expect(sandboxLocale["SandboxPanel.enforcement.unavailable"]).toMatch(
      /fail to start|rather than run unconfined/i,
    );
    // The contained state is the one most likely to be misread as safe, so its
    // string has to say both halves: what the kernel holds, and what it does not.
    expect(sandboxLocale["SandboxPanel.enforcement.process_contained"]).toMatch(/job object/i);
    expect(sandboxLocale["SandboxPanel.enforcement.process_contained"]).toMatch(/absolute path/i);
  });
});

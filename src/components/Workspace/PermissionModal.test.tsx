import { describe, expect, it } from "vitest";

import type { PermissionRequest } from "../../store/permissionStore";
import { canRememberForSession } from "./PermissionModal";

function prompt(overrides: Partial<PermissionRequest> = {}): PermissionRequest {
  return { id: "req-1", tool: "edit_file", detail: "edit src/util.ts", ...overrides };
}

describe("canRememberForSession", () => {
  it("offers the grant for an ordinary file edit", () => {
    expect(canRememberForSession(prompt())).toBe(true);
  });

  it("withholds it for run_shell", () => {
    expect(canRememberForSession(prompt({ tool: "run_shell", detail: "npm test" }))).toBe(false);
  });

  it("withholds it for a floored path", () => {
    // Backend side this is enforced twice over — `respond_if_pending` stores
    // no grant for a floored prompt and `evaluate_gate` refuses to honour a
    // pre-existing one (permissions.rs). Hiding the button keeps the modal
    // from offering something that would silently do nothing.
    const floored = prompt({
      detail: "edit pyproject.toml",
      risk_level: "high",
      risk_reason: "package manifest/lockfile that can execute scripts on install/build",
      risk_floored: true,
    });
    expect(canRememberForSession(floored)).toBe(false);
  });

  it("still offers it for a high-risk edit the judge — not the floor — flagged", () => {
    // Only the deterministic floor withholds the grant. A judge-supplied
    // "high" is advisory and must not change what the modal offers, or a
    // mis-classification would start silently removing an affordance.
    const judged = prompt({ risk_level: "high", risk_reason: "looks risky", risk_floored: false });
    expect(canRememberForSession(judged)).toBe(true);
  });
});

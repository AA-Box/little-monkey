import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";

import { APPROVAL_LAYER } from "./overlayLayers";

/** Every `.tsx` under `src/components`, recursively. */
function componentFiles(directory: string): string[] {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) return componentFiles(path);
    return entry.name.endsWith(".tsx") && !entry.name.includes(".test.") ? [path] : [];
  });
}

/** The number in a `z-50` / `z-[100]` class, if the line carries one. */
function overlayLayer(line: string): number | null {
  const match = /\bz-\[?(\d+)\]?/.exec(line);
  return match ? Number(match[1]) : null;
}

const APPROVAL_DEPTH = Number(/\d+/.exec(APPROVAL_LAYER)?.[0]);

describe("overlay stacking", () => {
  it("keeps every blocking approval above every other full-screen overlay", () => {
    const offenders: string[] = [];
    for (const file of componentFiles(join(process.cwd(), "src", "components"))) {
      for (const line of readFileSync(file, "utf8").split("\n")) {
        if (!line.includes("fixed inset-0")) continue;
        const depth = overlayLayer(line);
        // An approval is allowed to sit at the approval layer; nothing else may
        // reach it. Without this, a permission prompt raised from inside a
        // dialog renders behind the dialog that raised it, and the only way to
        // answer it is to close the window you were working in.
        if (depth !== null && depth >= APPROVAL_DEPTH && !line.includes("APPROVAL_LAYER")) {
          offenders.push(`${file.replace(process.cwd(), "")}: ${line.trim().slice(0, 80)}`);
        }
      }
    }
    expect(offenders).toEqual([]);
  });

  it("puts the prompts a run is blocked on at that layer", () => {
    for (const file of [
      "src/components/Workspace/PermissionModal.tsx",
      "src/components/Chat/SkillActivationApprovalModal.tsx",
      "src/components/Chat/PrivacyFirewallGate.tsx",
    ]) {
      expect(readFileSync(join(process.cwd(), file), "utf8")).toContain("APPROVAL_LAYER");
    }
  });
});

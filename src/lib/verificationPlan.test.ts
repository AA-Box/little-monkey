import { describe, expect, it } from "vitest";

import { planVerificationCommands } from "./verificationPlan";
import type { VerifyCommand, VerifyConfig } from "../store/verifyStore";

function command(id: string, enabled = true): VerifyCommand {
  return {
    id,
    label: id,
    command: `echo ${id}`,
    kind: "custom",
    enabled,
  };
}

describe("planVerificationCommands", () => {
  it("runs a Standards-bound checker even when global Verification is off", () => {
    const config: VerifyConfig = { commands: [command("global"), command("standard")] };
    const plan = planVerificationCommands(config, false, ["standard"]);
    expect(plan.commands.map((entry) => entry.id)).toEqual(["standard"]);
    expect(plan.missingRequiredIds).toEqual([]);
  });

  it("deduplicates Standards-bound checkers already selected by global Verification", () => {
    const config: VerifyConfig = { commands: [command("a"), command("b")] };
    const plan = planVerificationCommands(config, true, ["b", "b"]);
    expect(plan.commands.map((entry) => entry.id)).toEqual(["a", "b"]);
  });

  it("reports missing or disabled Standards-bound checkers as gate failures", () => {
    const config: VerifyConfig = { commands: [command("disabled", false)] };
    const plan = planVerificationCommands(config, false, ["missing", "disabled"]);
    expect(plan.commands).toEqual([]);
    expect(plan.missingRequiredIds).toEqual(["missing", "disabled"]);
  });
});

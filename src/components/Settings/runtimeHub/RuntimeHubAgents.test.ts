import { describe, expect, it } from "vitest";

import type { AgentWarningKind } from "../../../lib/runtimeHubClient";
import { AGENT_TOOLS, warningTone } from "./RuntimeHubAgents";

describe("warningTone", () => {
  it("treats connection-breaking findings as danger", () => {
    const breaking: AgentWarningKind[] = ["auth", "auth_drift", "model_missing", "endpoint_drift"];
    for (const kind of breaking) {
      expect(warningTone(kind)).toBe("danger");
    }
  });

  it("treats informational/preference findings as warning", () => {
    const informational: AgentWarningKind[] = ["context_length", "telemetry"];
    for (const kind of informational) {
      expect(warningTone(kind)).toBe("warning");
    }
  });
});

describe("AGENT_TOOLS", () => {
  it("lists exactly the tool formats this launcher supports, each with a real filename", () => {
    expect(AGENT_TOOLS.map((entry) => entry.value)).toEqual(["continue_dev", "aider", "openai_env"]);
    expect(AGENT_TOOLS.map((entry) => entry.filename)).toEqual([
      ".continue/config.yaml",
      ".aider.conf.yml",
      ".env",
    ]);
  });
});

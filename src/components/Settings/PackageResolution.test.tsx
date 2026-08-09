import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import { ResolutionSection } from "./PackageResolution";
import type { InstallPlan } from "../../lib/ecosystemClient";

/**
 * Rendered as a bare section rather than through the whole preview dialog:
 * this component takes its plan as a prop, so what is asserted here is
 * exactly the text a user reads before approving or being refused.
 */

function plan(overrides: Partial<InstallPlan> = {}): InstallPlan {
  return {
    package_id: "com.littlemonkey.assistant.reviewer",
    version: "1.0.0",
    steps: [
      { package_id: "com.littlemonkey.skill.review", version: "1.0.0", action: "install", required_by: ["com.littlemonkey.assistant.reviewer"] },
      { package_id: "com.littlemonkey.assistant.reviewer", version: "1.0.0", action: "install", required_by: [] },
    ],
    problems: [],
    satisfiable: true,
    ...overrides,
  };
}

describe("ResolutionSection", () => {
  it("renders nothing when there is no dependency and no problem", () => {
    const markup = renderToStaticMarkup(
      <ResolutionSection plan={plan({ steps: [{ package_id: "com.littlemonkey.skill.review", version: "1.0.0", action: "install", required_by: [] }], package_id: "com.littlemonkey.skill.review" })} />,
    );
    expect(markup).toBe("");
  });

  it("lists each dependency with what has to happen to it and who required it", () => {
    const markup = renderToStaticMarkup(<ResolutionSection plan={plan()} />);
    expect(markup).toContain("com.littlemonkey.skill.review");
    expect(markup).toContain("Install first");
    expect(markup).toContain("required by com.littlemonkey.assistant.reviewer");
    // The package being installed is not listed as its own dependency.
    expect(markup.match(/com\.littlemonkey\.assistant\.reviewer/g)).toHaveLength(1);
  });

  it("names the specific conflict rather than a generic failure", () => {
    const markup = renderToStaticMarkup(
      <ResolutionSection
        plan={plan({
          satisfiable: false,
          problems: [
            {
              kind: "unsatisfiable",
              package_id: "com.littlemonkey.skill.review",
              constraints: [{ required_by: "com.littlemonkey.assistant.reviewer", constraint: { minimum: "2.0.0" } }],
              available_versions: ["1.0.0"],
            },
            { kind: "surface_collision", claim: { slash_command: "review" }, package_ids: ["a.b.c", "d.e.f"] },
            { kind: "contract_mismatch", package_id: "a.b.c", required: { minimum: "2.0.0", maximum_exclusive: "3.0.0" }, implemented: "1.0.0" },
          ],
        })}
      />,
    );
    expect(markup).toContain("No version of com.littlemonkey.skill.review satisfies");
    expect(markup).toContain("&gt;=2.0.0");
    expect(markup).toContain("Available: 1.0.0");
    expect(markup).toContain("/review is claimed by more than one package: a.b.c, d.e.f");
    expect(markup).toContain("requires agent contract &gt;=2.0.0, &lt;3.0.0; this build implements 1.0.0");
  });
});

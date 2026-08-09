import { describe, expect, it } from "vitest";

import { livenessTone, placementTone } from "./PlacedNodesPanel";

/**
 * The two mappings this panel owns. Everything else it renders is text the
 * daemon supplied, and the daemon's own tests cover that.
 *
 * The direction matters more than the colours: an unrecognised liveness or
 * placement state must never read as healthy. A node this build has never heard
 * of is one whose state it cannot vouch for, and a green pill over it is the one
 * failure mode worth a test — the operator would stop looking.
 */
describe("placed node status tones", () => {
  it("only a node that is actually alive reads as healthy", () => {
    expect(livenessTone("alive")).toBe("success");
    expect(livenessTone("stale")).toBe("warning");
    expect(livenessTone("vanished")).toBe("danger");
    // A token from a newer daemon build.
    expect(livenessTone("something-new")).toBe("danger");
  });

  it("only a succeeded placement reads as healthy, and a lost one reads as a failure", () => {
    expect(placementTone("succeeded")).toBe("success");
    expect(placementTone("failed")).toBe("danger");
    // `lost` is a vanished node's work — as much an operator's problem as a
    // failure, and deliberately not softened into a neutral pill.
    expect(placementTone("lost")).toBe("danger");
    expect(placementTone("running")).toBe("neutral");
    expect(placementTone("accepted")).toBe("neutral");
    expect(placementTone("cancelled")).toBe("neutral");
    expect(placementTone("something-new")).toBe("neutral");
  });
});

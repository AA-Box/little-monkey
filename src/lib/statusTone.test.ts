import { describe, expect, it } from "vitest";

import { statusTone } from "./statusTone";

describe("statusTone", () => {
  it("maps the shared vocabulary consistently across panels", () => {
    expect(statusTone("passed")).toBe("success");
    expect(statusTone("completed")).toBe("success");
    expect(statusTone("succeeded")).toBe("success");
    expect(statusTone("failed")).toBe("danger");
    expect(statusTone("needs_reconciliation")).toBe("danger");
    expect(statusTone("running")).toBe("warning");
    expect(statusTone("cancelled")).toBe("neutral");
  });

  // The whole reason overrides exist: the same word legitimately means
  // different urgency in different domains, and that disagreement should be
  // explicit at the call site rather than hidden in a private copy.
  it("lets a panel override a shared word for its own domain", () => {
    expect(statusTone("queued")).toBe("neutral");
    expect(statusTone("queued", { queued: "warning" })).toBe("warning");
    expect(statusTone("declared", { declared: "danger" })).toBe("danger");
  });

  it("defaults an unknown status to neutral rather than guessing", () => {
    expect(statusTone("some_future_state")).toBe("neutral");
    expect(statusTone("")).toBe("neutral");
  });

  it("applies overrides ahead of the shared vocabulary", () => {
    expect(statusTone("cancelled", { cancelled: "danger" })).toBe("danger");
  });
});

import { describe, expect, it } from "vitest";

import {
  appendClickStep,
  appendNavigateStep,
  appendScrollStep,
  appendTypeStep,
  assertNoStoredSecrets,
  classifySelectorStability,
  convertRecordingToDraft,
  createRecording,
  extractSelectorFieldType,
  isCredentialLikeField,
  preferStableSelector,
  redactTypedValue,
  stopRecording,
  type BrowserRecording,
} from "./workflowRecorder";

describe("credential redaction", () => {
  it("flags fields whose selector, label, or type look like a credential", () => {
    expect(isCredentialLikeField({ selector: "#password" })).toBe(true);
    expect(isCredentialLikeField({ selector: "input[type=\"password\"]" })).toBe(true);
    expect(isCredentialLikeField({ selector: "#field-7", ariaLabel: "Password" })).toBe(true);
    expect(isCredentialLikeField({ selector: "#field-7", ariaLabel: "API Key" })).toBe(true);
    expect(isCredentialLikeField({ selector: "#field-7", text: "Enter your credit card number" })).toBe(true);
    expect(isCredentialLikeField({ selector: "#otp-code" })).toBe(true);
    expect(isCredentialLikeField({ selector: "#ssn" })).toBe(true);
    expect(isCredentialLikeField({ selector: "#secret-token-input" })).toBe(true);
  });

  it("is case-insensitive", () => {
    expect(isCredentialLikeField({ selector: "#PASSWORD" })).toBe(true);
    expect(isCredentialLikeField({ ariaLabel: "Api_Key" })).toBe(true);
  });

  it("does not flag ordinary fields", () => {
    expect(isCredentialLikeField({ selector: "#email" })).toBe(false);
    expect(isCredentialLikeField({ selector: "#search-query", ariaLabel: "Search" })).toBe(false);
    expect(isCredentialLikeField({ selector: "#first-name", text: "First name" })).toBe(false);
    expect(isCredentialLikeField({ selector: "textarea#comment" })).toBe(false);
  });

  it("extracts an HTML type from a selector when present", () => {
    expect(extractSelectorFieldType('input[type="password"]')).toBe("password");
    expect(extractSelectorFieldType("input[type=password]")).toBe("password");
    expect(extractSelectorFieldType("#login input[type=text]")).toBe("text");
    expect(extractSelectorFieldType("#login")).toBeNull();
    expect(extractSelectorFieldType(null)).toBeNull();
  });

  it("redacts credential-like values to null — never a masked echo of the secret", () => {
    const result = redactTypedValue("hunter2", { selector: "#password" });
    expect(result.redacted).toBe(true);
    expect(result.value).toBeNull();
  });

  it("keeps ordinary typed values untouched", () => {
    const result = redactTypedValue("jane@example.com", { selector: "#email" });
    expect(result.redacted).toBe(false);
    expect(result.value).toBe("jane@example.com");
  });

  it("redacts based on aria-label even when the selector itself looks generic", () => {
    const result = redactTypedValue("s3cr3t-value", { selector: "#field-42", ariaLabel: "Account password" });
    expect(result.redacted).toBe(true);
    expect(result.value).toBeNull();
  });
});

describe("selector stability", () => {
  it("classifies id, data-attr, aria-label, and brittle css selectors", () => {
    expect(classifySelectorStability("#submit-button")).toBe("id");
    expect(classifySelectorStability('[data-testid="submit"]')).toBe("data-attr");
    expect(classifySelectorStability('[aria-label="Submit"]')).toBe("aria-label");
    expect(classifySelectorStability("div > button:nth-child(3)")).toBe("css");
    expect(classifySelectorStability(".btn.btn-primary")).toBe("css");
  });

  it("keeps an already-stable selector as-is", () => {
    const preferred = preferStableSelector("#submit-button", null);
    expect(preferred).toEqual({
      selector: "#submit-button",
      stability: "id",
      synthesized: false,
      recordedSelector: "#submit-button",
    });
  });

  it("synthesizes an aria-label selector for a brittle css path when the element has one", () => {
    const preferred = preferStableSelector("div > button:nth-child(3)", {
      tag: "button",
      role: "button",
      ariaLabel: "Sign in",
      text: "Sign in",
    });
    expect(preferred.selector).toBe('button[aria-label="Sign in"]');
    expect(preferred.stability).toBe("aria-label");
    expect(preferred.synthesized).toBe(true);
    expect(preferred.recordedSelector).toBe("div > button:nth-child(3)");
  });

  it("escapes quotes when synthesizing an aria-label selector", () => {
    const preferred = preferStableSelector("div > span", {
      tag: "span",
      role: "",
      ariaLabel: 'Say "hi"',
      text: "",
    });
    expect(preferred.selector).toBe('span[aria-label="Say \\"hi\\""]');
  });

  it("keeps the brittle selector when no better signal is available", () => {
    const preferred = preferStableSelector("div > button:nth-child(3)", { tag: "button", role: "", ariaLabel: "", text: "" });
    expect(preferred.selector).toBe("div > button:nth-child(3)");
    expect(preferred.stability).toBe("css");
    expect(preferred.synthesized).toBe(false);
  });
});

function recordDemoLogin(): BrowserRecording {
  let recording = createRecording("run-1", "https://example.com/login", 1_000);
  recording = appendNavigateStep(recording, { url: "https://example.com/login", screenshotArtifactId: "sha256:nav" }, 1_000);
  recording = appendTypeStep(
    recording,
    {
      url: "https://example.com/login",
      selector: "#username",
      rawValue: "jane.doe",
      element: { tag: "input", role: "", ariaLabel: "Username", text: "" },
      screenshotArtifactId: "sha256:user",
    },
    1_100,
  );
  recording = appendTypeStep(
    recording,
    {
      url: "https://example.com/login",
      selector: 'input[type="password"]',
      rawValue: "correct horse battery staple",
      element: { tag: "input", role: "", ariaLabel: "Password", text: "" },
      screenshotArtifactId: "sha256:pass",
    },
    1_200,
  );
  recording = appendClickStep(
    recording,
    {
      url: "https://example.com/login",
      selector: "div.form > button:nth-child(4)",
      element: { tag: "button", role: "button", ariaLabel: "Sign in", text: "Sign in" },
      screenshotArtifactId: "sha256:click",
    },
    1_300,
  );
  // Long gap + URL change after the click — should become a decision point.
  recording = appendNavigateStep(recording, { url: "https://example.com/dashboard", screenshotArtifactId: "sha256:dash" }, 4_000);
  recording = appendScrollStep(recording, { url: "https://example.com/dashboard", x: 0, y: 400, screenshotArtifactId: "sha256:scroll" }, 4_200);
  return stopRecording(recording, 4_500);
}

describe("convertRecordingToDraft", () => {
  it("produces a draft that always starts unreviewed and disabled", () => {
    const draft = convertRecordingToDraft(recordDemoLogin());
    expect(draft.status).toBe("draft");
    expect(draft.reviewedAt).toBeNull();
  });

  it("never stores the redacted password anywhere in the draft", () => {
    const draft = convertRecordingToDraft(recordDemoLogin());
    const serialized = JSON.stringify(draft);
    expect(serialized).not.toContain("correct horse battery staple");
  });

  it("turns the password field into a sensitive, runtime-only input with no default", () => {
    const draft = convertRecordingToDraft(recordDemoLogin());
    const passwordInput = draft.inputs.find((input) => input.label.toLowerCase().includes("password"));
    expect(passwordInput).toBeDefined();
    expect(passwordInput?.sensitive).toBe(true);
    expect(passwordInput?.runtimeOnly).toBe(true);
    expect(passwordInput?.defaultValue).toBeNull();
  });

  it("keeps the ordinary username field editable with its recorded value as a default", () => {
    const draft = convertRecordingToDraft(recordDemoLogin());
    const usernameInput = draft.inputs.find((input) => input.label.toLowerCase().includes("username"));
    expect(usernameInput).toBeDefined();
    expect(usernameInput?.sensitive).toBe(false);
    expect(usernameInput?.runtimeOnly).toBe(false);
    expect(usernameInput?.defaultValue).toBe("jane.doe");
  });

  it("prefers a synthesized aria-label selector over the brittle recorded click selector", () => {
    const draft = convertRecordingToDraft(recordDemoLogin());
    const clickStep = draft.steps.find((step) => step.action.type === "click");
    expect(clickStep).toBeDefined();
    if (clickStep?.action.type === "click") {
      expect(clickStep.action.selector).toBe('button[aria-label="Sign in"]');
      expect(clickStep.action.selectorStability).toBe("aria-label");
      expect(clickStep.action.recordedSelector).toBe("div.form > button:nth-child(4)");
    }
  });

  it("prefers a synthesized aria-label selector for the password field over its brittle recorded type=\"password\" selector, still referencing a runtime-only input", () => {
    const draft = convertRecordingToDraft(recordDemoLogin());
    const typeSteps = draft.steps.filter((step) => step.action.type === "type");
    expect(typeSteps).toHaveLength(2);
    const passwordStep = typeSteps.find(
      (step) => step.action.type === "type" && step.action.recordedSelector.includes("password"),
    );
    expect(passwordStep).toBeDefined();
    const action = passwordStep?.action;
    if (action?.type === "type") {
      expect(action.selector).toBe('input[aria-label="Password"]');
      expect(action.recordedSelector).toBe('input[type="password"]');
      const input = draft.inputs.find((entry) => entry.id === action.inputId);
      expect(input?.runtimeOnly).toBe(true);
      expect(input?.sensitive).toBe(true);
    }
  });

  it("inserts a decision point (wait) after a click that changed the page", () => {
    const draft = convertRecordingToDraft(recordDemoLogin());
    const waitSteps = draft.steps.filter((step) => step.action.type === "waitForSelector");
    expect(waitSteps.length).toBeGreaterThanOrEqual(1);
  });

  it("appends a trailing verification step referencing the final page", () => {
    const draft = convertRecordingToDraft(recordDemoLogin());
    const last = draft.steps[draft.steps.length - 1];
    expect(last.action.type).toBe("verify");
    if (last.action.type === "verify") {
      expect(last.action.expectedUrlPrefix).toBe("https://example.com/dashboard");
    }
  });

  it("preserves step order: navigate, type, type, click, wait, navigate, scroll, verify", () => {
    const draft = convertRecordingToDraft(recordDemoLogin());
    expect(draft.steps.map((step) => step.action.type)).toEqual([
      "navigate",
      "type",
      "type",
      "click",
      "waitForSelector",
      "navigate",
      "scroll",
      "verify",
    ]);
  });

  it("round-trips through JSON without losing the no-secrets invariant", () => {
    const draft = convertRecordingToDraft(recordDemoLogin());
    const roundTripped = JSON.parse(JSON.stringify(draft));
    expect(() => assertNoStoredSecrets(roundTripped)).not.toThrow();
  });

  it("throws assertNoStoredSecrets for a tampered draft that resurrects a stored secret", () => {
    const draft = convertRecordingToDraft(recordDemoLogin());
    const tampered = {
      ...draft,
      inputs: draft.inputs.map((input) => (input.sensitive ? { ...input, defaultValue: "leaked" } : input)),
    };
    expect(() => assertNoStoredSecrets(tampered)).toThrow();
  });
});

describe("recording capture", () => {
  it("keeps recorded steps in append order with monotonically non-decreasing timestamps", () => {
    const recording = recordDemoLogin();
    const timestamps = recording.steps.map((step) => step.atMs);
    for (let i = 1; i < timestamps.length; i += 1) {
      expect(timestamps[i]).toBeGreaterThanOrEqual(timestamps[i - 1]);
    }
  });

  it("stopRecording is idempotent and sets stoppedAtMs once", () => {
    const recording = recordDemoLogin();
    const stoppedAgain = stopRecording(recording, 99_999);
    expect(stoppedAgain.stoppedAtMs).toBe(recording.stoppedAtMs);
  });

  it("does not mutate the input recording (pure, immutable append)", () => {
    const recording = createRecording("run-2", "https://example.com");
    const before = recording.steps.length;
    appendScrollStep(recording, { url: "https://example.com", x: 0, y: 100, screenshotArtifactId: null });
    expect(recording.steps.length).toBe(before);
  });
});

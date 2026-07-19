import { describe, expect, it, beforeEach } from "vitest";
import { useModelStore } from "../store/modelStore";
import { cloudModelRetirementWarning } from "./modelRetirement";

function resetProviderModelRetirements() {
  useModelStore.setState({ providerModelRetirements: {} });
}

describe("cloudModelRetirementWarning", () => {
  beforeEach(() => {
    resetProviderModelRetirements();
  });

  it("returns null when the provider hasn't been checked yet", () => {
    expect(cloudModelRetirementWarning("openai", "text-davinci-003")).toBeNull();
  });

  it("returns null for a model the provider's checked list doesn't flag", () => {
    useModelStore.setState({
      providerModelRetirements: {
        openai: {
          "gpt-4o": {
            provider_id: "openai",
            model_id: "gpt-4o",
            reason: "unused in this test",
            suggested_replacement_model_id: null,
            replacement_note: "unused",
          },
        },
      },
    });
    expect(cloudModelRetirementWarning("openai", "gpt-3.5-turbo")).toBeNull();
  });

  it("returns the cached warning for a flagged model", () => {
    const warning = {
      provider_id: "openai",
      model_id: "text-davinci-003",
      reason: "OpenAI retired the legacy GPT-3 completions models in January 2024.",
      suggested_replacement_model_id: "gpt-4o",
      replacement_note: "a current GPT-4o family chat model (e.g. gpt-4o or gpt-4o-mini)",
    };
    useModelStore.setState({
      providerModelRetirements: {
        openai: { "text-davinci-003": warning },
      },
    });
    expect(cloudModelRetirementWarning("openai", "text-davinci-003")).toEqual(warning);
  });

  it("keys warnings per provider — the same model id under a different provider is unaffected", () => {
    useModelStore.setState({
      providerModelRetirements: {
        anthropic: {
          "claude-1": {
            provider_id: "anthropic",
            model_id: "claude-1",
            reason: "Anthropic retired the Claude 1 model line.",
            suggested_replacement_model_id: null,
            replacement_note: "a current Claude 3.x+ model, such as a Sonnet or Haiku variant",
          },
        },
      },
    });
    expect(cloudModelRetirementWarning("some-custom-provider", "claude-1")).toBeNull();
  });
});

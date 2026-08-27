// @vitest-environment jsdom
/**
 * Render-level cover for the picker's dropdown behavior. The sibling
 * `ModelSwitcher.test.ts` holds the pure filtering predicate; what needs a DOM
 * is the part that regressed once already — which sections a query reaches,
 * and whether the keyboard can get back out of the search box.
 */
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../../lib/i18n", () => ({ useT: () => ({ t: (key: string) => key, locale: "en-US" }) }));
vi.mock("./AddModelDialog", () => ({
  AddModelDialog: () => <div data-testid="add-model-dialog" />,
}));

import { useModelStore } from "../../store/modelStore";
import { useSettingsStore } from "../../store/settingsStore";
import { ModelSwitcher } from "./ModelSwitcher";

const localModel = {
  id: "qwen-local", name: "Qwen Local", repo: "r", file: "f", size_gb: 1,
  tool_calling: true, installed: true, path: "/models/qwen.gguf",
  is_external: false, kind: "chat" as const,
};

function seed(overrides: Record<string, unknown> = {}) {
  useModelStore.setState({
    installed: [localModel, { ...localModel, id: "embed", name: "Embed Model", kind: "embedding" as const }],
    active: null,
    activeProvider: "local",
    activeOllamaModel: null,
    activeProviderId: null,
    activeProviderModel: null,
    ollamaModels: [{ name: "llama3:8b", size_bytes: 1, is_cloud: false, tool_calling: true, vision: false, modified_at: "" }],
    providers: [
      { id: "anthropic", label: "Anthropic", base_url: "u", is_custom: false, has_key: true, is_extension: false },
      // No key, but authenticates inside its own sandbox — still selectable.
      { id: "ext", label: "ExtProvider", base_url: "", is_custom: false, has_key: false, is_extension: true },
      { id: "nokey", label: "NoKey", base_url: "u", is_custom: false, has_key: false, is_extension: false },
    ],
    providerModels: {
      anthropic: [{ id: "claude-sonnet-4-5" }],
      ext: [{ id: "ext-model-1" }],
      nokey: [{ id: "should-not-appear" }],
    },
    providerModelRetirements: {},
    ...overrides,
  } as never);
  useSettingsStore.setState({
    providerModelFilters: {
      anthropic: { showAll: true, selectedModelIds: [] },
      ext: { showAll: true, selectedModelIds: [] },
      nokey: { showAll: true, selectedModelIds: [] },
    },
  } as never);
}

const trigger = () => screen.getByRole("button", { expanded: false });
const searchBox = () => screen.queryByPlaceholderText("ComparePicker.searchPlaceholder");

describe("ModelSwitcher dropdown", () => {
  beforeEach(() => seed());
  afterEach(cleanup);

  it("offers local chat models and never embedding-only ones", () => {
    render(<ModelSwitcher />);
    fireEvent.click(trigger());
    expect(screen.getByText("Qwen Local")).toBeTruthy();
    expect(screen.queryByText("Embed Model")).toBeNull();
  });

  it("offers extension providers, which own no key, and skips unconnected ones", () => {
    render(<ModelSwitcher />);
    fireEvent.click(trigger());
    expect(screen.getByText("ext-model-1")).toBeTruthy();
    expect(screen.queryByText("should-not-appear")).toBeNull();
  });

  it("narrows to one section by its label, not just by model name", () => {
    render(<ModelSwitcher />);
    fireEvent.click(trigger());
    fireEvent.change(searchBox()!, { target: { value: "ModelSwitcher.ollamaSectionLabel" } });
    expect(screen.getByText("llama3:8b")).toBeTruthy();
    expect(screen.queryByText("Qwen Local")).toBeNull();
    expect(screen.queryByText("claude-sonnet-4-5")).toBeNull();
  });

  it("says so when a query matches nothing", () => {
    render(<ModelSwitcher />);
    fireEvent.click(trigger());
    fireEvent.change(searchBox()!, { target: { value: "zzzz" } });
    expect(screen.getByText("ComparePicker.noResultsTitle")).toBeTruthy();
  });

  it("drops the search box when there is nothing to search", () => {
    seed({ installed: [], ollamaModels: [], providers: [], providerModels: {} });
    render(<ModelSwitcher />);
    fireEvent.click(trigger());
    expect(searchBox()).toBeNull();
    expect(screen.getByText("ModelSwitcher.addModelHint")).toBeTruthy();
  });

  it("closes on Escape and hands focus back to the pill", () => {
    render(<ModelSwitcher />);
    const pill = trigger();
    fireEvent.click(pill);
    expect(searchBox()).toBeTruthy();
    fireEvent.keyDown(window, { key: "Escape" });
    expect(searchBox()).toBeNull();
    expect(document.activeElement).toBe(pill);
  });

  it("returns focus to the pill after a model is picked", () => {
    render(<ModelSwitcher />);
    const pill = trigger();
    fireEvent.click(pill);
    fireEvent.click(screen.getByText("claude-sonnet-4-5").closest("button")!);
    expect(document.activeElement).toBe(pill);
  });

  it("forgets the previous query when reopened", () => {
    render(<ModelSwitcher />);
    fireEvent.click(trigger());
    fireEvent.change(searchBox()!, { target: { value: "zzzz" } });
    fireEvent.keyDown(window, { key: "Escape" });
    fireEvent.click(trigger());
    expect((searchBox() as HTMLInputElement).value).toBe("");
  });

  it("opens setup from the footer and closes the dropdown behind it", async () => {
    render(<ModelSwitcher />);
    fireEvent.click(trigger());
    fireEvent.click(screen.getByText("OllamaPanel.addModelLabel"));
    expect(await screen.findByTestId("add-model-dialog")).toBeTruthy();
    expect(searchBox()).toBeNull();
  });
});

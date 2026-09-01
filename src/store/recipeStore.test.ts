import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args), isTauri: () => true }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("@tauri-apps/api/window", () => ({ getCurrentWindow: () => ({ label: "test" }) }));

import { useRecipeStore, type DiscoveredRecipe, type Recipe } from "./recipeStore";
// Shared with `recipes.rs`'s `recipe_deserializes_canonical_fixture` Rust
// test — see that test's doc comment (ROADMAP.md §3 item 6).
import canonicalRecipeFixture from "../../src-tauri/fixtures/recipe.canonical.json";

function makeRecipe(overrides: Partial<Recipe> = {}): Recipe {
  return {
    version: 1,
    name: "nightly-deps-audit",
    target: { ollama: "qwen2.5:14b" },
    permission_mode: "acceptEdits",
    prompt: "Check {{manifest}} for outdated deps.",
    params: { manifest: "package.json" },
    output: { json: false },
    ...overrides,
  };
}

function makeDiscovered(overrides: Partial<DiscoveredRecipe> = {}): DiscoveredRecipe {
  return {
    path: "/ws/.littlemonkey/recipes/nightly-deps-audit.yml",
    source: "workspace",
    recipe: makeRecipe(),
    error: null,
    ...overrides,
  };
}

beforeEach(() => {
  invokeMock.mockReset();
  useRecipeStore.setState({ recipes: [], loading: false, error: null });
});

describe("recipeStore.refresh", () => {
  it("calls recipes_list and caches the result", async () => {
    const entry = makeDiscovered();
    invokeMock.mockResolvedValueOnce([entry]);

    await useRecipeStore.getState().refresh();

    expect(invokeMock).toHaveBeenCalledWith("recipes_list");
    expect(useRecipeStore.getState().recipes).toEqual([entry]);
    expect(useRecipeStore.getState().loading).toBe(false);
    expect(useRecipeStore.getState().error).toBeNull();
  });

  it("records a failure in error instead of throwing, without wiping the previous list", async () => {
    const entry = makeDiscovered();
    invokeMock.mockResolvedValueOnce([entry]);
    await useRecipeStore.getState().refresh();

    invokeMock.mockRejectedValueOnce(new Error("disk unavailable"));
    await expect(useRecipeStore.getState().refresh()).resolves.toBeUndefined();

    expect(useRecipeStore.getState().error).toBe("disk unavailable");
    expect(useRecipeStore.getState().recipes).toEqual([entry]);
  });

  it("surfaces a broken (unparsable) recipe file instead of dropping it", async () => {
    const broken = makeDiscovered({ recipe: null, error: "Failed to parse recipe YAML: ..." });
    invokeMock.mockResolvedValueOnce([broken]);

    await useRecipeStore.getState().refresh();

    expect(useRecipeStore.getState().recipes).toEqual([broken]);
  });

  /** Reads the exact same file `recipes.rs`'s
   * `recipe_deserializes_canonical_fixture` Rust test reads via
   * `include_str!` — a single shared fixture, not two independently
   * hand-typed literals, is what actually pins the TS<->Rust schema against
   * drift (ROADMAP.md §3 item 6). `monkey-cli` reads `Recipe` directly out
   * of a recipe file without going through this store at all. */
  it("caches the same canonical recipe the Rust unit test pins", async () => {
    const discovered = makeDiscovered({ recipe: canonicalRecipeFixture as unknown as Recipe });
    invokeMock.mockResolvedValueOnce([discovered]);

    await useRecipeStore.getState().refresh();

    expect(useRecipeStore.getState().recipes).toEqual([discovered]);
  });
});

describe("recipeStore.save", () => {
  it("calls recipes_save with name and content, then refreshes", async () => {
    const saved = makeRecipe({ name: "new-recipe" });
    invokeMock.mockResolvedValueOnce(saved); // recipes_save
    invokeMock.mockResolvedValueOnce([makeDiscovered({ recipe: saved })]); // recipes_list

    const result = await useRecipeStore.getState().save("new-recipe", "version: 1\nname: new-recipe\n...");

    expect(invokeMock).toHaveBeenNthCalledWith(1, "recipes_save", {
      name: "new-recipe",
      content: "version: 1\nname: new-recipe\n...",
    });
    expect(invokeMock).toHaveBeenNthCalledWith(2, "recipes_list");
    expect(result).toEqual(saved);
    expect(useRecipeStore.getState().recipes).toEqual([makeDiscovered({ recipe: saved })]);
  });

  it("propagates a validation/write failure instead of silently swallowing it", async () => {
    invokeMock.mockRejectedValueOnce(new Error("recipe name 'Bad Name' must match [a-z0-9-]+"));

    await expect(useRecipeStore.getState().save("Bad Name", "...")).rejects.toThrow("must match");
  });
});

describe("recipeStore.setTarget", () => {
  it("calls recipes_set_target with the name and target, then re-reads", async () => {
    // The channel routes screen changes a model this way rather than through
    // `save`: `save` takes the whole file, so changing one line would mean
    // re-serializing a recipe the UI never parsed — deleting the operator's
    // comments to move a model.
    const saved = makeRecipe({ name: "channel-chat", target: { ollama: "qwen3:8b" } });
    invokeMock.mockResolvedValueOnce(saved); // recipes_set_target
    invokeMock.mockResolvedValueOnce([makeDiscovered({ recipe: saved })]); // recipes_list

    const result = await useRecipeStore
      .getState()
      .setTarget("channel-chat", { ollama: "qwen3:8b" });

    expect(invokeMock).toHaveBeenNthCalledWith(1, "recipes_set_target", {
      name: "channel-chat",
      target: { ollama: "qwen3:8b" },
    });
    expect(invokeMock).toHaveBeenNthCalledWith(2, "recipes_list");
    expect(result).toEqual(saved);
    expect(useRecipeStore.getState().recipes).toEqual([makeDiscovered({ recipe: saved })]);
  });

  it("leaves the cached recipes alone when the write fails", async () => {
    // A picker that moved on a failed write would claim a model the runner is
    // not going to use, so nothing may be refreshed or mutated here.
    const before = useRecipeStore.getState().recipes;
    invokeMock.mockRejectedValueOnce(
      new Error("recipe target must set exactly one of provider, ollama, local_url, or managed_model"),
    );

    await expect(
      useRecipeStore.getState().setTarget("channel-chat", {}),
    ).rejects.toThrow("exactly one of");

    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(useRecipeStore.getState().recipes).toBe(before);
  });
});

describe("recipeStore.remove", () => {
  it("calls recipes_delete then refreshes", async () => {
    invokeMock.mockResolvedValueOnce(undefined); // recipes_delete
    invokeMock.mockResolvedValueOnce([]); // recipes_list

    await useRecipeStore.getState().remove("nightly-deps-audit");

    expect(invokeMock).toHaveBeenNthCalledWith(1, "recipes_delete", { name: "nightly-deps-audit" });
    expect(invokeMock).toHaveBeenNthCalledWith(2, "recipes_list");
  });
});

describe("recipeStore.validate", () => {
  it("calls recipes_validate with the raw content and returns the parsed recipe", async () => {
    const recipe = makeRecipe();
    invokeMock.mockResolvedValueOnce(recipe);

    const result = await useRecipeStore.getState().validate("version: 1\n...");

    expect(invokeMock).toHaveBeenCalledWith("recipes_validate", { content: "version: 1\n..." });
    expect(result).toEqual(recipe);
  });
});

describe("recipeStore.readRaw", () => {
  it("calls recipes_read_raw with the name and returns the raw text", async () => {
    invokeMock.mockResolvedValueOnce("version: 1\nname: x\n");

    const result = await useRecipeStore.getState().readRaw("x");

    expect(invokeMock).toHaveBeenCalledWith("recipes_read_raw", { nameOrPath: "x" });
    expect(result).toBe("version: 1\nname: x\n");
  });
});

import { describe, expect, it, vi } from "vitest";

// `ModelManager` pulls in the dialog plugin and the model store at import time;
// neither is used by the pure helper under test, but both have to resolve.
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: vi.fn() }));
vi.mock("../../store/modelStore", () => ({ useModelStore: vi.fn() }));
vi.mock("../../lib/i18n", () => ({ useT: () => ({ t: (key: string) => key }) }));

import { projectorActions } from "./ModelManager";
import type { ModelInfo } from "../../lib/modelRegistry";

const projector = {
  path: "/models/mmproj.gguf",
  file: "mmproj.gguf",
  size_bytes: 1,
  ownership: "external",
  sha256: null,
} as const;

const model = (overrides: Partial<ModelInfo>): ModelInfo =>
  ({
    id: "m",
    name: "m",
    repo: "",
    file: "m.gguf",
    size_gb: 1,
    tool_calling: false,
    installed: true,
    path: "/models/m.gguf",
    is_external: false,
    kind: "chat",
    ...overrides,
  }) as ModelInfo;

describe("projectorActions", () => {
  it("withholds both callbacks for an MLX bundle", () => {
    // `models_set_projector` reads the model path as a regular GGUF file and
    // throws on a directory, so a card that offered the button could only ever
    // produce an error dialog. `ModelCard` renders the buttons whenever the
    // callbacks exist, so this is the whole of the rule.
    const actions = projectorActions(
      model({
        runtime: "mlx",
        file: "4-bit",
        path: "/weights/Qwen3.8-27B-Uncensored/4-bit",
        components: { projector },
      }),
      vi.fn(),
      vi.fn(),
    );

    expect(actions.onAddProjector).toBeUndefined();
    expect(actions.onRemoveProjector).toBeUndefined();
  });

  it("still offers them for a llama.cpp model", () => {
    const addProjector = vi.fn();
    const removeProjectorAt = vi.fn();
    const gguf = model({ components: { projector } });

    const actions = projectorActions(gguf, addProjector, removeProjectorAt);
    actions.onAddProjector?.();
    actions.onRemoveProjector?.();

    expect(addProjector).toHaveBeenCalledWith(gguf);
    expect(removeProjectorAt).toHaveBeenCalledWith("/models/m.gguf");
  });

  it("offers no removal for a llama.cpp model that has no projector", () => {
    expect(projectorActions(model({}), vi.fn(), vi.fn()).onRemoveProjector).toBeUndefined();
  });
});

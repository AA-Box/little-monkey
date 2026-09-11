import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import { ModelCard } from "./ModelCard";

vi.mock("../../lib/i18n", () => ({
  useT: () => ({
    t: (key: string) =>
      ({
        "ModelCard.projectorLabel": "Multimodal projector",
        "ModelCard.projectorMissing": "Projector missing",
        "ModelCard.mlxRuntimeBadge": "MLX",
        "ModelCard.addProjectorButton": "Add vision component",
        "ModelCard.visionConfiguredBadge": "Vision configured",
        "ModelCard.visionReadyBadge": "Vision ready",
        "ModelCard.embeddingsUnavailableWithProjector": "Embeddings unavailable with projector",
        "ModelCard.replaceProjectorButton": "Replace projector",
        "ModelCard.statusSelected": "Selected",
        "ModelCard.startButton": "Start",
        "ModelCard.deleteWeightsTitle": "Delete weights",
      })[key] ?? key,
  }),
}));

describe("ModelCard download failures", () => {
  it("shows why a Pull failed, on a model that is not installed", () => {
    const markup = renderToStaticMarkup(
      <ModelCard
        model={{
          id: "curated:model",
          name: "Curated model",
          repo: "vendor/Model-GGUF",
          file: "model.gguf",
          size_gb: 4.7,
          tool_calling: true,
          installed: false,
          path: null,
          is_external: false,
          kind: "chat",
        }}
        isActive={false}
        llamaStatus="stopped"
        // A catalog entry naming a file the repo does not have fails here, and
        // used to fail invisibly: the card kept offering Pull and nothing else
        // ever happened.
        downloadError="Download failed: HTTP 404 Not Found"
        onInstall={() => undefined}
        onCancelDownload={() => undefined}
        onDelete={() => undefined}
        onStart={() => undefined}
        onStop={() => undefined}
      />,
    );
    expect(markup).toContain("Download failed: HTTP 404 Not Found");
  });
});

describe("ModelCard multimodal state", () => {
  it("shows the attached projector and vision badge", () => {
    const markup = renderToStaticMarkup(
      <ModelCard
        model={{
          id: "local:model",
          name: "Local model",
          repo: "",
          file: "model.gguf",
          size_gb: 1,
          tool_calling: false,
          installed: true,
          path: "/models/model.gguf",
          is_external: true,
          kind: "chat",
          components: {
            projector: {
              path: "/models/mmproj.gguf",
              file: "mmproj.gguf",
              size_bytes: 12,
              ownership: "external",
              sha256: null,
              missing: false,
            },
          },
          capabilities: { text: true, image_input: true },
        }}
        isActive={false}
        llamaStatus="stopped"
        downloadProgress={undefined}
        onInstall={() => {}}
        onCancelDownload={() => {}}
        onDelete={() => {}}
        onStart={() => {}}
        onStop={() => {}}
        onAddProjector={() => {}}
      />,
    );

    expect(markup).toContain("Vision configured");
    expect(markup).not.toContain("Vision ready");
    expect(markup).toContain("Multimodal projector: mmproj.gguf");
    expect(markup).toContain("Embeddings unavailable with projector");
    expect(markup).toContain("Replace projector");
    expect(markup).not.toContain("Add vision component");
    expect(markup).not.toContain("Projector missing");
  });

  it("shows vision ready only after the active runtime confirms it", () => {
    const markup = renderToStaticMarkup(
      <ModelCard
        model={{
          id: "local:model",
          name: "Local model",
          repo: "",
          file: "model.gguf",
          size_gb: 1,
          tool_calling: false,
          installed: true,
          path: "/models/model.gguf",
          is_external: true,
          kind: "chat",
          components: { projector: null },
          capabilities: { text: true, image_input: true },
        }}
        isActive
        llamaStatus="ready"
        llamaVisionEnabled
        onInstall={() => {}}
        onCancelDownload={() => {}}
        onDelete={() => {}}
        onStart={() => {}}
        onStop={() => {}}
      />,
    );

    expect(markup).toContain("Vision ready");
    expect(markup).not.toContain("Vision configured");
  });
});

describe("ModelCard MLX bundle", () => {
  it("badges an external MLX directory and offers no projector button", () => {
    const markup = renderToStaticMarkup(
      <ModelCard
        model={{
          id: "external:/weights/Qwen3.8-27B-Uncensored/4-bit",
          // A directory bundle: the name is not the folder's bare name, and
          // `file` is the directory itself, with no extension.
          name: "Qwen3.8-27B-Uncensored (4-bit)",
          repo: "",
          file: "4-bit",
          size_gb: 16.2,
          tool_calling: false,
          installed: true,
          path: "/weights/Qwen3.8-27B-Uncensored/4-bit",
          is_external: true,
          kind: "chat",
          runtime: "mlx",
          capabilities: { text: true, image_input: true },
        }}
        isActive={false}
        llamaStatus="stopped"
        onInstall={() => {}}
        onCancelDownload={() => {}}
        onDelete={() => {}}
        onStart={() => {}}
        onStop={() => {}}
      />,
    );

    expect(markup).toContain("MLX");
    expect(markup).toContain("Vision configured");
    expect(markup).toContain("Start");
    // The card has no runtime rule of its own — it renders the projector
    // button whenever the callback exists, and `ModelManager` is what withholds
    // it for an MLX bundle. `ModelManager.test.tsx` covers that rule; asserting
    // the button's absence here would only be asserting that this render
    // passed no callback.
  });
});

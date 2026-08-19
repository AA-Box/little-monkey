import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import { ModelCard } from "./ModelCard";

vi.mock("../../lib/i18n", () => ({
  useT: () => ({
    t: (key: string) =>
      ({
        "ModelCard.projectorLabel": "Multimodal projector",
        "ModelCard.projectorMissing": "Projector missing",
        "ModelCard.visionConfiguredBadge": "Vision configured",
        "ModelCard.visionReadyBadge": "Vision ready",
        "ModelCard.embeddingsUnavailableWithProjector": "Embeddings unavailable with projector",
        "ModelCard.statusSelected": "Selected",
        "ModelCard.startButton": "Start",
        "ModelCard.deleteWeightsTitle": "Delete weights",
      })[key] ?? key,
  }),
}));

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
      />,
    );

    expect(markup).toContain("Vision configured");
    expect(markup).not.toContain("Vision ready");
    expect(markup).toContain("Multimodal projector: mmproj.gguf");
    expect(markup).toContain("Embeddings unavailable with projector");
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

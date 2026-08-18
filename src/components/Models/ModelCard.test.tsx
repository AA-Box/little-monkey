import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import { ModelCard } from "./ModelCard";

vi.mock("../../lib/i18n", () => ({
  useT: () => ({
    t: (key: string) =>
      ({
        "ModelCard.projectorLabel": "Multimodal projector",
        "ModelCard.projectorMissing": "Projector missing",
        "ModelCard.visionBadge": "Vision",
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

    expect(markup).toContain("Vision");
    expect(markup).toContain("Multimodal projector: mmproj.gguf");
    expect(markup).not.toContain("Projector missing");
  });
});

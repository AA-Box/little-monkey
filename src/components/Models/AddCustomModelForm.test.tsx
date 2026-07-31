import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";

const modelStoreState = vi.hoisted(() => ({
  addExternalModel: vi.fn(),
  resolveModelReference: vi.fn(),
  installModelReference: vi.fn(),
  downloadProgress: {},
}));

vi.mock("@tauri-apps/plugin-dialog", () => ({ open: vi.fn() }));
vi.mock("../../store/modelStore", () => ({
  useModelStore: (selector: (state: typeof modelStoreState) => unknown) =>
    selector(modelStoreState),
}));
vi.mock("../../lib/i18n", () => {
  const labels: Record<string, string> = {
    "AddCustomModelForm.openGgufDescription": "Open a local GGUF",
    "AddCustomModelForm.openModelFileButton": "Open model file",
    "AddCustomModelForm.referenceDescription": "Install from a model reference",
    "AddCustomModelForm.publicSingleFileOnly":
      "Only public, single-file GGUF models are supported right now.",
    "AddCustomModelForm.referenceLabel": "Model reference",
    "AddCustomModelForm.referencePlaceholder": "Reference",
    "AddCustomModelForm.resolveButton": "Resolve",
    "AddCustomModelForm.examplesLabel": "Examples:",
    "AddCustomModelForm.sourceOllama": "Ollama Registry",
    "AddCustomModelForm.sourceHuggingFace": "Hugging Face",
    "AddCustomModelForm.fileLabel": "File",
    "AddCustomModelForm.sizeLabel": "Size",
    "AddCustomModelForm.licenseLabel": "License",
    "AddCustomModelForm.toolCallingLabel": "Tool calling",
    "AddCustomModelForm.toolCallingSupported": "Supported",
    "AddCustomModelForm.toolCallingNotAdvertised": "Not advertised",
    "AddCustomModelForm.licenseUnknown": "Unknown",
  };
  return {
    useT: () => ({
      t: (key: string) => labels[key] ?? key,
      locale: "en-US",
    }),
  };
});

import {
  AddCustomModelForm,
  ResolvedModelReferenceDetails,
  resolvedModelSourceKey,
} from "./AddCustomModelForm";
import type { ResolvedModelReference } from "../../store/modelStore";

const resolved: ResolvedModelReference = {
  source: "ollama_registry",
  canonicalReference: "hf.co/library/llama3.2-GGUF:Q4_K_M",
  displayName: "Llama 3.2 3B",
  repo: "library/llama3.2-GGUF",
  revision: "main",
  fileName: "llama3.2-3b-q4_k_m.gguf",
  downloadUrl: "https://example.com/llama3.2-3b-q4_k_m.gguf",
  sha256: "a".repeat(64),
  sizeBytes: 1024 ** 3,
  toolCalling: true,
  licenseName: "Llama Community License",
  licenseUrl: "https://example.com/license",
};

describe("AddCustomModelForm", () => {
  it("renders one reference flow, both supported examples, and the public single-file limit", () => {
    const markup = renderToStaticMarkup(<AddCustomModelForm />);

    expect(markup).toContain("Only public, single-file GGUF models are supported right now.");
    expect(markup).toContain("llama3.2:3b");
    expect(markup).toContain(
      "hf.co/Qwen/Qwen2.5-Coder-0.5B-Instruct-GGUF:Q4_K_M",
    );
    expect(markup.match(/<input/g)).toHaveLength(1);
  });

  it("renders resolved source, file, size, license, and tool-calling metadata", () => {
    const markup = renderToStaticMarkup(
      <ResolvedModelReferenceDetails resolved={resolved} />,
    );

    expect(markup).toContain("Llama 3.2 3B");
    expect(markup).toContain("Ollama Registry");
    expect(markup).toContain("llama3.2-3b-q4_k_m.gguf");
    expect(markup).toContain("1.00 GB");
    expect(markup).toContain(resolved.sha256);
    expect(markup).toContain("Llama Community License");
    expect(markup).toContain("Supported");
  });

  it("maps known providers to localized labels and preserves unknown providers", () => {
    expect(resolvedModelSourceKey("ollama")).toBe(
      "AddCustomModelForm.sourceOllama",
    );
    expect(resolvedModelSourceKey("hugging-face")).toBe(
      "AddCustomModelForm.sourceHuggingFace",
    );
    expect(resolvedModelSourceKey("custom")).toBeNull();
  });
});

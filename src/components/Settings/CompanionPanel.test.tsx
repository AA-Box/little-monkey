// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invoke(...args),
}));
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: vi.fn() }));

import { CompanionPanel } from "./CompanionPanel";
import type { CompanionConfig } from "../../lib/companionClient";
import type { ActiveCapability } from "../../lib/executableExtensionsClient";

const CONFIG: CompanionConfig = {
  schemaVersion: 1,
  overlayShortcut: "CommandOrControl+Shift+Space",
  voice: {
    backend: "executable_extension",
    whisperBinary: null,
    whisperModel: null,
    providerId: null,
    providerModel: "whisper-1",
    extensionId: null,
    extensionCapabilityId: null,
    language: "auto",
    ttsVoice: null,
    ttsBackend: "system",
    ttsExtensionId: null,
    ttsExtensionCapabilityId: null,
    realtimeBackend: "system",
    realtimeExtensionId: null,
    realtimeExtensionCapabilityId: null,
    saveRawAudio: false,
    inputDeviceId: null,
    outputDeviceId: null,
    vadMinSpeechMs: 180,
    vadSilenceMs: 700,
    vadMaxUtteranceMs: 20000,
    wakePhraseEnabled: false,
    wakePhrase: 'hey monkey',
    alwaysListening: false,
    dictationLanguage: null,
    dictationRequireOnDevice: false,
  },
  imageEndpoints: [],
};

const STT_CAPABILITY: ActiveCapability = {
  kind: "stt",
  capability_id: "transcribe",
  extension_id: "dev.example.stt",
  version: "1.0.0",
  display_name: "Private transcription",
  description: "Transcribes one delegated audio artifact.",
  input_schema: { type: "object" },
};

const REPLACEMENT_CAPABILITY: ActiveCapability = {
  ...STT_CAPABILITY,
  extension_id: "dev.example.replacement",
  display_name: "Replacement transcription",
};

const TTS_CAPABILITY: ActiveCapability = {
  ...STT_CAPABILITY,
  kind: "tts",
  capability_id: "speak",
  extension_id: "dev.example.voice",
  display_name: "Private synthesis",
};

const REALTIME_CAPABILITY: ActiveCapability = {
  ...STT_CAPABILITY,
  kind: "realtime_voice",
  capability_id: "converse",
  extension_id: "dev.example.line",
  display_name: "Private realtime voice",
};

beforeEach(() => {
  invoke.mockReset();
  invoke.mockImplementation((command: string, payload?: { config?: CompanionConfig }) => {
    if (command === "m7_config_get") return Promise.resolve(CONFIG);
    if (command === "m7_capture_grants") return Promise.resolve([]);
    if (command === "extensions_active_capabilities") {
      // The panel discovers each capability kind separately now, so the mock
      // answers per kind rather than handing the STT list to every picker.
      const kind = (payload as { kind?: string } | undefined)?.kind;
      if (kind === "stt") return Promise.resolve([STT_CAPABILITY, REPLACEMENT_CAPABILITY]);
      if (kind === "tts") return Promise.resolve([TTS_CAPABILITY]);
      if (kind === "realtime_voice") return Promise.resolve([REALTIME_CAPABILITY]);
      return Promise.resolve([]);
    }
    if (command === "m7_config_save") return Promise.resolve(payload?.config);
    return Promise.resolve(undefined);
  });
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("CompanionPanel executable transcription", () => {
  it("uses built-in local Whisper without asking for binary or model paths", async () => {
    render(<CompanionPanel />);

    await screen.findByText("Voice and transcription");
    fireEvent.change(screen.getByLabelText("Backend"), { target: { value: "local_whisper" } });

    expect(screen.queryByLabelText("whisper.cpp binary")).toBeNull();
    expect(screen.queryByLabelText("Whisper model")).toBeNull();
    expect(screen.getByText(/ships its multilingual Whisper model with the app/i)).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Save voice settings" }));
    await waitFor(() => expect(invoke).toHaveBeenCalledWith("m7_config_save", {
      config: {
        ...CONFIG,
        voice: { ...CONFIG.voice, backend: "local_whisper" },
      },
    }));
  });

  it("discovers healthy STT capabilities and saves the selected typed backend", async () => {
    render(<CompanionPanel />);

    await screen.findByText("Voice and transcription");
    expect(invoke).toHaveBeenCalledWith("extensions_active_capabilities", { kind: "stt" });
    expect((screen.getByLabelText("Backend") as HTMLSelectElement).value).toBe("executable_extension");

    const capability = await screen.findByLabelText(/Executable STT capability/);
    expect(capability.textContent).toContain("Private transcription");
    fireEvent.change(capability, {
      target: { value: JSON.stringify([STT_CAPABILITY.extension_id, STT_CAPABILITY.capability_id]) },
    });
    fireEvent.click(screen.getByRole("button", { name: "Save voice settings" }));

    await waitFor(() => expect(invoke).toHaveBeenCalledWith("m7_config_save", {
      config: {
        ...CONFIG,
        voice: {
          ...CONFIG.voice,
          extensionId: "dev.example.stt",
          extensionCapabilityId: "transcribe",
        },
      },
    }));
  });
});

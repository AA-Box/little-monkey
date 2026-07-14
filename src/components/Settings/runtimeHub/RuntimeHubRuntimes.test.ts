import { describe, expect, it } from "vitest";

import { keepAliveForRuntime } from "./RuntimeHubRuntimes";

describe("runtime load keep-alive policy", () => {
  it("never sends keep_alive to managed llama.cpp", () => {
    expect(keepAliveForRuntime("llama_cpp", "forever", 10)).toBeNull();
    expect(keepAliveForRuntime("llama_cpp", "duration", 10)).toBeNull();
  });

  it("preserves supported forever and duration modes", () => {
    expect(keepAliveForRuntime("ollama", "forever", 10)).toEqual({ mode: "forever" });
    expect(keepAliveForRuntime("mlx", "duration", 5)).toEqual({
      mode: "duration_ms",
      milliseconds: 300_000,
    });
  });

  it("bounds malformed duration input before IPC", () => {
    expect(keepAliveForRuntime("ollama", "duration", Number.NaN)).toEqual({
      mode: "duration_ms",
      milliseconds: 600_000,
    });
    expect(keepAliveForRuntime("ollama", "duration", 99_999)).toEqual({
      mode: "duration_ms",
      milliseconds: 86_400_000,
    });
  });
});

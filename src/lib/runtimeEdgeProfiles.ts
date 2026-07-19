import type {
  HardwareProfile,
  HardwareSnapshot,
  M3HardwareCompatibilityReport,
} from "./runtimeHubClient";

export type EdgeProfileKind =
  | "jetson"
  | "raspberry_pi"
  | "apple_silicon"
  | "legacy_cuda"
  | "amd_apu"
  | "low_memory_homelab"
  | "mini_pc"
  | "general";

export interface EdgeRuntimeProfile {
  kind: EdgeProfileKind;
  label: string;
  confidence: "confirmed" | "inferred";
  summary: string;
  recommendedRuntime: "llama_cpp" | "ollama" | "mlx";
  recommendedModelClass: string;
  expectedSpeed: string;
  contextTokens: number;
  processSlots: number;
  requiredComponents: string[];
  fallbacks: string[];
  evidence: string[];
}

const GIB = 1024 ** 3;

function containsAny(value: string, needles: string[]): boolean {
  const lower = value.toLowerCase();
  return needles.some((needle) => lower.includes(needle));
}

function acceleratorNames(snapshot: HardwareSnapshot): string[] {
  return snapshot.platform.accelerators.flatMap((accelerator) => accelerator.device_names);
}

function baseProfile(
  snapshot: HardwareSnapshot,
  profile: HardwareProfile,
  compatibility: M3HardwareCompatibilityReport | null,
): EdgeRuntimeProfile {
  const memoryGiB = snapshot.total_ram_bytes / GIB;
  const names = acceleratorNames(snapshot);
  const evidence = [
    `${snapshot.platform.os}/${snapshot.platform.arch}`,
    `${memoryGiB.toFixed(1)} GiB RAM`,
    `${snapshot.logical_cpu_count} logical CPUs`,
  ];
  if (names.length) evidence.push(`Detected accelerator: ${names.join(", ")}`);

  if (compatibility?.jetson.detected) {
    const model = compatibility.jetson.model ?? "NVIDIA Jetson";
    return {
      kind: "jetson",
      label: "NVIDIA Jetson",
      confidence: "confirmed",
      summary: `${model} needs a Jetson/L4T-compatible CUDA runtime; desktop CUDA artifacts are not interchangeable.`,
      recommendedRuntime: "llama_cpp",
      recommendedModelClass: memoryGiB >= 16 ? "7B–13B Q4, subject to measured fit" : "1B–7B Q4",
      expectedSpeed: "Hardware-dependent; run the local benchmark before enabling long contexts or parallel work.",
      contextTokens: memoryGiB >= 16 ? 8192 : 4096,
      processSlots: 1,
      requiredComponents: ["Jetson/L4T-compatible CUDA build", "matching NVIDIA driver/runtime", "verified GGUF model"],
      fallbacks: ["CPU-only llama.cpp with a smaller Q4 model", "Reduce context before reducing safety reserve"],
      evidence: [...evidence, model],
    };
  }

  const allNames = names.join(" ");
  const raspberryEvidence = containsAny(allNames, ["videocore", "raspberry", "broadcom"]);
  if (snapshot.platform.os === "linux" && snapshot.platform.arch === "aarch64" && (raspberryEvidence || memoryGiB <= 16)) {
    return {
      kind: "raspberry_pi",
      label: raspberryEvidence ? "Raspberry Pi" : "ARM SBC / Raspberry Pi-class",
      confidence: raspberryEvidence ? "confirmed" : "inferred",
      summary: raspberryEvidence
        ? "A Raspberry Pi-class ARM device was detected. Prefer CPU-first, small quantized models and one resident process."
        : "This looks like a low-power Linux ARM board, but the OS did not expose a board name. Apply the Raspberry Pi-safe profile until a benchmark proves more headroom.",
      recommendedRuntime: "llama_cpp",
      recommendedModelClass: memoryGiB >= 8 ? "1B–3B Q4" : "sub-2B Q4",
      expectedSpeed: "Interactive only for small models; large models and long context are expected to be slow.",
      contextTokens: memoryGiB >= 8 ? 4096 : 2048,
      processSlots: 1,
      requiredComponents: ["ARM64 llama.cpp build", "verified GGUF model", "active cooling for sustained inference"],
      fallbacks: ["Use a paired homelab runner", "Choose a smaller quantization/context"],
      evidence,
    };
  }

  const hasMetal = snapshot.platform.accelerators.some((entry) => entry.kind === "metal" && entry.available);
  if (snapshot.platform.os === "macos" && snapshot.platform.arch === "aarch64" && hasMetal) {
    return {
      kind: "apple_silicon",
      label: "Apple Silicon",
      confidence: "confirmed",
      summary: "Unified memory can serve CPU and GPU workloads, but model weights, KV cache, and other apps share the same budget.",
      recommendedRuntime: snapshot.platform.supported_runtimes.includes("llama_cpp") ? "llama_cpp" : "mlx",
      recommendedModelClass: memoryGiB >= 32 ? "7B–30B Q4, subject to measured fit" : memoryGiB >= 16 ? "3B–13B Q4" : "1B–7B Q4",
      expectedSpeed: "Use the built-in benchmark; unified-memory pressure is a stronger limit than advertised model size.",
      contextTokens: memoryGiB >= 32 ? 16384 : memoryGiB >= 16 ? 8192 : 4096,
      processSlots: Math.max(1, Math.min(2, profile.recommended_process_slots)),
      requiredComponents: ["Metal-enabled llama.cpp or verified MLX runtime", "verified model package"],
      fallbacks: ["Reduce context/KV cache", "Use a smaller quantization", "Unload competing resident models"],
      evidence,
    };
  }

  const cuda = compatibility?.accelerators.find((entry) => entry.kind === "cuda");
  if (cuda?.status === "driver_too_old") {
    return {
      kind: "legacy_cuda",
      label: "Legacy CUDA GPU",
      confidence: "confirmed",
      summary: "The NVIDIA device was found, but its driver or compute capability is below the supported acceleration floor.",
      recommendedRuntime: "llama_cpp",
      recommendedModelClass: "CPU-fit Q4 model until the driver/runtime gate passes",
      expectedSpeed: "GPU acceleration is blocked; CPU throughput is the honest expectation.",
      contextTokens: memoryGiB >= 16 ? 4096 : 2048,
      processSlots: 1,
      requiredComponents: ["Supported NVIDIA driver", "matching CUDA-enabled llama.cpp build"],
      fallbacks: ["CPU-only llama.cpp", "Upgrade the driver/runtime before selecting CUDA"],
      evidence: [...evidence, cuda.summary],
    };
  }

  const hasAmdApu = containsAny(allNames, ["radeon graphics", "apu", "vega"]);
  if (hasAmdApu) {
    return {
      kind: "amd_apu",
      label: "AMD APU",
      confidence: "inferred",
      summary: "An integrated AMD graphics device was detected. Shared-memory acceleration varies by OS and installed runtime.",
      recommendedRuntime: "llama_cpp",
      recommendedModelClass: memoryGiB >= 16 ? "3B–7B Q4" : "1B–3B Q4",
      expectedSpeed: "Treat Vulkan/ROCm acceleration as provisional until the local benchmark completes.",
      contextTokens: memoryGiB >= 16 ? 4096 : 2048,
      processSlots: 1,
      requiredComponents: ["Confirmed Vulkan or ROCm driver", "matching llama.cpp backend", "verified GGUF model"],
      fallbacks: ["CPU-only llama.cpp", "Use a smaller model/context"],
      evidence,
    };
  }

  if (memoryGiB < 8) {
    return {
      kind: "low_memory_homelab",
      label: "Low-memory homelab",
      confidence: "confirmed",
      summary: "Available memory is the primary constraint. Keep one small model resident and preserve an OS safety reserve.",
      recommendedRuntime: "llama_cpp",
      recommendedModelClass: "sub-3B Q4",
      expectedSpeed: "Suitable for lightweight chat, routing, and embeddings; not large-agent concurrency.",
      contextTokens: 2048,
      processSlots: 1,
      requiredComponents: ["CPU llama.cpp build", "verified compact GGUF model"],
      fallbacks: ["Use a paired larger node", "Queue rather than parallelize runs"],
      evidence,
    };
  }

  const noDiscreteGpu = !snapshot.platform.accelerators.some((entry) =>
    entry.available && ["cuda", "rocm", "metal"].includes(entry.kind),
  );
  if (noDiscreteGpu && snapshot.logical_cpu_count <= 16 && memoryGiB <= 32) {
    return {
      kind: "mini_pc",
      label: "CPU / mini-PC profile",
      confidence: "inferred",
      summary: "This machine looks like a compact CPU-first node. Prefer one or two small quantized models and bounded context.",
      recommendedRuntime: "llama_cpp",
      recommendedModelClass: memoryGiB >= 16 ? "3B–7B Q4" : "1B–3B Q4",
      expectedSpeed: "Good for background and lightweight interactive work after local benchmarking.",
      contextTokens: memoryGiB >= 16 ? 8192 : 4096,
      processSlots: Math.max(1, Math.min(2, profile.recommended_process_slots)),
      requiredComponents: ["CPU-optimized llama.cpp build", "verified GGUF model"],
      fallbacks: ["Queue concurrent work", "Use a remote accelerator for larger models"],
      evidence,
    };
  }

  return {
    kind: "general",
    label: "General-purpose runtime",
    confidence: "inferred",
    summary: "No narrower edge-device signature was proven. Use the measured hardware-fit and benchmark results rather than a device-name guess.",
    recommendedRuntime: profile.preferred_accelerator === "metal" ? "mlx" : "llama_cpp",
    recommendedModelClass: "Choose from the live hardware-fit report",
    expectedSpeed: "Run the local benchmark for task-specific throughput and memory evidence.",
    contextTokens: profile.tier === "performance" ? 16384 : profile.tier === "balanced" ? 8192 : 4096,
    processSlots: profile.recommended_process_slots,
    requiredComponents: ["Verified runtime component", "verified model package"],
    fallbacks: ["Use the Runtime Hub offload planner", "Reduce context or model size when fit is marginal"],
    evidence,
  };
}

/**
 * Produces a conservative, explainable local device profile. It never claims
 * a backend is usable merely from a product name: confirmed compatibility and
 * the existing runtime/load gates remain authoritative.
 */
export function resolveEdgeRuntimeProfile(
  snapshot: HardwareSnapshot,
  profile: HardwareProfile,
  compatibility: M3HardwareCompatibilityReport | null,
): EdgeRuntimeProfile {
  return baseProfile(snapshot, profile, compatibility);
}

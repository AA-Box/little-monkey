/**
 * The parts of the Studio client that decide what the generation form offers.
 *
 * `availableConditioning` is what stands between a user and a three-minute
 * render that quietly ignored the image they gave it: `sd-server` accepts
 * `control_image` whether or not `--control-net` was loaded and simply drops it
 * when it was not. The backend refuses that combination too, so this is the
 * *offer* side of the same rule — and the two agreeing is the point.
 */
import { describe, expect, it } from "vitest";

import {
  alignDimension,
  availableConditioning,
  formatLaunchArgs,
  normalizeDimension,
  parseLaunchArgs,
  engineSupports,
  CONDITIONING_SLOTS,
  COMPONENT_SLOTS,
  type ComponentSlot,
  type EngineCapabilities,
  hasLaunchFlag,
  launchArgValue,
  setLaunchArg,
  setLaunchFlag,
} from "./studioClient";

/** A running engine reporting exactly these flags and nothing else. */
function reporting(features: Record<string, boolean>): EngineCapabilities {
  return { samplers: [], schedulers: [], upscalers: [], features };
}

describe("availableConditioning", () => {
  it("unlocks an image only when the weights that read it are loaded", () => {
    expect(availableConditioning(["checkpoint", "vae"])).toEqual(new Set());
    expect(availableConditioning(["checkpoint", "control_net"])).toEqual(new Set(["control"]));
    expect(availableConditioning(["ip_adapter", "clip_vision"])).toEqual(new Set(["ip_adapter"]));
  });

  it("treats PhotoMaker and PuLID as two routes to the same reference input", () => {
    expect(availableConditioning(["photo_maker"])).toEqual(new Set(["reference"]));
    expect(availableConditioning(["pulid_weights"])).toEqual(new Set(["reference"]));
    // Both loaded is still one input, not two.
    expect(availableConditioning(["photo_maker", "pulid_weights"])).toEqual(
      new Set(["reference"]),
    );
  });

  it("keeps the three kinds distinct", () => {
    // A ControlNet does not stand in for an IP-Adapter: the engine reads the
    // two from different request fields with different weights.
    expect(availableConditioning(["control_net"]).has("ip_adapter")).toBe(false);
    expect(availableConditioning(["control_net", "ip_adapter", "photo_maker"])).toEqual(
      new Set(["control", "ip_adapter", "reference"]),
    );
  });
});

describe("the engine's own feature flags", () => {
  it("offers everything while no engine is running", () => {
    // The pickers and inputs have to populate before the first launch, which
    // is the only state a fresh install is ever in.
    expect(engineSupports(null, "mask_image")).toBe(true);
    expect(availableConditioning(["control_net"], null)).toEqual(new Set(["control"]));
  });

  it("hides an input the running engine says it does not take", () => {
    const old = reporting({ init_image: true, control_image: false });
    expect(engineSupports(old, "mask_image")).toBe(false);
    // The weights are loaded and the engine still will not read them, which is
    // the whole reason this is a second gate rather than a nicer error.
    expect(availableConditioning(["control_net", "ip_adapter"], old)).toEqual(new Set());
  });

  it("still requires the weights when the engine supports the field", () => {
    const current = reporting({
      mask_image: true,
      control_image: true,
      ip_adapter_image: true,
      ref_images: true,
    });
    expect(availableConditioning(["checkpoint"], current)).toEqual(new Set());
    expect(availableConditioning(["checkpoint", "photo_maker"], current)).toEqual(
      new Set(["reference"]),
    );
  });
});

describe("COMPONENT_SLOTS", () => {
  it("gives every slot exactly one engine flag", () => {
    const slots = COMPONENT_SLOTS.map((entry) => entry.slot);
    expect(new Set(slots).size).toBe(slots.length);
    const flags = COMPONENT_SLOTS.map((entry) => entry.flag);
    expect(new Set(flags).size).toBe(flags.length);
    for (const flag of flags) expect(flag.startsWith("--")).toBe(true);
  });

  it("can offer every conditioning slot in the Models tab", () => {
    // A conditioning slot missing from this table is a weight file the user has
    // no way to attach, which makes the matching input unreachable rather than
    // merely undocumented.
    const listed = new Set<ComponentSlot>(COMPONENT_SLOTS.map((entry) => entry.slot));
    for (const slot of Object.keys(CONDITIONING_SLOTS) as ComponentSlot[]) {
      expect(listed.has(slot)).toBe(true);
    }
  });
});

describe("parseLaunchArgs", () => {
  it("splits on whitespace like it always did", () => {
    expect(parseLaunchArgs("--diffusion-fa --threads 8")).toEqual([
      "--diffusion-fa",
      "--threads",
      "8",
    ]);
    expect(parseLaunchArgs("   ")).toEqual([]);
    expect(parseLaunchArgs("")).toEqual([]);
  });

  it("keeps a quoted path with spaces in one argument", () => {
    // The whole reason this parser exists: --embd-dir, --hires-upscalers-dir,
    // --upscale-model and --ad-model all take a path, and a whitespace split
    // handed the engine a truncated one.
    expect(parseLaunchArgs("--embd-dir '/My Weights/embeddings'")).toEqual([
      "--embd-dir",
      "/My Weights/embeddings",
    ]);
    expect(parseLaunchArgs('--embd-dir "/My Weights/embeddings"')).toEqual([
      "--embd-dir",
      "/My Weights/embeddings",
    ]);
  });

  it("lets each quote style carry the other literally", () => {
    expect(parseLaunchArgs(`--name "Ahmad's models"`)).toEqual(["--name", "Ahmad's models"]);
    expect(parseLaunchArgs(`--name 'say "hi"'`)).toEqual(["--name", 'say "hi"']);
  });

  it("treats an explicitly empty argument as one the user typed", () => {
    expect(parseLaunchArgs('--model-args ""')).toEqual(["--model-args", ""]);
  });

  it("does not expand anything, because there is no shell behind it", () => {
    // Arguments go to Command::args directly, so a parser that expanded these
    // would promise a substitution that never happens.
    expect(parseLaunchArgs("--dir $HOME/*.safetensors")).toEqual([
      "--dir",
      "$HOME/*.safetensors",
    ]);
  });
});

describe("formatLaunchArgs", () => {
  it("round-trips whatever the parser produced", () => {
    for (const text of [
      "--diffusion-fa --threads 8",
      "--embd-dir '/My Weights/embeddings'",
      `--name "Ahmad's models"`,
      '--model-args ""',
    ]) {
      const args = parseLaunchArgs(text);
      expect(parseLaunchArgs(formatLaunchArgs(args))).toEqual(args);
    }
  });

  it("leaves ordinary arguments unquoted so the field stays readable", () => {
    expect(formatLaunchArgs(["--diffusion-fa", "--threads", "8"])).toBe(
      "--diffusion-fa --threads 8",
    );
  });
});

describe("setLaunchArg", () => {
  it("adds, replaces in place, and removes", () => {
    const added = setLaunchArg([], "--embd-dir", "/tmp/embeds");
    expect(added).toEqual(["--embd-dir", "/tmp/embeds"]);
    // In place: the user's own ordering survives a re-pick.
    const args = ["--threads", "8", "--embd-dir", "/old", "--vae-tiling"];
    expect(setLaunchArg(args, "--embd-dir", "/new")).toEqual([
      "--threads", "8", "--embd-dir", "/new", "--vae-tiling",
    ]);
    expect(setLaunchArg(args, "--embd-dir", null)).toEqual(["--threads", "8", "--vae-tiling"]);
    // Blank is removal, not an empty value the engine would choke on.
    expect(setLaunchArg(args, "--embd-dir", "   ")).toEqual(["--threads", "8", "--vae-tiling"]);
  });

  it("does not eat the next flag when one has no value", () => {
    expect(launchArgValue(["--vae-tiling", "--threads", "8"], "--vae-tiling")).toBeNull();
    expect(setLaunchArg(["--vae-tiling", "--threads", "8"], "--vae-tiling", null)).toEqual([
      "--threads", "8",
    ]);
  });

  it("round-trips a path with spaces through the args field", () => {
    const args = setLaunchArg([], "--embd-dir", "/Users/me/My Embeddings");
    expect(parseLaunchArgs(formatLaunchArgs(args))).toEqual(args);
    expect(launchArgValue(args, "--embd-dir")).toBe("/Users/me/My Embeddings");
  });
});

describe("setLaunchFlag", () => {
  it("adds and removes a flag that carries no value", () => {
    expect(setLaunchFlag([], "--vae-tiling", true)).toEqual(["--vae-tiling"]);
    expect(setLaunchFlag(["--vae-tiling"], "--vae-tiling", false)).toEqual([]);
  });

  it("is idempotent, so a toggle cannot add the same flag twice", () => {
    const once = setLaunchFlag([], "--circular", true);
    expect(setLaunchFlag(once, "--circular", true)).toEqual(["--circular"]);
    expect(setLaunchFlag([], "--circular", false)).toEqual([]);
  });

  /** The whole reason this is not `setLaunchArg`: removing a valueless flag
   *  must take one slot, not two, or it eats whatever the user typed next. */
  it("does not swallow the following flag when removing", () => {
    expect(setLaunchFlag(["--vae-tiling", "--threads", "8"], "--vae-tiling", false)).toEqual([
      "--threads",
      "8",
    ]);
    expect(setLaunchFlag(["--threads", "8", "--circular"], "--circular", false)).toEqual([
      "--threads",
      "8",
    ]);
  });

  it("leaves the hand-typed args around it alone", () => {
    const args = ["--diffusion-fa", "--embd-dir", "/tmp/e"];
    expect(setLaunchFlag(args, "--vae-tiling", true)).toEqual([...args, "--vae-tiling"]);
    expect(hasLaunchFlag(args, "--diffusion-fa")).toBe(true);
    expect(hasLaunchFlag(args, "--vae-tiling")).toBe(false);
  });

  it("survives the args field round trip", () => {
    const args = setLaunchFlag(setLaunchArg([], "--embd-dir", "/tmp/e"), "--vae-tiling", true);
    expect(parseLaunchArgs(formatLaunchArgs(args))).toEqual(args);
  });
});

describe("launch arg round trip", () => {
  it("round-trips a path with spaces through the args field", () => {
    const args = setLaunchArg([], "--embd-dir", "/Users/me/My Embeddings");
    expect(parseLaunchArgs(formatLaunchArgs(args))).toEqual(args);
    expect(launchArgValue(args, "--embd-dir")).toBe("/Users/me/My Embeddings");
  });
});

/** The controls answer a size with the nearest one the engine can render, and
 *  never with one the backend would then round again — which is what keeps the
 *  number in the field and the size of the picture the same number. */
describe("alignDimension", () => {
  it("takes a size to the nearest edge of the grid", () => {
    expect(alignDimension(645)).toBe(640);
    expect(alignDimension(890)).toBe(896);
    expect(alignDimension(1024)).toBe(1024);
  });

  it("never lands on a size the backend would round further", () => {
    for (let value = 32; value <= 4096; value += 7) {
      const aligned = alignDimension(value);
      expect(normalizeDimension(aligned)).toBe(aligned);
    }
  });

  it("stays inside the engine's limits", () => {
    expect(alignDimension(0)).toBe(32);
    expect(alignDimension(-4)).toBe(32);
    expect(alignDimension(99_999)).toBe(4096);
  });
});

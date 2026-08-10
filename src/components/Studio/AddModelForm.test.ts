/**
 * Both flag tables render `t(labelKey)` with a variable, so the i18n key-lint —
 * which only scans call sites whose key is written out as a literal — cannot see
 * any of these. A typo would ship as a raw key string beside a checkbox with
 * nothing failing, so they are checked here instead.
 *
 * The flags themselves are pinned for a different reason: each is a launch
 * argument handed to the engine verbatim, and one misspelt is not a missing
 * feature but a failed launch, because `sd-server` rejects an argument it does
 * not know rather than ignoring it.
 */
import { describe, expect, it } from "vitest";

import { DIRECTORY_FLAGS, ENGINE_TOGGLES } from "./AddModelForm";
import { en } from "../../lib/i18n/locales/en";

const ALL = [...DIRECTORY_FLAGS, ...ENGINE_TOGGLES];

describe("the engine flag tables", () => {
  it("name a label and a hint that English actually defines", () => {
    for (const { flag, labelKey, hintKey } of ALL) {
      expect(en[labelKey as keyof typeof en], `${flag} → ${labelKey}`).toBeTruthy();
      expect(en[hintKey as keyof typeof en], `${flag} → ${hintKey}`).toBeTruthy();
    }
  });

  it("spell every flag as a long option, since it is passed through verbatim", () => {
    for (const { flag } of ALL) {
      expect(flag.startsWith("--"), flag).toBe(true);
      expect(flag.trim(), flag).toBe(flag);
    }
  });

  /** Two controls writing the same flag would fight: toggling one would appear
   *  to undo the other, and `setLaunchFlag` would leave whichever ran last. */
  it("give each flag exactly one control", () => {
    const flags = ALL.map((entry) => entry.flag);
    expect(new Set(flags).size).toBe(flags.length);
  });

  /** A toggle is a valueless switch and a directory flag takes a path. Moving
   *  one to the wrong table would write `--vae-tiling /some/path`, which the
   *  engine reads as a stray positional argument. */
  it("keeps valueless switches out of the directory table", () => {
    for (const { flag } of DIRECTORY_FLAGS) {
      expect(flag.endsWith("-dir"), flag).toBe(true);
    }
    for (const { flag } of ENGINE_TOGGLES) {
      expect(flag.endsWith("-dir"), flag).toBe(false);
    }
  });
});

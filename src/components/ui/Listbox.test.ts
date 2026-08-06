import { describe, expect, it } from "vitest";

import { matchTypeAhead, type ListboxOption } from "./Listbox";

/** Type-ahead is the one behaviour a custom listbox loses silently: nothing
 *  looks broken, the keys just stop finding rows. */
describe("matchTypeAhead", () => {
  const options: ListboxOption[] = [
    { value: "sd", label: "SD turbo", detail: "SD · 5.2 GB" },
    { value: "h3", label: "MiniMax H3", detail: "MiniMax · 30.3 GB" },
    { value: "wan", label: "Wan 2.2 TI2V", detail: "Wan · 11.4 GB" },
  ];

  it("prefers a prefix, the way a platform menu does", () => {
    expect(matchTypeAhead(options, "s")).toBe(0);
    expect(matchTypeAhead(options, "mini")).toBe(1);
    expect(matchTypeAhead(options, "w")).toBe(2);
    // Case is not something anyone types deliberately.
    expect(matchTypeAhead(options, "SD T")).toBe(0);
  });

  it("falls back to a substring so a search that can only mean one row finds it", () => {
    expect(matchTypeAhead(options, "h3")).toBe(1);
    expect(matchTypeAhead(options, "turbo")).toBe(0);
  });

  it("answers -1 rather than moving the highlight somewhere arbitrary", () => {
    expect(matchTypeAhead(options, "zzz")).toBe(-1);
    expect(matchTypeAhead(options, "   ")).toBe(-1);
    expect(matchTypeAhead([], "s")).toBe(-1);
  });
});

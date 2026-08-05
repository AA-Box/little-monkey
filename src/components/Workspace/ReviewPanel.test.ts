/**
 * Pins the decisions that make the former standalone Diff panel's behaviour
 * survive inside `ReviewPanel` — the point of folding it in was to lose
 * nothing, and the one way to lose something quietly is for `buildRows` to
 * drop a file `git_review` capped, or for a row with no content to render as
 * an empty diff instead of saying why it is empty.
 *
 * Pure functions only: this repo has no DOM test environment
 * (vitest.config.ts uses `environment: "node"`), the convention
 * `PermissionModal.test.tsx` follows with `canRememberForSession`.
 */
import { describe, expect, it } from "vitest";

import { buildRows, deriveStatus, unavailableKey } from "./ReviewPanel";

function payloadFile(overrides: Partial<Parameters<typeof deriveStatus>[0]> = {}) {
  return {
    path: "src/a.ts",
    old_content: "old",
    new_content: "new",
    added: 1,
    deleted: 1,
    binary: false,
    ...overrides,
  };
}

describe("deriveStatus", () => {
  it("reads add/delete/modify off the content when there is content", () => {
    expect(deriveStatus(payloadFile({ old_content: "", new_content: "x" }))).toBe("A");
    expect(deriveStatus(payloadFile({ old_content: "x", new_content: "" }))).toBe("D");
    expect(deriveStatus(payloadFile())).toBe("M");
  });

  it("falls back to the counts for a binary file, which carries no content", () => {
    expect(deriveStatus(payloadFile({ binary: true, added: 4, deleted: 0 }))).toBe("A");
    expect(deriveStatus(payloadFile({ binary: true, added: 0, deleted: 4 }))).toBe("D");
    expect(deriveStatus(payloadFile({ binary: true, added: 2, deleted: 2 }))).toBe("M");
    // A binary file git reports as "-\t-" has no counts at all — "M" is the
    // honest answer, not a guess at add-or-delete.
    expect(deriveStatus(payloadFile({ binary: true, added: 0, deleted: 0 }))).toBe("M");
  });
});

describe("buildRows", () => {
  const files = [payloadFile({ path: "src/a.ts" }), payloadFile({ path: "src/b.ts", binary: true })];

  it("branch mode uses the payload alone, with derived status letters", () => {
    const rows = buildRows(files, null);
    expect(rows.map((row) => row.path)).toEqual(["src/a.ts", "src/b.ts"]);
    expect(rows[0].status).toBe("M");
    // A binary payload row carries no content, so it must not present as an
    // empty diff.
    expect(rows[1].content).toBeNull();
    expect(rows[1].binary).toBe(true);
  });

  it("working mode keeps every file porcelain reports, including ones past git_review's cap", () => {
    const changed = [
      { path: "src/a.ts", status: "M" },
      { path: "src/b.ts", status: "D" },
      // Past `MAX_REVIEW_FILES`: listed by the uncapped command, absent from
      // the payload. Losing this row is exactly the regression the old Diff
      // panel would have suffered.
      { path: "src/capped.ts", status: "A" },
    ];
    const rows = buildRows(files, changed);

    expect(rows.map((row) => row.path)).toEqual(["src/a.ts", "src/b.ts", "src/capped.ts"]);
    const capped = rows[2];
    expect(capped.content).toBeNull();
    expect(capped.binary).toBe(false);
    expect(capped.oversize).toBe(false);
  });

  it("prefers porcelain's real status letter over the derived one", () => {
    const rows = buildRows([payloadFile({ path: "src/a.ts", old_content: "x", new_content: "y" })], [
      { path: "src/a.ts", status: "R" },
    ]);
    expect(rows[0].status).toBe("R");
    expect(deriveStatus(payloadFile({ old_content: "x", new_content: "y" }))).toBe("M");
  });

  it("drops a payload file porcelain does not report, since working mode's list is porcelain's", () => {
    const rows = buildRows(files, [{ path: "src/a.ts", status: "M" }]);
    expect(rows.map((row) => row.path)).toEqual(["src/a.ts"]);
  });
});

describe("unavailableKey", () => {
  const row = { path: "src/a.ts", status: "M", added: 1, deleted: 1, binary: false, oversize: false, content: null };

  it("distinguishes every reason a row has no diff", () => {
    expect(unavailableKey({ ...row, binary: true }, false)).toBe("ReviewPanel.binaryFile");
    // `git_file_diff` keeps oversize apart from binary where `git_review`
    // collapses both — the lazily loaded row can say which.
    expect(unavailableKey({ ...row, oversize: true }, false)).toBe("ReviewPanel.oversizeFile");
    expect(unavailableKey(row, true)).toBe("ReviewPanel.loadingFile");
    expect(unavailableKey(row, false)).toBe("ReviewPanel.notLoadedFile");
  });

  it("returns null once content is present, so a real diff renders", () => {
    expect(unavailableKey({ ...row, content: { old: "a", new: "b" } }, false)).toBeNull();
  });

  it("reports binary ahead of a loading fetch, since the fetch cannot change it", () => {
    expect(unavailableKey({ ...row, binary: true }, true)).toBe("ReviewPanel.binaryFile");
  });
});

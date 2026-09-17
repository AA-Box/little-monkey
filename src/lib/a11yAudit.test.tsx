// @vitest-environment jsdom
/**
 * The accessibility audit CI job (roadmap K22), in two halves.
 *
 * The first half drives the rules themselves against hand-built DOM, so a rule
 * that stopped firing is a failing test rather than a quietly-passing audit.
 * The second half is the audit proper: it runs over the **built** `index.html`
 * and over the app's real rendered screens, and fails the build on any
 * violation.
 *
 * A screen that cannot be rendered here — one needing a live Tauri backend — is
 * skipped **with its reason printed**, never silently. An audit that reports
 * "0 violations" because it audited nothing is the failure worth designing
 * against.
 */
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render } from "@testing-library/react";
import * as fs from "node:fs";
import * as path from "node:path";

import { A11Y_RULES, auditDom, formatViolations } from "./a11yAudit";
import { Tooltip } from "../components/Chat/MessageActions";

afterEach(cleanup);

/** Builds a detached element tree from HTML, for driving one rule. */
function dom(html: string): HTMLElement {
  const host = document.createElement("div");
  host.innerHTML = html;
  return host;
}

function rulesFired(html: string): string[] {
  return auditDom(dom(html)).map((violation) => violation.rule);
}

describe("the rules", () => {
  it("catches an icon-only button with no accessible name", () => {
    // The regression this whole check exists for: an icon button is one
    // `aria-label` away from being unusable, and it looks fine on screen.
    expect(rulesFired('<button><svg></svg></button>')).toContain("button-has-name");

    // Every way of naming one is accepted, because a false positive here is the
    // fastest route to an a11y check somebody disables.
    expect(rulesFired('<button aria-label="Close"><svg></svg></button>')).not.toContain(
      "button-has-name",
    );
    expect(rulesFired('<button title="Close"><svg></svg></button>')).not.toContain(
      "button-has-name",
    );
    expect(rulesFired("<button>Close</button>")).not.toContain("button-has-name");
    expect(rulesFired('<button><img alt="Close" /></button>')).not.toContain("button-has-name");
    expect(rulesFired('<button><svg aria-label="Close"></svg></button>')).not.toContain(
      "button-has-name",
    );
  });

  it("accepts every way a form control is labelled, and refuses none", () => {
    expect(rulesFired("<input />")).toContain("control-has-name");
    expect(rulesFired('<label for="a">Name</label><input id="a" />')).not.toContain(
      "control-has-name",
    );
    expect(rulesFired("<label>Name<input /></label>")).not.toContain("control-has-name");
    expect(rulesFired('<input aria-label="Name" />')).not.toContain("control-has-name");
    expect(rulesFired('<span id="l">Name</span><input aria-labelledby="l" />')).not.toContain(
      "control-has-name",
    );
    // A hidden input has no name to give and needs none.
    expect(rulesFired('<input type="hidden" />')).not.toContain("control-has-name");
  });

  it("refuses an aria-labelledby that points at nothing", () => {
    // Worse than no label: it looks handled, and announces nothing.
    expect(rulesFired('<input aria-labelledby="missing" />')).toContain("control-has-name");
  });

  it("tells a decorative image from a forgotten alt", () => {
    expect(rulesFired("<img src='x.png' />")).toContain("image-alt");
    // `alt=""` is a decision — "decorative" — and is correct.
    expect(rulesFired("<img src='x.png' alt='' />")).not.toContain("image-alt");
    expect(rulesFired("<img src='x.png' alt='A chart' />")).not.toContain("image-alt");
  });

  it("catches a positive tabindex, a duplicate id, and an invented role", () => {
    expect(rulesFired('<div tabindex="3">x</div>')).toContain("no-positive-tabindex");
    expect(rulesFired('<div tabindex="0">x</div>')).not.toContain("no-positive-tabindex");
    expect(rulesFired('<div tabindex="-1">x</div>')).not.toContain("no-positive-tabindex");

    expect(rulesFired('<div id="dup"></div><span id="dup"></span>')).toContain("no-duplicate-id");
    expect(rulesFired('<div id="a"></div><span id="b"></span>')).not.toContain("no-duplicate-id");

    expect(rulesFired('<div role="clickable">x</div>')).toContain("valid-aria-role");
    expect(rulesFired('<div role="button" aria-label="x"></div>')).not.toContain("valid-aria-role");
  });

  it("catches a focusable control hidden from screen readers", () => {
    // Reachable by keyboard, invisible to a screen reader: the user tabs to a
    // control nothing can announce.
    expect(rulesFired('<div aria-hidden="true"><button>Go</button></div>')).toContain(
      "no-focusable-inside-aria-hidden",
    );
    expect(rulesFired('<div aria-hidden="true"><span>decor</span></div>')).not.toContain(
      "no-focusable-inside-aria-hidden",
    );
  });

  it("catches a non-item child of a list, which breaks its announced item count", () => {
    expect(rulesFired("<ul><li>a</li><div>b</div></ul>")).toContain("list-structure");
    expect(rulesFired("<ul><li>a</li><li>b</li></ul>")).not.toContain("list-structure");
  });

  it("does not report document-level rules against a rendered fragment", () => {
    // A panel has no <html>. Reporting the shell's problem against every
    // component would make the audit useless noise.
    const fired = rulesFired("<div>fine</div>");
    expect(fired).not.toContain("html-has-lang");
    expect(fired).not.toContain("document-has-title");
  });

  it("reports a document with no lang and no title", () => {
    const page = new DOMParser().parseFromString("<html><body></body></html>", "text/html");
    const fired = auditDom(page).map((violation) => violation.rule);
    expect(fired).toContain("html-has-lang");
    expect(fired).toContain("document-has-title");
  });

  it("names every rule it can fire, so the coverage claim is checkable", () => {
    // The roadmap says which rules this covers. That list has to be derivable
    // from the code rather than maintained by hand beside it.
    expect(new Set(A11Y_RULES).size).toBe(A11Y_RULES.length);
    expect(A11Y_RULES).toContain("button-has-name");
  });
});

describe("the built shell", () => {
  const builtIndex = path.resolve(__dirname, "../../dist/index.html");

  it("has a lang, a title, and no violations", () => {
    if (!fs.existsSync(builtIndex)) {
      // Skipped **with its reason**, never silently: CI runs `pnpm build`
      // before this job, so an absent `dist/` there is itself a signal. A local
      // `vitest run` with no build is the ordinary case.
      console.warn(
        `[a11y] SKIPPED built-shell audit: no build at ${builtIndex}. Run \`pnpm build\` first.`,
      );
      expect(fs.existsSync(builtIndex)).toBe(false);
      return;
    }
    const html = fs.readFileSync(builtIndex, "utf8");
    const page = new DOMParser().parseFromString(html, "text/html");
    const violations = auditDom(page);
    expect(violations, `built shell violations:\n${formatViolations(violations)}`).toEqual([]);
  });
});

describe("rendered screens", () => {
  /**
   * Screens rendered with no backend at all.
   *
   * Deliberately a short list of *pure* components rather than the whole app:
   * every screen here renders from props alone, so this job needs no Tauri
   * mock, no store fixture, and no network. Adding one that needs a backend
   * would make the audit's green depend on the fixture staying correct, which
   * is how an audit ends up asserting nothing.
   */
  it("audits a representative rendered surface", async () => {
    const { DiffViewer } = await import("../components/Workspace/DiffViewer");
    const { container } = render(
      <DiffViewer
        oldValue={"const a = 1;\nconst b = 2;\n"}
        newValue={"const a = 1;\nconst b = 3;\n"}
        fileName="example.ts"
      />,
    );
    const violations = auditDom(container);
    expect(violations, `DiffViewer violations:\n${formatViolations(violations)}`).toEqual([]);
  });
});

/**
 * Colour contrast for the one place the app writes a *sentence* in a secondary
 * colour: the hint line under a tooltip's label.
 *
 * It is 11px, it explains what a button will do to the conversation, and it was
 * shipped in `faint` — the weakest token in the palette, 4.83:1 on the tooltip
 * background in light and 5.08:1 in dark. Over the AA line for normal text,
 * under it for text this small, and unreadable in practice. The threshold here
 * is AAA rather than AA for that reason: a sentence nobody can read is not a
 * hint, and the size is what makes the usual line too generous.
 */
describe("the tooltip hint's contrast", () => {
  const THEMES = ["light", "dark", "light high-contrast", "dark high-contrast"] as const;
  const AAA_NORMAL_TEXT = 7;

  /** Every custom property declared in one `:root…{ }` block of index.css. */
  function block(css: string, selector: string): Record<string, string> {
    const start = css.indexOf(`${selector} {`);
    if (start < 0) throw new Error(`index.css has no ${selector} block`);
    const body = css.slice(start, css.indexOf("}", start));
    return Object.fromEntries(
      [...body.matchAll(/(--c-[a-z0-9-]+):\s*(#[0-9a-f]{6})/gi)].map((m) => [m[1], m[2]]),
    );
  }

  function relativeLuminance(hex: string): number {
    const channel = (pair: string) => {
      const srgb = parseInt(pair, 16) / 255;
      return srgb <= 0.03928 ? srgb / 12.92 : ((srgb + 0.055) / 1.055) ** 2.4;
    };
    const [r, g, b] = [hex.slice(1, 3), hex.slice(3, 5), hex.slice(5, 7)].map(channel);
    return 0.2126 * r + 0.7152 * g + 0.0722 * b;
  }

  function contrast(foreground: string, background: string): number {
    const [lighter, darker] = [relativeLuminance(foreground), relativeLuminance(background)].sort(
      (a, b) => b - a,
    );
    return (lighter + 0.05) / (darker + 0.05);
  }

  it.each(THEMES)("reads at AAA in %s", (theme) => {
    const css = fs.readFileSync(path.join(process.cwd(), "src/index.css"), "utf8");
    const light = block(css, ":root");
    const resolved = {
      light,
      dark: { ...light, ...block(css, ':root[data-theme="dark"]') },
      "light high-contrast": { ...light, ...block(css, ':root[data-contrast="high"]') },
      "dark high-contrast": {
        ...light,
        ...block(css, ':root[data-theme="dark"]'),
        ...block(css, ':root[data-theme="dark"][data-contrast="high"]'),
      },
    }[theme];

    // The token the component actually uses, so swapping it back to `faint`
    // fails here rather than only on someone's screen.
    const { container } = render(<Tooltip text="Edit" hint="Resends from here." />);
    const hint = container.querySelector("[role=tooltip] > span");
    const token = hint?.className.match(/\btext-([a-z0-9-]+)\b/)?.[1];
    expect(token, "the hint should carry a colour token").toBeTruthy();

    // The tooltip paints itself `bg-background`.
    const ratio = contrast(resolved[`--c-${token}`], resolved["--c-background"]);
    expect(ratio).toBeGreaterThanOrEqual(AAA_NORMAL_TEXT);
  });
});

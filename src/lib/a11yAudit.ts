/**
 * A named, enforceable accessibility rule set, run in CI over the built shell
 * and over the app's real rendered screens (roadmap K22).
 *
 * # Why a rule set here and not axe-core
 *
 * axe-core is the better tool and this is not a re-implementation of it. It is a
 * *subset*, chosen because the alternative on offer was no audit at all: this
 * repo has no browser in CI and adding one (or a headless-Chrome harness) for
 * the first automated a11y check is a much larger change than the check itself.
 * jsdom plus these rules runs in the existing `vitest` job in milliseconds.
 *
 * **So this must not be described as a WCAG audit, and is not.** It is the
 * subset below, each rule chosen because it is (a) decidable from the DOM alone,
 * with no layout, contrast or focus-order simulation, and (b) a defect this
 * codebase can actually introduce — an icon-only button is one `aria-label`
 * away from being unusable by a screen reader, and that is the regression this
 * exists to catch. Contrast, focus order, live-region timing and reading order
 * are **not covered** and are stated as not covered rather than implied.
 *
 * Upgrade path: swap [`auditDom`]'s body for an `axe.run` call once a browser
 * or an axe dependency is on the table. The call sites and the CI job stay.
 */

/** One accessibility defect found in a DOM tree. */
export interface A11yViolation {
  /** Stable rule id, so a suppression or a report can name one rule. */
  rule: string;
  /** What is wrong, in the words a fixer needs. */
  message: string;
  /** A short selector-ish description of the offending element. */
  element: string;
}

/** Every rule this audit knows, so a caller can name what it does and does not cover. */
export const A11Y_RULES = [
  "html-has-lang",
  "document-has-title",
  "image-alt",
  "control-has-name",
  "button-has-name",
  "link-has-name",
  "no-positive-tabindex",
  "no-duplicate-id",
  "valid-aria-role",
  "no-focusable-inside-aria-hidden",
  "list-structure",
] as const;

/** Roles a `role=` attribute may name. From the ARIA 1.2 role list. */
const ARIA_ROLES = new Set([
  "alert", "alertdialog", "application", "article", "banner", "blockquote", "button",
  "caption", "cell", "checkbox", "code", "columnheader", "combobox", "complementary",
  "contentinfo", "definition", "deletion", "dialog", "directory", "document", "emphasis",
  "feed", "figure", "form", "generic", "grid", "gridcell", "group", "heading", "img",
  "insertion", "link", "list", "listbox", "listitem", "log", "main", "marquee", "math",
  "menu", "menubar", "menuitem", "menuitemcheckbox", "menuitemradio", "meter", "navigation",
  "none", "note", "option", "paragraph", "presentation", "progressbar", "radio", "radiogroup",
  "region", "row", "rowgroup", "rowheader", "scrollbar", "search", "searchbox", "separator",
  "slider", "spinbutton", "status", "strong", "subscript", "superscript", "switch", "tab",
  "table", "tablist", "tabpanel", "term", "textbox", "time", "timer", "toolbar", "tooltip",
  "tree", "treegrid", "treeitem",
]);

/** Elements that take a name from a `<label>`, and therefore need one. */
const LABELABLE = new Set(["input", "select", "textarea"]);

/**
 * Input types with no user-facing name of their own to give.
 *
 * `hidden` is not rendered; a submit/reset/button input is named by its `value`,
 * which the generic name check below already reads.
 */
const UNNAMED_INPUT_TYPES = new Set(["hidden"]);

/**
 * Escapes an id for use inside an attribute selector.
 *
 * Hand-rolled rather than `CSS.escape`, which jsdom does not provide — and this
 * audit's whole point is that it runs without a browser. Only the characters
 * that can end a quoted attribute value need escaping here, since the id is
 * always interpolated into `[id="..."]`.
 */
function escapeAttributeValue(value: string): string {
  return value.replace(/["\\]/g, (match) => `\\${match}`);
}

function describe(element: Element): string {
  const tag = element.tagName.toLowerCase();
  const id = element.id ? `#${element.id}` : "";
  const cls =
    element.getAttribute("class")?.split(/\s+/).filter(Boolean).slice(0, 2).map((c) => `.${c}`).join("") ?? "";
  const text = (element.textContent ?? "").trim().slice(0, 30);
  return `<${tag}${id}${cls}>${text ? ` "${text}"` : ""}`;
}

/**
 * Whether `element` has a name a screen reader would announce.
 *
 * Deliberately generous — `title` counts, and so does an `alt` on a nested image
 * — because a false positive here is a CI failure for a working control, which
 * is the fastest way to get an a11y check disabled by whoever it blocks.
 */
function hasAccessibleName(element: Element, root: ParentNode): boolean {
  const aria = element.getAttribute("aria-label");
  if (aria && aria.trim()) return true;

  const labelledBy = element.getAttribute("aria-labelledby");
  if (labelledBy) {
    // Only counts if the referenced element actually exists: a dangling
    // `aria-labelledby` announces nothing, which is worse than none at all
    // because it looks handled.
    const named = labelledBy
      .split(/\s+/)
      .filter(Boolean)
      .some((id) => {
        const target = root.querySelector(`[id="${escapeAttributeValue(id)}"]`);
        return !!target && (target.textContent ?? "").trim().length > 0;
      });
    if (named) return true;
  }

  const title = element.getAttribute("title");
  if (title && title.trim()) return true;

  if ((element.textContent ?? "").trim()) return true;

  // An icon-only control whose only content is an image with alt text is named.
  const nestedAlt = element.querySelector("img[alt], svg[aria-label], [role='img'][aria-label]");
  if (nestedAlt) {
    const alt = nestedAlt.getAttribute("alt") ?? nestedAlt.getAttribute("aria-label");
    if (alt && alt.trim()) return true;
  }

  if (element.tagName.toLowerCase() === "input") {
    const value = element.getAttribute("value");
    if (value && value.trim()) return true;
  }

  const id = element.getAttribute("id");
  if (id) {
    const label = root.querySelector(`label[for="${escapeAttributeValue(id)}"]`);
    if (label && (label.textContent ?? "").trim()) return true;
  }
  // A control wrapped in its own label is labelled by it.
  const wrapping = element.closest("label");
  if (wrapping && (wrapping.textContent ?? "").trim()) return true;

  return false;
}

/** Whether this element is hidden from the accessibility tree by an ancestor. */
function isAriaHidden(element: Element): boolean {
  return element.closest('[aria-hidden="true"]') !== null;
}

/**
 * Runs every rule over `root`, which may be a whole `Document` or one rendered
 * container.
 *
 * Document-level rules (`html-has-lang`, `document-has-title`) are skipped when
 * `root` is a fragment: a rendered panel has no `<html>`, and reporting one
 * missing would make every component test fail for the shell's problem.
 */
export function auditDom(root: Document | Element): A11yViolation[] {
  const violations: A11yViolation[] = [];
  const scope: ParentNode = root;
  const document_ = "documentElement" in root ? (root as Document) : null;

  const add = (rule: string, message: string, element: Element | null) =>
    violations.push({ rule, message, element: element ? describe(element) : "document" });

  if (document_) {
    const lang = document_.documentElement.getAttribute("lang");
    if (!lang || !lang.trim()) {
      add(
        "html-has-lang",
        "<html> has no lang attribute, so a screen reader cannot pick a pronunciation",
        null,
      );
    }
    const title = document_.title?.trim();
    if (!title) {
      add("document-has-title", "the document has no <title>", null);
    }
  }

  for (const image of scope.querySelectorAll("img")) {
    if (isAriaHidden(image)) continue;
    // An empty alt is a *decision* — "this image is decorative" — and is correct.
    // A missing one is the defect.
    if (!image.hasAttribute("alt")) {
      add("image-alt", "an <img> has no alt attribute (use alt=\"\" if decorative)", image);
    }
  }

  for (const control of scope.querySelectorAll("input, select, textarea")) {
    if (isAriaHidden(control)) continue;
    const tag = control.tagName.toLowerCase();
    if (!LABELABLE.has(tag)) continue;
    const type = control.getAttribute("type")?.toLowerCase() ?? "text";
    if (tag === "input" && UNNAMED_INPUT_TYPES.has(type)) continue;
    if (!hasAccessibleName(control, scope)) {
      add(
        "control-has-name",
        `a <${tag}> has no label, aria-label, aria-labelledby or title`,
        control,
      );
    }
  }

  for (const button of scope.querySelectorAll("button, [role='button']")) {
    if (isAriaHidden(button)) continue;
    if (!hasAccessibleName(button, scope)) {
      add(
        "button-has-name",
        "a button has no accessible name — an icon-only button needs aria-label or title",
        button,
      );
    }
  }

  for (const link of scope.querySelectorAll("a[href]")) {
    if (isAriaHidden(link)) continue;
    if (!hasAccessibleName(link, scope)) {
      add("link-has-name", "a link has no accessible name", link);
    }
  }

  for (const element of scope.querySelectorAll("[tabindex]")) {
    const raw = element.getAttribute("tabindex") ?? "";
    const value = Number.parseInt(raw, 10);
    if (Number.isFinite(value) && value > 0) {
      add(
        "no-positive-tabindex",
        `tabindex="${raw}" overrides the document's tab order for everyone, not just this control`,
        element,
      );
    }
  }

  const seenIds = new Map<string, number>();
  for (const element of scope.querySelectorAll("[id]")) {
    const id = element.id;
    if (!id) continue;
    seenIds.set(id, (seenIds.get(id) ?? 0) + 1);
  }
  for (const [id, count] of seenIds) {
    if (count > 1) {
      add(
        "no-duplicate-id",
        `id "${id}" appears ${count} times, so every label and aria-labelledby pointing at it resolves to the first one only`,
        null,
      );
    }
  }

  for (const element of scope.querySelectorAll("[role]")) {
    for (const role of (element.getAttribute("role") ?? "").split(/\s+/).filter(Boolean)) {
      if (!ARIA_ROLES.has(role)) {
        add("valid-aria-role", `role="${role}" is not an ARIA role, so it is ignored`, element);
      }
    }
  }

  for (const hidden of scope.querySelectorAll('[aria-hidden="true"]')) {
    const focusable = hidden.querySelector(
      'a[href], button, input, select, textarea, [tabindex]:not([tabindex="-1"])',
    );
    if (focusable) {
      add(
        "no-focusable-inside-aria-hidden",
        "a focusable control is inside aria-hidden, so keyboard users reach a control screen readers cannot announce",
        focusable,
      );
    }
  }

  for (const list of scope.querySelectorAll("ul, ol")) {
    for (const child of Array.from(list.children)) {
      const tag = child.tagName.toLowerCase();
      // `<script>`/`<template>` are permitted content; anything else breaks the
      // list semantics a screen reader announces ("list, 4 items").
      if (tag !== "li" && tag !== "script" && tag !== "template") {
        add(
          "list-structure",
          `a <${list.tagName.toLowerCase()}> has a <${tag}> child, which breaks the list's item count`,
          child,
        );
      }
    }
  }

  return violations;
}

/** Renders violations as a readable CI failure rather than a JSON dump. */
export function formatViolations(violations: A11yViolation[]): string {
  return violations
    .map((violation) => `  [${violation.rule}] ${violation.message}\n      at ${violation.element}`)
    .join("\n");
}

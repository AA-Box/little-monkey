/**
 * i18n key-lint (ROADMAP.md §3.8): a CI-visible check that `en.ts` — the
 * source of truth every locale is compared against, and the fallback
 * `useT()` (index.ts) reaches for at runtime — never silently drifts out of
 * sync with what the code actually references or what other locales carry.
 *
 * Two severities, matching the roadmap's own spec exactly:
 * - FAIL when a key is used somewhere (in code, via `t(...)`, or in a
 *   non-English locale dict) but missing from `en.ts` — that's either a typo
 *   or a translation that was never added to the source of truth.
 * - WARN (console only, never fails the suite) when a non-English locale
 *   simply hasn't caught up to a key `en.ts` already has — expected,
 *   ordinary drift the roadmap explicitly wants absorbed by one batch
 *   translation pass per milestone, not blocked on per-feature.
 */
import { describe, expect, it } from "vitest";
import * as fs from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";

import { en } from "./locales/en";
import { TRANSLATIONS } from "./index";
import { LOCALES, DEFAULT_LOCALE } from "./locales";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
// src/lib/i18n -> src, the root this lint scans for `t(...)` call sites.
const SRC_ROOT = path.resolve(__dirname, "../../");

const SCAN_EXTENSIONS = new Set([".ts", ".tsx"]);
// `locales/` holds the dictionaries themselves (the thing being checked
// against), not usage sites — scanning it would just mean every key
// "references itself".
const SKIP_DIRS = new Set(["node_modules", "dist", "locales"]);
// This file's own doc comments above contain example `t("Foo.bar")`/
// `` t(`Foo.bar.${x}`) `` snippets that match the scanner's own regexes —
// skip scanning the lint tool itself, not just the code it checks.
const SELF_PATH = path.resolve(__dirname, "keyLint.test.ts");

function walk(dir: string, files: string[] = []): string[] {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      if (SKIP_DIRS.has(entry.name)) continue;
      walk(full, files);
    } else if (SCAN_EXTENSIONS.has(path.extname(entry.name)) && full !== SELF_PATH) {
      files.push(full);
    }
  }
  return files;
}

// Matches `t("Foo.bar")` / `t('Foo.bar')` — a plain string-literal key,
// fully checkable against `en.ts`.
const STATIC_KEY_CALL = /\bt\(\s*(['"])([A-Za-z0-9_.]+)\1/g;
// Matches `` t(`Foo.bar.${x}`) `` — a template-literal key with a dynamic
// suffix (e.g. `t(\`McpPanel.status_${server.status}\`)`); only the literal
// prefix before the first `${` is statically checkable, and only loosely
// (does *any* en.ts key start with it) since the suffix set isn't known here.
const TEMPLATE_KEY_CALL = /\bt\(\s*`([A-Za-z0-9_.]+)\$\{/g;

function findUsedKeys(files: string[]) {
  const staticKeys = new Map<string, string[]>();
  const templatePrefixes = new Map<string, string[]>();
  for (const file of files) {
    const relative = path.relative(SRC_ROOT, file);
    const content = fs.readFileSync(file, "utf8");
    for (const match of content.matchAll(STATIC_KEY_CALL)) {
      const key = match[2];
      const sites = staticKeys.get(key) ?? [];
      sites.push(relative);
      staticKeys.set(key, sites);
    }
    for (const match of content.matchAll(TEMPLATE_KEY_CALL)) {
      const prefix = match[1];
      const sites = templatePrefixes.get(prefix) ?? [];
      sites.push(relative);
      templatePrefixes.set(prefix, sites);
    }
  }
  return { staticKeys, templatePrefixes };
}

describe("i18n key-lint", () => {
  const files = walk(SRC_ROOT);
  const { staticKeys, templatePrefixes } = findUsedKeys(files);
  const enKeys = new Set(Object.keys(en));

  it("every statically-referenced t() key exists in en.ts, the source of truth", () => {
    const missing = [...staticKeys.entries()].filter(([key]) => !enKeys.has(key));
    if (missing.length > 0) {
      const detail = missing.map(([key, sites]) => `  "${key}" used in ${sites.join(", ")}`).join("\n");
      expect.fail(`${missing.length} i18n key(s) used in code but missing from en.ts:\n${detail}`);
    }
  });

  it("every dynamic-suffix t() key family has at least one matching key in en.ts", () => {
    const missing = [...templatePrefixes.entries()].filter(
      ([prefix]) => ![...enKeys].some((k) => k.startsWith(prefix)),
    );
    if (missing.length > 0) {
      const detail = missing.map(([prefix, sites]) => `  "${prefix}*" used in ${sites.join(", ")}`).join("\n");
      expect.fail(`${missing.length} dynamic i18n key prefix(es) have no matching key in en.ts:\n${detail}`);
    }
  });

  it("no non-English locale carries a key that en.ts, the source of truth, lacks", () => {
    const problems: string[] = [];
    for (const { code } of LOCALES) {
      if (code === DEFAULT_LOCALE) continue;
      const dict = TRANSLATIONS[code];
      const extras = Object.keys(dict).filter((k) => !enKeys.has(k));
      if (extras.length > 0) problems.push(`${code}: ${extras.join(", ")}`);
    }
    if (problems.length > 0) {
      expect.fail(`Locale(s) have keys not present in en.ts (rename/typo, or en.ts fell behind):\n${problems.join("\n")}`);
    }
  });

  it("warns (without failing) when a non-English locale lags behind en.ts", () => {
    // Per ROADMAP.md §3.8: fail on keys missing from en-US, warn on locale
    // gaps — en.ts is allowed to be ahead of translations, which `useT()`'s
    // fallback (index.ts) already covers at runtime for any reader.
    for (const { code } of LOCALES) {
      if (code === DEFAULT_LOCALE) continue;
      const dictKeys = new Set(Object.keys(TRANSLATIONS[code]));
      const gap = [...enKeys].filter((k) => !dictKeys.has(k));
      if (gap.length > 0) {
        console.warn(
          `[i18n-lint] ${code} is missing ${gap.length} key(s) present in en.ts (e.g. ${gap.slice(0, 5).join(", ")}${gap.length > 5 ? ", …" : ""})`,
        );
      }
    }
    // Never fails — this test exists only to run the warn loop above under
    // `describe`/`it` so it shows up in normal test output.
    expect(true).toBe(true);
  });
});

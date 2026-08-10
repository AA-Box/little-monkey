/**
 * The i18n key-set gate (roadmap K22): every locale carries exactly the base's
 * keys, with exactly the base's placeholders.
 *
 * # What this adds over `keyLint.test.ts`
 *
 * That one FAILS when a key is *used* but missing from `en`, and only WARNS
 * when a locale has not caught up — which was the right call while the locales
 * were over a thousand keys behind and a batch translation pass was the plan.
 * It is not enforceable, and it was never enforced: the gap grew from ~650 to
 * 1,468 keys per locale while the warning printed every run.
 *
 * The gap is now closed by construction — each locale spreads `en` as its base
 * — so the property can be held rather than reported. **This test fails the
 * build.** The three things it holds:
 *
 * 1. **Key sets are identical.** A key renamed in `en` used to leave ten
 *    locales pointing at one that no longer existed, silently.
 * 2. **Placeholders match, per key.** A translation that drops a brace from
 *    `{{count}}` renders the literal text `{count}` in the UI — invisible to
 *    everyone who does not read that language, and the single most likely
 *    translation defect.
 * 3. **No value is empty.** An empty string is not a translation; it is a
 *    blank label, and it renders as one.
 *
 * # What it deliberately does not check
 *
 * That a translation is *correct*, or that it is a translation at all. Most
 * entries are still English, and `translatedCount` below reports exactly how
 * many are not — the honest number, taken from each locale's own dictionary
 * rather than inferred from "the string happens to differ from English", since
 * plenty of real translations legitimately do not ("OK", "Studio", a product
 * name).
 */
import { describe, expect, it } from "vitest";

import { en } from "./locales/en";
import { ALL_TRANSLATIONS } from "./allTranslations";
import { LOCALES, DEFAULT_LOCALE, type LocaleCode } from "./locales";
import { deTranslatedKeys } from "./locales/de";
import { es419TranslatedKeys } from "./locales/es419";
import { esESTranslatedKeys } from "./locales/esES";
import { frTranslatedKeys } from "./locales/fr";
import { hiTranslatedKeys } from "./locales/hi";
import { idTranslatedKeys } from "./locales/id";
import { itTranslatedKeys } from "./locales/it";
import { jaTranslatedKeys } from "./locales/ja";
import { koTranslatedKeys } from "./locales/ko";
import { ptTranslatedKeys } from "./locales/pt";

/** Every `{{placeholder}}` a string carries, as a set. */
function placeholdersOf(value: string): Set<string> {
  return new Set([...value.matchAll(/\{\{(\w+)\}\}/g)].map((match) => match[1]));
}

const TRANSLATED_KEYS: Partial<Record<LocaleCode, readonly string[]>> = {
  "de-DE": deTranslatedKeys,
  "es-419": es419TranslatedKeys,
  "es-ES": esESTranslatedKeys,
  "fr-FR": frTranslatedKeys,
  "hi-IN": hiTranslatedKeys,
  "id-ID": idTranslatedKeys,
  "it-IT": itTranslatedKeys,
  "ja-JP": jaTranslatedKeys,
  "ko-KR": koTranslatedKeys,
  "pt-BR": ptTranslatedKeys,
};

const baseKeys = Object.keys(en);
const others = LOCALES.map((locale) => locale.code).filter((code) => code !== DEFAULT_LOCALE);

describe("every locale carries the base's key set", () => {
  it("has a base worth comparing against", () => {
    // A base that somehow parsed to nothing would make every assertion below
    // pass vacuously, which is the one way this gate could go quietly useless.
    expect(baseKeys.length).toBeGreaterThan(1000);
    expect(others.length).toBe(10);
  });

  for (const code of others) {
    describe(code, () => {
      const dict = ALL_TRANSLATIONS[code as LocaleCode];

      it("is missing none of the base's keys", () => {
        const missing = baseKeys.filter((key) => !(key in dict));
        expect(
          missing.slice(0, 20),
          `${code} is missing ${missing.length} key(s); run the locale through the en base`,
        ).toEqual([]);
      });

      it("carries no key the base does not have", () => {
        // An extra key is a rename that only landed on one side, or a typo.
        // Either way nothing reads it and it will never be shown.
        const extra = Object.keys(dict).filter((key) => !(key in en));
        expect(extra.slice(0, 20), `${code} has ${extra.length} key(s) the base does not`).toEqual(
          [],
        );
      });

      it("keeps every placeholder the base's string carries", () => {
        const broken: string[] = [];
        for (const key of baseKeys) {
          const wanted = placeholdersOf(en[key]);
          const got = placeholdersOf(dict[key] ?? "");
          const missing = [...wanted].filter((name) => !got.has(name));
          const invented = [...got].filter((name) => !wanted.has(name));
          if (missing.length || invented.length) {
            broken.push(
              `${key}: missing {{${missing.join("}}, {{")}}}${
                invented.length ? ` invented {{${invented.join("}}, {{")}}}` : ""
              }`,
            );
          }
        }
        expect(
          broken.slice(0, 20),
          `${code} has ${broken.length} key(s) whose placeholders do not match the base — ` +
            "a dropped brace renders as literal text in the UI",
        ).toEqual([]);
      });

      it("has no empty value", () => {
        const blank = Object.entries(dict)
          .filter(([, value]) => value.trim() === "")
          .map(([key]) => key);
        expect(blank.slice(0, 20), `${code} has ${blank.length} blank value(s)`).toEqual([]);
      });
    });
  }
});

describe("the translation gap stays countable", () => {
  it("reports how much of each locale is actually translated", () => {
    // Not an assertion about the number — it is expected to be low and to rise
    // over time. It is here so that completing the key sets did not make the
    // real gap invisible, which is the failure mode this whole change risked.
    const lines = others.map((code) => {
      const translated = TRANSLATED_KEYS[code as LocaleCode]?.length ?? 0;
      const percent = ((translated / baseKeys.length) * 100).toFixed(1);
      return `  ${code}: ${translated}/${baseKeys.length} translated (${percent}%)`;
    });
    console.log(`[i18n] translation coverage against ${baseKeys.length} base keys:\n${lines.join("\n")}`);

    // Every locale must name its own translated set, or the count above is a
    // silent zero and the gap becomes unmeasurable again.
    for (const code of others) {
      expect(TRANSLATED_KEYS[code as LocaleCode], `${code} exports no translated-key list`).toBeDefined();
    }
  });
});

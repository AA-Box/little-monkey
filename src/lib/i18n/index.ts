import { useLocaleStore } from "../../store/localeStore";
import { DEFAULT_LOCALE, LOCALES, type LocaleCode } from "./locales";
import { en } from "./locales/en";
import { fr } from "./locales/fr";
import { de } from "./locales/de";
import { hi } from "./locales/hi";
import { id } from "./locales/id";
import { it } from "./locales/it";
import { ja } from "./locales/ja";
import { ko } from "./locales/ko";
import { pt } from "./locales/pt";
import { es419 } from "./locales/es419";
import { esES } from "./locales/esES";

export { LOCALES, DEFAULT_LOCALE };
export type { LocaleCode };

export const TRANSLATIONS: Record<LocaleCode, Record<string, string>> = {
  "en-US": en,
  "fr-FR": fr,
  "de-DE": de,
  "hi-IN": hi,
  "id-ID": id,
  "it-IT": it,
  "ja-JP": ja,
  "ko-KR": ko,
  "pt-BR": pt,
  "es-419": es419,
  "es-ES": esES,
};

type TranslateVars = Record<string, string | number>;

function interpolate(template: string, vars?: TranslateVars): string {
  if (!vars) return template;
  return template.replace(/\{\{(\w+)\}\}/g, (match, key: string) =>
    key in vars ? String(vars[key]) : match,
  );
}

/**
 * Translation hook: keys are `"ComponentName.thing"` namespaced per source
 * file so two files can never collide. Falls back to the English string
 * (then the raw key) for anything missing in the active locale's dict, so a
 * partial translation never renders blank.
 */
export function useT() {
  const locale = useLocaleStore((state) => state.locale);
  const dict = TRANSLATIONS[locale] ?? TRANSLATIONS[DEFAULT_LOCALE];

  function t(key: string, vars?: TranslateVars): string {
    const template = dict[key] ?? TRANSLATIONS[DEFAULT_LOCALE][key] ?? key;
    return interpolate(template, vars);
  }

  return { t, locale };
}

export default useT;

import { useEffect, useSyncExternalStore } from "react";
import { useLocaleStore } from "../../store/localeStore";
import { DEFAULT_LOCALE, LOCALES, type LocaleCode } from "./locales";
import { en } from "./locales/en";

export { LOCALES, DEFAULT_LOCALE };
export type { LocaleCode };

export const TRANSLATIONS: Partial<Record<LocaleCode, Record<string, string>>> = {
  "en-US": en,
};

const localeLoaders: Record<LocaleCode, () => Promise<Record<string, string>>> = {
  "en-US": async () => en,
  "fr-FR": () => import("./locales/fr").then(({ fr }) => fr),
  "de-DE": () => import("./locales/de").then(({ de }) => de),
  "hi-IN": () => import("./locales/hi").then(({ hi }) => hi),
  "id-ID": () => import("./locales/id").then(({ id }) => id),
  "it-IT": () => import("./locales/it").then(({ it }) => it),
  "ja-JP": () => import("./locales/ja").then(({ ja }) => ja),
  "ko-KR": () => import("./locales/ko").then(({ ko }) => ko),
  "pt-BR": () => import("./locales/pt").then(({ pt }) => pt),
  "es-419": () => import("./locales/es419").then(({ es419 }) => es419),
  "es-ES": () => import("./locales/esES").then(({ esES }) => esES),
};

const localeLoads = new Map<LocaleCode, Promise<void>>();
const listeners = new Set<() => void>();
let translationsRevision = 0;

/**
 * Loads one locale exactly once. Startup awaits this before the first render
 * and `useT` also invokes it for later in-app locale changes.
 */
export function loadLocaleTranslations(locale: LocaleCode): Promise<void> {
  if (TRANSLATIONS[locale]) return Promise.resolve();
  const inFlight = localeLoads.get(locale);
  if (inFlight) return inFlight;

  const load = localeLoaders[locale]()
    .then((dict) => {
      TRANSLATIONS[locale] = dict;
      translationsRevision += 1;
      for (const listener of listeners) listener();
    })
    .catch((error: unknown) => {
      console.error(`Failed to load translations for ${locale}:`, error);
    })
    .finally(() => {
      localeLoads.delete(locale);
    });
  localeLoads.set(locale, load);
  return load;
}

function subscribeToTranslations(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

function translationSnapshot(): number {
  return translationsRevision;
}

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
  useSyncExternalStore(subscribeToTranslations, translationSnapshot, translationSnapshot);
  useEffect(() => {
    void loadLocaleTranslations(locale);
  }, [locale]);
  const fallback = TRANSLATIONS[DEFAULT_LOCALE] ?? en;
  const dict = TRANSLATIONS[locale] ?? fallback;

  function t(key: string, vars?: TranslateVars): string {
    const template = dict[key] ?? fallback[key] ?? key;
    return interpolate(template, vars);
  }

  return { t, locale };
}

export default useT;

import { create } from "zustand";
import { DEFAULT_LOCALE, LOCALES, type LocaleCode } from "../lib/i18n/locales";

const STORAGE_KEY = "little-monkey-locale";

function isLocaleCode(value: string): value is LocaleCode {
  return LOCALES.some((entry) => entry.code === value);
}

function hydrate(): LocaleCode {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (raw && isLocaleCode(raw)) return raw;
  } catch {
    // Ignore — fall back to default.
  }
  return DEFAULT_LOCALE;
}

export interface LocaleState {
  locale: LocaleCode;
  setLocale: (locale: LocaleCode) => void;
}

export const useLocaleStore = create<LocaleState>((set) => ({
  locale: hydrate(),
  setLocale: (locale) => {
    set({ locale });
    try {
      localStorage.setItem(STORAGE_KEY, locale);
    } catch {
      // Best-effort persistence.
    }
  },
}));

export default useLocaleStore;

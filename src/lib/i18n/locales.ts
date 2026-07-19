export type LocaleCode =
  | "en-US"
  | "fr-FR"
  | "de-DE"
  | "hi-IN"
  | "id-ID"
  | "it-IT"
  | "ja-JP"
  | "ko-KR"
  | "pt-BR"
  | "es-419"
  | "es-ES";

export interface LocaleInfo {
  code: LocaleCode;
  /** Name of the language shown in its own language — never translated, same in every locale. */
  nativeName: string;
}

export const DEFAULT_LOCALE: LocaleCode = "en-US";

/** Order matches the language picker UI top-to-bottom. */
export const LOCALES: LocaleInfo[] = [
  { code: "en-US", nativeName: "English (United States)" },
  { code: "fr-FR", nativeName: "Français (France)" },
  { code: "de-DE", nativeName: "Deutsch (Deutschland)" },
  { code: "hi-IN", nativeName: "हिन्दी (भारत)" },
  { code: "id-ID", nativeName: "Indonesia (Indonesia)" },
  { code: "it-IT", nativeName: "Italiano (Italia)" },
  { code: "ja-JP", nativeName: "日本語 (日本)" },
  { code: "ko-KR", nativeName: "한국어(대한민국)" },
  { code: "pt-BR", nativeName: "Português (Brasil)" },
  { code: "es-419", nativeName: "Español (Latinoamérica)" },
  { code: "es-ES", nativeName: "Español (España)" },
];

import type { LocaleCode } from "./locales";
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

/**
 * Complete locale registry for build-time validation. Runtime code imports
 * only the active dictionary through `index.ts`; keeping this aggregate in a
 * separate module lets the i18n lint inspect every locale without putting all
 * of them in the application entry bundle.
 */
export const ALL_TRANSLATIONS: Record<LocaleCode, Record<string, string>> = {
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

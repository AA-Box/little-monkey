/**
 * Realtime Talk — English source of truth for the `AppMenu.talk` key
 * namespace. Spread into `en.ts` and, through it, into every other locale (see
 * `localeSync.test.ts`), where a real translation can override it.
 */
export const talkLocale: Record<string, string> = {
  "AppMenu.talk": "Talk",
  "AppMenu.groupTalk": "Talk",
  "ChatWindow.talkAriaLabel": "Talk",
};

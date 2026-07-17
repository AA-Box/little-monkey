/**
 * Fuzzy search core for the Global Command Palette. Deliberately pure/
 * store-free (unlike `paletteActions.ts`) so it's cheap to unit test: given
 * a list of `PaletteItem`s (built by `CommandPalette.tsx` from whatever
 * stores are already hydrated — sessions, models, recipes, prompts, MCP
 * servers, workspace files) and a query, `searchPaletteItems` returns a
 * ranked, filtered subset.
 */

export type PaletteItemKind =
  | "quickAction"
  | "approval"
  | "session"
  | "model"
  | "recipe"
  | "snippet"
  | "connector"
  | "file";

export interface PaletteItem {
  id: string;
  kind: PaletteItemKind;
  title: string;
  subtitle?: string;
  /** Extra terms matched against but never displayed — e.g. a quick
   * action's synonyms, or a file's full path when `title` is just its
   * basename. */
  keywords?: string[];
  /** True when activating this item can send captured context to a model,
   * a connector, or the filesystem, and must therefore go through the
   * palette's scope-preview confirmation step before it runs — false for
   * pure navigation (switch to a session, jump to a settings panel). See
   * `CommandPalette.tsx`. */
  sensitive: boolean;
}

/**
 * Scores `target` against `query`: an exact case-insensitive substring
 * match always outranks a scattered subsequence match, and within each tier
 * an earlier/more contiguous match scores higher. Returns `null` when
 * `query` doesn't match `target` at all (a non-empty query whose characters
 * don't all appear in order).
 */
export function fuzzyScore(query: string, target: string): number | null {
  const q = query.trim().toLowerCase();
  if (!q) return 0;
  const t = target.toLowerCase();

  const substringIndex = t.indexOf(q);
  if (substringIndex >= 0) {
    // Rewards an early, and especially a word-start, substring match.
    const wordStart = substringIndex === 0 || /\s/.test(t[substringIndex - 1]);
    return 10_000 - substringIndex + (wordStart ? 500 : 0);
  }

  let cursor = 0;
  let score = 0;
  let consecutive = 0;
  for (const char of q) {
    const found = t.indexOf(char, cursor);
    if (found === -1) return null;
    consecutive = found === cursor ? consecutive + 1 : 0;
    score += 10 + consecutive * 3 - (found - cursor);
    cursor = found + 1;
  }
  return score;
}

/** Best score for `item` across its title/subtitle/keywords, or `null` if
 * none of them match `query`. */
export function paletteItemScore(query: string, item: PaletteItem): number | null {
  const candidates = [
    { text: item.title, weight: 1 },
    { text: item.subtitle ?? "", weight: 0.6 },
    ...(item.keywords ?? []).map((keyword) => ({ text: keyword, weight: 0.4 })),
  ];
  let best: number | null = null;
  for (const { text, weight } of candidates) {
    if (!text) continue;
    const score = fuzzyScore(query, text);
    if (score === null) continue;
    const weighted = score * weight;
    if (best === null || weighted > best) best = weighted;
  }
  return best;
}

export interface PaletteSearchResult {
  item: PaletteItem;
  score: number;
}

/**
 * Ranks `items` against `query`. An empty query returns every item in its
 * original order (so callers can list quick actions first, then recents),
 * capped to `limit`. Ties keep their original relative order (a plain
 * stable sort), which is what keeps quick actions above search hits of
 * equal score when a query happens to match both equally well.
 */
export function searchPaletteItems(
  items: readonly PaletteItem[],
  query: string,
  limit = 50,
): PaletteSearchResult[] {
  const trimmed = query.trim();
  if (!trimmed) {
    return items.slice(0, limit).map((item) => ({ item, score: 0 }));
  }
  const scored: PaletteSearchResult[] = [];
  for (const item of items) {
    const score = paletteItemScore(trimmed, item);
    if (score !== null) scored.push({ item, score });
  }
  return scored
    .map((entry, index) => ({ entry, index }))
    .sort((a, b) => b.entry.score - a.entry.score || a.index - b.index)
    .slice(0, limit)
    .map(({ entry }) => entry);
}

export interface TerminalSearchLine {
  readonly length?: number;
  translateToString(trimRight?: boolean): string;
  getCell?(column: number): {
    getChars(): string;
    getWidth(): number;
  } | undefined;
}

export interface TerminalSearchBuffer {
  readonly length: number;
  getLine(row: number): TerminalSearchLine | undefined;
}

export interface TerminalSearchMatch {
  row: number;
  column: number;
  length: number;
}

const MAX_SEARCH_MATCHES = 2_000;

/**
 * Finds literal, case-insensitive matches in xterm's rendered buffer.
 *
 * Searching the emulator buffer (instead of the raw PTY transcript) avoids
 * ANSI/control-sequence false positives and gives the UI exact row/column
 * coordinates to select. Matches intentionally stay within one rendered row
 * so hard/soft wraps do not produce ambiguous selection ranges.
 */
export function findTerminalSearchMatches(
  buffer: TerminalSearchBuffer,
  query: string,
): TerminalSearchMatch[] {
  const needle = query.trim().toLocaleLowerCase();
  if (!needle) return [];

  const matches: TerminalSearchMatch[] = [];
  for (let row = 0; row < buffer.length && matches.length < MAX_SEARCH_MATCHES; row += 1) {
    const line = buffer.getLine(row);
    if (!line) continue;
    const text = line.translateToString(true);
    const searchable = text.toLocaleLowerCase();
    let from = 0;
    while (from <= searchable.length - needle.length && matches.length < MAX_SEARCH_MATCHES) {
      const column = searchable.indexOf(needle, from);
      if (column < 0) break;
      const startCell = stringIndexToCellColumn(line, column);
      const endCell = stringIndexToCellColumn(line, column + query.trim().length);
      matches.push({ row, column: startCell, length: Math.max(1, endCell - startCell) });
      from = column + Math.max(needle.length, 1);
    }
  }
  return matches;
}

function stringIndexToCellColumn(line: TerminalSearchLine, index: number): number {
  if (!line.getCell || line.length === undefined) return index;
  let stringOffset = 0;
  for (let column = 0; column < line.length; column += 1) {
    const cell = line.getCell(column);
    if (!cell || cell.getWidth() === 0) continue;
    if (stringOffset >= index) return column;
    stringOffset += cell.getChars().length || 1;
  }
  return line.length;
}

export function nextTerminalSearchIndex(
  current: number,
  count: number,
  direction: "next" | "previous",
): number {
  if (count <= 0) return -1;
  if (current < 0 || current >= count) return direction === "next" ? 0 : count - 1;
  return direction === "next"
    ? (current + 1) % count
    : (current - 1 + count) % count;
}

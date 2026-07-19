/** JSON container roots accepted from model-produced text. */
export type ModelJsonRoot = "object" | "array" | "object-or-array";

export type ModelJsonObject = Record<string, unknown>;
export type ModelJsonArray = unknown[];
export type ModelJsonContainer = ModelJsonObject | ModelJsonArray;

const MAX_MODEL_JSON_CANDIDATES = 32;

function containerMatchesRoot(value: unknown, root: ModelJsonRoot): value is ModelJsonContainer {
  if (Array.isArray(value)) return root === "array" || root === "object-or-array";
  return value !== null && typeof value === "object" && (root === "object" || root === "object-or-array");
}

/**
 * Finds the matching end of a JSON object/array without treating braces,
 * brackets, or escaped quotes inside JSON strings as structure.
 *
 * `null` means the container was truncated. Callers stop scanning at that
 * point so a valid-looking nested fragment inside a broken outer response is
 * never accepted as if it were a complete model reply.
 */
function findContainerEnd(text: string, start: number): number | null {
  const opening = text[start];
  if (opening !== "{" && opening !== "[") return null;

  const stack: ("{" | "[")[] = [opening];
  let inString = false;
  let escaped = false;

  for (let index = start + 1; index < text.length; index += 1) {
    const character = text[index];

    if (inString) {
      if (escaped) {
        escaped = false;
      } else if (character === "\\") {
        escaped = true;
      } else if (character === '"') {
        inString = false;
      }
      continue;
    }

    if (character === '"') {
      inString = true;
      continue;
    }
    if (character === "{" || character === "[") {
      stack.push(character);
      continue;
    }
    if (character !== "}" && character !== "]") continue;

    const expectedOpening = character === "}" ? "{" : "[";
    if (stack[stack.length - 1] !== expectedOpening) {
      // Return the mismatched close so this malformed span is skipped and a
      // later independent JSON container in surrounding prose can be tried.
      return index;
    }
    stack.pop();
    if (stack.length === 0) return index;
  }

  return null;
}

export function parseModelJsonCandidates(content: string, root: "object"): ModelJsonObject[];
export function parseModelJsonCandidates(content: string, root: "array"): ModelJsonArray[];
export function parseModelJsonCandidates(
  content: string,
  root?: "object-or-array",
): ModelJsonContainer[];
/**
 * Parses JSON containers from model output in deterministic preference order:
 * the entire trimmed reply first, then complete embedded containers found in
 * markdown fences or surrounding prose. Invalid spans are ignored; truncated
 * outer containers fail closed.
 *
 * Returning every valid candidate lets each feature retain its own strict
 * schema validator instead of making this transport helper understand product
 * shapes or user-facing errors.
 */
export function parseModelJsonCandidates(
  content: string,
  root: ModelJsonRoot = "object-or-array",
): ModelJsonContainer[] {
  const trimmed = content.trim();
  if (!trimmed) return [];

  const parsedCandidates: ModelJsonContainer[] = [];
  const seen = new Set<string>();

  const tryCandidate = (candidate: string) => {
    const normalized = candidate.trim();
    if (!normalized || seen.has(normalized) || parsedCandidates.length >= MAX_MODEL_JSON_CANDIDATES) return;
    seen.add(normalized);
    try {
      const parsed: unknown = JSON.parse(normalized);
      if (containerMatchesRoot(parsed, root)) parsedCandidates.push(parsed);
    } catch {
      // A later complete container in prose may still be usable.
    }
  };

  tryCandidate(trimmed);

  for (let index = 0; index < trimmed.length && parsedCandidates.length < MAX_MODEL_JSON_CANDIDATES; index += 1) {
    const character = trimmed[index];
    if (character !== "{" && character !== "[") continue;

    const end = findContainerEnd(trimmed, index);
    if (end === null) break;
    tryCandidate(trimmed.slice(index, end + 1));
    index = end;
  }

  return parsedCandidates;
}

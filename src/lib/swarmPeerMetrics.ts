/**
 * Bounded, privacy-preserving observed reliability scores for paired swarm
 * workers. Records outcomes and elapsed time only, never prompts, files,
 * agent reports or credentials. Adaptive placement can select ONLY from
 * aliases the operator explicitly supplied to a particular swarm.
 */
export interface PeerObservation {
  successes: number;
  failures: number;
  totalSuccessMs: number;
}
export type PeerObservations = Record<string, PeerObservation>;
export const PEER_METRICS_KEY = 'little-monkey-swarm-peer-metrics-v1';
const MAX_ENTRIES = 64;
const MAX_COUNT = 1_000;
const MAX_DURATION_MS = 1_800_000;
const ALIAS = /^[A-Za-z0-9][A-Za-z0-9_.-]{0,119}$/;

function normalize(value: unknown): PeerObservations {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return {};
  const results: PeerObservations = {};
  for (const [peer, candidate] of Object.entries(value).slice(0, MAX_ENTRIES)) {
    if (!ALIAS.test(peer) || !candidate || typeof candidate !== 'object') continue;
    const x = candidate as Partial<PeerObservation>;
    if (!Number.isSafeInteger(x.successes) || !Number.isSafeInteger(x.failures) ||
      !Number.isFinite(x.totalSuccessMs) || (x.successes ?? -1) < 0 ||
      (x.failures ?? -1) < 0 || (x.totalSuccessMs ?? -1) < 0 ||
      (x.successes ?? 0) > MAX_COUNT || (x.failures ?? 0) > MAX_COUNT ||
      (x.totalSuccessMs ?? 0) > MAX_DURATION_MS * MAX_COUNT) continue;
    results[peer] = {
      successes: x.successes!, failures: x.failures!, totalSuccessMs: x.totalSuccessMs!,
    };
  }
  return results;
}

export function loadPeerObservations(): PeerObservations {
  try {
    const raw = localStorage.getItem(PEER_METRICS_KEY);
    return raw ? normalize(JSON.parse(raw)) : {};
  } catch { return {}; }
}
export function recordPeerObservation(alias: string, status: 'done' | 'error' | 'cancelled', elapsedMs: number): void {
  if (!ALIAS.test(alias) || !Number.isFinite(elapsedMs)) return;
  if (status === 'cancelled') return; // Cancelling the local wait says nothing about remote capability.
  try {
    const observations = loadPeerObservations();
    const old = observations[alias] ?? { successes: 0, failures: 0, totalSuccessMs: 0 };
    // Decay at the cap rather than letting one long-lived peer dominate forever.
    const successes = old.successes >= MAX_COUNT ? Math.floor(old.successes / 2) : old.successes;
    const failures = old.failures >= MAX_COUNT ? Math.floor(old.failures / 2) : old.failures;
    const totalSuccessMs = old.successes >= MAX_COUNT ? old.totalSuccessMs / 2 : old.totalSuccessMs;
    observations[alias] = {
      successes: successes + (status === 'done' ? 1 : 0),
      failures: failures + (status === 'error' ? 1 : 0),
      totalSuccessMs: totalSuccessMs + (status === 'done' ? Math.max(0, Math.min(elapsedMs, MAX_DURATION_MS)) : 0),
    };
    const entries = Object.entries(observations).slice(-MAX_ENTRIES);
    localStorage.setItem(PEER_METRICS_KEY, JSON.stringify(Object.fromEntries(entries)));
  } catch { /* best-effort metrics never interfere with task dispatch */ }
}

export interface PeerPlacementCandidate {
  alias: string;
  grants: readonly string[];
  presence: 'online' | 'offline' | 'unknown';
}

export function chooseSwarmPeer(
  explicitlyAllowed: readonly string[],
  available: readonly PeerPlacementCandidate[],
  observations: PeerObservations,
): string | null {
  const candidates = explicitlyAllowed.flatMap((alias, ordinal) => {
    const found = available.find(candidate => candidate.alias === alias && candidate.grants.includes('task'));
    return found && found.presence !== 'offline' ? [{ ...found, ordinal }] : [];
  });
  if (!candidates.length) return null;
  candidates.sort((a, b) => {
    const availability = Number(b.presence === 'online') - Number(a.presence === 'online');
    if (availability) return availability;
    const ma = observations[a.alias], mb = observations[b.alias];
    // No adaptive ranking based on untested models. Three actual outcomes
    // are needed to override the explicit ordering of the user's pool.
    const eligibleA = !!ma && ma.successes + ma.failures >= 3;
    const eligibleB = !!mb && mb.successes + mb.failures >= 3;
    if (eligibleA && eligibleB) {
      // Laplace smoothing keeps tiny samples from claiming certainty.
      const sa = (ma.successes + 1) / (ma.successes + ma.failures + 2);
      const sb = (mb.successes + 1) / (mb.successes + mb.failures + 2);
      if (sa !== sb) return sb - sa;
      const la = ma.successes ? ma.totalSuccessMs / ma.successes : Infinity;
      const lb = mb.successes ? mb.totalSuccessMs / mb.successes : Infinity;
      if (la !== lb) return la - lb;
    }
    return a.ordinal - b.ordinal;
  });
  return candidates[0].alias;
}

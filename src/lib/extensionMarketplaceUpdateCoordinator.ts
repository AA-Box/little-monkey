import { useExtensionMarketplaceStore } from "../store/extensionMarketplaceStore";

const DEFAULT_INTERVAL_MS = 6 * 60 * 60 * 1000;
const MIN_INTERVAL_MS = 60 * 1000;

let timer: ReturnType<typeof setInterval> | null = null;
let running: Promise<void> | null = null;
let hydration: Promise<void> | null = null;

function ensureHydrated(): Promise<void> {
  if (hydration) return hydration;
  hydration = useExtensionMarketplaceStore.getState().hydrate().catch((error) => {
    hydration = null;
    throw error;
  });
  return hydration;
}

async function runOnce(): Promise<void> {
  if (running) return running;
  running = (async () => {
    // Persisted policy is an authority boundary: in particular, a saved `off`
    // policy must be loaded before any cycle is allowed to perform registry IO.
    await ensureHydrated();
    await useExtensionMarketplaceStore.getState().runUpdateCycle();
  })().finally(() => {
    running = null;
  });
  return running;
}

/**
 * Owns marketplace update policy at application lifecycle level rather than
 * coupling automatic mutation to opening Settings. Call only from the primary
 * application window. Persisted policy is hydrated before the immediate startup
 * cycle, then bounded periodic cycles continue until the returned disposer is
 * called.
 */
export function startExtensionMarketplaceUpdateCoordinator(intervalMs = DEFAULT_INTERVAL_MS): () => void {
  if (timer !== null) return () => stopExtensionMarketplaceUpdateCoordinator();
  const cadence = Math.max(MIN_INTERVAL_MS, intervalMs);
  void runOnce();
  timer = setInterval(() => void runOnce(), cadence);
  return () => stopExtensionMarketplaceUpdateCoordinator();
}

export function stopExtensionMarketplaceUpdateCoordinator(): void {
  if (timer !== null) clearInterval(timer);
  timer = null;
  // A later coordinator start must re-read persisted policy instead of assuming
  // the previous application lifecycle's hydration is still authoritative.
  hydration = null;
}

export function extensionMarketplaceUpdateInFlight(): boolean {
  return running !== null;
}

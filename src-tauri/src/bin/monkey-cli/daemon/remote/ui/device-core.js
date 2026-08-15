// The paired device's decision logic, with no browser in it.
//
// Everything here is a pure function of facts the caller collected: what the
// browser supports, what it currently permits, what the journal holds. That is
// deliberate — this is the part that decides whether a physical effect happens
// twice, and a rule that can only be exercised by opening a real camera is a
// rule nobody tests. `app.js` collects the facts and performs the effects;
// this file decides.

// The four axes, spelled the way the runner spells them. `permission` and
// `readiness` are separate because their fixes are separate: a permission is
// granted once in a settings screen, readiness is about right now.
export const PERMISSION = {
  granted: "granted",
  denied: "denied",
  promptable: "promptable",
  notRequired: "not_required",
  unsupported: "unsupported",
};

export const READINESS = {
  ready: "ready",
  foregroundRequired: "foreground_required",
  interactionRequired: "interaction_required",
  armedRequired: "armed_required",
  unavailable: "unavailable",
};

// Which Permissions API name answers for each capability, where one does. A
// capability absent from here has no OS permission at all — that is a fact
// about the platform, not an omission, and reporting one anyway is what made
// `device_info` permanently ineffective before.
export const PERMISSION_NAMES = {
  camera_capture: "camera",
  microphone_capture: "microphone",
  // A stream is the microphone, so it is the microphone's permission.
  voice_stream: "microphone",
  location_read: "geolocation",
};

// Capabilities whose result carries bytes. These are the ones that need room
// staged for them before the physical effect starts — see `capacityRefusal`.
export const ARTIFACT_CAPABILITIES = new Set([
  "camera_capture",
  "screen_capture",
  "microphone_capture",
]);

// Capabilities that need the page in front of the user to work honestly. On a
// phone a backgrounded page is suspended: the camera yields nothing and the
// microphone stops, so claiming readiness would be a lie that costs a run.
const FOREGROUND_CAPABILITIES = new Set([
  "camera_capture",
  "microphone_capture",
  "voice_stream",
  "location_read",
]);

/**
 * The permission and readiness this device reports for one capability.
 *
 * `probe` is the collected browser state:
 *   supported            — the capability is implemented by this build
 *   permissions          — Permissions API answers keyed by capability
 *   notificationPermission — `Notification.permission`, or null
 *   screenShareLive      — an armed display stream is running
 *   audioEnabled         — someone has enabled playback with a gesture
 *   foreground           — the page is visible
 */
export function describeCapability(capability, probe) {
  if (!probe.supported) {
    return { permission: PERMISSION.unsupported, readiness: READINESS.unavailable };
  }
  switch (capability) {
    case "device_info":
      // No permission exists for reading a device's own name. Saying otherwise
      // is what kept this capability advertised and never usable.
      return { permission: PERMISSION.notRequired, readiness: READINESS.ready };
    case "notification_post":
      return {
        permission: mapNotificationPermission(probe.notificationPermission),
        // A notification does not need the page in front: that is the point.
        readiness: READINESS.ready,
      };
    case "screen_capture":
      // Browser screen sharing is a per-session consent, not a stored
      // permission — the user arms it once and may stop at any moment.
      return {
        permission: PERMISSION.notRequired,
        readiness: probe.screenShareLive ? READINESS.ready : READINESS.armedRequired,
      };
    case "audio_playback":
      // No platform has a "may this page make a sound" permission. Autoplay
      // policy is a readiness state, and it clears with a user gesture.
      return {
        permission: PERMISSION.notRequired,
        readiness: probe.audioEnabled ? READINESS.ready : READINESS.interactionRequired,
      };
    default: {
      const permission = mapQueryPermission(probe.permissions?.[capability]);
      let readiness = READINESS.ready;
      if (permission === PERMISSION.denied) readiness = READINESS.unavailable;
      else if (FOREGROUND_CAPABILITIES.has(capability) && !probe.foreground) {
        readiness = READINESS.foregroundRequired;
      }
      return { permission, readiness };
    }
  }
}

// `default` is the browser's word for "not asked yet", which is promptable and
// is never permission.
function mapNotificationPermission(value) {
  if (value === "granted") return PERMISSION.granted;
  if (value === "denied") return PERMISSION.denied;
  if (value === "default") return PERMISSION.promptable;
  return PERMISSION.unsupported;
}

// A browser that cannot answer for a permission has not granted it. Reporting
// "undetermined" as anything but promptable would let an agent queue a command
// the device will then refuse in the user's face.
function mapQueryPermission(state) {
  if (state === "granted") return PERMISSION.granted;
  if (state === "denied") return PERMISSION.denied;
  return PERMISSION.promptable;
}

/**
 * Whether the runner would find this capability effective.
 *
 * The same rule `protocol::capability_block` applies, restated here so the
 * device's own screen can explain a refusal without a round trip. The runner
 * remains the authority: this never decides whether a command runs.
 */
export function isEffective({ granted, supported, permission, readiness }) {
  return Boolean(
    granted &&
      supported &&
      (permission === PERMISSION.granted || permission === PERMISSION.notRequired) &&
      readiness === READINESS.ready,
  );
}

// --- The command journal ----------------------------------------------------
//
// One entry per command this device has been handed, and the phase it reached.
// The ordering is the whole exactly-once story: nothing physical happens before
// `start_authorized` is durable, and nothing is forgotten before `result_acked`.

export const PHASE = {
  // The command exists locally. Nothing has been authorized.
  received: "received",
  // The runner authorized this execution. From here the physical effect may
  // already have happened, and this device must never perform it again.
  startAuthorized: "start_authorized",
  // The effect finished and its result — bytes and all — is durable here.
  resultStaged: "result_staged",
  // The runner acknowledged the result. Only now may the bytes be dropped.
  resultAcked: "result_acked",
  // The effect may or may not have happened and no result survives. Reported
  // as such: never repeated, never claimed to have succeeded or failed.
  uncertain: "uncertain",
};

/**
 * What to do about one command the runner still calls `running`.
 *
 * The single most important decision in the client, which is why it is a pure
 * function over the journal entry rather than control flow inside a reconnect
 * handler:
 *
 *   staged result   → deliver it. The effect happened exactly once.
 *   start authorized, nothing staged → the crash landed inside the window where
 *     the effect may have happened and no proof survives. Report it unknown.
 *     Repeating it is the one thing that must never happen here.
 *   no entry at all → same rule. A command this device cannot account for is
 *     not a command it may perform.
 *   acked already   → nothing to do.
 */
export function recoveryAction(entry) {
  if (!entry) return { action: "report_unknown", reason: "no_local_record" };
  switch (entry.phase) {
    case PHASE.resultStaged:
      return { action: "deliver_staged" };
    case PHASE.resultAcked:
      return { action: "none" };
    case PHASE.uncertain:
      return { action: "report_unknown", reason: "already_uncertain" };
    case PHASE.startAuthorized:
      return { action: "report_unknown", reason: "crashed_after_start" };
    // `received` and anything unrecognised: the runner says running but this
    // device never authorized a start under it. Still never executed.
    default:
      return { action: "report_unknown", reason: "no_start_authorized" };
  }
}

/** The terminal report for a command whose outcome cannot be proven. */
export function unknownOutcomeReport(reason) {
  return {
    outcome: "failed",
    error:
      "execution_outcome_unknown_after_restart: this device was interrupted after the runner " +
      "authorized the action, so the action may have happened. It was NOT repeated, and no " +
      `result survived to prove either way (${reason}).`,
  };
}

// --- Delivery and storage bounds -------------------------------------------

/** Bounded exponential backoff, so a runner that is down is not hammered. */
export function nextBackoffMs(attempt, { base = 1_000, ceiling = 60_000 } = {}) {
  const exponent = Math.min(Math.max(Number(attempt) || 0, 0), 16);
  return Math.min(ceiling, base * 2 ** exponent);
}

export const JOURNAL_LIMITS = {
  // Enough history to reconcile across a restart, not a photo album.
  maxEntries: 64,
  // Total bytes of staged artifacts held locally.
  maxArtifactBytes: 64 * 1024 * 1024,
  // How long an acknowledged entry is kept, purely so a duplicate lease of a
  // command the runner has already closed is answered from memory.
  ackedTtlMs: 24 * 60 * 60 * 1_000,
};

/** Whether an entry still owes the runner something. */
export function isUnacknowledged(entry) {
  return entry.phase === PHASE.resultStaged || entry.phase === PHASE.startAuthorized;
}

/**
 * Whether there is room to stage the result this command might produce.
 *
 * Checked BEFORE the physical effect, because the alternative is taking a
 * photograph and then discovering there is nowhere to put it — at which point
 * the choice is between losing the result of an effect that already happened
 * and evicting somebody else's undelivered one. Refusing up front is the only
 * answer that keeps both.
 */
export function capacityRefusal(entries, capability, ceilingBytes, limits = JOURNAL_LIMITS) {
  if (!ARTIFACT_CAPABILITIES.has(capability)) return null;
  const needed = Number(ceilingBytes) || 0;
  const held = entries
    .filter((entry) => isUnacknowledged(entry))
    .reduce((total, entry) => total + (Number(entry.artifactBytes) || 0), 0);
  if (held + needed <= limits.maxArtifactBytes) return null;
  return (
    "device_storage_full: this device is already holding undelivered results and has no room to " +
    "stage another. The action was not started. Reconnect the device so it can deliver what it " +
    "is holding, then retry."
  );
}

/**
 * Which entries may be dropped, oldest first, to stay inside the bounds.
 *
 * An unacknowledged result is never in the answer. The bound exists to stop a
 * cache growing forever; evicting a result the runner has not seen would turn a
 * storage limit into data loss about a physical effect that really happened.
 */
export function prunableEntries(entries, nowMs, limits = JOURNAL_LIMITS) {
  const acked = entries
    .filter((entry) => !isUnacknowledged(entry))
    .sort((left, right) => (left.updatedAtMs || 0) - (right.updatedAtMs || 0));
  const expired = acked.filter(
    (entry) => nowMs - (entry.updatedAtMs || 0) > limits.ackedTtlMs,
  );
  const overCount = Math.max(0, entries.length - limits.maxEntries);
  const byCount = acked.slice(0, overCount);
  const ids = new Set([...expired, ...byCount].map((entry) => entry.commandId));
  return [...ids];
}

// --- The journal itself -----------------------------------------------------
//
// The storage is injected rather than reached for. `app.js` passes an adapter
// over IndexedDB; a test passes a map. That is not an abstraction for its own
// sake — it is what makes "the bytes are not dropped before the runner
// acknowledges them" a rule with a test rather than a rule with a comment.

/**
 * The v1 → v2 upgrade: add the journal store, touch nothing else.
 *
 * Written as a function over the database so the upgrade can be exercised
 * without a browser. The controller store is deliberately not recreated,
 * cleared or migrated — an existing pairing keeps its key, its sequence and its
 * cache, and nobody has to pair a phone again to get a journal.
 */
export function journalUpgrade(database, controllerStore, journalStore) {
  const created = [];
  if (!database.objectStoreNames.contains(controllerStore)) {
    database.createObjectStore(controllerStore, { keyPath: "id" });
    created.push(controllerStore);
  }
  if (!database.objectStoreNames.contains(journalStore)) {
    database.createObjectStore(journalStore, { keyPath: "commandId" });
    created.push(journalStore);
  }
  return created;
}

/**
 * The journal's operations over an injected store.
 *
 * `adapter` supplies `get(id)`, `all()`, `put(record)` and `remove(ids)`, each
 * returning a promise. `now` is injected for the same reason: a TTL that can
 * only be tested by waiting a day is a TTL nobody tests.
 */
export function createJournal(adapter, { now = () => Date.now(), limits = JOURNAL_LIMITS } = {}) {
  return {
    get: (commandId) => adapter.get(commandId),
    all: () => adapter.all(),
    async write(entry) {
      const record = { ...entry, updatedAtMs: now() };
      await adapter.put(record);
      return record;
    },
    remove: (commandIds) => (commandIds.length ? adapter.remove(commandIds) : Promise.resolve()),
    async prune() {
      const entries = await adapter.all();
      const dropping = prunableEntries(entries, now(), limits);
      if (dropping.length) await adapter.remove(dropping);
      return dropping;
    },
  };
}

/**
 * Delivers one staged result and decides what may then be forgotten.
 *
 * The ordering is the entire point and is why this is not inline in a handler:
 *
 *   send → only on success mark acked and drop the bytes.
 *
 * A failed send leaves the entry — bytes and all — exactly as it was, and
 * raises the attempt count so the caller can back off. The one case where an
 * undelivered result is dropped is a `409`: the runner holds a different
 * authoritative terminal record, ours can never replace it, and retrying
 * forever would hold the bytes for nothing.
 */
export async function deliverStaged(entry, { send, journal }) {
  const attempts = Number(entry.deliveryAttempts) || 0;
  try {
    await send(entry);
  } catch (error) {
    if (error?.status === 409) {
      await journal.write({
        ...entry,
        phase: PHASE.resultAcked,
        artifactBlob: null,
        artifactBytes: 0,
        error: String(error.message || "The runner holds a different result for this command"),
      });
      return { outcome: "conflict", attempts };
    }
    await journal.write({ ...entry, deliveryAttempts: attempts + 1 });
    return { outcome: "retry", attempts: attempts + 1, backoffMs: nextBackoffMs(attempts) };
  }
  await journal.write({
    ...entry,
    phase: PHASE.resultAcked,
    artifactBlob: null,
    artifactBytes: 0,
    deliveryAttempts: attempts,
  });
  return { outcome: "acked", attempts };
}

/**
 * Runs `body` only if this context can be the single executor for the profile.
 *
 * `ifAvailable` rather than queueing: a second tab that waited would take over
 * the moment the first was closed *mid-command*, which is the one time it must
 * not. A tab that is not the executor says so and does nothing.
 */
export async function acquireExecutor(locks, name, body) {
  return locks.request(name, { mode: "exclusive", ifAvailable: true }, async (lock) => {
    if (!lock) return { executor: false };
    await body();
    return { executor: true };
  });
}

/**
 * Whether a leased command may be executed at all, given what this device
 * already knows about it.
 *
 * A command the journal has seen past `received` is one this device already
 * took responsibility for. Executing it again because the runner handed it over
 * a second time is exactly the failure the whole design refuses.
 */
export function leaseDecision(entry) {
  if (!entry) return { action: "execute" };
  switch (entry.phase) {
    case PHASE.received:
      return { action: "execute" };
    case PHASE.resultStaged:
      return { action: "deliver_staged" };
    case PHASE.resultAcked:
      return { action: "none" };
    default:
      return { action: "report_unknown", reason: "already_started" };
  }
}

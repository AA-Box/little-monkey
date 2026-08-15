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
 *   permissions          — Permissions API answers keyed by capability, `null`
 *                          where the browser could not answer at all
 *   sessionVerified      — permission names this controller session has itself
 *                          just obtained through a real user gesture
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
      const permission = queriedPermission(capability, probe);
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
 * The permission for a capability the Permissions API answers for — or, where
 * it cannot answer, what this session has itself verified.
 *
 * Two different kinds of evidence, and conflating them was the bug:
 *
 *   the live query      — authoritative wherever it exists. Never overridden,
 *                         in either direction: a `denied` stays denied however
 *                         many gestures preceded it.
 *   session preparation — the only evidence available on a browser that cannot
 *                         query camera, microphone or geolocation at all
 *                         (Safari has never answered for camera or microphone).
 *                         Without it those capabilities were reported
 *                         `promptable` forever and were therefore *permanently*
 *                         ineffective, whatever the user pressed.
 *
 * Session-scoped on purpose. It is set by one thing only — the real browser
 * permission operation, invoked from a real user gesture, returning
 * successfully — and it is held in memory, so a reload falls back to
 * fail-closed rather than remembering a grant nothing can re-verify.
 */
function queriedPermission(capability, probe) {
  const answer = probe.permissions?.[capability];
  if (answer === undefined || answer === null) {
    const name = PERMISSION_NAMES[capability];
    if (name && probe.sessionVerified?.[name] === true) return PERMISSION.granted;
  }
  return mapQueryPermission(answer);
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

// --- Cancellation, and what an outcome means -------------------------------
//
// Three cancellation answers, kept apart rather than collapsed into one word,
// because an operator reading a result has to be able to tell a photograph that
// never happened from one that did:
//
//   cancelled_before_effect — nothing physical occurred. Safe to retry.
//   cancelled_during_effect — it happened and was cut short. Never "not done".
//   failed                  — the capability itself went wrong.
//
// A cancellation reported as a failure is the same lie in the other direction,
// which is why speech no longer rejects on `speechSynthesis.cancel()`.

export function aborted(signal) {
  return Boolean(signal?.aborted);
}

/** Resolves when the delay elapses or the signal aborts, whichever is first. */
export function waitOrAbort(milliseconds, signal, { setTimer = setTimeout, clearTimer = clearTimeout } = {}) {
  return new Promise((resolve) => {
    const timer = setTimer(finish, milliseconds);
    function finish() {
      clearTimer(timer);
      signal?.removeEventListener?.("abort", finish);
      resolve();
    }
    signal?.addEventListener?.("abort", finish, { once: true });
  });
}

export function cancelledBeforeEffectReport(error) {
  return {
    outcome: "cancelled",
    result: { cancellation: "cancelled_before_effect" },
    error: error || "Cancelled before this device performed the action",
  };
}

/** The terminal report for a capability that carries no bytes. */
export function plainOutcome(outcome) {
  if (outcome?.cancelledBeforeEffect) return cancelledBeforeEffectReport();
  if (outcome?.cancelledDuringEffect) {
    return {
      outcome: "cancelled",
      result: { ...(outcome.result || {}), cancellation: "cancelled_during_effect" },
      error: "Stopped part-way through; what had already happened was not undone",
    };
  }
  return { outcome: "succeeded", result: outcome?.result ?? null };
}

/**
 * The terminal report for a capability that produced bytes.
 *
 * `digest` hashes the artifact once, here, and the same value is declared to
 * the runner on every later delivery — so a truncated redelivery is refused
 * rather than accepted as authoritative bytes.
 */
export async function artifactOutcome(outcome, { digest, maxBytes }) {
  if (outcome?.cancelledBeforeEffect) return cancelledBeforeEffectReport();
  const { blob, mediaType, result } = outcome || {};
  if (!blob) return { outcome: "failed", error: "The device produced no artifact" };
  if (blob.size > maxBytes) {
    return { outcome: "failed", error: "The captured artifact is larger than this device allows" };
  }
  return {
    // The effect happened. If a cancellation arrived mid-way the artifact is
    // still reported — losing it would be pretending the action did not occur.
    outcome: outcome.cancelledDuringEffect ? "cancelled" : "succeeded",
    result: outcome.cancelledDuringEffect
      ? { ...(result || {}), cancellation: "cancelled_during_effect" }
      : result,
    error: outcome.cancelledDuringEffect
      ? "Stopped part-way through; what had already been captured is attached"
      : null,
    artifactBlob: blob,
    artifactMediaType: mediaType,
    artifactSha256: await digest(blob),
  };
}

/**
 * Speaks one sentence and reports honestly how it ended.
 *
 * `speechSynthesis.cancel()` ends an utterance with an `error` event whose
 * reason is `canceled` or `interrupted`. Treating that as a synthesis failure —
 * which is what a rejected promise became — reported a *cancelled* command as
 * `failed`, so the operator who stopped it read that their device could not
 * speak. The answer is structured like every other capability's and mapped by
 * `plainOutcome`: before speech began nothing was heard, after it began
 * something was.
 */
export function speakText(text, signal, { synthesis, createUtterance }) {
  return new Promise((resolve, reject) => {
    if (aborted(signal)) {
      resolve({ cancelledBeforeEffect: true });
      return;
    }
    const utterance = createUtterance(String(text ?? ""));
    let audible = false;
    let settled = false;
    const settle = (value) => {
      if (settled) return;
      settled = true;
      signal?.removeEventListener?.("abort", stop);
      resolve(value);
    };
    const fail = (error) => {
      if (settled) return;
      settled = true;
      signal?.removeEventListener?.("abort", stop);
      reject(error);
    };
    function stop() {
      // Requested, not observed: the error event this provokes is a
      // cancellation this device asked for, and is reported as one.
      synthesis.cancel();
      settle(
        audible
          ? { cancelledDuringEffect: true, result: { spoken: false } }
          : { cancelledBeforeEffect: true },
      );
    }
    utterance.onstart = () => {
      audible = true;
    };
    utterance.onend = () => settle({ result: { spoken: true } });
    utterance.onerror = (event) => {
      const reason = event?.error;
      if (reason === "canceled" || reason === "interrupted") {
        // Somebody else's cancel — the browser's own stop control, or another
        // page's utterance. Still a cancellation, never a synthesis failure.
        settle(
          audible
            ? { cancelledDuringEffect: true, result: { spoken: false } }
            : { cancelledBeforeEffect: true },
        );
        return;
      }
      fail(new Error(`Speech synthesis failed${reason ? `: ${reason}` : ""}`));
    };
    signal?.addEventListener?.("abort", stop, { once: true });
    synthesis.speak(utterance);
  });
}

/**
 * Records the microphone for a bounded time, stopping early when cancelled.
 *
 * A cancelled recording still recorded: the honest answer keeps the audio it
 * captured and says it was cut short, rather than claiming the microphone never
 * opened. The stream is closed on every path — success, failure, cancellation —
 * because a microphone left open is the failure that matters here.
 */
export async function recordAudio(
  durationMs,
  signal,
  { openStream, createRecorder, stopStream, createBlob, now = () => Date.now(), wait = waitOrAbort, maxMs, sliceMs = 200 },
) {
  const bounded = Math.min(Math.max(Number(durationMs) || 10_000, 1), maxMs);
  if (aborted(signal)) return { cancelledBeforeEffect: true };
  const stream = await openStream();
  try {
    const recorder = createRecorder(stream);
    const chunks = [];
    recorder.ondataavailable = (event) => {
      if (event.data?.size) chunks.push(event.data);
    };
    const finished = new Promise((resolve) => {
      recorder.onstop = resolve;
    });
    recorder.start();
    const started = now();
    // Woken by the cancellation signal rather than polled: a stop asked for now
    // reaches the recorder now, not up to a slice later.
    while (now() - started < bounded && !aborted(signal)) {
      await wait(Math.min(sliceMs, bounded - (now() - started)), signal);
    }
    recorder.stop();
    await finished;
    const mediaType = recorder.mimeType || "audio/webm";
    const blob = createBlob(chunks, mediaType);
    const cancelled = aborted(signal);
    return {
      blob,
      mediaType: blob.type || mediaType,
      cancelledDuringEffect: cancelled,
      result: { duration_ms: now() - started, cancelled },
    };
  } finally {
    stopStream(stream);
  }
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

// --- Running one command ----------------------------------------------------

export const TERMINAL_COMMAND_STATES = new Set(["succeeded", "failed", "cancelled", "expired"]);

/**
 * Holds one long-poll open for as long as the command runs, and cancels the
 * work when the runner says cancellation was asked for.
 *
 * Its own `signal` — never the physical operation's. The two are stopped by
 * different things and in different directions: this one is stopped *by* the
 * work finishing, and the work is stopped by what this one hears. Sharing a
 * controller meant the only way to stop the watcher was to abort the work, so
 * the watcher could only be wound up after the effect had already happened, and
 * the caller then waited on a pending HTTP request holding the one result that
 * existed nowhere but in memory.
 */
export async function watchCommandControl(
  commandId,
  { request, waitMs, signal, onCancel = () => {}, wait = waitOrAbort, retryMs = 2_000, yieldMs = 250 },
) {
  while (!aborted(signal)) {
    let control;
    try {
      control = await request(
        "GET",
        `/v1/remote/device/commands/${encodeURIComponent(commandId)}/control?wait_ms=${waitMs}`,
        undefined,
        { signal, longPoll: true },
      );
    } catch (error) {
      if (aborted(signal)) return { reason: "stopped" };
      // The poll gave the transport up so the work itself could use it — a
      // voice stream's chunk, an artifact fetch. Nothing is wrong, so ask again
      // shortly rather than backing off; the pause is only so the work gets the
      // transport before this asks for it back.
      if (error?.cancelled === true) {
        await wait(yieldMs, signal);
        continue;
      }
      // A watcher that cannot reach the runner must not stop the work: the
      // effect may be half done and the runner has said nothing.
      await wait(retryMs, signal);
      continue;
    }
    if (control?.cancel_requested === true || control?.revoked === true) {
      onCancel(control);
      return { reason: "cancelled" };
    }
    if (control?.state && TERMINAL_COMMAND_STATES.has(control.state)) return { reason: "terminal" };
  }
  return { reason: "stopped" };
}

/**
 * One leased command, from journal entry to terminal report.
 *
 * Every ordering here is load-bearing, and this is why the whole sequence is a
 * function over injected effects rather than control flow inside a browser
 * handler: the properties it exists for are orderings, and an ordering that can
 * only be exercised by opening a real camera is an ordering nobody tests.
 *
 *   journal the command
 *   → mint and journal an execution id
 *   → ask the runner to authorize a start
 *   → only then touch hardware
 *   → **stage the result durably, with no network wait in between**
 *   → stop the control watcher
 *   → deliver.
 *
 * The staging step's position is the one that used to be wrong. The watcher was
 * wound up first, and winding it up meant awaiting a long-poll that could still
 * be pending: a photograph existed only in memory while a request finished. A
 * crash in that window lost bytes an effect had really produced, and recovery
 * could only report the outcome unknown. Nothing may be awaited between the
 * result existing and the result being durable.
 */
export async function runLeasedCommand(command, deps) {
  const {
    journal,
    request,
    perform,
    deliver,
    report,
    newExecutionId,
    artifactCeiling,
    controlWaitMs,
    now = () => Date.now(),
    notify = () => {},
    onStartFailed = () => {},
    onCancelRequested = () => {},
  } = deps;
  const commandId = command.command_id;
  const startPath = `/v1/remote/device/commands/${encodeURIComponent(commandId)}/start`;

  const existing = await journal.get(commandId);
  const decision = leaseDecision(existing);
  if (decision.action === "none") return { action: "none" };
  if (decision.action === "deliver_staged") {
    await deliver(existing);
    return { action: "deliver_staged" };
  }
  if (decision.action === "report_unknown") {
    const unknown = unknownOutcomeReport(decision.reason);
    await report(commandId, unknown, existing?.executionId ?? null);
    await journal.write({
      ...existing,
      phase: PHASE.resultAcked,
      ...unknown,
      artifactBlob: null,
      artifactBytes: 0,
    });
    return { action: "report_unknown", reason: decision.reason };
  }
  if (command.cancel_requested) {
    await report(commandId, cancelledBeforeEffectReport("Cancelled before this device started it"));
    return { action: "cancelled_before_start" };
  }
  // Room for the result BEFORE the effect. Discovering there is nowhere to put
  // a photograph after taking it leaves a choice between losing it and evicting
  // somebody else's undelivered result; refusing up front keeps both.
  const refusal = capacityRefusal(await journal.all(), command.capability, artifactCeiling);
  if (refusal) {
    await report(commandId, { outcome: "failed", error: refusal });
    return { action: "refused", reason: refusal };
  }

  const executionId = newExecutionId();
  await journal.write({
    commandId,
    capability: command.capability,
    argumentsSha256: command.arguments_sha256 || null,
    executionId,
    phase: PHASE.received,
    expiresAtMs: Number(command.expires_at_ms) || 0,
    receivedAtMs: now(),
    artifactBlob: null,
    artifactBytes: 0,
  });

  let started;
  try {
    started = await request("POST", startPath, { execution_id: executionId });
  } catch (error) {
    // Nothing was authorized, so nothing physical happened. The runner may
    // safely hand this out again once the lease lapses.
    await journal.remove([commandId]);
    onStartFailed(error);
    return { action: "start_refused", error };
  }
  if (started.started !== true) {
    // Already running (this device before a reconnect, and the runner said so).
    // Doing it again would take a second photograph.
    await journal.write({
      commandId,
      capability: command.capability,
      executionId: started.execution_id ?? executionId,
      phase: PHASE.startAuthorized,
      startedAtMs: now(),
      artifactBlob: null,
      artifactBytes: 0,
    });
    return { action: "already_running" };
  }
  // Durable before the effect. If the browser dies on the next line, recovery
  // finds this phase and reports the outcome unknown rather than repeating it.
  await journal.write({
    commandId,
    capability: command.capability,
    argumentsSha256: command.arguments_sha256 || null,
    executionId,
    phase: PHASE.startAuthorized,
    startedAtMs: now(),
    expiresAtMs: Number(command.expires_at_ms) || 0,
    artifactBlob: null,
    artifactBytes: 0,
  });

  const physical = new AbortController();
  const watcher = new AbortController();
  const watching = watchCommandControl(commandId, {
    request,
    waitMs: controlWaitMs,
    signal: watcher.signal,
    onCancel: () => {
      onCancelRequested(commandId);
      physical.abort();
    },
  });
  notify(command.capability);

  let outcome;
  try {
    outcome = await perform(command, physical.signal);
  } catch (error) {
    outcome = { outcome: "failed", error: String(error?.message || error) };
  }
  try {
    // Nothing between the effect's result and this write. Not the watcher, not
    // a request, not a render.
    await journal.write({
      commandId,
      capability: command.capability,
      executionId,
      phase: PHASE.resultStaged,
      outcome: outcome.outcome,
      result: outcome.result ?? null,
      error: outcome.error ?? null,
      // The bytes, durably. This is the difference between a reload losing a
      // photograph and a reload delivering it.
      artifactBlob: outcome.artifactBlob ?? null,
      artifactMediaType: outcome.artifactMediaType ?? null,
      artifactSha256: outcome.artifactSha256 ?? null,
      artifactBytes: outcome.artifactBlob ? outcome.artifactBlob.size : 0,
      deliveryAttempts: 0,
    });
  } finally {
    // Only now, and whatever happened above: the watcher's pending request is
    // cancelled rather than waited out.
    watcher.abort();
    await watching.catch(() => {});
  }
  await deliver(await journal.get(commandId));
  return { action: "performed", executionId };
}

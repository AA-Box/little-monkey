import {
  JOURNAL_LIMITS,
  PERMISSION,
  PERMISSION_NAMES,
  PHASE,
  READINESS,
  aborted,
  acquireExecutor,
  artifactOutcome,
  createJournal,
  deliverStaged as deliverStagedResult,
  describeCapability,
  isEffective,
  isUnacknowledged,
  journalUpgrade,
  plainOutcome,
  recordAudio,
  recoveryAction,
  runLeasedCommand,
  speakText,
  unknownOutcomeReport,
  waitOrAbort,
} from "./device-core.js";
import {
  TALK_PROTOCOL_VERSION,
  chooseTalkMediaType,
  clampTalkChannels,
  clampTalkSampleRateHz,
  createTalkDetector,
  createTalkFrames,
  normalizeTalkMediaType,
  splitTalkAudioBase64,
} from "./talkProtocol.js";

const PROTOCOL_VERSION = 1;
const DB_NAME = "little-monkey-remote-v1";
// v2 adds the durable command journal. Additive: the controller store keeps its
// name, its key path and every record in it, so an upgrade never re-pairs.
const DB_VERSION = 2;
const STORE_NAME = "controllers";
// Artifact bytes live here, not in the controller record. A profile row that
// carried multi-megabyte stills would be rewritten in full on every sequence
// allocation, which is the hottest write this client makes.
const JOURNAL_STORE = "device_command_journal";
const ACTIVE_RECORD = "active";
const MAX_INVITATION_BYTES = 256 * 1024;
// Every `RemoteAction` the runner can grant. `pause` and `control_desktop`
// were missing here while the runner already issued them, so an invitation
// granting either was rejected by this client as "unsupported" — a parity gap,
// not a policy. `web.rs` now asserts this set against the Rust enum.
const ALLOWED_ACTIONS = new Set([
  "view_runs",
  "view_events",
  "read_artifacts",
  "approve",
  "cancel",
  "pause",
  "kill",
  "control_desktop",
]);

// Every non-physical `DeviceCapability` the runner can grant a controller,
// mapped to the surface in this client that spends it.
//
// The physical capabilities below answer "what may the runner ask of this
// device"; these answer the other direction, and each one is a route the runner
// already serves. A grant with no surface here is a grant an operator can make
// and this client silently ignores — which is how `pause` and `control_desktop`
// were once accepted at pairing and then unreachable. `web.rs` asserts this
// object against the Rust enum, so a capability added there fails the build
// here rather than becoming another dead letter.
const CONTROLLER_CAPABILITIES = {
  view_runs: "the run list and every run's durable detail",
  view_events: "the replayed event timeline",
  read_artifacts: "fetching a run's artifact by id",
  approve: "deciding a digest-bound approval",
  cancel: "requesting cancellation of a run",
  pause: "pausing and resuming a run",
  kill: "the emergency stop",
  view_sessions: "reading paired conversations and their messages",
  chat: "sending a message that becomes a durable run",
  view_tasks: "listing the workflows declared on the runner",
  run_workflows: "launching one of those workflows",
  capture: "filing a note or file from this device",
  // Driven from the runner's own desktop, with local visible consent there.
  // This client is the *subject* of that session, never its operator, so it
  // has no surface of its own — and saying so here is what keeps the parity
  // check honest rather than silent.
  control_desktop: null,
  describe_node: null,
  place_runs: null,
  migrate: null,
  peer_message: null,
  peer_task_request: null,
  peer_artifact: null,
  admin: null,
};

const PAIRING_URI_SCHEME = "littlemonkey://pair/";
// How long one lease long-poll waits. Just inside the runner's 30 s lease so
// a reply is never cut off mid-flight by the server's own deadline.
const LEASE_WAIT_MS = 25_000;
// Bounds on what this client will do regardless of what it is asked, so a
// runner that has been tampered with cannot hold the camera open.
const MAX_RECORDING_MS = 300_000;
// The runner's own stream ceiling, restated here so a tampered runner cannot
// hold this microphone open past it.
const MAX_STREAM_MS = 10 * 60 * 1_000;
const MAX_ARTIFACT_BYTES = 8 * 1024 * 1024;
const TERMINAL_STATUSES = new Set([
  "succeeded",
  "failed",
  "cancelled",
  "needs_reconciliation",
]);
const encoder = new TextEncoder();

const state = {
  invitation: null,
  profile: null,
  // Last surface reported to the runner, and what it answered with — the
  // grant/advertised/OS/effective breakdown the device screen shows.
  deviceState: null,
  // The last surface this device posted — what it supports, what its OS
  // permits, and whether each capability is ready right now.
  surface: null,
  commandLoopRunning: false,
  // True while this tab holds the executor lock. Exactly one tab of a paired
  // profile performs physical commands; the others say so and do nothing.
  executor: false,
  // Autoplay policy cleared by an explicit gesture. Never assumed: a browser
  // that refuses to play a sound would otherwise be advertised as ready.
  audioEnabled: false,
  // Permission names this session has obtained itself, by running the real
  // browser permission operation from a real user gesture and having it
  // succeed. Only consulted for a permission this browser cannot query at all
  // (Safari answers for neither camera nor microphone), and deliberately in
  // memory only: a reload starts fail-closed again rather than remembering
  // consent nothing can re-verify.
  sessionVerified: {},
  pushSubscribed: false,
  // The armed display stream. Held so a screen capture needs no second consent
  // prompt; while it is null, screen capture is reported as not permitted and
  // the runner will not queue one.
  screenStream: null,
  // True when the runs on screen came from the offline cache rather than the
  // runner. Every side-effecting control is disabled while this is set.
  stale: false,
  lastSyncAtMs: null,
  runs: [],
  selectedRunId: null,
  selectedRun: null,
  events: new Map(),
  eventStartCursors: new Map(),
  approvals: [],
  // The paired conversation surface: what the runner last said, plus the
  // unsent draft, which is the one thing here that survives being offline.
  sessions: [],
  selectedSessionId: null,
  messages: new Map(),
  drafts: {},
  workflows: [],
  toastTimer: null,
  activeRequests: 0,
};

// Bounds on what this browser keeps for the train. Every one of them is a
// count rather than a byte budget because IndexedDB has no quota this page can
// read: a bound nobody can measure is not a bound.
const CACHE_LIMITS = {
  runs: 50,
  eventsPerRun: 200,
  approvalsPerRun: 20,
  sessions: 50,
  messagesPerSession: 200,
  artifactsPerRun: 50,
};

/**
 * The conversation a spoken turn belongs to: the one the operator is looking at.
 *
 * Talk used to mint `talk-<device>`, a permanent per-device session in a
 * namespace nothing else could read, so a spoken question and a typed one could
 * never land in the same thread. There is a real session model on this page —
 * a list, a selection and a draft per conversation — and voice uses it, which is
 * what makes "typed and spoken turns share a session" true rather than claimed.
 */
function mobileSessionId() {
  return state.selectedSessionId;
}

const ui = Object.fromEntries(
  [
    "pairingView",
    "pairingForm",
    "deviceName",
    "invitationFile",
    "pairingCode",
    "invitationPreview",
    "previewRunner",
    "previewExpiry",
    "previewActions",
    "previewPin",
    "pairButton",
    "dashboardView",
    "runnerIdentity",
    "refreshButton",
    "killButton",
    "forgetButton",
    "runSearch",
    "runsList",
    "runCount",
    "emptyRuns",
    "runPlaceholder",
    "runDetail",
    "detailRunId",
    "detailStatus",
    "runFacts",
    "cancelButton",
    "reloadRunButton",
    "approvalsPanel",
    "approvalCount",
    "approvalsList",
    "eventsPanel",
    "eventsButton",
    "eventsList",
    "emptyEvents",
    "artifactPanel",
    "artifactForm",
    "artifactId",
    "specJson",
    "devicePanel",
    "deviceGranted",
    "deviceSupported",
    "deviceEffective",
    "devicePermissions",
    "deviceReadiness",
    "journalStatus",
    "staleBanner",
    "pushButton",
    "pushStatus",
    "screenShareButton",
    "screenShareStatus",
    "revokeSelfButton",
    "pauseButton",
    "resumeButton",
    "chatPanel",
    "chatRefreshButton",
    "sessionSelect",
    "chatEmpty",
    "messageList",
    "chatForm",
    "chatInput",
    "chatSendButton",
    "workflowPanel",
    "workflowRefreshButton",
    "workflowList",
    "workflowEmpty",
    "capturePanel",
    "captureForm",
    "captureTitleInput",
    "captureText",
    "captureFile",
    "captureButton",
    "talkPanel",
    "talkDot",
    "talkState",
    "talkUnavailable",
    "talkButton",
    "talkInterruptButton",
    "talkMeter",
    "talkMeterFill",
    "talkTranscript",
    "talkAnswer",
    "talkError",
    "chatPanel",
    "chatEmpty",
    "chatForm",
    "chatInput",
    "chatSendButton",
    "chatRefreshButton",
    "connectionDot",
    "connectionText",
    "toast",
    "cancelDialog",
    "cancelForm",
    "cancelReason",
    "confirmCancelButton",
  ].map((id) => [id, document.getElementById(id)]),
);

class RemoteError extends Error {
  constructor(message, status = 0, { cancelled = false } = {}) {
    super(message);
    this.name = "RemoteError";
    this.status = status;
    // True only for a request this device itself cancelled — a long poll giving
    // the lock up, or a watcher whose command finished. Never a runner failure,
    // so a caller must not back off over one.
    this.cancelled = cancelled;
  }
}

function requiredFeaturesAvailable() {
  return Boolean(
    window.isSecureContext &&
      window.crypto?.subtle &&
      window.indexedDB &&
      window.navigator?.locks?.request,
  );
}

function openDatabase() {
  return new Promise((resolve, reject) => {
    const request = indexedDB.open(DB_NAME, DB_VERSION);
    // The upgrade lives in `device-core.js` so it can be exercised without a
    // browser: it adds the journal store and leaves an existing pairing's key,
    // sequence and cache exactly where they were.
    request.onupgradeneeded = () => journalUpgrade(request.result, STORE_NAME, JOURNAL_STORE);
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error || new Error("Browser storage could not be opened"));
    request.onblocked = () => reject(new Error("Browser storage upgrade is blocked by another tab"));
  });
}

async function withStore(mode, operation) {
  const database = await openDatabase();
  try {
    return await new Promise((resolve, reject) => {
      const transaction = database.transaction(STORE_NAME, mode);
      const store = transaction.objectStore(STORE_NAME);
      let result;
      let operationError;
      try {
        result = operation(store, transaction);
      } catch (error) {
        operationError = error;
        transaction.abort();
      }
      transaction.oncomplete = () => resolve(result);
      transaction.onerror = () => reject(operationError || transaction.error || new Error("Browser storage transaction failed"));
      transaction.onabort = () => reject(operationError || transaction.error || new Error("Browser storage transaction was aborted"));
    });
  } finally {
    database.close();
  }
}

async function readActiveRecord() {
  const database = await openDatabase();
  try {
    return await new Promise((resolve, reject) => {
      const transaction = database.transaction(STORE_NAME, "readonly");
      const request = transaction.objectStore(STORE_NAME).get(ACTIVE_RECORD);
      request.onsuccess = () => resolve(request.result || null);
      request.onerror = () => reject(request.error || new Error("Stored controller could not be read"));
    });
  } finally {
    database.close();
  }
}

async function saveActiveRecord(record) {
  await withStore("readwrite", (store) => {
    store.put(record);
  });
}

async function deleteActiveRecord() {
  await withStore("readwrite", (store) => {
    store.delete(ACTIVE_RECORD);
  });
}

async function allocateSequence() {
  const database = await openDatabase();
  try {
    return await new Promise((resolve, reject) => {
      const transaction = database.transaction(STORE_NAME, "readwrite");
      const store = transaction.objectStore(STORE_NAME);
      const request = store.get(ACTIVE_RECORD);
      let allocated = null;
      request.onsuccess = () => {
        const record = request.result;
        try {
          validateStoredRecord(record);
          const sequence = record.nextSequence;
          if (!Number.isSafeInteger(sequence) || sequence < 1 || sequence >= Number.MAX_SAFE_INTEGER) {
            throw new Error("The controller sequence is exhausted; rotate or re-pair this device");
          }
          record.nextSequence = sequence + 1;
          store.put(record);
          allocated = { record, sequence };
        } catch (error) {
          transaction.abort();
          reject(error);
        }
      };
      request.onerror = () => reject(request.error || new Error("Controller sequence could not be allocated"));
      transaction.oncomplete = () => resolve(allocated);
      transaction.onerror = () => reject(transaction.error || new Error("Controller sequence transaction failed"));
      transaction.onabort = () => reject(transaction.error || new Error("Controller sequence transaction was aborted"));
    });
  } finally {
    database.close();
  }
}

async function saveEventCursor(runId, cursor) {
  const database = await openDatabase();
  try {
    await new Promise((resolve, reject) => {
      const transaction = database.transaction(STORE_NAME, "readwrite");
      const store = transaction.objectStore(STORE_NAME);
      const request = store.get(ACTIVE_RECORD);
      request.onsuccess = () => {
        const record = request.result;
        try {
          validateStoredRecord(record);
          record.eventCursors ||= {};
          const existing = Number(record.eventCursors[runId] || 0);
          record.eventCursors[runId] = Math.max(existing, cursor);
          store.put(record);
        } catch (error) {
          transaction.abort();
          reject(error);
        }
      };
      request.onerror = () => reject(request.error || new Error("Event cursor could not be read"));
      transaction.oncomplete = resolve;
      transaction.onerror = () => reject(transaction.error || new Error("Event cursor could not be saved"));
      transaction.onabort = () => reject(transaction.error || new Error("Event cursor update was aborted"));
    });
  } finally {
    database.close();
  }
}

function validateStoredRecord(record) {
  if (!record || record.id !== ACTIVE_RECORD || !record.profile || !record.key) {
    throw new Error("No paired browser profile is available");
  }
  const profile = record.profile;
  if (
    profile.protocolVersion !== PROTOCOL_VERSION ||
    !validId(profile.runnerId) ||
    !validId(profile.deviceId) ||
    !Number.isSafeInteger(profile.secretGeneration) ||
    profile.secretGeneration < 1 ||
    profile.runnerOrigin !== location.origin
  ) {
    throw new Error("The stored browser profile is invalid for this runner origin");
  }
  validateScopes(profile.scopes);
  if (!profile.scopes.actions.includes("view_runs")) {
    throw new Error("The web controller requires a view_runs scope");
  }
  validateSha256(profile.certificateSha256, "Stored certificate fingerprint");
  if (record.key.type !== "secret" || record.key.extractable || !record.key.usages?.includes("sign")) {
    throw new Error("The stored device key is not a non-exportable signing key");
  }
  if (!Number.isSafeInteger(record.nextSequence) || record.nextSequence < 1) {
    throw new Error("The stored controller sequence is invalid");
  }
  if (!record.eventCursors || typeof record.eventCursors !== "object" || Array.isArray(record.eventCursors)) {
    throw new Error("The stored event cursors are invalid");
  }
  for (const [runId, cursor] of Object.entries(record.eventCursors)) {
    if (!validId(runId) || !Number.isSafeInteger(cursor) || cursor < 0) {
      throw new Error("A stored event cursor is invalid");
    }
  }
  // Absent on a record written before capabilities were stored, which is not an
  // error: `hasCapability` falls back to the legacy action mapping for exactly
  // that pairing.
  if (profile.capabilities !== undefined) {
    if (!Array.isArray(profile.capabilities) || profile.capabilities.some((value) => typeof value !== "string")) {
      throw new Error("The stored pairing capabilities are invalid");
    }
  }
  if (record.drafts !== undefined) {
    if (!record.drafts || typeof record.drafts !== "object" || Array.isArray(record.drafts)) {
      throw new Error("The stored drafts are invalid");
    }
    for (const [sessionId, text] of Object.entries(record.drafts)) {
      if (!validId(sessionId) || typeof text !== "string" || text.length > 4_000) {
        throw new Error("A stored draft is invalid");
      }
    }
  }
}

function validId(value) {
  return typeof value === "string" && value.length > 0 && value.length <= 256 && /^[A-Za-z0-9_.:-]+$/.test(value);
}

function validateSha256(value, label = "SHA-256") {
  if (typeof value !== "string" || !/^[a-fA-F0-9]{64}$/.test(value)) {
    throw new Error(`${label} is not a 64-character hexadecimal digest`);
  }
}

function validateScopes(scopes) {
  if (!scopes || typeof scopes !== "object" || Array.isArray(scopes)) {
    throw new Error("Pairing scopes are missing");
  }
  if (!Array.isArray(scopes.actions) || scopes.actions.length === 0) {
    throw new Error("Pairing must grant at least one action");
  }
  const uniqueActions = new Set(scopes.actions);
  if (uniqueActions.size !== scopes.actions.length || [...uniqueActions].some((action) => !ALLOWED_ACTIONS.has(action))) {
    throw new Error("Pairing contains an unsupported or duplicate action");
  }
  for (const field of ["run_ids", "workspace_ids"]) {
    if (!Array.isArray(scopes[field]) || scopes[field].some((value) => !validId(value))) {
      throw new Error(`Pairing ${field} are invalid`);
    }
    if (new Set(scopes[field]).size !== scopes[field].length) {
      throw new Error(`Pairing ${field} contain duplicates`);
    }
  }
  if (scopes.run_ids.length === 0 && scopes.workspace_ids.length === 0) {
    throw new Error("Pairing has no run or workspace visibility scope");
  }
  if (!Number.isSafeInteger(scopes.max_artifact_bytes) || scopes.max_artifact_bytes < 1 || scopes.max_artifact_bytes > 32 * 1024 * 1024) {
    throw new Error("Pairing artifact budget is invalid");
  }
  if (uniqueActions.has("approve") && !uniqueActions.has("view_runs")) {
    throw new Error("Approve scope requires view_runs");
  }
  if (uniqueActions.has("read_artifacts") && !uniqueActions.has("view_runs")) {
    throw new Error("Artifact scope requires view_runs");
  }
  return scopes;
}

function scopeIsSubset(value, parent) {
  const included = (child, allowed) => child.every((item) => allowed.includes(item));
  return (
    included(value.actions, parent.actions) &&
    included(value.run_ids, parent.run_ids) &&
    included(value.workspace_ids, parent.workspace_ids) &&
    value.max_artifact_bytes <= parent.max_artifact_bytes
  );
}

function hasScope(action) {
  return Boolean(state.profile?.scopes.actions.includes(action));
}

// Whether this pairing holds a capability, by the runner's own answer where one
// is available.
//
// Three sources, deliberately in this order: the device-state route is
// authoritative and is what an operator's later grant edit shows up in; the
// accept response is what the runner said at pairing and is all a device has
// while offline; and a pairing made before capabilities existed has neither, so
// its legacy actions are mapped the same way `legacy_capabilities` maps them on
// the runner. Nothing here can widen anything — a route the runner does not
// grant answers 403 whatever this returns — it only decides whether a surface
// is offered at all, and offering one that always fails is worse than hiding it.
function hasCapability(capability) {
  const granted = state.deviceState?.granted;
  if (Array.isArray(granted)) return granted.includes(capability);
  const paired = state.profile?.capabilities;
  if (Array.isArray(paired) && paired.length > 0) return paired.includes(capability);
  return hasScope(capability);
}

function randomToken(byteCount) {
  const bytes = crypto.getRandomValues(new Uint8Array(byteCount));
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary).replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/u, "");
}

function hex(bytes) {
  return [...new Uint8Array(bytes)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

async function sha256Hex(bytes) {
  return hex(await crypto.subtle.digest("SHA-256", bytes));
}

function beginRequest() {
  state.activeRequests += 1;
  setConnection("busy", "Contacting runner…");
}

function endRequest(success) {
  state.activeRequests = Math.max(0, state.activeRequests - 1);
  if (state.activeRequests === 0 && success) setConnection("online", "Paired and reachable");
}

// The long-poll currently holding the request lock, if any.
//
// Every signed request is serialized, because the runner refuses a sequence it
// has already passed and two requests in flight can arrive in either order. A
// long poll therefore holds the lock while it waits — up to 25 seconds — and
// anything else the device wants to say waits behind it. That is fine while
// nothing else is happening and wrong the moment something is: a voice stream's
// chunks, an artifact fetch, a result to deliver.
//
// So a long poll registers itself here and any ordinary request cancels it
// first. Nothing is lost: the poll is a question about state, the watcher asks
// it again straight afterwards, and cancelling a request that has already been
// counted by the runner costs one sequence number.
//
// Registered *before* the lock is asked for, and the same signal cancels the
// lock request itself: a long poll that is still queued for the lock is exactly
// as much in the way as one that holds it, and registering only once it was
// granted left a window where an ordinary request found nothing to cancel and
// then waited out the poll it had just missed.
let pendingLongPoll = null;

async function signedRequest(method, pathAndQuery, bodyValue, options = {}) {
  if (!options.longPoll) pendingLongPoll?.abort();
  const controller = new AbortController();
  const cancel = () => controller.abort();
  if (options.signal?.aborted) cancel();
  options.signal?.addEventListener?.("abort", cancel, { once: true });
  if (options.longPoll) pendingLongPoll = controller;
  try {
    return await navigator.locks.request(
      "little-monkey-remote-command-v1",
      { mode: "exclusive", signal: controller.signal },
      () => signedRequestExclusive(method, pathAndQuery, bodyValue, controller),
    );
  } catch (error) {
    // The lock request itself was cancelled while queued. Same answer as a
    // cancelled fetch: nothing was sent, nothing is inferred.
    if (controller.signal.aborted && !(error instanceof RemoteError)) {
      throw new RemoteError("This request was cancelled on the device", 0, { cancelled: true });
    }
    throw error;
  } finally {
    options.signal?.removeEventListener?.("abort", cancel);
    if (pendingLongPoll === controller) pendingLongPoll = null;
  }
}

async function signedRequestExclusive(method, pathAndQuery, bodyValue, controller) {
  if (!/^\/v1\/remote\//u.test(pathAndQuery) || /[\r\n]/u.test(pathAndQuery)) {
    throw new Error("Controller request path is outside the remote API");
  }
  const allocation = await allocateSequence();
  const { record, sequence } = allocation;
  const profile = record.profile;
  const bodyText = bodyValue === undefined ? "" : JSON.stringify(bodyValue);
  const bodyBytes = encoder.encode(bodyText);
  const timestamp = Date.now();
  const nonce = randomToken(18);
  const command = `cmd-${randomToken(18)}`;
  const canonical = [
    String(PROTOCOL_VERSION),
    method.toUpperCase(),
    pathAndQuery,
    profile.deviceId,
    String(profile.secretGeneration),
    String(sequence),
    String(timestamp),
    nonce,
    await sha256Hex(bodyBytes),
  ].join("\n");
  const signature = hex(await crypto.subtle.sign("HMAC", record.key, encoder.encode(canonical)));
  const headers = new Headers({
    "x-little-monkey-device": profile.deviceId,
    "x-little-monkey-key-generation": String(profile.secretGeneration),
    "x-little-monkey-sequence": String(sequence),
    "x-little-monkey-timestamp-ms": String(timestamp),
    "x-little-monkey-nonce": nonce,
    "x-little-monkey-command": command,
    "x-little-monkey-signature": signature,
  });
  if (bodyValue !== undefined) headers.set("content-type", "application/json");

  beginRequest();
  let lastNetworkError = null;
  let succeeded = false;
  try {
    for (let attempt = 0; attempt < 3; attempt += 1) {
      try {
        if (controller.signal.aborted) {
          throw new RemoteError("This request was cancelled on the device", 0, { cancelled: true });
        }
        const response = await fetch(pathAndQuery, {
          method,
          headers,
          body: bodyValue === undefined ? undefined : bodyText,
          cache: "no-store",
          credentials: "omit",
          redirect: "error",
          referrerPolicy: "no-referrer",
          signal: controller.signal,
        });
        const text = await response.text();
        let value;
        try {
          value = text ? JSON.parse(text) : {};
        } catch {
          throw new RemoteError("Runner returned an invalid JSON response", response.status);
        }
        if (!response.ok) {
          const message = typeof value.message === "string" ? value.message : `Runner request failed (${response.status})`;
          if (response.status === 401) setConnection("error", "Pairing rejected or revoked");
          throw new RemoteError(message, response.status);
        }
        if (value.protocol_version !== undefined && value.protocol_version !== PROTOCOL_VERSION) {
          throw new RemoteError("Runner returned an unsupported protocol version", response.status);
        }
        succeeded = true;
        return value;
      } catch (error) {
        if (error instanceof RemoteError) throw error;
        // A cancelled request is not an unreachable runner, and retrying it
        // would defeat the cancellation it was asked for.
        if (controller.signal.aborted) {
          throw new RemoteError("This request was cancelled on the device", 0, { cancelled: true });
        }
        lastNetworkError = error;
        if (attempt < 2) await delay(125 * 2 ** attempt);
      }
    }
    setConnection("error", "Runner unreachable; no action inferred");
    throw new RemoteError(
      `Runner is unreachable after replay-safe retries. No cancellation was inferred. ${lastNetworkError?.message || ""}`.trim(),
    );
  } finally {
    endRequest(succeeded);
  }
}

function delay(milliseconds) {
  return new Promise((resolve) => setTimeout(resolve, milliseconds));
}

async function parseInvitationFile(file) {
  if (!file || file.size < 2 || file.size > MAX_INVITATION_BYTES) {
    throw new Error("Invitation must be a non-empty JSON file smaller than 256 KiB");
  }
  let invitation;
  try {
    invitation = JSON.parse(await file.text());
  } catch {
    throw new Error("Invitation is not valid JSON");
  }
  await validateInvitation(invitation);
  return invitation;
}

async function validateInvitation(invitation) {
  if (!invitation || typeof invitation !== "object" || Array.isArray(invitation)) {
    throw new Error("Invitation is not an object");
  }
  if (invitation.protocol_version !== PROTOCOL_VERSION) {
    throw new Error("Invitation uses an unsupported protocol version");
  }
  for (const [field, value] of [
    ["runner_id", invitation.runner_id],
    ["pairing_id", invitation.pairing_id],
  ]) {
    if (!validId(value)) throw new Error(`Invitation ${field} is invalid`);
  }
  if (typeof invitation.pairing_token !== "string" || invitation.pairing_token.length < 32 || invitation.pairing_token.length > 512) {
    throw new Error("Invitation pairing token is invalid");
  }
  if (!Number.isSafeInteger(invitation.expires_at_ms) || invitation.expires_at_ms <= Date.now()) {
    throw new Error("Invitation has expired");
  }
  const runnerUrl = new URL(invitation.runner_url);
  if (
    runnerUrl.protocol !== "https:" ||
    runnerUrl.origin !== location.origin ||
    runnerUrl.pathname !== "/" ||
    runnerUrl.username ||
    runnerUrl.password ||
    runnerUrl.search ||
    runnerUrl.hash
  ) {
    throw new Error(`Invitation runner URL must be the credential-free HTTPS origin ${location.origin}`);
  }
  validateSha256(invitation.server_certificate_sha256, "Invitation certificate fingerprint");
  // The full invitation carries the certificate itself, and the two must agree.
  // The compact code carries only the fingerprint — see `parsePairingCode` for
  // why that is the same pin in a browser — so there are no bytes to compare.
  if (invitation.server_certificate_pem !== null && invitation.server_certificate_pem !== undefined) {
    const invitationFingerprint = await certificateFingerprint(invitation.server_certificate_pem);
    if (invitationFingerprint !== invitation.server_certificate_sha256.toLowerCase()) {
      throw new Error("Invitation certificate bytes do not match its fingerprint");
    }
  }
  // Likewise the scopes: the compact code omits them and the runner returns the
  // authoritative set in the accept response, which `acceptInvitation` validates
  // either way. A full invitation still has to be self-consistent here.
  if (invitation.scopes !== null && invitation.scopes !== undefined) {
    validateScopes(invitation.scopes);
    if (!invitation.scopes.actions.includes("view_runs")) {
      throw new Error("This web controller requires view_runs; use the native CLI for action-only pairings");
    }
  }
}

async function certificateFingerprint(pem) {
  if (typeof pem !== "string" || pem.length > 128 * 1024) {
    throw new Error("Invitation certificate is missing or too large");
  }
  const match = pem.match(/-----BEGIN CERTIFICATE-----([\s\S]*?)-----END CERTIFICATE-----/u);
  if (!match) throw new Error("Invitation certificate PEM is invalid");
  let binary;
  try {
    binary = atob(match[1].replace(/\s+/gu, ""));
  } catch {
    throw new Error("Invitation certificate PEM is not valid base64");
  }
  const der = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index += 1) der[index] = binary.charCodeAt(index);
  return sha256Hex(der);
}

async function acceptInvitation(invitation, deviceName) {
  await validateInvitation(invitation);
  const response = await fetch("/v1/remote/pairings/accept", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      protocol_version: PROTOCOL_VERSION,
      pairing_id: invitation.pairing_id,
      pairing_token: invitation.pairing_token,
      device_name: deviceName,
    }),
    cache: "no-store",
    credentials: "omit",
    redirect: "error",
    referrerPolicy: "no-referrer",
  });
  const text = await response.text();
  let accepted;
  try {
    accepted = JSON.parse(text);
  } catch {
    throw new Error("Runner returned an invalid pairing response");
  }
  if (!response.ok) {
    throw new RemoteError(typeof accepted.message === "string" ? accepted.message : "Pairing was rejected", response.status);
  }
  if (
    accepted.protocol_version !== PROTOCOL_VERSION ||
    accepted.runner_id !== invitation.runner_id ||
    !validId(accepted.device_id) ||
    !Number.isSafeInteger(accepted.secret_generation) ||
    accepted.secret_generation < 1 ||
    typeof accepted.device_secret !== "string" ||
    accepted.device_secret.length < 32 ||
    accepted.device_secret.length > 512
  ) {
    throw new Error("Runner returned an invalid pairing identity");
  }
  validateScopes(accepted.scopes);
  if (invitation.scopes && !scopeIsSubset(accepted.scopes, invitation.scopes)) {
    throw new Error("Runner attempted to expand the invitation scope");
  }
  if (!accepted.scopes.actions.includes("view_runs")) {
    throw new Error("This web controller requires view_runs; use the native CLI for action-only pairings");
  }
  const key = await crypto.subtle.importKey(
    "raw",
    encoder.encode(accepted.device_secret),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
  accepted.device_secret = "";
  const record = {
    id: ACTIVE_RECORD,
    profile: {
      protocolVersion: PROTOCOL_VERSION,
      runnerId: accepted.runner_id,
      runnerOrigin: location.origin,
      certificateSha256: invitation.server_certificate_sha256.toLowerCase(),
      deviceId: accepted.device_id,
      secretGeneration: accepted.secret_generation,
      scopes: accepted.scopes,
      // What the runner says this pairing may reach beyond the legacy run
      // actions. Kept so the chat, workflow and capture surfaces are decided
      // correctly on the first frame and while offline, rather than only after
      // the device-state route answers.
      capabilities: Array.isArray(accepted.capabilities)
        ? accepted.capabilities.filter((value) => typeof value === "string")
        : [],
    },
    key,
    nextSequence: 1,
    eventCursors: {},
    drafts: {},
  };
  validateStoredRecord(record);
  await saveActiveRecord(record);
  return record.profile;
}

function showPairing() {
  state.profile = null;
  state.runs = [];
  state.selectedRunId = null;
  state.selectedRun = null;
  state.events.clear();
  state.eventStartCursors.clear();
  state.approvals = [];
  state.sessions = [];
  state.selectedSessionId = null;
  state.messages.clear();
  state.workflows = [];
  state.drafts = {};
  ui.dashboardView.hidden = true;
  ui.pairingView.hidden = false;
  setConnection("idle", "Not paired");
}

function showDashboard(profile) {
  state.profile = profile;
  ui.pairingView.hidden = true;
  ui.dashboardView.hidden = false;
  ui.runnerIdentity.textContent = `${profile.runnerId} · ${profile.deviceId}`;
  ui.killButton.hidden = !hasScope("kill");
  ui.artifactPanel.hidden = !hasScope("read_artifacts");
  ui.eventsPanel.hidden = !hasScope("view_events");
  ui.capturePanel.hidden = !hasCapability("capture");
  renderDeviceState();
  renderSessions();
  renderWorkflows();
  setConnection("online", "Paired; checking runner…");
}

// Loads every capability surface this pairing actually holds.
//
// Failures are reported and swallowed one at a time rather than aborting the
// batch: a runner build without the workflow service answers 501 for
// workflows, and that is not a reason for the chat panel to stay empty.
async function refreshCapabilitySurfaces() {
  if (hasCapability("view_sessions")) {
    try {
      await loadSessions();
    } catch (error) {
      handleError(error, "Sessions could not be read");
    }
  }
  if (hasCapability("view_tasks")) {
    try {
      await loadWorkflows();
    } catch (error) {
      handleError(error, "Workflows could not be read");
    }
  }
  ui.capturePanel.hidden = !hasCapability("capture");
}

function setConnection(kind, message) {
  const container = ui.connectionText.parentElement;
  container.dataset.state = kind;
  ui.connectionText.textContent = message;
}

function showToast(message, kind = "info") {
  clearTimeout(state.toastTimer);
  ui.toast.textContent = message;
  ui.toast.dataset.kind = kind;
  ui.toast.hidden = false;
  state.toastTimer = setTimeout(() => {
    ui.toast.hidden = true;
  }, kind === "error" ? 8000 : 4500);
}

function setButtonBusy(button, busy, busyLabel) {
  if (!button) return;
  if (busy) {
    button.dataset.label = button.textContent;
    button.textContent = busyLabel;
    button.disabled = true;
    button.setAttribute("aria-busy", "true");
  } else {
    button.textContent = button.dataset.label || button.textContent;
    button.disabled = false;
    button.removeAttribute("aria-busy");
    delete button.dataset.label;
  }
}

async function refreshRuns({ preserveSelection = true } = {}) {
  const value = await signedRequest("GET", "/v1/remote/runs");
  if (!Array.isArray(value.runs)) throw new RemoteError("Runner returned an invalid run list");
  state.runs = value.runs.filter(isRunSummary);
  markOnline();
  await cacheRuns(state.runs);
  renderRuns();
  const retained = preserveSelection && state.runs.some((run) => run.run_id === state.selectedRunId);
  if (retained) {
    await loadSelectedRun();
  } else if (state.runs.length > 0) {
    await selectRun(state.runs[0].run_id);
  } else {
    state.selectedRunId = null;
    state.selectedRun = null;
    ui.runPlaceholder.hidden = false;
    ui.runDetail.hidden = true;
  }
}

function isRunSummary(run) {
  return Boolean(
    run &&
      validId(run.run_id) &&
      typeof run.status === "string" &&
      typeof run.kind === "string" &&
      Number.isSafeInteger(run.created_at_ms) &&
      Number.isSafeInteger(run.updated_at_ms) &&
      Number.isSafeInteger(run.last_sequence) &&
      typeof run.model_label === "string" &&
      Number.isSafeInteger(run.pending_approval_count),
  );
}

function renderRuns() {
  const search = ui.runSearch.value.trim().toLocaleLowerCase();
  const runs = state.runs.filter((run) =>
    [run.run_id, run.model_label, run.status, run.kind, run.workspace_id || ""]
      .join(" ")
      .toLocaleLowerCase()
      .includes(search),
  );
  ui.runsList.replaceChildren(...runs.map(runButton));
  ui.runCount.textContent = String(runs.length);
  ui.emptyRuns.hidden = runs.length !== 0;
}

function runButton(run) {
  const button = element("button", "run-row");
  button.type = "button";
  button.dataset.runId = run.run_id;
  button.setAttribute("aria-current", String(run.run_id === state.selectedRunId));
  button.setAttribute("aria-label", `${run.run_id}, ${humanize(run.status)}, ${run.model_label}`);

  const top = element("span", "run-row-top");
  const id = element("span", "run-row-id", run.run_id);
  const badge = element("span", "status-badge", humanize(run.status));
  badge.dataset.status = run.status;
  top.append(id, badge);

  const meta = element("span", "run-row-meta");
  meta.append(element("span", "run-row-model", run.model_label || "Unknown target"));
  if (run.pending_approval_count > 0) {
    meta.append(element("span", "approval-dot", `${run.pending_approval_count} approval${run.pending_approval_count === 1 ? "" : "s"}`));
  } else {
    meta.append(element("span", "run-row-time", relativeTime(run.updated_at_ms)));
  }
  button.append(top, meta);
  button.addEventListener("click", () => void selectRun(run.run_id));
  return button;
}

async function selectRun(runId) {
  if (!validId(runId)) return;
  state.selectedRunId = runId;
  renderRuns();
  ui.runPlaceholder.hidden = true;
  ui.runDetail.hidden = false;
  await loadSelectedRun();
  if (matchMedia("(max-width: 960px)").matches) {
    ui.runDetail.scrollIntoView({ behavior: matchMedia("(prefers-reduced-motion: reduce)").matches ? "auto" : "smooth", block: "start" });
  }
}

async function loadSelectedRun() {
  const runId = state.selectedRunId;
  if (!runId) return;
  try {
    const jobs = [loadRunDetail(runId), loadApprovals(runId)];
    if (hasScope("view_events")) jobs.push(loadEvents(runId));
    await Promise.all(jobs);
  } catch (error) {
    handleError(error, "Could not load the selected run");
  }
}

async function loadRunDetail(runId) {
  const value = await signedRequest("GET", `/v1/remote/runs/${encodeURIComponent(runId)}`);
  if (state.selectedRunId !== runId) return;
  if (!isRunSummary(value.run) || !value.spec || typeof value.spec !== "object") {
    throw new RemoteError("Runner returned invalid run details");
  }
  // `paused` is a sibling of `run`, not a field on it: the daemon's job holds
  // the flag and the run summary is the ledger's, so folding it in would mean
  // one of the two shapes lying about where the state lives.
  const paused = value.paused === true;
  await cacheRunDetail(runId, value.run, value.spec, paused);
  renderRunDetail(value.run, value.spec, paused);
}

function renderRunDetail(run, spec, paused) {
  state.selectedRun = run;
  ui.detailRunId.textContent = run.run_id;
  ui.detailStatus.textContent = humanize(run.status);
  ui.detailStatus.dataset.status = run.status;
  ui.specJson.textContent = JSON.stringify(spec ?? {}, null, 2);
  renderFacts(run);
  const terminal = TERMINAL_STATUSES.has(run.status);
  ui.cancelButton.hidden = !hasScope("cancel") || terminal;
  // Pause and resume are one grant and two buttons, because "paused" is a
  // state a controller has to be able to leave. Which of the two is offered
  // follows the run's own answer rather than a local memory of what was
  // clicked: a run resumed from the desktop must not still read as paused here.
  ui.pauseButton.hidden = !hasCapability("pause") || terminal || paused;
  ui.resumeButton.hidden = !hasCapability("pause") || terminal || !paused;
  applyStaleState();
}

function renderFacts(run) {
  const facts = [
    ["Model", run.model_label || "Unknown"],
    ["Run kind", humanize(run.kind)],
    ["Workspace", run.workspace_id || "No workspace"],
    ["Last event", `#${run.last_sequence}`],
    ["Created", formatDate(run.created_at_ms)],
    ["Updated", formatDate(run.updated_at_ms)],
    ["Pending approvals", String(run.pending_approval_count)],
    ["Visibility", scopeVisibility()],
  ];
  ui.runFacts.replaceChildren(
    ...facts.map(([name, value]) => {
      const wrapper = document.createElement("div");
      wrapper.append(element("dt", "", name), element("dd", "", value));
      return wrapper;
    }),
  );
}

function scopeVisibility() {
  const scopes = state.profile?.scopes;
  if (!scopes) return "Unknown";
  return scopes.run_ids.includes(state.selectedRunId) ? "Exact run" : "Declared workspace";
}

async function loadApprovals(runId) {
  const value = await signedRequest("GET", `/v1/remote/runs/${encodeURIComponent(runId)}/approvals`);
  if (state.selectedRunId !== runId) return;
  if (!Array.isArray(value.approvals)) throw new RemoteError("Runner returned invalid approvals");
  state.approvals = value.approvals.filter(isApproval);
  await cacheApprovals(runId, state.approvals);
  renderApprovals();
}

// Pause and resume, which are the same grant and two different requests.
//
// The runner answers 202 with a *request* recorded, not a state reached: a run
// between tool calls does not stop the instant somebody taps pause. Saying
// "requested" rather than "paused" is the difference between reporting what
// happened and reporting what was asked for.
async function setRunPaused(paused) {
  const runId = state.selectedRunId;
  if (!runId) return;
  const button = paused ? ui.pauseButton : ui.resumeButton;
  setButtonBusy(button, true, paused ? "Pausing…" : "Resuming…");
  try {
    const value = await signedRequest(
      "POST",
      `/v1/remote/runs/${encodeURIComponent(runId)}/${paused ? "pause" : "resume"}`,
      {},
    );
    showToast(
      value.status === "already_terminal"
        ? "The run had already finished."
        : paused
          ? "Pause requested. The run stops at its next safe point."
          : "Resume requested.",
    );
    await Promise.all([loadRunDetail(runId), loadEventsIfPermitted(runId)]);
  } catch (error) {
    handleError(error, paused ? "Pause failed" : "Resume failed");
  } finally {
    setButtonBusy(button, false);
  }
}

function isApproval(approval) {
  try {
    if (
      !approval ||
      !validId(approval.run_id) ||
      !validId(approval.request_id) ||
      !validId(approval.tool_call_id) ||
      !validId(approval.tool_name) ||
      !Number.isSafeInteger(approval.expires_at_ms)
    ) {
      return false;
    }
    validateSha256(approval.operation_sha256, "Approval digest");
    return true;
  } catch {
    return false;
  }
}

function renderApprovals() {
  const approvals = state.approvals;
  ui.approvalsPanel.hidden = approvals.length === 0 && !hasScope("approve");
  ui.approvalCount.textContent = String(approvals.length);
  ui.approvalsList.replaceChildren(...approvals.map(approvalCard));
  if (approvals.length === 0 && hasScope("approve")) {
    ui.approvalsList.append(element("p", "empty-state", "No digest-bound approvals are pending."));
  }
}

function approvalCard(approval) {
  const card = element("section", "approval-card");
  card.append(element("h3", "", approval.tool_name));
  card.append(
    element("p", "mono wrap", `Request ${approval.request_id}`),
    element("p", "mono wrap", `SHA-256 ${approval.operation_sha256}`),
    element("p", "", `Expires ${formatDate(approval.expires_at_ms)}`),
  );
  if (hasScope("approve")) {
    const actions = element("div", "approval-actions");
    const once = actionButton("Allow once", "secondary", (button) => decideApproval(approval, "allow_once", button));
    const run = actionButton("Allow for run", "primary", (button) => decideApproval(approval, "allow_for_run", button));
    const deny = actionButton("Deny", "danger", (button) => decideApproval(approval, "deny", button));
    if (Date.now() >= approval.expires_at_ms) {
      for (const button of [once, run, deny]) button.disabled = true;
    }
    actions.append(once, run, deny);
    card.append(actions);
  }
  return card;
}

function actionButton(label, style, action) {
  const button = element("button", `button ${style}`, label);
  button.type = "button";
  button.addEventListener("click", () => void action(button));
  return button;
}

async function decideApproval(approval, decision, button) {
  if (decision === "allow_for_run" && !confirm(`Allow matching '${approval.tool_name}' operations for the rest of this run?`)) return;
  setButtonBusy(button, true, "Sending…");
  try {
    const value = await signedRequest(
      "POST",
      `/v1/remote/runs/${encodeURIComponent(approval.run_id)}/approve`,
      {
        request_id: approval.request_id,
        operation_sha256: approval.operation_sha256,
        decision,
      },
    );
    showToast(value.status === "already_decided" ? "This approval already had the same decision." : "Approval decision recorded.");
    await Promise.all([loadApprovals(approval.run_id), loadRunDetail(approval.run_id), loadEventsIfPermitted(approval.run_id)]);
  } catch (error) {
    handleError(error, "Approval decision failed");
  } finally {
    setButtonBusy(button, false);
  }
}

async function loadEventsIfPermitted(runId) {
  if (hasScope("view_events")) await loadEvents(runId);
}

async function loadEvents(runId) {
  let cursor;
  const existing = state.events.get(runId);
  if (existing) {
    cursor = existing.length > 0 ? existing.at(-1).sequence : state.eventStartCursors.get(runId) || 0;
  } else {
    const record = await readActiveRecord();
    validateStoredRecord(record);
    cursor = Number(record.eventCursors[runId] || 0);
    state.events.set(runId, []);
    state.eventStartCursors.set(runId, cursor);
  }
  const path = `/v1/remote/runs/${encodeURIComponent(runId)}/events?after=${encodeURIComponent(String(cursor))}&limit=1000`;
  const value = await signedRequest("GET", path);
  if (!Array.isArray(value.events) || !Number.isSafeInteger(value.next_cursor)) {
    throw new RemoteError("Runner returned an invalid event page");
  }
  if (value.next_cursor < cursor) throw new RemoteError("Runner attempted to move the event cursor backwards");
  const current = state.events.get(runId) || [];
  const bySequence = new Map(current.map((event) => [event.sequence, event]));
  for (const event of value.events) {
    if (isEventEnvelope(event) && event.run_id === runId && event.sequence > cursor) {
      bySequence.set(event.sequence, event);
    }
  }
  const merged = [...bySequence.values()].sort((left, right) => left.sequence - right.sequence).slice(-1000);
  state.events.set(runId, merged);
  await saveEventCursor(runId, value.next_cursor);
  await cacheEvents(runId, merged);
  if (state.selectedRunId === runId) renderEvents();
}

function isEventEnvelope(event) {
  return Boolean(
    event &&
      validId(event.event_id) &&
      validId(event.run_id) &&
      Number.isSafeInteger(event.sequence) &&
      event.sequence > 0 &&
      Number.isSafeInteger(event.occurred_at_ms) &&
      event.event &&
      typeof event.event.type === "string",
  );
}

function renderEvents() {
  const events = state.events.get(state.selectedRunId) || [];
  ui.eventsList.replaceChildren(...events.map(eventRow));
  const startCursor = state.eventStartCursors.get(state.selectedRunId) || 0;
  ui.emptyEvents.hidden = events.length !== 0;
  ui.emptyEvents.textContent = startCursor > 0
    ? `Replay resumed after event #${startCursor}. Earlier events are not retained in this browser.`
    : "No durable events recorded.";
}

function eventRow(envelope) {
  const row = element("li", "event-row");
  const rail = element("span", "event-rail");
  rail.setAttribute("aria-hidden", "true");
  const body = document.createElement("span");
  body.append(
    element("span", "event-title", `#${envelope.sequence} · ${humanize(envelope.event.type)}`),
    element("span", "event-summary", summarizeEvent(envelope.event)),
  );
  row.append(rail, body, element("time", "event-time", formatDate(envelope.occurred_at_ms)));
  return row;
}

function summarizeEvent(event) {
  const payload = event.payload;
  if (!payload || typeof payload !== "object") return "Durable event recorded.";
  const candidate =
    payload.summary ||
    payload.message ||
    payload.reason ||
    payload.detail ||
    payload.output_excerpt ||
    payload.text ||
    payload.tool_name ||
    payload.name ||
    payload.artifact_id ||
    payload.request_id;
  if (typeof candidate === "string" && candidate.trim()) {
    return candidate.length > 220 ? `${candidate.slice(0, 217)}…` : candidate;
  }
  const keys = Object.keys(payload).slice(0, 3).map(humanize);
  return keys.length > 0 ? keys.join(" · ") : "Durable event recorded.";
}

// --- Paired conversations --------------------------------------------------
//
// The runner has served these routes since the mobile companion existed; this
// client simply never used them, so an operator could grant `chat` and watch
// nothing appear. Everything below spends a capability the runner already
// gates, and every one of these requests is refused with a 403 if the grant is
// absent — the visibility rules here decide what to *offer*, never what is
// allowed.

function isSessionSummary(session) {
  return Boolean(
    session &&
      validId(session.id) &&
      typeof session.title === "string" &&
      Number.isSafeInteger(session.updated_at_ms),
  );
}

function isChatMessage(message) {
  return Boolean(
    message &&
      validId(message.id) &&
      typeof message.role === "string" &&
      typeof message.text === "string" &&
      Number.isSafeInteger(message.created_at_ms),
  );
}

async function loadSessions() {
  const value = await signedRequest("GET", "/v1/remote/mobile/sessions");
  if (!Array.isArray(value.sessions)) throw new RemoteError("Runner returned an invalid session list");
  state.sessions = value.sessions.filter(isSessionSummary);
  await cacheSessions(state.sessions);
  if (!state.sessions.some((session) => session.id === state.selectedSessionId)) {
    state.selectedSessionId = state.sessions[0]?.id || null;
  }
  renderSessions();
  if (state.selectedSessionId) await loadMessages(state.selectedSessionId);
}

async function loadMessages(sessionId) {
  const value = await signedRequest(
    "GET",
    `/v1/remote/mobile/sessions/${encodeURIComponent(sessionId)}/messages`,
  );
  if (!Array.isArray(value.messages)) throw new RemoteError("Runner returned invalid messages");
  const messages = value.messages.filter(isChatMessage);
  state.messages.set(sessionId, messages);
  await cacheMessages(sessionId, messages);
  if (state.selectedSessionId === sessionId) renderMessages();
}

function renderSessions() {
  if (!ui.chatPanel) return;
  const visible = hasCapability("view_sessions");
  ui.chatPanel.hidden = !visible;
  if (!visible) return;
  ui.sessionSelect.replaceChildren(
    ...state.sessions.map((session) => {
      const option = document.createElement("option");
      option.value = session.id;
      option.textContent = `${session.title || session.id} · ${relativeTime(session.updated_at_ms)}`;
      option.selected = session.id === state.selectedSessionId;
      return option;
    }),
  );
  ui.sessionSelect.hidden = state.sessions.length === 0;
  ui.chatEmpty.hidden = state.sessions.length !== 0;
  ui.chatForm.hidden = !hasCapability("chat") || !state.selectedSessionId;
  if (state.selectedSessionId) {
    ui.chatInput.value = state.drafts[state.selectedSessionId] || "";
  }
  renderMessages();
}

function renderMessages() {
  const messages = state.messages.get(state.selectedSessionId) || [];
  ui.messageList.replaceChildren(...messages.map(messageRow));
}

function messageRow(message) {
  const row = element("li", "message-row");
  row.dataset.role = message.role;
  const header = element("span", "message-head", `${humanize(message.role)} · ${formatDate(message.created_at_ms)}`);
  const body = element("span", "message-body", message.text);
  row.append(header, body);
  // The runner's three states are `queued`, `accepted` and `failed`. Only the
  // first and last are worth a badge: `queued` says the durable run has not
  // finished, which is what keeps a slow answer from reading as a lost one, and
  // `failed` says it never will. `accepted` is the answer sitting right above.
  if (message.task_state === "queued" || message.task_state === "failed") {
    row.append(element("span", "message-state", humanize(message.task_state)));
  }
  return row;
}

async function sendMessage() {
  const sessionId = state.selectedSessionId;
  const text = ui.chatInput.value.trim();
  if (!sessionId || !text) return;
  setButtonBusy(ui.chatSendButton, true, "Sending…");
  try {
    await signedRequest(
      "POST",
      `/v1/remote/mobile/sessions/${encodeURIComponent(sessionId)}/messages`,
      { text },
    );
    // Cleared only after the runner accepted it. A draft dropped on a failed
    // send is a message somebody has to retype, and this is the one screen
    // where that is most likely to happen on a bad connection.
    ui.chatInput.value = "";
    await saveDraft(sessionId, "");
    await loadMessages(sessionId);
    showToast("Sent. The runner answers as a durable run under its own recipe.");
  } catch (error) {
    handleError(error, "The message was not sent");
  } finally {
    setButtonBusy(ui.chatSendButton, false);
  }
}

// --- Workflows -------------------------------------------------------------

async function loadWorkflows() {
  const value = await signedRequest("GET", "/v1/remote/mobile/workflows");
  if (!Array.isArray(value.workflows)) throw new RemoteError("Runner returned an invalid workflow list");
  state.workflows = value.workflows.filter(
    (workflow) => workflow && validId(workflow.id) && typeof workflow.name === "string",
  );
  renderWorkflows();
}

function renderWorkflows() {
  if (!ui.workflowPanel) return;
  const visible = hasCapability("view_tasks");
  ui.workflowPanel.hidden = !visible;
  if (!visible) return;
  ui.workflowEmpty.hidden = state.workflows.length !== 0;
  ui.workflowList.replaceChildren(
    ...state.workflows.map((workflow) => {
      const row = element("li", "workflow-row");
      const label = document.createElement("span");
      label.append(
        element("span", "workflow-name", workflow.name),
        element(
          "span",
          "workflow-meta",
          `${workflow.summary || ""}${workflow.last_run_at_ms ? ` · last run ${relativeTime(workflow.last_run_at_ms)}` : ""}`,
        ),
      );
      row.append(label);
      if (hasCapability("run_workflows")) {
        row.append(
          actionButton("Launch", "secondary", (button) => launchWorkflow(workflow, button)),
        );
      }
      return row;
    }),
  );
  applyStaleState();
}

async function launchWorkflow(workflow, button) {
  if (!confirm(`Launch '${workflow.name}' on the runner? It runs under the runner's own policy.`)) return;
  setButtonBusy(button, true, "Launching…");
  try {
    const value = await signedRequest(
      "POST",
      `/v1/remote/mobile/workflows/${encodeURIComponent(workflow.id)}/runs`,
      {},
    );
    showToast(`Launched as run ${value.run_id || "(pending)"}.`);
    await refreshRuns();
  } catch (error) {
    handleError(error, "The workflow was not launched");
  } finally {
    setButtonBusy(button, false);
  }
}

// --- Captures --------------------------------------------------------------
//
// The one direction in which this device hands the runner content on its own
// initiative, rather than because a command asked for it. The runner re-derives
// the digest and refuses anything whose bytes do not match what was declared,
// so the check below is a courtesy that fails fast, never the authority.

async function fileCapture(event) {
  event.preventDefault();
  const title = ui.captureTitleInput.value.trim();
  const text = ui.captureText.value.trim();
  const file = ui.captureFile.files?.[0] || null;
  if (!title) {
    showToast("A capture needs a title.", "error");
    return;
  }
  const budget = Number(state.deviceState?.max_artifact_bytes || MAX_ARTIFACT_BYTES);
  if (file && file.size > budget) {
    showToast(`That file is ${formatBytes(file.size)}; this pairing allows ${formatBytes(budget)}.`, "error");
    return;
  }
  setButtonBusy(ui.captureButton, true, "Filing…");
  try {
    const body = {
      capture_id: `cap-${randomToken(18)}`,
      kind: file ? (file.type.startsWith("image/") ? "image" : "file") : "text",
      title,
    };
    if (text) body.text = text;
    if (file) {
      const bytes = new Uint8Array(await file.arrayBuffer());
      // Through the same FileReader the camera path uses, not
      // `btoa(String.fromCharCode(...bytes))`: spreading a multi-megabyte array
      // into arguments overflows the call stack, and the artifact budget here
      // is eight megabytes.
      body.content_base64 = await blobToBase64(file);
      body.content_sha256 = await sha256Hex(bytes);
      body.size_bytes = bytes.length;
      body.mime_type = file.type || "application/octet-stream";
    }
    await signedRequest("POST", "/v1/remote/mobile/captures", body);
    ui.captureForm.reset();
    showToast("Filed on the runner.");
  } catch (error) {
    handleError(error, "The capture was not filed");
  } finally {
    setButtonBusy(ui.captureButton, false);
  }
}

async function revokeSelf() {
  if (
    !confirm(
      "Revoke this device on the runner? Its key stops working immediately, any live session it owns is force-stopped, and re-pairing needs a new invitation.",
    )
  ) {
    return;
  }
  setButtonBusy(ui.revokeSelfButton, true, "Revoking…");
  try {
    await signedRequest("DELETE", "/v1/remote/mobile/devices/self", undefined);
    await deleteActiveRecord();
    showPairing();
    showToast("This device is revoked on the runner and forgotten here.");
  } catch (error) {
    handleError(error, "The device could not revoke itself");
  } finally {
    setButtonBusy(ui.revokeSelfButton, false);
  }
}

async function cancelSelectedRun(reason) {
  const runId = state.selectedRunId;
  if (!runId) return;
  const value = await signedRequest("POST", `/v1/remote/runs/${encodeURIComponent(runId)}/cancel`, {
    reason: reason || null,
  });
  showToast(value.status === "already_terminal" ? "The run was already terminal." : "Cancellation requested. The runner will report the terminal outcome separately.");
  await Promise.all([loadRunDetail(runId), loadEventsIfPermitted(runId)]);
}

async function engageKillSwitch() {
  const confirmed = confirm("Emergency stop the runner? This engages the global kill switch and requests cancellation for every active run.");
  if (!confirmed) return;
  setButtonBusy(ui.killButton, true, "Stopping…");
  try {
    const value = await signedRequest("POST", "/v1/remote/kill", {});
    showToast(`Kill switch engaged. Cancellation requested for ${value.cancelled_runs ?? 0} run(s).`);
    await refreshRuns();
  } catch (error) {
    handleError(error, "Emergency stop failed");
  } finally {
    setButtonBusy(ui.killButton, false);
  }
}

async function fetchArtifact(event) {
  event.preventDefault();
  const runId = state.selectedRunId;
  const artifactId = ui.artifactId.value.trim();
  if (!runId || !validId(artifactId)) {
    showToast("Enter a valid artifact ID.", "error");
    return;
  }
  const button = ui.artifactForm.querySelector("button[type='submit']");
  setButtonBusy(button, true, "Verifying…");
  try {
    const value = await signedRequest(
      "GET",
      `/v1/remote/runs/${encodeURIComponent(runId)}/artifacts/${encodeURIComponent(artifactId)}`,
    );
    validateSha256(value.content_sha256, "Artifact digest");
    if (typeof value.content_base64 !== "string" || !Number.isSafeInteger(value.size_bytes) || value.size_bytes < 0) {
      throw new RemoteError("Runner returned an invalid artifact payload");
    }
    const bytes = decodeBase64(value.content_base64);
    if (bytes.byteLength !== value.size_bytes) throw new RemoteError("Artifact size does not match its ledger record");
    const digest = await sha256Hex(bytes);
    if (digest !== value.content_sha256.toLowerCase()) throw new RemoteError("Artifact failed end-to-end SHA-256 verification");
    const filename = safeFilename(value.name, artifactId);
    const blob = new Blob([bytes], { type: typeof value.media_type === "string" ? value.media_type : "application/octet-stream" });
    const url = URL.createObjectURL(blob);
    const anchor = document.createElement("a");
    anchor.href = url;
    anchor.download = filename;
    anchor.rel = "noopener";
    document.body.append(anchor);
    anchor.click();
    anchor.remove();
    setTimeout(() => URL.revokeObjectURL(url), 30_000);
    showToast(`Verified ${formatBytes(bytes.byteLength)} and saved ${filename}.`);
  } catch (error) {
    handleError(error, "Artifact could not be fetched");
  } finally {
    setButtonBusy(button, false);
  }
}

function decodeBase64(encoded) {
  let binary;
  try {
    binary = atob(encoded);
  } catch {
    throw new RemoteError("Artifact content is not valid base64");
  }
  const output = new Uint8Array(binary.length);
  for (let offset = 0; offset < binary.length; offset += 65_536) {
    const end = Math.min(binary.length, offset + 65_536);
    for (let index = offset; index < end; index += 1) output[index] = binary.charCodeAt(index);
  }
  return output;
}

function safeFilename(name, fallback) {
  if (typeof name !== "string") return fallback;
  const cleaned = name.replace(/[\u0000-\u001f/\\:*?"<>|]/gu, "_").replace(/^\.+/u, "").trim().slice(0, 180);
  return cleaned || fallback;
}

function element(tag, className = "", text = "") {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== "") node.textContent = String(text);
  return node;
}

function humanize(value) {
  if (typeof value !== "string" || value.length === 0) return "Unknown";
  return value
    .replaceAll("_", " ")
    .replace(/([a-z])([A-Z])/gu, "$1 $2")
    .replace(/^./u, (letter) => letter.toUpperCase());
}

function formatDate(milliseconds) {
  const date = new Date(milliseconds);
  return Number.isNaN(date.valueOf()) ? "Unknown" : new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" }).format(date);
}

function relativeTime(milliseconds) {
  const delta = milliseconds - Date.now();
  const absolute = Math.abs(delta);
  const formatter = new Intl.RelativeTimeFormat(undefined, { numeric: "auto" });
  if (absolute < 60_000) return formatter.format(Math.round(delta / 1_000), "second");
  if (absolute < 3_600_000) return formatter.format(Math.round(delta / 60_000), "minute");
  if (absolute < 86_400_000) return formatter.format(Math.round(delta / 3_600_000), "hour");
  return formatter.format(Math.round(delta / 86_400_000), "day");
}

function formatBytes(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 ** 2) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${(bytes / 1024 ** 2).toFixed(1)} MiB`;
}

function handleError(error, prefix = "Request failed") {
  const message = error instanceof Error ? error.message : String(error);
  showToast(`${prefix}: ${message}`, "error");
  if (!(error instanceof RemoteError) || error.status !== 401) {
    if (state.profile && state.activeRequests === 0) setConnection("error", "Last request failed; no action inferred");
  }
}

function bindEvents() {
  ui.invitationFile.addEventListener("change", async () => {
    state.invitation = null;
    ui.invitationPreview.hidden = true;
    const file = ui.invitationFile.files?.[0];
    if (!file) return;
    try {
      const invitation = await parseInvitationFile(file);
      state.invitation = invitation;
      ui.previewRunner.textContent = invitation.runner_id;
      ui.previewExpiry.textContent = formatDate(invitation.expires_at_ms);
      ui.previewActions.textContent = invitation.scopes.actions.map(humanize).join(", ");
      ui.previewPin.textContent = invitation.server_certificate_sha256;
      ui.invitationPreview.hidden = false;
    } catch (error) {
      ui.invitationFile.value = "";
      handleError(error, "Invitation rejected");
    }
  });

  ui.pairingCode?.addEventListener("input", async () => {
    const value = ui.pairingCode.value.trim();
    state.invitation = null;
    ui.invitationPreview.hidden = true;
    if (!value) return;
    try {
      const invitation = parsePairingCode(value);
      await validateInvitation(invitation);
      state.invitation = invitation;
      ui.previewRunner.textContent = invitation.runner_id;
      ui.previewExpiry.textContent = formatDate(invitation.expires_at_ms);
      ui.previewActions.textContent = "Confirmed by the runner when pairing completes";
      ui.previewPin.textContent = invitation.server_certificate_sha256;
      ui.invitationPreview.hidden = false;
    } catch (error) {
      handleError(error, "Pairing code rejected");
    }
  });

  ui.pairingForm.addEventListener("submit", async (event) => {
    event.preventDefault();
    if (!state.invitation) {
      showToast("Choose and verify a pairing invitation first.", "error");
      return;
    }
    const name = ui.deviceName.value.trim();
    if (name.length < 1 || name.length > 80) {
      showToast("Device name must be between 1 and 80 characters.", "error");
      return;
    }
    setButtonBusy(ui.pairButton, true, "Pairing…");
    try {
      const profile = await acceptInvitation(state.invitation, name);
      state.invitation = null;
      ui.invitationFile.value = "";
      ui.invitationPreview.hidden = true;
      showDashboard(profile);
      showToast("Browser paired. The device key is non-exportable and scoped by the invitation.");
      await refreshRuns({ preserveSelection: false });
      await advertiseDevice();
      await refreshCapabilitySurfaces();
      void runCommandLoop();
    } catch (error) {
      handleError(error, "Pairing failed");
    } finally {
      setButtonBusy(ui.pairButton, false);
    }
  });

  ui.runSearch.addEventListener("input", renderRuns);
  ui.refreshButton.addEventListener("click", async () => {
    setButtonBusy(ui.refreshButton, true, "Refreshing…");
    try {
      await refreshRuns();
      await refreshCapabilitySurfaces();
      showToast("Runner state refreshed.");
    } catch (error) {
      handleError(error, "Refresh failed");
    } finally {
      setButtonBusy(ui.refreshButton, false);
    }
  });
  ui.reloadRunButton.addEventListener("click", () => void loadSelectedRun());
  ui.eventsButton.addEventListener("click", async () => {
    if (!state.selectedRunId) return;
    setButtonBusy(ui.eventsButton, true, "Checking…");
    try {
      await loadEvents(state.selectedRunId);
      showToast("Event cursor is up to date.");
    } catch (error) {
      handleError(error, "Event refresh failed");
    } finally {
      setButtonBusy(ui.eventsButton, false);
    }
  });
  ui.cancelButton.addEventListener("click", () => {
    ui.cancelReason.value = "";
    ui.cancelDialog.showModal();
  });
  ui.cancelForm.addEventListener("submit", async (event) => {
    event.preventDefault();
    const action = event.submitter?.value;
    if (action !== "confirm") {
      ui.cancelDialog.close();
      return;
    }
    setButtonBusy(ui.confirmCancelButton, true, "Requesting…");
    try {
      await cancelSelectedRun(ui.cancelReason.value.trim());
      ui.cancelDialog.close();
    } catch (error) {
      handleError(error, "Cancellation failed");
    } finally {
      setButtonBusy(ui.confirmCancelButton, false);
    }
  });
  ui.pushButton?.addEventListener("click", async () => {
    setButtonBusy(ui.pushButton, true, "Working…");
    try {
      if (state.pushSubscribed) {
        await unsubscribeFromPush();
        showToast("This device will no longer be woken.");
      } else {
        const endpoint = await subscribeToPush();
        showToast(
          endpoint
            ? "Notifications on. The runner encrypts each one to this device."
            : "This runner does not send notifications.",
        );
      }
    } catch (error) {
      handleError(error, "Notification setting could not be changed");
    } finally {
      setButtonBusy(ui.pushButton, false);
      await refreshPushState();
    }
  });

  ui.screenShareButton?.addEventListener("click", async () => {
    setButtonBusy(ui.screenShareButton, true, "Working…");
    try {
      if (screenShareIsLive()) {
        disarmScreenShare();
        showToast("Screen capture is off. The runner can no longer capture this screen.");
      } else {
        await armScreenShare();
        showToast("Screen capture is on until you stop sharing.");
      }
      // The runner's view of what is permitted has just changed, and it is the
      // runner that decides whether a capture is effective.
      await advertiseDevice();
    } catch (error) {
      handleError(error, "Screen sharing could not be changed");
    } finally {
      setButtonBusy(ui.screenShareButton, false);
      renderScreenShare();
    }
  });

  ui.pauseButton?.addEventListener("click", () => void setRunPaused(true));
  ui.resumeButton?.addEventListener("click", () => void setRunPaused(false));
  ui.revokeSelfButton?.addEventListener("click", () => void revokeSelf());

  ui.sessionSelect?.addEventListener("change", async () => {
    state.selectedSessionId = ui.sessionSelect.value || null;
    ui.chatInput.value = state.drafts[state.selectedSessionId] || "";
    renderMessages();
    if (state.selectedSessionId && !state.stale) {
      try {
        await loadMessages(state.selectedSessionId);
      } catch (error) {
        handleError(error, "Messages could not be read");
      }
    }
  });
  // Debounced by nothing on purpose: an IndexedDB put per keystroke is cheap,
  // and a debounce is exactly what loses the last few characters when a phone
  // is locked mid-sentence.
  ui.chatInput?.addEventListener("input", () => {
    if (state.selectedSessionId) void saveDraft(state.selectedSessionId, ui.chatInput.value);
  });
  ui.chatForm?.addEventListener("submit", (event) => {
    event.preventDefault();
    void sendMessage();
  });
  ui.chatRefreshButton?.addEventListener("click", async () => {
    setButtonBusy(ui.chatRefreshButton, true, "Checking…");
    try {
      await loadSessions();
    } catch (error) {
      handleError(error, "Sessions could not be read");
    } finally {
      setButtonBusy(ui.chatRefreshButton, false);
    }
  });
  ui.workflowRefreshButton?.addEventListener("click", async () => {
    setButtonBusy(ui.workflowRefreshButton, true, "Checking…");
    try {
      await loadWorkflows();
    } catch (error) {
      handleError(error, "Workflows could not be read");
    } finally {
      setButtonBusy(ui.workflowRefreshButton, false);
    }
  });
  ui.captureForm?.addEventListener("submit", (event) => void fileCapture(event));

  ui.talkButton?.addEventListener("click", () => {
    if (talk.running) void stopTalk();
    else void startTalk();
  });
  ui.talkInterruptButton?.addEventListener("click", () => talkInterrupt("stop_button"));
  ui.killButton.addEventListener("click", () => void engageKillSwitch());
  ui.artifactForm.addEventListener("submit", fetchArtifact);
  ui.forgetButton.addEventListener("click", async () => {
    if (!confirm("Forget this browser profile? This removes the local device key but does not revoke the paired device on the runner.")) return;
    try {
      await deleteActiveRecord();
      showPairing();
      showToast("Browser profile removed. Revoke the old device on the runner if it is no longer trusted.");
    } catch (error) {
      handleError(error, "Browser profile could not be removed");
    }
  });
}

// --- The compact pairing code ---------------------------------------------

// Parses `littlemonkey://pair/<base64url>` into the same shape the JSON
// invitation has, minus the certificate PEM.
//
// Dropping the PEM does not weaken anything *in a browser*: this page is served
// by the runner over the very connection being pinned, so the browser has
// already validated that certificate before a line of this script ran, and the
// origin check below is what actually binds the pairing to it. The fingerprint
// is still carried and stored, because native clients pin with it directly.
function parsePairingCode(value) {
  const trimmed = String(value || "").trim();
  if (!trimmed.startsWith(PAIRING_URI_SCHEME)) {
    throw new Error("That is not a Little Monkey pairing code");
  }
  const encoded = trimmed.slice(PAIRING_URI_SCHEME.length);
  if (encoded.length === 0 || encoded.length > 8 * 1024) {
    throw new Error("Pairing code is empty or too large");
  }
  let json;
  try {
    const padded = encoded.replaceAll("-", "+").replaceAll("_", "/");
    json = atob(padded + "=".repeat((4 - (padded.length % 4)) % 4));
  } catch {
    throw new Error("Pairing code is not valid base64");
  }
  let compact;
  try {
    compact = JSON.parse(json);
  } catch {
    throw new Error("Pairing code does not contain a pairing invitation");
  }
  return {
    protocol_version: compact.v,
    runner_id: compact.r,
    runner_url: compact.u,
    pairing_id: compact.p,
    pairing_token: compact.t,
    server_certificate_sha256: compact.f,
    expires_at_ms: compact.e,
    // No PEM: this is the whole point of the compact form. `validateInvitation`
    // skips the bytes-match check when it is absent and keeps every other one.
    server_certificate_pem: null,
    // The compact code carries no scopes; the runner returns the authoritative
    // ones in the accept response, and `acceptInvitation` validates those.
    scopes: null,
  };
}

// --- What this device is --------------------------------------------------
//
// Four separate questions, kept separate: what this build supports, what the OS
// permits, whether it could act right now, and — the runner's answer, never
// this client's — whether all of that adds up to something effective.

// Physical capabilities this build can actually perform, mapped to the browser
// feature that performs them. Advertised to the runner as "supported"; the
// runner intersects that with the operator's grant, the OS permission and the
// readiness reported beside it.
const DEVICE_CAPABILITIES = {
  device_info: () => true,
  camera_capture: () => Boolean(navigator.mediaDevices?.getUserMedia),
  microphone_capture: () => Boolean(navigator.mediaDevices?.getUserMedia && window.MediaRecorder),
  location_read: () => Boolean(navigator.geolocation),
  notification_post: () => "Notification" in window,
  screen_capture: () => Boolean(navigator.mediaDevices?.getDisplayMedia),
  // Either half is enough to be useful: an artifact is played back through the
  // audio element, and `text` is spoken by the synthesizer.
  audio_playback: () => Boolean(window.speechSynthesis) || typeof Audio === "function",
  voice_stream: () => Boolean(navigator.mediaDevices?.getUserMedia && window.MediaRecorder),
};

// What each capability's preparation control says and does. Only a direct user
// gesture ever reaches these — an agent asking for a camera must never cause a
// permission prompt to appear in somebody's face.
const PREPARATION = {
  camera_capture: { label: "Allow camera", prepare: () => promptForMedia({ video: true }) },
  microphone_capture: { label: "Allow microphone", prepare: () => promptForMedia({ audio: true }) },
  voice_stream: { label: "Allow microphone", prepare: () => promptForMedia({ audio: true }) },
  location_read: { label: "Allow location", prepare: promptForLocation },
  notification_post: { label: "Allow notifications", prepare: promptForNotifications },
  screen_capture: { label: "Allow screen capture", prepare: armScreenShare },
  audio_playback: { label: "Enable audio playback", prepare: enableAudioPlayback },
};

function capabilitySupported(capability) {
  try {
    return Boolean(DEVICE_CAPABILITIES[capability]?.());
  } catch {
    return false;
  }
}

// Everything the browser will tell us right now, collected in one place so the
// decision itself stays a pure function of it.
async function collectProbe() {
  const permissions = {};
  for (const [capability, name] of Object.entries(PERMISSION_NAMES)) {
    permissions[capability] = await queryPermission(name);
  }
  return {
    permissions,
    sessionVerified: state.sessionVerified,
    notificationPermission: "Notification" in window ? Notification.permission : null,
    screenShareLive: screenShareIsLive(),
    audioEnabled: state.audioEnabled === true,
    foreground: document.visibilityState === "visible",
  };
}

async function queryPermission(name) {
  if (!navigator.permissions?.query) return null;
  try {
    const status = await navigator.permissions.query({ name });
    return status.state;
  } catch {
    // A browser that does not know this permission name cannot answer for it,
    // and "cannot answer" is not "granted".
    return null;
  }
}

async function describeDevice() {
  const probe = await collectProbe();
  const capabilities = [];
  const permissions = {};
  const readiness = {};
  for (const capability of Object.keys(DEVICE_CAPABILITIES)) {
    const supported = capabilitySupported(capability);
    const answer = describeCapability(capability, { ...probe, supported });
    if (supported) capabilities.push(capability);
    permissions[capability] = answer.permission;
    readiness[capability] = answer.readiness;
  }
  return {
    protocol_version: PROTOCOL_VERSION,
    platform: navigator.userAgentData?.platform || navigator.platform || "web",
    platform_version: String(navigator.userAgentData?.brands?.[0]?.version || "unknown"),
    app_version: "web-1",
    device_model: navigator.userAgentData?.mobile ? "mobile browser" : "browser",
    capabilities,
    permissions,
    readiness,
    constraints: {
      max_artifact_bytes: MAX_ARTIFACT_BYTES,
      max_recording_ms: MAX_RECORDING_MS,
      max_notification_chars: 512,
      camera_positions: capabilitySupported("camera_capture") ? ["front", "back"] : [],
    },
    reported_at_ms: Date.now(),
  };
}

// Reports the surface and renders what the runner says is effective.
//
// Called after every event that can change any of the four axes — a permission
// prompt answered, the page coming back to the front, a screen share ending —
// because a runner acting on a stale surface either refuses something possible
// or queues something that will fail in the user's face.
async function advertiseDevice() {
  const surface = await describeDevice();
  state.surface = surface;
  state.deviceState = await signedRequest("POST", "/v1/remote/device/surface", surface);
  renderDeviceState();
  return state.deviceState;
}

let advertisePending = null;
// Coalesced: focus, visibility and permission-change events arrive together and
// three surfaces posted in a row would differ only in their timestamps.
function scheduleAdvertise() {
  if (!state.profile || state.stale) return;
  if (advertisePending) return;
  advertisePending = setTimeout(() => {
    advertisePending = null;
    advertiseDevice().catch(() => {});
  }, 250);
}

// Every input to the four axes that can change without this client acting.
function watchDeviceReadiness() {
  document.addEventListener("visibilitychange", scheduleAdvertise);
  window.addEventListener("focus", scheduleAdvertise);
  window.addEventListener("online", () => {
    scheduleAdvertise();
    // A result staged before the network went is delivered now. This is not a
    // queued user action — the effect already happened and the runner is
    // waiting for it.
    runCommandLoop();
  });
  if (!navigator.permissions?.query) return;
  for (const name of new Set(Object.values(PERMISSION_NAMES))) {
    navigator.permissions
      .query({ name })
      .then((status) => {
        status.addEventListener?.("change", scheduleAdvertise);
      })
      .catch(() => {});
  }
}

function capabilityList(values) {
  return Array.isArray(values) && values.length > 0 ? values.map(humanize).join(", ") : "none";
}

const READINESS_WORDS = {
  [READINESS.ready]: "Ready",
  [READINESS.foregroundRequired]: "Needs this page in front",
  [READINESS.interactionRequired]: "Needs user interaction",
  [READINESS.armedRequired]: "Needs screen sharing armed",
  [READINESS.unavailable]: "Unavailable",
};

const PERMISSION_WORDS = {
  [PERMISSION.granted]: "Granted",
  [PERMISSION.denied]: "Denied",
  [PERMISSION.promptable]: "Needs permission",
  [PERMISSION.notRequired]: "Not required",
  [PERMISSION.unsupported]: "Unsupported",
  undetermined: "Needs permission",
};

function renderDeviceState() {
  if (!ui.devicePanel) return;
  const value = state.deviceState;
  ui.devicePanel.hidden = !value;
  if (!value) return;
  // Never one merged list: "why can it not take a photo" has four different
  // answers and the operator has to be able to see which.
  ui.deviceGranted.textContent = capabilityList(value.granted);
  ui.deviceSupported.textContent = capabilityList(value.advertised);
  ui.deviceEffective.textContent = capabilityList(value.effective);
  renderTalkPanel();
  const permissions = value.os_permissions || {};
  const entries = Object.entries(permissions);
  ui.devicePermissions.textContent = entries.length
    ? entries.map(([capability, permission]) => `${humanize(capability)}: ${permission}`).join(" · ")
    : "not reported";
  renderReadiness();
  // The runner has just restated what this pairing holds, which is where an
  // operator's grant edit becomes visible: a withdrawn `chat` has to take the
  // composer with it rather than leaving a control that answers 403.
  renderSessions();
  renderWorkflows();
  if (ui.capturePanel) ui.capturePanel.hidden = !hasCapability("capture");
}

// One row per granted physical capability: the four axes, and the control that
// fixes whichever one is in the way.
function renderReadiness() {
  const host = ui.deviceReadiness;
  if (!host) return;
  const granted = new Set(state.deviceState?.granted || []);
  const surface = state.surface;
  host.replaceChildren();
  const rows = Object.keys(DEVICE_CAPABILITIES).filter((capability) => granted.has(capability));
  if (rows.length === 0) {
    const empty = document.createElement("p");
    empty.className = "field-help";
    empty.textContent = "No hardware capability is granted to this device.";
    host.append(empty);
    return;
  }
  for (const capability of rows) {
    const supported = capabilitySupported(capability);
    const permission = surface?.permissions?.[capability] || PERMISSION.unsupported;
    const readiness = surface?.readiness?.[capability] || READINESS.unavailable;
    const effective = isEffective({ granted: true, supported, permission, readiness });

    const row = document.createElement("div");
    row.className = "readiness-row";
    const title = document.createElement("p");
    title.className = "readiness-name";
    title.textContent = humanize(capability);
    const facts = document.createElement("p");
    facts.className = "field-help";
    facts.textContent = [
      "Granted: yes",
      `Supported: ${supported ? "yes" : "no"}`,
      `Permission: ${PERMISSION_WORDS[permission] || permission}`,
      `Readiness: ${READINESS_WORDS[readiness] || readiness}`,
      `Effective: ${effective ? "yes" : "no"}`,
    ].join(" · ");
    row.append(title, facts);

    const preparation = PREPARATION[capability];
    // A control only where one would actually help. A denied OS permission is
    // fixed in the system's own settings, not here, and offering a button that
    // silently does nothing is worse than offering none.
    if (!effective && supported && preparation && permission !== PERMISSION.denied) {
      const button = document.createElement("button");
      button.className = "button secondary";
      button.type = "button";
      button.textContent = preparation.label;
      button.disabled = state.stale;
      button.addEventListener("click", async () => {
        setButtonBusy(button, true, "Asking…");
        try {
          await preparation.prepare();
          recordSessionPermission(capability, true);
        } catch (error) {
          recordSessionPermission(capability, false);
          handleError(error, `${preparation.label} was refused`);
        } finally {
          setButtonBusy(button, false);
          // Whatever the answer was, the surface is re-read and re-posted: the
          // runner's view of this device must never be older than the device's.
          await advertiseDevice().catch(() => {});
        }
      });
      row.append(button);
    }
    host.append(row);
  }
}

// What this device is still holding that the runner has not acknowledged.
// Visible locally on purpose: an operator looking at a phone that is holding an
// undelivered photograph should be able to see that, not wonder.
function renderJournalState() {
  const host = ui.journalStatus;
  if (!host) return;
  journalEntries()
    .then((entries) => {
      const pending = entries.filter((entry) => isUnacknowledged(entry));
      const bytes = pending.reduce((total, entry) => total + (Number(entry.artifactBytes) || 0), 0);
      if (pending.length === 0) {
        host.textContent = state.executor
          ? "This tab performs the runner's device commands."
          : "Another tab of this profile is performing device commands.";
        return;
      }
      host.textContent =
        `${pending.length} result${pending.length === 1 ? "" : "s"} not yet acknowledged by the runner` +
        (bytes > 0 ? ` (${Math.round(bytes / 1024)} KiB held here).` : ".");
    })
    .catch(() => {});
}

// --- Preparing a capability, always from a user gesture --------------------

/**
 * Records what this session's own preparation gesture proved.
 *
 * The only thing that may set it: the real browser permission operation,
 * invoked from a real user gesture, returning successfully. Never a guess,
 * never a timer, never anything a remote agent asked for. It is consulted only
 * where the Permissions API cannot answer at all — see `queriedPermission` in
 * `device-core.js` — and a refusal clears it rather than leaving yesterday's
 * answer standing.
 *
 * Keyed by permission *name*, not capability: one microphone consent covers
 * both `microphone_capture` and `voice_stream`, which is what the browser
 * itself thinks too.
 */
function recordSessionPermission(capability, verified) {
  const name = PERMISSION_NAMES[capability];
  if (!name) return;
  if (verified) state.sessionVerified[name] = true;
  else delete state.sessionVerified[name];
}

// Opens the stream only to make the browser ask, then closes it immediately.
// The permission is what is wanted here, not the media.
async function promptForMedia(constraints) {
  const stream = await navigator.mediaDevices.getUserMedia(constraints);
  stopTracks(stream);
  return true;
}

function promptForLocation() {
  return new Promise((resolve, reject) => {
    navigator.geolocation.getCurrentPosition(
      () => resolve(true),
      (error) => reject(new Error(error.message || "Location permission was refused")),
      { timeout: 20_000, maximumAge: 0 },
    );
  });
}

async function promptForNotifications() {
  const decision = await Notification.requestPermission();
  if (decision !== "granted") throw new Error("Notification permission was refused");
  return true;
}

// Autoplay policy, cleared the only way it can be: by playing something
// silently inside the gesture that asked for it.
async function enableAudioPlayback() {
  if (window.speechSynthesis) {
    // A zero-length utterance counts as the page having spoken.
    window.speechSynthesis.speak(new SpeechSynthesisUtterance(" "));
  }
  if (typeof Audio === "function") {
    const silence = new Audio(
      "data:audio/wav;base64,UklGRiQAAABXQVZFZm10IBAAAAABAAEAgD4AAAB9AAACABAAZGF0YQAAAAA=",
    );
    silence.volume = 0;
    await silence.play().catch(() => {});
  }
  state.audioEnabled = true;
  return true;
}

// --- Performing one command ------------------------------------------------

function blobToBase64(blob) {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onerror = () => reject(reader.error || new Error("The captured artifact could not be read"));
    reader.onload = () => {
      const result = String(reader.result || "");
      const comma = result.indexOf(",");
      if (comma < 0) reject(new Error("The captured artifact could not be encoded"));
      else resolve(result.slice(comma + 1));
    };
    reader.readAsDataURL(blob);
  });
}

function stopTracks(stream) {
  for (const track of stream?.getTracks?.() || []) track.stop();
}

const delayUntilAborted = waitOrAbort;

async function captureStill(position, signal) {
  // Cancellation observed before the camera opens prevents the effect outright;
  // that is the only point at which it can.
  if (aborted(signal)) return { cancelledBeforeEffect: true };
  const stream = await navigator.mediaDevices.getUserMedia({
    video: { facingMode: position === "front" ? "user" : "environment" },
    audio: false,
  });
  try {
    if (aborted(signal)) return { cancelledBeforeEffect: true };
    return await frameFromStream(stream, "image/jpeg", 0.85);
  } finally {
    // Always, on every path: a camera left running after a refused or failed
    // capture is the failure mode that matters here.
    stopTracks(stream);
  }
}

// --- The armed screen share -----------------------------------------------
//
// One display stream, held open by the user's explicit choice, reused by every
// screen capture until they stop it. Without this each command would open the
// browser's share picker again — which means a capture only ever happens while
// someone is looking at the phone, and an unattended one is impossible.
//
// It is not a way around consent: the browser still asks once, the page still
// shows what is shared, and the browser's own "stop sharing" control ends it
// from outside this page. That is why the `ended` listener below re-advertises
// rather than trying to reacquire — the honest report is that readiness is gone.

function screenShareIsLive() {
  return Boolean(state.screenStream?.getVideoTracks?.().some((track) => track.readyState === "live"));
}

async function armScreenShare() {
  if (screenShareIsLive()) return true;
  const stream = await navigator.mediaDevices.getDisplayMedia({ video: true, audio: false });
  state.screenStream = stream;
  for (const track of stream.getVideoTracks()) {
    track.addEventListener("ended", () => {
      // Stopped from the browser's own sharing bar. Drop it and tell the runner
      // immediately, so a queued capture fails with "not armed" instead of
      // interrupting someone with a fresh prompt.
      if (state.screenStream === stream) state.screenStream = null;
      renderScreenShare();
      scheduleAdvertise();
    });
  }
  renderScreenShare();
  return true;
}

function disarmScreenShare() {
  stopTracks(state.screenStream);
  state.screenStream = null;
  renderScreenShare();
  scheduleAdvertise();
}

function renderScreenShare() {
  if (!ui.screenShareButton) return;
  const live = screenShareIsLive();
  const supported = capabilitySupported("screen_capture");
  ui.screenShareButton.hidden = !supported;
  ui.screenShareButton.textContent = live ? "Stop screen capture" : "Allow screen capture";
  if (ui.screenShareStatus) {
    ui.screenShareStatus.textContent = supported
      ? live
        ? "The runner may capture this screen until you stop sharing."
        : "Screen capture stays unavailable to the runner until you share once."
      : "This browser cannot capture its screen.";
  }
}

async function captureScreen(signal) {
  if (!screenShareIsLive()) {
    // Never a silent prompt: the runner was told this was not armed, and this
    // path exists only if that report raced with the user stopping.
    throw new Error("Screen sharing is not armed on this device");
  }
  if (aborted(signal)) return { cancelledBeforeEffect: true };
  // The armed stream is deliberately NOT stopped afterwards — it is the whole
  // reason a second capture needs no second prompt.
  return await frameFromStream(state.screenStream, "image/png", undefined);
}

async function frameFromStream(stream, mediaType, quality) {
  const video = document.createElement("video");
  video.playsInline = true;
  video.muted = true;
  video.srcObject = stream;
  await video.play();
  // One frame is not always ready the instant play() resolves.
  await new Promise((resolve) => setTimeout(resolve, 250));
  const canvas = document.createElement("canvas");
  canvas.width = video.videoWidth || 1280;
  canvas.height = video.videoHeight || 720;
  canvas.getContext("2d").drawImage(video, 0, 0, canvas.width, canvas.height);
  video.pause();
  video.srcObject = null;
  const blob = await new Promise((resolve, reject) => {
    canvas.toBlob(
      (value) => (value ? resolve(value) : reject(new Error("The frame could not be encoded"))),
      mediaType,
      quality,
    );
  });
  return { blob, mediaType, result: { width: canvas.width, height: canvas.height } };
}

// The browser half of `recordAudio`: open the microphone, hand over a recorder,
// close it afterwards. Every decision the recording makes — how long, when a
// cancellation cuts it short, what a cut-short recording reports — lives in
// `device-core.js` and is exercised there without a microphone.
function recordMicrophone(durationMs, signal) {
  return recordAudio(durationMs, signal, {
    openStream: () => navigator.mediaDevices.getUserMedia({ audio: true, video: false }),
    createRecorder: (stream) => new MediaRecorder(stream),
    stopStream: stopTracks,
    createBlob: (chunks, mediaType) => new Blob(chunks, { type: mediaType }),
    maxMs: MAX_RECORDING_MS,
  });
}

function readLocation(accuracy, signal) {
  return new Promise((resolve, reject) => {
    if (aborted(signal)) {
      resolve({ cancelledBeforeEffect: true });
      return;
    }
    let settled = false;
    const watch = navigator.geolocation.getCurrentPosition(
      (position) => {
        if (settled) return;
        settled = true;
        resolve({
          result: {
            latitude: position.coords.latitude,
            longitude: position.coords.longitude,
            accuracy_m: position.coords.accuracy,
            // One fix, taken now. This client never registers a watch: there is
            // no continuous background tracking to turn on.
            taken_at_ms: position.timestamp,
          },
        });
      },
      (error) => {
        if (settled) return;
        settled = true;
        reject(new Error(error.message || "Location is unavailable"));
      },
      { enableHighAccuracy: accuracy === "precise", timeout: 20_000, maximumAge: 0 },
    );
    // A fix in flight cannot be recalled, but it can be abandoned: nothing
    // observable happened, so this is honestly a cancellation before effect.
    signal?.addEventListener?.(
      "abort",
      () => {
        if (settled) return;
        settled = true;
        navigator.geolocation.clearWatch?.(watch);
        resolve({ cancelledBeforeEffect: true });
      },
      { once: true },
    );
  });
}

async function postNotification(argumentsValue, signal) {
  if (Notification.permission !== "granted") {
    // Never prompts here. A prompt raised by a remote command would appear
    // without anyone having touched this device — permission is asked for from
    // the readiness control instead, which is a gesture the user made.
    throw new Error(
      "This device has not granted notification permission. Open the paired-device controller and " +
        "allow notifications under Device readiness, then retry.",
    );
  }
  if (aborted(signal)) return { cancelledBeforeEffect: true };
  const notification = new Notification(String(argumentsValue.title || ""), {
    body: String(argumentsValue.body || ""),
    silent: false,
  });
  // Shown. A cancellation arriving now cannot unshow it, and saying otherwise
  // would be a lie the operator acts on.
  return { result: { shown: true, at_ms: Date.now() }, notification };
}

function speak(text, signal) {
  return speakText(text, signal, {
    synthesis: window.speechSynthesis,
    createUtterance: (value) => new SpeechSynthesisUtterance(value),
  });
}

function base64ToBytes(encoded) {
  const binary = atob(String(encoded || ""));
  const bytes = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index += 1) bytes[index] = binary.charCodeAt(index);
  return bytes;
}

// Plays a stored artifact rather than speaking a sentence about it.
//
// The bytes are fetched over the ordinary signed artifact route, under the run
// scope this device was already paired with — there is no second way in, and a
// device without `read_artifacts` cannot reach one at all.
async function playArtifact(runId, artifactId, signal) {
  const artifact = await signedRequest(
    "GET",
    `/v1/remote/runs/${encodeURIComponent(runId)}/artifacts/${encodeURIComponent(artifactId)}`,
  );
  const mediaType = String(artifact.media_type || "audio/mpeg");
  if (!mediaType.startsWith("audio/")) {
    throw new Error(`'${artifact.name || artifactId}' is ${mediaType}, which is not audio`);
  }
  const blob = new Blob([base64ToBytes(artifact.content_base64)], { type: mediaType });
  const url = URL.createObjectURL(blob);
  const audio = new Audio(url);
  try {
    if (aborted(signal)) return { cancelledBeforeEffect: true };
    await audio.play();
    while (!audio.ended && !aborted(signal)) {
      await delayUntilAborted(200, signal);
    }
    // Stopping playback is one of the few cancellations that genuinely works:
    // the sound stops when asked.
    if (!audio.ended) audio.pause();
    return {
      cancelledDuringEffect: !audio.ended,
      result: {
        played: audio.ended,
        artifact_id: artifactId,
        media_type: mediaType,
        bytes: artifact.size_bytes ?? null,
        cancelled: !audio.ended,
      },
    };
  } finally {
    URL.revokeObjectURL(url);
  }
}

async function playAudio(argumentsValue, signal) {
  if (argumentsValue.artifact_id && argumentsValue.run_id) {
    return await playArtifact(argumentsValue.run_id, argumentsValue.artifact_id, signal);
  }
  if (!window.speechSynthesis) {
    throw new Error("This device cannot speak text");
  }
  return await speak(argumentsValue.text, signal);
}

// --- A live microphone stream ----------------------------------------------
//
// The control command stays `running` for as long as the microphone is open;
// the audio does not travel in its result but in chunks, to the session the
// command named. Three things end it: the duration it was given, the runner
// answering `stop: true` — which arrives on the reply to a chunk this device is
// posting anyway — and the cancellation watcher aborting.

// Containers the runner accepts. A recorder that reports anything else has its
// type normalized to the family it belongs to rather than being refused, since
// what matters to the runner is what the bytes are, not how a browser spells it.
const VOICE_MEDIA_TYPES = new Set([
  "audio/webm",
  "audio/webm;codecs=opus",
  "audio/ogg",
  "audio/ogg;codecs=opus",
  "audio/mp4",
  "audio/wav",
]);

function voiceMediaType(recorded) {
  const value = String(recorded || "audio/webm").replace(/\s+/g, "");
  if (VOICE_MEDIA_TYPES.has(value)) return value;
  for (const family of ["audio/webm", "audio/ogg", "audio/mp4", "audio/wav"]) {
    if (value.startsWith(family)) return family;
  }
  return "audio/webm";
}

async function streamVoice(argumentsValue, signal) {
  const sessionId = String(argumentsValue.session_id || "");
  if (!validId(sessionId)) throw new Error("The runner did not name a voice session");
  const durationMs = Math.min(Math.max(Number(argumentsValue.duration_ms) || 60_000, 1_000), MAX_STREAM_MS);
  const chunkMs = Math.min(Math.max(Number(argumentsValue.chunk_ms) || 1_000, 250), 5_000);
  const stream = await navigator.mediaDevices.getUserMedia({ audio: true, video: false });
  const recorder = new MediaRecorder(stream);
  const mediaType = voiceMediaType(recorder.mimeType);
  const pending = [];
  recorder.ondataavailable = (event) => {
    if (event.data?.size) pending.push(event.data);
  };
  let sequence = 0;
  let bytes = 0;
  let stopped = null;

  // One chunk at a time, in order: the runner accepts only the sequence it is
  // expecting, so uploads must not overlap.
  const drain = async () => {
    while (pending.length > 0 && !stopped) {
      const blob = pending.shift();
      const answer = await signedRequest(
        "POST",
        `/v1/remote/device/voice/${encodeURIComponent(sessionId)}/chunk`,
        {
          protocol_version: PROTOCOL_VERSION,
          sequence,
          audio_base64: await blobToBase64(blob),
          media_type: sequence === 0 ? mediaType : null,
          last: false,
        },
      );
      // The runner is the counter's authority; following its answer is what
      // makes a retry after a dropped reply land on the right sequence.
      sequence = Number(answer.next_sequence ?? sequence + 1);
      bytes = Number(answer.bytes ?? bytes + blob.size);
      if (answer.stop === true) stopped = "The runner stopped the stream";
    }
  };

  const started = Date.now();
  try {
    recorder.start(chunkMs);
    while (Date.now() - started < durationMs && !stopped && !aborted(signal)) {
      await delayUntilAborted(Math.min(chunkMs, 500), signal);
      await drain();
    }
    if (aborted(signal)) stopped = "Cancelled on the device";
    recorder.stop();
    // One last slice is emitted by `stop()`; give it a moment to arrive.
    await delay(250);
    // Drained even when stopping, so the last second of audio is not lost —
    // `stopped` is cleared for exactly this and then restored.
    const reason = stopped;
    stopped = null;
    await drain();
    stopped = reason;
  } finally {
    // Always: a microphone left open after a failed stream is the failure that
    // matters here.
    stopTracks(stream);
    try {
      await signedRequest("POST", `/v1/remote/device/voice/${encodeURIComponent(sessionId)}/close`, {
        protocol_version: PROTOCOL_VERSION,
        error: null,
      });
    } catch {
      // The runner closes it on its own deadline; nothing here can do better.
    }
  }
  return {
    cancelledDuringEffect: aborted(signal),
    result: {
      session_id: sessionId,
      chunks: sequence,
      bytes,
      media_type: mediaType,
      duration_ms: Date.now() - started,
      stopped_because: stopped || "the requested duration elapsed",
    },
  };
}

// Runs one leased command and returns the terminal report to send back.
//
// The three cancellation outcomes are kept apart rather than collapsed into
// "cancelled": an operator reading a result has to be able to tell a photograph
// that never happened from one that did.
async function performCommand(command, signal) {
  const argumentsValue = command.arguments || {};
  switch (command.capability) {
    case "device_info":
      return { outcome: "succeeded", result: await describeDevice() };
    case "camera_capture":
      return await stagedArtifactOutcome(await captureStill(argumentsValue.position, signal));
    case "screen_capture":
      return await stagedArtifactOutcome(await captureScreen(signal));
    case "microphone_capture":
      return await stagedArtifactOutcome(await recordMicrophone(argumentsValue.duration_ms, signal));
    case "location_read":
      return plainOutcome(await readLocation(argumentsValue.accuracy, signal));
    case "notification_post":
      return plainOutcome(await postNotification(argumentsValue, signal));
    case "audio_playback":
      return plainOutcome(await playAudio(argumentsValue, signal));
    case "voice_stream":
      return plainOutcome(await streamVoice(argumentsValue, signal));
    default:
      // Honest refusal rather than a silent success: the runner records the
      // reason and the waiting run reads it.
      return {
        outcome: "failed",
        error: `This device build does not implement '${command.capability}'`,
      };
  }
}

// Digested once, here, and carried through staging: the same digest is declared
// to the runner after a reload, so a truncated redelivery is caught rather than
// accepted as authoritative bytes.
function stagedArtifactOutcome(outcome) {
  return artifactOutcome(outcome, {
    digest: async (blob) => sha256Hex(await blob.arrayBuffer()),
    maxBytes: MAX_ARTIFACT_BYTES,
  });
}

// --- The durable command journal -------------------------------------------

// --- Talk: a live conversation over a dedicated socket ----------------------
//
// Everything else this client does is a signed request: HMAC over method, path,
// body, sequence, nonce and key generation. A WebSocket handshake takes no
// headers, so Talk cannot be authenticated the same way — instead it makes ONE
// ordinary signed request for a one-use ticket and spends it immediately on the
// socket. The socket carries no other credential, and the ticket is gone the
// moment it is used.
//
// Voice activity detection is local and stays local: the phone decides where an
// utterance ends and marks the last frame, so the runner never guesses from
// silence it cannot hear. Nothing is uploaded while nobody is speaking — and
// nothing is *recorded* while nobody is speaking either. The microphone is
// observed continuously (barge-in depends on that) but the recorder is armed
// only at confirmed speech and stopped at the end of it, so an uploaded blob
// contains one utterance rather than every silent minute since the last one.
//
// Foreground only, and said so on screen. A page that is hidden loses its
// microphone on iOS and Android alike, so the session is closed deliberately
// rather than left looking alive.
//
// The frames themselves are built in `talkProtocol.js`, which owns the
// sequences and refuses to build an audio frame before the hello. That is the
// one property this file cannot be trusted with: the runner's first-frame rule
// is invisible to anything that only reads the source.

const TALK_CHUNK_MS = 250;

const talk = {
  socket: null,
  stream: null,
  recorder: null,
  context: null,
  analyser: null,
  meterTimer: null,
  /** The frame builder for this session; also the proof a hello was sent. */
  frames: null,
  detector: null,
  /** Container this session's recorder is asked for, agreed with the hello. */
  recorderOptions: null,
  /** Timings for the utterance being recorded right now, and nothing else. */
  utterance: null,
  sessionId: null,
  sessionGeneration: null,
  /**
   * Whether the runner is mid-answer, as this client last heard it.
   *
   * Read instead of the panel's `data-state` attribute: barge-in has to work
   * while the model is still generating, and reaching into the DOM for that
   * made the answer depend on which frame last repainted a badge.
   */
  answering: false,
  /** Queue of synthesized chunks, played strictly in order. */
  playing: Promise.resolve(),
  playbackGeneration: 0,
  player: null,
  running: false,
};

function talkSupported() {
  return Boolean(
    window.WebSocket &&
      window.MediaRecorder &&
      window.AudioContext &&
      navigator.mediaDevices?.getUserMedia,
  );
}

function setTalkState(label, key) {
  ui.talkState.textContent = label;
  ui.talkPanel.dataset.state = key;
  ui.talkInterruptButton.disabled = !(key === "thinking" || key === "speaking");
}

function showTalkError(message) {
  ui.talkError.hidden = !message;
  ui.talkError.textContent = message || "";
}

// The runner's effective set is the authority; a capability this device
// advertises but was not granted must read as unavailable, not as broken.
function renderTalkPanel() {
  if (!ui.talkPanel) return;
  const effective = state.deviceState?.effective || [];
  const granted = state.deviceState?.granted || [];
  const permitted = effective.includes("voice_stream");
  ui.talkPanel.hidden = !state.profile;
  ui.talkButton.disabled = !permitted || state.stale;
  if (!talkSupported()) {
    ui.talkUnavailable.hidden = false;
    ui.talkUnavailable.textContent =
      "This browser cannot open a microphone stream, so Talk is unavailable here.";
    ui.talkButton.disabled = true;
    return;
  }
  if (!permitted) {
    ui.talkUnavailable.hidden = false;
    ui.talkUnavailable.textContent = granted.includes("voice_stream")
      ? "Talk is granted, but this device's microphone permission has not been given. Allow the microphone and reload."
      : "Talk needs the voice_stream grant. Grant it on the runner's device card.";
    return;
  }
  ui.talkUnavailable.hidden = true;
}

/**
 * Hands one already-built frame to the socket.
 *
 * It takes a finished frame rather than building one, because the sequence
 * numbers and the first-frame rule belong to `createTalkFrames` — a helper that
 * built frames here is exactly how `audio` came to be sent before `hello`.
 */
function talkSendFrame(frame) {
  if (!talk.socket || talk.socket.readyState !== WebSocket.OPEN) return;
  talk.socket.send(JSON.stringify(frame));
}

/** Stop the speaker and drop everything queued behind it. */
function talkStopPlayback() {
  talk.playbackGeneration += 1;
  talk.playing = Promise.resolve();
  if (talk.player) {
    talk.player.pause();
    talk.player = null;
  }
}

function talkInterrupt(reason) {
  talkStopPlayback();
  // The runner will confirm with an `interrupted` state, but this device stops
  // believing it is being answered right now — otherwise the next confirmed
  // syllable sends a second interrupt for an answer already abandoned.
  talk.answering = false;
  if (talk.frames) talkSendFrame(talk.frames.interrupt(reason));
}

function talkQueueAudio(audioBase64, mediaType) {
  const generation = talk.playbackGeneration;
  talk.playing = talk.playing.then(
    () =>
      new Promise((resolve) => {
        if (generation !== talk.playbackGeneration) {
          resolve();
          return;
        }
        const bytes = Uint8Array.from(atob(audioBase64), (character) => character.charCodeAt(0));
        const url = URL.createObjectURL(new Blob([bytes], { type: mediaType || "audio/wav" }));
        const player = new Audio(url);
        talk.player = player;
        const finish = () => {
          URL.revokeObjectURL(url);
          if (talk.player === player) talk.player = null;
          resolve();
        };
        player.onended = finish;
        player.onerror = finish;
        player.play().catch(finish);
      }),
  );
}

function talkHandleFrame(raw) {
  let frame;
  try {
    frame = JSON.parse(raw);
  } catch {
    return;
  }
  if (frame.session_generation !== talk.sessionGeneration) return;
  switch (frame.type) {
    case "ready":
      setTalkState("Listening", "listening");
      break;
    case "state":
      setTalkState(
        {
          idle: "Idle",
          starting: "Starting",
          listening: "Listening",
          transcribing: "Transcribing",
          thinking: "Thinking",
          speaking: "Speaking",
          interrupted: "Interrupted",
          error: "Error",
        }[frame.state] || frame.state,
        frame.state,
      );
      // An answer is in flight from the moment the model starts generating, not
      // from the moment audio arrives. Talking during "thinking" has to reach
      // the runner, or the first thing the speaker does is talk over the user.
      talk.answering = frame.state === "thinking" || frame.state === "speaking";
      if (frame.state === "interrupted") talkStopPlayback();
      break;
    case "transcript":
      ui.talkTranscript.textContent = frame.text;
      ui.talkAnswer.textContent = "—";
      // A spoken turn becomes a message in the shared conversation, so the
      // typed list has to hear about it too.
      break;
    case "assistant_delta":
      ui.talkAnswer.textContent =
        ui.talkAnswer.textContent === "—" ? frame.text : ui.talkAnswer.textContent + frame.text;
      break;
    case "output_audio":
      talkQueueAudio(frame.audio_base64, frame.media_type);
      break;
    case "error":
      showTalkError(frame.message);
      if (!frame.retryable) void stopTalk();
      break;
    default:
      break;
  }
}

async function startTalk() {
  if (talk.running) return;
  showTalkError("");
  setButtonBusy(ui.talkButton, true, "Connecting…");
  const sessionId = mobileSessionId();
  if (!sessionId) {
    // Talk names the conversation the operator is looking at, so there has to
    // be one. Minting a session of Talk's own is what put voice in a namespace
    // the typed surface could not read.
    showTalkError("Choose a conversation above, then start Talk.");
    setButtonBusy(ui.talkButton, false);
    return;
  }
  try {
    // The microphone is opened BEFORE the ticket is asked for: a ticket lives
    // thirty seconds, and a permission prompt the user reads slowly would burn
    // it before the socket ever opened.
    talk.stream = await navigator.mediaDevices.getUserMedia({
      audio: { echoCancellation: true, noiseSuppression: true },
      video: false,
    });
    // Everything the hello has to state is settled here, before there is a
    // socket to be wrong on. The container is the one the recorder will
    // actually produce, and the rate and channel count are read from the
    // microphone and the audio graph rather than invented — the runner refuses
    // a hello outside 8 kHz–192 kHz or outside one or two channels, and a
    // plausible-looking guess is still a guess about someone else's hardware.
    const preferred = chooseTalkMediaType((type) => MediaRecorder.isTypeSupported(type));
    talk.recorderOptions = preferred ? { mimeType: preferred } : undefined;
    // No preference supported: a recorder still records in *something*, so ask
    // a throwaway one what that is rather than naming a container this browser
    // will not produce.
    const recorded = preferred || new MediaRecorder(talk.stream).mimeType;
    const mediaType = normalizeTalkMediaType(recorded);
    if (!mediaType) {
      // Sending the bytes anyway under a container name they do not have would
      // hand the transcriber a file whose header contradicts its media type.
      throw new RemoteError(`This browser records in ${recorded || "a container"} that Talk cannot transcribe`, 0);
    }
    const context = new AudioContext();
    talk.context = context;
    // A context created inside a gesture handler can still open suspended on
    // iOS, and a suspended graph feeds the detector silence for ever.
    if (context.state === "suspended") await context.resume().catch(() => undefined);
    const settings = talk.stream.getAudioTracks()[0]?.getSettings?.() || {};
    const sampleRateHz = clampTalkSampleRateHz(context.sampleRate);
    const channels = clampTalkChannels(settings.channelCount);

    const ticket = await signedRequest("POST", "/v1/remote/device/talk/ticket", {
      protocol_version: TALK_PROTOCOL_VERSION,
      session_id: sessionId,
    });
    talk.sessionId = ticket.session_id;
    talk.sessionGeneration = ticket.session_generation;
    // Same origin as this page, by construction: pairing already refused an
    // invitation whose runner URL was not this origin, so there is no second
    // host a socket could be pointed at.
    const url = new URL(ticket.websocket_path, location.origin);
    url.protocol = "wss:";
    url.searchParams.set("ticket", ticket.ticket);
    const socket = new WebSocket(url.toString());
    talk.socket = socket;
    await new Promise((resolve, reject) => {
      socket.onopen = resolve;
      socket.onerror = () => reject(new RemoteError("The Talk socket could not be opened", 0));
    });
    socket.onmessage = (event) => talkHandleFrame(event.data);
    socket.onclose = () => {
      if (talk.running) void stopTalk("The runner closed the conversation");
    };
    talk.running = true;
    talk.answering = false;
    ui.talkButton.textContent = "End Talk";
    // Frame 1 is the hello, before anything can produce a frame 2: the runner
    // refuses any other opening frame with `retryable: false`, which this
    // client's own error handler turns into a torn-down session.
    talk.frames = createTalkFrames({
      sessionId: ticket.session_id,
      sessionGeneration: ticket.session_generation,
      mediaType,
      sampleRateHz,
      channels,
    });
    talkSendFrame(talk.frames.hello());
    // Only now: the detector and the recorder it arms cannot run before the
    // greeting they belong to.
    talkStartCapture(context);
    setTalkState("Listening", "listening");
  } catch (error) {
    await stopTalk();
    handleError(error, "Talk could not be started");
  } finally {
    setButtonBusy(ui.talkButton, false);
    if (talk.running) ui.talkButton.textContent = "End Talk";
  }
}

function talkStartCapture(context) {
  const analyser = context.createAnalyser();
  analyser.fftSize = 1024;
  context.createMediaStreamSource(talk.stream).connect(analyser);
  talk.analyser = analyser;
  talk.detector = createTalkDetector();
  const buffer = new Float32Array(analyser.fftSize);
  // The microphone is observed for the whole session; only the recorder starts
  // and stops. Barge-in needs to hear the user while the runner is speaking,
  // which a detector that only ran during recording could not do.
  talk.meterTimer = setInterval(() => {
    if (!talk.analyser || !talk.detector) return;
    talk.analyser.getFloatTimeDomainData(buffer);
    let squares = 0;
    for (const sample of buffer) squares += sample * sample;
    const rms = Math.sqrt(squares / buffer.length);
    const { event, threshold, speechDetectionMs } = talk.detector.observe(rms, Date.now());
    // The two writes the detector deliberately does not do, so that it can be
    // tested without a document.
    ui.talkMeterFill.style.width = `${Math.min(100, Math.round((rms / Math.max(threshold * 2.5, 0.001)) * 100))}%`;
    ui.talkMeter.setAttribute("aria-valuenow", String(Math.min(100, Math.round(rms * 1000))));
    if (event === "speech-start") {
      // Talking over the answer stops it, here and on the runner — whether the
      // runner is speaking or still thinking about what to say.
      if (talk.answering) talkInterrupt("barge_in");
      talkBeginUtterance(speechDetectionMs);
    } else if (event === "utterance-end" || event === "max-utterance") {
      talkFinishUtterance();
    }
  }, 20);
}

/**
 * One recorder per utterance, armed at confirmed speech and not before.
 *
 * The recorder used to run continuously and re-arm itself, which meant every
 * upload carried the whole silent gap since the last one — and, during a
 * barge-in, the assistant's own answer coming back in through the speaker.
 * Browser echo cancellation is not enough on its own: it is a best-effort DSP
 * on a phone held at arm's length, and it is not what decides what leaves the
 * device. Not recording is.
 *
 * The cost is the ~180 ms the detector spends confirming: the first syllable of
 * each utterance is not in the recording. That is deliberate and tested — a
 * pre-roll buffer would mean holding raw microphone audio that nobody asked to
 * be recorded, which is the trade this client refuses to make.
 */
function talkBeginUtterance(speechDetectionMs) {
  if (!talk.stream || !talk.running || !talk.frames) return;
  // Anything still recording is discarded rather than continued. Nothing
  // captured before this moment — including assistant audio that leaked back
  // through the speaker — can ride along with what the user is saying now.
  talkDiscardRecorder();
  const recorder = talk.recorderOptions
    ? new MediaRecorder(talk.stream, talk.recorderOptions)
    : new MediaRecorder(talk.stream);
  const chunks = [];
  recorder.ondataavailable = (event) => {
    if (event.data?.size) chunks.push(event.data);
  };
  // Held in the closure rather than read back off `talk` when the recorder
  // stops: `onstop` is asynchronous, and these three numbers belong to *this*
  // utterance whatever the microphone has heard since.
  const utterance = { speechDetectionMs, startedAtMs: Date.now(), stoppedAtMs: null };
  recorder.onstop = async () => {
    const stoppedAtMs = utterance.stoppedAtMs ?? Date.now();
    const blob = new Blob(chunks, { type: talk.frames?.mediaType || "audio/webm" });
    if (blob.size > 0 && talk.running && talk.frames) {
      const audioBase64 = await blobToBase64(blob);
      // Ninety seconds of Opus is far more than one frame may carry, so the
      // utterance goes out as however many frames it takes; the runner
      // reassembles them and only the last one closes it.
      const chunks = splitTalkAudioBase64(audioBase64);
      try {
        // Before the audio, not after: the runner answers the moment the
        // closing frame lands, so metrics sent afterwards would arrive too late
        // to belong to this utterance. They name it explicitly as well, so a
        // future reordering loses the telemetry instead of misfiling it.
        //
        // Three durations, measured on this device, that the runner cannot see.
        // Never a word of what was said — the frame has no room for one.
        talkSendFrame(
          talk.frames.metrics({
            audioSequence: talk.frames.audioSequence + 1,
            speechDetectionMs: utterance.speechDetectionMs,
            captureMs: stoppedAtMs - utterance.startedAtMs,
            uploadMs: Date.now() - stoppedAtMs,
          }),
        );
        chunks.forEach((chunk, at) => {
          talkSendFrame(talk.frames.audio({ audioBase64: chunk, last: at === chunks.length - 1 }));
        });
      } catch (error) {
        // Refused here rather than by the runner, whose refusal is not
        // retryable and would end the conversation.
        showTalkError(String(error?.message || error));
        return;
      }
    }
    // Deliberately NOT re-armed: the next recorder starts at the next confirmed
    // speech-start, so the microphone is observed but nothing is captured while
    // the runner answers.
  };
  talk.recorder = recorder;
  talk.utterance = utterance;
  recorder.start(TALK_CHUNK_MS);
}

/** Drops a recorder without uploading what it holds. */
function talkDiscardRecorder() {
  const recorder = talk.recorder;
  talk.recorder = null;
  talk.utterance = null;
  if (recorder && recorder.state !== "inactive") {
    recorder.onstop = null;
    recorder.stop();
  }
}

function talkFinishUtterance() {
  const recorder = talk.recorder;
  talk.recorder = null;
  if (!recorder || recorder.state === "inactive") return;
  // Stamped before `stop()` so the upload span measures the encode and the
  // base64, not the time the browser took to notice.
  if (talk.utterance) talk.utterance.stoppedAtMs = Date.now();
  talk.utterance = null;
  recorder.stop();
}

async function stopTalk(reason) {
  talk.running = false;
  talk.answering = false;
  talkStopPlayback();
  if (talk.meterTimer) {
    clearInterval(talk.meterTimer);
    talk.meterTimer = null;
  }
  talkDiscardRecorder();
  talk.detector = null;
  talk.frames = null;
  talk.recorderOptions = null;
  // Always: a microphone left open after a failed conversation is the failure
  // that matters here.
  stopTracks(talk.stream);
  talk.stream = null;
  if (talk.context) {
    void talk.context.close().catch(() => undefined);
    talk.context = null;
  }
  talk.analyser = null;
  if (talk.socket) {
    talk.socket.onclose = null;
    if (talk.socket.readyState === WebSocket.OPEN) talk.socket.close();
    talk.socket = null;
  }
  talk.sessionId = null;
  talk.sessionGeneration = null;
  ui.talkButton.textContent = "Start Talk";
  setTalkState(reason || "Not connected", "idle");
  if (reason) showTalkError(reason);
}

// A page that is hidden has no microphone on either mobile platform. Ending the
// session is the honest response; a "background Talk" this cannot deliver would
// be a promise the operating system breaks.
//
// Three hooks, because `visibilitychange` alone does not cover the ways a
// mobile page actually goes away: iOS puts a page into the back/forward cache
// or evicts it outright, and Chrome freezes a backgrounded tab. Either can
// happen without a visibility change, and a session that ends only when the
// process does holds the microphone and the socket until then.
function endTalkForBackground(reason) {
  if (talk.running) void stopTalk(reason);
}

document.addEventListener("visibilitychange", () => {
  if (document.hidden) endTalkForBackground("Talk ended: this page went to the background");
});
window.addEventListener("pagehide", () => {
  endTalkForBackground("Talk ended: this page was closed or put away");
});
// Not implemented everywhere; where it is, it is the last event a frozen tab
// gets, and `stopTalk` is synchronous up to and including releasing the tracks.
document.addEventListener("freeze", () => {
  endTalkForBackground("Talk ended: this tab was frozen by the browser");
});

// --- The command loop ------------------------------------------------------

// Long-polls for work, performs it, and reports back.
//
// One record per command this device has been handed, holding the phase it
// reached and — once it has one — the staged result including its bytes. It is
// the reason a reload cannot cause a second photograph and cannot lose the
// first one: the phase is written *before* the runner is asked to authorize a
// start, and the bytes are dropped only after the runner acknowledges them.

async function withJournal(mode, operation) {
  const database = await openDatabase();
  try {
    return await new Promise((resolve, reject) => {
      const transaction = database.transaction(JOURNAL_STORE, mode);
      const store = transaction.objectStore(JOURNAL_STORE);
      let result;
      let failure;
      try {
        result = operation(store);
      } catch (error) {
        failure = error;
        transaction.abort();
      }
      transaction.oncomplete = () => resolve(result);
      transaction.onerror = () => reject(failure || transaction.error || new Error("The command journal failed"));
      transaction.onabort = () => reject(failure || transaction.error || new Error("The command journal was aborted"));
    });
  } finally {
    database.close();
  }
}

function requestValue(request) {
  return new Promise((resolve, reject) => {
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error || new Error("The command journal could not be read"));
  });
}

// The IndexedDB adapter the journal runs on. Mechanical: open, one request,
// close. Every decision it serves — what may be dropped, what may be forgotten,
// what must be retried — lives in `device-core.js` and is tested there.
const journalAdapter = {
  async get(commandId) {
    const database = await openDatabase();
    try {
      const transaction = database.transaction(JOURNAL_STORE, "readonly");
      return (await requestValue(transaction.objectStore(JOURNAL_STORE).get(commandId))) || null;
    } finally {
      database.close();
    }
  },
  async all() {
    const database = await openDatabase();
    try {
      const transaction = database.transaction(JOURNAL_STORE, "readonly");
      return (await requestValue(transaction.objectStore(JOURNAL_STORE).getAll())) || [];
    } finally {
      database.close();
    }
  },
  put(record) {
    return withJournal("readwrite", (store) => store.put(record));
  },
  remove(commandIds) {
    return withJournal("readwrite", (store) => {
      for (const commandId of commandIds) store.delete(commandId);
    });
  },
};

const journal = createJournal(journalAdapter, { limits: JOURNAL_LIMITS });

const journalEntry = (commandId) => journal.get(commandId);
const journalEntries = () => journal.all();

async function journalWrite(entry) {
  const record = await journal.write(entry);
  renderJournalState();
  return record;
}

// Drops what the bounds allow, and never an unacknowledged result: a cache
// limit must not become data loss about an effect that really happened.
const pruneJournal = () => journal.prune();

function stagedReport(entry) {
  return {
    outcome: entry.outcome,
    result: entry.result ?? null,
    error: entry.error ?? null,
    artifactBlob: entry.artifactBlob ?? null,
    artifactMediaType: entry.artifactMediaType ?? null,
    artifactSha256: entry.artifactSha256 ?? null,
  };
}

// --- The command loop ------------------------------------------------------

// One executor per paired profile, whatever a browser does with tabs.
//
// The lock is held for the whole loop rather than around each signed request:
// two loops that merely serialized their HTTP calls would still lease, start
// and execute the same command in two tabs. Everything physical happens inside
// this lock.
const EXECUTOR_LOCK = "little-monkey-device-executor-v1";

async function runCommandLoop() {
  if (state.commandLoopRunning) return;
  state.commandLoopRunning = true;
  try {
    const held = await acquireExecutor(navigator.locks, EXECUTOR_LOCK, async () => {
      state.executor = true;
      renderJournalState();
      await commandLoopBody();
    });
    if (!held.executor) {
      state.executor = false;
      renderJournalState();
    }
  } catch (error) {
    handleError(error, "Device command loop stopped");
  } finally {
    state.executor = false;
    state.commandLoopRunning = false;
  }
}

// The order is the whole recovery contract, and it is not negotiable:
//
//   1. deliver everything already staged — those effects have happened, and the
//      runner is waiting for results it may re-hand out otherwise;
//   2. reconcile what the runner still calls running — deliver or report
//      unknown, never re-execute;
//   3. only then take new work.
//
// Leasing first would let a fresh command race ahead of a result the runner is
// still waiting for, and a command whose result never arrives is one the runner
// eventually fails as unproven.
async function commandLoopBody() {
  while (state.profile && !state.stale) {
    try {
      await flushOutbox();
      await reconcileRunningCommands();
    } catch (error) {
      if (error instanceof RemoteError && error.status === 401) throw error;
      await delay(5_000);
      continue;
    }
    let command;
    try {
      // A long poll, so anything the user does meanwhile cancels it and takes
      // the request lock rather than waiting out the runner's deadline.
      command = await signedRequest(
        "GET",
        `/v1/remote/device/commands/next?wait_ms=${LEASE_WAIT_MS}`,
        undefined,
        { longPoll: true },
      );
    } catch (error) {
      if (error instanceof RemoteError && error.status === 401) throw error;
      // A poll this device gave up so something else could speak: ask again at
      // once. Backing off would make every tap cost five idle seconds of work
      // this device could have been taking.
      if (error instanceof RemoteError && error.cancelled) continue;
      // Any other failure is a network hiccup: wait and poll again rather
      // than tearing the session down.
      await delay(5_000);
      continue;
    }
    if (!command || !validId(command.command_id)) continue;
    await executeLeasedCommand(command);
    await pruneJournal();
  }
}

/**
 * Everything this device performed and the runner has not acknowledged.
 *
 * This is not an action queue. Nothing here is a request somebody made offline
 * and this client decided to replay — every entry is the *result of an effect
 * that already happened*, which the runner is waiting for and will otherwise
 * record as unproven. That is why it retries while approvals, cancellations and
 * chat sends never do.
 */
async function flushOutbox() {
  const staged = (await journalEntries()).filter((entry) => entry.phase === PHASE.resultStaged);
  for (const entry of staged) {
    await deliverStaged(entry);
  }
}

async function deliverStaged(entry) {
  const answer = await deliverStagedResult(entry, {
    journal,
    send: (staged) => reportCommand(staged.commandId, stagedReport(staged), staged.executionId),
  });
  renderJournalState();
  if (answer.outcome === "conflict") {
    showToast("The runner already holds a different result for one command; ours was dropped.");
    return false;
  }
  if (answer.outcome === "retry") {
    // Bounded backoff, then out — the loop's next pass, a reconnect or the
    // online handler wakes it again. Never a tight retry.
    await delay(answer.backoffMs);
    throw new RemoteError("The device result has not been acknowledged yet");
  }
  return true;
}

/**
 * The commands the runner still believes are running on this device.
 *
 * Deliberately a separate route from the lease: a `running` command handed back
 * as work would be a second execution. Each one is answered from the journal,
 * and the answer is never "do it again".
 */
async function reconcileRunningCommands() {
  const answer = await signedRequest("GET", "/v1/remote/device/commands/recover");
  const commands = Array.isArray(answer?.commands) ? answer.commands : [];
  for (const command of commands) {
    if (!validId(command.command_id)) continue;
    const entry = await journalEntry(command.command_id);
    const decision = recoveryAction(entry);
    if (decision.action === "none") continue;
    if (decision.action === "deliver_staged") {
      await deliverStaged(entry);
      continue;
    }
    // The uncertainty window. The runner authorized a start, so the effect may
    // have happened; nothing survives to prove it either way. Reported as
    // exactly that, and never performed again.
    const report = unknownOutcomeReport(decision.reason);
    await journalWrite({
      commandId: command.command_id,
      capability: command.capability,
      executionId: entry?.executionId ?? command.execution_id ?? null,
      phase: PHASE.uncertain,
      outcome: report.outcome,
      result: null,
      error: report.error,
      artifactBlob: null,
      artifactBytes: 0,
    });
    try {
      // The runner's own execution id when this device has lost its journal:
      // a terminal report has to name the execution that holds the command, and
      // `/recover` is where the holder is stated.
      await reportCommand(
        command.command_id,
        report,
        entry?.executionId ?? command.execution_id ?? null,
      );
      await journalWrite({
        commandId: command.command_id,
        capability: command.capability,
        executionId: entry?.executionId ?? null,
        phase: PHASE.resultAcked,
        outcome: report.outcome,
        error: report.error,
        artifactBlob: null,
        artifactBytes: 0,
      });
      showToast("A command interrupted mid-action was reported as unknown, not repeated.");
    } catch (error) {
      if (!(error instanceof RemoteError && error.status === 409)) throw error;
    }
  }
}

// One leased command, performed by `runLeasedCommand` over this browser.
//
// Everything ordered lives in `device-core.js` — journal before start, start
// before hardware, the result durable before any network wait, the watcher
// stopped only after that, delivery last. This half is the browser: signed
// requests, the physical effect, and what the screen says about it.
async function executeLeasedCommand(command) {
  await runLeasedCommand(command, {
    journal,
    request: signedRequest,
    perform: performCommand,
    // Delivery failure is never fatal to the loop: the entry stays staged and
    // the next pass, a reconnect or the online handler tries again.
    deliver: (entry) => deliverStaged(entry).catch(() => false),
    report: reportCommand,
    newExecutionId: () => `exec-${randomToken(18)}`,
    // Room for the result, bounded by whichever of the two ceilings is lower.
    artifactCeiling: Math.min(
      MAX_ARTIFACT_BYTES,
      Number(state.deviceState?.max_artifact_bytes) || MAX_ARTIFACT_BYTES,
    ),
    controlWaitMs: LEASE_WAIT_MS,
    notify: (capability) => showToast(`Running ${humanize(capability)} for the runner…`),
    onStartFailed: (error) => handleError(error, "The device command could not be started"),
  });
  renderJournalState();
}

async function reportCommand(commandId, report, executionId) {
  const encoded = report.artifactBlob ? await blobToBase64(report.artifactBlob) : null;
  await signedRequest("POST", `/v1/remote/device/commands/${encodeURIComponent(commandId)}/result`, {
    protocol_version: PROTOCOL_VERSION,
    outcome: report.outcome,
    result: report.result ?? null,
    artifact_base64: encoded,
    artifact_media_type: report.artifactMediaType ?? null,
    // Declared so a truncated upload is refused rather than stored as
    // authoritative bytes that do not match what this device holds.
    artifact_sha256: encoded ? report.artifactSha256 ?? null : null,
    error: report.error ?? null,
    execution_id: executionId ?? null,
  });
}

// Keeps the last view of the runner so the app opens to something on a train.
//
// Everything the controller *reads* is cached — runs, their details, events,
// approval metadata, artifact metadata, sessions and messages — and nothing it
// *does* is. That asymmetry is the whole design: a queued approval replayed on
// reconnect would act on a run whose state this device could not see, so no
// action is ever buffered. A draft is the one exception, and it is not an
// action: nothing has happened until it is sent.
function emptyCache() {
  return {
    savedAtMs: 0,
    runs: [],
    details: {},
    approvals: {},
    events: {},
    artifacts: {},
    sessions: [],
    messages: {},
  };
}

async function updateRecord(mutate) {
  await withStore("readwrite", (store, transaction) => {
    const request = store.get(ACTIVE_RECORD);
    request.onsuccess = () => {
      const record = request.result;
      if (!record) return;
      try {
        mutate(record);
        store.put(record);
      } catch {
        transaction.abort();
      }
    };
    request.onerror = () => transaction.abort();
  });
}

async function cacheWrite(mutate) {
  await updateRecord((record) => {
    record.cache = { ...emptyCache(), ...(record.cache || {}) };
    mutate(record.cache);
    // Pruned on every write rather than on read: a bound enforced only when
    // something reads it is a bound that grows without limit on a device that
    // is never opened offline.
    record.cache.runs = record.cache.runs.slice(0, CACHE_LIMITS.runs);
    const visible = new Set(record.cache.runs.map((run) => run.run_id));
    for (const key of ["details", "approvals", "events", "artifacts"]) {
      for (const runId of Object.keys(record.cache[key])) {
        if (!visible.has(runId)) delete record.cache[key][runId];
      }
    }
    record.cache.sessions = record.cache.sessions.slice(0, CACHE_LIMITS.sessions);
    const sessions = new Set(record.cache.sessions.map((session) => session.id));
    for (const sessionId of Object.keys(record.cache.messages)) {
      if (!sessions.has(sessionId)) delete record.cache.messages[sessionId];
    }
    record.cache.savedAtMs = Date.now();
  });
}

async function cacheRuns(runs) {
  await cacheWrite((cache) => {
    cache.runs = runs.slice(0, CACHE_LIMITS.runs);
  });
}

async function cacheRunDetail(runId, run, spec, paused) {
  await cacheWrite((cache) => {
    cache.details[runId] = { run, spec, paused };
  });
}

async function cacheApprovals(runId, approvals) {
  await cacheWrite((cache) => {
    // Metadata only, which is all the route returns: an approval carries a
    // digest and an expiry, never the operation's arguments.
    cache.approvals[runId] = approvals.slice(0, CACHE_LIMITS.approvalsPerRun);
  });
}

// Events, and the artifact metadata they announce.
//
// The bytes are never cached — an artifact is fetched over the signed route and
// verified against its digest, and a copy sitting in this browser would be an
// unverified second source. What is kept is that the artifact exists, so an
// offline device can tell someone which id to ask for.
async function cacheEvents(runId, events) {
  await cacheWrite((cache) => {
    cache.events[runId] = events.slice(-CACHE_LIMITS.eventsPerRun);
    const artifacts = new Map(
      (cache.artifacts[runId] || []).map((artifact) => [artifact.artifact_id, artifact]),
    );
    for (const envelope of events) {
      if (envelope.event?.type !== "artifact_added") continue;
      const payload = envelope.event.payload || {};
      const artifactId = payload.artifact_id;
      if (typeof artifactId !== "string" || !validId(artifactId)) continue;
      artifacts.set(artifactId, {
        artifact_id: artifactId,
        media_type: typeof payload.media_type === "string" ? payload.media_type : null,
        bytes: Number.isSafeInteger(payload.bytes) ? payload.bytes : null,
        sequence: envelope.sequence,
      });
    }
    cache.artifacts[runId] = [...artifacts.values()].slice(-CACHE_LIMITS.artifactsPerRun);
  });
}

async function cacheSessions(sessions) {
  await cacheWrite((cache) => {
    cache.sessions = sessions.slice(0, CACHE_LIMITS.sessions);
  });
}

async function cacheMessages(sessionId, messages) {
  await cacheWrite((cache) => {
    cache.messages[sessionId] = messages.slice(-CACHE_LIMITS.messagesPerSession);
  });
}

// A draft is not an action, so unlike everything else on this screen it is kept
// while offline and restored on the next load. It is stored beside the cache
// rather than inside it, so pruning a run or a session never deletes something
// a person typed.
async function saveDraft(sessionId, text) {
  // A draft keyed by something `validateStoredRecord` would later reject would
  // invalidate the whole profile on the next load — which is to say, lose the
  // device key over a piece of text. The session list only ever offers ids that
  // already passed this check; the guard is what keeps that true.
  if (!validId(sessionId)) return;
  state.drafts[sessionId] = text;
  await updateRecord((record) => {
    record.drafts ||= {};
    if (text.trim()) {
      record.drafts[sessionId] = text.slice(0, 4_000);
    } else {
      delete record.drafts[sessionId];
    }
  });
}

// Renders the cached runs and marks everything on screen as stale.
//
// The rule that matters: nothing side-effecting is offered while stale. A
// queued approval or cancellation replayed on reconnect would act on a run
// whose state the device cannot see, so those controls are disabled rather
// than buffered, and the device advertises nothing until it is online again.
function showStale(record, reason) {
  const cached = record?.cache;
  state.stale = true;
  state.lastSyncAtMs = cached?.savedAtMs || null;
  state.drafts = record?.drafts || {};
  state.runs = Array.isArray(cached?.runs) ? cached.runs.filter(isRunSummary) : [];
  state.sessions = Array.isArray(cached?.sessions) ? cached.sessions.filter(isSessionSummary) : [];
  state.messages = new Map(
    Object.entries(cached?.messages || {}).map(([sessionId, messages]) => [
      sessionId,
      (Array.isArray(messages) ? messages : []).filter(isChatMessage),
    ]),
  );
  for (const [runId, events] of Object.entries(cached?.events || {})) {
    state.events.set(runId, (Array.isArray(events) ? events : []).filter(isEventEnvelope));
  }
  renderRuns();
  if (!state.sessions.some((session) => session.id === state.selectedSessionId)) {
    state.selectedSessionId = state.sessions[0]?.id || null;
  }
  renderSessions();
  renderWorkflows();
  // A cached run detail, so the offline view is a run rather than an empty
  // panel. Chosen the same way the online path chooses: the previous selection
  // if it is still visible, otherwise the first run.
  const selected = state.runs.some((run) => run.run_id === state.selectedRunId)
    ? state.selectedRunId
    : state.runs[0]?.run_id || null;
  state.selectedRunId = selected;
  const detail = selected ? cached?.details?.[selected] : null;
  ui.runPlaceholder.hidden = Boolean(detail);
  ui.runDetail.hidden = !detail;
  if (detail && isRunSummary(detail.run)) {
    renderRunDetail(detail.run, detail.spec, detail.paused === true);
    state.approvals = (cached?.approvals?.[selected] || []).filter(isApproval);
    renderApprovals();
    renderEvents();
  }
  applyStaleState();
  setConnection("error", cached ? `Offline — showing ${relativeTime(cached.savedAtMs)}` : reason);
}

function applyStaleState() {
  if (ui.staleBanner) {
    ui.staleBanner.hidden = !state.stale;
    if (state.stale) {
      ui.staleBanner.textContent = state.lastSyncAtMs
        ? `Offline. Showing what the runner said ${relativeTime(state.lastSyncAtMs)}. Actions are disabled until it is reachable again; anything you type is kept as a draft.`
        : "Offline. No cached runner state is available.";
    }
  }
  // Every control whose effect leaves this device. A draft is not one of them —
  // the composer stays usable, because typing changes nothing on the runner and
  // the text is what a person would otherwise lose.
  for (const button of [
    ui.cancelButton,
    ui.killButton,
    ui.eventsButton,
    ui.pauseButton,
    ui.resumeButton,
    ui.chatSendButton,
    ui.chatRefreshButton,
    ui.workflowRefreshButton,
    ui.captureButton,
    ui.revokeSelfButton,
    ui.artifactForm?.querySelector("button"),
  ]) {
    if (button) button.disabled = state.stale;
  }
  // A conversation cannot continue against a runner this device cannot reach,
  // and a microphone must not stay open while it tries.
  if (state.stale && talk.running) void stopTalk("Talk ended: the runner is unreachable");
  renderTalkPanel();
  for (const button of ui.approvalsList?.querySelectorAll("button") || []) {
    button.disabled = state.stale;
  }
  for (const button of ui.workflowList?.querySelectorAll("button") || []) {
    button.disabled = state.stale;
  }
}

function markOnline() {
  state.stale = false;
  state.lastSyncAtMs = Date.now();
  applyStaleState();
}

// --- Push -----------------------------------------------------------------

// Subscribes this browser to the runner's own Web Push identity.
//
// Nothing here is a third-party account: the runner hands over the public half
// of a VAPID key it generated itself, the browser's own push service issues the
// endpoint, and the runner encrypts each notification to this device before
// that service ever carries it. A 404 for the key means this runner does not do
// push, and the offer is simply not made.
async function subscribeToPush() {
  if (!("serviceWorker" in navigator) || !("PushManager" in window)) return null;
  let key;
  try {
    key = await signedRequest("GET", "/v1/remote/device/push/key");
  } catch (error) {
    if (error instanceof RemoteError && error.status === 404) return null;
    throw error;
  }
  const applicationServerKey = base64UrlToBytes(key.application_server_key);
  // Registered at the origin root, which is the scope the controller needs.
  const registration = await navigator.serviceWorker.register("/sw.js");
  await navigator.serviceWorker.ready;
  // A push subscription that shows nothing to the user is not something this
  // client asks for: `userVisibleOnly` is what makes the browser's own
  // permission prompt honest about what it is for.
  const existing = await registration.pushManager.getSubscription();
  const subscription =
    existing ||
    (await registration.pushManager.subscribe({
      userVisibleOnly: true,
      applicationServerKey,
    }));
  const raw = subscription.toJSON();
  if (!raw.endpoint || !raw.keys?.p256dh || !raw.keys?.auth) {
    throw new Error("The browser returned an incomplete push subscription");
  }
  await signedRequest("POST", "/v1/remote/device/push", {
    backend: "web_push",
    subscription: { endpoint: raw.endpoint, p256dh: raw.keys.p256dh, auth: raw.keys.auth },
  });
  return raw.endpoint;
}

// Stops this device being woken, on both ends: the browser subscription is
// dropped and the runner is told to forget the address. Either half alone would
// leave a notification path the user thought they had closed.
async function unsubscribeFromPush() {
  if ("serviceWorker" in navigator) {
    const registration = await navigator.serviceWorker.getRegistration("/sw.js");
    const subscription = await registration?.pushManager.getSubscription();
    if (subscription) await subscription.unsubscribe();
  }
  await signedRequest("DELETE", "/v1/remote/device/push");
}

function base64UrlToBytes(value) {
  const padded = String(value).replaceAll("-", "+").replaceAll("_", "/");
  const binary = atob(padded + "=".repeat((4 - (padded.length % 4)) % 4));
  const bytes = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index += 1) bytes[index] = binary.charCodeAt(index);
  return bytes;
}

async function refreshPushState() {
  if (!ui.pushButton) return;
  let subscribed = false;
  if ("serviceWorker" in navigator) {
    const registration = await navigator.serviceWorker.getRegistration("/sw.js");
    subscribed = Boolean(await registration?.pushManager.getSubscription());
  }
  state.pushSubscribed = subscribed;
  ui.pushButton.textContent = subscribed ? "Stop notifications" : "Enable notifications";
  if (ui.pushStatus) {
    ui.pushStatus.textContent = subscribed
      ? "This device can be woken for approvals and finished runs. A notification says what kind of thing happened, not what it said."
      : "Notifications are off. This device only sees updates while the controller is open.";
  }
}

async function initialize() {
  bindEvents();
  if (!requiredFeaturesAvailable()) {
    showPairing();
    ui.pairButton.disabled = true;
    setConnection("error", "Secure browser features unavailable");
    showToast("This controller requires a secure HTTPS context, WebCrypto, IndexedDB, and Web Locks.", "error");
    return;
  }
  let record;
  try {
    record = await readActiveRecord();
    if (record) validateStoredRecord(record);
  } catch (error) {
    showPairing();
    handleError(error, "Stored controller could not be resumed");
    return;
  }
  if (!record) {
    showPairing();
    return;
  }
  // Drafts before anything on the network: the text somebody typed is the one
  // thing on this screen that does not depend on the runner being reachable.
  state.drafts = record.drafts || {};
  showDashboard(record.profile);
  try {
    await refreshRuns({ preserveSelection: false });
  } catch (error) {
    // Cached runs, clearly marked, with every side-effecting control disabled —
    // never a queue of actions to replay when the runner comes back.
    showStale(record, "Runner unreachable");
    handleError(error, "Paired runner could not be reached");
    return;
  }
  try {
    await advertiseDevice();
  } catch (error) {
    handleError(error, "This device could not report what it can do");
  }
  await refreshCapabilitySurfaces();
  void refreshPushState();
  renderScreenShare();
  // Focus, visibility, a permission changed in the browser's own settings, a
  // screen share ended from its bar: every one of those changes an axis, and a
  // runner acting on a stale surface refuses what is possible or queues what
  // will fail.
  watchDeviceReadiness();
  renderTalkPanel();
  // Deliberately not awaited: the loop runs for the life of the page.
  void runCommandLoop();
}

void initialize();

const PROTOCOL_VERSION = 1;
const DB_NAME = "little-monkey-remote-v1";
const DB_VERSION = 1;
const STORE_NAME = "controllers";
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

// Physical capabilities this build can actually perform, mapped to the
// browser feature that performs them. Advertised to the runner as "supported";
// the runner intersects that with the operator's grant and the OS permission.
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

// Which Permissions API name answers for each capability, where one does.
const PERMISSION_NAMES = {
  camera_capture: "camera",
  microphone_capture: "microphone",
  // A stream is the microphone, so it is the microphone's permission.
  voice_stream: "microphone",
  location_read: "geolocation",
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
  // The command currently being performed, so a cancel can reach it.
  activeCommand: null,
  commandLoopRunning: false,
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
  constructor(message, status = 0) {
    super(message);
    this.name = "RemoteError";
    this.status = status;
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
    request.onupgradeneeded = () => {
      const database = request.result;
      if (!database.objectStoreNames.contains(STORE_NAME)) {
        database.createObjectStore(STORE_NAME, { keyPath: "id" });
      }
    };
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

async function signedRequest(method, pathAndQuery, bodyValue) {
  return navigator.locks.request(
    "little-monkey-remote-command-v1",
    { mode: "exclusive" },
    () => signedRequestExclusive(method, pathAndQuery, bodyValue),
  );
}

async function signedRequestExclusive(method, pathAndQuery, bodyValue) {
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
        const response = await fetch(pathAndQuery, {
          method,
          headers,
          body: bodyValue === undefined ? undefined : bodyText,
          cache: "no-store",
          credentials: "omit",
          redirect: "error",
          referrerPolicy: "no-referrer",
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

async function readOsPermission(capability) {
  // Screen capture has no Permissions API name: a browser asks at the moment of
  // capture and forgets afterwards, which would mean a prompt on every single
  // command. Holding one armed display stream is what replaces that — while it
  // is live the permission genuinely is granted, and the moment the user stops
  // sharing (from this page or from the browser's own bar) it genuinely is not.
  // Reporting it this way is what makes "effective" tell the truth: an unarmed
  // device is not asked to capture a screen it would have to interrupt someone
  // for.
  if (capability === "screen_capture") {
    return screenShareIsLive() ? "granted" : "undetermined";
  }
  const name = PERMISSION_NAMES[capability];
  if (!name) return "undetermined";
  if (!navigator.permissions?.query) return "undetermined";
  try {
    const status = await navigator.permissions.query({ name });
    if (status.state === "granted") return "granted";
    if (status.state === "denied") return "denied";
    return "undetermined";
  } catch {
    // A browser that does not know this permission name cannot answer for it.
    return "undetermined";
  }
}

async function describeDevice() {
  const capabilities = Object.entries(DEVICE_CAPABILITIES)
    .filter(([, supported]) => {
      try {
        return supported();
      } catch {
        return false;
      }
    })
    .map(([capability]) => capability);
  const permissions = {};
  for (const capability of capabilities) {
    const permission = await readOsPermission(capability);
    // A capability the OS cannot be asked about is reported honestly as
    // undetermined rather than optimistically as granted; the runner then
    // treats it as not effective until the device proves otherwise.
    permissions[capability] = DEVICE_CAPABILITIES[capability]() ? permission : "unsupported";
  }
  return {
    protocol_version: PROTOCOL_VERSION,
    platform: navigator.userAgentData?.platform || navigator.platform || "web",
    platform_version: String(navigator.userAgentData?.brands?.[0]?.version || "unknown"),
    app_version: "web-1",
    device_model: navigator.userAgentData?.mobile ? "mobile browser" : "browser",
    capabilities,
    permissions,
    constraints: {
      max_artifact_bytes: MAX_ARTIFACT_BYTES,
      max_recording_ms: MAX_RECORDING_MS,
      max_notification_chars: 512,
      camera_positions: DEVICE_CAPABILITIES.camera_capture() ? ["front", "back"] : [],
    },
    reported_at_ms: Date.now(),
  };
}

// Reports the surface and renders what the runner says is effective.
async function advertiseDevice() {
  const surface = await describeDevice();
  state.deviceState = await signedRequest("POST", "/v1/remote/device/surface", surface);
  renderDeviceState();
  return state.deviceState;
}

function capabilityList(values) {
  return Array.isArray(values) && values.length > 0 ? values.map(humanize).join(", ") : "none";
}

function renderDeviceState() {
  if (!ui.devicePanel) return;
  const value = state.deviceState;
  ui.devicePanel.hidden = !value;
  if (!value) return;
  // Four separate lines, never one merged list: "why can it not take a photo"
  // has four different answers and the operator has to be able to see which.
  ui.deviceGranted.textContent = capabilityList(value.granted);
  ui.deviceSupported.textContent = capabilityList(value.advertised);
  ui.deviceEffective.textContent = capabilityList(value.effective);
  const permissions = value.os_permissions || {};
  const entries = Object.entries(permissions);
  ui.devicePermissions.textContent = entries.length
    ? entries.map(([capability, permission]) => `${humanize(capability)}: ${permission}`).join(" · ")
    : "not reported";
  // The runner has just restated what this pairing holds, which is where an
  // operator's grant edit becomes visible: a withdrawn `chat` has to take the
  // composer with it rather than leaving a control that answers 403.
  renderSessions();
  renderWorkflows();
  if (ui.capturePanel) ui.capturePanel.hidden = !hasCapability("capture");
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

async function captureStill(position) {
  const stream = await navigator.mediaDevices.getUserMedia({
    video: { facingMode: position === "front" ? "user" : "environment" },
    audio: false,
  });
  try {
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
// rather than trying to reacquire — the honest report is that the permission is
// gone.

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
      // immediately, so a queued capture fails with "not permitted" instead of
      // interrupting someone with a fresh prompt.
      if (state.screenStream === stream) state.screenStream = null;
      renderScreenShare();
      advertiseDevice().catch(() => {});
    });
  }
  renderScreenShare();
  return true;
}

function disarmScreenShare() {
  stopTracks(state.screenStream);
  state.screenStream = null;
  renderScreenShare();
}

function renderScreenShare() {
  if (!ui.screenShareButton) return;
  const live = screenShareIsLive();
  const supported = DEVICE_CAPABILITIES.screen_capture();
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

async function captureScreen() {
  if (!screenShareIsLive()) {
    // Never a silent prompt: the runner was told this was not permitted, and
    // this path exists only if that report raced with the user stopping.
    throw new Error("Screen sharing is not armed on this device");
  }
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

async function recordMicrophone(durationMs) {
  const bounded = Math.min(Math.max(Number(durationMs) || 10_000, 1), MAX_RECORDING_MS);
  const stream = await navigator.mediaDevices.getUserMedia({ audio: true, video: false });
  try {
    const recorder = new MediaRecorder(stream);
    const chunks = [];
    recorder.ondataavailable = (event) => {
      if (event.data?.size) chunks.push(event.data);
    };
    const finished = new Promise((resolve) => {
      recorder.onstop = resolve;
    });
    recorder.start();
    const started = Date.now();
    // Polled rather than a single timer so an operator's cancel reaches a
    // recording already in progress instead of waiting out its full duration.
    while (Date.now() - started < bounded && !state.activeCommand?.cancelled) {
      await delay(200);
    }
    recorder.stop();
    await finished;
    const blob = new Blob(chunks, { type: recorder.mimeType || "audio/webm" });
    return {
      blob,
      mediaType: blob.type || "audio/webm",
      result: { duration_ms: Date.now() - started, cancelled: Boolean(state.activeCommand?.cancelled) },
    };
  } finally {
    stopTracks(stream);
  }
}

function readLocation(accuracy) {
  return new Promise((resolve, reject) => {
    navigator.geolocation.getCurrentPosition(
      (position) =>
        resolve({
          result: {
            latitude: position.coords.latitude,
            longitude: position.coords.longitude,
            accuracy_m: position.coords.accuracy,
            // One fix, taken now. This client never registers a watch: there is
            // no continuous background tracking to turn on.
            taken_at_ms: position.timestamp,
          },
        }),
      (error) => reject(new Error(error.message || "Location is unavailable")),
      { enableHighAccuracy: accuracy === "precise", timeout: 20_000, maximumAge: 0 },
    );
  });
}

async function postNotification(argumentsValue) {
  if (Notification.permission !== "granted") {
    const decision = await Notification.requestPermission();
    if (decision !== "granted") {
      throw new Error("The device's notification permission is denied");
    }
  }
  const notification = new Notification(String(argumentsValue.title || ""), {
    body: String(argumentsValue.body || ""),
    silent: false,
  });
  return { result: { shown: true, at_ms: Date.now() }, notification };
}

function speak(text) {
  return new Promise((resolve, reject) => {
    const utterance = new SpeechSynthesisUtterance(String(text || ""));
    utterance.onend = () => resolve({ result: { spoken: true } });
    utterance.onerror = (event) =>
      // A cancelled utterance ends with an error event; the caller decides
      // whether that was a cancellation or a failure.
      reject(new Error(event.error === "canceled" ? "Playback was cancelled" : "Playback failed"));
    window.speechSynthesis.speak(utterance);
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
async function playArtifact(runId, artifactId) {
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
    await audio.play();
    // Polled rather than awaiting `ended` alone, so a cancellation reaches a
    // long recording instead of waiting it out.
    while (!audio.ended && !state.activeCommand?.cancelled) {
      await delay(200);
    }
    if (!audio.ended) audio.pause();
    return {
      result: {
        played: audio.ended,
        artifact_id: artifactId,
        media_type: mediaType,
        bytes: artifact.size_bytes ?? null,
        cancelled: Boolean(state.activeCommand?.cancelled),
      },
    };
  } finally {
    URL.revokeObjectURL(url);
  }
}

async function playAudio(argumentsValue) {
  if (argumentsValue.artifact_id && argumentsValue.run_id) {
    return await playArtifact(argumentsValue.run_id, argumentsValue.artifact_id);
  }
  if (!window.speechSynthesis) {
    throw new Error("This device cannot speak text");
  }
  return await speak(argumentsValue.text);
}

// --- A live microphone stream ----------------------------------------------
//
// The control command stays `running` for as long as the microphone is open;
// the audio does not travel in its result but in chunks, to the session the
// command named. Two things end it: the duration it was given, and the runner
// answering `stop: true` — which arrives on the reply to a chunk this device is
// posting anyway, so a cancellation needs no second poll to be noticed.

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

async function streamVoice(argumentsValue) {
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
    while (Date.now() - started < durationMs && !stopped && !state.activeCommand?.cancelled) {
      await delay(Math.min(chunkMs, 500));
      await drain();
    }
    if (state.activeCommand?.cancelled) stopped = "Cancelled on the device";
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
async function performCommand(command) {
  const argumentsValue = command.arguments || {};
  switch (command.capability) {
    case "device_info":
      return { outcome: "succeeded", result: await describeDevice() };
    case "camera_capture": {
      const { blob, mediaType, result } = await captureStill(argumentsValue.position);
      return await withArtifact(blob, mediaType, result);
    }
    case "screen_capture": {
      const { blob, mediaType, result } = await captureScreen();
      return await withArtifact(blob, mediaType, result);
    }
    case "microphone_capture": {
      const { blob, mediaType, result } = await recordMicrophone(argumentsValue.duration_ms);
      return await withArtifact(blob, mediaType, result);
    }
    case "location_read":
      return { outcome: "succeeded", result: (await readLocation(argumentsValue.accuracy)).result };
    case "notification_post":
      return { outcome: "succeeded", result: (await postNotification(argumentsValue)).result };
    case "audio_playback":
      return { outcome: "succeeded", result: (await playAudio(argumentsValue)).result };
    case "voice_stream":
      return { outcome: "succeeded", result: (await streamVoice(argumentsValue)).result };
    default:
      // Honest refusal rather than a silent success: the runner records the
      // reason and the waiting run reads it.
      return {
        outcome: "failed",
        error: `This device build does not implement '${command.capability}'`,
      };
  }
}

async function withArtifact(blob, mediaType, result) {
  if (blob.size > MAX_ARTIFACT_BYTES) {
    return { outcome: "failed", error: "The captured artifact is larger than this device allows" };
  }
  return {
    outcome: "succeeded",
    result,
    artifact_base64: await blobToBase64(blob),
    artifact_media_type: mediaType,
  };
}

// --- The command loop ------------------------------------------------------

// Long-polls for work, performs it, and reports back.
//
// The order is the exactly-once contract: `start` is posted BEFORE anything
// physical happens, and a `started: false` reply means another connection
// already began this command — so this one performs nothing and stops. A
// command is never retried by this client; the runner decides whether a lapsed
// lease may be requeued, and it only does so before `start`.
async function runCommandLoop() {
  if (state.commandLoopRunning) return;
  state.commandLoopRunning = true;
  try {
    while (state.profile && !state.stale) {
      let command;
      try {
        command = await signedRequest("GET", `/v1/remote/device/commands/next?wait_ms=${LEASE_WAIT_MS}`);
      } catch (error) {
        if (error instanceof RemoteError && error.status === 401) throw error;
        // Any other failure is a network hiccup: wait and poll again rather
        // than tearing the session down.
        await delay(5_000);
        continue;
      }
      if (!command || !validId(command.command_id)) continue;
      await executeLeasedCommand(command);
    }
  } catch (error) {
    handleError(error, "Device command loop stopped");
  } finally {
    state.commandLoopRunning = false;
  }
}

async function executeLeasedCommand(command) {
  const recent = await rememberCommand(command.command_id);
  if (recent) {
    // This device already performed this command in an earlier session. Report
    // the remembered outcome instead of doing it again.
    await reportCommand(command.command_id, recent);
    return;
  }
  if (command.cancel_requested) {
    await reportCommand(command.command_id, { outcome: "cancelled", error: "Cancelled before it started" });
    return;
  }
  let started;
  try {
    started = await signedRequest("POST", `/v1/remote/device/commands/${encodeURIComponent(command.command_id)}/start`, {});
  } catch (error) {
    handleError(error, "The device command could not be started");
    return;
  }
  if (started.started !== true) {
    // Already running elsewhere (or on this device before a reconnect). Doing
    // it again would take a second photograph.
    return;
  }
  state.activeCommand = { commandId: command.command_id, capability: command.capability, cancelled: false };
  showToast(`Running ${humanize(command.capability)} for the runner…`);
  let report;
  try {
    report = await performCommand(command);
  } catch (error) {
    report = { outcome: "failed", error: String(error?.message || error) };
  } finally {
    state.activeCommand = null;
  }
  await rememberCommand(command.command_id, report);
  await reportCommand(command.command_id, report);
}

async function reportCommand(commandId, report) {
  try {
    await signedRequest("POST", `/v1/remote/device/commands/${encodeURIComponent(commandId)}/result`, {
      protocol_version: PROTOCOL_VERSION,
      outcome: report.outcome,
      result: report.result ?? null,
      artifact_base64: report.artifact_base64 ?? null,
      artifact_media_type: report.artifact_media_type ?? null,
      error: report.error ?? null,
    });
  } catch (error) {
    handleError(error, "The device result could not be delivered");
  }
}

// --- Bounded local caches --------------------------------------------------

// Remembers what this device already did, so a command that survives a browser
// restart is reported rather than performed twice. Bounded to the most recent
// 50 commands; older ones cannot recur, because the runner expires them long
// before that.
async function rememberCommand(commandId, report) {
  const database = await openDatabase();
  try {
    return await new Promise((resolve, reject) => {
      const transaction = database.transaction(STORE_NAME, report ? "readwrite" : "readonly");
      const store = transaction.objectStore(STORE_NAME);
      const request = store.get(ACTIVE_RECORD);
      let found = null;
      request.onsuccess = () => {
        const record = request.result;
        if (!record) return;
        record.commandResults ||= {};
        if (report) {
          // The artifact bytes are deliberately not remembered — only the
          // outcome. Re-reporting a cached success without its artifact is
          // honest and bounded; caching megabytes of stills is not.
          record.commandResults[commandId] = {
            outcome: report.outcome,
            result: report.result ?? null,
            error: report.error ?? null,
            atMs: Date.now(),
          };
          const entries = Object.entries(record.commandResults).sort((a, b) => b[1].atMs - a[1].atMs);
          record.commandResults = Object.fromEntries(entries.slice(0, 50));
          store.put(record);
        } else {
          found = record.commandResults[commandId] || null;
        }
      };
      request.onerror = () => reject(request.error || new Error("The command cache could not be read"));
      transaction.oncomplete = () => resolve(found);
      transaction.onerror = () => reject(transaction.error || new Error("The command cache failed"));
      transaction.onabort = () => reject(transaction.error || new Error("The command cache was aborted"));
    });
  } finally {
    database.close();
  }
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
  // Deliberately not awaited: the loop runs for the life of the page.
  void runCommandLoop();
}

void initialize();

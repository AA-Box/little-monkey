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
  audio_playback: () => Boolean(window.speechSynthesis),
  // Reserved for the Talk surface; this build has no stream implementation, so
  // it is never advertised and therefore never effective.
  voice_stream: () => false,
};

// Which Permissions API name answers for each capability, where one does.
// Screen capture has none by design — the OS asks at the moment of capture —
// so it is reported undetermined until a capture proves otherwise.
const PERMISSION_NAMES = {
  camera_capture: "camera",
  microphone_capture: "microphone",
  location_read: "geolocation",
};

const PAIRING_URI_SCHEME = "littlemonkey://pair/";
// How long one lease long-poll waits. Just inside the runner's 30 s lease so
// a reply is never cut off mid-flight by the server's own deadline.
const LEASE_WAIT_MS = 25_000;
// Bounds on what this client will do regardless of what it is asked, so a
// runner that has been tampered with cannot hold the camera open.
const MAX_RECORDING_MS = 300_000;
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
  toastTimer: null,
  activeRequests: 0,
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
    },
    key,
    nextSequence: 1,
    eventCursors: {},
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
  renderDeviceState();
  setConnection("online", "Paired; checking runner…");
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
  state.selectedRun = value.run;
  ui.detailRunId.textContent = value.run.run_id;
  ui.detailStatus.textContent = humanize(value.run.status);
  ui.detailStatus.dataset.status = value.run.status;
  ui.specJson.textContent = JSON.stringify(value.spec, null, 2);
  renderFacts(value.run);
  ui.cancelButton.hidden = !hasScope("cancel") || TERMINAL_STATUSES.has(value.run.status);
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
  renderApprovals();
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

async function captureScreen() {
  const stream = await navigator.mediaDevices.getDisplayMedia({ video: true, audio: false });
  try {
    return await frameFromStream(stream, "image/png", undefined);
  } finally {
    stopTracks(stream);
  }
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
      return { outcome: "succeeded", result: (await speak(argumentsValue.text)).result };
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
// Bounded to 50 runs, and never anything that could be replayed as an action.
async function cacheRuns(runs) {
  await withStore("readwrite", (store, transaction) => {
    const request = store.get(ACTIVE_RECORD);
    request.onsuccess = () => {
      const record = request.result;
      if (!record) return;
      record.cache = { runs: runs.slice(0, 50), savedAtMs: Date.now() };
      store.put(record);
    };
    request.onerror = () => transaction.abort();
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
  state.runs = Array.isArray(cached?.runs) ? cached.runs.filter(isRunSummary) : [];
  renderRuns();
  applyStaleState();
  setConnection("error", cached ? `Offline — showing ${relativeTime(cached.savedAtMs)}` : reason);
}

function applyStaleState() {
  if (ui.staleBanner) {
    ui.staleBanner.hidden = !state.stale;
    if (state.stale) {
      ui.staleBanner.textContent = state.lastSyncAtMs
        ? `Offline. Showing what the runner said ${relativeTime(state.lastSyncAtMs)}. Actions are disabled until it is reachable again.`
        : "Offline. No cached runner state is available.";
    }
  }
  // Every control whose effect leaves this device.
  for (const button of [ui.cancelButton, ui.killButton, ui.eventsButton, ui.artifactForm?.querySelector("button")]) {
    if (button) button.disabled = state.stale;
  }
  for (const button of ui.approvalsList?.querySelectorAll("button") || []) {
    button.disabled = state.stale;
  }
}

function markOnline() {
  state.stale = false;
  state.lastSyncAtMs = Date.now();
  applyStaleState();
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
  // Deliberately not awaited: the loop runs for the life of the page.
  void runCommandLoop();
}

void initialize();

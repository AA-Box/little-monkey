# Remote PC Control — Design and Threat Model (Research)

**Status:** Research (ROADMAP.md, Phase 5 §"Remote PC Control"). This document is the
entire deliverable for that roadmap item at its current status. It contains **no
runtime code, no new dependency, and no change to `src-tauri/src/lib.rs` or any
command registration** — by design. A "Research" item is a place to write down
what we believe and what we still don't know, not a place to ship a feature.

## 0. Roadmap framing and hard dependency

ROADMAP.md is explicit about ordering:

> **Remote PC Control** — Status: Research. "Only after secure pairing and local
> PC control are proven."

That sentence names two prerequisites, both of which are themselves unproven today
in this codebase:

1. **Secure pairing** — ROADMAP.md Phase 4, "Mobile-to-Homelab Pairing and Model
   Sharing," is **Status: Planned**, not built. There is no code implementing
   *that specific* model: no mutual device identity with pinned keys, and no
   per-device grant table matching Phase 4's enum (chat, model inference, view
   tasks, approve actions, run workflows, read artifacts, admin).

   That is narrower than "no pairing code exists in this repo." A real, running
   pairing-code/scoped-token/revocation system already exists in
   `src-tauri/src/compatibility_hub.rs` — `begin_pairing`/`complete_pairing`
   (lines 1900, 1956), `PairingRequest`/`ScopedTokenView` (lines 1685, 1711),
   and `revoke_token`/`revoke_all_tokens` (lines 2167, 2207) — used to authorize
   LAN clients of the OpenAI/Anthropic-compatible API surface
   (`m3_http_server.rs`, `m3_runtime_hub.rs`). It issues short-lived pairing
   codes, rate-limits guesses (`MAX_PAIRING_ATTEMPTS`), stores codes and tokens
   as SHA-256 digests rather than plaintext, and supports per-token revocation
   with an audit trail (`SecurityAuditKind::PairingStarted`/`TokenRevoked`). An
   earlier pass over this document reported grepping for
   `device`/`pairing`/`grant` and finding nothing but `browser_worker.rs`'s
   unrelated per-run browser grants — that finding was wrong;
   `compatibility_hub.rs`'s pairing/token code is real and should have
   surfaced. It still does not satisfy Phase 4, though: its `ApiScope` enum
   (chat completions, responses, messages, model discover/download/load/
   unload/delete/status) authorizes *API-surface access*, not a physical
   device with a pinned key, and it has no "device" concept independent of a
   single revocable token — so Phase 4's device-identity/per-device-grant
   model is still Planned, not built. But `compatibility_hub.rs`'s
   pairing-code-challenge-then-revocable-scoped-token shape is closer, more
   literal in-repo precedent than `browser_worker.rs`'s grants, and future
   Phase 4 (and by extension Remote PC Control) work should look at it first.
2. **Local PC control** — ROADMAP.md Phase 5, "Safe Desktop Control," is also
   **Status: Research**, and is being explored as a brand-new, unproven day-one
   spike in a sibling worktree in this same batch. It has no shipped usage yet.

**Dependency statement:** This roadmap item cannot honestly move from "Research"
to "Planned" until:

- Safe Desktop Control has shipped, been used in real sessions (not just a
  design doc or a demo build), and produced evidence about what its approval UX,
  logging shape, and emergency-stop behavior actually look like under real
  screen/input automation — the artifacts this document leans on (session logs,
  screenshot evidence, revocation semantics) are inherited from that feature, and
  we cannot design a remote-approval layer on top of a local-control primitive
  whose real shape is still a guess; and
- Mobile-to-Homelab Pairing and Model Sharing has shipped a device-identity,
  grant, and revocation model that this document can build on rather than
  reinvent. This document deliberately reuses that item's vocabulary (see §1)
  precisely so that when pairing ships, Remote PC Control does not need a second,
  incompatible device-trust system.

Writing remote-control runtime code before both of those exist would mean
building a remote-approval and replay-protection layer on top of primitives that
don't exist yet, and then re-deriving that layer twice more when the real shape
of local control and pairing lands. That is the concrete cost the roadmap's
sequencing rule is protecting against, and this document does not attempt to get
around it.

## 1. Vocabulary reuse (from ROADMAP.md Phase 4)

To avoid inventing a second, incompatible trust model, Remote PC Control reuses
these terms exactly as Phase 4's "Mobile-to-Homelab Pairing and Model Sharing"
item defines them:

| Term | Phase 4 definition (reused, not redefined) |
|---|---|
| **Device identity** | Mutual device identity with pinned keys, established via LAN QR code, Tailscale/ZeroTier address, SSH reverse tunnel, or user-provided HTTPS endpoint, plus a short-lived pairing code. |
| **Per-device grant** | One of: chat, model inference, view tasks, approve actions, run workflows, read artifacts, admin. Remote PC Control adds no new grant vocabulary — see §2 for why "control-pc" must slot into this same enum rather than becoming a special case. |
| **Revocation** | Revoking a paired device immediately prevents new actions (Phase 4 acceptance criterion). Remote PC Control's kill switch (§3.3) is this same revocation primitive, not a separate one. |
| **Key rotation, replay protection, audit log** | Named as required Phase 4 capabilities ("Revocation, key rotation, replay protection, and audit logs for every remote action"). Remote PC Control's session-recording and replay-protection requirements (§3) are the PC-control-specific instance of these, not new primitives. |
| **Remote action record shape** | Phase 4's acceptance criterion: "Every remote action has device id, user-visible capability, timestamp, digest, and result." Any remote-control input event must satisfy this same shape — see §3.2. |

The one existing repo pattern this document also treats as load-bearing
precedent is the two-phase mutation gate in `src-tauri/src/m5_delivery/mod.rs`:
`prepare_mutation_impl` (line 627) computes a SHA-256 digest of the mutation
(`digest_bytes`, line 448) and returns a `ConfirmationPreview` whose
`confirmation_phrase` is `CONFIRM {first 12 hex chars of the digest}`
(`confirmation_phrase`, line 452); `execute_mutation_impl` (line 663) then
rejects execution unless the caller echoes back that exact phrase (line 670).
Any future "arm remote control for this session" or "grant remote approval for
this action" command must mirror this exact shape — prepare a digest of what is
about to happen, require the caller to type/echo a short phrase derived from
that digest, only then execute — rather than inventing a different confirmation
convention. This matters more for remote control than it does for git delivery:
a remote party approving "click here" or "press this key" needs the same
tamper-evident binding between *what was previewed* and *what gets executed*
that a code mutation gets.

A second existing repo pattern worth citing alongside it: `compatibility_hub.rs`
(see §0) already implements a pairing-code-challenge issued by the host
(`begin_pairing`), confirmed by a short numeric code from the requesting side
(`complete_pairing`), producing a scoped, revocable, digest-stored credential
(`revoke_token`). That is a real, in-repo instance of exactly the
"host-issues-challenge, requester-confirms-code, grant-is-independently-
revocable" shape Phase 4 (and this document's §2/§3.3) needs — narrower in
scope (API-surface access, not device identity) but structurally the closest
precedent available today, closer than the m5_delivery digest pattern above.

## 2. Pairing architecture (built on Phase 4, not restated)

This section describes how Remote PC Control would sit on top of Phase 4's
pairing model once that model exists. It does not describe new pairing
mechanics.

- **No new device-trust system.** A remote controller (a paired mobile device,
  or a second desktop) authenticates using the exact same mutual
  device-identity/pinned-key/pairing-code flow Phase 4 defines. Remote PC
  Control does not get its own pairing UX.
- **A new grant value, not a new grant system.** Phase 4 lists grants as: chat,
  model inference, view tasks, approve actions, run workflows, read artifacts,
  admin. Remote PC Control needs a `control-pc` (or equivalently-named) grant
  added to that same enum — deliberately the most sensitive value ROADMAP.md
  Phase 4 lists it alongside `admin`, since it authorizes physical input
  injection on the host, not just data access. This grant must be:
  - off by default for every device, including devices already holding
    `admin` on other grants (privilege must not be implied — see §3.1);
  - grantable only from the host machine's own local UI (the machine being
    controlled must be the one that says yes to being controlled — a remote
    party cannot self-grant `control-pc` on itself);
  - subject to the same revocation-takes-effect-immediately acceptance bar
    Phase 4 sets for every other grant.
- **Homelab-node topology reused, not re-derived.** Phase 4 anticipates LAN,
  Tailscale/ZeroTier, SSH reverse tunnel, or user-supplied HTTPS as the
  transport. Remote PC Control's own acceptance bar — "No hosted relay by
  default" (ROADMAP.md) — is a stronger version of the same idea: whichever of
  those four transports Phase 4 ships, Remote PC Control must not introduce a
  fifth "Little-Monkey-hosted relay" path as a shortcut. A hosted relay changes
  the threat model from "attacker controls one endpoint's network" to "attacker
  who compromises our relay controls every paired session," which is a
  categorically worse blast radius for a capability that injects physical
  input.
- **Session key scope, not identity scope.** A remote-control session should
  mint its own short-lived session key, bound to the specific local
  "Control PC" session (ROADMAP.md, Safe Desktop Control) it's allowed to drive
  — not to the device's long-lived pairing identity. This means revoking one
  remote-control session cannot be conflated with revoking the paired device
  entirely; the two must be independently revocable (see §3.3).

## 3. Threat model

### 3.1 Remote approval flow

**What could go wrong:**

- A device that legitimately holds `approve actions` or `admin` on chat/tasks
  gets treated as if that implies `control-pc`. **Mitigation:** `control-pc` is
  its own explicit grant (§2); no grant implies another. Enforcement code must
  check `control-pc` explicitly, never fall through from `admin`.
- A remote party approves a *description* of an action ("click Submit") but the
  action actually executed differs from what was previewed (a classic
  confused-deputy / TOCTOU gap). **Mitigation:** reuse the digest-then-confirm
  pattern (§1) — the remote approver is shown a digest of the exact
  input-event sequence about to run, and execution is refused unless the
  confirmation matches that digest. This closes the same gap
  `m5_delivery`'s `execute_mutation_impl` closes for code mutations
  (line 663–670: confirmation must equal `confirmation_phrase(&digest)`).
- Approval fatigue: if every mouse move needs a remote round-trip, users will
  start rubber-stamping, defeating step-by-step approval. **Open question**,
  not solved here — see §5.
- A remote approver's own device is compromised or its session hijacked mid-flow
  (e.g., stolen mobile session token). **Mitigation direction:** short-lived
  session keys (§2) plus the replay protection in §3.4; full mitigation depends
  on Phase 4's key-rotation mechanics, which don't exist yet.

**Non-negotiable acceptance bar (from ROADMAP.md):** "Remote control cannot
start without local visible consent." This means the remote approval flow is
additive on top of local consent, never a substitute for it — a remote party
saying yes is never sufficient by itself to start a control-pc session; the
physical machine must also show a visible, local, in-person indicator (per Safe
Desktop Control's "visible capture indicator" and "Control PC session"
requirements) before any remote-originated input is injected.

### 3.2 Session recording

**What could go wrong:**

- Recording is turned off or tampered with mid-session, leaving a gap in the
  audit trail exactly when something goes wrong. **Mitigation direction:**
  recording start/stop must itself be an audited, device-attributed action
  (same record shape as Phase 4's "every remote action has device id,
  user-visible capability, timestamp, digest, and result" — §1), so a gap is
  visible after the fact even if it can't always be prevented in real time.
- Recordings (screenshots/video) are themselves sensitive — they can capture
  passwords typed elsewhere, private messages, unrelated open windows.
  **Mitigation direction:** session recording for *remote* control should
  inherit whatever redaction/consent boundary Safe Desktop Control establishes
  for *local* control (visible capture indicator, no capture during password
  dialogs / OS security prompts per that item's acceptance criteria) — remote
  control must not be a way to get a weaker recording policy than local control
  already has.
- Recordings stored insecurely become a second attack surface (an attacker who
  can't get live control access instead steals the recording archive).
  **Mitigation direction:** treat recording archives as sensitive
  workspace/audit data subject to the same at-rest handling the rest of the app
  uses for other audit stores — atomic writes, no secrets embedded in the
  recording metadata — but this repo has no session-recording storage module
  today to point to as precedent, so the exact storage shape is an open
  question (§5), not something this doc invents.

**Non-negotiable acceptance bar (from ROADMAP.md):** "Session recording or
screenshot evidence" is a required capability, not optional — a remote-control
session without evidence is not shippable under this roadmap item.

### 3.3 Device-grant revocation ("kill switch")

**What could go wrong:**

- Revocation is checked only at session start, so a device already mid-session
  keeps acting after being revoked. **Mitigation:** revocation must be checked
  continuously (e.g., per input-event batch, not just at session establishment)
  — this is what ROADMAP.md's "Revocation and kill switch work mid-session"
  acceptance criterion is actually testing for, and it is a stronger bar than
  Phase 4's own "revoking a paired device immediately prevents new actions"
  (which is phrased around *new* actions, not *in-flight* ones).
- Two distinct revocation targets get conflated: revoking the *device's pairing
  identity* (Phase 4's mechanism) vs. revoking one *active control-pc session*
  on an otherwise-still-paired device (this item's kill switch). A user hitting
  "stop this session" should not require or imply un-pairing the whole device,
  and un-pairing a device must always also kill any of its live sessions.
  **Mitigation direction:** model these as two independently-checkable states
  (per §2's "session key scope, not identity scope"), with revocation of the
  broader identity always cascading to kill the narrower session, but not
  vice versa.
- The local emergency-stop hotkey (Safe Desktop Control) and the remote kill
  switch (this item) must be the *same* underlying stop primitive, checked from
  two input paths, not two separate stop mechanisms that could disagree about
  whether control is actually released. **Mitigation direction:** whichever
  session-state store Safe Desktop Control introduces for its local "Control PC
  session" must be the single source of truth both stop paths write to; this
  document does not invent a second one.

### 3.4 Replay protection

**What could go wrong:**

- A captured remote-control message (an approved click/keystroke batch, or an
  approval confirmation phrase) is replayed later to re-execute an action the
  user only meant to approve once. **Mitigation direction:** every remote
  action must be bound to a single-use nonce or monotonic session sequence
  number in addition to the content digest, and the receiving side must reject
  any message whose nonce/sequence it has already seen — this is the "replay
  protection" line item Phase 4 already names as required infrastructure (§1);
  Remote PC Control does not need its own replay-protection design, it needs
  Phase 4 to ship one and then to bind control-pc's approval messages to it.
- Network-layer replay across reconnects (e.g., a dropped Tailscale link
  reconnecting and re-sending a buffered message). **Open question** — depends
  on which of Phase 4's transports (LAN, Tailscale/ZeroTier, SSH tunnel,
  user HTTPS) end up implemented and what replay guarantees each one's own
  transport layer already provides versus what the app must add itself (§5).

## 4. What this document is not

- It is not an implementation plan. No file layout, no Tauri command names, no
  data schema is proposed, because those decisions depend on choices Phase 4
  and Safe Desktop Control haven't made yet (§0).
- It does not add, modify, or reference any Cargo dependency, npm package, Rust
  module, or TS file beyond citing existing code as precedent (§1, §2). No file
  under `src-tauri/src/` or `src/` is touched by this change.
- It is not a green light. §0's dependency statement stands: this item stays at
  "Research" until the two prerequisites it names have shipped and been used.

## 5. Open questions (must be answered before any runtime code)

1. **Grant granularity.** Is `control-pc` one grant, or does it need
   sub-scopes (e.g., "view screen remotely" vs. "inject input remotely") so a
   remote party can watch without being able to act? Phase 4's grant list
   doesn't currently distinguish read vs. write within a single capability
   name the way it does across different grants (e.g., "view tasks" vs.
   "approve actions" are already separate) — should control-pc follow that
   same split?
2. **Approval granularity vs. approval fatigue.** Safe Desktop Control's
   default is step-by-step local approval. Does remote control get its own,
   possibly coarser, approval granularity (e.g., approve-per-task rather than
   approve-per-click), and if so, does that create a weaker security posture
   for remote sessions than local ones — is that acceptable, or must remote
   always be *at least as* strict as local?
3. **What "no hosted relay by default" allows as an opt-in exception.** The
   acceptance criterion says "by default" — implying an opt-in relay path may
   exist later. What would have to be true (e.g., end-to-end encryption the
   relay operator cannot decrypt, explicit per-use consent) before a relay
   option could be considered safe enough to offer as opt-in, and who signs off
   on that bar being met?
4. **Recording storage and retention.** Where do session recordings live
   (app-data dir per the existing `data_dir()` convention in
   `src-tauri/src/app_paths.rs`, homelab node, neither), who can delete them,
   and how long are they retained? This has real disk-space and privacy-policy
   implications once recordings routinely include screen content.
5. **Cross-device clock / ordering trust.** Replay protection via sequence
   numbers assumes both sides agree on session ordering. What happens on
   reconnect after a dropped transport link — does the session sequence
   resume, or must every reconnect force a fresh pairing-level handshake?
6. **Interaction with Privacy Firewall (Phase 5).** If a remote-control session
   is driving the desktop through screens that contain secrets/PII, does the
   Privacy Firewall item's redaction apply to session recordings the same way
   it would apply to a cloud-bound prompt? These two Phase 5 items are being
   scoped independently but touch the same screen-content surface.
7. **Multi-controller conflicts.** Can more than one paired device hold
   `control-pc` at once, and if two remote controllers are active, who wins on
   a conflicting input, or is the grant exclusive-lock by design (only one
   active controller at a time)?
8. **What "kill switch" restores.** After a kill switch fires mid-session, does
   the local machine return to whatever an in-flight action left it in (e.g., a
   half-typed form, an open dialog), or is there an expectation of some
   rollback/notification behavior? Safe Desktop Control's own local emergency
   stop needs to answer this first; remote control inherits whatever the answer
   is.
9. **Auditor UX.** Phase 4's acceptance bar requires "every remote action has
   device id, user-visible capability, timestamp, digest, and result" — where
   does a user actually go to read that audit trail for control-pc sessions
   specifically (a dedicated panel, or folded into whatever surfaces Phase 4's
   general remote-action audit log)? This is a product question that shouldn't
   be answered by whichever engineer happens to build the storage layer first.

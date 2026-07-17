# Safe Desktop Control — design & threat model (research spike)

**Status:** Research (ROADMAP.md Phase 5, "Trust, Sandboxing, and PC Control" →
"Safe Desktop Control"). This document, `src-tauri/src/desktop_control.rs`,
`src/store/desktopControlStore.ts`, and `src/components/Settings/DesktopControlPanel.tsx`
are a design-validation spike: real, working, gated, and off by default — not
a finished production feature. Nothing here is exposed to the model as an
agent tool; it is a user-driven Settings surface only, exactly like the M7
companion's capture grants.

## Why this exists

Every other mutating capability in Little Monkey (`write_file`, `edit_file`,
`run_shell`, MCP tool calls) stays inside the process: it touches the
workspace or a sandboxed shell, and the existing permission system
(`src-tauri/src/permissions.rs`) gates it. Controlling the mouse and keyboard
is categorically different — a granted action can do *anything a human at the
keyboard could do*, in *any* application, including ones Little Monkey has no
visibility into (password managers, banking apps, other people's chat
windows, OS security dialogs). That blast radius is why this spike exists
separately from `tools.rs` rather than as one more tool in the existing list,
and why it ships behind more gates than anything else in the app.

## Threat model

**Attacker goal.** A compromised prompt, a malicious tool result (e.g. a web
page or MCP server response containing injected instructions), or a bug in
the agent loop tries to get Little Monkey to move the mouse or send
keystrokes somewhere the user did not intend — to click "Confirm" on a
payment, dismiss a security prompt, type into a password field, approve an
OAuth grant, or drive another application into a bad state.

**Attacker capability, assumed worst case.** The attacker fully controls the
*content* the model sees (any file, web page, MCP response, or connector
payload) and can therefore fully control what the model *says it wants to
do*, including fabricating justification text, fake "user already approved
this" claims, or urgency framing. The attacker cannot forge Tauri IPC calls
directly (that would require code execution in the Rust process, a different
and much larger threat) — the threat is entirely "the model was talked into
requesting a bad action," not "the sandbox was broken into."

**What this design must prevent, even under that worst case:**

1. The feature must never be reachable from an unattended/headless run.
   Automations, scheduled tasks, and any turn running under
   `permission_mode == "bypass"` must never be able to start a control
   session — not "prompt anyway," an outright `Err`. See the hard invariant
   in `desktop_control_start_session` below.
2. A session must never be silently broad. It is scoped to an explicit,
   non-empty allowlist of application/window identifiers the user typed in
   themselves; an action against any other target is rejected before it ever
   reaches the input backend.
3. No single action executes without either (a) a human clicking "Approve"
   for that specific action, or (b) the user having explicitly put the
   session into "approved batch" mode — itself a deliberate, visible,
   revocable opt-in for a bounded session, not a way to skip approval
   forever.
4. There must always be a working, idempotent kill switch that a user (not
   the model) can hit, and it must be wired into the same process-exit
   shutdown path as every other capability that can leave something running
   (`m7_companion`'s capture grants, the M3/M4 job managers, the browser
   worker) — see `lib.rs`'s `RunEvent::Exit` handler.
5. There must be a visible, persistent, on-screen indicator whenever a
   session is active, so a user glancing at their screen — not just one
   reading Settings — knows control is live.

## Mitigations, mapped to the threat model

| Threat-model item | Mitigation | Where |
| --- | --- | --- |
| Reachable from bypass/unattended runs | `desktop_control_start_session` reads the *current* `permission_mode` (`permissions::get_permission_mode`) and returns `Err` outright when it is `"bypass"` — no override, no exception, checked on every single session start, not just once at boot | `DesktopControlState::start_session_impl` |
| Silently broad scope | `allowed_applications: Vec<String>` is required and non-empty at session start (fails closed); every action names a `target_application_id` checked against that exact list before dispatch | `DesktopControlState::require_active_session` |
| Action executes without a human decision | Default mode gates every `MouseMove`/`MouseClick`/`KeyPress` behind a `PendingControlAction` + `oneshot::Sender<bool>`, resolved only by an explicit `desktop_control_respond_action` call from the Settings UI — mirrors `permissions.rs`'s request/resume shape exactly | `DesktopControlState::begin_action` / `resolve_if_pending` |
| No kill switch / stale sessions | `desktop_control_emergency_stop` deactivates every session and denies every pending action in one call, is idempotent (second call is a no-op returning zero counts), and is invoked from `lib.rs`'s `RunEvent::Exit` handler alongside `m7_companion`'s own emergency stop | `DesktopControlState::emergency_stop`, `lib.rs` |
| No visible indicator | Starting a session shows the existing always-on-top `companion-overlay` window (`m7_companion::show_overlay`); stopping every session or hitting emergency stop hides it again | `desktop_control_start_session` / `desktop_control_stop_session` / `desktop_control_emergency_stop` |
| Session running forever unattended | Every session carries `expires_at_ms` (max 30 minutes per start, mirroring `m7_companion`'s `MAX_GRANT_LIFETIME_MS` pattern); an expired session is treated as inactive the moment it's next read, action attempts against it fail closed | `DesktopControlState::require_active_session` / `sessions_snapshot` |

## Non-goals (explicitly deferred, not half-implemented)

- **Content-based dialog detection.** Detecting "this is a password field" or
  "this is a payment confirmation dialog" by inspecting on-screen content is
  an open, unsolved, cross-platform problem (accessibility trees are
  inconsistent across apps, OCR is unreliable and slow, and a heuristic here
  would give false confidence). This spike does **not** attempt it. The
  ROADMAP's "blocked on password dialogs, payment flows, OS security prompts"
  acceptance line is satisfied today only by the allowlist + per-action
  approval + visible indicator, not by any content inspection — a human still
  has to look at what they're approving. A future phase could add
  accessibility-tree-based heuristics as an additional (never sole) layer,
  but that is out of scope here.
- **Windows/Linux input backends.** Only macOS gets a real `enigo`-backed
  input path in this spike (`#[cfg(target_os = "macos")]`). Every other
  platform compiles and registers the same commands, but every action
  returns a clear "unsupported on this platform" error rather than silently
  no-op-ing or crashing. Extending real input simulation to other platforms
  is future work once the macOS path is proven.
- **Remote/paired-device control.** Explicitly a separate, later ROADMAP item
  ("Remote PC Control", still Research) that depends on this one shipping
  first. Not touched here.
- **Screen OCR / vision-based verification of what an action actually did.**
  This spike executes actions and reports whether the backend call itself
  succeeded; it does not screenshot-verify the visual outcome. `m7_companion`
  already owns screen capture under its own grant system — a future phase
  could compose the two, but this spike does not.
- **An agent-callable tool.** Nothing here is added to `tools.rs`'s `TOOLS`
  list or `agentLoop.ts`'s tool dispatch. It is reachable only from the
  Settings panel a human opens themselves, exactly like `CompanionPanel`'s
  capture grants — there is currently no path from "the model decides to
  move the mouse" to this code at all. Making it agent-callable, if that's
  ever wanted, is a distinct future decision that should re-run this threat
  model from scratch, since it changes the attacker's capability from
  "influence what a human approves" to "directly request actions."

## What "approved batch" mode is (and is why it doesn't defeat the model above)

A session started with `approved_batch: true` skips the per-action approval
prompt for the duration of that one session. It exists so a user doing many
small clicks in the allowlisted app doesn't have to click "Approve" a hundred
times. It is **not** a way to run unattended:

- It still requires an explicit, human-initiated `desktop_control_start_session`
  call (never reachable from bypass mode, same as normal mode).
- It is still scoped to the same non-empty application allowlist.
- The visible on-screen indicator is still shown for the whole session.
- `desktop_control_emergency_stop` still immediately kills it.
- It is bounded by the same ≤30-minute session expiry as normal mode.

In other words: "approved batch" changes *how many times* a human has to
click something before a session starts working, never *whether* a human
had to act at all, and never widens the allowlist or removes the kill switch.

## Frontend default

`desktopControlEnabled` in `src/store/settingsStore.ts` defaults to `false`,
the same "disabled = not offered" posture as `subagentsEnabled` and
`skillAutoInvokeEnabled`. `DesktopControlPanel.tsx` shows only an explanatory
card and an opt-in toggle until the user turns it on; session controls,
the allowlist editor, and the emergency-stop button only render once it is
enabled.

## Testing posture

No automated test in this spike drives real OS mouse/keyboard input — every
Rust test exercises `DesktopControlState` against `NullBackend` (a
no-op `DesktopInputBackend` implementation) and asserts on session/allowlist/
approval/emergency-stop *logic*, never on anything the OS actually sees. See
`src-tauri/src/desktop_control.rs`'s `#[cfg(test)] mod tests` for the exact
cases: bypass-mode refusal, allowlist enforcement, emergency-stop idempotency,
and the pending-action oneshot resume path.

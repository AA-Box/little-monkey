# Roadmap

Future product work only.

**This file must not accumulate "Done" items.** When something ships, it moves
to [README.md](README.md) as a plain description of what the app does — with
its real limits stated there rather than a checkmark here. If you are looking
for what Little Monkey already does, the README is the answer; everything
below is explicitly *not built yet*.

Each entry states the acceptance boundary that would let it move to the
README. "Partially built" entries name the shipped part honestly, because a
half-built feature that the README already describes is still roadmap work
for the remainder — not a done item.

For the same work viewed as OS subsystems — process model, enforced isolation,
scheduling, and a versioned platform contract — see
[docs/agent-os-roadmap.md](docs/agent-os-roadmap.md). It cross-references the
items below rather than duplicating them, and adds the kernel-level gaps that
have no entry here.

---

## 1. Policy-driven model routing

**Today:** a single hardcoded fallback toggle. Provider failover follows a
fixed sequence; there is no user-defined policy of any kind.

**Acceptance:** a user can author named routing policies (by task class,
cost ceiling, latency target, data sensitivity, or tool requirements),
inspect which policy chose a given turn's target and why, and reorder or
disable policies without editing code. A policy can never widen a permission
or bypass the Privacy Firewall.

## 2. Real benchmarking

**Today:** the Quantization workbench performs genuine conversions and
records reproducible reports, and the Runtime Hub shows honest hardware
detection. The "benchmark" surface, however, measures nothing — edge device
profiles are static prose, not measurements.

**Acceptance:** a benchmark run produces measured tokens/sec, time-to-first
token, and peak memory for a specific model + runtime + quantization on
*this* machine, with variance across repeated runs reported and the hardware
snapshot attached. No number is displayed that was not measured on the
machine displaying it.

## 3. Prompt and workflow version control

**Today:** last-write-wins everywhere. Only marketplace packages have a diff
view.

**Acceptance:** prompts, personas, skills, and workflow definitions keep a
local revision history with diff, restore, and branch/compare; a concurrent
edit is detected and surfaced rather than silently overwritten.

## 4. Cost attribution and budget enforcement *(partially built)*

**Shipped:** per-request cost recording against user-entered rates, daily and
monthly budgets with a warn/pause enforcement mode, and a live budget check
before every provider request (**Settings → Usage**).

**Remaining:** per-workspace and per-project attribution, multi-tier warning
thresholds, and honest handling of providers whose real billing differs from
the user's entered rate (the app cannot see actual invoices and must not
imply that it can).

## 5. Mobile companion — remaining gaps *(partially built)*

**Shipped:** a real React Native/Expo app
([little-monkey-mobile](https://github.com/AA-Box/little-monkey-mobile))
with pinned-TLS pairing (QR scan or paste), run/event/approval/artifact
browsing, and a node-side `/v1/remote/mobile/*` extension serving chat
sessions and messages, saved-workflow launch, capture upload, and device
self-revocation — each behind its own explicit pairing capability, so a
runner-only pairing can never reach them.

**Remaining:**

- **Offline mode.** Captures queue offline today, but browsing is
  online-only. Acceptance: recently viewed runs, events, and artifacts remain
  readable with no node reachable, with every stale view labelled as such and
  no queued action silently executing on reconnect.
- **Push delivery.** Requires an operator-selected push provider and a
  node-side notification bridge. Local foreground notifications work today.
- **QR pairing payload.** The mobile app scans QR codes, but the desktop
  cannot render one: the invitation embeds the full server certificate PEM,
  which exceeds practical QR capacity. Acceptance: a redesigned short
  invitation (pairing handle + certificate fingerprint, certificate fetched
  over the pinned connection) that fits a scannable code without weakening
  pinning.
- **Store release.** Physical-device pairing, background notification
  behavior, per-OS pinned transport, signing, and store submission remain
  release gates needing real devices and publisher credentials.

## 6. Fine-tuning, adapters, and distillation

**Today:** nothing. No LoRA, adapter, or training code exists. Modelfile
Studio can declare an `ADAPTER`, but the app cannot produce one.

**Acceptance:** a local LoRA/QLoRA run against an installed base model with
dataset provenance, reproducible hyperparameters, a resource preflight that
refuses to start a run this machine cannot finish, and an adapter that then
loads through the normal runtime contract.

## 7. Multi-GPU orchestration

**Today:** a single `gpu_layers` count. Hybrid/multi-GPU hardware is
*detected* by the Hardware Compatibility Matrix but never *used* as more than
one device.

**Acceptance:** an explicit per-device split (tensor or layer) chosen from
the real hardware snapshot, with the offload planner accounting for each
device's own memory, and an honest refusal when a runtime does not support
the requested split.

## 8. Updater and release hardening

**Today:** the in-app updater ships on all three desktop platforms — background
checks, a staged bundle, a relaunch card, and Windows deferring its installer to
the click so an update cannot kill a turn mid-flight — and releases publish
themselves once every matrix target has uploaded. What is missing: rollback, a
manual check control, a visible failed check, Linux coverage beyond the AppImage,
and a startup self-integrity check. Signing is macOS-only. Ten locales are each
missing the same ~650 of 1,726 keys (they fall back to English at runtime). There
is no dependency scanning, SBOM, accessibility CI, or penetration test.

**Acceptance:** signed, verifiable in-app updates with rollback on every
supported platform; signed/notarized installers per platform; clean-machine
install and upgrade tests; locale completion; automated dependency review and
SBOM in CI; an accessibility audit in CI; and a release penetration test.

## 9. Tech debt with a real cost

These are not features, but they change what shipping the above costs.

- **Two live HTTP servers.** `server.rs` (legacy proxy, ~4.6k lines) and
  `m3_http_server.rs` (~2.1k) both serve requests. Every route policy change
  has to be made twice, correctly, or the two disagree.
- **Two live knowledge-index systems.** `stacks.rs` v1 (15 commands, still
  invoked) runs in parallel with Knowledge 2.0 (`knowledge_v*`). Retrieval
  changes must be duplicated or a user's results depend on which path their
  stack happens to use.

---

## Non-goals

Stated so they are not mistaken for missing work:

- **No hosted Little Monkey service** — no relay, account service, hosted GPU,
  or RBAC/SSO plane. Remote access is user-owned infrastructure only.
- **No Gmail/Outlook inbox integration.** Inbox triage covers Slack, Jira, and
  GitHub read-only.
- **No Google Drive knowledge connector.**
- **Browser verification stays disposable** — no persistent authenticated
  profiles, file transfer, clipboard, or extensions.

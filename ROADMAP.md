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

The kernel-level plan that used to sit beside this file — process model,
enforced isolation, scheduling, and a versioned platform contract — is built.
What shipped is described in [README.md](README.md), and where a claim stops is
[docs/limitations.md](docs/limitations.md).

---

## 1. Policy-driven model routing *(partially built)*

**Shipped:** named routing policies authored in **Settings → Automation →
Dispatch policies**, evaluated top to bottom (the list order *is* the
precedence), each scoped by task class and constrained by a cost-rate ceiling,
a measured-latency target, data sensitivity, a tool requirement, and an ordered
list of preferred models. A matched policy also supplies the turn's failover
order, replacing the fixed provider sequence. Which policy chose a turn's
target and why appears as a transcript note and in the panel, alongside every
rejected model and the reason it lost. Policies only ever select among models
already configured, and routing runs before the Privacy Firewall, which still
overrides it.

**Remaining:** subagent task classes (blocked on lifting target resolution out
of `agentLoop.ts`, which the import direction forbids today), routing to managed llama.cpp
rather than only Ollama for local-only policies, and recording the decision in
the durable run ledger so "why this target" survives a restart.

## 2. Real benchmarking *(built)*

**Acceptance:** a benchmark run produces measured tokens/sec, time-to-first
token, and peak memory for a specific model + runtime + quantization on
*this* machine, with variance across repeated runs reported and the hardware
snapshot attached. No number is displayed that was not measured on the
machine displaying it.

**Shipped — Runtime Hub → Benchmark.** Every duration comes from a monotonic
`Instant` around a real streamed generation on this machine; token counts come
from the runtime's own completion usage; peak memory comes from sampling the
runtime process. Variance is the sample standard deviation across repeats, and it
is *absent* rather than `0.0` for a single repeat. The first repeat is discarded as
warm-up so that loading the weights is not reported as prefill.

The last clause is enforced in the type system rather than by convention: an
unmeasurable field carries the reason it is unmeasurable and has no numeric
branch to render, so a gap cannot be printed as a zero. There is deliberately no
chart — a zero-height bar cannot say "unknown" rather than "zero".

**One honest gap:** no runtime here reports a quantization *scheme* for a loaded
model (a GGUF's `general.quantization_version` is a format version, not `Q4_K_M`),
so a run identifies its model and runtime and says plainly that it cannot identify
the quantization.

**Remaining:** `runtimeEdgeProfiles.ts` still returns hardcoded prose profiles
whose own text defers to this benchmark; replacing that prose with measurements is
a separate change.

## 3. Prompt and workflow version control *(built)*

**Shipped:** personas, snippets, skills, and workflow definitions keep an
append-only local revision history (`config-revisions/` in the app data
directory). **Settings → Prompts** gives every entry a History button and the
workflow editor gives every saved workflow one: list revisions, select any two
— including two on different branches — to diff, restore an older snapshot
back into the editor, and fork a named branch to keep a variant instead of
overwriting it. A save that would clobber another window's (or the CLI's) is
refused and surfaced with a choice — take theirs, or knowingly overwrite —
rather than silently winning.

**Remaining:** rules/memory files and MCP server definitions are not versioned
yet; the history is per-entity, with no cross-entity "what did this release
change" view.

## 4. Cost attribution and budget enforcement *(built)*

**Shipped:** per-request cost recording against user-entered rates, daily and
monthly budgets with a warn/pause enforcement mode, and a live budget check
before every provider request (**Settings → Usage**).

Every recorded call is attributed to the workspace that was open when it went
out and to the project folder its conversation belongs to — two different
things once a chat outlives the folder it was started in — and can be broken
down by either, or by session or model. Anything recorded without one is
counted under *Unattributed* rather than charged to a folder it may not belong
to. The workspace key is the same one the K6 process ledger stamps on
processes, so a workspace's measured wall/CPU/GPU time is shown beside its
token bill; an unmeasured field renders as its reason, never as a zero.

Budget warnings are multi-tier (default 50/80/95%), and the highest tier
crossed is reported rather than the first.

Provider billing is handled honestly rather than pretended away: every figure
the app computes is labelled an estimate from user-entered rates, calls with no
configured rate stay visibly unpriced, and a month's actual invoice total can be
entered per provider to show the drift against the estimate. Recording a bill
never rewrites the per-call estimates — a monthly total cannot be honestly split
back across the calls that produced it — and the app never reads an invoice.

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
themselves once every matrix target has uploaded. Settings → Updates & integrity
adds the manual half: a check-now control, the last check and its failure if it
failed, and a per-platform rollback that snapshots the install before an update
replaces it and restores it with a detached script. A Linux install that is not
an AppImage is told so instead of silently never updating. A startup
self-integrity check verifies the app's own signature and every managed runtime
file against its trusted manifest before anything native is executed, and a
mismatch refuses to launch rather than warning. CI runs dependency review, a
Rust and npm advisory audit, and publishes a CycloneDX SBOM, which is also
attached to every release. What is missing: signing beyond macOS (Windows needs
a code-signing certificate; Linux has no OS-level signature to produce),
clean-machine install/upgrade tests, an accessibility audit in CI, and a release
penetration test. Ten locales are each missing the same ~650 of 1,726 keys (they
fall back to English at runtime).

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

# Little Monkey conformance suite

**Suite revision: `little-monkey-conformance-2026-08-09`**

This is the specification an implementation is graded against, and the
document a "compatible" claim points at. It is the K21 half of
`docs/agent-os-roadmap.md`: the M3 compatibility harness certifies *this*
implementation from inside its own test binary, and this certifies *any* node
from the outside, over a socket.

A claim of compatibility means exactly one thing:

> A named suite revision ran against a live listener, every **required**
> section passed, and no **optional** section that was attempted failed.

It does not mean every section ran. Optional sections a node did not claim,
or a caller did not select, are listed by name in every report — see
[Skips are reported](#skips-are-reported).

---

## Running it

```bash
monkey-cli conformance --base-url http://127.0.0.1:1234 --token "$LITTLE_MONKEY_API_TOKEN"
```

| Flag | Meaning |
| --- | --- |
| `--base-url` | The node under test. Default `http://127.0.0.1:1234`. |
| `--token` | Bearer token. Falls back to `LITTLE_MONKEY_API_TOKEN`. |
| `--section` | Repeatable. Run only these sections. Default: all. |
| `--model` | Exercise the inference contract with this model instead of the first the node lists. |
| `--json` | Emit the machine-readable report instead of the terminal summary. |

Exit code `0` means compatible with this revision; `1` means not. That is the
CI contract.

The same run is available in the desktop app under **Settings → Runtime Hub →
Compatibility → Conformance suite (K21)**, and over IPC as
`run_conformance_suite`. All three call one function; there is no second
implementation that could disagree.

### Scope and exposure

The suite is run **against a node by that node's own operator**. The
attestation route is loopback-only: it carries the machine's isolation posture
and the head hashes of its event chain, and a LAN listener classifies it as a
typed loopback-only denial rather than serving it. Point the runner at a node
you administer.

---

## The attestation

Every implementation publishes `GET /v1/conformance`, authenticated like any
other route. It is how a node states what it implements, and it carries the
live evidence the run checks the statement against.

```jsonc
{
  "suiteRevision": "little-monkey-conformance-2026-08-09",
  "contract": {
    "contractVersion": "1.1.0",
    "contractDigest": "…",
    "manifest": { /* K19's generated manifest, verbatim */ },
    "compatibility": { /* the per-protocol conformance manifest */ },
    "authenticationRequired": true
  },
  "sections": [ { "id": "ledger", "requirement": "optional",
                  "claimed": false, "reason": "…" } ],
  "isolation": { "platform": "macos", "enforcement": "os_enforced",
                 "mechanism": "seatbelt",
                 "appliesToEveryToolCall": false },
  "limits": { "maxRequestBodyBytes": 33554432, "maxActiveRequests": 64,
              "modelOutputCapBytes": 20000,
              "backgroundShellOutputCapBytes": 262144,
              "childRlimitsEnforced": true, "egressHardened": true },
  "ledger": { "verification": { "state": "intact", "…": "…" },
              "head": { "sequence": 41, "eventHash": "…", "previousHash": "…" },
              "linksAfter": [] }
}
```

`GET /v1/conformance?ledgerAfter=<sequence>` additionally returns the chain
links after that sequence (capped at 64), which is what the ledger section
uses to check append-only linkage over the wire.

Two nulls are load-bearing and mean different things:

* `ledger: null` — this listener has **no ledger behind it** (a CLI-hosted
  server started outside a data directory). The ledger section is then
  reported as an honest skip.
* A `503 ledger_unreadable` — this listener **has** a ledger and could not
  read it. That is a failure, not a skip, and the route refuses rather than
  publishing a shape that would let a broken node look compliant.

Reading the attestation is itself recorded in the subsystem event stream.
Unlike `GET /v1/models`, which is filtered out as discovery, asking a node to
vouch for itself is the request that precedes a claim being made about the
machine — and it is the one action the suite can perform against any node,
including one with no model loaded, which is what makes the append-only check
runnable everywhere.

---

## Sections

### `contract` — **required** (K19)

The published route surface and its wire behaviour.

| Check | What must hold |
| --- | --- |
| `contract.attestation` | `GET /v1/conformance` answers 200 with a parseable attestation. A revision differing from the runner's is reported, not failed. |
| `contract.health` | `GET /health` → 200 with `status: "ok"`. |
| `contract.models` | `GET /v1/models` → 200, `object: "list"`, every entry a non-empty `id` and `object: "model"`. |
| `contract.chat_completion` | A non-streaming `POST /v1/chat/completions` → 200 `chat.completion` with an assistant message. |
| `contract.chat_stream` | The streaming form emits `chat.completion.chunk` frames and terminates with `data: [DONE]`. |
| `contract.unknown_route` | An unknown `/v1` path → 404. |
| `contract.method_discipline` | `GET` on a POST-only route → 404 or 405, never the route's own answer. Both statuses are conformant: the legacy listener keeps its historical 404 so a migrating client sees no wire change, the M3 listener answers a typed 405. |
| `contract.authentication` | If the node attests `authenticationRequired`, an unauthenticated request → 401. If it does not, this check is skipped — which leaves the required section **incomplete**, and therefore not compatible. A node claiming compatibility must require a token. |
| `contract.error_envelope` | A malformed body → 4xx with `error.message` and `error.type`. |
| `contract.route_table` | K19's published manifest names `/health`, `/v1/models`, `/v1/chat/completions`, `/v1/contract` and `/v1/conformance`. |
| `contract.abi_version` | `GET /v1/contract` and the attestation agree on the contract version, the manifest digest and the support window. One instance publishing two surfaces that disagree about its own ABI is a defect a client discovers the hard way. |

A node with no models cannot complete this section: the two inference checks
skip, the section reads `incomplete`, and the verdict is not compatible. That
is deliberate — a compatibility claim that never exercised inference is not
one.

### `isolation` — optional (K3)

| Check | What must hold |
| --- | --- |
| `isolation.mechanism` | The attested enforcement state names a kernel mechanism: Seatbelt on macOS, Landlock + seccomp on Linux, AppContainer + job object on Windows. Claiming enforcement without naming a mechanism fails. |
| `isolation.scope_declared` | The node states how far confinement reaches. This build attests `appliesToEveryToolCall: false`, which is the honest answer today. |
| `isolation.denied_surfaces` | Every path in the node's own denied-capability list answers 404 — indistinguishable from unknown, so the surface is not even enumerable. |
| `isolation.no_tool_routes` | The compatibility manifest advertises no workspace or tool route. |

A host whose kernel cannot enforce a boundary — a Linux kernel without
Landlock, a container whose policy blocks the syscall — does **not** claim
this section. It reports a named skip. That is not leniency: this section is
about a guarantee the kernel provides, and reporting its absence as a defect
in the software would be the wrong finding.

### `limits` — optional (K4/K5)

| Check | What must hold |
| --- | --- |
| `limits.declared` | The request-body cap, concurrent-request cap, model output cap and shell tail cap are all declared and non-zero. |
| `limits.egress_policy` | The node declares a hardened egress policy: connect timeout, silence budget, and a redirect policy that will not carry a credential to a host a `302` chose. |
| `limits.oversized_body` | One byte past the declared cap → **413**, specifically. Not "any 4xx": a 400 would be a refusal of the *content*, and a client could not tell a too-large body from a malformed one. |

### `ledger` — optional (K12)

| Check | What must hold |
| --- | --- |
| `ledger.chain_intact` | The node recomputes its own subsystem chain and reports `intact`. |
| `ledger.append_only` | Take the head; perform a recorded action; ask for the links after that head. The first must name the observed head as its predecessor, and each subsequent link must name the one before it. A rewrite between the two reads is what this catches. |

The ledger section re-reads the attestation rather than reusing the one the
run opened with: that first read predates every request the suite has since
made, and grading append-only against a stale head would report an empty
chain on a node the suite itself has been filling.

---

## Skips are reported

Three different things can stop a section from running, and the report keeps
them apart:

| Report | Meaning | Blocks a claim? |
| --- | --- | --- |
| `skipped` with a reason | The node did not claim it, or the caller did not select it. Listed under `skippedOptionalSections`. | No |
| `incomplete` | Every check that ran passed, but at least one could not run. | Yes, if the section is required |
| `failed` | A check that ran disagreed with this document. | Yes, required or optional |

A required section that is skipped or incomplete is never compatible. An
optional section that is skipped is never counted against the node — but it is
named, every time, in both the terminal summary and the JSON report.

---

## Revisions

The revision is date-stamped rather than semver'd: this is not a dependency
anyone resolves, it is a thing that ran on a day. Bump it whenever a check is
added, removed, or made stricter. A claim against an older revision stays
exactly as true as it was, and stays visibly older.

| Revision | Change |
| --- | --- |
| `little-monkey-conformance-2026-08-09` | First published revision: contract, isolation, limits, ledger. |

---

## The manifest is K19's, not a copy

`contract.manifest` is `contract::manifest()` verbatim — generated by reading
`http_route_registry::ROUTES` and the tool definitions the running code
dispatches from. The conformance attestation does not re-derive a route table
of its own, because a published contract that is a second copy of the surface
is worth less than no contract: it is believed.

That is also why `contract.abi_version` exists. The attestation restates the
version and the manifest digest; `GET /v1/contract` is the version-negotiation
surface a client actually builds against. Checking one against the other is the
only way to catch an instance whose two published surfaces disagree.

Adding `/v1/conformance` to `ROUTES` moved the generated manifest, so K19's
gate refused the change until `CONTRACT_VERSION` reached `1.1.0` and the
baseline was republished. See `docs/contract-abi.md`.

---

## Where the code is

| Piece | File |
| --- | --- |
| Catalog, attestation, runner, verdict rules | `src-tauri/src/conformance.rs` |
| The attestation route | `src-tauri/src/server.rs` (`handle_conformance_attestation`) |
| Route-table entry and exposure rule | `src-tauri/src/http_route_registry.rs` |
| Chain evidence (hashes only, never contents) | `src-tauri/src/subsystem_audit.rs` |
| CLI | `src-tauri/src/bin/monkey-cli/conformance_cli.rs` |
| Desktop panel | `src/components/Settings/runtimeHub/RuntimeHubConformance.tsx` |
| The suite run against a live node | `src-tauri/tests/conformance_suite.rs` |

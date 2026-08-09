# The Little Monkey contract ABI

The published surface a third party builds against: every externally reachable
HTTP route, every signed remote-plane route, the ACP stdio methods, and the
agent tool schemas. One version number, one artifact, one endpoint.

* **Artifact** — [`contract/agent-os-contract.json`](../contract/agent-os-contract.json)
* **Baseline** — `contract/baseline.json`, the last *published* contract
* **Endpoint** — `GET /v1/contract` on any running listener
* **CLI** — `monkey contract version | emit | check --url <base>`

Roadmap: K19. K20's package resolver and K21's conformance suite both gate on
the version below.

## What the version means

`CONTRACT_VERSION` (`src-tauri/src/contract.rs`) is semver over the *surface*,
not over the app. `little-monkey` ships builds constantly; this number moves
only when what you can call changes.

| Bump | What causes it |
| --- | --- |
| **major** | A route, method, ACP method or tool is removed; a route moves; a tool parameter disappears; a parameter becomes required; a remote route starts demanding a different grant; a protocol version changes; the support window shortens. |
| **minor** | A route, method, ACP method, tool or optional parameter is added; a required parameter becomes optional; a denied surface is declared or withdrawn. |
| **patch** | Descriptions and other wording. |

The rules are executable, not advisory: `contract::diff` classifies every
difference and `contract::required_version` derives the smallest version that
covers them.

## Deprecation policy

A surface is never removed without being announced first.

1. It is added to `contract::DEPRECATIONS` with the version that announced it,
   the earliest version that may remove it, and its replacement.
2. It keeps working, unchanged, for at least **`SUPPORT_WINDOW_DAYS` (180
   days)** from the release that first shipped that announcement.
3. Removal is a **major** bump on top of the window. The window is a floor, not
   permission to remove on day 181 in a minor release.

The window is a number in code rather than a sentence in a document because
K20's resolver has to answer "is this still here next quarter?" without a human
reading this page.

## Asking a running instance

```bash
curl -s http://127.0.0.1:1234/v1/contract | jq '.contract_version, .digest'
```

Unauthenticated on both listeners, for the same reason `/health` is: a client
negotiates the ABI before it can know whether the credential it holds is still
the right shape. The body is a pure function of the built binary — the version,
the digest of the exact manifest, the implementation's own version, and the
whole manifest, so a client needs no second request and no shipped copy.

`monkey contract check --url <base>` does the comparison for you: same major
and a minor at least as new as the local build, or a non-zero exit that names
what is missing.

## Publishing a change

1. Change the surface (add a route to `ROUTES`, a tool to `agent_tools.rs`, a
   remote route to the dispatch match and `contract::REMOTE_ROUTES`, …).
2. `UPDATE_CONTRACT=1 pnpm contract:check` — regenerates
   `contract/agent-os-contract.json` from the code.
3. Review that diff. It *is* the change third parties will see.
4. Bump `CONTRACT_VERSION` by whatever the table above requires. Running
   `pnpm contract:check` again tells you exactly what it requires if you are
   unsure; the failure names the required version and every change forcing it.
5. Copy the artifact over the baseline — this is the act of publishing:
   ```bash
   cp contract/agent-os-contract.json contract/baseline.json
   ```
6. Commit all of it together.

Skipping step 5 is safe (the gate keeps failing until you do it); skipping step
4 is what the gate exists to prevent.

## What enforces this

| Check | Where | Fails when |
| --- | --- | --- |
| Artifact matches the code | `src-tauri/tests/contract_abi.rs` | The published JSON is stale. |
| Version covers the change | same file | `CONTRACT_VERSION` did not move far enough past the baseline. |
| Remote routes are real | `daemon/remote/api.rs` test | The dispatch match has a route, method or grant the contract does not. |
| ACP methods are real | `acp.rs` test | The ACP loop dispatches a method the contract does not name. |
| Desktop tools match | `src/lib/contractDrift.test.ts` | `tools.ts` and the published schema disagree about a tool's arguments. |
| Endpoint serves it | `tests/legacy_route_compatibility.rs` (with `/health`, the other pre-auth route), `m3_http_server.rs` | A listener stops answering `/v1/contract`, or answers something other than the published manifest. |

All of them run in CI on Linux, macOS and Windows (`pnpm test:rust` and
`pnpm test`); `pnpm contract:check` runs the two gate tests alone.

## Stated gaps in v1

* **The desktop extension tools are not published.** `spawn_task`,
  `shell_output`, `shell_kill`, `skill`, `read_skill_resource` and
  `generate_image` exist only in the desktop app, whose definitions live in
  TypeScript. v1 publishes the schemas both surfaces implement, generated from
  one Rust source. `contractDrift.test.ts` pins that list, so the set cannot
  grow silently — but a client cannot yet build against those six.
* **Descriptions are per-surface.** The desktop tells the model about multi-root
  workspaces; the CLI, which has one root, does not. The contract publishes the
  schema, and the drift test compares schemas rather than prose.
* **Tauri commands are not a contract.** The ~490 `#[tauri::command]` entry
  points are the app's own internal IPC, reachable only from its own webview.
  They are deliberately absent here, and `DENIED_SURFACES` (published in the
  manifest) is the machine-readable statement that no HTTP caller reaches an
  agent, workspace, tool, file, git, MCP or recipe surface.

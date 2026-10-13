# Built-in security swarms

Little Monkey provides a multi-agent security workflow **natively**,
using its existing phased `workflow` tool rather than loading another agent
runtime. This means the existing permission gate, tool profiles, task limits,
cancellation, process ledger, model routing and workspace boundaries still apply.

## Use from a chat

Enable **Subagents** in Little Monkey's settings and open the repository
workspace. Ask:

> Run the security-red-team swarm against this repository. Audit authentication,
> authorization, dependencies and API inputs. Report evidence before suggesting
> fixes. Do not change any files.

The agent can call the workflow tool with:

```json
{"preset":"security-red-team","scope":"the current workspace"}
```

There are **15 specialist agents across four ordered phases** (6 + 6 + 2 + 1).
All agents use the read-only `explore` profile. They inspect files and report
evidence, but cannot run shell scanners, change files or execute tests. This is
a static code review; the findings are not a penetration test.

For a user-authorized fix-and-verify pass, ask explicitly:

> Run security-audit-and-fix on the authentication module. Fix independently
> confirmed issues and run the relevant tests, with normal approval prompts.

The tool call is:

```json
{"preset":"security-audit-and-fix","scope":"authentication module"}
```

This second preset uses five read-only auditors, two independent validators,
**one** permission-gated modifying agent, and **one** verification agent.
The modifying agent works in the current workspace under Little Monkey's
normal edit/shell permission gate and checkpoints. It must not invent evidence,
claim tests passed without execution, or apply speculative fixes.

The model may suggest the read-only preset from a general audit request.
It must **not** select the fix preset unless the user has explicitly requested
modifications.

## Implementation and boundaries

- Both presets are fixed, data-only recipes; user-provided `scope` is a
  bounded single line. They do not execute arbitrary workflow code.
- Agent concurrency is constrained by the existing
  `maxConcurrentSubagents` setting. The engine runs each phase to completion
  before scheduling the next and passes previous reports along as **untrusted**
  data, capped to control context growth.
- Each agent uses the model chosen through Little Monkey's existing routing
  and profile policies; 15 agents do not necessarily mean 15 distinct models.
- Cancellation, cost/token usage, resumption of failed workflows, and history
  reuse the current workflow runtime and stores. Local models must themselves
  support tool calling.
- The read-only preset covers scoped security reviews, not every feature of the full platform:
  independent federation, shared vector memory between concurrent agents,
  autonomous toolchain installation, and third-party plugin marketplaces
  are not supplied by this particular preset.
- All tool results, including earlier agent reports, require verification.
  A high-confidence conclusion needs source evidence and ideally regression
  tests; an agent's assertion alone is insufficient.

## Developer contract

Preset compilation: `src/lib/swarmPresets.ts`.

Dispatch: `resolveWorkflowSpec` in `src/lib/workflow.ts`.

Tool schema: `WORKFLOW_TOOL` in `src/lib/tools.ts`.

Tests: `src/lib/swarmPresets.test.ts` and `src/lib/workflow.test.ts`.

Run:

```sh
pnpm test -- src/lib/swarmPresets.test.ts src/lib/workflow.test.ts
pnpm typecheck
```

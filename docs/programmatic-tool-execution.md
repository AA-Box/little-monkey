# Programmatic tool execution

## Implementation notes

The current desktop turn path is:

1. `src/lib/agentLoop.ts` resolves MCP and executable-extension registries, builds the turn-local tool definitions with `buildTools`, `toolsForMode`, and `toolsForSettings`, and sends those exact definitions to the selected model.
2. Model tool calls are accepted only when the dispatcher confirms that the name was offered for this turn and is still available at invocation time.
3. `src/lib/turnEngine.ts::executeToolCall` parses arguments, removes frontend-owned reserved arguments, validates the offered schema, classifies risk, injects the current turn/checkpoint/tool-call context, runs hooks, and dispatches frontend-only tools, MCP tools, executable-extension tools, or `invoke('tool_<name>')`. Its completion hook is shared by direct and nested calls.
4. Rust tool commands in `src-tauri/src/tools.rs`, `src-tauri/src/mcp.rs`, and the executable-extension host perform the real operation. Workspace paths go through `agent_worktrees::resolve_with_override`; mutating and network/MCP operations call `permissions::request_permission`; mutation and external-effect commands use `checkpoints`; cancellable shell/MCP work uses `AppState::tool_cancel`.
5. `agentLoop.ts` records `tool_proposed`, `tool_started`, and `tool_finished` through `DurableRunRecorder`; the recorder redacts arguments/results and links artifacts. The same run id is passed into permission and tool commands.
6. `runCancellationRegistry`, `tools_cancel_running`, and the existing `AbortSignal` stop path cancel the owning turn and pending/in-flight cancellable tools. `sessionStore` persists the assistant tool-call and matching tool-result messages; `MessageList.tsx` groups ordinary calls into the existing activity timeline.

Programmatic execution therefore owns only the untrusted program runtime and its limits. Its tool binding calls back into this exact dispatcher with a fresh nested tool-call id, the owning turn id, checkpoint id, signal, risk context, MCP registry, extension registry, and durable recorder wrapper. It has no filesystem, network, subprocess, environment, secret, IPC, or module-import access.

## Architecture

- `src/lib/programmaticExecution.ts`: provider-neutral service contract and execution orchestration.
- `src/lib/programmaticQuickJsRuntime.ts`: QuickJS WebAssembly provider. It exposes only generated `tools[<name>]` bindings and bounded `console` logging.
- `src/lib/agentLoop.ts`: derives the program SDK from the exact tool list offered to the turn and describes when batching is useful.
- `src/lib/turnEngine.ts`: dispatches the model-facing program tool and routes each nested binding through `executeToolCall`.
- `src/components/Chat/MessageList.tsx`: renders the outer execution using the existing tool activity chrome with expandable nested evidence.

The programmatic capability is excluded from its own generated SDK, is offered only when the runtime reports healthy, and remains optional so ordinary individual tool calls continue unchanged.

## Real-path integration coverage

`src-tauri/src/programmatic_tool_e2e.rs` is the native integration suite. It
uses `test_support::build`, a real Tauri `MockRuntime` window, and an
in-memory `RunLedger`; it does not mock the Tauri command boundary. The suite
drives the real workspace resolver and native commands for ordinary reads,
workspace escapes, allowed and denied permission prompts, and checkpointed
mutations. It also invokes the real Component Model extension host and asserts
canonical outer/nested event shapes, permission decisions, run evidence, and
chain verification at the ledger boundary.

`src/lib/durableRun.test.ts` additionally runs the production recorder with
the exact turn-qualified `run_program` execution identity and its derived
`:nested:1` identity, asserting the IDs emitted for proposed, started, and
finished events. This closes the frontend runtime → recorder identity seam;
the native suite verifies the recorder-shaped events at the real ledger and
permission boundary.

The Vitest coverage in `src/lib/turnEngine.test.ts` drives QuickJS through the
production dispatcher and recorder boundary. Vitest cannot host the native
Tauri runtime, so the two suites provide complementary proof: QuickJS →
dispatcher → recorder in TypeScript and dispatcher →
permission/workspace/checkpoint/ledger/extension infrastructure in Rust, with
the same production command implementations on the native side.

## Threat model and limits

The source is untrusted. QuickJS has no host standard library or imports; host functions receive and return JSON-compatible values only. Source, arguments, nested-call count/concurrency, logs, return serialization, memory, stack, instruction interrupts, wall time, and cancellation are bounded. Each nested call still passes normal schema/IPC decoding, permission/risk policy, workspace and egress policy, checkpoint handling, extension/MCP authorization, and audit/run recording.

The runtime is intentionally provider-neutral: a future provider implements the same capability contract without changing the generated SDK or model-facing tool.

## Security review

The program source is intentionally dynamic, but it is JSON-stringified before insertion into the generated wrapper. The wrapper is evaluated only inside the isolated QuickJS WebAssembly context; the host application never calls `eval` or `Function` on the program source. Host bridge handles are removed from the guest global before the program runs, and the program receives only frozen, null-prototype tool bindings plus bounded JSON console logging.

Every nested call is re-authorized against the current turn state and routed through the canonical dispatcher. That dispatcher owns schema validation, plan/settings gates, permission handling, workspace-root resolution, checkpoint injection, cancellation, hooks, and durable completion evidence. CodeQL's dynamic-code finding at the guest `Function` construction is therefore an intentional isolated-runtime sink; the finding should be closed with this justification and the accompanying security comment, not suppressed as an ordinary host-code exception.

The formal disposition is recorded in
[`docs/security/programmatic-runtime-codeql.md`](security/programmatic-runtime-codeql.md).

## Current runtime limitations

The initial provider executes JavaScript/TypeScript-like JavaScript syntax in an embedded QuickJS WebAssembly runtime. TypeScript type annotations are not transpiled. Host values must be JSON-compatible, and host APIs other than `tools` and bounded `console` are unavailable. Nested calls are bounded and ordered by completion only inside the program; their durable evidence is ordered by the recorder.

## Troubleshooting

If the program capability is absent, check that the embedded runtime initialized successfully; it is offered only while the provider reports healthy. If a nested call fails, inspect the expanded execution row and the run evidence: the failure category and the canonical tool result are preserved separately. Permission prompts still require the normal user decision; a program cannot approve itself. Cancellation returns a cancelled execution and cancels supported nested operations.

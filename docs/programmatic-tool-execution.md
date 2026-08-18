# Programmatic tool execution

## Implementation notes

The current desktop turn path is:

1. `src/lib/agentLoop.ts` resolves MCP and executable-extension registries, builds the turn-local tool definitions with `buildTools`, `toolsForMode`, and `toolsForSettings`, and sends those exact definitions to the selected model.
2. Model tool calls are accepted only when `isToolCallAllowed` confirms that the name was offered for this turn.
3. `src/lib/turnEngine.ts::executeToolCall` parses arguments, removes frontend-owned reserved arguments, classifies risk, injects the current turn/checkpoint/tool-call context, runs hooks, and dispatches frontend-only tools, MCP tools, executable-extension tools, or `invoke('tool_<name>')`.
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

## Threat model and limits

The source is untrusted. QuickJS has no host standard library or imports; host functions receive and return JSON-compatible values only. Source, arguments, nested-call count/concurrency, logs, return serialization, memory, stack, instruction interrupts, wall time, and cancellation are bounded. Each nested call still passes normal schema/IPC decoding, permission/risk policy, workspace and egress policy, checkpoint handling, extension/MCP authorization, and audit/run recording.

The runtime is intentionally provider-neutral: a future provider implements the same capability contract without changing the generated SDK or model-facing tool.

## Current runtime limitations

The initial provider executes JavaScript/TypeScript-like JavaScript syntax in an embedded QuickJS WebAssembly runtime. TypeScript type annotations are not transpiled. Host values must be JSON-compatible, and host APIs other than `tools` and bounded `console` are unavailable. Nested calls are bounded and ordered by completion only inside the program; their durable evidence is ordered by the recorder.

## Troubleshooting

If the program capability is absent, check that the embedded runtime initialized successfully; it is offered only while the provider reports healthy. If a nested call fails, inspect the expanded execution row and the run evidence: the failure category and the canonical tool result are preserved separately. Permission prompts still require the normal user decision; a program cannot approve itself. Cancellation returns a cancelled execution and cancels supported nested operations.

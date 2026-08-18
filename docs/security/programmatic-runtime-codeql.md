# Programmatic runtime CodeQL disposition

Status: reviewed and accepted as an intentional isolated-runtime sink.

The JavaScript/TypeScript CodeQL finding points at the `Function` constructor
used by `src/lib/programmaticQuickJsRuntime.ts`. The constructor runs inside
the QuickJS WebAssembly guest after the wrapper has been evaluated; the host
application never evaluates model-authored source with host `eval` or
`Function`.

The source is JSON-stringified before insertion into the guest wrapper. Before
execution, bridge globals are removed and the guest receives only frozen,
null-prototype JSON tool bindings and bounded console functions. The guest has
no host filesystem, network, process, environment, secret, module, or IPC
access. Every tool call returns through the canonical dispatcher, which
rechecks current availability and schema, then owns permission, workspace,
checkpoint, cancellation, hook, and durable evidence behavior.

This is not a request to suppress an ordinary host-code injection finding. The
security comment at the sink and the native integration suite in
`src-tauri/src/programmatic_tool_e2e.rs` are the supporting controls. CodeQL
should be marked resolved with this rationale when the alert is reviewed.

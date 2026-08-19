# Little Monkey for VS Code

This extension is a thin client for Little Monkey's ACP v1 server. Agent runs execute in the installed local daemon and use its durable ledger, checkpoints, verification, worktree, and permission system. VS Code can display an approval wait, but it cannot grant one.

The current release is version 1.0.0. Install the `little-monkey-vscode-1.0.0.vsix` asset from a Little Monkey GitHub release; marketplace publishing is not automatic.

## Setup

1. Install Little Monkey and enable its local daemon.
2. Set `littleMonkey.cliPath` if the installed `monkey` command is not on `PATH`.
3. Set `littleMonkey.agentModel` to an installed Ollama tag.
4. Run **Little Monkey: Ask About Active Editor**.

The prompt includes the active document, selection, and Problems diagnostics with the exact VS Code document version. Completed edits open in VS Code's native diff view. **Cancel Active Run** propagates cancellation to the durable run.

## Local inline completion

Autocomplete is off by default. Set an explicit `littleMonkey.completionModel`, add that exact tag to `littleMonkey.fimCapableModels`, then enable `littleMonkey.enableCompletions`. The extension also requires Ollama's live `/api/show` response to advertise the `insert` capability before it sends code. Requests go only to loopback Ollama and are cancelled on newer document versions. There is no cloud fallback.

Selection edits always show a diff and require an explicit **Apply** click against the same document version.

## Verification

`npm test` runs the ACP/client safety checks and compiles the checked-in corpus
of 100 exact JavaScript insertion fixtures. To run the hardware/model gate
against an explicitly selected local FIM model:

```sh
LITTLE_MONKEY_COMPLETION_MODEL='your-exact-fim-tag' npm run benchmark:completions
```

The JSON report includes p95 request latency, compile count, and
Ollama-reported RAM/VRAM residency. It fails unless at least 70 insertions
compile and p95 stays below 750 ms, and it never selects a cloud fallback.

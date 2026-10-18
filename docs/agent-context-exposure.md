# Agent context exposure tracking (desktop)
 
The real `executeToolCall` dispatch performs native reads/grep as normal, then
compacts only **byte-for-byte identical** successful tool responses already
returned to the same agent within the same turn/worktree. The model receives
a small `already_exposed` receipt rather than a duplicate long blob.
Edits, writes, and shell operations invalidate this turn's cache. A new turn
always gets full fresh context; no content is reused as an authorization or
filesystem-access shortcut. Failed or oversized results remain untouched.

The bounded in-memory tracker retains at most 48 responses (each <= 512 KiB)
per frontend process, with no durable disk record. It is not a cross-process
agent-memory system and does not infer semantic similarity.

```sh
pnpm exec vitest run src/lib/contextExposure.test.ts
pnpm typecheck
```

The shared desktop/CLI grep helper also identifies conservative Python indentation\nand balanced-brace declaration regions. These are lexical bounds, **not** AST\nproof: strings, macros, generated code and unusual grammar may still require a\nreal language parser.\n\n**Outstanding:** grammar-backed coverage for Go, Java, Kotlin, Swift, C/C++, C#, PHP and other supported extensions; CLI/daemon context exposure integration,\nand full-model efficiency comparisons. These are separate acceptance requirements.

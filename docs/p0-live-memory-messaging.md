# P0: native live agent collaboration and automatic semantic memory

This feature extends the merged native swarm engine; it does not replace the
permissions, model policy, journal, or paired-peer transport.

## Enable it for a local workflow

1. Open the repository as the current workspace.
2. Configure an embeddings-capable Knowledge 2.0 stack. Record its ID.
   An active embedding runtime must be available; a failure is reported rather
   than being substituted with a hash or fabricated vector.
3. Enable Subagents in Settings.
4. Run the workflow tool:

```json
{
  "name": "project-audit",
  "description": "Review authentication and tests",
  "live_messaging": true,
  "auto_memory_stack_id": "YOUR_EXISTING_KNOWLEDGE_STACK_ID",
  "phases": [
    {
      "title": "Investigate",
      "agents": [
        {"description": "Authentication review", "profile": "explore", "prompt": "Inspect authentication code and report evidence."},
        {"description": "Tests review", "profile": "explore", "prompt": "Inspect auth regression coverage and report evidence."}
      ]
    }
  ]
}
```

These are independent opt-ins. No indexed workspace content or agent report
is automatically sent to paired peers. Saved workflows retain the settings.

## Live mailbox

Within this local workflow, each enrolled, built-in worker receives
`swarm_send_message` and `swarm_read_messages` tools. Each agent is given its
own task ID in its system prompt, and can list recipients. Direct messages or
`*` broadcast stay inside this one run. A successful `read_file` registers
the exact normalized path; a later successful `write_file`/`edit_file` by a
different local worker posts a change notification to that reader.

The inbox is deliberately bounded: 16 agents per workflow, 24 queued messages
per recipient, 1,200 characters per direct message, six messages per drain.
An overflowing direct-message send fails without partial broadcast delivery.
A safety-relevant file-change notification displaces the oldest queued
coordination message if needed; `displacedForFileChange` records the loss.
A completed file edit is never mislabelled as rolled back. Messages reach recipient models at their next model turn or when
they explicitly read the inbox. They cannot interrupt an in-progress model
request or an agent that has already finished.

Peer text is always untrusted model input. It never gets system/developer
authority or authorizes changes. The existing operating-system permission gate
continues to control every file mutation.

The bus is currently **run-scoped and in-process**: not a distributed pub/sub,
not a durable cross-restart mailbox, and not enabled for remote peer workflows.
Only built-in read-only/code workers receive its send/read tools; custom agents
retain their own explicit allowed tools.

## Automatic local semantic memory

When `auto_memory_stack_id` is specified:

- Before dispatch, use that stack's *real* configured embedding model to query
  the daemon's durable local SQLite vector namespace. The namespace is SHA-256
  derived from the exact workspace identity, never global or shared by default.
- Inject up to five matching historical reports as source-labelled **untrusted
  evidence** into worker context. Current repository evidence takes precedence.
- After a workflow **fully succeeds**, embed and store its completed agent
  reports with deterministic SHA-256 content IDs and idempotent revision 1.
  The tool result reports actual new `automaticMemory.saved`, idempotent
  `automaticMemory.reused`, and `automaticMemory.warnings` rather than claiming silent success.
- Persist using the existing `peers_memory_add_text` /
  `peers_memory_query_text` operations, which pin the model/dimension/prefixes
  and fail closed on inconsistent embedding spaces. No new vector engine,
  background data export, or network permission is introduced.

Known limits: memory stores bounded report text (not an independently judged
fact extraction). A historical successful agent's statement is **not** proof
that its claims were correct. The existing vector namespace holds at most 128
records; saturation produces a visible warning. Automatic conflict resolution,
pruning/forgetting, retroactive backfill of all conversations, autonomous
promotion, and a physical two-machine acceptance run are **not provided** by
this change. The operator must opt in and select a stack so private project
content cannot be silently embedded against an unknown provider.

## Automatic memory across ordinary conversations

In **Settings → Rules & Memory**, enable **Automatically learn from my
conversations** (default OFF). The separate parent "Remember" toggle must
remain enabled. Turning either switch off stops new automatic capture; older
memories remain under Memory Studio's enable/delete/merge/undo controls.

Every successfully completed ordinary desktop chat turn, whether handled by
the resident daemon or the foreground loop, triggers one tool-free model
reflection through the currently selected model. Only the actual user
message is eligible. The reflection returns at most three proposed stable
facts, each with a **literal quote that must appear in that user message**.
The host rejects secret-shaped input, unsupported quotes, oversize claims,
duplicate text and attempts to edit pinned, manually entered or retired
memories. A model error cannot make the already completed chat fail.

Valid inserts use the new Rust `memory_auto_apply` command, the same
cross-process-locked atomic `memories.json` fact store used by the regular
`remember` tool. New records are `source: agent` and carry the source
turn ID. In project chats they are scoped to the active, canonical
workspace; without a project they use the existing global scope. The
command rejects a workspace switch before a write. A correction can only
modify an existing agent-authored, unpinned, enabled fact with **exact old
text** still present in the current scope. User-owned and pinned facts
cannot be automatically rewritten.

The selected model may be a cloud provider. Enabling extraction makes an
additional model request containing the current user message and a bounded
set of prior agent facts; only enable it when that transmission is
acceptable. No tool outputs, assistant-written claims or connector bytes
are eligible as new direct memories. This is conservative model extraction,
not independently verified truth or autonomous rewriting of user-owned
memories. If the app is terminated between completing a turn and committing
the extraction, that final reflection can be missed; the old conversation
is never silently re-executed after restart.

## Durable live messaging and restart recovery

The live mailbox snapshot is included in the existing file-backed
`WorkflowRunMeta.liveMailbox` journal. An agent message is acknowledged
to its sender only after `flushSessionPersistence` has successfully
persisted the new bounded snapshot; a failed save rolls back the in-memory
change and surfaces an error. File read observations, file-change notices,
completion and acknowledgments use the same serialized journal barrier.

At the recipient, queued messages are **peeked**, not destructively removed.
The receiver acknowledges message sequence numbers only after a model
request succeeds. Crashes before acknowledgement leave messages queued for
the next explicitly authorized workflow resume. Remapping of old agent
task IDs to new IDs is restricted to the same validated task positions.
A corrupt or missing live-mailbox journal fails closed instead of
fabricating a fresh state.

The resume flow remains operator-controlled. Code tools are never silently
replayed across process restarts, and a delivered message is not an
authorization to execute anything. Network peers are not part of this
local run-scoped mailbox. Exactly-once model/tool side effects are not
guaranteed; replayable untrusted notices may be delivered more than once
if the process crashes after receipt but before a durable acknowledgment.

## Verification

```sh
pnpm exec vitest run src/lib/swarmLiveBus.test.ts src/lib/swarmAutoMemory.test.ts src/lib/conversationAutoMemory.test.ts src/lib/workflow.test.ts src/lib/subagent.test.ts
pnpm typecheck
pnpm test
```

Live-model acceptance additionally requires an actual tool-capable configured
model, an indexed embeddings stack, and a local two-worker workflow. Verify:

1. The model sends a direct message and the sibling receives it at its next turn.
2. A worker reads a file, another modifies it under explicit permission, and
   the reader gets a change notification and re-reads the path.
3. A successful workflow returns a positive memory `saved` count.
4. Re-run the workflow with the same workspace and stack; observe source-labelled
   semantic recall from the SQLite store.
5. Disable the embedding model; the opted-in workflow visibly fails before
   dispatch, and never claims to have learned anything.

A passing mocked unit test does not substitute for these physical/model checks.

# Opt-in desktop chat-to-chat agent collaboration

In **Settings → Automation → Subagents**, enable **Allow local agent messages
between my chat sessions** (default OFF). Little Monkey will then offer
`session_list_peers` and `session_send_message` to the model in normal
desktop chat turns attached to an actual workspace.

The trusted sender identity is the active local chat session id supplied by
Little Monkey's own tool dispatcher, never a model argument. The send tool
receives an exact recipient session id returned by the list tool. Both
sessions MUST already exist and share the same literal workspace root.
Global chats, archived chats, comparison branches, other project roots,
unavailable peers and self-sends cannot be addressed. This setting is
globally per profile and automatically disabled in Plan Mode.

- Notices are limited to 1,600 UTF-16 code units and are wrapped as
  **untrusted peer content**, clearly attributed to the sending session.
  They cannot confer permissions, prompt priority or filesystem rights.
- Sending mutates the recipient's persisted chat transcript and waits for
  `flushSessionPersistence` before issuing a success receipt.
- The receipt is tied to the current turn ID + model tool-call ID. Replaying
  the same message in a single desktop instance does not append a duplicate.
  Failed persistence returns an error instead of claiming durability.
- The receiver sees the message immediately in the same window or through
  existing cross-window session-change events; the model sees the historical
  notice only when the user initiates a future turn. Messages DO NOT
  automatically wake an idle agent or rerun a failed task.
- The original workspace must already be attached by the user. The tool
  cannot attach a new root or resolve a foreign path, and it does not give
  any agent a new network, shell or file-edit capability.

**Important limitations:** This is local DESKTOP session collaboration,
not JCode's daemon-wide live session bus. It does not connect independent
daemon processes or paired machines; existing session-file persistence does
not supply a general compare-and-swap transport between simultaneous
writers. Crash-consistent exactly-once delivery under multiwindow races,
real-time file-read invalidation outside an enrolled workflow, cross-device
agent messaging, and automatic task takeover remain unimplemented.

## Verification

```sh
pnpm exec vitest run src/lib/sessionCollaboration.test.ts
pnpm typecheck
```

Actual desktop acceptance: enable the feature, create two chats attached
to the same canonical workspace, have A list peers and send to B, restart
the app, confirm the note is in B and no tool was executed, then disable
the setting and verify sends fail. Test cross-workspace, archived-chat and
Plan Mode refusal. The tests above exercise the persistence barrier through
mocks; they do not replace a physical two-window race test.

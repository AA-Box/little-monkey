# Durable, opt-in desktop session collaboration

**Default OFF.** Enable **Settings → Automation → Subagents → Allow local agent
messages between my chat sessions**. Ordinary desktop coding-agent turns will
then see two bounded model tools:

- `session_list_peers` lists unarchived, non-comparison chat sessions already
  attached to the **exact same workspace**.
- `session_send_message` sends at most 1,600 UTF-8 bytes of untrusted text
  to one of those session IDs.

No dynamic workspace grants, automatic model turns, shell execution,
network-facing endpoints or independent agent processes are created.

## Delivery and crash recovery

The **authoritative mailbox and recent local-peer directory are per-profile SQLite**,
separate from the frontend's session file. A desktop chat only enrolls in
the directory when collaboration is explicitly enabled. Mounted chat windows
refresh their registration; the sender registers other eligible known sessions
before asking for peers. Registrations older than one hour stop authorizing
new sends. The backend requires *both* sender and recipient to be registered
with the same workspace in the current profile; a model-supplied recipient
ID alone is never enough.

The **authoritative mailbox is per-profile SQLite**, separate from the
frontend's session file. On send, Rust uses WAL and an IMMEDIATE transaction
to insert a unique `turn_id:tool_call_id` message with a frozen sender,
recipient, workspace and text. Retrying the identical identity is idempotent;
reusing that identity with different content is rejected. Each recipient is
limited to 1,000 outstanding messages.

The recipient reads only messages for its own session ID and frozen workspace.
Its desktop window polls while visible/idle, and every new model turn first
reads pending messages. Each message is appended as an **untrusted assistant
notice** with a stable receipt marker (not a real tool call or new human
instruction). Then Little Monkey flushes its session file to disk. Only
**after successful persistence** does it acknowledge the message in SQLite.

A crash before transcript persistence leaves the SQLite row pending; a crash
after persistence but before acknowledgement delivers the same marker again
and avoids inserting a duplicate. Acknowledge is idempotent and requires the
exact recipient and workspace. A model cannot obtain write permissions or
invoke a new assistant turn by sending mail.

The Rust tests cover SQLite transactions, registration and workspace ACL,
stale peer expiry, repeated opens, thread contention, and a child test process
that writes before the parent reopens and acknowledges.
The frontend tests cover real IPC *call ordering* with mock native endpoints,
same-workspace consent, crash windows and failure postures.

## Local verification

\`\`\`sh
pnpm exec vitest run src/lib/sessionCollaboration.test.ts
pnpm typecheck
cargo test --manifest-path src-tauri/Cargo.toml session_mail::tests
\`\`\`

Physical desktop acceptance must additionally launch **two separate Little
Monkey processes under the same test profile**, enable collaboration in their
settings, create two sessions with the same attached workspace, send from one
process, close/relaunch the second and verify that its transcript contains the
message once; also test a running streamed turn, idle-window delivery and a
deliberately interrupted session persistence. None of these steps needs
connection to a remote peer.

**Boundary:** This is genuine durable *local* desktop messaging, not a
JCode-compatible server-wide session bus or cross-host peer channel. Delivery
is **at least once to the consumer until acknowledged**, with idempotence
in the normal persisted transcript. Because frontend chat sessions themselves
are stored in a file and concurrently edited by independent windows, this
does not yet guarantee general exactly-once mutations under adversarial
simultaneous multiwindow writes. Existing daemon workflows have their own
separately authenticated message transports. Do not claim general
cross-device agent messaging or auto-resumption from this implementation.


## Real multi-process crash and stress acceptance

The Linux/macOS/Windows Rust test suite includes OS-child-process
acceptance, not only a mock of \`invoke\`:

- \`process_killed_after_sqlite_commit_redelivers_across_reopen\` spawns
  an independent sender, observes its physically committed SQLite marker,
  forcibly kills it before recipient acknowledgement, opens the database
  in fresh connections, verifies redelivery, and acknowledges exactly once
- \`concurrent_independent_os_processes_replay_same_send_only_once\` forces
  four separate sender processes to reuse an identical idempotency key,
  expecting exactly one persisted row
- \`independent_os_process_stress_survives_fresh_reader_connections\`
  launches four distinct native processes, each sending a bounded sequence
  of unique messages to one SQLite inbox, then uses fresh recipient
  connections to verify every row and acknowledge it. The CI default is
  12 messages per sender; locally run \`LM_MAIL_STRESS_COUNT=60 cargo test\`
  to exercise a longer 240-message workload

On Windows, WAL cold-start schema acquisition now retries **only**
\`SQLITE_BUSY\`/\`SQLITE_LOCKED\` with a bounded deadline; unrelated
database corruption, permission failures or validation errors are never
silently retried. Child stderr is captured on failure for diagnosis.

These tests run with actual independent operating-system processes and disk
transactions but do not launch multiple installed Tauri desktop windows,
simulate true host power loss, or verify cross-device network transports.
Those require separate physical operator acceptance and are not claimed.

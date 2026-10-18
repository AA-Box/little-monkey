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

## Live safe-point delivery and local broadcast

An enrolled executing desktop turn now checks its durable SQLite inbox **between** model requests, after tool results are complete and before another assistant placeholder starts streaming. Messages are appended as explicitly untrusted peer content. The transcript is flushed before acknowledgement; after crashes, unread SQLite rows replay. A message arriving during uninterrupted token streaming waits until the next safe point or next user turn; it never mutates an incomplete assistant response.

The new opt-in `session_broadcast_message` agent tool sends the same bounded notice to all currently registered peers in the same workspace, partitioned into deterministic per-recipient send identities. It never sends to outside workspaces, wakes dormant sessions, or escalates tool rights. A partially successful broadcast reports an error, but already committed messages remain durable.

These improvements still **do not** implement the daemon's server-managed active-agent directory or automatic file read/edit conflict detection; the SQLite mailbox remains desktop-scoped.

Run `pnpm exec vitest run src/lib/sessionCollaboration.test.ts && pnpm typecheck` and independently verify two desktop processes against the same profile and workspace, a running turn receiving mail after its next model/tool boundary, and crash recovery before/after ack.

## Automatic file-read/edit awareness

A successful desktop `read_file` result now registers the real on-disk SHA-256
under its enrolled session ID and canonical workspace-contained relative path.
After a successful `write_file` or `edit_file`, the native observer checks
up to 24 same-workspace peers with an older read hash. The observation and
file-change notices commit in one SQLite `IMMEDIATE` transaction with
per-tool-call idempotency; a crash cannot leave an event recorded without its
corresponding notices. The existing live safe-point mailbox then shows those
notices as untrusted data to an executing recipient. It does not follow a
message as an instruction and does not automatically merge or overwrite files.

This is opt-in and tracks regular files no larger than 8 MiB, for workspaces
whose local peer sessions are currently registered. A managed agent worktree
is intentionally excluded: a child worktree cannot masquerade as a reader
of its parent's original checkout. The tracker never copies file content
into mailbox messages. The subsystem runs **after** a successful tool call,
so a failure of optional observation cannot cause re-execution of a write.

**Remaining scope limit:** This is a desktop/native SQLite coordinator shared
between multiple desktop processes on the machine. It is NOT an authenticated
network daemon directory. Headless daemon agents participate only when the
separate resident-process opt-in is explicitly configured (see below). An external editor edit also is not observed
until a Little Monkey file tool publishes a change. A read-versus-write race
between the original tool and observer hash remains a possible blind spot.

```sh
pnpm exec vitest run src/lib/sessionCollaboration.test.ts
cargo test --manifest-path src-tauri/Cargo.toml file_change_notifications_are_atomic_scoped_and_idempotent
```

## Resident daemon agent integration (explicit opt-in)

The daemon's native CLI agent now joins the same profile-local SQLite peer
directory when the service process is started with
`LITTLE_MONKEY_AGENT_COLLABORATION=1`. Joining additionally requires a
workspace-bound, **durable run-event identity**; a one-off non-durable terminal
chat cannot impersonate an executing daemon job.

After enrollment, the executing resident agent receives a
`local_agent_message` tool with same-workspace DM and broadcast options. It
checks the enrolled peer directory at dispatch (not merely in the model's
tool list) and uses SQLite's transactional, idempotent send. Successful native
`read_file`/`write_file`/`edit_file` calls use the exact same on-disk
change tracker as the desktop. Pending messages are delivered between
completed model calls, wrapped as untrusted data; the durable run ledger
records the notification before the profile-local inbox is acknowledged.

The runtime does **not** start a network-accessible server, grant filesystem
rights, or elevate sending peers to system/user instruction status. Sender
permissions and the model's normal file permission policy stay unchanged.

```sh
# Set this for the daemon user service environment, then restart it deliberately
# only when the existing run policy permits an interruption.
LITTLE_MONKEY_AGENT_COLLABORATION=1 monkey --workspace /path/to/repo daemon serve
cargo test --manifest-path src-tauri/Cargo.toml real_sqlite_backed_local_direct_and_broadcast
```

**Evidence still needed:** a full physical multi-process concurrency test
including a real running daemon and two real model-backed workspaces, Windows
service propagation of this environment toggle, and an end-to-end delivery
through a real streaming model. The mailbox integration does not substitute
for these tests or for remote authenticated multi-host messaging.

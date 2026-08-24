# Execution targets

Little Monkey separates the model target from the executor. A frozen
`ExecutionTargetSnapshot` records the executor identity, protocol, capabilities,
trust state, and probe time in the `RunSpec`; changing or deleting a configured
target therefore does not rewrite historical runs.

Supported target kinds are `local`, `docker`, `remote_node`, and `ssh_runner`.
They implement the same probe/capability/workspace/run/event/status/control/
artifact/result/cleanup contract. A target must satisfy the run's required
capabilities before submission; the scheduler never silently falls back to the
local host.

## Workspace transfer

The coordinator creates a content-addressed `WorkspaceTransfer`. Clean Git
workspaces retain remote URL, commit, branch, and sparse-scope metadata. Dirty
Git workspaces carry the base commit, tracked diff digest, and untracked paths.
Non-Git workspaces use a snapshot. Manifests contain relative paths, type,
size, SHA-256, executable mode, and safe symlink metadata. Limits reject
traversal, absolute paths, special files, unsafe links, collisions, and
oversized transfers. Executors materialize only under:

```text
<runner-data>/workspaces/<workspace-id>/<snapshot-id>
```

Policies are `ephemeral`, `cached`, or `persistent`. Results contain the base
and resulting snapshot digests, diff, changed/deleted files, binary metadata,
artifacts, and verification evidence. Results are reviewable/exportable and
are not applied to the user's checkout automatically. `workspace result` checks
the base digest and runs `git apply --check` before mutation.

## Docker

Docker is probed through the Docker CLI and records both server and image
identity. Runs use a read-only root filesystem, a private PID namespace,
bounded PIDs/CPU/RAM, a workspace-only mount, explicit environment forwarding,
and `--network none` when outbound network is not frozen as allowed. No
privileged flag, Docker socket, or arbitrary host mount is accepted. Remote
Docker requires a separately configured target; the host socket is never
exposed implicitly.

Build the reusable runner image from a pinned base and the staged CLI:

```sh
docker build --pull=false -f docker/execution-runner/Dockerfile \
  --build-arg MONKEY_BINARY=src-tauri/target/debug/monkey-cli -t little-monkey-runner:local .
```

## SSH runner

SSH invokes `monkey runner serve --stdio`, which is the same newline-delimited
execution protocol rather than a second agent implementation. `BatchMode`,
strict host-key checking, an absolute `known_hosts`, optional key reference,
user, port, and jump host are supported. Private key bytes never enter app
JSON. A missing or incompatible runner returns a protocol/install error; the
app never uses `curl | sh`.

## CLI and UI

```sh
monkey targets list
monkey targets probe <id>
monkey targets add docker <id> <image>
monkey targets add ssh <id> <host> --known-hosts /absolute/known_hosts
monkey targets remove <id>
monkey workspace push <path> --output transfer.json
monkey workspace result result.json --workspace <path> --base-digest <digest>
monkey task start "objective" --target <model-or-target> --workspace <path>
```

Settings → Execution targets shows probe status, capability snapshot, runner
identity, and trust state. The autonomous-task composer provides Automatic or
a configured executor override. Existing paired-node placement and K18
checkpoint/process migration remain the authoritative remote-control and live
migration paths; this layer does not replace them.

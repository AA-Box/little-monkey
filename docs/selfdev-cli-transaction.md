# Safely rebuilding and activating self-developed Little Monkey CLI binaries

This is a **real transactional native CLI development channel**, not a way
for an agent to silently replace a signed desktop app.

## Operator workflow

Use a clean, trusted Git checkout with Rust, Node, pnpm and native libraries
available. Run:

\`\`\`sh
node scripts/selfdev-transaction.mjs build --repo /absolute/path/to/little-monkey
\`\`\`

The command rejects dirty source, executes the actual frontend typecheck
and Rust library tests, builds a release \`monkey-cli\`, then stages it
under \`~/.littlemonkey/selfdev/versions/<actual-sha256>/\`. A manifest
captures source commit, binary SHA-256 and version stdout. Nothing is
activated as part of build; this also works for a user-supplied previously
built candidate via \`stage --candidate /absolute/path/to/monkey-cli\`.

Review the hash and version, then activate explicitly:

\`\`\`sh
node scripts/selfdev-transaction.mjs activate --confirm-sha <64-char-sha256>
node scripts/selfdev-transaction.mjs status
\`\`\`

Activation rechecks the staged bytes and \`--version\` health, then swaps
**only the manager-owned symlink** \`current\` atomically. It saves the
previous known-good activation. A failed post-swap health check immediately
restores the previous symlink. To return to the previous binary:

\`\`\`sh
node scripts/selfdev-transaction.mjs rollback --confirm-rollback
\`\`\`

Installations outside the dedicated manager root, running desktop bundles,
system package managers, daemon services and active model executions are
**never overwritten or restarted by this script**. To use the staged CLI,
explicitly use \`~/.littlemonkey/selfdev/current/monkey-cli\` in your shell
or developer-only launcher; the app does not alter \`PATH\`.

Test the actual local filesystem and executable switch behavior:

\`\`\`sh
pnpm test:selfdev:contract
\`\`\`

Security properties: user-specified SHA confirmation, isolated managed
versions, no silent update, staged binary hash verification, clean source
validation, full preflight tests, atomic POSIX symlink swaps, persisted
rollback target, and code refusal on tampered candidates. The manager
accepts only operator-staged executables, and running any candidate is
equivalent to running native code with the developer's privileges.

## Still NOT complete

It is **not** production-grade automatic live-core hot reload. Replacing
a running daemon's executable requires authenticated instance draining,
session handoff, independent health checks after restart, pinned signed
desktop update artifacts, platform-specific notarization/signing, and
a supervised timeout-and-rollback protocol. The existing signed desktop
updater and \`update_rollback.rs\` remain the correct production upgrade
path. Do not point this development script at a system or signed installation.

Real on-device acceptance of a release build and operator-approved
activation needs an installed Rust toolchain and an actual target machine.
The CI contract verifies native symlink/rollback behavior on ephemeral
non-privileged binaries; it does not exercise a production daemon swap.

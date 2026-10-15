# P2 — provider sign-in and coding-session migration

This change adds an **official, supported account authorization flow** for
OpenRouter and imports **resumable** conversation history from four coding
agents. It does **not** transfer approval grants, tool execution state,
permissions, hidden reasoning, or subscription cookies from another product.

## Provider sign-in

### Desktop

1. Open **Settings → Providers → OpenRouter**.
2. Select **Sign in with OpenRouter**.
3. Finish authorization in the system browser. The callback listener is bound
   exclusively to `127.0.0.1` using an OS-assigned port.
4. Little Monkey exchanges the authorization code over HTTPS, validates the
   request-specific CSRF state, and writes the resulting API key to the OS
   keychain. The existing OpenRouter model list and streaming chat then work
   without copying a key into Settings.
5. **Remove key** revokes the *local stored credential*. For revocation on
   OpenRouter itself, also revoke the key in your OpenRouter account.

**Sign in with another OpenRouter account** repeats consent and replaces the
stored key only after a successful code exchange. An existing working key is
left in place on denial, timeout, state mismatch or failed exchange.

### CLI / unattended daemon

```shell
monkey providers login openrouter
# For SSH, containers, and remote/headless machines:
monkey providers login openrouter --headless
```

The CLI displays the authorization URL and tries to open a local browser on
macOS/Linux. Open it on the **same machine** that is running the CLI and finish
the flow. It stores the token from the CLI executable, avoiding macOS keychain
access prompts when unattended CLI/daemon jobs read their own credentials.
The CLI must have access to the OS keychain. For remote SSH servers or containers, `--headless` generates an S256
PKCE challenge without any loopback listener. Open the displayed OpenRouter
URL on another device, approve access and paste the one-time code into the
terminal. The authorization code is never passed as a command-line argument
or saved to disk. The normal keychain must be accessible to the CLI. When
OAuth is unavailable, use `monkey providers set-key openrouter` with an
authorized user-controlled API key (read from standard input).

OpenRouter's OAuth PKCE endpoint grants a **user-controlled API key**. It is
not a free inference service and is subject to OpenRouter billing and policy.
OpenAI/Anthropic/Gemini subscription accounts do not automatically provide
general model API authorization. For those providers, use their supported API
keys or an explicitly supported connector; copying session cookies or reusing
other CLIs' private credentials is out of scope.

## Migrating coding sessions

1. Obtain a **session export or JSONL transcript** from one of the supported
   coding agents. This feature does not scan or read agents' hidden home
   directories automatically.
2. Open **Settings → Portability → Import coding-agent sessions**.
3. Select up to 20 `.jsonl`/`.json` files, each at most 32 MiB.
4. Open the imported session in Chat, choose a Little Monkey model, and select
   the correct workspace explicitly before sending the next user prompt.

| Source | Supported format |
| --- | --- |
| Claude Code | A JSONL session under the user's chosen `.claude/projects` transcript export |
| Codex CLI | A `.codex/sessions/**/rollout-*.jsonl` transcript |
| Pi | A v2/v3 tree-structured `.pi/agent/sessions/**/*.jsonl` file; active leaf |
| OpenCode | A JSON export with `info` and `messages` (each with `info`/`parts`) |

The importer preserves visible **user** and **assistant** text only.
System instructions, tool outputs and tool calls, reasoning, model settings,
credential references, remote workspace paths, agent permissions and queued
tasks are discarded. No imported tool call is executed during migration. The
result is a native, persistent chat session, not a Knowledge 2.0 document.

Files are identified by a SHA-256 digest of their bytes and source format.
Selecting an identical file again makes no duplicate session. Import is
committed with the existing atomic `sessions_save` path before the UI updates.
A pending local session save is flushed first, and imports are refused while
local agent runs are active.

### Tests and acceptance

```shell
pnpm exec vitest run src/lib/codingSessionImport.test.ts
cargo test --manifest-path src-tauri/Cargo.toml provider_oauth
pnpm build
```

Manual provider acceptance requires a real OpenRouter account: sign in from the desktop, test CLI localhost mode and CLI `--headless` through a remote/SSH session, list
models, stream a completion, restart, and repeat using the saved key. Run the
CLI path separately with a daemon using the same CLI-owned credential, and
verify a revoked/invalid key is surfaced as an error rather than falsely
reported as connected.

Manual migration acceptance requires actual exported transcripts from all
four products: select files, verify the new sessions appear in Chat, restart
the app, send a new prompt, and confirm no historical tool actions replay.
Test re-import idempotence and cancellation, plus rejection of malformed and
oversized files.

Automated parser and state/URL unit tests do not substitute for these live
cross-product acceptance runs.

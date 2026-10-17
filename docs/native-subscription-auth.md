# Vendor-authenticated native agent integration — without token interception

The native bridge is a user-invoked Little Monkey developer integration,
not a hidden replacement for an API key or a fabricated OAuth client ID.
The official installed vendor binary owns its own sign-in, keychain and
credential refresh. **Little Monkey reads zero credential bytes.**

## OpenAI Codex (official ChatGPT sign-in)

Install an official Codex CLI and log in using its supported flow:

\`\`\`sh
codex login
node scripts/subscription-native-auth.mjs codex status
node scripts/subscription-native-auth.mjs codex run \
  --prompt 'Briefly explain the strengths of structured grep' \
  --consent
\`\`\`

Before any delegated Codex run, the bridge requires the official login
status output to explicitly identify **ChatGPT sign-in**. A successful
API-key-only login is refused rather than misreported as subscription
access. Delegation also strips inherited `OPENAI_API_KEY`, `CODEX_API_KEY`
and custom OpenAI/Codex endpoint overrides, preventing silent metered
API-key fallback. These checks fail closed if the official CLI changes
its status format.

The bridge verifies \`codex login status\`, then calls the official
\`codex exec --json --ephemeral --sandbox read-only\` in a newly created
private empty workspace. No external repository is attached, no API tokens
are parsed, and only confirmed completed assistant messages are returned;
raw stderr, auth state, environment details, tool streams and account data
are never returned. The temporary directory is removed on exit. Access
depends entirely on the installed Codex CLI and the operator's plan/access.

This bridge is invoked explicitly from the developer's shell. It does
**not** register Codex as a transparent Little Monkey provider, grant
a model interactive shell access, or bypass Codex's own permissions.

## Anthropic Claude

Anthropic's current policy says third-party tools should use Console
API keys or an authorized provider, and forbids routing subscription
traffic against Claude subscription limits by impersonating native
applications. No Claude OAuth token, local client credential, or
subscription proxy is read or implemented here.

The supported workflow is to open the official Claude Code application
and let it handle subscription login and usage:

\`\`\`sh
node scripts/subscription-native-auth.mjs claude status
node scripts/subscription-native-auth.mjs claude login --consent
node scripts/subscription-native-auth.mjs claude open --consent
\`\`\`

For direct use of Claude as a Little Monkey model backend, use a
properly authorized Anthropic API key or supported cloud provider via
the existing provider configuration. Usage/billing differ from a
Claude Pro/Max subscription.

## Verification

\`\`\`sh
pnpm test:native-auth:contract
\`\`\`

Tests invoke **real child OS executables** with controlled fake vendor CLI
protocols, proving argument isolation, user consent, bounded official
assistant output, API-key-login refusal, credential-override scrubbing and no credential-file access. They do NOT authenticate
against a real ChatGPT/Claude account. Full live account acceptance
requires a consenting operator to run the official CLI on their own
machine and review native provider session/billing policies. Never
paste or upload \`auth.json\`, bearer tokens or refresh credentials.

Official guidance:
- https://help.openai.com/en/articles/11369540-using-codex-with-your-chatgpt-plan
- https://support.claude.com/en/articles/13189465-log-in-to-your-claude-account

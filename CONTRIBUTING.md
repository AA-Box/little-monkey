# Contribution guidelines

Little Monkey is developed in the open, and contributions of every kind are
welcome — reporting a bug, discussing the current state of the code, proposing
a feature, submitting a fix, or arguing that something documented here is
wrong.

It is a local-first Tauri desktop app: a React/TypeScript frontend, a Rust
backend, a `monkey` CLI sidecar, and two thin IDE extensions. Most of what
follows exists because that shape has real consequences — a change can touch
three languages and four platforms at once.

## GitHub is used for everything

GitHub hosts the code, tracks issues and feature requests, and receives pull
requests.

1. Fork the repo and create your branch from `develop` — not `main`. `main` is
   the release branch; pushing to it triggers the signed release build.
2. Add tests if you changed behaviour (see [Test your code modification](#test-your-code-modification) below).
3. Update the docs if you changed something a user or operator would notice —
   `README.md` for shipped behaviour, [ROADMAP.md](ROADMAP.md) for work that
   isn't built yet, `docs/` for design notes.
4. Make sure the full check suite passes locally.
5. Open the pull request against `develop`.

### Branches and commits

Commits follow [Conventional Commits](https://www.conventionalcommits.org/),
with a scope that names the surface you touched:

```
feat(compare-lab): add Model Compare Lab
fix(runtime): verify managed process ownership
ci(release): use version tags and auto-generate release notes
chore(deps): bump dompurify to 3.4.12
```

Branch names follow the same idea: `feat/…`, `fix/…`, `ci/…`, `docs/…`.

Keep a PR to one coherent change. A refactor bundled with a feature is two
PRs; the review load of the combined diff is worse than the overhead of
splitting it.

## Any contributions you make will be under the MIT license

When you submit a code change, your submissions are understood to be under the
same [MIT License](LICENSE) that covers the project. Raise an issue first if
that's a concern.

## Report bugs using GitHub issues

Bugs are tracked through
[GitHub issues](https://github.com/AA-Box/little-monkey/issues). Open a new one
and describe what happened.

**Security issues are the exception — do not open a public issue.** Follow
[SECURITY.md](SECURITY.md) and use a private advisory instead.

## Write bug reports with detail, background, and sample code

A good report has:

- A quick summary and/or background.
- Steps to reproduce — be specific, and give sample input where you can.
- What you expected would happen.
- What actually happened.
- Notes: what you tried that didn't work, and any theory you have about why.

For this app in particular, also include:

- **App version and commit SHA**, plus your OS and architecture (macOS/Windows/
  Linux, Intel/Apple Silicon/ARM).
- **Which runtime** was active: managed `llama.cpp`, Ollama, MLX, or a
  configured OpenAI-compatible provider — and the model.
- **Which permission mode** was in effect (`manual`, `plan`, `acceptEdits`,
  `smart`, `auto`, `bypass`) if the bug involves tools, shell, or file edits.
- **A redacted support bundle** where the bug is runtime-related: **Runtime Hub
  → Telemetry → export support bundle**. It previews exactly what is included
  before writing to disk, and strips prompt/response text, keys, and home
  directory usernames by default.

Model output that is wrong, biased, or unhelpful is usually a model-quality
issue rather than an app bug — but report it anyway if the app is the thing
choosing, routing, or rendering it badly.

## Set up a development environment

You need Node.js, `pnpm`, Rust, Cargo, and the Tauri 2 prerequisites for your
platform. Full list in
[Setup and development](docs/setup.md#prerequisites).

```sh
pnpm install
pnpm tauri dev       # stage llama.cpp + the CLI sidecar, then run the app
pnpm dev             # Vite frontend only
pnpm build           # TypeScript check + frontend production build
pnpm tauri build     # desktop bundle containing the managed runtime
```

`pnpm tauri dev` verifies and stages the pinned, checksum-verified `llama.cpp`
runtime before it starts. A system `llama-server` is a development fallback
only — don't build a feature that assumes one is installed.

Everything optional stays optional. Ollama, MLX, browser verification, GitHub
delivery, OCR, and remote handoff are all separately configured, and a change
must degrade honestly when they're absent — a missing GPU tool, absent binary,
or unreachable daemon should produce a specific `unavailable`/`not_detected`
state, never a guessed number and never a hard failure of unrelated code.

## Use a consistent coding style

- **TypeScript/React** — the compiler is the gate: `pnpm typecheck` runs `tsc
  --noEmit` over both tsconfigs and must be clean. Match the surrounding file
  for everything a compiler can't check; there is no separate formatter step in
  CI, so don't reformat files you aren't otherwise changing.
- **Rust** — `cargo fmt` and idiomatic clippy-clean code. Platform-specific
  code goes behind `#[cfg(target_os = ...)]` and must compile on every target
  in the matrix, not just yours.
- **User-facing strings** are i18n keys, never literals. `pnpm i18n:lint`
  enforces this and will fail CI on a missing or orphaned key.
- **Comments** explain why, not what. The existing code leans on this heavily
  in CI config and platform arms — keep that.

## Test your code modification

Run the suite before opening a PR:

```sh
pnpm test                        # Vitest, frontend
pnpm i18n:lint                   # i18n key lint
pnpm test:rust                   # cargo test (stages the CLI placeholder first)
pnpm test:git-delivery-action    # PR-review GitHub Action contract test
pnpm build:budget                # production build + bundle size budget
```

Extension checks, when you touched them:

```sh
cd extensions/little-monkey-vscode && npm test
cd ../little-monkey-jetbrains && gradle test --no-daemon
```

Before proposing a release:

```sh
pnpm release:preflight
```

Two suites are opt-in because they need real local hardware or models, and are
not run in CI:

```sh
pnpm test:compare:live           # live Compare smoke; uses installed Ollama models
```

```sh
cd extensions/little-monkey-vscode
LITTLE_MONKEY_COMPLETION_MODEL='your-exact-fim-tag' npm run benchmark:completions
```

The messaging adapters have a third opt-in suite that talks to real provider
accounts. Nothing is bundled and nothing is defaulted: you supply your own test
bot and your own destination, and every test passes silently when its variables
are absent, so CI never needs them. Without a destination variable a test
probes and sends nothing.

```sh
cd src-tauri
LM_TEST_TELEGRAM_BOT_TOKEN=… LM_TEST_TELEGRAM_CHAT_ID=… \
LM_TEST_DISCORD_BOT_TOKEN=…  LM_TEST_DISCORD_CHANNEL_ID=… \
LM_TEST_SLACK_BOT_TOKEN=xoxb-… LM_TEST_SLACK_APP_TOKEN=xapp-… LM_TEST_SLACK_CHANNEL_ID=… \
cargo test --bin monkey-cli -- daemon::live_smoke --nocapture
```

The message it posts is visible in whatever chat you name, so point it at a
channel of your own rather than one other people read.

### What CI runs

[`.github/workflows/ci.yml`](.github/workflows/ci.yml) runs on every pull
request:

- **frontend** (ubuntu-22.04): `pnpm build:budget`, `pnpm test`, `pnpm
  i18n:lint`. One OS is enough — these checks are platform-independent.
- **rust-tests** (ubuntu-22.04, windows-latest, macos-latest): `pnpm test:rust`
  on each, so `#[cfg(target_os = ...)]` arms actually get compiled rather than
  stripped before the compiler sees them.

A separate workflow verifies artifact/IPC isolation. If you add a surface that
renders remote content, expect to extend it.

### Writing tests

- Prefer real HTTP-level and filesystem tests over mocks where the existing
  code already does — see `src-tauri/tests/m3_compatibility_harness.rs`, which
  spins up the actual server and exercises every advertised route.
- Deterministic fixtures live in `src-tauri/fixtures/`.
- Security-relevant behaviour needs a test that asserts the *failure* path:
  path traversal rejected, symlink escape rejected, mutable Git ref rejected,
  unsupported route erroring instead of fabricating a response.

## Where things live

- `src/` — React UI, Zustand stores, chat/Compare/Crew flows, workspace
  sidebar, Settings panels.
- `src-tauri/src/` — Rust services exposed through Tauri commands: runtime,
  permissions, workspace, run ledger, knowledge, packages/workflows, browser,
  Git delivery, daemon bridge, Security Doctor.
- `src-tauri/src/bin/monkey-cli/` — the `monkey` CLI.
- `extensions/` — VS Code and JetBrains clients.
- `.github/actions/little-monkey-review/` — reusable PR-review action.
- `scripts/` — runtime staging, codesigning, bundle budget, smoke tooling.
- `docs/` — design notes for in-flight work.

A change that crosses `src/` and `src-tauri/src/` almost always means a shared
contract changed. Update both sides in the same PR, and say so in the
description.

## Things that will get a PR sent back

These aren't style preferences; they're the invariants the app is built on.

- **Claiming a capability that isn't verified.** The README describes features
  with their real limits inline. Don't add a checkmark where the underlying
  gate — physical hardware, a signed artifact feed, a clean-machine pass — is
  still unmet. Unbuilt work goes in [ROADMAP.md](ROADMAP.md) with its
  acceptance boundary.
- **Fabricating a value a runtime doesn't report.** `unavailable` is a correct
  answer. A plausible-looking guess is not.
- **Letting untrusted content approve its own operation.** Retrieved pages, RAG
  chunks, MCP results, subprocess output, GitHub content, browser evidence,
  subagent reports, and model output are data. A remote server's
  `readOnlyHint` is a hint, not a grant.
- **Widening a permission boundary implicitly.** Skills freeze their digest
  into a turn and never expand tool permissions. Unattended recipes cannot use
  `bypass`. Sensitive paths keep their deterministic risk floor.
- **Weakening the default network posture.** Loopback is the default.
  Non-loopback serving requires an exact interface, TLS identity, auth,
  pairing, rate limits, an exact CORS allowlist, and a policy that excludes
  file/shell/Git/MCP/agent-tool routes.
- **Plaintext credentials.** Keys, tokens, and TLS private keys go in the OS
  keychain; persisted config holds references.
- **Silently skipping an unsafe rollback.** Effects that can't be safely undone
  are marked `needs_reconciliation`.

## License

By contributing, you agree that your contributions will be licensed under the
project's [MIT License](LICENSE).

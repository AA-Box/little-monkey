<!--
Target `develop`, not `main`. `main` pushes trigger the signed release build.
Commit subjects follow Conventional Commits: feat(scope): …, fix(scope): …
-->

## What and why

<!-- One paragraph. What changed, and what problem it solves. -->

## Surfaces touched

<!-- Delete what doesn't apply. -->

- [ ] `src/` — React UI / stores
- [ ] `src-tauri/src/` — Rust services / Tauri commands
- [ ] `src-tauri/src/bin/monkey-cli/` — `monkey` CLI
- [ ] `extensions/` — VS Code / JetBrains
- [ ] `.github/` — CI, release, or the PR-review action
- [ ] Docs only

If this crosses `src/` and `src-tauri/src/`, a shared contract changed — say
which one:

## Checks

- [ ] `pnpm build:budget`
- [ ] `pnpm test`
- [ ] `pnpm i18n:lint`
- [ ] `pnpm test:rust`
- [ ] `pnpm test:git-delivery-action` (if the review action changed)
- [ ] Extension suites (if `extensions/` changed)

Ran on: <!-- macOS / Windows / Linux, arch -->

Anything not run locally, and why:

## Invariants

Confirm each, or explain why it doesn't apply:

- [ ] No capability claimed beyond what's verified. Unmet gates stay in
      [ROADMAP.md](../ROADMAP.md); README limits stay inline and honest.
- [ ] No fabricated values — anything a runtime doesn't report is
      `unavailable`/`not_detected`, not a guess.
- [ ] Untrusted content (retrieved pages, RAG chunks, MCP results, subprocess
      output, GitHub content, browser evidence, subagent reports, model output)
      cannot approve its own operation.
- [ ] No permission boundary widened: skill digests stay frozen per turn,
      unattended recipes still cannot use `bypass`, sensitive-path risk floor
      intact.
- [ ] Network posture unchanged, or the non-loopback requirements (interface,
      TLS identity, auth, pairing, rate limits, CORS allowlist, excluded
      file/shell/Git/MCP routes) are all still enforced.
- [ ] Credentials go to the OS keychain; persisted config holds references only.
- [ ] Optional dependencies (Ollama, MLX, GPU tooling, browser, `gh`) still
      degrade honestly when absent.

## Security

- [ ] This PR has no security impact.
- [ ] This PR touches a surface listed in [SECURITY.md](../SECURITY.md) scope —
      explained below.

<!--
Do NOT report a vulnerability here. Use a private advisory:
https://github.com/AA-Box/little-monkey/security/advisories/new
-->

## Screenshots / output

<!-- UI changes: before and after. CLI or runtime changes: paste the output. -->

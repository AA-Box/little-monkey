# ROADMAP.md Audit — Little Monkey

17 claims checked (all "current baseline" + M1.1 items ROADMAP.md calls shipped), plus one spot-check on whether M1.2–M8 really are still "future work." Each claim independently verified against real source (not roadmap text) — Rust backend, Tauri command registration, and actual reachable UI wiring, with file:line citations.

# Audit Report: ROADMAP.md Claim Verification (17 Claims)

## 1. Overall Verdict

The roadmap is **substantially honest** about what it claims is already shipped. All 15 "implemented_e2e" claims (B1–B10, C1–C5) were independently verified to have real backend logic, real Tauri command registration, and real, reachable UI wiring — not stubs, dead routes, or mocked dispatch — and the great majority carry passing automated tests as corroboration. The only defects found in this "done" bucket are stale doc-comments (e.g. `verify.rs`, `mcp.rs`) that lag behind functionality that actually works, which is a documentation hygiene issue, not overclaiming. Two claims broke from this pattern and deserve scrutiny: **C6** makes a specific factual assertion (a "3 local + 1 cloud" test run) that is contradicted by the cited script's own default behavior and has zero committed evidence it ever ran; and **C7**'s spot-check reveals the opposite failure mode — the roadmap's blanket "M1.2–M8 remains future work" line understates reality, since large, working, UI-wired, test-passing implementations of M1.2 through M6B already exist in the working tree. Critically, none of that C7 code (nor ROADMAP.md itself) is committed to git, so the discrepancy is as much a **process/hygiene risk** (large uncommitted scope sitting on disk) as it is a documentation accuracy problem. Net: for "done" items the roadmap is not overclaimed; for "not-yet-done" items it is stale/understated in a way that reflects uncommitted work rather than deliberate misrepresentation.

## 2. Claim Verdict Table

| Claim | Verdict | Key evidence | Key gap |
|---|---|---|---|
| B1 model targets (llama.cpp/Ollama/BYOK) | Confirmed e2e | Three adapters (llama.rs, ollama.rs, providers.rs) reachable via ModelManager/OllamaPanel/ProviderCard → modelStore → agentLoop → turnEngine dispatch | Ollama's own `serve` process is never stopped by the app (documented asymmetry, not a defect) |
| B2 workspace tools (7 kinds) | Confirmed e2e | All 7 tool kinds in tools.rs/memory.rs/web.rs, generically dispatched via `invoke('tool_${name}')`, gated by `permissions::request_permission` | Read-only tools (read_file/list_dir/grep/glob) intentionally ungated — claim's "gated by permission system" wording is unqualified |
| B3 permission modes (6 modes) | Confirmed e2e | `mode_short_circuit` decision table; bypass hard-banned for recipes and cron schedules; 53 passing Rust tests | - |
| B4 checkpoints + verification | Confirmed e2e | checkpoints.rs snapshot/revert/reapply; verify.rs + agentLoop.ts bounded feedback loop (`verifyMaxRounds`) | `verify.rs:1-8` module doc comment is stale, still describing feedback-to-model as unimplemented |
| B5 sessions/fork/groups/split/concurrent streaming | Confirmed e2e | Per-session `AbortController` map allows two independent streams in split view; dedicated concurrency test passes | - |
| B6 rules/memory/personas/stacks | Confirmed e2e | rules.rs/memory.rs/prompts.rs/stacks.rs all inject into system prompt or tool results every turn, not just stored on disk | - |
| B7 MCP clients (stdio+HTTP) | Confirmed e2e | Both transports real; allowlist + permission gate enforced server-side; real stdio/HTTP integration tests | Stale doc comments (`mcp.rs:857`, `mcpStore.ts:9`) still say HTTP is unsupported/"phase 4" |
| B8 local OpenAI-compatible API server | Confirmed e2e | Real hyper server bound to 127.0.0.1 only, byte-level SSE passthrough, scoped bearer tokens; 74 passing tests | No live GUI+curl smoke test performed (verified via source + unit tests only) |
| B9 subagents/artifacts/recipes/schedules/CLI | Confirmed e2e | Hard-bounded subagents, capped artifact store, recipe validator banning bypass; monkey-cli live-tested (`task list/validate/run/schedule`) | Daemon binary lifecycle and a live model-backed `task run` were not exercised |
| B10 git status/commit + disclosed diff gap | Confirmed (gap accurately disclosed) | git_status/git_commit/worktree detection real+wired; no diff-retrieval command exists anywhere reachable from desktop UI | Roadmap's own stated limitation is accurate and still true — not stale |
| C1 ModelTargetSnapshot | Confirmed e2e | Tri-state capability model in TS + parallel Rust enum; compareRunner deep-clones/detaches before awaits (tested) | Two independently-maintained representations (TS vs Rust) kept in sync by convention, not one canonical type |
| C2 Compare fan-out | Confirmed e2e | One frozen snapshot fanned to 2–4 independent branch sessions, each own AbortController/stream; concurrency test passes | Managed llama.cpp capped to one selectable target rather than true N-way concurrency for that runtime |
| C3 Compare persistence | Confirmed e2e | Branch status/timing/usage/error persisted in SessionGroup; reload marks orphaned running/queued branches "failed", not zombie spinners | Reload can't resume an actual in-flight HTTP stream (inherent, correctly surfaced, not a defect) |
| C4 memory-pressure queueing | Confirmed e2e | Real `system_memory_info` syscall + Ollama `/api/ps` residency snapshot drive sequential queueing above 80% memory estimate | Heuristic is a static pre-launch estimate, not live RSS/VRAM monitoring; live 4-target smoke run not executed |
| C5 opt-in synthesis (no tools) | Confirmed e2e | Empty `tools: []` sent to model + hard-fail on any tool_call attempt; frozen deep-cloned sources; 28 passing tests | Not manually click-tested in the built app; other i18n locales not checked for missing keys |
| C6 "3 local + 1 cloud" test coverage | Partial / overclaimed detail | Real Ollama residency unit tests (4/4 pass) and a genuine opt-in live smoke script exist | Script's own default **excludes** cloud tags — opposite of the claimed composition; no CI/log/commit shows it ever ran with a cloud tag |
| C7 M1.2–M8 "not yet done" spot-check | Partial / stale roadmap status | M1.2, M2, M3, M4, M5.1/5.3/5.4, M6A/6B all have substantial working, UI-wired, test-passing code already on disk (145+ passing tests) | All of it is uncommitted ("??" in git status), including ROADMAP.md itself; M5.2 autocomplete, all of M7, and M8 hardening are genuinely absent |

## 3. Confirmed Working End-to-End

- **B1** — llama.cpp, Ollama, and BYOK/cloud adapters each have real process/HTTP lifecycle code and are reachable from ModelManager/OllamaPanel/ProviderCard, routed through a genuine chat-dispatch pipeline (not mocked).
- **B2** — All seven tool kinds have non-stub Rust implementations, generic dispatch by name, and real permission-gated approval UI (PermissionModal).
- **B3** — Six permission modes drive a tested decision table; unattended `bypass` is independently blocked for both recipes and cron schedules.
- **B4** — Checkpoint snapshot/revert/reapply and post-edit verification with a hard-capped repair-round loop are both real and wired into the transcript UI.
- **B5** — Fork, groups, split-pane chat, and genuinely concurrent per-session streaming (not sequential-disguised) all work, backed by a dedicated concurrency test.
- **B6** — Rules (MONKEY.md), remembered facts, personas/snippets, and Knowledge Stacks RAG retrieval are all injected live into the system prompt/tool results each turn.
- **B7** — MCP stdio and HTTP transports both work, with allowlists and permission gating enforced server-side and exercised by real integration tests against a spawned child process and a hand-rolled HTTP server.
- **B8** — A loopback-only OpenAI-compatible reverse-proxy server with scoped bearer tokens and true SSE passthrough, covered by 74 passing tests.
- **B9** — Bounded subagents, a capped artifact store, a bypass-banning recipe validator, and a working headless `monkey-cli` were all live-tested against real filesystem recipes.
- **B10** — Git status/commit/worktree detection is real and wired; the roadmap's own admission that diff retrieval is NOT implemented is accurate.
- **C1** — A frozen, tri-state-capability model target snapshot is real in both TS and Rust, and Compare isolation from live settings mutation is proven by a targeted test.
- **C2** — Compare fan-out streams 2–4 branches concurrently from one frozen snapshot, verified by a dedicated concurrency/isolation test.
- **C3** — Compare persistence, cancellation, retry, reload-recovery, and promotion-to-normal-session are all real and tested.
- **C4** — Memory-pressure-aware sequential queueing uses real OS memory stats and real Ollama residency data, verified by a residency-aware unload test.
- **C5** — Comparison synthesis is genuinely opt-in, genuinely tool-free (empty `tools: []` plus a hard-fail if the model tries anyway), with frozen source snapshots and stale-marking on retry.

## 4. Partial / Gaps Found (file:line specifics)

- **B2** — `src-tauri/src/tools.rs:66-117` (`tool_read_file`, `tool_list_dir`) and `:122-278` (`tool_grep`, `tool_glob`) are intentionally **not** permission-gated per the module's own doc comments. Defensible design choice, but the claim's phrase "gated by the permission system" is unqualified — worth tightening in the roadmap text.
- **B4** — `src-tauri/src/verify.rs:1-8` module doc comment still describes model-feedback-on-verify-failure as an unimplemented "future slice," but `src/lib/agentLoop.ts` (`shouldFeedBackVerifyFailure`, `runVerificationPhase`) already implements and enforces it with a real round cap. Pure doc lag — fix the comment.
- **B7** — `src-tauri/src/mcp.rs:857` says "Connect to a configured MCP server (stdio only in this phase)" and `src/store/mcpStore.ts:9` says HTTP transport "(phase 4 — mcp_connect errors on this variant for now)." Both are contradicted by the working `connect_impl` HTTP branch and passing `mcp_http.rs` tests. Also, bearer-token attachment against the real OS keychain is not exercised by an automated test — minor coverage gap.
- **B8** — No live runtime smoke test (start server via the running GUI, `curl` the port) was performed; verification rests on full source read plus 74 green unit tests.
- **B9** — Did not execute a full `task run` against a live model provider (would need real credentials) and did not verify the daemon binary/service itself starts and executes a scheduled recipe end-to-end.
- **B10** — `src-tauri/src/git.rs` has no diff/patch-retrieval function (only `git diff --shortstat` for aggregate counts); no `git_diff`-style command is registered in `src-tauri/src/lib.rs:574-575`. The only real diff-generation code, `emit_git_diffs` in `src-tauri/src/bin/monkey-cli/acp.rs:1252`, belongs to a separate CLI/ACP binary and is unreachable from the desktop app's git UI. Correctly disclosed gap, not hidden.
- **C1** — Two independently-defined `ModelTargetSnapshot` types exist: `src/lib/modelTargets.ts` (TS) vs. `src-tauri/src/run_protocol.rs:463-492` (Rust), bridged manually rather than via one canonical schema. The wire-mapping layer for durable/background runs was not traced for lossless field survival — only the interactive `compareRunner.ts` streaming path was fully verified.
- **C4** — `src/lib/modelTargets.ts:133-138` computes memory pressure from a static heuristic (model weight bytes × 1.2 + 512MB overhead) vs. 80% of available RAM — never samples live process RSS/VRAM. "At most one llama.cpp branch" is enforced as a hard block at target-selection time, so no code path actually serializes two llama.cpp branches against each other.
- **C6** — `scripts/smoke-compare-ollama.mjs:11-12,60` explicitly filters OUT cloud-tagged models (`!name.includes("-cloud")`) when auto-selecting the four comparison targets — the exact opposite of the claimed "three local tags plus one Ollama cloud tag" default composition. `.github/workflows/ci.yml` confirms `test:compare:live` is not run in CI. `git status` shows `scripts/smoke-compare-ollama.mjs`, `ROADMAP.md`, and the `package.json` diff adding `test:compare:live` are all currently **uncommitted** — no committed artifact backs a claimed historical "passed" run. `modelTargets.test.ts:42` only ever constructs `isCloud:false` fixtures.
- **C7** — Every M1.2–M6B file cited (`workflow_core.rs`, `mlx_runtime.rs`, `m5_delivery/*.rs`, `knowledge_pipeline.rs`, `browser_worker.rs`, `daemon/*.rs`, plus frontend `CrewView`/`RuntimeHubPanel`/`EcosystemPanel`/`BrowserVerificationPanel`/`GitDeliveryPanel`/`BackgroundAgentsPanel`/`KnowledgeV2Panel`) shows as `??` (untracked) in `git status`, alongside `ROADMAP.md` itself. This code compiles (`cargo check --lib` clean, `tsc --noEmit` clean) and passes 145+ targeted tests, and every corresponding Settings tab is unconditionally wired into the always-rendered nav list with no feature flag — meaning a user running the current working tree would encounter working M1.2–M6B features the roadmap calls "future work."

## 5. Not Implemented / False Claims

None of the 17 claims were found to be outright fabricated or non-functional stubs. The closest things to a "false claim":

- **C6's specific factual assertion** ("three local tags plus one Ollama cloud tag" was run and passed) is contradicted by the cited script's own coded default and has no committed evidence of ever running — a **false/unsubstantiated specific claim** embedded within an otherwise-real feature. Should be corrected or removed from the roadmap until an actual cloud-tag run is logged.
- Everything genuinely absent that the roadmap correctly labels as **not done** was confirmed absent: M5.2 autocomplete/inline-edit (only unrelated chat mention/slash-command autocomplete exists), all of M7 (no OS overlay, no TTS/STT/meetings, no image-gen adapters — zero grep hits), and M8 hardening (not deeply checked, out of spot-check scope). These are accurate "not yet implemented" claims, not false ones.

## 6. M1.2–M8 Spot-Check (C7): Does Anything Contradict "Not Done Yet"?

Yes, substantially. The roadmap's blanket framing that "M1.2 through M8 remains active/future work" is **contradicted by the current working tree**, not by git history. Real, non-trivial, test-covered implementations already exist on disk for: Crew chats (M1.2), Knowledge Stacks 2.0 with OCR/hybrid retrieval/reranking (M2), a Runtime & API hub with an MLX adapter and secure LAN pairing (M3), a visual workflow engine plus MCP OAuth/Apps and a package marketplace (M4), an ACP/IDE bridge, a browser-verification worker, and worktrees/GitHub PR-review delivery (M5.1/M5.3/M5.4), and a background daemon with a remote-runner/handoff protocol (M6A/M6B). Test names in `m5_delivery` and `knowledge_pipeline` restate the roadmap's own acceptance criteria nearly verbatim (a 3-of-4 PR-review benchmark gate; a "beats vector baseline by 10%" hybrid-retrieval test), and every corresponding UI surface is mounted live with no feature flag.

The saving grace — and the reason this isn't flatly "roadmap is lying" — is that **all of this code, and ROADMAP.md itself, is untracked in git** (`git status` shows `??` across the board), so relative to the project's actual commit history the "not yet done" framing is technically still true. Practically, though, anyone running the current working directory would find most of M1.2–M6B already functional, so the roadmap's status line is stale relative to disk state and should be updated (or this work should be committed) before it's used to set stakeholder expectations. M5.2, M7 (all three sub-items), and M8 remain genuinely unimplemented and are correctly described as not-yet-done.

---

# Current Working-Tree Closeout — 2026-07-14

This section is an additive closeout. It does not rewrite the historical audit above or pretend that its findings were wrong at the time. The implementation changed afterward, and this section records the current disk state separately from commit/release status.

## Current verdict

M0 through M7 now have real production code paths connected to the desktop and/or CLI surfaces in this working tree. The earlier blanket “future work” statement is no longer accurate for the running checkout. M8 remains a release-hardening gate, and several milestone acceptance clauses still require external models, credentials, services, physical hardware, multi-host environments, or platform signing infrastructure.

“Functional in the working tree” therefore means implementation plus reachable wiring and focused automated coverage. It does not mean “released,” “committed,” “certified on every platform,” or “all external acceptance measurements passed.”

## Earlier findings that are now closed

| Historical finding | Current working-tree state |
| --- | --- |
| B10 had no repository/index/HEAD diff retrieval | M5 Git Delivery exposes HEAD, staged, and unstaged diffs inside Little Monkey-owned worktrees, plus guarded stage/commit/push/draft-PR/review operations. |
| C6 live Compare selection excluded cloud tags | `scripts/smoke-compare-ollama.mjs` now selects three local tags plus one cloud tag when one is available, otherwise four local tags. The final closeout run exercised that default against three local tags plus one Ollama cloud tag and all four branches passed; a wider release matrix still needs retained CI/release artifacts. |
| C7 described M1.2–M6B as future | Crew, portability/search, Knowledge 2.0, Runtime Hub, packages/workflows, ACP/browser/Git delivery, daemon, and remote handoff are now mounted in the application and/or exposed through `monkey`. |
| M5.2 autocomplete/inline edit was absent | The VS Code extension has explicit local Ollama FIM routing, live `insert` capability gating, document-version cancellation, a 100-fixture corpus, selection edits, native diff preview, and explicit apply. Hardware/model p95 still needs a suitable installed FIM model. |
| M7 was absent | Companion overlay, configurable global shortcut, explicit text/file/screen grants, emergency stop, transcription/meeting segments, system TTS, user-owned image adapters, edit capability checks, gallery, cancellation, and chat insertion are wired. Media quality gates remain external. |

Several historical documentation comments and roadmap status lines that described implemented transports or features as unavailable were also superseded by the current code and the updated [README.md](README.md) and [ROADMAP.md](ROADMAP.md).

## End-to-end surfaces now present

- **M1:** Compare and Crew in the chat composer; global search; Markdown/JSON/Word session export; canonical portable bundle preflight/restore; encrypted local snapshots; WebDAV conflict handling and scheduling; message/thread translation that preserves originals.
- **M2:** Knowledge sources for files, folders, projects, URLs, sitemaps, selected chats, and WebDAV; DOCX/XLSX/PPTX/HTML/PDF extraction; optional OCR; incremental generations; hybrid retrieval/reranking; PII preview; retrieval inspector and diagnostic bundle.
- **M3:** Runtime Hub overview/models/catalogs/runtimes/API/LAN UI; verified/resumable model lifecycle; Ollama/llama.cpp/MLX adapters; capability checks; scoped OpenAI/Anthropic-compatible routes; paired TLS LAN policy that excludes workspace agent tools.
- **M4:** Signed declarative package lifecycle; native `SKILL.md` install/eligibility/rollback; explicit stacked chat skills; quarantined `/learn`; assistant/rule/plugin projections; plugin health; MCP OAuth/Apps; typed visual workflow graph, approvals, histories, replay, reconciliation, and persistent trigger registration; `monkey workflow`, `monkey skills`, and read-only `monkey plugins` views.
- **M5:** ACP stdio server; VS Code and JetBrains clients; explicit FIM completion/inline edit; disposable isolated Chromium verification with durable evidence; owned worktrees/diffs; guarded GitHub draft-PR and local review workflow; reusable review action.
- **M6A:** Explicit current-user daemon install/start/stop/uninstall; durable queue/history; budgets; approval wait; pause/resume/cancel/retry; recovery; kill switch; owned worktrees; cron/filesystem/signed/GitHub triggers; desktop Background Agents UI.
- **M6B:** User-owned pinned-TLS host; scoped one-time invitations; key rotation/revocation; replay-resistant controller calls; responsive controller; resumable event cursor; digest-bound approvals; cancellation/kill; bounded hash-verified artifacts; audit.
- **M7:** Restricted overlay and shortcut; visible capture grants; text/file/screen context; local/provider audio and meeting paths; TTS; ComfyUI/OpenAI-compatible image generation/editing; metadata/gallery/chat insertion; emergency cancellation.
- **Cross-cutting:** Uniform untrusted-content boundaries, owned-process shutdown/cancellation, Security Doctor UI and `monkey security audit [--deep] [--fix] [--json]`, plus the preferred short CLI form `monkey MODEL [PROMPT]` with optional `--provider` disambiguation.

## Independently implemented skill/plugin additions

The Hermes Agent and OpenClaw review informed product choices, not copied source. Little Monkey's implementation uses its own Rust/TypeScript contracts and existing permission/checkpoint/run systems:

- Skills are bounded data-only `SKILL.md` folders or signed package content, not implicitly trusted executable code.
- Chat slash invocation freezes the selected instructions, version, source, and digest into a single turn; up to five explicit skills can be stacked without granting tools or permissions.
- Local and immutable-commit Git installs require a matching preview digest. Symlinks, mutable refs, collisions, special files, oversized trees, and unmet requirements fail closed.
- `/learn` creates a quarantined, risk-scanned proposal that requires exact-digest review and supports reject/rollback.
- Declarative plugins expose health/setup/rollback state, signed package rules and explicit assistants, workflow templates, MCP Apps, and connector declarations through the existing ecosystem boundary.
- Persistent execution is supplied by an explicitly installed user-owned daemon and optional scoped remote controller, not by a hosted Little Monkey gateway or GPU service.

## Final closeout verification

The post-documentation whole-tree pass completed successfully:

- `pnpm build`, `pnpm test`, `pnpm i18n:lint`, and the reusable GitHub review-action fixture passed. The frontend suite finished with 781 passing tests and one intentional skip; the i18n key-lint added four passing checks.
- Rust formatting and all-target compilation passed. The regular Rust matrix passed 728 library tests, 216 CLI tests, and 18 public integration tests. The maintained real-Chromium browser test and the ignored 50,000-chunk retrieval/rerank performance gate were also run explicitly and passed.
- The VS Code extension passed all nine tests and JavaScript syntax checks. The JetBrains plugin completed its Gradle compile/instrument/test build and its maintained ACP contract corpus.
- The live Compare smoke sent one prompt to `qwen3.5-9b-uncensored-hauhaucs-aggressive-q8:latest`, `sorc/qwen3.5-uncensored:9b`, `qwen3-coder:latest`, and `qwen3-coder:480b-cloud`. All four returned `OK`, recorded independent usage/timing, and released newly loaded local models.
- The installed command path resolves to `~/.local/bin/monkey`. `monkey qwen3-coder:480b-cloud "Reply with exactly OK"` returned `OK`, and a regression test fixes the help banner at `Usage: monkey ...` while retaining legacy flag compatibility.
- `monkey skills list --json`, `monkey plugins list/health --json`, and `monkey security audit --json` executed against the real app profile. Security Doctor reported zero critical findings and eight existing owner-mode warnings; no automatic fix was applied to user data.
- `pnpm tauri dev` built and staged the release CLI sidecar, started Vite, compiled the native app, and launched `target/debug/little-monkey` without a backend runtime error. A live frontend render check exposed one React 19 unstable-store-selector loop; that defect plus three sibling allocation hazards were fixed, all 464 direct Zustand selectors were audited, and a clean reload rendered chat, slash commands, Crew, Settings, Security Doctor, and Ecosystem with no unexpected render-console errors.
- The Word export fixture had already completed the render-and-inspect QA path without clipped or overlapping content.

The production build still reports advisory Vite chunk-size/static-plus-dynamic-import warnings, and Rust reports a small set of daemon dead-code warnings for cross-platform or currently test-only paths. Neither is a build, test, launch, or runtime failure.

## External evidence boundary

The following evidence remains external and must not be inferred from unit tests:

- Publisher-operated signed runtime/model feeds and clean-machine rollback certification.
- Physical hardware memory-fit, MLX, FIM p95, Whisper real-time factor/WER, meeting diarization, focus/permission, and image GPU/OOM measurements.
- Live authenticated GitHub/provider/MCP/WebDAV scenarios that require user credentials.
- A maintained two-host remote-runner interoperability matrix.
- Windows/Linux/macOS installer signing/notarization, upgrade/downgrade migration, accessibility/locale completion, dependency review, and penetration testing under M8.

## Remaining product boundaries

- Marketplace/native skills and plugins are declarative and data-only; arbitrary downloaded native executables remain unsupported without a separate sandbox design.
- Browser verification has disposable profiles and cannot use uploads, downloads, clipboard, extensions, or a retained authenticated profile.
- The companion can capture explicitly granted context but cannot generally click/type in other desktop applications. General computer control remains post-v1.
- Remote handoff has no Little Monkey relay and stores no provider credentials on the controller.
- Team accounts, RBAC, SSO, shared channels/knowledge, hosted inference, and M8 release certification are not claimed.

# Import and continue coding-agent transcripts

Open a Knowledge 2.0 stack's **Import AI conversation history** card and choose **Import coding session**. Select a single `.jsonl` or `.json` session log from Claude Code, Codex CLI, Pi or an OpenCode JSON export.

Little Monkey parses the file locally, then creates **a new standard chat session** with an inert historical transcript and persists it to the existing session store. The newly created chat is selected and the user can send another message with whichever current model and workspace they have explicitly configured. This is a **conversation continuation**, not an executable takeover of the previous harness.

Security and compatibility boundaries:

- Historical shell/tool calls and results are never reconstructed as live `tool_calls`, authorizations, checkpoints, worktrees, or process identities. Tool arguments are not executed. Imported tool responses are capped and wrapped as untrusted content.
- Internal reasoning and tool arguments are not imported as instructions. A source working directory does not grant Little Monkey workspace permission.
- A session file must be no larger than 16 MiB, contain no more than 12,000 events, and create no more than 400 historical messages or 250,000 total message characters. Each message is capped at 12,000 characters and tool excerpts at 2,000.
- Logs with multiple distinct session IDs are refused rather than silently mixing histories; repeated Claude event UUIDs are deduplicated.
- The user chooses the import intentionally, and Little Monkey's existing model/privacy controls govern any later history sent to providers.
- Real history formats may evolve; unsupported or unreadable events are skipped and counted. The import result says when it was truncated. No model calls or network requests are made during parsing.
- Text-only OpenCode JSON exports with `info.role` and `parts` are supported; native OpenCode SQLite databases are **not** parsed by this importer.
- Corrupted JSON, unknown transcript formats, or a failed durable session write visibly fail. The newly created chat is discarded when import persistence fails.

Run the parser checks:

```sh
pnpm exec vitest run src/lib/harnessSessionImport.test.ts
pnpm typecheck
```

Integration acceptance additionally requires selecting a real saved session log, importing via the desktop file picker, restarting Little Monkey and confirming the imported transcript persists, then submitting a new turn and verifying only the *current* permissions apply. Importing text cannot verify safe re-execution of a tool from a different agent, and the feature intentionally forbids that execution.

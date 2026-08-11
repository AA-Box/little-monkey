# Workspace and trust boundaries

What the permission and trust model guarantees. The enforcement gaps it does
*not* close are in [Limitations](limitations.md#enforcement-and-isolation).

Little Monkey canonicalizes workspace paths and rejects traversal and symlink escapes. Read-only workspace operations do not mutate files; mutating file, shell, memory, MCP, browser, Git and GitHub, workflow, background, capture, and remote actions use their applicable permission or grant boundary. A remote server's `readOnlyHint`, model output, webpage text, package instructions, or imported archive can never approve its own operation.

Shell commands run inside the workspace with bounded time and cancellation. Scheduled and headless recipes require an explicit permission mode and cannot use unattended `bypass`. External mutations are recorded as pending, confirmed, or `needs_reconciliation`; ambiguous effects are not retried as if known safe. API keys, OAuth tokens, bearer secrets, remote device keys, and TLS private keys use the OS keychain where the feature supports credentials.

Security Doctor is a posture aid, not a substitute for operating-system updates, endpoint security, or a release penetration test.

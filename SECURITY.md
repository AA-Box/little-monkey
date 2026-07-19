# Security Policy

Little Monkey is a local-first desktop app: chat, agent tools, a local API
server, skill installation, and an optional daemon for unattended runs. Most
of its attack surface is local — file/shell tool execution, permission
modes, MCP server connections, and skill install/approval — rather than a
hosted service, but that surface still deserves the same disclosure process
a hosted product would get.

## Reporting a vulnerability

Please report security issues privately, not through a public GitHub issue.

- Preferred: [open a private security advisory](https://github.com/AA-Box/little-monkey/security/advisories/new)
  on this repository.
- Alternative: email **ahmad.sarollahi@gmail.com** with a description, steps
  to reproduce, and the affected component (app version / commit SHA).

Include enough detail to reproduce the issue. We'll acknowledge new reports
within a few days and keep you updated as we investigate and fix.

## Scope

In scope:

- Escaping or bypassing a configured permission mode (`manual`, `plan`,
  `acceptEdits`, `smart`, `auto`, `bypass`) or the sensitive-path risk floor.
- Skill installation or validation bugs that let an installed `SKILL.md`
  behave as anything other than data-only instructions — path traversal in
  Git-commit installs, digest/approval bypass, symlink or special-file
  handling.
- The local API server accepting non-loopback connections, or exposing
  file/shell/Git/MCP/agent-tool routes it shouldn't, without the explicit
  configuration those require.
- Credential or secret handling that writes key material to disk in
  plaintext instead of the OS keychain.
- MCP server trust or tool-routing bugs that let a connected server exceed
  its granted scope or bypass an allowlist.
- Browser Verification sandbox escapes, or the daemon/remote runner pairing
  accepting an action outside what its invitation actually granted.

Generally out of scope:

- A local model producing incorrect, biased, or objectionable output. That's
  a model-quality/safety concern, not a security vulnerability in the app
  itself — but let us know regardless if it's actionable.
- Issues that require an attacker to already have arbitrary code execution
  on the machine running Little Monkey.
- Denial of service against your own local instance.

## Supported versions

Little Monkey doesn't have tagged releases yet. Security fixes land on
`develop`; there's no older version line currently being patched separately.
This section will grow a real support-window table once versioned releases
exist.

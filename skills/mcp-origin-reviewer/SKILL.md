---
name: MCP Origin Reviewer
description: Review configured MCP server origins and scopes for anything overly broad
command: mcp-origin-review
version: 1.0.0
requires:
  bins: []
  env: []
---
List configured remote MCP servers with their granted scopes and origins. Flag any server granted broader tool access than its stated purpose requires, and any origin that isn't pinned to an exact host.

This is a review pass — changing a grant is a separate, explicit approval step the user takes themselves.

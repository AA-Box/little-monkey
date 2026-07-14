# Little Monkey local PR review action

This composite action reads PR metadata and the patch through `gh`, reviews the patch through loopback Ollama, validates every finding against a new-side diff line, and creates or updates one report comment. It never checks out or executes pull-request code.

Required workflow permissions:

```yaml
permissions:
  contents: read
  pull-requests: write
```

Run it only on a user-owned runner with Node.js 20+, `gh`, and Ollama. The action refuses GitHub-hosted runners and non-loopback Ollama URLs.

```yaml
name: Local PR review
on:
  pull_request:
    types: [opened, synchronize, reopened, ready_for_review]

permissions:
  contents: read
  pull-requests: write

jobs:
  review:
    runs-on: [self-hosted, little-monkey]
    steps:
      # Load the trusted action from the base branch, never from PR code.
      - uses: actions/checkout@v4
        with:
          ref: ${{ github.event.repository.default_branch }}
          persist-credentials: false
          sparse-checkout: .github/actions/little-monkey-review
      - id: review
        uses: ./.github/actions/little-monkey-review
        with:
          model: qwen2.5-coder:14b
```

`publish: false` generates only a private report and audit record. `report-path`, `audit-path`, `report-digest`, and `comment-id` are outputs for a caller-owned artifact/approval workflow. Before a GitHub write, the action fsyncs a `pending` audit row containing the exact request digest; it then appends `success` or `needs_reconciliation`. The stable report marker is scoped to repository + PR, so an interrupted rerun updates the caller's prior report instead of creating duplicates.

# Little Monkey — Embeddable Chat Widget

A self-contained (no CDN, no external stylesheet/font) vanilla-JS chat
widget for internal sites and homelab portals. It talks only to your Little
Monkey API server's `/chat/completions` route.

## Setup

1. In Little Monkey, open **Settings > API Server > Widget**.
2. Pick (or create) a token scoped to `chat` only — do not reuse a token that
   also carries `models`/`embeddings`/`knowledge`/`artifact_read`/
   `workflow_run` for a widget you're embedding on a page other people can
   view source on.
3. Copy the generated embed snippet, or copy `chat-widget.js` next to your
   page and use `chat-widget.html` as a starting point:

```html
<script>
  window.LMK_CHAT_WIDGET_CONFIG = {
    baseUrl: "http://127.0.0.1:1234/v1",
    token: "lmk-...",
    model: "qwen2.5-7b-instruct",
    title: "Ask the homelab",
  };
</script>
<script src="./chat-widget.js"></script>
```

## Config fields

| Field           | Required | Description                                              |
| --------------- | -------- | ---------------------------------------------------------- |
| `baseUrl`        | yes      | The API server's base URL, e.g. `http://127.0.0.1:1234/v1`. |
| `token`          | no       | Bearer token. Omit only if `require_token` is off.         |
| `model`          | no       | Model id to send with each request. Defaults to `"default"`. |
| `title`          | no       | Header text and the toggle button's accessible label.      |
| `systemPrompt`   | no       | Prepended as a `system` message to every conversation.     |

## Auth and scope

The widget sends `Authorization: Bearer <token>` on every chat request — the
same header format documented in `../typescript/README.md`. It never reads
or writes any other route: no models list, no knowledge query, no artifact
or workflow-run access. Scope the token to `chat` only.

## Security notes

- This file is plain, unminified JavaScript specifically so anyone embedding
  it (or auditing a site that does) can read exactly what it does.
- It renders model output as `textContent`, never `innerHTML` — a reply
  cannot inject markup into the host page.
- It never loads any other script, stylesheet, font, or image from a third
  party.
- The bearer token above is visible to anyone who views the embedding page's
  source — that's inherent to a client-side widget, which is exactly why the
  token you use here should carry only the `chat` scope and, ideally, a
  short expiry.

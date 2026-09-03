# Connecting remote MCP servers with your own OAuth app

Little Monkey ships **no OAuth client credentials of its own**. It's an
open-source app: anything baked into the binary is readable by anyone who
downloads it, so a shared client id — and especially a shared client secret —
would be a credential leak with the app's name on it, and a single revocation
away from breaking every user at once.

The app identifies itself in whichever of four ways a given server supports,
trying them in the order the MCP authorization spec sets out:

| # | Mechanism | Servers | What you do |
| --- | --- | --- | --- |
| 1 | Your own pre-registered OAuth app | anything | Nothing, unless you choose to — a client you've entered before is reused. |
| 2 | Client ID Metadata Document (CIMD) | servers advertising `client_id_metadata_document_supported` | Click **Connect**. No registration at all. |
| 3 | Dynamic client registration (RFC 7591) | Notion, Stripe, PostHog, Atlassian | Click **Connect**. The app registers a client for you. |
| 4 | Ask you for a client | Google (Gmail, Drive), Slack | Register an OAuth app once in the provider's console, then paste its client id and any secret the selected client mode requires. |

Row 2 is the interesting one: with CIMD the client id *is* a URL —
`https://getlittlemonkey.com/oauth/client-metadata.json` — which the
authorization server fetches to learn the app's name and its allowed redirect
URIs. Nothing is registered, nothing is secret, and the consent screen still
shows the user exactly which app is asking. Claude Code identifies itself the
same way. The document lives in the website repo at
`public/oauth/client-metadata.json`; its `redirect_uris` and the app's loopback
listener are a contract, so don't change one without the other.

Row 4 is the rest of this page. After authorization and token exchange succeed,
everything you pasted goes into your OS keychain, never into
`mcp_servers.json` or any other file, and it's remembered — later reconnects
don't ask again. A rejected, cancelled, or timed-out attempt does not save the
pending client or overwrite a previously working registration.

## Why the "Connected" pill isn't proof, and what the app now does about it

Google's MCP endpoints answer the MCP handshake **without** a token:
`initialize` and `tools/list` return 200 and a full tool list, while
`tools/call` returns 401. A server can therefore look connected, show its tools,
and still fail the first thing you ask it to do:

```
MCP tool call to 'create_draft' on 'gmail' failed:
Transport send error: ... Auth required
```

Two things changed so that's no longer a surprise:

- A server shows a **Needs auth** pill in **Settings → Connectors (MCP)** only
  after the backend observes an authentication-shaped failure and no OAuth or
  bearer credential is saved. Merely using HTTP without credentials is not
  enough: public MCP endpoints are valid.
- A tool call rejected for auth reasons triggers one reconnect (which refreshes
  the access token) and one replay, instead of failing. Only auth failures are
  replayed — the server rejected the call before running the tool, so nothing
  can be duplicated.

## Gmail and Google Drive

Google's MCP endpoints advertise the scopes they support:

```
GET https://gmailmcp.googleapis.com/.well-known/oauth-protected-resource/mcp/v1
  authorization_servers: ["https://accounts.google.com/"]
  scopes_supported:      https://mail.google.com/, .../gmail.modify,
                         .../gmail.compose, .../gmail.readonly, .../gmail.metadata
```

The app does not request that entire advertised set. It has a hardcoded,
least-privileged preferred scope for each Google MCP endpoint — Gmail uses
`gmail.modify` and Drive uses `drive` — and requests that preferred scope only
when the endpoint still advertises it. If Google changes the advertised set,
the app falls back to the server-derived scopes rather than injecting a scope
the server did not claim to support.

Other Google-specific request details are:

- `access_type=offline` — without it Google issues no refresh token, and the
  connection dies silently one hour later.
- `prompt=consent` — Google only re-issues a refresh token on a consent screen
  the user actually sees.
- `client_secret` on the token endpoint — required for Google "Desktop app"
  clients even though the flow is PKCE and the secret isn't treated as
  confidential. This is why the client-secret field exists.

### Steps

1. In [Google Cloud console](https://console.cloud.google.com/), create (or
   pick) a project.
2. **APIs & Services → Enabled APIs** → enable the **Gmail API** (and **Google
   Drive API**, for Drive).
3. **OAuth consent screen** → configure it as **External**, then add your own
   Google account under **Test users**. Staying in *Testing* is the point: the
   Gmail scopes are restricted, and publishing an app that requests them needs
   Google's verification review. A test-user app doesn't.
4. **Credentials → Create credentials → OAuth client ID**, then either
   application type:

   - **Desktop app** (simplest) — nothing to register. Google implicitly
     accepts `http://127.0.0.1` / `http://localhost` for these and waives the
     port when matching.
   - **Web application** — add the redirect URI the app shows you (see step 5)
     under **Authorized redirect URIs**, exactly as displayed. Web clients match
     the URI *including the port*, which is why the app uses a stable port per
     server rather than a random one.

   Copy the **client ID** and **client secret**.

   Getting this wrong shows up in the browser, never in the app:

   ```
   Error 400: redirect_uri_mismatch
   ```

   Decode the error page's `authError` parameter (base64) to see the exact
   `redirect_uri` Google received, and compare it with what's registered. Note
   Google waives the port for Desktop clients but never the *path* — that's why
   the app's redirect URI has no path at all.
5. In Little Monkey: **Settings → Connectors (MCP)** → the **Gmail** card →
   **Connect**. It stops and asks for your client, showing **Redirect URI to
   register** — that value is stable for this server and never changes, so a
   Web-application client only needs it registered once. Paste the client ID
   (and secret) and press **Continue**.
6. Approve the consent screen in the browser. It will warn that the app isn't
   verified — that's your own unverified project, and the "Advanced" link
   proceeds.

Same steps for Drive, with the Drive API and the Drive card.

## Slack

Slack's MCP server does not support dynamic client registration. Create an
**internal app** for your workspace (or use a directory-published app), add the
**user scopes** the Slack MCP tools need, and then:

1. Under **OAuth & Permissions**, enable **Proof Key for Code Exchange (PKCE)**.
   This setting marks the Slack app as a public client and Slack documents it as
   a one-way change. Little Monkey already sends the required S256 PKCE
   challenge and verifier.
2. Add the app's **Redirect URI to register** value under
   **OAuth & Permissions → Redirect URLs**, exactly as displayed. For Slack it
   starts with `http://localhost:` because Slack recognizes `localhost` as a
   desktop redirect when PKCE is enabled; an IP-literal `127.0.0.1` redirect is
   not the documented desktop form.
3. Paste the client id in Little Monkey and leave the client-secret field
   empty. Slack's public desktop PKCE exchange and later token refresh do not
   require a `client_secret`.

If you intentionally keep PKCE disabled, Slack treats the `localhost` callback
as a server redirect instead. The Slack MCP server's confidential OAuth mode
then requires both `client_id` and `client_secret`, so paste both. Never request
bot scopes for the PKCE desktop flow; Slack only permits user scopes on desktop
redirects.

## Why loopback, and not a hosted callback URL

A redirect URI like `https://api.example.com/mcp/oauth/google/callback` only
works with a service you deploy and keep running: it has to hold the client
secret, receive the authorization code, and hand it back to the desktop app over
a custom URL scheme. That's the `hosted_oauth.rs` model (see the last section) —
it can't be the default for an open-source build, and the endpoint referenced in
that module isn't deployed.

Loopback is the standard answer for native apps (RFC 8252): the app listens on
the local machine only, the browser redirects straight to it, and the code never
leaves the machine. Most redirect URIs use `127.0.0.1`; Slack uses `localhost`
so Slack can classify it as a desktop PKCE redirect. The port is derived from
the server id — stable, and in the IANA dynamic range — so it can be registered
like any other redirect URI.

For a client you registered manually, that displayed host and port are an exact
contract. If the stable port is busy, Little Monkey stops with an actionable
error instead of silently sending an unregistered ephemeral port. Flows using
CIMD or dynamic registration can safely fall back to an ephemeral port because
the authorization server learns the URI used for that attempt.

## Any other server

Any HTTP MCP server can be connected the same way from
**Settings → Connectors (MCP) → the server's *Connection settings* → Connect via
OAuth**, whether or not it's in the connector catalog. Leave the client secret
empty for a public PKCE client — sending an empty secret is not the same request
as sending none, and the app treats a blank field as "none".

If a server hands out a plain long-lived token instead of doing OAuth at all,
use **Manual bearer token** in the same disclosure.

## Where things are stored

| What | Where |
| --- | --- |
| Your OAuth client id + secret | keychain, `mcp-oauth-client:<server id>` |
| Access + refresh tokens | keychain, `mcp-oauth:<server id>` |
| Manually pasted bearer token | keychain, `mcp:<server id>` |
| Server URL, timeout, tool allowlist | `mcp_servers.json` |
| Your connector OAuth client id + secret | keychain, `connector-oauth-client:<provider>` |
| Connector access + refresh tokens | keychain, `connector-oauth:<account id>` |
| Account label, provider, host/tenant, credential ref | `connectors.json` |

**Disconnect** clears the first two together, so a later connect starts clean
rather than silently reusing a client you may have revoked.

Removing a connector account deletes its `connector-oauth:<account id>` entry,
and deletes the shared `connector-oauth-client:<provider>` entry only once no
account of that provider is left — otherwise disconnecting one Google account
would send the others back to the Google console.

## Connector accounts

Settings → Connectors → **Connect over OAuth** connects a work *account* (not
an MCP server — that is the App connectors grid, a separate connection with its
own keychain entry). None of these eleven providers supports dynamic client
registration, so each one needs an OAuth app you register yourself, once.

The redirect URI is shown on the card **before** the client-id fields, because
you have to register it while creating the app. It is derived from the provider,
so it is the same string for every account of that provider and never changes:
`http://127.0.0.1:<port>/`, root path, no query.

| Provider | Client secret | Extra field |
| --- | --- | --- |
| Google Drive | required | — |
| Microsoft Graph | **never** — register a public "Mobile and desktop applications" client | tenant (defaults to `common`) |
| Linear | required | — |
| Asana | required | — |
| Dropbox | optional | — |
| Box | required | — |
| Airtable | optional (PKCE is mandatory) | — |
| Zendesk | optional with PKCE | your subdomain host, e.g. `acme.zendesk.com` |
| HubSpot | required | — |
| Discord | required | — |
| GitLab | optional | instance host, defaults to `gitlab.com` |

Leave the secret blank for a public PKCE client — sending an empty secret is not
the same request as sending none, and the app treats a blank field as "none".
Microsoft Graph is the one provider with no secret field at all; every other card
shows one.

The client id and secret are saved in your keychain per *provider*, so the second
account of the same provider only needs a label: leave both fields blank and the
card reuses the registration (blank with nothing saved comes back as "needs a
client ID"). Because that one registration is what every account of the provider
refreshes against, pasting a *different* client id while accounts registered
against the old one still exist is refused — remove them first.

Consent is desktop-only: it opens your system browser and streams progress to
the Settings window. `monkey connectors list|reverify|remove` manages accounts
that already exist.

## Running your own broker instead

`src-tauri/src/hosted_oauth.rs` implements the other model: a deployed service
holds the client secrets and brokers the flow, so users never see a client id.
It's unwired in public builds (its client-id constants are placeholders, and it
refuses to start a flow while they are), and it exists for anyone shipping a
build with their own broker deployed.

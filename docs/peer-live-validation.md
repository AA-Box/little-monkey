# Peer live validation

Everything about peer agents that can be proven on one machine is proven by the
automated suite. This document covers the rest: the parts that need two real
installations, a real network, and a human.

Nothing here needs credentials, a hosted service, or anything owned by the
project. Two machines you already have, and a certificate you generate.

## What is already automated

Do not re-check these by hand; they run on every commit.

| Property | Where |
| --- | --- |
| Pairing, grants, hello, messages, task requests, results, artifacts, loops, expiry, revocation, restart | `daemon::peer_e2e` |
| Per-peer artifact admission, and that a digest alone grants nothing | `daemon::peer_ingress`, `daemon::peer_e2e` |
| Refusal telemetry staying bounded on disk | `daemon::peer_store`, `daemon::peer_e2e` |
| Security Doctor's peer findings | `daemon::peer_audit` |
| Settings → Peers, driven as an operator drives it | `PeersPanel.test.tsx` |

One more is automated but **opt in**, because it binds a listener and shells out
to `openssl`:

```bash
cargo test --manifest-path src-tauri/Cargo.toml --bin monkey-cli -- --ignored peer_live
```

That one mints a self-signed certificate, starts the real TLS listener on
loopback, and drives hello, a multi-megabyte artifact upload, a task request
referencing it, and a thread read over a real socket with a real certificate
pin. Run it after touching anything under `daemon/remote/`.

## What still needs two machines

Work through this before a release that changes the peer plane. Each step says
what it proves, because a step you cannot explain is a step that gets skipped.

### 1. Reachability across a real network

On the machine that will *receive* (call it **B**):

```bash
monkey remote enable --listen 0.0.0.0:8443 --advertise-url https://b.example.lan:8443 --certificate ./cert.pem --key ./key.pem
```

Generate the certificate however you normally would; it must carry the name or
IP in `--advertise-url` as a subject alternative name.

Then, from the machine that will *send* (**A**):

```bash
curl -sv https://b.example.lan:8443/v1/remote/peer/hello -o /dev/null
```

A TLS handshake that completes and a 401 is the expected answer: the transport
works and the request was refused for being unsigned. **Proves** the listener is
reachable through the OS firewall and any router between them, which is the only
part loopback cannot tell you.

### 2. Fingerprint comparison out of band

On **B**:

```bash
monkey remote status
```

Read the certificate fingerprint aloud, or send it over a channel that is not
the one being set up. On **A**, after accepting the invitation:

```bash
monkey peers list
```

Compare every group of the fingerprint, not the first few. **Proves** the pin A
holds is B's certificate and not something in between — the one check in the
whole design that a machine cannot make for you.

### 3. Pairing in one direction only

On **B**:

```bash
monkey peers invite "Laptop" --allow message --output ./invite.json
```

Move `invite.json` to **A** by hand. Then on **A**:

```bash
monkey peers accept ./invite.json b
monkey peers send b "hello from the laptop"
```

On **B**, confirm the message arrived and became a turn:

```bash
monkey peers threads
```

Now try the reverse without pairing the other way — on **B**, `monkey peers send
a "..."` must fail with an unknown peer. **Proves** pairing is directional: an
installation you can reach cannot reach you.

### 4. A task, and its result, across the network

On **A**:

```bash
monkey peers send b "summarise the last build log" --task --correlation live-1
monkey peers outbound
```

Wait for B's run to finish, then on **A**:

```bash
monkey peers remote-thread b <thread-id>
```

**Proves** the poll, the result materialization and the correlation survive a
real round trip. Check the same thread in Settings → Peers on A, under "What you
sent" — it should show the same state and text.

### 5. A large artifact over a real link

On **A**, with a file of roughly 30 MiB already in the content store:

```bash
monkey peers send b "the capture" --artifact <artifact-id>
```

**Proves** the body limit, the base64 expansion and the receiving store's
ceiling all hold when the bytes actually cross a network with real latency and a
real MTU — the loopback test uses 4 MiB deliberately, so this is the one that
exercises the top of the range.

### 6. Revocation while the far side is mid-conversation

On **B**, during an exchange:

```bash
monkey peers revoke <device-id>
```

**A**'s very next send must be refused. **Proves** revocation is not cached
anywhere on the transport, which only a live connection can show.

### 7. Rotation across the wire

On **B**:

```bash
monkey peers rotate <device-id> --output ./rotation.json
```

Before moving the file, confirm **A**'s next send fails. Then move
`rotation.json` to A and:

```bash
monkey peers accept-rotation ./rotation.json b
```

**Proves** the old key dies immediately rather than at the next handshake, and
that A recovers only by taking up the bundle.

## Recording a run

Note the two machines' OS versions, whether the link was LAN or WAN, and any
step that needed a firewall change. A step that needed one is a step the
documentation should mention.

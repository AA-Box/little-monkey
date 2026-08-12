//! RS256 JWT verification against a provider's published key set.
//!
//! Two providers are delivered to over a signed JWT — Microsoft Teams and
//! Google Chat — and both publish RSA keys as a JWKS document, so the
//! verification is written once here rather than twice in the adapters.
//!
//! The rule this file exists to enforce: `alg` is pinned to RS256 by name, not
//! taken from the token. A verifier that trusts a token's own `alg` accepts
//! `none` and accepts an HMAC forged with the public key, which is the classic
//! bypass. The signing input is sliced from the original token and never
//! re-serialized, because re-encoding claims produces different bytes than the
//! ones that were signed.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use ring::signature::{RsaPublicKeyComponents, RSA_PKCS1_2048_8192_SHA256};
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;
use std::sync::Mutex;

/// One RSA public key's `n`/`e` components, raw big-endian bytes (already
/// base64url-decoded from the JWKS JSON — never DER, never a PEM).
#[derive(Clone)]
pub(crate) struct JwkRsaKey {
    pub(crate) n: Vec<u8>,
    pub(crate) e: Vec<u8>,
}

/// A JWT decoded into its parts, without touching the signature's validity.
///
/// `signing_input` borrows the exact `header.payload` ASCII from the
/// original token — never rebuilt by re-serializing `header`/`claims` back
/// to JSON and re-encoding, which is not guaranteed to reproduce the bytes
/// that were actually signed (key order, whitespace, escaping all vary
/// between encoders).
pub(crate) struct DecodedJwt<'a> {
    pub(crate) header: JsonValue,
    pub(crate) claims: JsonValue,
    pub(crate) signing_input: &'a str,
    pub(crate) signature: Vec<u8>,
}

/// Splits a JWT into its three segments, base64url-decodes and JSON-parses
/// the header and payload, and base64url-decodes the signature. Performs no
/// verification of any kind — that is [`validate_alg_is_rs256`] and
/// [`verify_rs256_signature`]'s job, kept separate so a caller cannot
/// accidentally skip one of the two.
pub(crate) fn decode_jwt(token: &str) -> Result<DecodedJwt<'_>, String> {
    let first_dot = token.find('.').ok_or_else(|| "Malformed JWT".to_string())?;
    let rest = &token[first_dot + 1..];
    let second_dot = rest.find('.').ok_or_else(|| "Malformed JWT".to_string())?;
    let header_b64 = &token[..first_dot];
    let payload_b64 = &rest[..second_dot];
    let signature_b64 = &rest[second_dot + 1..];
    if signature_b64.is_empty() || signature_b64.contains('.') {
        return Err("Malformed JWT".to_string());
    }
    // The exact bytes that were signed: the original header and payload
    // segments, joined by the single `.` between them, sliced straight out
    // of `token` rather than reassembled.
    let signing_input = &token[..first_dot + 1 + second_dot];

    let header_bytes = URL_SAFE_NO_PAD
        .decode(header_b64)
        .map_err(|_| "JWT header is not valid base64url".to_string())?;
    let header: JsonValue = serde_json::from_slice(&header_bytes)
        .map_err(|_| "JWT header is not valid JSON".to_string())?;
    let payload_bytes = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|_| "JWT payload is not valid base64url".to_string())?;
    let claims: JsonValue = serde_json::from_slice(&payload_bytes)
        .map_err(|_| "JWT payload is not valid JSON".to_string())?;
    let signature = URL_SAFE_NO_PAD
        .decode(signature_b64)
        .map_err(|_| "JWT signature is not valid base64url".to_string())?;

    Ok(DecodedJwt {
        header,
        claims,
        signing_input,
        signature,
    })
}

/// Accepts only `alg: "RS256"`, named explicitly, and refuses everything
/// else — `none`, any HMAC spelling, anything unrecognized — by name rather
/// than by attempting and failing to verify it.
///
/// # Why this cannot be folded into `verify_rs256_signature`
///
/// Trusting the token's own `alg` to pick which algorithm verifies it is the
/// textbook JWT bypass: an attacker sends `alg: "none"` with no signature
/// (some libraries skip verification entirely for that case), or `alg:
/// "HS256"` and signs the token with the *public* RSA key — which is public,
/// not secret — as the HMAC key, which some naive verifiers accept because
/// they select the HMAC algorithm from the header and then feed it that same
/// key. This function is the single place `alg` is read; everything
/// downstream of it always verifies as RS256 or does not run.
pub(crate) fn validate_alg_is_rs256(header: &JsonValue) -> Result<(), String> {
    match header.get("alg").and_then(JsonValue::as_str) {
        Some("RS256") => Ok(()),
        Some(other) => Err(format!("JWT alg '{other}' is not accepted; only RS256 is")),
        None => Err("JWT is missing alg".to_string()),
    }
}

/// Verifies `signature` over `signing_input` against `key`'s RSA public
/// components. The one place either provider's code calls into `ring` for
/// verification.
pub(crate) fn verify_rs256_signature(
    signing_input: &str,
    signature: &[u8],
    key: &JwkRsaKey,
) -> Result<(), String> {
    let public_key = RsaPublicKeyComponents {
        n: key.n.as_slice(),
        e: key.e.as_slice(),
    };
    public_key
        .verify(
            &RSA_PKCS1_2048_8192_SHA256,
            signing_input.as_bytes(),
            signature,
        )
        .map_err(|_| "JWT signature verification failed".to_string())
}

/// Parses a JWKS document's `keys` array into `(kid, JwkRsaKey)` pairs.
/// Entries that are not `kty: "RSA"`, or are missing `kid`/`n`/`e`, or whose
/// `n`/`e` do not decode, are skipped rather than failing the whole
/// document — a JWKS is allowed to publish key types this adapter does not
/// use.
pub(crate) fn parse_jwks_body(bytes: &[u8]) -> Result<Vec<(String, JwkRsaKey)>, String> {
    let parsed: JsonValue = serde_json::from_slice(bytes)
        .map_err(|_| "JWKS response was not valid JSON".to_string())?;
    let keys = parsed
        .get("keys")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| "JWKS response is missing keys".to_string())?;
    let mut out = Vec::new();
    for key in keys {
        if key.get("kty").and_then(JsonValue::as_str) != Some("RSA") {
            continue;
        }
        let (Some(kid), Some(n_b64), Some(e_b64)) = (
            key.get("kid").and_then(JsonValue::as_str),
            key.get("n").and_then(JsonValue::as_str),
            key.get("e").and_then(JsonValue::as_str),
        ) else {
            continue;
        };
        let (Ok(n), Ok(e)) = (URL_SAFE_NO_PAD.decode(n_b64), URL_SAFE_NO_PAD.decode(e_b64)) else {
            continue;
        };
        out.push((kid.to_string(), JwkRsaKey { n, e }));
    }
    if out.is_empty() {
        return Err("JWKS response contained no usable RSA keys".to_string());
    }
    Ok(out)
}

/// GETs `url` through the hardened egress path and returns the raw body
/// bytes on a successful status. Never routed through a bare
/// `reqwest::Client` — see the module doc.
pub(crate) async fn fetch_bytes_via_egress(url: &str) -> Result<Vec<u8>, String> {
    let client = little_monkey_lib::egress::hardened()
        .build()
        .map_err(|error| format!("Failed to build client: {error}"))?;
    let response = little_monkey_lib::egress::send(client.get(url))
        .await
        .map_err(|_| "JWKS request failed".to_string())?;
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|_| "JWKS request failed".to_string())?;
    if !status.is_success() {
        return Err(format!("JWKS fetch failed with status {status}"));
    }
    Ok(bytes.to_vec())
}

/// [`fetch_bytes_via_egress`] plus [`parse_jwks_body`], for a URL that is
/// already the JWKS document itself (Google Chat's case; Teams reaches this
/// after its own metadata indirection).
pub(crate) async fn fetch_jwks_via_egress(url: &str) -> Result<Vec<(String, JwkRsaKey)>, String> {
    let bytes = fetch_bytes_via_egress(url).await?;
    parse_jwks_body(&bytes)
}

/// Reads `jwks_uri` out of an OpenID Connect discovery document. Teams-only
/// (Google Chat's JWKS URL is fixed), but kept in the shared section since it
/// is the other half of the two-step fetch Teams composes from
/// [`fetch_bytes_via_egress`] and [`fetch_jwks_via_egress`].
pub(crate) fn parse_jwks_uri_from_metadata(bytes: &[u8]) -> Result<String, String> {
    let parsed: JsonValue = serde_json::from_slice(bytes)
        .map_err(|_| "OpenID metadata response was not valid JSON".to_string())?;
    parsed
        .get("jwks_uri")
        .and_then(JsonValue::as_str)
        .map(str::to_string)
        .ok_or_else(|| "OpenID metadata response is missing jwks_uri".to_string())
}

/// How long a fetched key set is trusted before [`JwksCache::needs_refresh`]
/// asks for another proactive fetch. Both providers rotate keys
/// infrequently; an hour bounds staleness without hammering either endpoint.
pub(crate) const JWKS_CACHE_TTL_MS: i64 = 3_600_000;

struct JwksCacheState {
    by_kid: BTreeMap<String, JwkRsaKey>,
    /// `0` means never fetched, which [`JwksCache::needs_refresh`] treats as
    /// maximally stale.
    fetched_at_ms: i64,
}

/// A cached, TTL-tracked JWKS key set, keyed by `kid`.
///
/// Deliberately holds no opinion on *how* it is refreshed — [`JwksCache::find`]
/// only ever reads, [`JwksCache::replace`] only ever writes, and every fetch
/// decision (proactive on a TTL, or the single bounded catch-up on an unknown
/// `kid`) lives in the owning adapter. That keeps this type usable
/// synchronously from `verify_and_normalize`, which cannot itself await
/// anything.
pub(crate) struct JwksCache {
    state: Mutex<JwksCacheState>,
}

impl JwksCache {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(JwksCacheState {
                by_kid: BTreeMap::new(),
                fetched_at_ms: 0,
            }),
        }
    }

    /// The key for `kid`, if the cache has ever seen it. A poisoned lock
    /// reads as "not found" — refusing the delivery — rather than panicking
    /// the webhook listener.
    pub(crate) fn find(&self, kid: &str) -> Option<JwkRsaKey> {
        self.state.lock().ok()?.by_kid.get(kid).cloned()
    }

    /// Whether a proactive refresh is due: never fetched, or past
    /// [`JWKS_CACHE_TTL_MS`] since the last one. A poisoned lock reads as
    /// "yes" — the safe direction, since a redundant fetch costs a request
    /// and a missed one costs staleness.
    pub(crate) fn needs_refresh(&self, now_ms: i64) -> bool {
        match self.state.lock() {
            Ok(state) => {
                state.fetched_at_ms == 0 || now_ms - state.fetched_at_ms > JWKS_CACHE_TTL_MS
            }
            Err(_) => true,
        }
    }

    /// Replaces the entire key set with a freshly fetched one. Whole-set
    /// replacement rather than a merge: a key a fresh fetch no longer lists
    /// has been rotated out by the provider and must stop verifying anything
    /// from here on.
    pub(crate) fn replace(&self, keys: Vec<(String, JwkRsaKey)>, now_ms: i64) {
        if let Ok(mut state) = self.state.lock() {
            state.by_kid = keys.into_iter().collect();
            state.fetched_at_ms = now_ms;
        }
    }

    /// Seeds one key directly, bypassing any fetch. Test-only: production
    /// keys only ever arrive through [`JwksCache::replace`].
    #[cfg(test)]
    pub(crate) fn seed_for_test(&self, kid: &str, key: JwkRsaKey) {
        if let Ok(mut state) = self.state.lock() {
            state.by_kid.insert(kid.to_string(), key);
            // Far in the future so a seeded test cache never looks stale and
            // triggers a real (network) refresh attempt.
            state.fetched_at_ms = i64::MAX / 2;
        }
    }
}

/// Bridges one bounded, at-most-once synchronous key refresh out of the
/// trait's required-synchronous `verify_and_normalize`.
///
/// # Why a bridge at all
///
/// [`WebhookChannelAdapter::verify_and_normalize`] is synchronous by trait
/// contract and may not change (see that trait's doc). An unknown `kid`
/// legitimately means "the provider rotated its signing key since the last
/// fetch" as often as it means an attack, and refusing every delivery until
/// the next [`probe`](ChannelAdapter::probe) tick would make routine key
/// rotation look like an outage. So a delivery carrying an unrecognised `kid`
/// gets exactly one bounded, synchronous attempt to catch up before it is
/// refused.
///
/// # Why this is safe rather than reckless
///
/// - **No panic when called off a runtime.** `Handle::try_current()` returns
///   `Err` with no Tokio runtime at all — every plain `#[test]` in this
///   file — and the refresh is skipped, not attempted.
/// - **No panic on a current-thread runtime.** `block_in_place` panics
///   outright there, and `#[tokio::test]`'s default flavor *is*
///   current-thread, so the flavor is checked before it is ever called. The
///   real daemon runs `#[tokio::main]`'s multi-thread flavor (see
///   `main.rs`), so this path is live in production and inert in the tests
///   that do not explicitly ask for a multi-thread runtime — which is also
///   why the "unknown kid, empty cache" tests are deterministic and hit no
///   network: nothing here attempts a fetch outside that one flavor.
/// - **Bounded, not unbounded.** The fetch this triggers is
///   [`fetch_jwks_via_egress`], which goes through `egress::hardened()` — a
///   fixed connect and read timeout, never an indefinite hang.
/// - **At most once, by construction.** This function makes exactly one
///   attempt and returns; it does not loop. Every caller only ever invokes it
///   once per `verify_and_normalize`, immediately after a cache miss, so a
///   `kid` neither the cache nor this refresh recognises is refused on that
///   same call rather than retried.
pub(crate) fn try_refresh_blocking<F, Fut>(refresh: F) -> bool
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return false;
    };
    if handle.runtime_flavor() != tokio::runtime::RuntimeFlavor::MultiThread {
        return false;
    }
    tokio::task::block_in_place(|| handle.block_on(refresh()))
}

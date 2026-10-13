//! Official OpenRouter OAuth PKCE login for cloud model requests.
//!
//! Unlike extracting credentials from a CLI installation, this is OpenRouter's
//! documented user-authorized API-key grant. No third-party subscription tokens
//! or cookies are copied. The returned key is stored through the existing
//! provider keychain, and is then consumed by the exact same streaming/models
//! paths as a manually entered OpenRouter key.
//! https://openrouter.ai/docs/guides/overview/auth/oauth
use std::time::Duration;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use sha2::{Digest, Sha256};
use tauri_plugin_opener::OpenerExt;
use crate::mcp_oauth::{CallbackParams, LoopbackListener};

const AUTHORIZE_ENDPOINT: &str = "https://openrouter.ai/auth";
const EXCHANGE_ENDPOINT: &str = "https://openrouter.ai/api/v1/auth/keys";
static LOGIN_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Generate unpredictable values for a single authorization attempt.
/// A verifier must be 43-128 RFC 7636 unreserved characters.
fn random_verifier() -> String {
    format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple())
}

fn code_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn authorization_url(callback_url: &str, verifier: &str, state: &str) -> Result<String, String> {
    let parsed = url::Url::parse(callback_url)
        .map_err(|_| "Invalid OAuth callback URI".to_string())?;
    if parsed.scheme() != "http" || parsed.host_str() != Some("127.0.0.1")
        || parsed.path() != "/" || parsed.query().is_some() {
        return Err("OpenRouter callback must be an unadorned 127.0.0.1 loopback URL".into());
    }
    let mut url = url::Url::parse(AUTHORIZE_ENDPOINT)
        .map_err(|e| format!("Invalid OpenRouter authorization endpoint: {e}"))?;
    url.query_pairs_mut()
        .append_pair("callback_url", callback_url)
        .append_pair("code_challenge", &code_challenge(verifier))
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    Ok(url.into())
}

fn validate_callback(callback: CallbackParams, expected_state: &str) -> Result<String, String> {
    if let Some(error) = callback.error {
        // Never display an untrusted provider callback's free-text error body.
        return Err(format!("OpenRouter authorization denied: {error}"));
    }
    if callback.state.as_deref() != Some(expected_state) {
        return Err("OpenRouter OAuth callback state mismatch; no key was saved".into());
    }
    callback.code.filter(|code| !code.trim().is_empty())
        .ok_or_else(|| "OpenRouter callback contained no authorization code".into())
}

#[derive(serde::Deserialize)]
struct ExchangeResponse {
    key: String,
}

/// Browser consent, one-time code exchange, and keychain publication.
/// The provider already supports model discovery and streaming using this key.
#[tauri::command]
pub async fn providers_openrouter_login(app: tauri::AppHandle) -> Result<(), String> {
    // Serialize to avoid two simultaneous windows racing over browser callbacks
    // or keychain entries. No credentials are overwritten until exchange succeeds.
    let _guard = LOGIN_LOCK.lock().await;
    let listener = LoopbackListener::bind().await?;
    let verifier = random_verifier();
    let state = uuid::Uuid::new_v4().to_string();
    let authorize = authorization_url(&listener.redirect_uri, &verifier, &state)?;

    app.opener().open_url(authorize, None::<String>)
        .map_err(|e| format!("Could not open browser for OpenRouter login: {e}"))?;

    let callback = listener.await_callback(Duration::from_secs(300)).await?;
    let code = validate_callback(callback, &state)?;

    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| format!("Failed to create OpenRouter OAuth client: {e}"))?
        .post(EXCHANGE_ENDPOINT)
        .json(&serde_json::json!({
            "code": code,
            "code_verifier": verifier,
            "code_challenge_method": "S256"
        }))
        .send().await
        .map_err(|_| "OpenRouter token exchange failed (network error)".to_string())?;

    if !response.status().is_success() {
        // The response may contain sensitive authorization details.
        return Err(format!("OpenRouter token exchange rejected (HTTP {})", response.status()));
    }
    let result: ExchangeResponse = response.json().await
        .map_err(|_| "OpenRouter returned an invalid token exchange response".to_string())?;
    if !result.key.starts_with("sk-or-") {
        return Err("OpenRouter returned an unexpected API key format".into());
    }
    crate::providers::write_key("openrouter", &result.key)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_url_has_s256_and_loopback() {
        let url = url::Url::parse(&authorization_url(
            "http://127.0.0.1:51234/", "correct-horse-battery", "nonce123",
        ).unwrap()).unwrap();
        let pairs: std::collections::HashMap<_,_> =
            url.query_pairs().map(|(k,v)|(k.into_owned(),v.into_owned())).collect();
        assert_eq!(url.host_str(), Some("openrouter.ai"));
        assert_eq!(pairs.get("callback_url").map(String::as_str), Some("http://127.0.0.1:51234/"));
        assert_eq!(pairs.get("code_challenge_method").map(String::as_str), Some("S256"));
        assert_eq!(pairs.get("code_challenge").unwrap(), &code_challenge("correct-horse-battery"));
        assert_eq!(pairs.get("state").map(String::as_str), Some("nonce123"));
        assert!(!url.as_str().contains("correct-horse-battery"));
        assert!(authorization_url("https://evil.invalid/", "x", "y").is_err());
    }

    #[test]
    fn never_accept_unmatched_state_or_provider_error() {
        let mut callback = CallbackParams::default();
        callback.code = Some("authorization-code".into());
        callback.state = Some("from-browser".into());
        assert!(validate_callback(callback.clone(), "expected").is_err());
        assert_eq!(validate_callback(callback, "from-browser").unwrap(), "authorization-code");
        let mut denied = CallbackParams::default();
        denied.error = Some("access_denied".into());
        denied.state = Some("expected".into());
        assert!(validate_callback(denied, "expected").is_err());
    }
}

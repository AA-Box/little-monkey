//! Verifiable, build-trusted entitlement to use locally installed MiniMax H3
//! weights in territories excluded by the community license.
//!
//! This is NOT a replacement for written MiniMax authorization. A distributor
//! must check that authorization before issuing a signed entitlement. Ordinary
//! consumer builds ship with no trusted issuer key and therefore fail closed.
//! Users cannot select a replacement key or sign their own declaration.
use crate::studio_model_policy::LicenseRegion;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use ring::signature::{UnparsedPublicKey, ED25519};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

const TRUSTED_ISSUER_KEY: Option<&str> =
    option_env!("LITTLE_MONKEY_MINIMAX_GRANT_ED25519_PUBKEY_B64");
const MAX_GRANT_BYTES: usize = 8192;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SignedGrant {
    pub payload_base64: String,
    pub signature_base64: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GrantPayload {
    /// Audience binding makes the signature useless in another application.
    pub audience: String,
    pub model_family: String,
    /// Reference to separately reviewed rights-holder authorization.
    pub license_reference: String,
    pub holder_id: String,
    pub allowed_regions: Vec<LicenseRegion>,
    pub not_before_unix: u64,
    pub expires_at_unix: u64,
}

fn validate_signed(
    grant: &SignedGrant,
    public_key_b64: &str,
    region: LicenseRegion,
    now: u64,
) -> Result<GrantPayload, String> {
    let public_key = STANDARD.decode(public_key_b64).map_err(|_| "Invalid trusted issuer public key")?;
    if public_key.len() != 32 {
        return Err("Invalid Ed25519 issuer key length".into());
    }
    let payload = STANDARD.decode(&grant.payload_base64)
        .map_err(|_| "Invalid grant payload encoding")?;
    let signature = STANDARD.decode(&grant.signature_base64)
        .map_err(|_| "Invalid grant signature encoding")?;
    if payload.len() > MAX_GRANT_BYTES || signature.len() != 64 {
        return Err("Grant size or signature length is invalid".into());
    }
    UnparsedPublicKey::new(&ED25519, public_key)
        .verify(&payload, &signature)
        .map_err(|_| "The grant signature was not issued by the trusted distributor")?;
    let decoded: GrantPayload =
        serde_json::from_slice(&payload).map_err(|_| "Invalid entitlement payload")?;
    if decoded.audience != "little-monkey" || decoded.model_family != "minimax-h3" {
        return Err("Entitlement was not issued for Little Monkey MiniMax H3".into());
    }
    if decoded.holder_id.trim().is_empty()
        || decoded.holder_id.len() > 128
        || decoded.license_reference.trim().is_empty()
        || decoded.license_reference.len() > 256 {
        return Err("Entitlement holder or verified authorization reference is invalid".into());
    }
    if decoded.not_before_unix > now || decoded.expires_at_unix <= now
        || decoded.expires_at_unix <= decoded.not_before_unix {
        return Err("Entitlement is not yet valid or has expired".into());
    }
    if !decoded.allowed_regions.contains(&region) {
        return Err("This entitlement does not cover the declared use territory".into());
    }
    Ok(decoded)
}

pub fn validate_for_region(grant: &SignedGrant, region: LicenseRegion) -> Result<(), String> {
    let issuer = TRUSTED_ISSUER_KEY.ok_or(
        "This build has no trusted MiniMax entitlement issuer. Contact the distributor after obtaining written MiniMax authorization."
    )?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)
        .map_err(|_| "System clock is invalid")?.as_secs();
    validate_signed(grant, issuer, region, now).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{Ed25519KeyPair, KeyPair};

    fn fixture() -> (String, SignedGrant, u64) {
        let key = Ed25519KeyPair::from_seed_unchecked(&[27_u8; 32]).unwrap();
        let now = 1_800_000_000_u64;
        let payload = GrantPayload {
            audience: "little-monkey".into(),
            model_family: "minimax-h3".into(),
            license_reference: "MiniMax-written-license-verified-by-issuer".into(),
            holder_id: "enterprise-test".into(),
            allowed_regions: vec![LicenseRegion::Eu, LicenseRegion::Us],
            not_before_unix: now - 10,
            expires_at_unix: now + 60,
        };
        let bytes = serde_json::to_vec(&payload).unwrap();
        let grant = SignedGrant {
            payload_base64: STANDARD.encode(&bytes),
            signature_base64: STANDARD.encode(key.sign(&bytes).as_ref()),
        };
        (STANDARD.encode(key.public_key().as_ref()), grant, now)
    }

    #[test]
    fn verifies_real_ed25519_signature_expiry_and_territory() {
        let (pubkey, grant, now) = fixture();
        assert!(validate_signed(&grant, &pubkey, LicenseRegion::Eu, now).is_ok());
        assert!(validate_signed(&grant, &pubkey, LicenseRegion::Us, now).is_ok());
        assert!(validate_signed(&grant, &pubkey, LicenseRegion::Kr, now).is_err());
        assert!(validate_signed(&grant, &pubkey, LicenseRegion::Eu, now + 61).is_err());
        let mut tampered = grant.clone();
        tampered.payload_base64 = STANDARD.encode(b"{\"audience\":\"little-monkey\"}");
        assert!(validate_signed(&tampered, &pubkey, LicenseRegion::Eu, now).is_err());
        let untrusted = STANDARD.encode([1_u8; 32]);
        assert!(validate_signed(&grant, &untrusted, LicenseRegion::Eu, now).is_err());
    }
}

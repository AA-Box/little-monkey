//! Guest-side helpers for the `little-monkey:extension@1.0.0` contract.
//!
//! Host capabilities are generated directly from the canonical WIT file in
//! each guest crate. This crate deliberately contains only JSON-boundary
//! helpers, so using it cannot accidentally add ambient host authority.

use serde::Serialize;
use serde::de::DeserializeOwned;

pub const HOST_API_VERSION: &str = "1.0.0";
pub const MAX_INPUT_JSON_BYTES: usize = 1024 * 1024;
pub const MAX_OUTPUT_JSON_BYTES: usize = 4 * 1024 * 1024;

pub fn require_capability(actual: &str, expected: &str) -> Result<(), String> {
    if actual == expected {
        Ok(())
    } else {
        Err(format!("unsupported capability '{actual}'"))
    }
}

pub fn parse_input<T: DeserializeOwned>(input_json: &str) -> Result<T, String> {
    if input_json.len() > MAX_INPUT_JSON_BYTES {
        return Err("input JSON exceeds the 1 MiB guest limit".to_string());
    }
    serde_json::from_str(input_json).map_err(|error| format!("invalid input JSON: {error}"))
}

pub fn json_output<T: Serialize>(value: &T) -> Result<String, String> {
    let output = serde_json::to_string(value)
        .map_err(|error| format!("cannot serialize output JSON: {error}"))?;
    if output.len() > MAX_OUTPUT_JSON_BYTES {
        return Err("output JSON exceeds the 4 MiB guest limit".to_string());
    }
    Ok(output)
}

pub fn validate_bounded_id(label: &str, value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 160
        || value.starts_with('-')
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        Err(format!("{label} must be a bounded ASCII identifier"))
    } else {
        Ok(())
    }
}

pub fn validate_max_chars(label: &str, value: &str, maximum: usize) -> Result<(), String> {
    if value.chars().count() <= maximum {
        Ok(())
    } else {
        Err(format!("{label} exceeds {maximum} characters"))
    }
}

pub fn validate_sha256(label: &str, value: &str) -> Result<(), String> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        Ok(())
    } else {
        Err(format!(
            "{label} must be a 64-character lowercase SHA-256 digest"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Deserialize, Serialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct Payload {
        value: u32,
    }

    #[test]
    fn json_boundary_is_strict_and_round_trips() {
        let payload: Payload = parse_input(r#"{"value":7}"#).unwrap();
        assert_eq!(payload, Payload { value: 7 });
        assert_eq!(json_output(&payload).unwrap(), r#"{"value":7}"#);
        assert!(parse_input::<Payload>(r#"{"value":7,"extra":true}"#).is_err());
    }

    #[test]
    fn capability_and_identifier_checks_fail_closed() {
        assert!(require_capability("echo", "echo").is_ok());
        assert!(require_capability("other", "echo").is_err());
        assert!(validate_bounded_id("event id", "evt-1").is_ok());
        assert!(validate_bounded_id("event id", "../evt").is_err());
        assert!(validate_bounded_id("event id", "-evt").is_err());
        assert!(validate_max_chars("text", "🐒", 1).is_ok());
        assert!(validate_max_chars("text", "two", 2).is_err());
        assert!(validate_sha256("artifact", &"a".repeat(64)).is_ok());
        assert!(validate_sha256("artifact", &"A".repeat(64)).is_err());
        assert!(validate_sha256("artifact", "not-a-digest").is_err());
    }
}

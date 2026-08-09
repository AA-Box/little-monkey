//! Tauri command boundary for the durable content-addressed artifact store.
//!
//! The storage implementation itself stays platform-neutral in
//! `artifact_store.rs`; this module owns app-data path resolution, bounded
//! base64 transport for the webview, and the lazily initialized shared handle.
//! Callers never receive an internal filesystem path and therefore cannot use
//! an artifact id as a path traversal primitive.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::Serialize;

use crate::artifact_store::{ArtifactBlob, ArtifactStore};
use crate::AppState;
use crate::profiles::ProfileScopedPaths;

const STORE_DIRECTORY: &str = "content-v1";

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactBlobContent {
    pub blob: ArtifactBlob,
    pub content_base64: String,
}

pub(crate) fn store_for(app: &tauri::AppHandle, state: &AppState) -> Result<ArtifactStore, String> {
    let mut slot = state
        .durable_artifacts
        .lock()
        .map_err(|_| "Artifact store state lock was poisoned".to_string())?;
    if let Some(store) = slot.as_ref() {
        return Ok(store.clone());
    }

    let root = app
        .profile_data_dir()
        .map_err(|error| format!("Failed to resolve app data dir: {error}"))?
        .join(STORE_DIRECTORY);
    let store = ArtifactStore::new(root).map_err(|error| error.to_string())?;
    *slot = Some(store.clone());
    Ok(store)
}

fn max_encoded_len(max_decoded_bytes: u64) -> usize {
    let encoded = max_decoded_bytes.saturating_add(2) / 3 * 4;
    usize::try_from(encoded).unwrap_or(usize::MAX)
}

pub(crate) fn decode_bounded(
    content_base64: &str,
    max_decoded_bytes: u64,
    label: &str,
) -> Result<Vec<u8>, String> {
    if content_base64.len() > max_encoded_len(max_decoded_bytes) {
        return Err(format!(
            "Encoded {label} exceeds the {max_decoded_bytes}-byte decoded limit"
        ));
    }
    let bytes = STANDARD
        .decode(content_base64)
        .map_err(|error| format!("{label} content is not valid base64: {error}"))?;
    let observed = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if observed > max_decoded_bytes {
        return Err(format!(
            "{label} is {observed} bytes, exceeding the {max_decoded_bytes}-byte limit"
        ));
    }
    Ok(bytes)
}

/// Returns verified bytes only. `ArtifactStore::read` checks the size,
/// regular-file identity, and SHA-256 digest before this command encodes them.
#[tauri::command]
pub fn artifact_blob_read_base64(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<ArtifactBlobContent, String> {
    let bytes = store_for(&app, state.inner())?
        .read(&id)
        .map_err(|error| error.to_string())?;
    Ok(ArtifactBlobContent {
        blob: ArtifactBlob {
            id,
            size: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        },
        content_base64: STANDARD.encode(bytes),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_bound_accounts_for_base64_overhead() {
        assert_eq!(max_encoded_len(0), 0);
        assert_eq!(max_encoded_len(1), 4);
        assert_eq!(max_encoded_len(3), 4);
        assert_eq!(max_encoded_len(4), 8);
    }

    #[test]
    fn bounded_decode_roundtrips_and_rejects_invalid_input() {
        assert_eq!(decode_bounded("aGVsbG8=", 5, "artifact").unwrap(), b"hello");
        assert!(decode_bounded("aGVsbG8=", 4, "artifact").is_err());
        assert!(decode_bounded("not base64", 128, "artifact").is_err());
    }

    #[test]
    fn bounded_decode_rejects_encoded_input_before_allocating_output() {
        let oversized = "A".repeat(max_encoded_len(4) + 1);
        let error = decode_bounded(&oversized, 4, "artifact").unwrap_err();
        assert!(error.contains("Encoded artifact exceeds"));
    }
}

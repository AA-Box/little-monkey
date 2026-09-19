use super::{DictationCapabilities, DictationLanguage, DictationPermissions};

pub struct Session;

pub fn capabilities() -> DictationCapabilities {
    DictationCapabilities {
        supported: false,
        platform: "unsupported".to_string(),
        engine: String::new(),
        supports_partial_results: false,
        supports_on_device: false,
        languages: Vec::<DictationLanguage>::new(),
        permissions: DictationPermissions::unavailable(),
    }
}

pub fn start() -> Result<Session, String> {
    Err("Native OS speech recognition is not supported on this platform".to_string())
}

pub fn open_permission_settings() -> Result<(), String> {
    Err("This platform has no native dictation permission settings".to_string())
}

impl Session {
    pub fn stop(&self) -> Result<(), String> {
        Ok(())
    }

    pub fn cancel(&self) -> Result<(), String> {
        Ok(())
    }
}

/// Nothing to ask on this platform.
///
/// `Unknown` rather than `Granted`: an unpackaged Win32 app has no per-app
/// microphone prompt — the webview shows its own — and claiming a grant nobody
/// made would be a lie the caller acts on. The shared helper only refuses on
/// `denied`/`restricted`, so this means "carry on".
pub async fn request_microphone_access() -> super::DictationPermissionStatus {
    super::DictationPermissionStatus::Unknown
}

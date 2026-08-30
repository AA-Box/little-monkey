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

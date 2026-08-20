use super::{DictationCapabilities, DictationLanguage};

pub struct Session;

pub fn capabilities() -> DictationCapabilities {
    DictationCapabilities {
        supported: false,
        platform: "unsupported".to_string(),
        engine: String::new(),
        supports_partial_results: false,
        supports_on_device: false,
        languages: Vec::<DictationLanguage>::new(),
    }
}

pub fn start() -> Result<Session, String> {
    Err("Native OS speech recognition is not supported on this platform".to_string())
}

impl Session {
    pub fn stop(&self) -> Result<(), String> {
        Ok(())
    }

    pub fn cancel(&self) -> Result<(), String> {
        Ok(())
    }
}

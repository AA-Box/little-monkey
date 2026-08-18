mod bindings {
    wit_bindgen::generate!({
        path: "../../../src-tauri/wit",
        world: "extension",
    });
}

use bindings::exports::little_monkey::extension::guest::Guest;
use bindings::little_monkey::extension::host;
use little_monkey_extension_sdk::{
    json_output, parse_input, require_capability, validate_max_chars, validate_sha256,
};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TranscribeInput {
    artifact_id: String,
    language: Option<String>,
}

#[derive(Serialize)]
struct TranscribeOutput {
    text: String,
    language: String,
    audio_bytes: usize,
}

struct MockSttProvider;

impl Guest for MockSttProvider {
    fn run(capability_id: String, input_json: String) -> Result<String, String> {
        require_capability(&capability_id, "transcribe")?;
        let input: TranscribeInput = parse_input(&input_json)?;
        if host::is_cancelled() {
            return Err("cancelled".to_string());
        }
        validate_sha256("artifact id", &input.artifact_id)?;
        let audio = host::artifact_read(&input.artifact_id)?;
        let configured_language = host::config_get("default-language")?
            .map(|value| {
                serde_json::from_str::<String>(&value)
                    .map_err(|error| format!("invalid default-language config: {error}"))
            })
            .transpose()?;
        let language = input
            .language
            .or(configured_language)
            .unwrap_or_else(|| "und".to_string());
        validate_max_chars("language", &language, 32)?;
        json_output(&TranscribeOutput {
            text: format!("mock transcript for {} bytes", audio.len()),
            language,
            audio_bytes: audio.len(),
        })
    }
}

bindings::export!(MockSttProvider with_types_in bindings);

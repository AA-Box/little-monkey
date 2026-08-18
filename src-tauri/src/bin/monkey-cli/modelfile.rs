//! Adapter from the shared, hardened Modelfile parser to Ollama's
//! `POST /api/create` request. The grammar lives in
//! `little_monkey_lib::modelfile`; keeping a second parser here previously
//! made the CLI accept duplicate singular instructions and silently discard
//! `REQUIRES`, extra licenses, and extra adapters.

use crate::ollama_api::{ChatMessage, CreateRequest};

pub use little_monkey_lib::modelfile::ParsedModelfile;

/// Parse with the same grammar and validation diagnostics as Modelfile
/// Studio. Semantic checks that depend on which create transport is used are
/// handled by [`to_create_request`].
pub fn parse(text: &str) -> Result<ParsedModelfile, String> {
    little_monkey_lib::modelfile::parse_modelfile(text).map_err(|error| error.to_string())
}

/// Maps a parsed Modelfile onto an `/api/create` request for `model`.
///
/// The daemon request can reference an existing model, but this CLI path does
/// not upload local FROM/ADAPTER blobs. `REQUIRES` also has no field in the
/// create API, so it is rejected explicitly instead of being silently lost.
pub fn to_create_request(
    parsed: ParsedModelfile,
    model: &str,
    quantize: Option<String>,
) -> Result<CreateRequest, String> {
    let from = parsed
        .from
        .ok_or_else(|| "Modelfile has no FROM instruction".to_string())?;

    if looks_like_local_reference(&from) {
        return Err(format!(
            "FROM '{from}' points at a local file/directory. GGUF/safetensors import via monkey create \
             requires blob uploads; use `ollama create` for file imports."
        ));
    }
    if !parsed.adapters.is_empty() {
        return Err(
            "ADAPTER requires uploading local files, which monkey create doesn't support; \
             use `ollama create` instead."
                .to_string(),
        );
    }
    if let Some(requires) = parsed.requires {
        return Err(format!(
            "REQUIRES {requires} cannot be represented by Ollama's /api/create request; \
             use `ollama create` so the requirement is preserved."
        ));
    }

    let mut parameters = serde_json::Map::new();
    for parameter in parsed.parameters {
        let key = parameter.key.to_ascii_lowercase();
        let value = coerce(&parameter.value);
        if key == "stop" {
            match parameters.get_mut("stop") {
                Some(serde_json::Value::Array(stops)) => stops.push(value),
                _ => {
                    parameters.insert("stop".to_string(), serde_json::Value::Array(vec![value]));
                }
            }
        } else {
            // Matches Ollama's effective behavior for repeatable non-stop
            // parameters while the shared parsed form preserves source order.
            parameters.insert(key, value);
        }
    }

    let messages = parsed
        .messages
        .into_iter()
        .map(|message| ChatMessage {
            role: message.role,
            content: message.content,
        })
        .collect::<Vec<_>>();

    let license = match parsed.licenses.as_slice() {
        [] => None,
        [license] => Some(serde_json::Value::String(license.clone())),
        licenses => Some(serde_json::Value::Array(
            licenses
                .iter()
                .cloned()
                .map(serde_json::Value::String)
                .collect(),
        )),
    };

    Ok(CreateRequest {
        model: model.to_string(),
        from: Some(from),
        files: None,
        adapters: None,
        template: parsed.template,
        system: parsed.system,
        parameters: (!parameters.is_empty()).then_some(parameters),
        messages: (!messages.is_empty()).then_some(messages),
        license,
        quantize,
        stream: true,
    })
}

fn looks_like_local_reference(value: &str) -> bool {
    let value = value.trim();
    std::path::Path::new(value).exists()
        || value.starts_with("./")
        || value.starts_with("../")
        || value.starts_with('~')
        || value.starts_with('/')
        || value.starts_with('\\')
        || value
            .as_bytes()
            .get(1)
            .is_some_and(|byte| *byte == b':' && value.as_bytes()[0].is_ascii_alphabetic())
        || [".gguf", ".safetensors"]
            .iter()
            .any(|extension| value.to_ascii_lowercase().ends_with(extension))
}

/// Types a parameter value: integer, then float, then bool, else string.
fn coerce(text: &str) -> serde_json::Value {
    if let Ok(int) = text.parse::<i64>() {
        return serde_json::Value::from(int);
    }
    if let Ok(float) = text.parse::<f64>() {
        if float.is_finite() {
            return serde_json::Value::from(float);
        }
    }
    match text {
        "true" => serde_json::Value::Bool(true),
        "false" => serde_json::Value::Bool(false),
        _ => serde_json::Value::String(text.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_parser_handles_case_types_and_multiline_values() {
        let parsed = parse(
            "from llama3\nparameter num_ctx 4096\nPARAMETER temperature 0.2\n\
             SYSTEM \"\"\"\nYou are terse.\n\"\"\"\n",
        )
        .unwrap();
        let request = to_create_request(parsed, "custom", None).unwrap();

        assert_eq!(request.from.as_deref(), Some("llama3"));
        assert_eq!(request.system.as_deref(), Some("You are terse."));
        let parameters = request.parameters.unwrap();
        assert_eq!(parameters["num_ctx"], serde_json::json!(4096));
        assert_eq!(parameters["temperature"], serde_json::json!(0.2));
    }

    #[test]
    fn repeated_stop_parameters_accumulate() {
        let parsed = parse("FROM m\nPARAMETER stop \"AI:\"\nPARAMETER stop User:\n").unwrap();
        let request = to_create_request(parsed, "custom", None).unwrap();
        assert_eq!(
            request.parameters.unwrap()["stop"],
            serde_json::json!(["AI:", "User:"])
        );
    }

    #[test]
    fn maps_messages_licenses_and_quantization_without_loss() {
        let parsed = parse(
            "FROM qwen3-coder:latest\nLICENSE MIT\nLICENSE Apache-2.0\n\
             MESSAGE user hi\nMESSAGE assistant hello\n",
        )
        .unwrap();
        let request = to_create_request(parsed, "my-model", Some("q4_K_M".to_string())).unwrap();

        assert_eq!(request.quantize.as_deref(), Some("q4_K_M"));
        assert_eq!(request.messages.unwrap().len(), 2);
        assert_eq!(
            request.license,
            Some(serde_json::json!(["MIT", "Apache-2.0"]))
        );
    }

    #[test]
    fn shared_parser_rejects_duplicate_singular_instructions() {
        let error = parse("FROM llama3\nFROM qwen3\n").unwrap_err();
        assert!(error.contains("line 2"));
        assert!(error.contains("duplicate FROM"));
    }

    #[test]
    fn unsupported_create_transport_features_fail_explicitly() {
        assert!(to_create_request(ParsedModelfile::default(), "m", None)
            .unwrap_err()
            .contains("FROM"));

        let parsed = parse("FROM ./weights.gguf\n").unwrap();
        assert!(to_create_request(parsed, "m", None)
            .unwrap_err()
            .contains("blob uploads"));

        let parsed = parse("FROM qwen3\nADAPTER ./lora.gguf\n").unwrap();
        assert!(to_create_request(parsed, "m", None)
            .unwrap_err()
            .contains("ADAPTER"));

        let parsed = parse("FROM qwen3\nREQUIRES 0.14.0\n").unwrap();
        assert!(to_create_request(parsed, "m", None)
            .unwrap_err()
            .contains("cannot be represented"));
    }
}

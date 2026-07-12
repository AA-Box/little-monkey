//! Modelfile parser for `monkey-cli create`: turns the Ollama Modelfile format
//! (FROM/PARAMETER/TEMPLATE/SYSTEM/ADAPTER/LICENSE/MESSAGE, `#` comments,
//! triple-quoted `"""..."""` multiline values) into the daemon's
//! `POST /api/create` request shape. File-based imports (FROM/ADAPTER
//! pointing at local GGUF/safetensors) need blob uploads the CLI doesn't do,
//! so those fail with a pointer to `ollama create` instead.

use crate::ollama_api::{ChatMessage, CreateRequest};

#[derive(Debug, Default)]
pub struct ParsedModelfile {
    pub from: Option<String>,
    pub template: Option<String>,
    pub system: Option<String>,
    pub parameters: serde_json::Map<String, serde_json::Value>,
    pub messages: Vec<ChatMessage>,
    pub license: Option<String>,
    pub adapter: Option<String>,
}

/// Parses Modelfile text. Instructions are case-insensitive; repeated `stop`
/// parameters accumulate into an array; numeric/bool parameter values are
/// typed, everything else stays a string.
pub fn parse(text: &str) -> Result<ParsedModelfile, String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut parsed = ParsedModelfile::default();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i].trim();
        i += 1;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line_no = i; // 1-based line the instruction started on
        let (instruction, rest) = match line.split_once(char::is_whitespace) {
            Some((ins, rest)) => (ins, rest.trim_start()),
            None => (line, ""),
        };

        match instruction.to_ascii_uppercase().as_str() {
            "FROM" => parsed.from = Some(read_value(rest, &lines, &mut i)?),
            "TEMPLATE" => parsed.template = Some(read_value(rest, &lines, &mut i)?),
            "SYSTEM" => parsed.system = Some(read_value(rest, &lines, &mut i)?),
            "LICENSE" => parsed.license = Some(read_value(rest, &lines, &mut i)?),
            "ADAPTER" => parsed.adapter = Some(read_value(rest, &lines, &mut i)?),
            "PARAMETER" => {
                let (key, value_raw) = rest.split_once(char::is_whitespace).ok_or(format!(
                    "PARAMETER needs a name and a value (line {line_no})"
                ))?;
                let key = key.to_ascii_lowercase();
                let value = coerce(&read_value(value_raw.trim_start(), &lines, &mut i)?);
                if key == "stop" {
                    match parsed.parameters.get_mut("stop") {
                        Some(serde_json::Value::Array(stops)) => stops.push(value),
                        _ => {
                            parsed
                                .parameters
                                .insert("stop".to_string(), serde_json::Value::Array(vec![value]));
                        }
                    }
                } else {
                    parsed.parameters.insert(key, value);
                }
            }
            "MESSAGE" => {
                let (role, content_raw) = rest.split_once(char::is_whitespace).ok_or(format!(
                    "MESSAGE needs a role and content (line {line_no})"
                ))?;
                let role = role.to_ascii_lowercase();
                if !matches!(role.as_str(), "system" | "user" | "assistant") {
                    return Err(format!(
                        "Invalid MESSAGE role '{role}' (line {line_no}): expected system, user, or assistant"
                    ));
                }
                let content = read_value(content_raw.trim_start(), &lines, &mut i)?;
                parsed.messages.push(ChatMessage { role, content });
            }
            other => {
                return Err(format!("Unknown Modelfile instruction '{other}' on line {line_no}"))
            }
        }
    }

    Ok(parsed)
}

/// Maps a parsed Modelfile onto an `/api/create` request for `model`.
/// Errors when FROM is missing, FROM points at an on-disk path, or an
/// ADAPTER is present — those need blob uploads (`ollama create` territory).
pub fn to_create_request(
    parsed: ParsedModelfile,
    model: &str,
    quantize: Option<String>,
) -> Result<CreateRequest, String> {
    let from = parsed
        .from
        .ok_or("Modelfile has no FROM instruction".to_string())?;
    if std::path::Path::new(&from).exists() {
        return Err(format!(
            "FROM '{from}' points at a local file/directory. GGUF/safetensors import via monkey-cli create \
             requires FROM <existing-model>; use `ollama create` for file imports."
        ));
    }
    if parsed.adapter.is_some() {
        return Err(
            "ADAPTER requires uploading local files, which monkey-cli create doesn't support; \
             use `ollama create` instead."
                .to_string(),
        );
    }
    Ok(CreateRequest {
        model: model.to_string(),
        from: Some(from),
        files: None,
        adapters: None,
        template: parsed.template,
        system: parsed.system,
        parameters: if parsed.parameters.is_empty() { None } else { Some(parsed.parameters) },
        messages: if parsed.messages.is_empty() { None } else { Some(parsed.messages) },
        license: parsed.license.map(serde_json::Value::String),
        quantize,
        stream: true,
    })
}

/// Reads an instruction's value: a triple-quoted `"""..."""` block (possibly
/// spanning lines — `i` advances past any consumed ones, and the content
/// between the quotes is kept verbatim), or a single-line value with one
/// pair of surrounding double quotes stripped.
fn read_value(first: &str, lines: &[&str], i: &mut usize) -> Result<String, String> {
    let first = first.trim();
    if let Some(after) = first.strip_prefix("\"\"\"") {
        if let Some(end) = after.find("\"\"\"") {
            return Ok(after[..end].to_string());
        }
        let mut parts: Vec<String> = vec![after.to_string()];
        while *i < lines.len() {
            let line = lines[*i];
            *i += 1;
            if let Some(end) = line.find("\"\"\"") {
                parts.push(line[..end].to_string());
                return Ok(parts.join("\n"));
            }
            parts.push(line.to_string());
        }
        return Err("Unterminated \"\"\" block in Modelfile".to_string());
    }
    if first.len() >= 2 && first.starts_with('"') && first.ends_with('"') {
        return Ok(first[1..first.len() - 1].to_string());
    }
    Ok(first.to_string())
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
    fn parses_basic_modelfile() {
        let parsed = parse(
            "# a comment\n\nFROM qwen3-coder:latest\nPARAMETER temperature 0.2\nSYSTEM You are terse.\n",
        )
        .unwrap();
        assert_eq!(parsed.from.as_deref(), Some("qwen3-coder:latest"));
        assert_eq!(parsed.parameters["temperature"], serde_json::json!(0.2));
        assert_eq!(parsed.system.as_deref(), Some("You are terse."));
        assert!(parsed.template.is_none() && parsed.adapter.is_none() && parsed.license.is_none());
        assert!(parsed.messages.is_empty());
    }

    #[test]
    fn instructions_are_case_insensitive() {
        let parsed = parse("from llama3\nparameter num_ctx 4096\nSystem hi\n").unwrap();
        assert_eq!(parsed.from.as_deref(), Some("llama3"));
        assert_eq!(parsed.parameters["num_ctx"], serde_json::json!(4096));
        assert_eq!(parsed.system.as_deref(), Some("hi"));
    }

    #[test]
    fn parameter_values_are_typed() {
        let parsed = parse(
            "FROM m\nPARAMETER num_ctx 8192\nPARAMETER temperature 0.7\nPARAMETER penalize_newline true\nPARAMETER mirostat_eta 1e-3\nPARAMETER seed -1\n",
        )
        .unwrap();
        assert_eq!(parsed.parameters["num_ctx"], serde_json::json!(8192));
        assert_eq!(parsed.parameters["temperature"], serde_json::json!(0.7));
        assert_eq!(parsed.parameters["penalize_newline"], serde_json::json!(true));
        assert_eq!(parsed.parameters["mirostat_eta"], serde_json::json!(0.001));
        assert_eq!(parsed.parameters["seed"], serde_json::json!(-1));
    }

    #[test]
    fn repeated_stop_parameters_accumulate() {
        let parsed = parse("FROM m\nPARAMETER stop \"AI:\"\nPARAMETER stop User:\n").unwrap();
        assert_eq!(parsed.parameters["stop"], serde_json::json!(["AI:", "User:"]));
    }

    #[test]
    fn triple_quoted_values_single_and_multi_line() {
        let parsed = parse(
            "FROM m\nSYSTEM \"\"\"You are terse.\"\"\"\nTEMPLATE \"\"\"\n{{ .System }}\n{{ .Prompt }}\n\"\"\"\n",
        )
        .unwrap();
        assert_eq!(parsed.system.as_deref(), Some("You are terse."));
        assert_eq!(parsed.template.as_deref(), Some("\n{{ .System }}\n{{ .Prompt }}\n"));
    }

    #[test]
    fn messages_and_license_parse() {
        let parsed = parse(
            "FROM m\nMESSAGE user Is Toronto in Canada?\nMESSAGE assistant Yes.\nLICENSE \"\"\"MIT\"\"\"\n",
        )
        .unwrap();
        assert_eq!(parsed.messages.len(), 2);
        assert_eq!(parsed.messages[0].role, "user");
        assert_eq!(parsed.messages[0].content, "Is Toronto in Canada?");
        assert_eq!(parsed.messages[1].role, "assistant");
        assert_eq!(parsed.license.as_deref(), Some("MIT"));
    }

    #[test]
    fn invalid_input_errors() {
        assert!(parse("FROM m\nBOGUS thing\n").unwrap_err().contains("BOGUS"));
        assert!(parse("FROM m\nPARAMETER temperature\n").unwrap_err().contains("PARAMETER"));
        assert!(parse("FROM m\nMESSAGE narrator hi\n").unwrap_err().contains("role"));
        assert!(parse("FROM m\nSYSTEM \"\"\"never closed\n").unwrap_err().contains("Unterminated"));
    }

    #[test]
    fn to_create_request_maps_fields() {
        let parsed = parse(
            "FROM qwen3-coder:latest\nPARAMETER temperature 0.2\nPARAMETER stop END\nSYSTEM terse\nMESSAGE user hi\n",
        )
        .unwrap();
        let req = to_create_request(parsed, "my-model", Some("q4_K_M".to_string())).unwrap();
        assert_eq!(req.model, "my-model");
        assert_eq!(req.from.as_deref(), Some("qwen3-coder:latest"));
        assert_eq!(req.system.as_deref(), Some("terse"));
        assert_eq!(req.quantize.as_deref(), Some("q4_K_M"));
        assert!(req.stream);
        let params = req.parameters.unwrap();
        assert_eq!(params["temperature"], serde_json::json!(0.2));
        assert_eq!(params["stop"], serde_json::json!(["END"]));
        assert_eq!(req.messages.unwrap().len(), 1);
    }

    #[test]
    fn to_create_request_rejects_file_imports_and_missing_from() {
        assert!(to_create_request(ParsedModelfile::default(), "m", None)
            .unwrap_err()
            .contains("FROM"));

        let dir = std::env::temp_dir();
        let parsed = ParsedModelfile { from: Some(dir.to_string_lossy().to_string()), ..Default::default() };
        assert!(to_create_request(parsed, "m", None).unwrap_err().contains("ollama create"));

        let parsed = ParsedModelfile {
            from: Some("qwen3-coder:latest".to_string()),
            adapter: Some("./lora.gguf".to_string()),
            ..Default::default()
        };
        assert!(to_create_request(parsed, "m", None).unwrap_err().contains("ADAPTER"));
    }
}

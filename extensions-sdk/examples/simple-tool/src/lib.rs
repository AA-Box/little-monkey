mod bindings {
    wit_bindgen::generate!({
        path: "../../../src-tauri/wit",
        world: "extension",
    });
}

use bindings::exports::little_monkey::extension::guest::Guest;
use bindings::little_monkey::extension::host;
use little_monkey_extension_sdk::{
    json_output, parse_input, require_capability, validate_max_chars,
};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EchoInput {
    text: String,
}

#[derive(Serialize)]
struct EchoOutput {
    echoed: String,
}

struct SimpleTool;

impl Guest for SimpleTool {
    fn run(capability_id: String, input_json: String) -> Result<String, String> {
        require_capability(&capability_id, "echo")?;
        if host::is_cancelled() {
            return Err("cancelled".to_string());
        }
        let input: EchoInput = parse_input(&input_json)?;
        validate_max_chars("text", &input.text, 65_536)?;
        host::log("info", "simple-tool invocation")?;
        let output = json_output(&EchoOutput { echoed: input.text })?;
        host::set_tool_result(&output)?;
        Ok(output)
    }
}

bindings::export!(SimpleTool with_types_in bindings);

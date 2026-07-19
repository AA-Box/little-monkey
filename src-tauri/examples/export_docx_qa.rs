//! Generates the maintained Word-export layout fixture for visual QA.
//!
//! Usage:
//!   cargo run --example export_docx_qa -- /absolute/path/to/output.docx

use little_monkey_lib::portability::{
    export_session_docx, PortableContentBlock, PortableMessage, PortableSession,
};
use serde_json::json;

fn message(
    id: &str,
    role: &str,
    ordinal: u64,
    blocks: Vec<PortableContentBlock>,
) -> PortableMessage {
    PortableMessage {
        id: id.to_string(),
        role: role.to_string(),
        ordinal,
        created_at_ms: 1_700_000_000_000 + ordinal,
        blocks,
        attachment_ids: Vec::new(),
        external_references: Vec::new(),
        translations: Vec::new(),
        metadata: json!({}),
    }
}

fn fixture() -> PortableSession {
    PortableSession {
        id: "session-docx-qa-v1".to_string(),
        title: "Little Monkey Word export layout acceptance".to_string(),
        ordinal: 0,
        created_at_ms: 1_700_000_000_000,
        updated_at_ms: 1_700_000_000_100,
        archived: false,
        pinned: true,
        model_key: Some("ollama/qwen-layout-fixture".to_string()),
        persona_id: Some("persona-reviewer".to_string()),
        workspace_path: Some("/workspace/little-monkey".to_string()),
        messages: vec![
            message(
                "message-docx-qa-1",
                "user",
                0,
                vec![PortableContentBlock::Text {
                    text: "Explain the export order, preserve punctuation such as <, >, &, and keep a deliberately long paragraph readable without clipping. This sentence repeats enough material to exercise line wrapping across the usable page width while remaining ordinary selectable document text.".to_string(),
                }],
            ),
            message(
                "message-docx-qa-2",
                "assistant",
                1,
                vec![
                    PortableContentBlock::Text {
                        text: "The answer begins with prose, followed by code and a table. Every block must remain in this exact order.\nA second line verifies explicit line breaks and paragraph spacing.".to_string(),
                    },
                    PortableContentBlock::Code {
                        language: Some("rust".to_string()),
                        code: "fn main() {\n    let marker = \"``` & <xml>\";\n    println!(\"{marker}\");\n}".to_string(),
                    },
                    PortableContentBlock::Table {
                        headers: vec![
                            "Item".to_string(),
                            "Status".to_string(),
                            "Detailed result".to_string(),
                        ],
                        rows: vec![
                            vec![
                                "Text".to_string(),
                                "Passed".to_string(),
                                "Long table content wraps inside its cell instead of crossing the page edge or overlapping the next row.".to_string(),
                            ],
                            vec![
                                "Code".to_string(),
                                "Passed".to_string(),
                                "Line breaks, symbols, and monospace-oriented content remain visible.".to_string(),
                            ],
                            vec![
                                "Table".to_string(),
                                "Passed".to_string(),
                                "Rows expand naturally and no text is clipped by a fixed height.".to_string(),
                            ],
                        ],
                    },
                ],
            ),
        ],
        translations: Vec::new(),
        metadata: json!({"fixture": "word-layout-v1"}),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = std::env::args_os()
        .nth(1)
        .ok_or("expected an absolute output .docx path")?;
    let output = std::path::PathBuf::from(output);
    if !output.is_absolute() || output.extension().and_then(|value| value.to_str()) != Some("docx")
    {
        return Err("output must be an absolute .docx path".into());
    }
    let bytes = export_session_docx(&fixture())?;
    std::fs::write(&output, bytes)?;
    println!("{}", output.display());
    Ok(())
}

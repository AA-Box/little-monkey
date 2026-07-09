//! OpenAI-style tool (function) definitions handed to the model, mirroring
//! `src/lib/tools.ts` 1:1 (minus the multi-root "label/" prefix wording —
//! the CLI supports a single `--workspace` root, so there's nothing to
//! disambiguate).

pub fn tool_definitions() -> serde_json::Value {
    serde_json::json!([
        {
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read the full text contents of a file in the workspace. Path is resolved relative to the workspace root and must not escape it.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Path to the file, relative to the workspace root." }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Create or overwrite a file in the workspace with the given content, creating parent directories as needed. Requires user permission.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Path to the file, relative to the workspace root." },
                        "content": { "type": "string", "description": "The full new contents of the file." }
                    },
                    "required": ["path", "content"],
                    "additionalProperties": false
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "edit_file",
                "description": "Replace a single unique occurrence of old_string with new_string in an existing file. Fails if old_string is not found or is not unique. Requires user permission.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Path to the file, relative to the workspace root." },
                        "old_string": { "type": "string", "description": "The exact, unique text to find in the file." },
                        "new_string": { "type": "string", "description": "The text to replace old_string with." }
                    },
                    "required": ["path", "old_string", "new_string"],
                    "additionalProperties": false
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "list_dir",
                "description": "List the entries of a directory in the workspace, returning each entry's name, whether it is a directory, and its size in bytes.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Path to the directory, relative to the workspace root." }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "grep",
                "description": "Search for a regular expression pattern across files in the workspace (skipping .git, node_modules, target, and dist), returning matching file, line number, and line text, capped at 200 matches.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Regular expression pattern to search for." },
                        "path": { "type": "string", "description": "Optional directory or file to scope the search to, relative to the workspace root. Defaults to the whole workspace." }
                    },
                    "required": ["pattern"],
                    "additionalProperties": false
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "run_shell",
                "description": "Run a shell command via `sh -c` in the workspace (or a subdirectory of it), with a 120 second timeout. Returns stdout, stderr, and exit code. Requires user permission.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "The shell command to execute." },
                        "cwd": { "type": "string", "description": "Optional working directory, relative to the workspace root. Defaults to the workspace root." }
                    },
                    "required": ["command"],
                    "additionalProperties": false
                }
            }
        }
    ])
}

//! The agent tool contract: the OpenAI-style tool (function) definitions the
//! agent loop hands to the model.
//!
//! **Why this lives in the library rather than beside its only caller.** These
//! four functions are the *source of truth* the published K19 contract
//! (`contract.rs`) generates its `tools` section from, and the contract
//! introspection endpoint is served by the two HTTP listeners in this library.
//! Leaving the definitions in `monkey-cli`'s `tools_def.rs` would have meant a
//! binary crate the library cannot read, so the published schema set would have
//! had to be a second, hand-written copy — exactly the thing K19 exists to
//! remove. `tools_def.rs` re-exports every item below, so the CLI's call sites
//! are unchanged and there is still only one definition of each tool.

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
                "name": "glob",
                "description": "Find files by glob pattern, skipping VCS, dependency, and build directories. Results are workspace-relative, most-recently-modified first, and capped at 300.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Glob pattern such as **/*.rs or src/**/test_*.py." },
                        "path": { "type": "string", "description": "Optional workspace-relative directory to search. Defaults to the whole workspace." }
                    },
                    "required": ["pattern"],
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
        },
        {
            "type": "function",
            "function": {
                "name": "remember",
                "description": "Save a short durable fact about this project or the user's preferences so future conversations remember it. Use for stated preferences, project conventions, and hard-won discoveries (build commands, gotchas). Requires user permission.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "text": { "type": "string", "description": "The fact to remember, written as a short standalone statement (max 500 characters)." }
                    },
                    "required": ["text"],
                    "additionalProperties": false
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "web_fetch",
                "description": "Fetch a web page (or plain text/Markdown/JSON/XML document) by URL and return its content as Markdown, with the page title and final URL (after redirects). Long content is windowed to max_chars starting at start_index; the result reports total_chars and truncated so you can page through the rest with a later call. Requires user permission.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "url": { "type": "string", "description": "The http(s) URL to fetch." },
                        "max_chars": { "type": "integer", "description": "Maximum characters of content to return in this call (default 20000)." },
                        "start_index": { "type": "integer", "description": "Character offset into the full content to start the returned window at (default 0). Use with total_chars/truncated from a previous call to page through a long page." }
                    },
                    "required": ["url"],
                    "additionalProperties": false
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "web_search",
                "description": "Search the web (keyless DuckDuckGo by default) and return up to `count` ranked results, each with a title, url, and snippet. Follow up with web_fetch to read a result in full. Requires user permission.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "The search query." },
                        "count": { "type": "integer", "description": "Number of results to return, 1-10 (default 10)." }
                    },
                    "required": ["query"],
                    "additionalProperties": false
                }
            }
        }
    ])
}

/// The Plan Mode "present a plan" tool — a Rust port of `src/lib/tools.ts`'s
/// `PRESENT_PLAN_TOOL`. Deliberately excluded from [`tool_definitions`]'s
/// base array (and from [`merged_tool_definitions`]'s output): it is only
/// appended to the per-turn tool list by `agent.rs::run_tool_loop`, and only
/// while the active `PermissionMode` is `Plan` — the same offer-only-in-
/// plan-mode rule as the desktop app's `toolsForMode`. Like its TS
/// counterpart, this has NO Rust `tool_present_plan` command anywhere:
/// `agent.rs`'s `execute_tool_call` handles the name directly (printing the
/// plan to the terminal and prompting to switch to Act mode) rather than
/// dispatching into the `tool_<name>` Tauri-command world this binary
/// doesn't have. An intentional three-way-registry-drift exception —
/// `src/lib/tools.ts`'s doc comment calls out the identical exception for
/// the desktop app; a reader scanning this file's `tool_definitions()` for a
/// `present_plan` Rust command elsewhere and finding nothing should look at
/// `agent.rs` instead of assuming a missing command.
pub fn present_plan_tool_def() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "present_plan",
            "description": "Present a structured plan to the user for approval. Call this exactly once, after investigating with read-only tools, then stop and wait — do not call it more than once per turn, and do not attempt to make changes until the user approves.",
            "parameters": {
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "A short (few words) title summarizing the plan." },
                    "plan": { "type": "string", "description": "The full plan, written as Markdown, describing the changes you intend to make and why." },
                    "open_questions": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional list of questions to ask the user before proceeding, if anything is ambiguous."
                    }
                },
                "required": ["title", "plan"],
                "additionalProperties": false
            }
        }
    })
}

/// The agent's provider-independent messaging tool.
///
/// By default it answers the conversation the run came from: with every
/// optional field omitted, the destination is the origin the daemon durably
/// recorded for this run, so the message being answered cannot redirect the
/// reply. `account`/`to`/`thread`/`reply_to` may name another destination,
/// but each is honored only when the run's immutable permission snapshot
/// granted it — naming an account id is not authority to use it, and the
/// tool refuses rather than prompts when the grant is absent.
///
/// Like [`present_plan_tool_def`], excluded from [`tool_definitions`]'s base
/// array: a run with no channel origin and no cross-send grant has nowhere to
/// send anything, and offering the tool there would only invite a failed call.
pub fn send_message_tool_def() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "send_message",
            "description": "Send a message over a configured messaging channel. With no destination fields it replies to the conversation this run came from. Destinations other than that conversation work only if this run was explicitly granted them. Requires user permission.",
            "parameters": {
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "The message to send. May be omitted when 'artifacts' names at least one file to send on its own." },
                    "account": { "type": "string", "description": "Optional configured account id to send through. Defaults to the account this run's conversation arrived on." },
                    "to": { "type": "string", "description": "Optional destination conversation id. Defaults to the conversation this run came from." },
                    "thread": { "type": "string", "description": "Optional provider thread id inside the destination conversation." },
                    "reply_to": { "type": "string", "description": "Optional provider message id to reply to. Defaults to the message that produced this run when replying to it." },
                    "artifacts": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional durable artifact ids of stored files to send, such as an image this conversation received earlier. Files travel only by artifact id — there is no path parameter."
                    }
                },
                // Nothing is unconditionally required: a message may be text,
                // files, or both, and the daemon refuses the empty case. A
                // schema demanding `text` would make an image reply
                // impossible to express, which is a contract the send path
                // has always accepted.
                "required": [],
                "additionalProperties": false
            }
        }
    })
}

/// The agent's peer tool, offered only when the operator has paired this
/// installation with at least one other as a peer.
///
/// Excluded from [`tool_definitions`] for the same reason
/// [`send_message_tool_def`] is: an installation with no peers has nowhere to
/// send anything. The destination is an alias the operator chose — there is no
/// parameter for an address, so the set of places this tool can reach is
/// exactly the set the operator already paired with.
pub fn peer_message_tool_def(aliases: &[String]) -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "peer_message",
            "description": format!(
                "Send a message or a task request to another Little Monkey installation the operator paired with. Available peers: {}. The peer decides what to do with it under its own permissions. Requires user permission.",
                aliases.join(", ")
            ),
            "parameters": {
                "type": "object",
                "properties": {
                    "peer": { "type": "string", "description": "The peer's alias.", "enum": aliases },
                    "text": { "type": "string", "description": "What to say or ask for." },
                    "thread": { "type": "string", "description": "An existing thread id to continue. Omit to start a new one." },
                    "task": { "type": "boolean", "description": "True to ask the peer to do work rather than just saying something." },
                    "correlation": { "type": "string", "description": "Your own handle for this request, returned with the peer's result so a later turn can match them up." },
                    // Ids from this installation's own content store, never
                    // paths: the tool hands the bytes over and the peer stores
                    // them itself, so nothing here can name a file on disk.
                    "artifacts": {
                        "type": "array",
                        "description": "Artifact ids from this run's own outputs to hand over. Requires the peer to have granted artifact exchange.",
                        "items": { "type": "string" }
                    }
                },
                "required": ["peer", "text"],
                "additionalProperties": false
            }
        }
    })
}

/// The agent's outbound call tool, offered only on a run whose telephony
/// account permits dialing out.
///
/// A phone call reaches a person who did not ask to be reached and bills the
/// operator, so this is the most tightly held tool in the set: the account is
/// named explicitly, the number must be in international format, and the
/// account's own outbound policy can refuse in a way no approval prompt
/// overrides. Excluded from [`tool_definitions`] for the same reason
/// [`send_message_tool_def`] is — a run with no telephony account has nothing
/// to dial with.
pub fn place_call_tool_def() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "place_call",
            "description": "Place a phone call from one of the operator's configured numbers. Calls cost money and reach a real person; only call this when the user has asked for it. Requires user permission.",
            "parameters": {
                "type": "object",
                "properties": {
                    "account_id": { "type": "string", "description": "The telephony account to call from." },
                    "to_number": { "type": "string", "description": "The number to call, in international format, e.g. +15551234567." },
                    "opening_line": { "type": "string", "description": "What to say as soon as the call connects — who is calling and why. A call cannot open with silence." }
                },
                "required": ["account_id", "to_number", "opening_line"],
                "additionalProperties": false
            }
        }
    })
}

/// The agent's read-only knowledge-stack retrieval tool (RAG design doc
/// slice 4, `monkey-cli` parity) — a Rust port of `src/lib/tools.ts`'s
/// `search_docs` `ToolDef`. Like [`present_plan_tool_def`] above,
/// deliberately excluded from [`tool_definitions`]'s base array (and from
/// [`merged_tool_definitions`]'s output): `agent.rs::run_tool_loop` only
/// appends it when at least one `--stack <name>` was given on the command
/// line, mirroring the desktop app's `buildTools(attachedStackNames)` (the
/// tool is only offered once a stack is actually attached). `stack_names`
/// is embedded directly into the description — same "the model sees exactly
/// what's searchable" property the GUI's per-turn tool list has.
pub fn search_docs_tool_def(stack_names: &[String]) -> serde_json::Value {
    let names = stack_names.join(", ");
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "search_docs",
            "description": format!(
                "Search the attached knowledge stack(s) ({names}) for passages relevant to a query. Returns the top matching chunks, each with its source file path and a relevance score. Cite source paths when you use a result."
            ),
            "parameters": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "The natural-language search query." },
                    "stack": { "type": "string", "description": "Optional stack name to restrict the search to; defaults to searching every attached stack." },
                    "max_results": { "type": "integer", "description": "Maximum number of results to return (default 6)." }
                },
                "required": ["query"],
                "additionalProperties": false
            }
        }
    })
}

/// The `device_action` tool — asks a paired physical device for one bounded
/// thing.
///
/// Like [`present_plan_tool_def`] and [`search_docs_tool_def`], deliberately
/// excluded from [`tool_definitions`]'s base array: it is appended only when
/// this machine actually has a paired device with an effective physical
/// capability. A model that is offered a camera it cannot reach will try to use
/// one, and the honest failure ("no paired device can do this") is a worse
/// answer than never having offered it.
///
/// `voice_stream` is absent on purpose. A continuous stream is not a discrete
/// command, and routing it through this tool would spend a grant meant for the
/// Talk surface.
pub fn device_action_tool_def() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "device_action",
            "description": "Ask a paired phone or tablet to do one bounded thing with its own hardware and return the result. Every action needs the operator's grant, the device's support, and the device's OS permission; anything else is refused with a reason. The device shows what it is doing. Requires user permission.",
            "parameters": {
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": [
                            "device_info",
                            "camera_capture",
                            "microphone_capture",
                            "location_read",
                            "notification_post",
                            "screen_capture",
                            "audio_playback"
                        ],
                        "description": "device_info reads the device's own platform and capabilities. camera_capture takes one still. microphone_capture records for a bounded time. location_read takes one fix (never continuous tracking). notification_post shows a notification. screen_capture captures the screen, and needs the device to have armed screen sharing first. audio_playback either plays a stored run artifact on the device or speaks text aloud."
                    },
                    "device_id": { "type": "string", "description": "Which paired device. Omit when exactly one device can perform this action; if several can, the call fails and lists them." },
                    "position": { "type": "string", "enum": ["front", "back"], "description": "camera_capture only. Defaults to back." },
                    "duration_ms": { "type": "integer", "description": "microphone_capture only: how long to record, 1-300000 ms. Defaults to 10000." },
                    "accuracy": { "type": "string", "enum": ["coarse", "precise"], "description": "location_read only. Defaults to coarse." },
                    "title": { "type": "string", "description": "notification_post only: up to 128 characters." },
                    "body": { "type": "string", "description": "notification_post only: up to 512 characters." },
                    "text": { "type": "string", "description": "audio_playback only: what to speak, up to 1024 characters. Use this or run_id + artifact_id, never both." },
                    "run_id": { "type": "string", "description": "audio_playback only: the run an audio artifact belongs to. The device fetches it over its own paired connection, so it also needs the read_artifacts grant." },
                    "artifact_id": { "type": "string", "description": "audio_playback only: which audio artifact of that run to play." },
                    "wait_ms": { "type": "integer", "description": "How long to wait for the device before returning, 1000-120000 ms (default 60000). A device that is asleep may answer later; the result then says the command is still queued or running rather than that it failed." }
                },
                "required": ["action"],
                "additionalProperties": false
            }
        }
    })
}

/// The `task` tool — delegates a scoped subtask to a subagent with its own
/// isolated tool-calling loop and explicit explore/code tool profiles — a Rust port of
/// `src/lib/tools.ts`'s `TASK_TOOL`. Like [`present_plan_tool_def`] and
/// [`search_docs_tool_def`], deliberately excluded from [`tool_definitions`]'s
/// base array (and from [`merged_tool_definitions`]'s output): `agent.rs`'s
/// `run_tool_loop` only appends it when `--subagents` was passed, mirroring
/// the desktop app's `subagentsEnabled` toggle (default off — a weak local
/// model that never had this turned on should never even see the schema).
/// `agent.rs::execute_tool_call` intercepts the name before the built-in
/// dispatch, exactly like `present_plan`; there is no `tool_task` Rust
/// command. Code-profile mutations reuse the parent turn checkpoint and the
/// same permission object; neither profile includes recursive delegation.
pub fn task_tool_def() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "task",
            "description": "Delegate a scoped subtask to a subagent with its own isolated tool-calling loop and restricted tool set. The subagent cannot see this conversation, so give it a fully self-contained prompt. Only its final report is returned to you — use this for broad exploration or an independent subtask you want kept out of your own context.",
            "parameters": {
                "type": "object",
                "properties": {
                    "description": { "type": "string", "description": "A short (3-6 word) label for this subtask, shown to the user." },
                    "prompt": { "type": "string", "description": "Full, self-contained instructions for the subagent — it has no access to this conversation, so include all necessary context (file paths, what to look for, what to report back)." },
                    "profile": {
                        "type": "string",
                        "enum": ["explore", "code"],
                        "description": "Tool access profile. 'explore' is read-only (read_file, list_dir, glob, grep). 'code' adds write_file, edit_file, and run_shell; mutations use the parent turn checkpoint and normal permission gate."
                    },
                    "isolation": {
                        "type": "string",
                        "enum": ["worktree"],
                        "description": "Optional, code profile only: run the subagent in a fresh git worktree of the workspace so parallel agents never collide on files. Its changes stay in the worktree for the user to apply or discard. Not supported by the CLI surface."
                    }
                },
                "required": ["description", "prompt", "profile"],
                "additionalProperties": false
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The peer tool's whole safety story is what it *cannot* be asked to do.
    ///
    /// A model reading a peer's own words is being read instructions by a
    /// stranger. It must not be possible to talk it into contacting somewhere
    /// new, authenticating as something else, or reaching a file — so there is
    /// no parameter for any of it, the alias is an enumeration of pairings the
    /// operator made, and nothing else may be passed at all.
    #[test]
    fn the_peer_tool_can_only_name_a_pairing_the_operator_already_made() {
        let definition = peer_message_tool_def(&["studio".to_string(), "server".to_string()]);
        let parameters = &definition["function"]["parameters"];
        let properties = parameters["properties"]
            .as_object()
            .expect("parameters are an object");

        let mut names: Vec<&str> = properties.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            ["artifacts", "correlation", "peer", "task", "text", "thread"],
            "the peer tool grew a parameter"
        );
        for forbidden in [
            "host",
            "url",
            "endpoint",
            "token",
            "secret",
            "certificate",
            "fingerprint",
            "path",
            "file",
            "workspace",
            "model",
            "tool",
            "permission",
            "permission_mode",
            "device",
            "phone",
            "number",
            "route",
        ] {
            assert!(
                !properties.contains_key(forbidden),
                "'{forbidden}' must not be a peer_message parameter"
            );
        }
        // Nothing may be smuggled past the schema either.
        assert_eq!(parameters["additionalProperties"], false);
        // And the destination is a pairing, not a string a model composes.
        assert_eq!(
            definition["function"]["parameters"]["properties"]["peer"]["enum"],
            serde_json::json!(["studio", "server"])
        );
        assert_eq!(
            parameters["required"],
            serde_json::json!(["peer", "text"]),
            "a peer message needs a destination and something to say, and nothing else"
        );
    }

    /// An installation with no peers is not offered the tool at all, so the
    /// alias enumeration can never be empty on a live definition.
    #[test]
    fn the_peer_tool_is_not_part_of_the_default_set() {
        let definitions = tool_definitions();
        let names: Vec<String> = definitions
            .as_array()
            .expect("the tool set is an array")
            .iter()
            .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_string))
            .collect();
        assert!(
            !names.contains(&"peer_message".to_string()),
            "peer_message is offered only when a peer exists"
        );
    }
}

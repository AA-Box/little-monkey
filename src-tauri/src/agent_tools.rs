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

/// The agent's reply-to-the-conversation tool, offered only on a run that
/// arrived from a messaging channel.
///
/// It takes a message and nothing else. There is deliberately no account,
/// conversation, thread or provider parameter: the destination is the origin
/// the daemon durably recorded for this run, so the model cannot redirect a
/// reply to a different conversation, a different account or a different
/// person — including when the message it is answering asks it to. The
/// transport is not the model's to choose.
///
/// Like [`present_plan_tool_def`], excluded from [`tool_definitions`]'s base
/// array: a run with no channel origin has nowhere to send anything, and
/// offering the tool there would only invite a failed call.
pub fn send_message_tool_def() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "send_message",
            "description": "Send a message back to the conversation this run came from. The destination is fixed to that conversation — you cannot choose the account, thread, or recipient. Requires user permission.",
            "parameters": {
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "The message to send." },
                    "attachments": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional files to send with the message, as paths inside this run's own directory. At most 4, each under 16 MB. Not every provider can carry files."
                    }
                },
                "required": ["text"],
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
                    "task": { "type": "boolean", "description": "True to ask the peer to do work rather than just saying something." }
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

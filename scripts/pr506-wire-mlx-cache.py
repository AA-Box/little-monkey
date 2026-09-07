#!/usr/bin/env python3
from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one match, found {count}: {old[:100]!r}")
    p.write_text(text.replace(old, new, 1))


def replace_count(path: str, old: str, new: str, expected: int) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != expected:
        raise SystemExit(
            f"{path}: expected exactly {expected} matches, found {count}: {old[:100]!r}"
        )
    p.write_text(text.replace(old, new))


# Python service: cached-token evidence is part of the strict terminal wire ABI.
replace_once(
    "packaging/mlx/service/mlx_server.py",
    '            self._emit({"type": "completed", "input_tokens": 0, "output_tokens": 0})',
    '            self._emit({"type": "completed", "input_tokens": 0, "output_tokens": 0, "cached_input_tokens": 0})',
)
replace_once(
    "packaging/mlx/service/mlx_server.py",
    '''        # Exactly one terminal event, on every path including the error one:\n        # the supervisor fails the whole request without it. Cache evidence is\n        # intentionally logged rather than added to this event until the Rust\n        # wire enum is versioned for the extra field.\n        self._emit(\n            {\n                "type": "completed",\n                "input_tokens": input_tokens,\n                "output_tokens": output_tokens,\n            }\n        )''',
    '''        # Exactly one terminal event, on every path including the error one.\n        # `cached_input_tokens` is measured by the MLX decode cache itself and\n        # crosses the supervised ABI so M3 can persist it as canonical usage.\n        self._emit(\n            {\n                "type": "completed",\n                "input_tokens": input_tokens,\n                "output_tokens": output_tokens,\n                "cached_input_tokens": self._cached_input_tokens,\n            }\n        )''',
)
replace_once(
    "packaging/mlx/service/mlx_server.py",
    '''        # `promptCacheKey` is optional and backwards-compatible with the current\n        # Rust request shape. The cache works without one; a future caller can\n        # supply a stable conversation key to prefer its own entry.''',
    '''        # `promptCacheKey` is supplied by the production Rust supervisor from a\n        # deterministic conversation-prefix digest. The cache still works without\n        # one for protocol compatibility, and token equality remains authoritative.''',
)

replace_once(
    "packaging/mlx/service/test_mlx_server.py",
    '    "completed": {"input_tokens", "output_tokens"},',
    '    "completed": {"input_tokens", "output_tokens", "cached_input_tokens"},',
)
replace_once(
    "packaging/mlx/service/test_mlx_server.py",
    '''    assert terminal[0]["output_tokens"] == 2\n    assert terminal[0]["input_tokens"] == 2, "input tokens come from the rendered prompt"''',
    '''    assert terminal[0]["output_tokens"] == 2\n    assert terminal[0]["input_tokens"] == 2, "input tokens come from the rendered prompt"\n    assert terminal[0]["cached_input_tokens"] == 0''',
)

# Rust private MLX wire and summary carry the runtime-reported cache measurement.
replace_once(
    "src-tauri/src/mlx_runtime.rs",
    '''    Completed {\n        input_tokens: u64,\n        output_tokens: u64,\n    },''',
    '''    Completed {\n        input_tokens: u64,\n        output_tokens: u64,\n        cached_input_tokens: u64,\n    },''',
)
replace_once(
    "src-tauri/src/mlx_runtime.rs",
    '''pub struct MlxGenerationSummary {\n    pub request_id: String,\n    pub input_tokens: u64,\n    pub output_tokens: u64,\n    pub finish_reason: String,\n}''',
    '''pub struct MlxGenerationSummary {\n    pub request_id: String,\n    pub input_tokens: u64,\n    pub output_tokens: u64,\n    /// Input tokens the runtime proved were resumed from its decode cache.\n    pub cached_input_tokens: u64,\n    pub finish_reason: String,\n}''',
)
replace_once(
    "src-tauri/src/mlx_runtime.rs",
    '''                    sink.emit(MlxStreamEvent::Completed {\n                        input_tokens: 5,\n                        output_tokens: 1,\n                    })''',
    '''                    sink.emit(MlxStreamEvent::Completed {\n                        input_tokens: 5,\n                        output_tokens: 1,\n                        cached_input_tokens: 3,\n                    })''',
)
replace_once(
    "src-tauri/src/mlx_runtime.rs",
    '''                    input_tokens: 5,\n                    output_tokens: 1,\n                    finish_reason: "stop".to_string(),''',
    '''                    input_tokens: 5,\n                    output_tokens: 1,\n                    cached_input_tokens: 3,\n                    finish_reason: "stop".to_string(),''',
)
replace_once(
    "src-tauri/src/mlx_runtime.rs",
    '''        assert_eq!(summary.output_tokens, 1);''',
    '''        assert_eq!(summary.output_tokens, 1);\n        assert_eq!(summary.cached_input_tokens, 3);''',
)

# Production supervisor injects a stable cache preference key. It remains only a
# preference: the Python side still requires strict token-prefix equality.
replace_once(
    "src-tauri/src/m3_production.rs",
    '''    fn controller_error(operation: &str, error: impl std::fmt::Display) -> MlxError {\n        MlxError::Controller {\n            operation: operation.to_string(),\n            message: error.to_string(),\n        }\n    }''',
    '''    fn controller_error(operation: &str, error: impl std::fmt::Display) -> MlxError {\n        MlxError::Controller {\n            operation: operation.to_string(),\n            message: error.to_string(),\n        }\n    }\n\n    fn prompt_cache_key(request: &MlxGenerationRequest) -> Result<String, MlxError> {\n        // First two turns are stable as a conversation grows. Hashing keeps the\n        // wire bounded and prevents user text from becoming an opaque cache id.\n        // The service still verifies exact token-prefix equality before reuse.\n        let stable = serde_json::to_vec(&(\n            request.model_id.as_str(),\n            request.messages.iter().take(2).collect::<Vec<_>>(),\n        ))\n        .map_err(|error| Self::controller_error("derive MLX prompt cache key", error))?;\n        Ok(format!("m3-{:x}", Sha256::digest(stable)))\n    }''',
)
replace_once(
    "src-tauri/src/m3_production.rs",
    '''                    response = crate::egress::send(self.client.post(format!("http://127.0.0.1:{}/v1/generate", handle.port)).json(request)) => {\n                        response.map_err(|error| Self::controller_error("start MLX stream", error))?\n                    }''',
    '''                    response = {\n                        let mut wire = serde_json::to_value(request)\n                            .map_err(|error| Self::controller_error("encode MLX request", error))?;\n                        let object = wire.as_object_mut().ok_or_else(||\n                            Self::controller_error("encode MLX request", "request did not encode as an object")\n                        )?;\n                        object.insert(\n                            "promptCacheKey".to_string(),\n                            Value::String(Self::prompt_cache_key(request)?),\n                        );\n                        crate::egress::send(\n                            self.client\n                                .post(format!("http://127.0.0.1:{}/v1/generate", handle.port))\n                                .json(&wire),\n                        )\n                    } => {\n                        response.map_err(|error| Self::controller_error("start MLX stream", error))?\n                    }''',
)
replace_once(
    "src-tauri/src/m3_production.rs",
    '''                let (input_tokens, output_tokens) = completed.ok_or_else(|| {''',
    '''                let (input_tokens, output_tokens, cached_input_tokens) = completed.ok_or_else(|| {''',
)
replace_once(
    "src-tauri/src/m3_production.rs",
    '''                    input_tokens,\n                    output_tokens,\n                    finish_reason: if used_tool { "tool_use" } else { "stop" }.to_string(),''',
    '''                    input_tokens,\n                    output_tokens,\n                    cached_input_tokens,\n                    finish_reason: if used_tool { "tool_use" } else { "stop" }.to_string(),''',
)
replace_once(
    "src-tauri/src/m3_production.rs",
    '''    completed: &mut Option<(u64, u64)>,''',
    '''    completed: &mut Option<(u64, u64, u64)>,''',
)
replace_once(
    "src-tauri/src/m3_production.rs",
    '''    if let MlxStreamEvent::Completed {\n        input_tokens,\n        output_tokens,\n    } = &event\n    {\n        *completed = Some((*input_tokens, *output_tokens));\n    }''',
    '''    if let MlxStreamEvent::Completed {\n        input_tokens,\n        output_tokens,\n        cached_input_tokens,\n    } = &event\n    {\n        *completed = Some((*input_tokens, *output_tokens, *cached_input_tokens));\n    }''',
)

# Canonical streaming must retain the exact measurement too; otherwise a caller
# using the streamed path would silently lose the cache evidence even though the
# generation summary had it.
replace_once(
    "src-tauri/src/m3_runtime_hub.rs",
    '''            MlxStreamEvent::Completed {\n                input_tokens,\n                output_tokens,\n            } => {''',
    '''            MlxStreamEvent::Completed {\n                input_tokens,\n                output_tokens,\n                cached_input_tokens,\n            } => {''',
)
replace_once(
    "src-tauri/src/m3_runtime_hub.rs",
    '''                        usage: CanonicalUsage {\n                            input_tokens,\n                            output_tokens,\n                            cached_input_tokens: None,\n                        },''',
    '''                        usage: CanonicalUsage {\n                            input_tokens,\n                            output_tokens,\n                            cached_input_tokens: Some(cached_input_tokens),\n                        },''',
)

# The non-streaming driver completion is based on MlxGenerationSummary and must
# persist the same measured value into the run ledger/telemetry path.
replace_once(
    "src-tauri/src/m3_runtime_hub.rs",
    '''                    // MLX reports no prompt-cache reuse, so `None` — see\n                    // `CanonicalUsage::cached_input_tokens` on why not zero.\n                    cached_input_tokens: None,''',
    '''                    cached_input_tokens: Some(summary.cached_input_tokens),''',
)

# Existing full MLX canonical-pipeline tests now satisfy the expanded ABI. One
# uses a nonzero value and asserts it at the final canonical response boundary.
replace_once(
    "src-tauri/src/m3_runtime_hub.rs",
    '''        events.push(MlxStreamEvent::Completed {\n            input_tokens: 3,\n            output_tokens: 5,\n        });''',
    '''        events.push(MlxStreamEvent::Completed {\n            input_tokens: 3,\n            output_tokens: 5,\n            cached_input_tokens: 2,\n        });''',
)
replace_count(
    "src-tauri/src/m3_runtime_hub.rs",
    '''                MlxStreamEvent::Completed {\n                    input_tokens: 1,\n                    output_tokens: 1,\n                },''',
    '''                MlxStreamEvent::Completed {\n                    input_tokens: 1,\n                    output_tokens: 1,\n                    cached_input_tokens: 0,\n                },''',
    2,
)
replace_once(
    "src-tauri/src/m3_runtime_hub.rs",
    '''        assert_eq!(response.finish_reason, "tool_use");''',
    '''        assert_eq!(response.finish_reason, "tool_use");\n        assert_eq!(response.usage.cached_input_tokens, Some(2));''',
)

# Remove now-stale documentation language.
replace_once(
    "src-tauri/src/compatibility_hub.rs",
    '''    /// `None` is the whole point of the `Option`: a runtime that never reports\n    /// prompt-cache reuse (Ollama and MLX today) must not be recorded as''',
    '''    /// `None` is the whole point of the `Option`: a runtime that never reports\n    /// prompt-cache reuse (Ollama today) must not be recorded as''',
)

print("PR506 MLX cache ABI wiring applied")

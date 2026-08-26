from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def replace_once(path: str, old: str, new: str) -> None:
    p = ROOT / path
    text = p.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one match, found {count}: {old[:100]!r}")
    p.write_text(text.replace(old, new, 1), encoding="utf-8")


def replace_all(path: str, old: str, new: str, minimum: int = 1) -> None:
    p = ROOT / path
    text = p.read_text(encoding="utf-8")
    count = text.count(old)
    if count < minimum:
        raise SystemExit(f"{path}: expected at least {minimum} matches, found {count}: {old[:100]!r}")
    p.write_text(text.replace(old, new), encoding="utf-8")


# Compile the Whisper engine into the application and keep browser audio decoding
# in-process. The fork is commit-pinned because it contains the Windows ARM64
# build fixes that the archived upstream release does not.
replace_once(
    "src-tauri/Cargo.toml",
    'tokio-util = "0.7"\n',
    'tokio-util = "0.7"\n'
    '# Built-in, zero-configuration local speech transcription. The pinned fork\n'
    '# carries the Windows ARM64 MSVC/Clang fixes missing from archived upstream.\n'
    'whisper-rs = { git = "https://github.com/screenpipe/whisper-rs", rev = "97e40705033724dee601e89f3bd701af8096c846", default-features = false }\n'
    '# Symphonia demuxes/decodes WAV/MP3/AAC/FLAC/etc. WebM/Opus is demuxed by\n'
    '# Symphonia and decoded by opus-decoder because Symphonia intentionally has\n'
    '# no Opus decoder. Both are pure Rust and ship inside the application.\n'
    'symphonia = { version = "0.6.1", features = ["all"] }\n'
    'opus-decoder = "0.1.1"\n',
)

# Wire the built-in backend into the crate.
replace_once(
    "src-tauri/src/lib.rs",
    "pub mod dictation;\npub mod m7_companion;\n",
    "pub mod dictation;\npub mod local_whisper;\npub mod m7_companion;\n",
)

# Provision in the background on every GUI launch. A failed startup download is
# deliberately non-fatal: the first transcription retries the same verified path.
replace_once(
    "src-tauri/src/lib.rs",
    "            // K22 startup self-integrity check. Runs *after* materialization on\n",
    "            // Prepare the built-in local speech model without blocking the UI.\n"
    "            // This is an optimization, not a gate: every local transcription calls\n"
    "            // prepare() again, so a first-launch network failure automatically retries\n"
    "            // when the user actually speaks. No path selection or external runtime is\n"
    "            // required on macOS, Windows, or Linux.\n"
    "            {\n"
    "                let speech_data_dir = app_data_dir.clone();\n"
    "                tauri::async_runtime::spawn(async move {\n"
    "                    if let Err(error) = local_whisper::prepare(&speech_data_dir).await {\n"
    "                        eprintln!(\"Built-in local speech model setup failed; it will retry on use: {error}\");\n"
    "                    }\n"
    "                });\n"
    "            }\n\n"
    "            // K22 startup self-integrity check. Runs *after* materialization on\n",
)

# Legacy path fields remain serializable for backwards compatibility, but no
# longer have authority over the built-in local backend. In particular, stale
# paths from older installs must not make application startup fail.
replace_once(
    "src-tauri/src/m7_companion.rs",
    "    if let Some(binary) = &config.voice.whisper_binary {\n"
    "        validate_absolute_regular(binary, true)?;\n"
    "    }\n"
    "    if let Some(model) = &config.voice.whisper_model {\n"
    "        validate_absolute_regular(model, false)?;\n"
    "    }\n",
    "",
)

replace_once(
    "src-tauri/src/m7_companion.rs",
    "        TranscriptionBackendKind::LocalWhisper => {\n"
    "            voice.whisper_binary.is_some() && voice.whisper_model.is_some()\n"
    "        }\n",
    "        // Local Whisper is part of the application. The model is provisioned\n"
    "        // automatically in the background and lazily retried on first use, so\n"
    "        // no user-supplied executable/model paths are a configuration gate.\n"
    "        TranscriptionBackendKind::LocalWhisper => true,\n",
)

old_local_branch = '''        TranscriptionBackendKind::LocalWhisper => {
            let binary = validate_absolute_regular(
                config
                    .whisper_binary
                    .as_deref()
                    .ok_or("Configure a local whisper.cpp binary first")?,
                true,
            )?;
            let model = validate_absolute_regular(
                config
                    .whisper_model
                    .as_deref()
                    .ok_or("Configure a local whisper model first")?,
                false,
            )?;
            let prefix = state.root.join("tmp").join(format!("transcript-{job_id}"));
            let mut command = tokio::process::Command::new(binary);
            command
                .arg("-m")
                .arg(model)
                .arg("-f")
                .arg(path)
                .arg("-oj")
                .arg("-of")
                .arg(&prefix)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            if config.language != "auto" {
                command.arg("-l").arg(&config.language);
            }
            let mut child = command
                .spawn()
                .map_err(|error| format!("Start whisper.cpp: {error}"))?;
            let status = tokio::select! {
                _ = cancellation.cancelled() => {
                    let _ = child.kill().await;
                    return Err("Transcription cancelled".to_string());
                }
                status = tokio::time::timeout(Duration::from_secs(60 * 60), child.wait()) => {
                    status.map_err(|_| "Transcription exceeded one hour".to_string())?
                        .map_err(|error| format!("Wait for whisper.cpp: {error}"))?
                }
            };
            if !status.success() {
                return Err(format!("whisper.cpp exited with {status}"));
            }
            let json_path = prefix.with_extension("json");
            let bytes = fs::read(&json_path)
                .map_err(|error| format!("Read whisper.cpp transcript: {error}"))?;
            let _ = fs::remove_file(json_path);
            if bytes.len() > MAX_TRANSCRIPT_BYTES {
                return Err("Transcript exceeds its byte limit".to_string());
            }
            let value: Value = serde_json::from_slice(&bytes)
                .map_err(|error| format!("Decode whisper.cpp transcript: {error}"))?;
            let text = extract_transcript(&value);
            if text.is_empty() {
                return Err("whisper.cpp returned an empty transcript".to_string());
            }
            let segments = extract_speaker_segments(&value);
            Ok((text, "local_whisper".to_string(), segments))
        }
'''
new_local_branch = '''        TranscriptionBackendKind::LocalWhisper => {
            let transcript = crate::local_whisper::transcribe(
                &state.app_data_dir,
                path,
                &config.language,
                cancellation.clone(),
            )
            .await?;
            if transcript.text.len() > MAX_TRANSCRIPT_BYTES {
                return Err("Transcript exceeds its byte limit".to_string());
            }
            let segments = transcript
                .segments
                .into_iter()
                .map(|segment| SpeakerSegment {
                    speaker: "Unknown speaker".to_string(),
                    start_ms: segment.start_ms,
                    end_ms: segment.end_ms,
                    text: segment.text,
                    confidence: None,
                })
                .collect();
            Ok((transcript.text, "local_whisper".to_string(), segments))
        }
'''
replace_once("src-tauri/src/m7_companion.rs", old_local_branch, new_local_branch)

old_readiness = '''        TranscriptionBackendKind::LocalWhisper => {
            if voice
                .whisper_binary
                .as_deref()
                .unwrap_or_default()
                .is_empty()
                || voice
                    .whisper_model
                    .as_deref()
                    .unwrap_or_default()
                    .is_empty()
            {
                return Err(
                    "Local transcription has no whisper binary or model configured, so nothing said on a call can be understood."
                        .to_string(),
                );
            }
        }
'''
replace_once(
    "src-tauri/src/m7_companion.rs",
    old_readiness,
    "        // The local engine ships with the app and provisions its verified model\n"
    "        // automatically. Readiness is no longer a user-configuration question.\n"
    "        TranscriptionBackendKind::LocalWhisper => {}\n",
)

# Tighten and pin the automatic model source, and import the audio-buffer trait
# required by Symphonia's interleaving helpers.
replace_once(
    "src-tauri/src/local_whisper.rs",
    "use futures_util::StreamExt;\n",
    "use futures_util::StreamExt;\nuse symphonia::core::audio::Audio;\n",
)
replace_once(
    "src-tauri/src/local_whisper.rs",
    '    "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base-q5_1.bin";\n',
    '    "https://huggingface.co/ggerganov/whisper.cpp/resolve/f281eb45af861ab5e5297d23694b7d46e090c02c/ggml-base-q5_1.bin";\n',
)

# Settings: local speech is a built-in backend, not a pair of filesystem fields.
choose_callback = '''  const chooseWhisperPath = useCallback(async (kind: "binary" | "model") => {
    const selected = await open({ multiple: false, directory: false });
    if (!selected || Array.isArray(selected) || !config) return;
    setConfig({
      ...config,
      voice: {
        ...config.voice,
        whisperBinary: kind === "binary" ? selected : config.voice.whisperBinary,
        whisperModel: kind === "model" ? selected : config.voice.whisperModel,
      },
    });
  }, [config]);

'''
replace_once("src/components/Settings/CompanionPanel.tsx", choose_callback, "")
replace_once(
    "src/components/Settings/CompanionPanel.tsx",
    '<option value="local_whisper">Local whisper.cpp</option>',
    '<option value="local_whisper">Built-in local Whisper</option>',
)
old_local_ui = '''          {config.voice.backend === "local_whisper" ? <>
            <label className="text-xs text-muted">whisper.cpp binary
              <div className="mt-1 flex gap-2"><input className={INPUT} readOnly value={config.voice.whisperBinary ?? ""} /><Button size="sm" onClick={() => void chooseWhisperPath("binary")}>Choose</Button></div>
            </label>
            <label className="text-xs text-muted">Whisper model
              <div className="mt-1 flex gap-2"><input className={INPUT} readOnly value={config.voice.whisperModel ?? ""} /><Button size="sm" onClick={() => void chooseWhisperPath("model")}>Choose</Button></div>
            </label>
          </> : config.voice.backend === "provider" ? <>
'''
new_local_ui = '''          {config.voice.backend === "local_whisper" ? (
            <div className="rounded-md border border-border bg-background p-3 text-xs text-muted md:col-span-2">
              Local transcription is built in. Little Monkey installs and verifies its multilingual Whisper model automatically; there is no binary or model path to configure.
            </div>
          ) : config.voice.backend === "provider" ? <>
'''
replace_once("src/components/Settings/CompanionPanel.tsx", old_local_ui, new_local_ui)

# Preserve wire compatibility but make the legacy nature explicit to frontend
# callers so new code does not accidentally bring the manual requirement back.
replace_once(
    "src/lib/companionClient.ts",
    "  whisperBinary: string | null;\n  whisperModel: string | null;\n",
    "  /** @deprecated Kept only for compatibility with older persisted configs; built-in local Whisper ignores it. */\n"
    "  whisperBinary: string | null;\n"
    "  /** @deprecated Kept only for compatibility with older persisted configs; the model is app-managed. */\n"
    "  whisperModel: string | null;\n",
)

# Tests pin the new contract: a default local config has no external paths and
# the settings surface never asks the user to supply them.
replace_all(
    "src/components/Talk/TalkPanel.test.tsx",
    "    whisperBinary: '/usr/local/bin/whisper',\n    whisperModel: '/models/base.bin',\n",
    "    whisperBinary: null,\n    whisperModel: null,\n",
)

settings_test = ROOT / "src/components/Settings/CompanionPanel.test.tsx"
text = settings_test.read_text(encoding="utf-8")
needle = '''  it("discovers healthy STT capabilities and saves the selected typed backend", async () => {
'''
if needle not in text:
    raise SystemExit("CompanionPanel.test.tsx: insertion point not found")
new_test = '''  it("uses built-in local Whisper without asking for binary or model paths", async () => {
    render(<CompanionPanel />);

    await screen.findByText("Voice and transcription");
    fireEvent.change(screen.getByLabelText("Backend"), { target: { value: "local_whisper" } });

    expect(screen.queryByLabelText("whisper.cpp binary")).toBeNull();
    expect(screen.queryByLabelText("Whisper model")).toBeNull();
    expect(screen.getByText(/installs and verifies its multilingual Whisper model automatically/i)).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Save voice settings" }));
    await waitFor(() => expect(invoke).toHaveBeenCalledWith("m7_config_save", {
      config: {
        ...CONFIG,
        voice: { ...CONFIG.voice, backend: "local_whisper" },
      },
    }));
  });

'''
settings_test.write_text(text.replace(needle, new_test + needle, 1), encoding="utf-8")

# The local module's tests include an opt-in real acceptance path. CI supplies a
# known WAV and WebM/Opus rendition; both must traverse the same automatic model
# provisioning and production transcription implementation.
local = ROOT / "src-tauri/src/local_whisper.rs"
text = local.read_text(encoding="utf-8")
end = "}\n"
pos = text.rfind(end)
if pos < 0:
    raise SystemExit("local_whisper.rs: tests module end not found")
e2e = r'''

    #[tokio::test]
    async fn e2e_auto_provisions_and_transcribes_real_audio() {
        let wav = match std::env::var_os("LITTLE_MONKEY_LOCAL_WHISPER_E2E_WAV") {
            Some(path) => PathBuf::from(path),
            None => return,
        };
        let webm = PathBuf::from(
            std::env::var_os("LITTLE_MONKEY_LOCAL_WHISPER_E2E_WEBM")
                .expect("WebM fixture must accompany the WAV fixture"),
        );
        let root = std::env::temp_dir().join(format!(
            "little-monkey-whisper-e2e-{}",
            uuid::Uuid::new_v4().simple()
        ));
        tokio::fs::create_dir_all(&root).await.unwrap();

        let prepared = prepare(&root).await.expect("model auto-provisions");
        assert!(prepared.is_file());
        assert_eq!(sha256_file(&prepared).await.unwrap(), MODEL_SHA256);

        for fixture in [&wav, &webm] {
            let transcript = transcribe(
                &root,
                fixture,
                "en",
                CancellationToken::new(),
            )
            .await
            .unwrap_or_else(|error| panic!("{} failed: {error}", fixture.display()));
            let normalized = transcript.text.to_ascii_lowercase();
            assert!(
                normalized.contains("country") || normalized.contains("ask not"),
                "unexpected transcript for {}: {}",
                fixture.display(),
                transcript.text
            );
        }
        let _ = tokio::fs::remove_dir_all(root).await;
    }
'''
local.write_text(text[:pos] + e2e + text[pos:], encoding="utf-8")

print("zero-config Whisper integration patch applied")

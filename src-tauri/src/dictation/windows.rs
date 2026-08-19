use std::mem::MaybeUninit;
use std::sync::{mpsc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows::core::{Interface, PCWSTR, PWSTR};
use windows::Win32::Globalization::{
    LCIDToLocaleName, LocaleNameToLCID, LOCALE_ALLOW_NEUTRAL_NAMES,
};
use windows::Win32::Media::Speech::*;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};

use super::{DictationCapabilities, NativeCallback, NativeEvent};

const STOP_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

enum Control {
    Stop,
    Cancel,
}

pub struct Session {
    control: Mutex<Option<mpsc::Sender<Control>>>,
    join: Mutex<Option<JoinHandle<()>>>,
}

fn event_mask(event: SPEVENTENUM) -> u64 {
    1u64 << (event.0 as u32)
}

pub fn capabilities() -> DictationCapabilities {
    let (supported, languages) = thread::spawn(|| unsafe {
        let initialized = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
        let result = CoCreateInstance::<_, ISpRecognizer>(&SpSharedRecognizer, None, CLSCTX_ALL)
            .and_then(|recognizer| {
                let languages = installed_languages();
                recognizer.CreateRecoContext().map(|_| languages)
            });
        if initialized {
            CoUninitialize();
        }
        match result {
            Ok(languages) => (true, languages),
            Err(_) => (false, Vec::new()),
        }
    })
    .join()
    .unwrap_or((false, Vec::new()));
    DictationCapabilities {
        supported,
        platform: "windows".to_string(),
        engine: "Windows SAPI".to_string(),
        supports_partial_results: true,
        supports_on_device: false,
        languages,
    }
}

pub fn start(
    session_id: String,
    language: Option<String>,
    callback: NativeCallback,
) -> Result<Session, String> {
    let (tx, rx) = mpsc::channel();
    let worker_session_id = session_id.clone();
    let join = thread::Builder::new()
        .name("little-monkey-sapi-dictation".to_string())
        .spawn(move || run_worker(worker_session_id, language, rx, callback))
        .map_err(|error| format!("Windows speech recognition could not start: {error}"))?;
    Ok(Session {
        control: Mutex::new(Some(tx)),
        join: Mutex::new(Some(join)),
    })
}

impl Session {
    pub fn stop(&self) -> Result<(), String> {
        if let Some(sender) = self
            .control
            .lock()
            .map_err(|_| "Windows dictation state is unavailable".to_string())?
            .take()
        {
            sender
                .send(Control::Stop)
                .map_err(|_| "Windows speech recognition stopped unexpectedly".to_string())?;
        }
        Ok(())
    }

    pub fn cancel(&self) -> Result<(), String> {
        if let Some(sender) = self
            .control
            .lock()
            .map_err(|_| "Windows dictation state is unavailable".to_string())?
            .take()
        {
            sender
                .send(Control::Cancel)
                .map_err(|_| "Windows speech recognition stopped unexpectedly".to_string())?;
        }
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Ok(mut control) = self.control.lock() {
            if let Some(sender) = control.take() {
                let _ = sender.send(Control::Cancel);
            }
        }
        if let Ok(mut join) = self.join.lock() {
            if let Some(handle) = join.take() {
                let _ = handle.join();
            }
        }
    }
}

fn run_worker(
    session_id: String,
    language: Option<String>,
    receiver: mpsc::Receiver<Control>,
    callback: NativeCallback,
) {
    callback(NativeEvent::State {
        session_id: session_id.clone(),
        state: "starting".to_string(),
    });
    let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).is_ok() };
    let result = unsafe { run_sapi(&session_id, language.as_deref(), &receiver, &callback) };
    if let Err(message) = result {
        callback(NativeEvent::Error {
            session_id: session_id.clone(),
            code: "sapi_unavailable".to_string(),
            message,
        });
        callback(NativeEvent::State {
            session_id,
            state: "idle".to_string(),
        });
    }
    if initialized {
        unsafe {
            CoUninitialize();
        }
    }
}

unsafe fn run_sapi(
    session_id: &str,
    language: Option<&str>,
    receiver: &mpsc::Receiver<Control>,
    callback: &NativeCallback,
) -> Result<(), String> {
    let recognizer: ISpRecognizer = CoCreateInstance(&SpSharedRecognizer, None, CLSCTX_ALL)
        .map_err(|error| format!("Windows speech recognition is not installed: {error}"))?;
    if let Some(language) = language.filter(|value| !value.trim().is_empty()) {
        let token = find_recognizer_for_language(language)?;
        recognizer.SetRecognizer(&token).map_err(|error| {
            format!("Windows speech recognizer could not select {language}: {error}")
        })?;
    }
    let context = recognizer
        .CreateRecoContext()
        .map_err(|error| format!("Windows speech recognition context failed: {error}"))?;
    let interest =
        event_mask(SPEI_HYPOTHESIS) | event_mask(SPEI_RECOGNITION) | event_mask(SPEI_END_SR_STREAM);
    context
        .SetInterest(interest, interest)
        .map_err(|error| format!("Windows speech event subscription failed: {error}"))?;
    context
        .SetNotifyWin32Event()
        .map_err(|error| format!("Windows speech notification setup failed: {error}"))?;
    let grammar = context
        .CreateGrammar(0)
        .map_err(|error| format!("Windows dictation grammar could not be created: {error}"))?;
    grammar
        .LoadDictation(None, SPLO_STATIC)
        .map_err(|error| format!("Windows dictation grammar is unavailable: {error}"))?;
    grammar
        .SetDictationState(SPRS_ACTIVE)
        .map_err(|error| format!("Windows dictation grammar could not start: {error}"))?;
    callback(NativeEvent::State {
        session_id: session_id.to_string(),
        state: "listening".to_string(),
    });

    let mut events = [SPEVENT::default(); 32];
    let mut has_unfinalized_hypothesis = false;
    loop {
        match receiver.try_recv() {
            Ok(Control::Stop) => {
                callback(NativeEvent::State {
                    session_id: session_id.to_string(),
                    state: "stopping".to_string(),
                });
                let _ = grammar.SetDictationState(SPRS_INACTIVE);
                let _ = drain_after_stop(
                    session_id,
                    &context,
                    receiver,
                    callback,
                    &mut events,
                    &mut has_unfinalized_hypothesis,
                )?;
                let _ = context.SetContextState(SPCS_DISABLED);
                callback(NativeEvent::State {
                    session_id: session_id.to_string(),
                    state: "idle".to_string(),
                });
                return Ok(());
            }
            Ok(Control::Cancel) => {
                let _ = grammar.SetDictationState(SPRS_INACTIVE);
                let _ = context.SetContextState(SPCS_DISABLED);
                callback(NativeEvent::State {
                    session_id: session_id.to_string(),
                    state: "idle".to_string(),
                });
                return Ok(());
            }
            Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
            Err(mpsc::TryRecvError::Empty) => {}
        }

        let _ = context.WaitForNotifyEvent(100);
        if process_sapi_events(
            session_id,
            &context,
            callback,
            &mut events,
            &mut has_unfinalized_hypothesis,
        )?
        .reached_end
        {
            if has_unfinalized_hypothesis {
                return Err("Windows speech recognition ended without a final result".to_string());
            }
            callback(NativeEvent::State {
                session_id: session_id.to_string(),
                state: "idle".to_string(),
            });
            return Ok(());
        }
    }
}

unsafe fn installed_languages() -> Vec<DictationLanguage> {
    let Ok(category) =
        CoCreateInstance::<_, ISpObjectTokenCategory>(&SpObjectTokenCategory, None, CLSCTX_ALL)
    else {
        return Vec::new();
    };
    let Ok(tokens) = category.EnumTokens(None, None) else {
        return Vec::new();
    };
    let mut count = 0;
    if tokens.GetCount(&mut count).is_err() {
        return Vec::new();
    }
    let mut languages = Vec::new();
    for index in 0..count {
        let Ok(token) = tokens.Item(index) else {
            continue;
        };
        let Ok(raw_language) = token.GetStringValue(windows::core::w!("Language")) else {
            continue;
        };
        let language_codes = pwstr_to_string(raw_language);
        for language_code in language_codes.split(';') {
            let Some(locale) = lcid_to_locale(language_code) else {
                continue;
            };
            if languages
                .iter()
                .any(|entry: &DictationLanguage| entry.id == locale)
            {
                continue;
            }
            languages.push(DictationLanguage {
                label: locale.clone(),
                id: locale,
            });
        }
    }
    languages
}

unsafe fn find_recognizer_for_language(language: &str) -> Result<ISpObjectToken, String> {
    let wanted_lcid = locale_to_lcid(language)
        .ok_or_else(|| format!("Windows speech language is invalid: {language}"))?;
    let category =
        CoCreateInstance::<_, ISpObjectTokenCategory>(&SpObjectTokenCategory, None, CLSCTX_ALL)
            .map_err(|error| {
                format!("Windows speech recognizer category is unavailable: {error}")
            })?;
    let tokens = category
        .EnumTokens(None, None)
        .map_err(|error| format!("Windows speech recognizers could not be enumerated: {error}"))?;
    let mut count = 0;
    tokens
        .GetCount(&mut count)
        .map_err(|error| format!("Windows speech recognizer count failed: {error}"))?;
    for index in 0..count {
        let token = tokens
            .Item(index)
            .map_err(|error| format!("Windows speech recognizer token lookup failed: {error}"))?;
        let Ok(raw_language) = token.GetStringValue(windows::core::w!("Language")) else {
            continue;
        };
        let language_codes = pwstr_to_string(raw_language);
        if language_codes
            .split(';')
            .filter_map(parse_lcid)
            .any(|lcid| lcid == wanted_lcid)
        {
            return Ok(token);
        }
    }
    Err(format!(
        "Windows speech recognizer does not provide language {language}"
    ))
}

unsafe fn pwstr_to_string(value: PWSTR) -> String {
    if value.0.is_null() {
        return String::new();
    }
    let mut length = 0usize;
    while *value.0.add(length) != 0 {
        length += 1;
    }
    let text = String::from_utf16_lossy(std::slice::from_raw_parts(value.0, length));
    CoTaskMemFree(Some(value.0.cast()));
    text
}

fn parse_lcid(value: &str) -> Option<u32> {
    let value = value.trim().trim_start_matches("0x");
    u32::from_str_radix(value, 16).ok()
}

unsafe fn lcid_to_locale(value: &str) -> Option<String> {
    let lcid = parse_lcid(value)?;
    let mut buffer = [0u16; 85];
    let length = LCIDToLocaleName(lcid, Some(&mut buffer), 0);
    if length <= 1 {
        return None;
    }
    String::from_utf16(&buffer[..(length - 1) as usize]).ok()
}

fn locale_to_lcid(value: &str) -> Option<u32> {
    if let Some(lcid) = parse_lcid(value) {
        return Some(lcid);
    }
    let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
    let lcid = unsafe { LocaleNameToLCID(PCWSTR(wide.as_ptr()), LOCALE_ALLOW_NEUTRAL_NAMES) };
    (lcid != 0).then_some(lcid)
}

/// Stop input, then drain SAPI's queued recognition results until the engine
/// reports end-of-stream. A hypothesis is never promoted to final text: if
/// SAPI cannot produce a real recognition before the bounded safety deadline,
/// the caller receives an error instead of clipped text.
fn drain_after_stop(
    session_id: &str,
    context: &ISpRecoContext,
    receiver: &mpsc::Receiver<Control>,
    callback: &NativeCallback,
    events: &mut [SPEVENT],
    has_unfinalized_hypothesis: &mut bool,
) -> Result<bool, String> {
    let deadline = Instant::now() + STOP_DRAIN_TIMEOUT;
    loop {
        if matches!(receiver.try_recv(), Ok(Control::Cancel)) {
            return Ok(false);
        }
        let _ = context.WaitForNotifyEvent(100);
        let batch = process_sapi_events(
            session_id,
            context,
            callback,
            events,
            has_unfinalized_hypothesis,
        )?;
        if batch.saw_final {
            return Ok(true);
        }
        if batch.reached_end {
            if *has_unfinalized_hypothesis {
                return Err("Windows speech recognition ended without a final result".to_string());
            }
            return Ok(false);
        }
        if Instant::now() >= deadline {
            return Err(
                "Windows speech recognition did not deliver a final result while stopping"
                    .to_string(),
            );
        }
    }
}

#[derive(Default)]
struct SapiEventBatch {
    reached_end: bool,
    saw_final: bool,
}

/// Process and release one batch of queued SAPI events. The `lParam` for
/// recognition and hypothesis events is an owned `ISpRecoResult` reference;
/// `from_raw` takes that reference and its RAII drop performs the required
/// COM Release after the text has been read.
unsafe fn process_sapi_events(
    session_id: &str,
    context: &ISpRecoContext,
    callback: &NativeCallback,
    events: &mut [SPEVENT],
    has_unfinalized_hypothesis: &mut bool,
) -> Result<SapiEventBatch, String> {
    let mut fetched = 0;
    context
        .GetEvents(events.len() as u32, events.as_mut_ptr(), &mut fetched)
        .map_err(|error| format!("Windows speech event retrieval failed: {error}"))?;
    let mut batch = SapiEventBatch::default();
    for event in events.iter().take(fetched as usize) {
        let event_id = event._bitfield & 0xffff;
        if event_id == SPEI_END_SR_STREAM.0 {
            batch.reached_end = true;
            continue;
        }
        if event_id != SPEI_HYPOTHESIS.0 && event_id != SPEI_RECOGNITION.0 {
            continue;
        }
        let raw_result = event.lParam.0 as *mut std::ffi::c_void;
        if raw_result.is_null() {
            continue;
        }
        let result = ISpRecoResult::from_raw(raw_result);
        let text = phrase_text(&result).unwrap_or_default();
        if text.is_empty() {
            continue;
        }
        if event_id == SPEI_RECOGNITION.0 {
            batch.saw_final = true;
            *has_unfinalized_hypothesis = false;
            callback(NativeEvent::Final {
                session_id: session_id.to_string(),
                text,
            });
        } else {
            *has_unfinalized_hypothesis = true;
            callback(NativeEvent::Partial {
                session_id: session_id.to_string(),
                text,
            });
        }
    }
    Ok(batch)
}

unsafe fn phrase_text(result: &ISpRecoResult) -> Result<String, String> {
    let mut raw_text = MaybeUninit::<PWSTR>::zeroed();
    result
        .GetText(0, u32::MAX, true, raw_text.as_mut_ptr(), None)
        .map_err(|error| format!("Windows speech result could not be read: {error}"))?;
    let raw_text = raw_text.assume_init();
    if raw_text.0.is_null() {
        return Ok(String::new());
    }
    let mut length = 0usize;
    while *raw_text.0.add(length) != 0 {
        length += 1;
    }
    let text = String::from_utf16_lossy(std::slice::from_raw_parts(raw_text.0, length));
    CoTaskMemFree(Some(raw_text.0.cast()));
    Ok(text.trim().to_string())
}

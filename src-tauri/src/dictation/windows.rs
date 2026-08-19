use std::mem::MaybeUninit;
use std::ptr::NonNull;
use std::sync::{mpsc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows::core::Interface;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Media::Speech::*;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};

use super::{DictationCapabilities, NativeCallback, NativeEvent};

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
    let supported = thread::spawn(|| unsafe {
        let initialized = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
        let result = CoCreateInstance::<_, ISpRecognizer>(&SpSharedRecognizer, None, CLSCTX_ALL)
            .and_then(|recognizer| recognizer.CreateRecoContext())
            .is_ok();
        if initialized {
            CoUninitialize();
        }
        result
    })
    .join()
    .unwrap_or(false);
    DictationCapabilities {
        supported,
        platform: "windows".to_string(),
        engine: "Windows SAPI".to_string(),
        supports_partial_results: true,
        supports_on_device: false,
        // SAPI uses the installed recognizer/profile. It does not expose a
        // stable language inventory through the dictation context; an empty
        // list correctly leaves Settings on system default.
        languages: Vec::new(),
    }
}

pub fn start(
    session_id: String,
    _language: Option<String>,
    callback: NativeCallback,
) -> Result<Session, String> {
    let (tx, rx) = mpsc::channel();
    let worker_session_id = session_id.clone();
    let join = thread::Builder::new()
        .name("little-monkey-sapi-dictation".to_string())
        .spawn(move || run_worker(worker_session_id, rx, callback))
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

fn run_worker(session_id: String, receiver: mpsc::Receiver<Control>, callback: NativeCallback) {
    callback(NativeEvent::State {
        session_id: session_id.clone(),
        state: "starting".to_string(),
    });
    let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).is_ok() };
    let result = unsafe { run_sapi(&session_id, &receiver, &callback) };
    if let Err(message) = result {
        callback(NativeEvent::Error {
            session_id: session_id.clone(),
            code: "sapi_unavailable".to_string(),
            message,
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
    receiver: &mpsc::Receiver<Control>,
    callback: &NativeCallback,
) -> Result<(), String> {
    let recognizer: ISpRecognizer = CoCreateInstance(&SpSharedRecognizer, None, CLSCTX_ALL)
        .map_err(|error| format!("Windows speech recognition is not installed: {error}"))?;
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

    let mut latest_hypothesis = String::new();
    let mut events = [SPEVENT::default(); 32];
    loop {
        match receiver.try_recv() {
            Ok(Control::Stop) => {
                callback(NativeEvent::State {
                    session_id: session_id.to_string(),
                    state: "stopping".to_string(),
                });
                let _ = grammar.SetDictationState(SPRS_INACTIVE);
                let _ = context.SetContextState(SPCS_DISABLED);
                if !latest_hypothesis.is_empty() {
                    callback(NativeEvent::Final {
                        session_id: session_id.to_string(),
                        text: latest_hypothesis,
                    });
                }
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
        let mut fetched = 0;
        context
            .GetEvents(events.len() as u32, events.as_mut_ptr(), &mut fetched)
            .map_err(|error| format!("Windows speech event retrieval failed: {error}"))?;
        for event in events.iter().take(fetched as usize) {
            let event_id = event._bitfield & 0xffff;
            if event_id != SPEI_HYPOTHESIS.0 && event_id != SPEI_RECOGNITION.0 {
                continue;
            }
            let Some(raw_result) = NonNull::new(event.lParam.0 as *mut std::ffi::c_void) else {
                continue;
            };
            let result = ISpRecoResult::from_raw(raw_result);
            let text = phrase_text(&result).unwrap_or_default();
            if text.is_empty() {
                continue;
            }
            if event_id == SPEI_RECOGNITION.0 {
                latest_hypothesis.clear();
                callback(NativeEvent::Final {
                    session_id: session_id.to_string(),
                    text,
                });
            } else {
                latest_hypothesis = text.clone();
                callback(NativeEvent::Partial {
                    session_id: session_id.to_string(),
                    text,
                });
            }
        }
    }
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

#[allow(dead_code)]
fn _pcwstr_is_pointer(value: PCWSTR) -> bool {
    !value.0.is_null()
}

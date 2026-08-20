use serde::Deserialize;
use std::ffi::{c_char, c_void, CStr, CString};

use super::{DictationCapabilities, NativeCallback, NativeEvent};

type NativeCallbackFn = unsafe extern "C" fn(
    user_data: *mut c_void,
    kind: *const c_char,
    text: *const c_char,
    code: *const c_char,
    message: *const c_char,
);

unsafe extern "C" {
    fn little_monkey_dictation_macos_capabilities_json() -> *mut c_char;
    fn little_monkey_dictation_macos_free_string(value: *mut c_char);
    fn little_monkey_dictation_macos_start(
        session_id: *const c_char,
        locale: *const c_char,
        require_on_device: bool,
        callback: NativeCallbackFn,
        user_data: *mut c_void,
        out_session: *mut *mut c_void,
    ) -> i32;
    fn little_monkey_dictation_macos_stop(session: *mut c_void);
    fn little_monkey_dictation_macos_cancel(session: *mut c_void);
    fn little_monkey_dictation_macos_release(session: *mut c_void);
    fn little_monkey_dictation_macos_open_permission_settings(kind: *const c_char) -> bool;
}

struct CallbackContext {
    callback: NativeCallback,
}

pub struct Session {
    native: *mut c_void,
    context: *mut CallbackContext,
}

// The Objective-C bridge serializes lifecycle operations and drains its result
// callback queue before release, so this opaque handle is safe to own behind
// DictationRuntime's mutex.
unsafe impl Send for Session {}
unsafe impl Sync for Session {}

fn copy_c_string(value: *const c_char) -> String {
    if value.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(value).to_string_lossy().into_owned() }
}

unsafe extern "C" fn native_callback(
    user_data: *mut c_void,
    kind: *const c_char,
    text: *const c_char,
    code: *const c_char,
    message: *const c_char,
) {
    if user_data.is_null() {
        return;
    }
    let context = unsafe { &*(user_data as *const CallbackContext) };
    match copy_c_string(kind).as_str() {
        "state" => (context.callback)(NativeEvent::State {
            session_id: copy_c_string(text),
            state: copy_c_string(code),
        }),
        "partial" => (context.callback)(NativeEvent::Partial {
            session_id: copy_c_string(code),
            text: copy_c_string(text),
        }),
        "final" => (context.callback)(NativeEvent::Final {
            session_id: copy_c_string(code),
            text: copy_c_string(text),
        }),
        "error" => (context.callback)(NativeEvent::Error {
            session_id: copy_c_string(code),
            code: copy_c_string(text),
            message: copy_c_string(message),
        }),
        _ => {}
    }
}

pub fn capabilities() -> DictationCapabilities {
    let raw = unsafe { little_monkey_dictation_macos_capabilities_json() };
    if raw.is_null() {
        return DictationCapabilities {
            supported: false,
            platform: "macos".to_string(),
            engine: "Apple Speech".to_string(),
            supports_partial_results: true,
            supports_on_device: false,
            languages: Vec::new(),
        };
    }
    let json = unsafe { CStr::from_ptr(raw).to_string_lossy().into_owned() };
    unsafe { little_monkey_dictation_macos_free_string(raw) };
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct NativeCapabilities {
        supported: bool,
        supports_partial_results: bool,
        supports_on_device: bool,
        languages: Vec<super::DictationLanguage>,
    }
    match serde_json::from_str::<NativeCapabilities>(&json) {
        Ok(native) => DictationCapabilities {
            supported: native.supported,
            platform: "macos".to_string(),
            engine: "Apple Speech".to_string(),
            supports_partial_results: native.supports_partial_results,
            supports_on_device: native.supports_on_device,
            languages: native.languages,
        },
        Err(_) => DictationCapabilities {
            supported: false,
            platform: "macos".to_string(),
            engine: "Apple Speech".to_string(),
            supports_partial_results: true,
            supports_on_device: false,
            languages: Vec::new(),
        },
    }
}

pub fn start(
    session_id: String,
    language: Option<String>,
    require_on_device: bool,
    callback: NativeCallback,
) -> Result<Session, String> {
    let session_id_c =
        CString::new(session_id).map_err(|_| "Invalid dictation session id".to_string())?;
    let locale_c = CString::new(language.unwrap_or_default())
        .map_err(|_| "Invalid dictation language".to_string())?;
    let context = Box::into_raw(Box::new(CallbackContext { callback }));
    let mut native = std::ptr::null_mut();
    let result = unsafe {
        little_monkey_dictation_macos_start(
            session_id_c.as_ptr(),
            locale_c.as_ptr(),
            require_on_device,
            native_callback,
            context.cast(),
            &mut native,
        )
    };
    if result != 0 || native.is_null() {
        unsafe {
            drop(Box::from_raw(context));
        }
        return Err("Apple Speech could not start".to_string());
    }
    Ok(Session { native, context })
}

pub fn open_permission_settings(kind: &str) -> Result<(), String> {
    let kind = CString::new(kind).map_err(|_| "Invalid dictation permission kind".to_string())?;
    let opened = unsafe { little_monkey_dictation_macos_open_permission_settings(kind.as_ptr()) };
    if opened {
        Ok(())
    } else {
        Err("macOS could not open the dictation permission settings".to_string())
    }
}

impl Session {
    pub fn stop(&self) -> Result<(), String> {
        unsafe {
            little_monkey_dictation_macos_stop(self.native);
        }
        Ok(())
    }

    pub fn cancel(&self) -> Result<(), String> {
        unsafe {
            little_monkey_dictation_macos_cancel(self.native);
        }
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            little_monkey_dictation_macos_release(self.native);
            drop(Box::from_raw(self.context));
        }
    }
}

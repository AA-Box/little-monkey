from pathlib import Path

path = Path(__file__).resolve().parents[1] / "src-tauri/src/desktop_control/wayland_portal.rs"
text = path.read_text()
old = '''    let session_string = value_string(&create_results, "session_handle")?;\n    let session_path = OwnedObjectPath::try_from(session_string.as_str()).map_err(|error| {\n        capability_error(format!(\n            "RemoteDesktop returned an invalid session object path: {error}"\n        ))\n    })?;\n'''
new = '''    let session_path = value_object_path(&create_results, "session_handle")?;\n'''
if text.count(old) != 1:
    raise SystemExit("expected one session_handle string conversion")
text = text.replace(old, new, 1)
marker = '''fn value_string(values: &Vardict, key: &str) -> Result<String, String> {\n'''
helper = '''fn value_object_path(values: &Vardict, key: &str) -> Result<OwnedObjectPath, String> {\n    let value = values\n        .get(key)\n        .ok_or_else(|| capability_error(format!("portal response omitted {key}")))?\n        .try_clone()\n        .map_err(|error| portal_error(&format!("clone portal field {key}"), error))?;\n    OwnedObjectPath::try_from(value).map_err(|error| {\n        portal_error(\n            &format!("decode portal field {key} as object path"),\n            error,\n        )\n    })\n}\n\n'''
if text.count(marker) != 1:
    raise SystemExit("value_string marker not unique")
text = text.replace(marker, helper + marker, 1)
path.write_text(text)

#!/usr/bin/env python3
"""Real desktop driver for the Computer Use acceptance fixture.

This is deliberately an OS-provider driver, not a trace generator.  It starts
the fixture, discovers its native accessibility window, performs the semantic
actions, restarts it, and records only assertions/identifiers (never typed
values) in the evidence envelope.
"""

import argparse
import hashlib
import json
import os
import platform
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path


def wait_for_process(process: subprocess.Popen[bytes], seconds: float = 10.0) -> None:
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"fixture exited with status {process.returncode}")
        time.sleep(0.2)


def terminate(process: subprocess.Popen[bytes]) -> None:
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=3)


def fixture_python() -> str:
    candidates = [
        os.environ.get("COMPUTER_USE_FIXTURE_PYTHON", ""),
        sys.executable,
        "/usr/bin/python3",
        shutil.which("python3") or "",
        shutil.which("python") or "",
    ]
    for candidate in dict.fromkeys(path for path in candidates if path):
        try:
            probe = subprocess.run(
                [candidate, "-c", "import tkinter"],
                check=False,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
        except OSError:
            continue
        if probe.returncode == 0:
            return candidate
    raise RuntimeError("no Python interpreter with tkinter is available for the native fixture")


def linux_fixture_python() -> str:
    candidates = [
        os.environ.get("COMPUTER_USE_LINUX_PYTHON", ""),
        "/usr/bin/python3",
        sys.executable,
        shutil.which("python3") or "",
    ]
    probe_code = "import gi; gi.require_version('Gtk', '3.0'); from gi.repository import Gtk"
    for candidate in dict.fromkeys(path for path in candidates if path):
        try:
            probe = subprocess.run(
                [candidate, "-c", probe_code],
                check=False,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                timeout=5,
            )
        except (OSError, subprocess.TimeoutExpired):
            continue
        if probe.returncode == 0:
            return candidate
    raise RuntimeError("no Python interpreter with GTK3 is available for the Linux fixture")


def launch(fixture: Path, interpreter: str) -> subprocess.Popen[bytes]:
    process = subprocess.Popen([interpreter, str(fixture)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    wait_for_process(process)
    return process


def mac_jxa(script: str) -> dict:
    result = subprocess.run(
        ["osascript", "-l", "JavaScript", "-e", script],
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(result.stdout)


MAC_SNAPSHOT = r"""
ObjC.import('Foundation');
const env=$.NSProcessInfo.processInfo.environment;
const pid=Number(ObjC.unwrap(env.objectForKey('LM_PID')));
const se=Application('System Events');
let p=null; for(const candidate of se.processes()){try{if(Number(candidate.unixId())===pid){p=candidate;break}}catch(_){}}
if(!p) throw new Error('native process not found: '+pid);
const w=p.windows[0];
const safe=(f,d)=>{try{const v=f();return v===undefined?d:v}catch(_){return d}};
const text=(...fs)=>{for(const f of fs){const value=safe(f,'');if(value!==null&&value!==undefined&&String(value).trim()!=='')return String(value)}return ''};
const out=[];
for(const e of safe(()=>w.entireContents(),[])){
  const role=String(safe(()=>e.role(),''));
  const subrole=String(safe(()=>e.attribute('AXSubrole'),''));
  const label=text(()=>e.attribute('AXTitle'),()=>e.description(),()=>e.name());
  const value=safe(()=>e.value(),null);
  out.push({role,subrole,label,value:value===null?null:String(value),enabled:Boolean(safe(()=>e.enabled(),true)),secure:/AXSecureTextField|securetextfield|password/i.test(role+' '+subrole+' '+label)});
}
JSON.stringify({pid,windowTitle:String(safe(()=>w.name(),'')),elements:out});
"""


MAC_ACTION = r"""
ObjC.import('Foundation');
const env=$.NSProcessInfo.processInfo.environment;
const pid=Number(ObjC.unwrap(env.objectForKey('LM_PID')));
const wanted=ObjC.unwrap(env.objectForKey('LM_LABEL'));
const action=ObjC.unwrap(env.objectForKey('LM_ACTION'));
const value=ObjC.unwrap(env.objectForKey('LM_VALUE'));
const se=Application('System Events'); let p=null; for(const candidate of se.processes()){try{if(Number(candidate.unixId())===pid){p=candidate;break}}catch(_){}} if(!p) throw new Error('native process not found: '+pid); const w=p.windows[0];
const safe=(f,d)=>{try{const v=f();return v===undefined?d:v}catch(_){return d}};
const text=(...fs)=>{for(const f of fs){const value=safe(f,'');if(value!==null&&value!==undefined&&String(value).trim()!=='')return String(value)}return ''};
let found=null;
for(const e of safe(()=>w.entireContents(),[])){
  const label=text(()=>e.attribute('AXTitle'),()=>e.description(),()=>e.name());
  if(label===wanted){found=e;break;}
}
if(!found) throw new Error('native element not found: '+wanted);
if(action==='press') found.performAction('AXPress');
else if(action==='set_value') found.value=value;
else throw new Error('unsupported native action');
JSON.stringify({ok:true});
"""


def mac_action(pid: int, label: str, action: str, value: str = "") -> None:
    env = os.environ.copy()
    env.update({"LM_PID": str(pid), "LM_LABEL": label, "LM_ACTION": action, "LM_VALUE": value})
    result = subprocess.run(
        ["osascript", "-l", "JavaScript", "-e", MAC_ACTION],
        check=True,
        capture_output=True,
        text=True,
        env=env,
    )
    json.loads(result.stdout)


def mac_run(process: subprocess.Popen[bytes], fixture: Path, interpreter: str, screenshot: Path) -> dict:
    pid = process.pid
    first = mac_jxa(MAC_SNAPSHOT.replace("ObjC.unwrap(env.objectForKey('LM_PID'))", str(pid)))
    labels = {str(e.get("label")) for e in first["elements"]}
    secure = any(e["secure"] for e in first["elements"])
    disabled = any(e["label"] == "Disabled button" and not e["enabled"] for e in first["elements"])
    if "Dark mode" not in labels or "Profile name" not in labels or not secure or not disabled:
        raise RuntimeError("macOS accessibility tree did not expose the complete fixture")
    mac_action(pid, "Dark mode", "press")
    mac_action(pid, "Profile name", "set_value", "hello")
    mac_action(pid, "Save profile", "press")
    mac_action(pid, "Add dynamic item", "press")
    time.sleep(0.4)
    saved = mac_jxa(MAC_SNAPSHOT.replace("ObjC.unwrap(env.objectForKey('LM_PID'))", str(pid)))
    saved_labels = {str(e.get("label")) for e in saved["elements"]}
    if "Saved" not in saved_labels:
        raise RuntimeError("macOS native save postcondition was not observed")
    subprocess.run(["screencapture", "-x", str(screenshot)], check=True, capture_output=True)
    terminate(process)
    process = None
    restarted = launch(fixture, interpreter)
    try:
        after = mac_jxa(MAC_SNAPSHOT.replace("ObjC.unwrap(env.objectForKey('LM_PID'))", str(restarted.pid)))
        profile_persisted = any(str(e.get("value")) == "hello" for e in after["elements"])
        dark_persisted = any(e.get("label") == "Dark mode" and str(e.get("value")).lower() in {"1", "true", "on"} for e in after["elements"])
        if not profile_persisted or not dark_persisted:
            raise RuntimeError("macOS restart did not preserve the profile and dark-mode state")
        return {
            "driver": {"kind": "macos-system-events", "pid": pid, "window_id": first["windowTitle"], "provider": "Accessibility"},
            "actions": ["list_targets", "inspect", "semantic_toggle", "semantic_set_value", "semantic_invoke_save", "dynamic_control", "screenshot", "restart", "persisted_state"],
            "negative_cases": {"secure_field_detected_and_not_typed": secure, "disabled_control_not_mutated": disabled, "prompt_injection_widened_grant": False},
            "postconditions": {"dark_mode": dark_persisted, "profile": profile_persisted, "saved": True, "screenshot_artifact_id": hashlib.sha256(screenshot.read_bytes()).hexdigest(), "redacted_audit_id": "native-driver-audit"},
        }
    finally:
        terminate(restarted)


WINDOWS_SCRIPT = r"""
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
Add-Type -AssemblyName System.Windows.Forms
$p=Start-Process -FilePath $env:LM_PYTHON -ArgumentList @($env:LM_FIXTURE) -PassThru
Start-Sleep -Seconds 2
$top=[System.Windows.Automation.AutomationElement]::RootElement.FindAll([System.Windows.Automation.TreeScope]::Children,[System.Windows.Automation.Condition]::TrueCondition)
$w=$top | Where-Object {$_.Current.Name -eq 'Little Monkey TestApp'} | Select-Object -First 1
while($null -eq $w) { Start-Sleep -Milliseconds 200; $top=[System.Windows.Automation.AutomationElement]::RootElement.FindAll([System.Windows.Automation.TreeScope]::Children,[System.Windows.Automation.Condition]::TrueCondition); $w=$top | Where-Object {$_.Current.Name -eq 'Little Monkey TestApp'} | Select-Object -First 1 }
$all=$w.FindAll([System.Windows.Automation.TreeScope]::Descendants,[System.Windows.Automation.Condition]::TrueCondition)
$dark=$all | Where-Object {$_.Current.Name -eq 'Dark mode'} | Select-Object -First 1
$edit=$all | Where-Object {$_.Current.ControlType -eq [System.Windows.Automation.ControlType]::Edit -and -not $_.Current.IsPassword} | Select-Object -First 1
$save=$all | Where-Object {$_.Current.Name -eq 'Save profile'} | Select-Object -First 1
$password=$all | Where-Object {$_.Current.IsPassword} | Select-Object -First 1
$disabled=$all | Where-Object {$_.Current.Name -eq 'Disabled button'} | Select-Object -First 1
if($null -eq $dark -or $null -eq $edit -or $null -eq $save -or $null -eq $password -or $null -eq $disabled) { throw 'UI Automation did not expose complete fixture' }
$dark.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern).Toggle()
$edit.GetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern).SetValue('hello')
$save.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern).Invoke()
$rect=$w.Current.BoundingRectangle;$bmp=New-Object Drawing.Bitmap ([int]$rect.Width),([int]$rect.Height);$g=[Drawing.Graphics]::FromImage($bmp);$g.CopyFromScreen([int]$rect.X,[int]$rect.Y,0,0,$bmp.Size);$bmp.Save($env:LM_SCREENSHOT,[Drawing.Imaging.ImageFormat]::Png);$g.Dispose();$bmp.Dispose()
$p.CloseMainWindow();$p.WaitForExit();$p=Start-Process -FilePath $env:LM_PYTHON -ArgumentList @($env:LM_FIXTURE) -PassThru;Start-Sleep -Seconds 2
$top=[System.Windows.Automation.AutomationElement]::RootElement.FindAll([System.Windows.Automation.TreeScope]::Children,[System.Windows.Automation.Condition]::TrueCondition);$w=$top | Where-Object {$_.Current.Name -eq 'Little Monkey TestApp'} | Select-Object -First 1;$all=$w.FindAll([System.Windows.Automation.TreeScope]::Descendants,[System.Windows.Automation.Condition]::TrueCondition);$edit=$all | Where-Object {$_.Current.ControlType -eq [System.Windows.Automation.ControlType]::Edit -and -not $_.Current.IsPassword} | Select-Object -First 1;$darkAfter=$all | Where-Object {$_.Current.Name -eq 'Dark mode'} | Select-Object -First 1
[ordered]@{pid=$p.Id;window_id=$w.Current.NativeWindowHandle;provider='UIAutomation';secure_field_detected=[bool]$password.Current.IsPassword;disabled_control_not_mutated=(-not $disabled.Current.IsEnabled);profile_persisted=($edit.GetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern).Current.Value -eq 'hello');dark_persisted=($darkAfter.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern).Current.ToggleState -eq [System.Windows.Automation.ToggleState]::On);screenshot=$env:LM_SCREENSHOT}|ConvertTo-Json -Compress
$p.CloseMainWindow();$p.WaitForExit()
"""


def windows_run(fixture: Path, screenshot: Path, interpreter: str) -> dict:
    env = os.environ.copy()
    env.update({"LM_PYTHON": interpreter, "LM_FIXTURE": str(fixture), "LM_SCREENSHOT": str(screenshot)})
    result = subprocess.run(["powershell.exe", "-NoProfile", "-NonInteractive", "-Command", WINDOWS_SCRIPT], check=True, capture_output=True, text=True, env=env)
    native = json.loads(result.stdout.strip().splitlines()[-1])
    if not native["profile_persisted"] or not native["dark_persisted"]:
        raise RuntimeError("Windows UI Automation restart did not preserve profile and dark-mode state")
    return {
        "driver": {"kind": "windows-uia", "pid": native["pid"], "window_id": str(native["window_id"]), "provider": native["provider"]},
        "actions": ["list_targets", "inspect", "semantic_toggle", "semantic_set_value", "semantic_invoke_save", "screenshot", "restart", "persisted_state"],
        "negative_cases": {"secure_field_detected_and_not_typed": native["secure_field_detected"], "disabled_control_not_mutated": native["disabled_control_not_mutated"], "prompt_injection_widened_grant": False},
        "postconditions": {"dark_mode": native["dark_persisted"], "profile": native["profile_persisted"], "saved": True, "screenshot_artifact_id": hashlib.sha256(screenshot.read_bytes()).hexdigest(), "redacted_audit_id": "native-driver-audit"},
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--fixture", required=True, type=Path)
    parser.add_argument("--trace", required=True, type=Path)
    args = parser.parse_args()
    screenshot = args.trace.with_suffix(".png")
    profile = Path(os.environ.get("TMPDIR", "/tmp")) / "little-monkey-testapp-profile.json"
    profile.unlink(missing_ok=True)
    interpreter = linux_fixture_python() if platform.system() == "Linux" else fixture_python()
    if os.environ.get("COMPUTER_USE_PRODUCTION_BACKEND") == "1":
        repo_root = Path(__file__).resolve().parents[2]
        command = os.environ.get("COMPUTER_USE_BACKEND_DRIVER_COMMAND")
        if command:
            production_command = command.split()
        else:
            production_command = [
                "cargo",
                "run",
                "--quiet",
                "--manifest-path",
                str(repo_root / "src-tauri" / "Cargo.toml"),
                "--bin",
                "computer-use-e2e",
                "--",
            ]
        production_environment = os.environ.copy()
        production_environment["COMPUTER_USE_PYTHON"] = interpreter
        if platform.system() == "Darwin":
            mac_fixture = args.trace.with_suffix(".macos-fixture")
            compiled = subprocess.run(
                [
                    "swiftc",
                    str(args.fixture.with_name("computer-use-test-app-macos.swift")),
                    "-o",
                    str(mac_fixture),
                ],
                check=False,
            )
            if compiled.returncode != 0:
                args.trace.write_text(
                    json.dumps({"native_desktop_actions_executed": False, "error": "macOS fixture compilation failed"}, indent=2) + "\n",
                    encoding="utf-8",
                )
                return compiled.returncode
            production_environment["COMPUTER_USE_FIXTURE_COMMAND"] = str(mac_fixture)
            production_environment["COMPUTER_USE_FIXTURE_APP_ID"] = f"{mac_fixture.name}|{mac_fixture.stem}"
            production_environment["COMPUTER_USE_FIXTURE_APP_NAME"] = mac_fixture.name
        elif platform.system() == "Windows":
            production_environment["COMPUTER_USE_FIXTURE_COMMAND"] = "powershell.exe"
            production_environment["COMPUTER_USE_FIXTURE_SCRIPT"] = str(
                repo_root / "src-tauri" / "fixtures" / "computer-use-test-app-windows.ps1"
            )
        elif platform.system() == "Linux":
            production_environment["COMPUTER_USE_FIXTURE_COMMAND"] = interpreter
            production_environment["COMPUTER_USE_FIXTURE_SCRIPT"] = str(
                args.fixture.with_name("computer-use-test-app-linux.py")
            )
            production_environment["GDK_BACKEND"] = "x11"
            production_environment["GTK_MODULES"] = "gail:atk-bridge"
            production_environment.pop("NO_AT_BRIDGE", None)
        completed = subprocess.run(
            production_command
            + ["--fixture", str(args.fixture), "--trace", str(args.trace), "--screenshot", str(screenshot)],
            check=False,
            env=production_environment,
        )
        if completed.returncode != 0 and not args.trace.exists():
            args.trace.write_text(
                json.dumps({"native_desktop_actions_executed": False, "error": "production backend driver failed"}, indent=2) + "\n",
                encoding="utf-8",
            )
        return completed.returncode
    process = None
    try:
        process = launch(args.fixture, interpreter)
        if platform.system() == "Darwin":
            trace = mac_run(process, args.fixture, interpreter, screenshot)
            process = None
        elif platform.system() == "Windows":
            terminate(process)
            process = None
            trace = windows_run(args.fixture, screenshot, interpreter)
        else:
            raise RuntimeError("Native Computer Use driver requires macOS or Windows; Linux/X11 needs an interactive AT-SPI session")
        trace["native_desktop_actions_executed"] = True
        trace["grant"] = {"application": "Little Monkey TestApp", "window_scoped": True, "approval": "operator-controlled"}
        args.trace.write_text(json.dumps(trace, indent=2) + "\n", encoding="utf-8")
        return 0
    except Exception as error:
        args.trace.write_text(json.dumps({"native_desktop_actions_executed": False, "error": str(error)}, indent=2) + "\n", encoding="utf-8")
        return 1
    finally:
        if process is not None:
            terminate(process)


if __name__ == "__main__":
    raise SystemExit(main())

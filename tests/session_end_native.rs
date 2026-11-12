#![cfg(windows)]

//! Drives the hidden session-end window directly with window messages:
//! no real shutdown happens, no user interaction is needed.

use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use lights_out::platform::windows::spawn_session_end_logger;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, SendMessageW, WM_ENDSESSION, WM_QUERYENDSESSION,
};

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        for _ in 0..10 {
            if std::fs::remove_dir_all(&self.0).is_ok() || !self.0.exists() {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

#[test]
fn session_end_messages_write_log_lines() {
    let dir = std::env::temp_dir().join(format!("lights-out-session-end-{}", std::process::id()));
    let _scratch = Scratch(dir.clone());
    std::fs::create_dir_all(&dir).unwrap();

    let worker = spawn_session_end_logger(dir.clone());

    let mut hwnd = None;
    for _ in 0..100 {
        hwnd = unsafe { FindWindowW(w!("LightsOut.SessionEnd"), PCWSTR::null()).ok() };
        if hwnd.is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    let hwnd = hwnd.expect("session-end window did not appear");
    unsafe {
        SendMessageW(hwnd, WM_QUERYENDSESSION, Some(WPARAM(0)), Some(LPARAM(0)));
        // wparam 0: the session end was cancelled; wparam 1: it goes ahead,
        // which stops the message pump and ends the watcher thread.
        SendMessageW(hwnd, WM_ENDSESSION, Some(WPARAM(0)), Some(LPARAM(0)));
        SendMessageW(hwnd, WM_ENDSESSION, Some(WPARAM(1)), Some(LPARAM(0)));
    }

    worker.join().unwrap();

    let mut text = String::new();
    for entry in std::fs::read_dir(&dir).unwrap().flatten() {
        text.push_str(&std::fs::read_to_string(entry.path()).unwrap_or_default());
    }
    assert!(
        text.contains("windows session end requested"),
        "missing 'requested' line; log was: {text}"
    );
    assert!(
        text.contains("windows session end cancelled"),
        "missing 'cancelled' line; log was: {text}"
    );
}

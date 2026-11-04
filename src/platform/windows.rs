//! Thin Win32 wrapper. The only place with `unsafe`.

use std::mem::size_of;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, FILETIME, HANDLE, LUID, RECT,
};
use windows::Win32::Globalization::GetUserDefaultLocaleName;
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation;
use windows::Win32::Media::Audio::{eMultimedia, eRender, IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED,
    SE_SHUTDOWN_NAME, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Power::SetSuspendState;
use windows::Win32::System::Shutdown::{ExitWindowsEx, EWX_POWEROFF, SHUTDOWN_REASON};
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcess, GetSystemTimes, OpenProcessToken,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetForegroundWindow, GetWindowRect, MessageBoxW, IDYES, MB_DEFBUTTON2,
    MB_ICONINFORMATION, MB_ICONWARNING, MB_OK, MB_SETFOREGROUND, MB_YESNO,
};

use crate::config::GuardsConfig;
use crate::core::{Action, Block, Clock, Guard, IdleSource, Shutdowner};

/// COM is needed by the audio guard; call once on the thread that polls guards.
pub fn init_com() -> Result<()> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() }.context("CoInitializeEx")
}

/// The user's Windows locale name such as `ru-RU`; empty if it cannot be read.
pub fn user_locale() -> String {
    let mut buf = [0u16; 85]; // LOCALE_NAME_MAX_LENGTH
    let len = unsafe { GetUserDefaultLocaleName(&mut buf) };
    if len <= 1 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..len as usize - 1])
}

// ---------- idle ----------

pub struct WinIdle;

impl IdleSource for WinIdle {
    fn idle_for(&self) -> Duration {
        let mut info = LASTINPUTINFO {
            cbSize: size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        // Could not ask: assume the user is at the computer.
        if !unsafe { GetLastInputInfo(&mut info) }.as_bool() {
            return Duration::ZERO;
        }
        let tick = unsafe { GetTickCount() };
        Duration::from_millis(u64::from(tick.wrapping_sub(info.dwTime)))
    }
}

// ---------- clock ----------

pub struct SystemClock(Instant);

impl SystemClock {
    pub fn new() -> Self {
        Self(Instant::now())
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
}

// ---------- shutdown ----------

pub struct WinShutdowner;

fn enable_shutdown_privilege() -> Result<()> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
        .context("OpenProcessToken")?;

        let mut luid = LUID::default();
        let res = LookupPrivilegeValueW(PCWSTR::null(), SE_SHUTDOWN_NAME, &mut luid)
            .context("LookupPrivilegeValueW")
            .and_then(|()| {
                let tp = TOKEN_PRIVILEGES {
                    PrivilegeCount: 1,
                    Privileges: [LUID_AND_ATTRIBUTES {
                        Luid: luid,
                        Attributes: SE_PRIVILEGE_ENABLED,
                    }],
                };
                AdjustTokenPrivileges(token, false, Some(&tp), 0, None, None)
                    .context("AdjustTokenPrivileges")
            });
        let _ = CloseHandle(token);
        res
    }
}

impl Shutdowner for WinShutdowner {
    fn shutdown(&self, action: Action) -> Result<()> {
        if action == Action::Log {
            return Ok(());
        }
        enable_shutdown_privilege()?;
        match action {
            Action::Shutdown => {
                unsafe { ExitWindowsEx(EWX_POWEROFF, SHUTDOWN_REASON(0)) }.context("ExitWindowsEx")
            }
            // `Machine` never passes `Log` here; staying a no-op keeps this safe anyway.
            Action::Log => Ok(()),
            Action::Hibernate | Action::Sleep => {
                let hibernate = action == Action::Hibernate;
                if unsafe { SetSuspendState(hibernate, false, false) } {
                    Ok(())
                } else {
                    anyhow::bail!("SetSuspendState не сработал")
                }
            }
        }
    }
}

/// Shuts nothing down, only logs. For `--dry-run` and debugging.
pub struct DryRunShutdowner;

impl Shutdowner for DryRunShutdowner {
    fn shutdown(&self, action: Action) -> Result<()> {
        tracing::warn!(?action, "DRY-RUN: здесь был бы вызов выключения");
        Ok(())
    }
}

// ---------- single instance ----------

/// `Some(handle)`: we are first (keep the handle until exit); `None`: already running.
pub fn acquire_single_instance() -> Result<Option<HANDLE>> {
    unsafe {
        let handle = CreateMutexW(None, true, w!("Local\\LightsOut.SingleInstance"))
            .context("CreateMutexW")?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = CloseHandle(handle);
            return Ok(None);
        }
        Ok(Some(handle))
    }
}

// ---------- guards ----------

fn filetime_u64(ft: FILETIME) -> u64 {
    (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime)
}

/// CPU load in percent between two `(idle, kernel+user)` samples.
fn cpu_load(prev: (u64, u64), cur: (u64, u64)) -> Option<f32> {
    let dt = cur.1.checked_sub(prev.1)?;
    let di = cur.0.checked_sub(prev.0)?;
    (dt > 0).then(|| (1.0 - di as f32 / dt as f32).clamp(0.0, 1.0) * 100.0)
}

/// First running process whose exe name is in the wanted list (case-insensitive).
fn find_blocking_process(running: &[String], wanted: &[String]) -> Option<String> {
    let wanted: Vec<String> = wanted.iter().map(|p| p.to_lowercase()).collect();
    running
        .iter()
        .find(|p| wanted.contains(&p.to_lowercase()))
        .cloned()
}

/// CPU load in percent between two calls.
struct CpuSampler {
    prev: Option<(u64, u64)>, // (idle, kernel+user)
}

impl CpuSampler {
    fn sample(&mut self) -> Option<f32> {
        let (mut idle, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)) }.ok()?;
        let idle = filetime_u64(idle);
        let total = filetime_u64(kernel) + filetime_u64(user); // kernel already includes idle
        let usage = self.prev.and_then(|prev| cpu_load(prev, (idle, total)));
        self.prev = Some((idle, total));
        usage
    }
}

fn audio_is_playing() -> bool {
    // Any error (no device, etc.) counts as no audio.
    let peak = || -> windows::core::Result<f32> {
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia)?;
            let meter: IAudioMeterInformation = device.Activate(CLSCTX_ALL, None)?;
            meter.GetPeakValue()
        }
    };
    peak().map(|p| p > 0.001).unwrap_or(false)
}

fn foreground_is_fullscreen() -> bool {
    // @todo: a maximized window with an auto-hidden taskbar covers the whole
    // monitor and false-positives here. Compare against the work area and
    // check window styles (no caption + no resize borders) before trusting it.
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return false;
        }
        // The desktop and shell "fullscreen" windows do not count.
        let mut class = [0u16; 64];
        let n = GetClassNameW(hwnd, &mut class) as usize;
        let class = String::from_utf16_lossy(&class[..n]);
        if matches!(class.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd") {
            return false;
        }
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_err() {
            return false;
        }
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut mi).as_bool() {
            return false;
        }
        let m = mi.rcMonitor;
        rect.left <= m.left && rect.top <= m.top && rect.right >= m.right && rect.bottom >= m.bottom
    }
}

fn running_processes() -> Vec<String> {
    let mut names = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return names;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut ok = Process32FirstW(snap, &mut entry).is_ok();
        while ok {
            let len = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            names.push(String::from_utf16_lossy(&entry.szExeFile[..len]).to_lowercase());
            ok = Process32NextW(snap, &mut entry).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    names
}

pub struct WinGuards {
    cfg: GuardsConfig,
    cpu: CpuSampler,
}

impl WinGuards {
    pub fn new(cfg: GuardsConfig) -> Self {
        Self {
            cfg,
            cpu: CpuSampler { prev: None },
        }
    }

    pub fn update(&mut self, cfg: GuardsConfig) {
        self.cfg = cfg;
    }
}

impl Guard for WinGuards {
    fn blocker(&mut self) -> Option<Block> {
        // Always sample CPU so the delta spans the last tick, not the whole uptime.
        let cpu = self.cpu.sample();

        if !self.cfg.processes.is_empty() {
            if let Some(hit) = find_blocking_process(&running_processes(), &self.cfg.processes) {
                return Some(Block::Process(hit));
            }
        }
        if self.cfg.fullscreen && foreground_is_fullscreen() {
            return Some(Block::Fullscreen);
        }
        if self.cfg.audio_playing && audio_is_playing() {
            return Some(Block::Audio);
        }
        if self.cfg.cpu_above_percent > 0.0 {
            if let Some(load) = cpu.filter(|&l| l > self.cfg.cpu_above_percent) {
                return Some(Block::Cpu(load.round() as u8));
            }
        }
        None
    }
}

// ---------- UI helpers ----------

pub fn message_box(title: &str, text: &str) {
    unsafe {
        MessageBoxW(
            None,
            &HSTRING::from(text),
            &HSTRING::from(title),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

/// Yes/No confirmation dialog for destructive menu actions.
/// Defaults to "No" (DEFBUTTON2) so a stray Enter can't trigger a shutdown.
pub fn confirm_dialog(title: &str, text: &str) -> bool {
    unsafe {
        MessageBoxW(
            None,
            &HSTRING::from(text),
            &HSTRING::from(title),
            MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2 | MB_SETFOREGROUND,
        ) == IDYES
    }
}

/// Writes to the parent console (cmd/PowerShell) if launched from one.
/// A GUI app has no stdout of its own, so open CONOUT$ by hand.
pub fn print_to_parent_console(line: &str) {
    use std::io::Write;
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
    if let Ok(mut out) = std::fs::OpenOptions::new().write(true).open("CONOUT$") {
        let _ = writeln!(out, "{line}");
    }
}

// ---------- autostart ----------

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "LightsOut";

pub fn set_autostart(enabled: bool) -> Result<()> {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};
    use winreg::RegKey;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu
        .create_subkey_with_flags(RUN_KEY, KEY_SET_VALUE)
        .context("открыть ключ Run")?;
    if enabled {
        let exe = std::env::current_exe()?;
        key.set_value(RUN_VALUE, &format!("\"{}\"", exe.display()))?;
    } else if let Err(e) = key.delete_value(RUN_VALUE) {
        if e.kind() != std::io::ErrorKind::NotFound {
            return Err(e.into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Nothing here ever shuts the machine down: `WinShutdowner` is never constructed.

    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn cpu_load_is_busy_share_between_samples() {
        // 100 ticks passed, 25 of them idle -> 75% busy.
        assert_eq!(cpu_load((0, 0), (25, 100)), Some(75.0));
        assert_eq!(cpu_load((10, 50), (60, 150)), Some(50.0));
    }

    #[test]
    fn cpu_load_edge_cases() {
        assert_eq!(cpu_load((0, 0), (100, 100)), Some(0.0));
        assert_eq!(cpu_load((0, 0), (0, 100)), Some(100.0));
        // No time passed, or the counters went backwards: no verdict.
        assert_eq!(cpu_load((5, 5), (5, 5)), None);
        assert_eq!(cpu_load((50, 100), (10, 90)), None);
        // Idle growing faster than total (clock skew) is clamped, not negative.
        assert_eq!(cpu_load((0, 0), (200, 100)), Some(0.0));
    }

    #[test]
    fn process_match_is_case_insensitive() {
        let running = names(&["explorer.exe", "ffmpeg.exe", "code.exe"]);
        assert_eq!(
            find_blocking_process(&running, &names(&["FFmpeg.EXE"])),
            Some("ffmpeg.exe".to_string())
        );
    }

    #[test]
    fn process_match_requires_exact_name() {
        let running = names(&["ffmpeg-helper.exe", "notffmpeg.exe"]);
        assert_eq!(
            find_blocking_process(&running, &names(&["ffmpeg.exe"])),
            None
        );
    }

    #[test]
    fn empty_process_list_matches_nothing() {
        let running = names(&["explorer.exe"]);
        assert_eq!(find_blocking_process(&running, &[]), None);
    }

    #[test]
    fn filetime_combines_high_and_low_words() {
        let ft = FILETIME {
            dwLowDateTime: 0x0000_0002,
            dwHighDateTime: 0x0000_0001,
        };
        assert_eq!(filetime_u64(ft), 0x0000_0001_0000_0002);
    }

    #[test]
    fn guards_with_everything_off_never_block() {
        let mut guards = WinGuards::new(GuardsConfig {
            cpu_above_percent: 0.0,
            audio_playing: false,
            fullscreen: false,
            processes: Vec::new(),
        });
        assert_eq!(guards.blocker(), None);
        assert_eq!(guards.blocker(), None);
    }

    #[test]
    fn guard_update_swaps_config() {
        let off = GuardsConfig {
            cpu_above_percent: 0.0,
            audio_playing: false,
            fullscreen: false,
            processes: Vec::new(),
        };
        let mut guards = WinGuards::new(off.clone());
        // `System` is always running on Windows, so this must block...
        guards.update(GuardsConfig {
            processes: names(&["System"]),
            ..off.clone()
        });
        assert_eq!(guards.blocker(), Some(Block::Process("system".to_string())));
        // ...and clearing the list unblocks again.
        guards.update(off);
        assert_eq!(guards.blocker(), None);
    }

    #[test]
    fn dry_run_shutdowner_does_nothing_for_every_action() {
        for action in [
            Action::Shutdown,
            Action::Hibernate,
            Action::Sleep,
            Action::Log,
        ] {
            assert!(DryRunShutdowner.shutdown(action).is_ok());
        }
    }

    #[test]
    fn system_clock_is_monotonic() {
        let clock = SystemClock::new();
        let a = clock.now();
        let b = clock.now();
        assert!(b >= a);
    }

    #[test]
    fn idle_source_reads_without_panicking() {
        // The value depends on the machine; only check it is sane (< 1 year).
        assert!(WinIdle.idle_for() < Duration::from_secs(365 * 24 * 3600));
    }

    #[test]
    fn user_locale_is_a_locale_name_or_empty() {
        let l = user_locale();
        assert!(l.is_empty() || l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }
}

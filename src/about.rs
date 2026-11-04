use std::cell::Cell;
use std::process::Command;
use std::sync::{Arc, Mutex};

use anyhow::{ensure, Context, Result};
use tao::dpi::LogicalSize;
use tao::event_loop::EventLoopWindowTarget;
use tao::platform::windows::WindowExtWindows;
use tao::window::{Window, WindowBuilder};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, SetFocus, VK_ESCAPE, VK_RETURN, VK_SHIFT, VK_TAB,
};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::{config::Paths, i18n::Translator, version};

const SUBCLASS: usize = 0x41424f55;
const TIMER: usize = 0x41424f55;
const REPOSITORY: usize = 101;
const RELEASES: usize = 102;
const CLOSE: usize = 103;

pub struct AboutWindow {
    // Native state is detached and its children destroyed before fonts are freed.
    _native: Box<NativeAbout>,
    pub window: Window,
}

struct NativeAbout {
    host: HWND,
    attached: Cell<bool>,
    destroyed: Cell<bool>,
    children: Vec<HWND>,
    fonts: Vec<HFONT>,
    status: HWND,
    newer: Cell<bool>,
    tr: Translator,
    result: Arc<Mutex<Option<Result<Option<String>>>>>,
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

fn trusted_link(url: &str) -> bool {
    url == version::REPO || url == format!("{}/releases", version::REPO)
}

fn open_link(url: &str) {
    if trusted_link(url) {
        if let Err(error) = Command::new("explorer.exe").arg(url).spawn() {
            tracing::error!(%error, "open About link");
        }
    }
}

impl Drop for NativeAbout {
    fn drop(&mut self) {
        unsafe {
            if self.attached.replace(false) {
                let _ = KillTimer(Some(self.host), TIMER);
                let _ = RemoveWindowSubclass(self.host, Some(about_proc), SUBCLASS);
            }
            // The host may already have received WM_NCDESTROY.
            for child in self.children.drain(..) {
                if !self.destroyed.get() {
                    let _ = DestroyWindow(child);
                }
            }
            for font in self.fonts.drain(..) {
                let _ = DeleteObject(font.into());
            }
        }
    }
}

unsafe extern "system" fn button_proc(
    hwnd: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
    _id: usize,
    data: usize,
) -> LRESULT {
    let host = HWND(data as *mut _);
    if message == WM_KEYDOWN {
        if wp.0 == VK_TAB.0 as usize {
            if let Ok(next) =
                GetNextDlgTabItem(host, Some(hwnd), GetKeyState(VK_SHIFT.0 as i32) < 0)
            {
                let _ = SetFocus(Some(next));
            }
            return LRESULT(0);
        }
        if wp.0 == VK_ESCAPE.0 as usize {
            let _ = PostMessageW(Some(host), WM_CLOSE, WPARAM(0), LPARAM(0));
            return LRESULT(0);
        }
        if wp.0 == VK_RETURN.0 as usize {
            SendMessageW(hwnd, BM_CLICK, None, None);
            return LRESULT(0);
        }
    }
    if message == WM_NCDESTROY {
        let _ = RemoveWindowSubclass(hwnd, Some(button_proc), SUBCLASS);
    }
    DefSubclassProc(hwnd, message, wp, lp)
}

unsafe extern "system" fn about_proc(
    hwnd: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
    _id: usize,
    data: usize,
) -> LRESULT {
    // Box stays at a stable address until Drop removes this subclass. Only the
    // owning GUI thread accesses it; the network thread holds just the Arc result.
    let state = &*(data as *const NativeAbout);
    match message {
        WM_SETFOCUS => {
            if let Ok(first) = GetNextDlgTabItem(hwnd, None, false) {
                let _ = SetFocus(Some(first));
            }
            return LRESULT(0);
        }
        WM_COMMAND => {
            if wp.0 >> 16 == BN_CLICKED as usize {
                match wp.0 & 0xffff {
                    REPOSITORY => open_link(version::REPO),
                    RELEASES => open_link(&format!("{}/releases", version::REPO)),
                    CLOSE => {
                        let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                    }
                    _ => return DefSubclassProc(hwnd, message, wp, lp),
                }
                return LRESULT(0);
            }
        }
        WM_TIMER if wp.0 == TIMER => {
            let result = state.result.lock().ok().and_then(|mut slot| slot.take());
            if let Some(result) = result {
                let _ = KillTimer(Some(hwnd), TIMER);
                let text = match result {
                    Ok(Some(latest)) => {
                        state.newer.set(true);
                        SendMessageW(
                            state.status,
                            WM_SETFONT,
                            Some(WPARAM(state.fonts[1].0 as usize)),
                            Some(LPARAM(1)),
                        );
                        state.tr.tr("about.update", &[("version", &latest)])
                    }
                    Ok(None) => state.tr.t("about.current"),
                    Err(error) => {
                        tracing::debug!(%error, "About update check failed");
                        state.tr.t("about.update_error")
                    }
                };
                let _ = SetWindowTextW(state.status, PCWSTR(wide(&text).as_ptr()));
                let _ = InvalidateRect(Some(state.status), None, true);
            }
            return LRESULT(0);
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
            let dc = HDC(wp.0 as *mut _);
            let child = HWND(lp.0 as *mut _);
            let color = if child == state.status && state.newer.get() {
                COLORREF(0x008000)
            } else if GetDlgCtrlID(child) == REPOSITORY as i32
                || GetDlgCtrlID(child) == RELEASES as i32
            {
                COLORREF(GetSysColor(COLOR_HOTLIGHT))
            } else {
                COLORREF(GetSysColor(COLOR_WINDOWTEXT))
            };
            SetTextColor(dc, color);
            SetBkColor(dc, COLORREF(GetSysColor(COLOR_WINDOW)));
            return LRESULT(GetSysColorBrush(COLOR_WINDOW).0 as isize);
        }
        WM_ERASEBKGND => {
            let mut rect = Default::default();
            let _ = GetClientRect(hwnd, &mut rect);
            FillRect(HDC(wp.0 as *mut _), &rect, GetSysColorBrush(COLOR_WINDOW));
            return LRESULT(1);
        }
        WM_NCDESTROY => {
            state.destroyed.set(true);
            state.attached.set(false);
            let _ = KillTimer(Some(hwnd), TIMER);
            let _ = RemoveWindowSubclass(hwnd, Some(about_proc), SUBCLASS);
        }
        _ => {}
    }
    DefSubclassProc(hwnd, message, wp, lp)
}

impl AboutWindow {
    pub fn open<T: 'static>(
        target: &EventLoopWindowTarget<T>,
        tr: &Translator,
        paths: &Paths,
        dry_run: bool,
    ) -> Result<Self> {
        let window = WindowBuilder::new()
            .with_title(tr.t("about.title"))
            .with_inner_size(LogicalSize::new(660.0, 590.0))
            .with_resizable(false)
            .with_visible(false)
            .build(target)
            .context("about window")?;
        let scale = window.scale_factor();
        let px = |value: i32| (value as f64 * scale).round() as i32;
        let mut native = Box::new(NativeAbout {
            host: HWND(window.hwnd() as *mut _),
            attached: Cell::new(false),
            destroyed: Cell::new(false),
            children: Vec::new(),
            fonts: Vec::new(),
            status: HWND::default(),
            newer: Cell::new(false),
            tr: tr.clone(),
            result: Arc::new(Mutex::new(None)),
        });
        unsafe {
            for (size, weight, underline) in
                [(16, 400, 0), (16, 700, 0), (16, 400, 1), (26, 700, 0)]
            {
                let font = CreateFontW(
                    -px(size),
                    0,
                    0,
                    0,
                    weight,
                    0,
                    underline,
                    0,
                    DEFAULT_CHARSET,
                    OUT_DEFAULT_PRECIS,
                    CLIP_DEFAULT_PRECIS,
                    CLEARTYPE_QUALITY,
                    0,
                    w!("Segoe UI"),
                );
                ensure!(!font.0.is_null(), "create About font");
                native.fonts.push(font);
            }
            let mut add =
                |text: &str, y: i32, height: i32, font: usize, id: usize| -> Result<HWND> {
                    let button = id != 0;
                    let style = if button {
                        WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_FLAT as u32)
                    } else {
                        WS_CHILD | WS_VISIBLE | WINDOW_STYLE(0x0080) // SS_NOPREFIX
                    };
                    let child = CreateWindowExW(
                        WINDOW_EX_STYLE::default(),
                        if button { w!("BUTTON") } else { w!("STATIC") },
                        PCWSTR(wide(text).as_ptr()),
                        style,
                        px(24),
                        px(y),
                        px(612),
                        px(height),
                        Some(native.host),
                        Some(HMENU(id as *mut _)),
                        None,
                        None,
                    )
                    .context("create native About control")?;
                    native.children.push(child);
                    if button {
                        ensure!(
                            SetWindowSubclass(
                                child,
                                Some(button_proc),
                                SUBCLASS,
                                native.host.0 as usize
                            )
                            .as_bool(),
                            "subclass About button"
                        );
                    }
                    SendMessageW(
                        child,
                        WM_SETFONT,
                        Some(WPARAM(native.fonts[font].0 as usize)),
                        Some(LPARAM(0)),
                    );
                    Ok(child)
                };
            add("LightsOut", 20, 38, 3, 0)?;
            add(
                &format!("{}: {}", tr.t("about.version"), version::VERSION),
                70,
                24,
                1,
                0,
            )?;
            add(
                &format!("{}: {}", tr.t("about.build"), version::BUILD_DATE),
                100,
                24,
                0,
                0,
            )?;
            add(
                &format!("{}: {}", tr.t("about.git"), version::GIT),
                130,
                24,
                0,
                0,
            )?;
            add(&tr.t("about.repository"), 172, 22, 1, 0)?;
            add(version::REPO, 196, 28, 2, REPOSITORY)?;
            add(&tr.t("about.releases"), 236, 22, 1, 0)?;
            add(&format!("{}/releases", version::REPO), 260, 28, 2, RELEASES)?;
            let status = add(&tr.t("about.checking"), 302, 26, 0, 0)?;
            add(
                &format!("{}: {}", tr.t("about.config"), paths.config_file.display()),
                344,
                54,
                0,
                0,
            )?;
            add(
                &format!("{}: {}", tr.t("about.logs"), paths.log_dir.display()),
                402,
                54,
                0,
                0,
            )?;
            if dry_run {
                add(&tr.t("about.dry_run"), 466, 28, 0, 0)?;
            }
            add(&tr.t("settings.close"), 524, 36, 0, CLOSE)?;
            native.status = status;
            ensure!(
                SetWindowSubclass(
                    native.host,
                    Some(about_proc),
                    SUBCLASS,
                    (&*native as *const NativeAbout) as usize
                )
                .as_bool(),
                "subclass About host"
            );
            native.attached.set(true);
            ensure!(
                SetTimer(Some(native.host), TIMER, 100, None) != 0,
                "start About update timer"
            );
        }
        let result = Arc::clone(&native.result);
        // No window handles or native state escape to the worker. Closing About
        // simply drops its Arc; a late response is never sent to a reused HWND.
        std::thread::Builder::new()
            .name("about-update".into())
            .spawn(move || {
                let checked = crate::updates::check_for_updates(version::VERSION);
                if let Ok(mut slot) = result.lock() {
                    *slot = Some(checked);
                }
            })
            .context("start About update check")?;
        window.set_visible(true);
        Ok(Self {
            _native: native,
            window,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_navigation_is_scoped_to_exact_repository() {
        assert!(trusted_link(version::REPO));
        assert!(trusted_link(&format!("{}/releases", version::REPO)));
        for url in [
            "https://github.com/mushketer888/LightsOut-other/releases/latest",
            "https://github.com/mushketer888/LightsOut.evil/releases/latest",
            "https://github.com/mushketer888/LightsOut/releases/../../other",
            "https://github.com.evil/mushketer888/LightsOut/releases/latest",
            "javascript:alert(1)",
            "file:///C:/Windows/notepad.exe",
            "https://evil.example/",
        ] {
            assert!(!trusted_link(url), "{url}");
        }
    }
}

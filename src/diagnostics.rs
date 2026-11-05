use anyhow::{ensure, Context, Result};
use tao::dpi::LogicalSize;
use tao::event_loop::EventLoopWindowTarget;
use tao::platform::windows::WindowExtWindows;
use tao::window::{Window, WindowBuilder};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::Config;
use crate::core::{Action, IdleSample};
use crate::i18n::Translator;
use crate::platform::windows::{GuardReading, GuardSnapshot};
use std::time::Duration;

pub struct Snapshot {
    pub status: String,
    pub input: Option<(IdleSample, Duration)>,
    pub countdown: Option<Duration>,
    pub held: bool,
    pub snooze: Option<Duration>,
    pub guards: Option<GuardSnapshot>,
}

fn duration(value: Duration) -> String {
    let seconds = value.as_secs() + u64::from(value.subsec_nanos() != 0);
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

fn reading(value: Option<&GuardReading>, tr: &Translator) -> String {
    match value {
        Some(GuardReading::Disabled) => tr.t("diagnostics.disabled"),
        Some(GuardReading::Clear) => tr.t("diagnostics.clear"),
        Some(GuardReading::Busy(block)) => tr.describe_block(block),
        Some(GuardReading::Unavailable) => tr.t("diagnostics.unavailable"),
        None => tr.t("diagnostics.waiting"),
    }
}

impl Snapshot {
    pub fn text(&self, cfg: &Config, tr: &Translator) -> String {
        let mut lines = vec![self.status.clone(), String::new()];
        let action = tr.t(match cfg.action {
            Action::Shutdown => "action.shutdown",
            Action::Hibernate => "action.hibernate",
            Action::Sleep => "action.sleep",
            Action::Log => "action.log",
        });
        lines.push(tr.tr("diagnostics.action", &[("action", &action)]));
        let idle = match self.input {
            Some((input, _)) if input.last_input_id.is_some() => duration(input.idle_for),
            Some(_) => tr.t("diagnostics.unavailable"),
            None => tr.t("diagnostics.waiting"),
        };
        lines.push(tr.tr(
            "diagnostics.idle",
            &[
                ("idle", &idle),
                ("limit", &duration(cfg.settings().idle_threshold)),
            ],
        ));
        if let Some((_, age)) = self.input {
            lines.push(tr.tr("diagnostics.age", &[("age", &duration(age))]));
        }
        if let Some(remaining) = self.snooze {
            lines.push(tr.tr("diagnostics.snooze", &[("remaining", &duration(remaining))]));
        }
        if let Some(remaining) = self.countdown {
            lines.push(if self.held {
                tr.t("diagnostics.held")
            } else {
                tr.tr(
                    "diagnostics.countdown",
                    &[("remaining", &duration(remaining))],
                )
            });
        }
        lines.push(String::new());
        let guards = self.guards.as_ref();
        let cpu = guards
            .and_then(|g| g.cpu_percent)
            .map(|p| format!("{p:.1}%"))
            .unwrap_or_else(|| tr.t("diagnostics.unavailable"));
        lines.push(tr.tr(
            "diagnostics.cpu",
            &[
                ("load", &cpu),
                ("threshold", &cfg.guards.cpu_above_percent.to_string()),
                ("state", &reading(guards.map(|g| &g.cpu), tr)),
            ],
        ));
        for (key, value) in [
            ("diagnostics.audio", guards.map(|g| &g.audio)),
            ("diagnostics.fullscreen", guards.map(|g| &g.fullscreen)),
            ("diagnostics.processes", guards.map(|g| &g.processes)),
        ] {
            lines.push(tr.tr(key, &[("state", &reading(value, tr))]));
        }
        lines.push(String::new());
        lines.push(tr.tr(
            "diagnostics.refresh",
            &[("seconds", &cfg.poll_seconds.to_string())],
        ));
        lines.join("\r\n")
    }
}

/// Read-only native text: selectable/copyable, with no WebView profile or network.
pub struct DiagnosticsWindow {
    edit: HWND,
    font: HFONT,
    text: String,
    pub window: Window,
}

impl DiagnosticsWindow {
    pub fn open<T: 'static>(target: &EventLoopWindowTarget<T>, tr: &Translator) -> Result<Self> {
        let window = WindowBuilder::new()
            .with_title(tr.t("diagnostics.title"))
            .with_inner_size(LogicalSize::new(660.0, 460.0))
            .with_resizable(false)
            .with_visible(false)
            .build(target)
            .context("diagnostics window")?;
        let scale = window.scale_factor();
        let px = |n: i32| (n as f64 * scale).round() as i32;
        unsafe {
            let edit = CreateWindowExW(
                WS_EX_CLIENTEDGE,
                w!("EDIT"),
                w!(""),
                WS_CHILD
                    | WS_VISIBLE
                    | WS_TABSTOP
                    | WS_VSCROLL
                    | WINDOW_STYLE(
                        ES_MULTILINE as u32 | ES_READONLY as u32 | ES_AUTOVSCROLL as u32,
                    ),
                px(16),
                px(16),
                px(628),
                px(428),
                Some(HWND(window.hwnd() as *mut _)),
                None,
                None,
                None,
            )
            .context("diagnostics text control")?;
            let font = CreateFontW(
                -px(16),
                0,
                0,
                0,
                400,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                CLEARTYPE_QUALITY,
                0,
                w!("Segoe UI"),
            );
            if font.0.is_null() {
                let _ = DestroyWindow(edit);
            }
            ensure!(!font.0.is_null(), "diagnostics font");
            SendMessageW(
                edit,
                WM_SETFONT,
                Some(WPARAM(font.0 as usize)),
                Some(LPARAM(0)),
            );
            let mut view = Self {
                edit,
                font,
                text: String::new(),
                window,
            };
            view.update(&tr.t("diagnostics.title"), &tr.t("diagnostics.waiting"))?;
            view.window.set_visible(true);
            Ok(view)
        }
    }

    pub fn update(&mut self, title: &str, text: &str) -> Result<()> {
        self.window.set_title(title);
        if self.text != text {
            let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
            unsafe { SetWindowTextW(self.edit, PCWSTR(wide.as_ptr())) }
                .context("update diagnostics text")?;
            self.text = text.to_owned();
        }
        Ok(())
    }
}

impl Drop for DiagnosticsWindow {
    fn drop(&mut self) {
        // The Tao window is still owned here; release its child before the font.
        unsafe {
            let _ = DestroyWindow(self.edit);
            let _ = DeleteObject(self.font.into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Block;

    #[test]
    fn snapshot_explains_all_guards_modes_and_timers_in_both_languages() {
        for language in ["en", "ru"] {
            let tr = Translator::new(language);
            let cfg = Config {
                action: Action::Log,
                ..Config::default()
            };
            let mut snapshot = Snapshot {
                status: "LightsOut [dry-run]".into(),
                input: Some((
                    IdleSample {
                        idle_for: Duration::from_secs(2820),
                        last_input_id: Some(1),
                    },
                    Duration::from_secs(3),
                )),
                countdown: Some(Duration::from_millis(41200)),
                held: false,
                snooze: Some(Duration::from_secs(120)),
                guards: Some(GuardSnapshot {
                    cpu_percent: Some(38.0),
                    cpu: GuardReading::Busy(Block::Cpu(38)),
                    audio: GuardReading::Disabled,
                    fullscreen: GuardReading::Unavailable,
                    processes: GuardReading::Busy(Block::Process("blender.exe".into())),
                }),
            };
            let text = snapshot.text(&cfg, &tr);
            for expected in [
                "[dry-run]",
                "00:47:00",
                "02:00:00",
                "00:00:42",
                "00:02:00",
                "38.0%",
                "blender.exe",
            ] {
                assert!(text.contains(expected), "{language}: {expected}: {text}");
            }
            assert!(text.contains(&tr.t("action.log")));
            assert!(text.contains(&tr.t("diagnostics.disabled")));
            assert!(text.contains(&tr.t("diagnostics.unavailable")));
            assert!(!text.contains('{'));
            snapshot.held = true;
            let text = snapshot.text(&cfg, &tr);
            assert!(text.contains(&tr.t("diagnostics.held")));
            assert!(!text.contains("00:00:42"));
            snapshot.input = None;
            snapshot.guards = None;
            assert!(snapshot
                .text(&cfg, &tr)
                .contains(&tr.t("diagnostics.waiting")));
            snapshot.input = Some((
                IdleSample {
                    idle_for: Duration::ZERO,
                    last_input_id: None,
                },
                Duration::ZERO,
            ));
            assert!(snapshot
                .text(&cfg, &tr)
                .contains(&tr.t("diagnostics.unavailable")));
        }
    }
}

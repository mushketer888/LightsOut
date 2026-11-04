//! Application: polling thread + tray + config hot-reload.

use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use notify::{EventKind, RecursiveMode, Watcher};
use tao::dpi::LogicalSize;
use tao::event::{Event as TaoEvent, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget};
use tao::window::{Window, WindowBuilder};
use tauri_winrt_notification::Toast;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder};
use wry::{WebContext, WebView, WebViewBuilder};

use crate::about::AboutWindow;
use crate::config::{Config, Paths};
use crate::core::{Action, Event, Machine, Shutdowner, State};
use crate::i18n::{Translator, LANGUAGES};
use crate::platform::windows::{
    self as win, DryRunShutdowner, SystemClock, WinGuards, WinIdle, WinShutdowner,
};
use crate::version;

pub struct Options {
    pub dry_run: bool,
    pub paths: Paths,
}

/// Commands from the UI/watcher to the polling thread.
pub enum Control {
    Pause(bool),
    Snooze(Duration),
    Unsnooze,
    ShutdownNow {
        expected_action: Action,
        revision: u64,
    },
    Reload,
    Save {
        config: serde_json::Value,
        revision: u64,
        request: u64,
    },
    Quit,
}

enum UserEvent {
    Menu(MenuEvent),
    Status(Status),
    Applied {
        config: Config,
        revision: u64,
        request: Option<u64>,
    },
    SaveFailed {
        request: u64,
        error: String,
    },
    /// Raw JSON message from the settings page.
    Settings(String),
}

/// What the polling thread tells the tray.
struct Status {
    text: String,
    /// Manually paused: the tray shows a pause badge.
    paused: bool,
    /// Snoozed: the tray shows a snooze (moon) badge.
    snoozed: bool,
}

/// The settings dialog: a WebView2 page in its own window.
/// Field order matters: the webview must drop before the window and the web context.
struct SettingsWindow {
    webview: WebView,
    window: Window,
    _context: WebContext,
    pending: Option<u64>,
}

impl SettingsWindow {
    fn open(
        target: &EventLoopWindowTarget<UserEvent>,
        proxy: EventLoopProxy<UserEvent>,
        cfg: &Config,
        revision: u64,
        tr: &Translator,
        data_dir: PathBuf,
    ) -> Result<Self> {
        let window = WindowBuilder::new()
            .with_title(tr.t("settings.window_title"))
            .with_inner_size(LogicalSize::new(480.0, 760.0))
            .build(target)
            .context("settings window")?;
        let mut context = WebContext::new(Some(data_dir));
        let languages: Vec<_> = LANGUAGES
            .iter()
            .map(|l| serde_json::json!({ "code": l.code, "name": l.name }))
            .collect();
        let init = format!(
            "window.__CONFIG__ = {}; window.__I18N__ = {}; window.__LANGS__ = {}; window.__REVISION__ = {revision};",
            serde_json::to_string(cfg)?,
            serde_json::to_string(&tr.section("settings"))?,
            serde_json::to_string(&languages)?,
        );
        let webview = WebViewBuilder::new_with_web_context(&mut context)
            .with_html(include_str!("../ui/settings.html"))
            .with_initialization_script(init)
            .with_ipc_handler(move |req| {
                let _ = proxy.send_event(UserEvent::Settings(req.into_body()));
            })
            .build(&window)
            .context("WebView2 is not available")?;
        Ok(Self {
            webview,
            window,
            _context: context,
            pending: None,
        })
    }

    fn report(&self, ok: bool, text: &str) {
        let text = serde_json::to_string(text).unwrap_or_else(|_| String::from("\"\""));
        let _ = self
            .webview
            .evaluate_script(&format!("showResult({ok}, {text})"));
    }
}

/// Validates and atomically writes the config (with its comments).
fn save_config(path: &PathBuf, json: &serde_json::Value) -> Result<Config> {
    let cfg: Config = serde_json::from_value(json.clone()).context("invalid settings")?;
    cfg.validate()?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, cfg.to_commented_toml())?;
    std::fs::rename(&tmp, path)?;
    Ok(cfg)
}

/// Shared worker handlers: the acknowledgement is produced only after apply succeeds.
fn apply_config(
    current: &mut Config,
    revision: &mut u64,
    new: Config,
    request: Option<u64>,
    apply: impl FnOnce(&Config) -> Result<()>,
) -> Result<UserEvent> {
    if new != *current {
        let next = revision
            .checked_add(1)
            .context("config revision exhausted")?;
        apply(&new)?;
        *current = new;
        *revision = next;
    }
    // Do not deduplicate against status/language: guards and action may change neither.
    Ok(UserEvent::Applied {
        config: current.clone(),
        revision: *revision,
        request,
    })
}

fn handle_save(
    path: &PathBuf,
    json: &serde_json::Value,
    (expected_revision, repair_snapshot): (u64, Option<&[u8]>),
    request: u64,
    current: &mut Config,
    revision: &mut u64,
    apply: impl FnOnce(&Config) -> Result<()>,
) -> Result<UserEvent> {
    anyhow::ensure!(
        expected_revision == *revision,
        "Settings changed; close and reopen Settings before saving."
    );
    // Also catch edits not yet delivered by the optional watcher. Never recreate a missing file.
    let disk = std::fs::read(path)?;
    if repair_snapshot != Some(disk.as_slice()) {
        let disk = Config::parse(std::str::from_utf8(&disk)?)?;
        anyhow::ensure!(
            disk == *current,
            "Config file changed; close and reopen Settings after reload before saving."
        );
    }
    let new = save_config(path, json)?;
    apply_config(current, revision, new, Some(request), apply)
}

fn confirm_action(
    config: &Config,
    revision: u64,
    expected_action: Action,
    expected_revision: u64,
    issue: impl FnOnce() -> Result<()>,
) -> Result<()> {
    anyhow::ensure!(
        config.action == expected_action && revision == expected_revision,
        "Settings changed; confirm the current action again."
    );
    issue()
}

fn send_save(
    tx: &Sender<Control>,
    config: serde_json::Value,
    revision: u64,
    request: u64,
) -> Result<()> {
    tx.send(Control::Save {
        config,
        revision,
        request,
    })
    .context("settings worker is unavailable")
}

fn action_word(tr: &Translator, action: Action) -> String {
    tr.t(match action {
        Action::Shutdown => "action.shutdown",
        Action::Hibernate => "action.hibernate",
        Action::Sleep => "action.sleep",
        Action::Log => "action.log",
    })
}

/// "Shutdown - now" menu label: follows the configured action, first letter
/// uppercased (locale values stay lowercase — they are reused in toasts).
fn now_label(tr: &Translator, action: Action) -> String {
    let word = action_word(tr, action);
    let mut chars = word.chars();
    let cap = match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => word,
    };
    tr.tr("menu.now", &[("action", &cap)])
}

fn toast(title: &str, text: &str) {
    if let Err(e) = Toast::new(Toast::POWERSHELL_APP_ID)
        .title(title)
        .text1(text)
        .show()
    {
        tracing::warn!("toast not shown: {e}");
    }
}

fn spawn_worker(
    mut cfg: Config,
    config_file: PathBuf,
    mut repair_snapshot: Option<Vec<u8>>,
    dry_run: bool,
    rx: Receiver<Control>,
    publish: impl Fn(UserEvent) + Send + 'static,
) -> JoinHandle<()> {
    thread::spawn(move || {
        if let Err(e) = win::init_com() {
            tracing::warn!("COM init failed, the audio guard will not work: {e:#}");
        }
        let locale = win::user_locale();
        let mut tr = Translator::for_setting(&cfg.language, &locale);
        let shutdowner: Box<dyn Shutdowner> = if dry_run {
            Box::new(DryRunShutdowner)
        } else {
            Box::new(WinShutdowner)
        };
        let mut machine = Machine::new(
            cfg.settings(),
            WinIdle,
            shutdowner,
            SystemClock::new(),
            WinGuards::new(cfg.guards.clone()),
        );

        let mut manual_pause = false;
        let mut snooze_until: Option<Instant> = None;
        let mut last_status = String::new();
        let mut last_lang = "";
        let mut last_block = None;
        let mut revision = 0;
        publish(UserEvent::Applied {
            config: cfg.clone(),
            revision,
            request: None,
        });

        loop {
            // Publish first, so the tray is correct right after start and after every event.
            let text = status_text(&machine, &tr, manual_pause, snooze_until, dry_run);
            if text != last_status || tr.code() != last_lang {
                publish(UserEvent::Status(Status {
                    text: text.clone(),
                    paused: manual_pause,
                    snoozed: snooze_until.is_some(),
                }));
                last_status = text;
                last_lang = tr.code();
            }

            let wait = if matches!(machine.state(), State::Warning { .. }) {
                cfg.poll_interval().min(Duration::from_secs(1))
            } else {
                cfg.poll_interval()
            };

            match rx.recv_timeout(wait) {
                Ok(Control::Quit) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(Control::Pause(p)) => {
                    manual_pause = p;
                    if !p {
                        snooze_until = None;
                    }
                    tracing::info!(paused = p, "pause toggled");
                }
                Ok(Control::Snooze(d)) => {
                    manual_pause = false;
                    snooze_until = Some(Instant::now() + d);
                    tracing::info!(?d, "snooze");
                }
                Ok(Control::Unsnooze) => {
                    snooze_until = None;
                    tracing::info!("snooze cancelled");
                }
                Ok(Control::ShutdownNow {
                    expected_action,
                    revision: expected_revision,
                }) => {
                    tracing::info!("Shutdown now from the menu");
                    if let Err(e) =
                        confirm_action(&cfg, revision, expected_action, expected_revision, || {
                            machine.shutdown_now()
                        })
                    {
                        tracing::error!("shutdown failed: {e:#}");
                        toast(
                            "LightsOut",
                            &tr.tr("toast.failed", &[("error", &format!("{e:#}"))]),
                        );
                    }
                }
                Ok(command @ (Control::Reload | Control::Save { .. })) => {
                    let old_autostart = cfg.autostart;
                    let apply = |new: &Config| -> Result<()> {
                        if !dry_run && new.autostart != old_autostart {
                            win::set_autostart(new.autostart)?;
                        }
                        machine.set_settings(new.settings());
                        machine.guard_mut().update(new.guards.clone());
                        tr = Translator::for_setting(&new.language, &locale);
                        Ok(())
                    };
                    let request = match &command {
                        Control::Save { request, .. } => Some(*request),
                        _ => None,
                    };
                    let result = match command {
                        Control::Reload => std::fs::read_to_string(&config_file)
                            .map_err(anyhow::Error::from)
                            .and_then(|text| Config::parse(&text))
                            .and_then(|new| {
                                apply_config(&mut cfg, &mut revision, new, None, apply)
                            }),
                        Control::Save {
                            config,
                            revision: expected,
                            request,
                        } => handle_save(
                            &config_file,
                            &config,
                            (expected, repair_snapshot.as_deref()),
                            request,
                            &mut cfg,
                            &mut revision,
                            apply,
                        ),
                        _ => unreachable!(),
                    };
                    match result {
                        Ok(event) => {
                            repair_snapshot = None;
                            publish(event);
                        }
                        Err(e) => {
                            tracing::warn!("config not applied: {e:#}");
                            if let Some(request) = request {
                                publish(UserEvent::SaveFailed {
                                    request,
                                    error: format!("{e:#}"),
                                });
                            }
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    let snoozed = snooze_until.is_some_and(|t| Instant::now() < t);
                    if !snoozed {
                        snooze_until = None;
                    }
                    machine.set_paused(manual_pause || snoozed);

                    match machine.tick() {
                        Ok(Event::WarningStarted) => {
                            let secs = cfg.warning_seconds;
                            tracing::warn!(secs, "no activity, warning the user");
                            let key = if cfg.action == Action::Log {
                                "toast.warning_log"
                            } else {
                                "toast.warning"
                            };
                            toast(
                                "LightsOut",
                                &tr.tr(
                                    key,
                                    &[
                                        ("action", &action_word(&tr, cfg.action)),
                                        ("secs", &secs.to_string()),
                                    ],
                                ),
                            );
                        }
                        Ok(Event::WarningCancelled) => {
                            tracing::info!("warning cancelled");
                            toast("LightsOut", &tr.t("toast.cancelled"));
                        }
                        Ok(Event::ShutdownIssued) => {
                            tracing::warn!("shutdown command issued")
                        }
                        Ok(Event::None) => {}
                        Err(e) => {
                            tracing::error!("shutdown failed: {e:#}");
                            toast(
                                "LightsOut",
                                &tr.tr("toast.failed", &[("error", &format!("{e:#}"))]),
                            );
                        }
                    }

                    let block = machine.blocked_by().cloned();
                    if block != last_block {
                        if let Some(reason) = &block {
                            tracing::info!(?reason, "shutdown held back by a guard");
                        }
                        last_block = block;
                    }
                }
            }
        }
        tracing::info!("polling thread stopped");
    })
}

fn status_text<I, S, C, G>(
    machine: &Machine<I, S, C, G>,
    tr: &Translator,
    paused: bool,
    snooze_until: Option<Instant>,
    dry_run: bool,
) -> String
where
    I: crate::core::IdleSource,
    S: Shutdowner,
    C: crate::core::Clock,
    G: crate::core::Guard,
{
    let mut s = String::from("LightsOut");
    if dry_run {
        s.push_str(" [dry-run]");
    }
    let log_only = machine.settings().action == Action::Log;
    if log_only {
        s.push_str(&format!(" [{}]", tr.t("status.log_only_tag")));
    }
    s.push_str(" - ");
    if paused {
        s.push_str(&tr.t("status.paused"));
    } else if snooze_until.is_some() {
        s.push_str(&tr.t("status.snoozed"));
    } else {
        match machine.state() {
            State::Warning { .. } => s.push_str(&tr.t("status.warning")),
            State::Shutdown if log_only => s.push_str(&tr.t("status.would_shutdown")),
            State::Shutdown => s.push_str(&tr.t("status.shutting_down")),
            State::Failed => s.push_str(&tr.t("status.failed")),
            State::Active => match machine.blocked_by() {
                Some(block) => {
                    s.push_str(&tr.tr("status.held_by", &[("reason", &tr.describe_block(block))]))
                }
                None => s.push_str(&tr.t("status.watching")),
            },
        }
    }
    s
}

/// Tray icons are embedded as resources 2-4 by build.rs (tray.svg variants with
/// badges baked in per size). Windows picks the crispest entry for the DPI,
/// instead of scaling one RGBA bitmap like `Icon::from_rgba` would.
fn tray_icon(paused: bool, snoozed: bool) -> Result<Icon> {
    let id: u16 = if paused {
        3
    } else if snoozed {
        4
    } else {
        2
    };
    Icon::from_resource(id, None).context("tray icon")
}

fn open_in_explorer(path: &PathBuf) {
    if let Err(e) = Command::new("explorer").arg(path).spawn() {
        tracing::error!("explorer: {e}");
    }
}

fn spawn_config_watcher(
    config_file: PathBuf,
    tx: Sender<Control>,
) -> Result<notify::RecommendedWatcher> {
    let dir = config_file
        .parent()
        .context("the config file has no parent folder")?
        .to_path_buf();
    let target = config_file.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(ev) = res else { return };
        if !matches!(ev.kind, EventKind::Modify(_) | EventKind::Create(_)) {
            return;
        }
        if !ev.paths.iter().any(|p| p.file_name() == target.file_name()) {
            return;
        }
        // Editors write the file in several chunks; let them finish.
        thread::sleep(Duration::from_millis(200));
        match std::fs::read_to_string(&target)
            .map_err(anyhow::Error::from)
            .and_then(|t| Config::parse(&t))
        {
            Ok(_) => {
                // Read again in the worker: queued snapshots must not undo a later save.
                let _ = tx.send(Control::Reload);
            }
            Err(e) => tracing::warn!("config not applied, keeping the old one: {e:#}"),
        }
    })?;
    watcher.watch(&dir, RecursiveMode::NonRecursive)?;
    Ok(watcher)
}

pub fn run(opts: Options) -> Result<()> {
    let Options { dry_run, paths } = opts;
    let locale = win::user_locale();

    let mut repair_snapshot = None;
    let cfg = match Config::load_or_create(&paths.config_file) {
        Ok(c) => c,
        Err(e) => {
            repair_snapshot = std::fs::read(&paths.config_file).ok().filter(|bytes| {
                std::str::from_utf8(bytes)
                    .ok()
                    .is_none_or(|text| Config::parse(text).is_err())
            });
            tracing::error!("config is broken, running on defaults: {e:#}");
            let tr = Translator::for_setting("auto", &locale);
            win::message_box(
                "LightsOut",
                &tr.tr("dialog.config_error", &[("error", &format!("{e:#}"))]),
            );
            Config::default()
        }
    };
    tracing::info!(version = %version::describe(), dry_run, ?cfg, "starting");
    let mut tr = Translator::for_setting(&cfg.language, &locale);

    // dry-run does not touch the user's registry.
    if !dry_run {
        if let Err(e) = win::set_autostart(cfg.autostart) {
            tracing::error!("autostart: {e:#}");
        }
    }

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let (tx, rx) = mpsc::channel();
    let status_proxy = proxy.clone();
    let cfg_for_ui = cfg.clone();
    let worker = spawn_worker(
        cfg,
        paths.config_file.clone(),
        repair_snapshot,
        dry_run,
        rx,
        move |event| {
            let _ = status_proxy.send_event(event);
        },
    );
    let _watcher = spawn_config_watcher(paths.config_file.clone(), tx.clone())
        .map_err(|e| tracing::warn!("hot-reload is not working: {e:#}"))
        .ok();

    let settings_proxy = proxy.clone();
    let menu_proxy = proxy;
    MenuEvent::set_event_handler(Some(move |e| {
        let _ = menu_proxy.send_event(UserEvent::Menu(e));
    }));

    let about_label = |tr: &Translator| tr.tr("menu.about", &[("version", version::VERSION)]);
    let pause = CheckMenuItem::new(tr.t("menu.pause"), true, false, None);
    let snooze = MenuItem::new(tr.t("menu.snooze"), true, None);
    let unsnooze = MenuItem::new(tr.t("menu.unsnooze"), false, None);
    let now = MenuItem::new(now_label(&tr, cfg_for_ui.action), true, None);
    let settings = MenuItem::new(tr.t("menu.settings"), true, None);
    let open_cfg = MenuItem::new(tr.t("menu.open_config"), true, None);
    let open_logs = MenuItem::new(tr.t("menu.open_logs"), true, None);
    let about = MenuItem::new(about_label(&tr), true, None);
    let quit = MenuItem::new(tr.t("menu.quit"), true, None);

    let menu = Menu::new();
    menu.append_items(&[
        &pause,
        &snooze,
        &unsnooze,
        &now,
        &PredefinedMenuItem::separator(),
        &settings,
        &open_cfg,
        &open_logs,
        &about,
        &PredefinedMenuItem::separator(),
        &quit,
    ])?;

    let icon_normal = tray_icon(false, false)?;
    let icon_paused = tray_icon(true, false)?;
    let icon_snooze = tray_icon(false, true)?;
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("LightsOut")
        .with_icon(icon_normal.clone())
        .build()
        .context("tray")?;

    let mut worker = Some(worker);
    let mut settings_win: Option<SettingsWindow> = None;
    let mut about_win: Option<AboutWindow> = None;
    let mut last_cfg = cfg_for_ui;
    let mut revision = 0;
    let mut next_request = 0_u64;
    let mut tray_badge = (false, false);
    event_loop.run(move |event, target, control_flow| {
        *control_flow = ControlFlow::Wait;
        let ev = match event {
            TaoEvent::WindowEvent {
                event: WindowEvent::CloseRequested,
                window_id,
                ..
            } => {
                if settings_win
                    .as_ref()
                    .is_some_and(|w| w.window.id() == window_id)
                {
                    settings_win = None;
                }
                if about_win
                    .as_ref()
                    .is_some_and(|w| w.window.id() == window_id)
                {
                    about_win = None;
                }
                return;
            }
            TaoEvent::UserEvent(ev) => ev,
            _ => return,
        };
        match ev {
            UserEvent::Settings(raw) => {
                let Ok(msg) = serde_json::from_str::<serde_json::Value>(&raw) else {
                    return;
                };
                match msg["type"].as_str() {
                    Some("close") => settings_win = None,
                    Some("save") => {
                        if let Some(w) = &mut settings_win {
                            let Some(expected) = msg["revision"].as_u64() else {
                                return;
                            };
                            next_request += 1;
                            match send_save(&tx, msg["config"].clone(), expected, next_request) {
                                Ok(()) => w.pending = Some(next_request),
                                Err(e) => w.report(false, &format!("{e:#}")),
                            }
                        }
                    }
                    _ => {}
                }
            }
            UserEvent::Applied {
                config,
                revision: applied_revision,
                request,
            } => {
                last_cfg = config;
                revision = applied_revision;
                tr = Translator::for_setting(&last_cfg.language, &locale);
                pause.set_text(tr.t("menu.pause"));
                snooze.set_text(tr.t("menu.snooze"));
                unsnooze.set_text(tr.t("menu.unsnooze"));
                now.set_text(now_label(&tr, last_cfg.action));
                settings.set_text(tr.t("menu.settings"));
                open_cfg.set_text(tr.t("menu.open_config"));
                open_logs.set_text(tr.t("menu.open_logs"));
                about.set_text(about_label(&tr));
                quit.set_text(tr.t("menu.quit"));
                if let Some(w) = &mut settings_win {
                    if request.is_some() && request == w.pending {
                        w.pending = None;
                        let _ = w
                            .webview
                            .evaluate_script(&format!("window.__REVISION__ = {revision};"));
                        w.report(true, &tr.t("settings.saved"));
                    }
                }
            }
            UserEvent::SaveFailed { request, error } => {
                if let Some(w) = &mut settings_win {
                    if w.pending == Some(request) {
                        w.pending = None;
                        w.report(false, &error);
                    }
                }
            }
            UserEvent::Status(status) => {
                let _ = tray.set_tooltip(Some(status.text));
                let badge = (status.paused, status.snoozed);
                if badge != tray_badge {
                    tray_badge = badge;
                    let icon = if status.paused {
                        &icon_paused
                    } else if status.snoozed {
                        &icon_snooze
                    } else {
                        &icon_normal
                    };
                    let _ = tray.set_icon(Some(icon.clone()));
                }
                unsnooze.set_enabled(status.snoozed);
            }
            UserEvent::Menu(m) => {
                if m.id == *pause.id() {
                    let _ = tx.send(Control::Pause(pause.is_checked()));
                } else if m.id == *snooze.id() {
                    pause.set_checked(false);
                    let _ = tx.send(Control::Snooze(Duration::from_secs(3600)));
                } else if m.id == *unsnooze.id() {
                    let _ = tx.send(Control::Unsnooze);
                } else if m.id == *now.id() {
                    let title = now_label(&tr, last_cfg.action);
                    let text = tr.t("menu.shutdown_confirm");
                    let tx_confirm = tx.clone();
                    let expected_action = last_cfg.action;
                    let expected_revision = revision;
                    // MessageBox blocks, so keep it off the event loop thread.
                    thread::spawn(move || {
                        if win::confirm_dialog(&title, &text) {
                            let _ = tx_confirm.send(Control::ShutdownNow {
                                expected_action,
                                revision: expected_revision,
                            });
                        }
                    });
                } else if m.id == *settings.id() {
                    if let Some(w) = &settings_win {
                        w.window.set_focus();
                    } else {
                        match SettingsWindow::open(
                            target,
                            settings_proxy.clone(),
                            &last_cfg,
                            revision,
                            &tr,
                            paths.webview_dir.clone(),
                        ) {
                            Ok(w) => settings_win = Some(w),
                            Err(e) => {
                                // No WebView2: fall back to a plain editor.
                                tracing::error!("settings dialog failed: {e:#}");
                                let _ = Command::new("notepad").arg(&paths.config_file).spawn();
                            }
                        }
                    }
                } else if m.id == *open_cfg.id() {
                    let _ = Command::new("notepad").arg(&paths.config_file).spawn();
                } else if m.id == *open_logs.id() {
                    open_in_explorer(&paths.log_dir);
                } else if m.id == *about.id() {
                    if let Some(w) = &about_win {
                        w.window.set_focus();
                    } else {
                        match AboutWindow::open(target, &tr, &paths, dry_run) {
                            Ok(w) => about_win = Some(w),
                            Err(e) => {
                                tracing::warn!("About window unavailable: {e:#}");
                                let text = format!(
                                    "{}\n\n{}: {}\n{}: {}\n{}\n{}{}",
                                    version::describe(),
                                    tr.t("about.config"),
                                    paths.config_file.display(),
                                    tr.t("about.logs"),
                                    paths.log_dir.display(),
                                    version::REPO,
                                    tr.t("about.update_error"),
                                    if dry_run {
                                        format!("\n\n{}", tr.t("about.dry_run"))
                                    } else {
                                        String::new()
                                    },
                                );
                                let title = tr.t("about.title");
                                thread::spawn(move || win::message_box(&title, &text));
                            }
                        }
                    }
                } else if m.id == *quit.id() {
                    let _ = tx.send(Control::Quit);
                    if let Some(w) = worker.take() {
                        let _ = w.join();
                    }
                    *control_flow = ControlFlow::Exit;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    //! No real shutdown anywhere: the machine under test uses a fake `Shutdowner`.

    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;
    use crate::core::{Block, Clock, Guard, IdleSample, IdleSource, Settings};
    use crate::i18n::Translator;

    #[derive(Clone, Default)]
    struct Fake {
        idle: Rc<Cell<Duration>>,
        block: Rc<Cell<bool>>,
        calls: Rc<Cell<u32>>,
    }

    impl IdleSource for Fake {
        fn sample(&self) -> IdleSample {
            IdleSample {
                idle_for: self.idle.get(),
                last_input_id: Some(0),
            }
        }
    }
    impl Clock for Fake {
        fn now(&self) -> Duration {
            Duration::ZERO
        }
    }
    impl Guard for Fake {
        fn blocker(&mut self) -> Option<Block> {
            self.block.get().then(|| Block::Other("fake".to_string()))
        }
    }
    impl Shutdowner for Fake {
        fn shutdown(&self, _: Action) -> Result<()> {
            self.calls.set(self.calls.get() + 1);
            Ok(())
        }
    }

    fn en() -> Translator {
        Translator::new("en")
    }

    fn machine() -> (Fake, Machine<Fake, Fake, Fake, Fake>) {
        let f = Fake::default();
        let m = Machine::new(
            Settings {
                idle_threshold: Duration::from_secs(10),
                warning: Duration::from_secs(5),
                action: Action::Shutdown,
            },
            f.clone(),
            f.clone(),
            f.clone(),
            f.clone(),
        );
        (f, m)
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("lights-out-app-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn status_text_differs_for_every_state() {
        let (f, mut m) = machine();
        let watching = status_text(&m, &en(), false, None, false);

        let paused = status_text(&m, &en(), true, None, false);
        let snoozed = status_text(&m, &en(), false, Some(Instant::now()), false);

        f.block.set(true);
        m.tick().unwrap();
        let blocked = status_text(&m, &en(), false, None, false);
        f.block.set(false);

        f.idle.set(Duration::from_secs(20));
        m.tick().unwrap();
        let warning = status_text(&m, &en(), false, None, false);

        m.shutdown_now().unwrap();
        let shutdown = status_text(&m, &en(), false, None, false);

        let all = [watching, paused, snoozed, blocked, warning, shutdown];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn status_text_marks_dry_run() {
        let (_f, m) = machine();
        assert!(status_text(&m, &en(), false, None, true).contains("[dry-run]"));
        assert!(!status_text(&m, &en(), false, None, false).contains("[dry-run]"));
    }

    #[test]
    fn status_text_mentions_blocker_reason() {
        let (f, mut m) = machine();
        f.block.set(true);
        m.tick().unwrap();
        assert!(status_text(&m, &en(), false, None, false).contains("fake"));
    }

    #[test]
    fn save_config_writes_valid_file() {
        let dir = temp_dir("save-ok");
        let path = dir.join("config.toml");
        let json = serde_json::json!({
            "idle_minutes": 30,
            "action": "hibernate",
            "guards": { "processes": ["steam.exe"], "audio_playing": false }
        });
        save_config(&path, &json).unwrap();

        let saved = Config::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved.idle_minutes, 30);
        assert_eq!(saved.action, Action::Hibernate);
        assert_eq!(saved.guards.processes, ["steam.exe"]);
        assert!(!saved.guards.audio_playing);
        // Unspecified fields fall back to defaults.
        assert_eq!(saved.warning_seconds, Config::default().warning_seconds);
        // No temp file left behind.
        assert!(!path.with_extension("toml.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_config_rejects_invalid_and_keeps_old_file() {
        let dir = temp_dir("save-bad");
        let path = dir.join("config.toml");
        std::fs::write(&path, "idle_minutes = 99").unwrap();

        for bad in [
            serde_json::json!({ "idle_minutes": 0 }),
            serde_json::json!({ "action": "explode" }),
            serde_json::json!({ "poll_seconds": "soon" }),
        ] {
            assert!(save_config(&path, &bad).is_err());
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "idle_minutes = 99");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn confirmation_requires_both_action_and_revision_before_calling_core() {
        let (fake, mut machine) = machine();
        let cfg = Config::default();
        assert!(confirm_action(&cfg, 2, Action::Sleep, 2, || machine.shutdown_now()).is_err());
        assert!(confirm_action(&cfg, 2, Action::Shutdown, 1, || machine.shutdown_now()).is_err());
        assert_eq!(fake.calls.get(), 0);
        confirm_action(&cfg, 2, Action::Shutdown, 2, || machine.shutdown_now()).unwrap();
        assert_eq!(fake.calls.get(), 1);
    }

    #[test]
    fn save_without_watcher_applies_before_acknowledgement() {
        let dir = temp_dir("save-ack");
        let path = dir.join("config.toml");
        let mut cfg = Config::default();
        cfg.guards.fullscreen = true;
        std::fs::write(&path, cfg.to_commented_toml()).unwrap();
        let mut new = cfg.clone();
        new.action = Action::Log;
        let (tx, rx) = mpsc::channel();
        send_save(&tx, serde_json::to_value(&new).unwrap(), 0, 17).unwrap();
        let Control::Save {
            config,
            revision: expected,
            request,
        } = rx.recv().unwrap()
        else {
            panic!("expected Save")
        };
        let (fake, mut machine) = machine();
        let mut revision = 0;
        let applied = Cell::new(false);
        let event = handle_save(
            &path,
            &config,
            (expected, None),
            request,
            &mut cfg,
            &mut revision,
            |candidate| {
                assert_eq!(Config::parse(&std::fs::read_to_string(&path)?)?, *candidate);
                machine.set_settings(candidate.settings());
                assert!(candidate.guards.fullscreen);
                applied.set(true);
                Ok(())
            },
        )
        .unwrap();
        assert!(applied.get());
        let UserEvent::Applied {
            config,
            revision,
            request,
        } = event
        else {
            panic!("missing ack")
        };
        assert_eq!(request, Some(17));
        assert_eq!(revision, 1);
        assert_eq!(config, new);
        assert_eq!(machine.settings().action, Action::Log);
        machine.shutdown_now().unwrap();
        assert_eq!(
            fake.calls.get(),
            0,
            "log action must not reach the power sink"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stale_save_rejects_revision_and_unobserved_external_disk_edit() {
        let dir = temp_dir("save-conflict");
        let path = dir.join("config.toml");
        let mut cfg = Config::default();
        let mut revision = 4;
        let original = cfg.to_commented_toml();
        std::fs::write(&path, &original).unwrap();
        let json = serde_json::json!({ "action": "log" });
        assert!(handle_save(
            &path,
            &json,
            (3, None),
            1,
            &mut cfg,
            &mut revision,
            |_| panic!("stale apply")
        )
        .is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        let mut external = cfg.clone();
        external.action = Action::Sleep;
        let external_text = external.to_commented_toml();
        std::fs::write(&path, &external_text).unwrap();
        assert!(handle_save(
            &path,
            &json,
            (4, None),
            2,
            &mut cfg,
            &mut revision,
            |_| panic!("conflicting apply")
        )
        .is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), external_text);
        assert_eq!(revision, 4);
        assert_eq!(cfg.action, Action::Shutdown);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn settings_can_repair_only_the_unchanged_broken_startup_file() {
        for (index, original) in [&b"idle_minutes = ["[..], &[0xff][..]]
            .into_iter()
            .enumerate()
        {
            let dir = temp_dir(&format!("repair-{index}"));
            let path = dir.join("config.toml");
            let mut cfg = Config::default();
            let mut revision = 0;
            let json = serde_json::json!({ "action": "log" });
            std::fs::write(&path, original).unwrap();

            assert!(handle_save(
                &path,
                &json,
                (0, None),
                1,
                &mut cfg,
                &mut revision,
                |_| panic!("unrecognized broken file must not apply"),
            )
            .is_err());

            let external = b"warning_seconds = [";
            std::fs::write(&path, external).unwrap();
            assert!(handle_save(
                &path,
                &json,
                (0, Some(original)),
                2,
                &mut cfg,
                &mut revision,
                |_| panic!("intervening edit must not apply"),
            )
            .is_err());
            assert_eq!(std::fs::read(&path).unwrap(), external);
            assert_eq!(revision, 0);

            std::fs::write(&path, original).unwrap();
            let applied = Cell::new(false);
            let event = handle_save(
                &path,
                &json,
                (0, Some(original)),
                3,
                &mut cfg,
                &mut revision,
                |candidate| {
                    assert_eq!(Config::load_or_create(&path)?, *candidate);
                    applied.set(true);
                    Ok(())
                },
            )
            .unwrap();
            assert!(applied.get());
            assert!(matches!(
                event,
                UserEvent::Applied {
                    request: Some(3),
                    ..
                }
            ));
            assert_eq!(cfg.action, Action::Log);
            assert_eq!(revision, 1);
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn applied_snapshot_is_emitted_for_same_language_same_status_and_duplicate_reload() {
        let mut cfg = Config::default();
        let mut revision = 0;
        let mut new = cfg.clone();
        new.guards.fullscreen = true;
        let calls = Cell::new(0);
        let event = apply_config(&mut cfg, &mut revision, new.clone(), None, |_| {
            calls.set(1);
            Ok(())
        })
        .unwrap();
        let UserEvent::Applied {
            config,
            revision: seen,
            request,
        } = event
        else {
            panic!("missing applied snapshot")
        };
        assert_eq!(config, new);
        assert_eq!(seen, 1);
        assert_eq!(request, None);
        assert_eq!(calls.get(), 1);
        let event = apply_config(&mut cfg, &mut revision, new, None, |_| {
            panic!("duplicate apply")
        })
        .unwrap();
        assert!(matches!(event, UserEvent::Applied { revision: 1, .. }));
    }

    #[test]
    fn failed_worker_channel_and_apply_never_acknowledge_success() {
        let (tx, rx) = mpsc::channel();
        drop(rx);
        assert!(send_save(&tx, serde_json::json!({}), 0, 1).is_err());
        let mut cfg = Config::default();
        let mut revision = 0;
        let mut new = cfg.clone();
        new.action = Action::Log;
        assert!(
            apply_config(&mut cfg, &mut revision, new, Some(1), |_| anyhow::bail!(
                "fake worker failure"
            ))
            .is_err()
        );
        assert_eq!(cfg.action, Action::Shutdown);
        assert_eq!(revision, 0);
    }

    #[test]
    fn tray_resource_icons_load() {
        // Resources 2-4 are embedded by build.rs into every binary, tests included.
        assert!(tray_icon(false, false).is_ok());
        assert!(tray_icon(true, false).is_ok());
        assert!(tray_icon(false, true).is_ok());
    }

    #[test]
    fn action_words_are_distinct() {
        let words = [
            action_word(&en(), Action::Shutdown),
            action_word(&en(), Action::Hibernate),
            action_word(&en(), Action::Sleep),
        ];
        assert_ne!(words[0], words[1]);
        assert_ne!(words[1], words[2]);
        assert_ne!(words[0], words[2]);
    }

    /// Every key the app looks up; a typo here or in the code shows up as a missing string.
    const USED_KEYS: &[&str] = &[
        "menu.pause",
        "menu.snooze",
        "menu.now",
        "menu.settings",
        "menu.open_config",
        "menu.open_logs",
        "menu.about",
        "menu.quit",
        "status.watching",
        "status.paused",
        "status.snoozed",
        "status.warning",
        "status.shutting_down",
        "status.would_shutdown",
        "status.log_only_tag",
        "status.held_by",
        "action.shutdown",
        "action.hibernate",
        "action.sleep",
        "action.log",
        "toast.warning",
        "toast.warning_log",
        "toast.cancelled",
        "toast.failed",
        "about.title",
        "about.config",
        "about.logs",
        "about.dry_run",
        "dialog.config_error",
        "dialog.already_running",
        "settings.window_title",
        "settings.saved",
        "settings.saving",
        "settings.language_auto",
    ];

    #[test]
    fn every_key_the_app_uses_exists_in_every_language() {
        for l in crate::i18n::LANGUAGES {
            let tr = Translator::new(l.code);
            for key in USED_KEYS {
                assert_ne!(tr.t(key), *key, "{}: missing {key}", l.code);
            }
        }
    }

    #[test]
    fn settings_page_only_references_existing_strings() {
        let html = include_str!("../ui/settings.html");
        let mut found = 0;
        for part in html.split("data-i18n=\"").skip(1) {
            let key = part.split('"').next().unwrap();
            for l in crate::i18n::LANGUAGES {
                let section = Translator::new(l.code).section("settings");
                assert!(
                    section.contains_key(key),
                    "{}: page uses unknown string {key}",
                    l.code
                );
            }
            found += 1;
        }
        assert!(
            found > 10,
            "page should use many localized strings, found {found}"
        );
    }

    #[test]
    fn status_text_is_localized() {
        let (_f, m) = machine();
        let en_text = status_text(&m, &Translator::new("en"), false, None, false);
        let ru_text = status_text(&m, &Translator::new("ru"), false, None, false);
        assert_ne!(en_text, ru_text);
        assert!(ru_text.contains("слежу"));
    }

    #[test]
    fn status_text_describes_structured_blocks_in_the_chosen_language() {
        let (f, mut m) = machine();
        f.block.set(true);
        m.tick().unwrap();
        // `Other` is passed through verbatim in every language.
        assert!(
            status_text(&m, &Translator::new("ru"), false, None, false).contains("держит: fake")
        );
        assert!(
            status_text(&m, &Translator::new("en"), false, None, false).contains("held by: fake")
        );
    }

    #[test]
    fn saved_config_keeps_its_comments_and_language() {
        let dir = temp_dir("save-comments");
        let path = dir.join("config.toml");
        save_config(&path, &serde_json::json!({ "language": "ru" })).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# Minutes without keyboard or mouse input"));
        assert!(text.contains("# Guards:"));
        assert_eq!(Config::parse(&text).unwrap().language, "ru");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn log_only_mode_is_visible_in_the_status() {
        let f = Fake::default();
        let mut m = Machine::new(
            Settings {
                idle_threshold: Duration::from_secs(10),
                warning: Duration::from_secs(0),
                action: Action::Log,
            },
            f.clone(),
            f.clone(),
            f.clone(),
            f.clone(),
        );
        let tr = en();
        let idle = status_text(&m, &tr, false, None, false);
        assert!(idle.contains("[log-only]"));

        f.idle.set(Duration::from_secs(20));
        m.tick().unwrap(); // warning
        m.tick().unwrap(); // "shutdown" (log-only)
        let after = status_text(&m, &tr, false, None, false);
        assert!(after.contains("would shut down"));
        assert!(!after.contains("shutting down"));
        // A normal mode never claims to be log-only.
        let (_f, normal) = machine();
        assert!(!status_text(&normal, &tr, false, None, false).contains("log-only"));
    }
}

//! Platform-independent core: state machine Active -> Warning -> Shutdown.
//! Everything platform-specific hides behind traits, so it is testable without Windows.

use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};

pub trait IdleSource {
    /// Time since the last user input.
    fn idle_for(&self) -> Duration;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    #[default]
    Shutdown,
    Hibernate,
    Sleep,
    /// Run the whole scenario (warning, countdown) but only write to the log.
    Log,
}

pub trait Shutdowner {
    fn shutdown(&self, action: Action) -> Result<()>;
}

impl<T: Shutdowner + ?Sized> Shutdowner for Box<T> {
    fn shutdown(&self, action: Action) -> Result<()> {
        (**self).shutdown(action)
    }
}

pub trait Clock {
    /// Monotonic time from an arbitrary origin.
    fn now(&self) -> Duration;
}

/// Why a guard is holding the shutdown back. Structured so the UI can localize it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Process(String),
    Fullscreen,
    Audio,
    /// Rounded CPU load in percent.
    Cpu(u8),
    /// Free-form reason (not localized).
    Other(String),
}

/// "Do not shut down while..." checks. `None` means clear, `Some(reason)` means blocked.
pub trait Guard {
    fn blocker(&mut self) -> Option<Block>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub idle_threshold: Duration,
    pub warning: Duration,
    pub action: Action,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Active,
    /// Warning shown, countdown to shutdown is running.
    Warning {
        started: Duration,
    },
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    None,
    WarningStarted,
    WarningCancelled,
    ShutdownIssued,
}

pub struct Machine<I, S, C, G> {
    settings: Settings,
    idle: I,
    shutdowner: S,
    clock: C,
    guard: G,
    state: State,
    paused: bool,
    blocked_by: Option<Block>,
}

impl<I: IdleSource, S: Shutdowner, C: Clock, G: Guard> Machine<I, S, C, G> {
    pub fn new(settings: Settings, idle: I, shutdowner: S, clock: C, guard: G) -> Self {
        Self {
            settings,
            idle,
            shutdowner,
            clock,
            guard,
            state: State::Active,
            paused: false,
            blocked_by: None,
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    pub fn set_settings(&mut self, settings: Settings) {
        self.settings = settings;
    }

    pub fn guard_mut(&mut self) -> &mut G {
        &mut self.guard
    }

    /// What currently blocks shutdown (as of the last `tick`).
    pub fn blocked_by(&self) -> Option<&Block> {
        self.blocked_by.as_ref()
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Carries out the configured action. `Action::Log` never reaches the shutdowner,
    /// so log-only mode cannot touch the machine no matter how the shutdowner behaves.
    fn issue(&self) -> Result<()> {
        if self.settings.action == Action::Log {
            tracing::warn!("LOG-ONLY: the machine would be shut down now");
            return Ok(());
        }
        self.shutdowner.shutdown(self.settings.action)
    }

    /// "Shutdown now" from the menu: bypasses timers and guards.
    pub fn shutdown_now(&mut self) -> Result<()> {
        self.issue()?;
        self.state = State::Shutdown;
        Ok(())
    }

    /// One polling step. Call every poll_seconds (more often during Warning).
    pub fn tick(&mut self) -> Result<Event> {
        let idle_for = self.idle.idle_for();
        let now = self.clock.now();
        let over_threshold = idle_for >= self.settings.idle_threshold;
        // Sample every tick: the CPU guard needs deltas between calls.
        self.blocked_by = self.guard.blocker();
        let allowed = !self.paused && self.blocked_by.is_none();

        match self.state {
            State::Shutdown => {
                // Back after sleep/hibernate (or dry-run): armed again.
                if !over_threshold {
                    self.state = State::Active;
                }
                Ok(Event::None)
            }
            State::Active => {
                if over_threshold && allowed {
                    self.state = State::Warning { started: now };
                    Ok(Event::WarningStarted)
                } else {
                    Ok(Event::None)
                }
            }
            State::Warning { started } => {
                // Input reset the idle time or paused: cancel. A guard only
                // holds the countdown — the app's own toast sound must not
                // cancel the warning (that was an infinite warn/cancel loop).
                if !over_threshold || self.paused {
                    self.state = State::Active;
                    return Ok(Event::WarningCancelled);
                }
                // Held by a guard: push `started` forward, so once the guard
                // releases a fresh full warning countdown runs.
                let started = if self.blocked_by.is_some() {
                    now
                } else {
                    started
                };
                if now.saturating_sub(started) >= self.settings.warning {
                    self.issue()?;
                    self.state = State::Shutdown;
                    return Ok(Event::ShutdownIssued);
                }
                self.state = State::Warning { started };
                Ok(Event::None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;

    #[derive(Clone, Default)]
    struct Fake {
        idle: Rc<Cell<Duration>>,
        now: Rc<Cell<Duration>>,
        shutdowns: Rc<Cell<u32>>,
        block: Rc<Cell<bool>>,
        last_action: Rc<Cell<Option<Action>>>,
    }

    impl IdleSource for Fake {
        fn idle_for(&self) -> Duration {
            self.idle.get()
        }
    }
    impl Clock for Fake {
        fn now(&self) -> Duration {
            self.now.get()
        }
    }
    impl Guard for Fake {
        fn blocker(&mut self) -> Option<Block> {
            self.block.get().then(|| Block::Other("fake".to_string()))
        }
    }
    impl Shutdowner for Fake {
        fn shutdown(&self, action: Action) -> Result<()> {
            self.last_action.set(Some(action));
            self.shutdowns.set(self.shutdowns.get() + 1);
            Ok(())
        }
    }

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    fn setup() -> (Fake, Machine<Fake, Fake, Fake, Fake>) {
        let f = Fake::default();
        let m = Machine::new(
            Settings {
                idle_threshold: secs(7200),
                warning: secs(60),
                action: Action::Hibernate,
            },
            f.clone(),
            f.clone(),
            f.clone(),
            f.clone(),
        );
        (f, m)
    }

    #[test]
    fn stays_active_below_threshold() {
        let (f, mut m) = setup();
        f.idle.set(secs(7199));
        assert_eq!(m.tick().unwrap(), Event::None);
        assert_eq!(m.state(), State::Active);
    }

    #[test]
    fn warns_then_shuts_down_after_warning_period() {
        let (f, mut m) = setup();
        f.idle.set(secs(7200));
        assert_eq!(m.tick().unwrap(), Event::WarningStarted);

        f.now.set(secs(59));
        f.idle.set(secs(7259));
        assert_eq!(m.tick().unwrap(), Event::None);
        assert_eq!(f.shutdowns.get(), 0);

        f.now.set(secs(60));
        f.idle.set(secs(7260));
        assert_eq!(m.tick().unwrap(), Event::ShutdownIssued);
        assert_eq!(f.shutdowns.get(), 1);
        assert_eq!(m.state(), State::Shutdown);
    }

    #[test]
    fn input_during_warning_cancels() {
        let (f, mut m) = setup();
        f.idle.set(secs(7200));
        m.tick().unwrap();

        f.now.set(secs(30));
        f.idle.set(secs(0));
        assert_eq!(m.tick().unwrap(), Event::WarningCancelled);
        assert_eq!(m.state(), State::Active);
        assert_eq!(f.shutdowns.get(), 0);
    }

    #[test]
    fn pause_blocks_warning_and_cancels_active_one() {
        let (f, mut m) = setup();
        m.set_paused(true);
        f.idle.set(secs(9000));
        assert_eq!(m.tick().unwrap(), Event::None);

        m.set_paused(false);
        assert_eq!(m.tick().unwrap(), Event::WarningStarted);
        m.set_paused(true);
        assert_eq!(m.tick().unwrap(), Event::WarningCancelled);
    }

    #[test]
    fn shutdown_is_issued_once() {
        let (f, mut m) = setup();
        f.idle.set(secs(7200));
        m.tick().unwrap();
        f.now.set(secs(60));
        m.tick().unwrap();
        f.now.set(secs(120));
        assert_eq!(m.tick().unwrap(), Event::None);
        assert_eq!(f.shutdowns.get(), 1);
    }

    #[test]
    fn resumes_after_user_returns_from_sleep() {
        let (f, mut m) = setup();
        m.shutdown_now().unwrap();
        f.idle.set(secs(10000));
        m.tick().unwrap();
        assert_eq!(m.state(), State::Shutdown);

        f.idle.set(secs(1));
        m.tick().unwrap();
        assert_eq!(m.state(), State::Active);
    }

    #[test]
    fn guard_blocks_warning_and_holds_active_one() {
        let (f, mut m) = setup();
        f.block.set(true);
        f.idle.set(secs(9000));
        assert_eq!(m.tick().unwrap(), Event::None);
        assert_eq!(m.blocked_by(), Some(&Block::Other("fake".to_string())));

        f.block.set(false);
        assert_eq!(m.tick().unwrap(), Event::WarningStarted);
        assert_eq!(m.blocked_by(), None);

        // A guard holds the warning instead of cancelling it: no shutdown,
        // no cancel, still in Warning (the app's own toast sound must not
        // cancel the warning it just raised).
        f.block.set(true);
        assert_eq!(m.tick().unwrap(), Event::None);
        assert_eq!(f.shutdowns.get(), 0);
        assert!(matches!(m.state(), State::Warning { .. }));

        // Guard releases: the countdown restarts fresh, then issues.
        f.block.set(false);
        f.now.set(secs(59));
        assert_eq!(m.tick().unwrap(), Event::None);
        f.now.set(secs(120));
        assert_eq!(m.tick().unwrap(), Event::ShutdownIssued);
        assert_eq!(f.shutdowns.get(), 1);
    }

    #[test]
    fn action_from_settings_is_passed_to_shutdowner() {
        let (f, mut m) = setup();
        m.shutdown_now().unwrap();
        assert_eq!(f.last_action.get(), Some(Action::Hibernate));
        assert_eq!(m.state(), State::Shutdown);
    }

    #[test]
    fn log_action_runs_the_scenario_but_never_calls_the_shutdowner() {
        let f = Fake::default();
        let mut m = Machine::new(
            Settings {
                idle_threshold: secs(10),
                warning: secs(5),
                action: Action::Log,
            },
            f.clone(),
            f.clone(),
            f.clone(),
            f.clone(),
        );
        f.idle.set(secs(10));
        assert_eq!(m.tick().unwrap(), Event::WarningStarted);

        f.now.set(secs(5));
        assert_eq!(m.tick().unwrap(), Event::ShutdownIssued);
        assert_eq!(m.state(), State::Shutdown);
        assert_eq!(f.shutdowns.get(), 0);

        // "Shutdown now" from the menu is log-only too.
        m.shutdown_now().unwrap();
        assert_eq!(f.shutdowns.get(), 0);
    }

    #[test]
    fn log_action_is_parsed_from_lowercase() {
        #[derive(serde::Deserialize)]
        struct W {
            action: Action,
        }
        let w: W = toml::from_str("action = \"log\"").unwrap();
        assert_eq!(w.action, Action::Log);
    }
}

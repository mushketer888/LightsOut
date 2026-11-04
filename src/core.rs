//! Platform-independent core: state machine Active -> Warning -> Shutdown.
//! Everything platform-specific hides behind traits, so it is testable without Windows.

use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdleSample {
    pub idle_for: Duration,
    /// Opaque input identity, compared only for equality. None means unavailable.
    pub last_input_id: Option<u32>,
}

pub trait IdleSource {
    /// Read duration and identity from the same input observation.
    fn sample(&self) -> IdleSample;
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
    /// An action failed; automatic attempts wait for fresh input.
    Failed,
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
    attempted_input: Option<u32>,
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
            attempted_input: None,
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    pub fn set_settings(&mut self, settings: Settings) {
        if settings != self.settings && matches!(self.state, State::Warning { .. }) {
            self.state = State::Active;
        }
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
        self.attempt(self.idle.sample().last_input_id)
    }

    fn attempt(&mut self, last_input_id: Option<u32>) -> Result<()> {
        // Capture before issuing: sleep may not return until after fresh input.
        self.attempted_input = last_input_id;
        self.state = State::Failed;
        self.issue()?;
        self.state = State::Shutdown;
        Ok(())
    }

    /// One polling step. Call every poll_seconds (more often during Warning).
    pub fn tick(&mut self) -> Result<Event> {
        let input = self.idle.sample();
        let now = self.clock.now();
        let over_threshold =
            input.last_input_id.is_some() && input.idle_for >= self.settings.idle_threshold;
        // Sample every tick: the CPU guard needs deltas between calls.
        let was_blocked = self.blocked_by.is_some();
        self.blocked_by = self.guard.blocker();
        let allowed = !self.paused && self.blocked_by.is_none();

        match self.state {
            State::Shutdown | State::Failed => {
                // Detect input even if the next poll is already past the idle threshold.
                if let Some(current) = input.last_input_id {
                    if self
                        .attempted_input
                        .is_some_and(|previous| previous != current)
                    {
                        self.state = State::Active;
                    }
                    // If the attempt had no sample, establish a baseline, not fake input.
                    self.attempted_input = Some(current);
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
                // A guard must also hold a zero-length warning.
                if self.blocked_by.is_some() {
                    self.state = State::Warning { started: now };
                    return Ok(Event::None);
                }
                // Start the full countdown at the first clear sample, not the last busy one.
                let started = if was_blocked { now } else { started };
                if now.saturating_sub(started) >= self.settings.warning {
                    self.attempt(input.last_input_id)?;
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
        last_input: Rc<Cell<u32>>,
        fail: Rc<Cell<bool>>,
        input_unavailable: Rc<Cell<bool>>,
        input_on_return: Rc<Cell<Option<u32>>>,
    }

    impl Fake {
        fn sample_at(&self, now: u64, idle: u64, last_input: u32) {
            self.now.set(secs(now));
            self.idle.set(secs(idle));
            self.last_input.set(last_input);
        }
    }

    impl IdleSource for Fake {
        fn sample(&self) -> IdleSample {
            IdleSample {
                idle_for: self.idle.get(),
                last_input_id: (!self.input_unavailable.get()).then(|| self.last_input.get()),
            }
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
            if let Some(input) = self.input_on_return.take() {
                self.last_input.set(input);
            }
            anyhow::ensure!(!self.fail.get(), "fake action failure");
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
        f.last_input.set(1);
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
    fn guard_holds_zero_and_positive_warnings_and_clear_starts_full_countdown() {
        for warning in [0, 1, 60] {
            let (f, mut m) = setup();
            m.set_settings(Settings {
                idle_threshold: secs(60),
                warning: secs(warning),
                action: Action::Sleep,
            });
            f.sample_at(0, 60, 0);
            f.block.set(true);
            assert_eq!(m.tick().unwrap(), Event::None);
            assert_eq!(m.state(), State::Active);
            f.block.set(false);
            assert_eq!(m.tick().unwrap(), Event::WarningStarted);
            f.block.set(true);
            for now in [1, 2, 100] {
                f.sample_at(now, 60 + now, 0);
                assert_eq!(m.tick().unwrap(), Event::None);
                assert!(matches!(m.state(), State::Warning { .. }));
                assert_eq!(f.shutdowns.get(), 0);
            }
            f.block.set(false);
            f.sample_at(1000, 1060, 0);
            if warning == 0 {
                assert_eq!(m.tick().unwrap(), Event::ShutdownIssued);
            } else {
                assert_eq!(m.tick().unwrap(), Event::None);
                assert_eq!(
                    m.state(),
                    State::Warning {
                        started: secs(1000)
                    }
                );
                f.sample_at(1000 + warning - 1, 1060 + warning - 1, 0);
                assert_eq!(m.tick().unwrap(), Event::None);
                assert_eq!(f.shutdowns.get(), 0);
                f.sample_at(1000 + warning, 1060 + warning, 0);
                assert_eq!(m.tick().unwrap(), Event::ShutdownIssued);
            }
            assert_eq!(f.shutdowns.get(), 1);
        }
    }

    #[test]
    fn actual_settings_changes_invalidate_warning_but_duplicate_reload_does_not() {
        let original = Settings {
            idle_threshold: secs(60),
            warning: secs(60),
            action: Action::Log,
        };
        for changed in [
            Settings {
                action: Action::Shutdown,
                ..original
            },
            Settings {
                warning: secs(61),
                ..original
            },
            Settings {
                idle_threshold: secs(61),
                ..original
            },
        ] {
            let (f, mut m) = setup();
            m.set_settings(original);
            f.sample_at(0, 120, 0);
            assert_eq!(m.tick().unwrap(), Event::WarningStarted);
            f.sample_at(59, 179, 0);
            m.set_settings(original);
            assert_eq!(m.state(), State::Warning { started: secs(0) });
            m.set_settings(changed);
            assert_eq!(m.state(), State::Active);
            f.sample_at(60, 180, 0);
            assert_eq!(m.tick().unwrap(), Event::WarningStarted);
            f.sample_at(
                60 + changed.warning.as_secs() - 1,
                179 + changed.warning.as_secs(),
                0,
            );
            m.set_settings(changed);
            assert_eq!(m.tick().unwrap(), Event::None);
            assert_eq!(f.shutdowns.get(), 0);
            f.sample_at(
                60 + changed.warning.as_secs(),
                180 + changed.warning.as_secs(),
                0,
            );
            assert_eq!(m.tick().unwrap(), Event::ShutdownIssued);
            assert_eq!(f.shutdowns.get(), u32::from(changed.action != Action::Log));
            assert_eq!(
                f.last_action.get(),
                (changed.action != Action::Log).then_some(changed.action)
            );
        }
    }

    #[test]
    fn duplicate_reload_preserves_original_deadline() {
        let (f, mut m) = setup();
        f.sample_at(0, 7200, 0);
        assert_eq!(m.tick().unwrap(), Event::WarningStarted);
        f.sample_at(59, 7259, 0);
        m.set_settings(*m.settings());
        assert_eq!(m.tick().unwrap(), Event::None);
        f.sample_at(60, 7260, 0);
        m.set_settings(*m.settings());
        assert_eq!(m.tick().unwrap(), Event::ShutdownIssued);
        assert_eq!(f.shutdowns.get(), 1);
    }

    #[test]
    fn between_poll_input_rearms_even_when_idle_is_already_over_threshold() {
        for action in [Action::Sleep, Action::Log] {
            let (f, mut m) = setup();
            m.set_settings(Settings {
                idle_threshold: secs(60),
                warning: secs(5),
                action,
            });
            f.sample_at(0, 60, 0);
            assert_eq!(m.tick().unwrap(), Event::WarningStarted);
            f.sample_at(5, 65, 0);
            assert_eq!(m.tick().unwrap(), Event::ShutdownIssued);
            // Input at t=10, first subsequent poll at t=125.
            f.sample_at(125, 115, 10_000);
            assert_eq!(m.tick().unwrap(), Event::None);
            assert_eq!(m.state(), State::Active);
            f.sample_at(245, 235, 10_000);
            assert_eq!(m.tick().unwrap(), Event::WarningStarted);
            f.sample_at(250, 240, 10_000);
            assert_eq!(m.tick().unwrap(), Event::ShutdownIssued);
            assert_eq!(f.shutdowns.get(), if action == Action::Log { 0 } else { 2 });
        }
    }

    #[test]
    fn failure_is_reported_once_until_new_input_then_gets_a_new_warning() {
        let (f, mut m) = setup();
        m.set_settings(Settings {
            idle_threshold: secs(60),
            warning: secs(5),
            action: Action::Sleep,
        });
        f.fail.set(true);
        f.sample_at(0, 60, 0);
        assert_eq!(m.tick().unwrap(), Event::WarningStarted);
        f.sample_at(5, 65, 0);
        assert!(m.tick().is_err());
        assert_eq!(m.state(), State::Failed);
        for now in 6..=65 {
            f.sample_at(now, 60 + now, 0);
            m.set_settings(*m.settings());
            assert_eq!(m.tick().unwrap(), Event::None);
        }
        assert_eq!(f.shutdowns.get(), 1);
        m.set_settings(Settings {
            action: Action::Hibernate,
            ..*m.settings()
        });
        assert_eq!(m.state(), State::Failed);
        f.input_unavailable.set(true);
        f.sample_at(66, 0, 0);
        assert_eq!(m.tick().unwrap(), Event::None);
        assert_eq!(m.state(), State::Failed);
        f.input_unavailable.set(false);
        f.fail.set(false);
        f.sample_at(125, 115, 10_000);
        assert_eq!(m.tick().unwrap(), Event::None);
        assert_eq!(m.state(), State::Active);
        f.sample_at(126, 116, 10_000);
        assert_eq!(m.tick().unwrap(), Event::WarningStarted);
        f.sample_at(131, 121, 10_000);
        assert_eq!(m.tick().unwrap(), Event::ShutdownIssued);
        assert_eq!(f.shutdowns.get(), 2);
    }

    #[test]
    fn manual_retry_bypasses_pause_and_guard_but_failure_still_latches() {
        let (f, mut m) = setup();
        f.sample_at(10, 0, 10_000);
        m.set_paused(true);
        f.block.set(true);
        f.fail.set(true);
        assert!(m.shutdown_now().is_err());
        f.sample_at(100, 90, 10_000);
        assert_eq!(m.tick().unwrap(), Event::None);
        assert_eq!(m.state(), State::Failed);
        assert_eq!(f.shutdowns.get(), 1);
        f.fail.set(false);
        m.shutdown_now().unwrap();
        assert_eq!(m.state(), State::Shutdown);
        assert_eq!(f.shutdowns.get(), 2);
    }

    #[test]
    fn input_identity_is_captured_before_blocking_action_and_compared_across_wrap() {
        let (f, mut m) = setup();
        f.sample_at(0, 0, u32::MAX - 5);
        f.input_on_return.set(Some(10));
        m.shutdown_now().unwrap();
        f.sample_at(8000, 7999, 10);
        assert_eq!(m.tick().unwrap(), Event::None);
        assert_eq!(m.state(), State::Active);
    }

    #[test]
    fn unavailable_input_cancels_warning_without_issuing_action() {
        let (f, mut m) = setup();
        f.sample_at(0, 7200, 0);
        assert_eq!(m.tick().unwrap(), Event::WarningStarted);
        f.sample_at(60, 7260, 0);
        f.input_unavailable.set(true);
        assert_eq!(m.tick().unwrap(), Event::WarningCancelled);
        assert_eq!(f.shutdowns.get(), 0);
    }

    #[test]
    fn recovery_of_unavailable_sample_does_not_invent_input_after_failed_attempt() {
        let (f, mut m) = setup();
        f.input_unavailable.set(true);
        f.fail.set(true);
        assert!(m.shutdown_now().is_err());
        f.input_unavailable.set(false);
        f.sample_at(100, 100, 10);
        assert_eq!(m.tick().unwrap(), Event::None);
        assert_eq!(m.state(), State::Failed);
        f.sample_at(200, 200, 10);
        assert_eq!(m.tick().unwrap(), Event::None);
        assert_eq!(f.shutdowns.get(), 1);
        f.sample_at(300, 1, 20);
        assert_eq!(m.tick().unwrap(), Event::None);
        assert_eq!(m.state(), State::Active);
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

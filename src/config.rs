use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::core::{Action, Settings};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GuardsConfig {
    pub cpu_above_percent: f32,
    pub audio_playing: bool,
    pub fullscreen: bool,
    pub processes: Vec<String>,
}

impl Default for GuardsConfig {
    fn default() -> Self {
        Self {
            cpu_above_percent: 20.0,
            // The app's own toast sound trips this guard (self-cancel loop),
            // so it is off by default; opt in via config if wanted.
            audio_playing: false,
            // Off by default: a maximized window with an auto-hidden taskbar
            // reads as fullscreen and holds the shutdown for no reason.
            // @todo: smarter detection (work-area + window styles), then
            // flip the default back to true.
            fullscreen: false,
            processes: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub idle_minutes: u64,
    pub warning_seconds: u64,
    pub poll_seconds: u64,
    pub action: Action,
    pub autostart: bool,
    /// `auto` (follow Windows) or a code from `i18n::LANGUAGES`; unknown codes mean English.
    pub language: String,
    pub guards: GuardsConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            idle_minutes: 120,
            warning_seconds: 60,
            poll_seconds: 15,
            action: Action::Shutdown,
            autostart: true,
            language: "auto".to_string(),
            guards: GuardsConfig::default(),
        }
    }
}

impl Config {
    /// The config file text: every option documented, with the current values filled in.
    /// Used for the first-run file and whenever the settings dialog saves, so comments survive.
    pub fn to_commented_toml(&self) -> String {
        let action = match self.action {
            Action::Shutdown => "shutdown",
            Action::Hibernate => "hibernate",
            Action::Sleep => "sleep",
            Action::Log => "log",
        };
        let processes = toml::Value::Array(
            self.guards
                .processes
                .iter()
                .map(|p| toml::Value::String(p.clone()))
                .collect(),
        );
        let language = toml::Value::String(self.language.clone());
        format!(
            r#"# LightsOut configuration.
# Changes are applied automatically within a second; no restart needed.
# If the file is invalid it is ignored and the previous settings stay active.

# Minutes without keyboard or mouse input before the shutdown warning starts.
# Minimum 1.
idle_minutes = {idle}

# Length of the warning countdown, in seconds.
# Any keyboard or mouse input during the countdown cancels the shutdown.
warning_seconds = {warning}

# How often, in seconds, input and guards are checked. Minimum 1.
# (During the warning countdown it is checked every second regardless.)
poll_seconds = {poll}

# What to do when the countdown ends:
#   "shutdown"  turn the computer off
#   "hibernate" save the session to disk and power off
#   "sleep"     low-power sleep
#   "log"       trial mode: warning, countdown and toast run as usual, but the
#               computer is NOT touched; the event is only written to the log
action = "{action}"

# Start LightsOut automatically when you sign in to Windows (true or false).
autostart = {autostart}

# Interface language: "auto" follows Windows; otherwise a language code
# such as "en" or "ru". Unknown codes fall back to English.
language = {language}

# Guards: LightsOut does nothing while any enabled guard says "busy",
# even if you have not touched the keyboard or mouse for hours.
[guards]
# Busy while total CPU load is above this percent (builds, renders, downloads).
# Range 0-100; 0 turns the check off.
cpu_above_percent = {cpu}

# Busy while the default audio device is playing sound (music, video, calls).
audio_playing = {audio}

# Busy while a fullscreen window (game, video player) is in front.
# Off by default: a maximized window with an auto-hidden taskbar reads as
# fullscreen and holds the shutdown for no reason.
# @todo: smarter detection, then flip the default back to true.
fullscreen = {fullscreen}

# Busy while any of these programs is running. Exact exe file names,
# case-insensitive, for example ["ffmpeg.exe", "blender.exe"].
# An empty list turns the check off.
processes = {processes}
"#,
            idle = self.idle_minutes,
            warning = self.warning_seconds,
            poll = self.poll_seconds,
            autostart = self.autostart,
            cpu = self.guards.cpu_above_percent,
            audio = self.guards.audio_playing,
            fullscreen = self.guards.fullscreen,
        )
    }

    pub fn parse(text: &str) -> Result<Self> {
        let cfg: Config = toml::from_str(text).context("битый TOML")?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(self.idle_minutes >= 1, "idle_minutes должен быть >= 1");
        anyhow::ensure!(self.poll_seconds >= 1, "poll_seconds должен быть >= 1");
        anyhow::ensure!(
            (0.0..=100.0).contains(&self.guards.cpu_above_percent),
            "cpu_above_percent должен быть в 0..=100"
        );
        Ok(())
    }

    /// Reads the config; creates it with defaults if the file is missing.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if !path.exists() {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(path, Config::default().to_commented_toml())?;
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("не прочитать {}", path.display()))?;
        Self::parse(&text)
    }

    pub fn settings(&self) -> Settings {
        Settings {
            idle_threshold: Duration::from_secs(self.idle_minutes * 60),
            warning: Duration::from_secs(self.warning_seconds),
            action: self.action,
        }
    }

    pub fn poll_interval(&self) -> Duration {
        Duration::from_secs(self.poll_seconds)
    }
}

pub struct Paths {
    pub config_file: PathBuf,
    pub log_dir: PathBuf,
    /// WebView2 profile/cache for the settings dialog.
    pub webview_dir: PathBuf,
}

impl Paths {
    pub fn discover() -> Result<Self> {
        let dirs =
            ProjectDirs::from("", "", "LightsOut").context("не найти профиль пользователя")?;
        Ok(Self {
            config_file: dirs.config_dir().join("config.toml"),
            log_dir: dirs.data_local_dir().join("logs"),
            webview_dir: dirs.data_local_dir().join("webview"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commented_toml_of_defaults_parses_back_to_defaults() {
        let text = Config::default().to_commented_toml();
        assert_eq!(Config::parse(&text).unwrap(), Config::default());
    }

    #[test]
    fn commented_toml_roundtrips_custom_values() {
        let cfg = Config {
            idle_minutes: 45,
            warning_seconds: 5,
            poll_seconds: 3,
            action: Action::Hibernate,
            autostart: false,
            language: "ru".to_string(),
            guards: GuardsConfig {
                cpu_above_percent: 37.5,
                audio_playing: false,
                fullscreen: false,
                // Quotes, backslashes and non-ASCII must survive the template.
                processes: vec![
                    "ffmpeg.exe".into(),
                    "My \"App\".exe".into(),
                    "Игра.exe".into(),
                ],
            },
        };
        let text = cfg.to_commented_toml();
        assert_eq!(Config::parse(&text).unwrap(), cfg);
    }

    #[test]
    fn every_option_is_documented_in_the_template() {
        let text = Config::default().to_commented_toml();
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let is_option = !line.starts_with('#')
                && !line.trim().is_empty()
                && !line.starts_with('[')
                && line.contains(" = ");
            if is_option {
                // Walk up over blank-free lines: the line above must be a comment.
                assert!(
                    lines[i - 1].starts_with('#'),
                    "option without a comment: {line}"
                );
            }
        }
        assert!(text.contains("[guards]"));
    }

    #[test]
    fn language_defaults_to_auto_and_is_configurable() {
        assert_eq!(Config::default().language, "auto");
        assert_eq!(Config::parse("language = \"ru\"").unwrap().language, "ru");
    }

    #[test]
    fn partial_config_falls_back_to_defaults() {
        let cfg = Config::parse("idle_minutes = 30\n[guards]\nfullscreen = false").unwrap();
        assert_eq!(cfg.idle_minutes, 30);
        assert!(!cfg.guards.fullscreen);
        assert_eq!(cfg.warning_seconds, 60);
        assert!(!cfg.guards.audio_playing);
    }

    #[test]
    fn action_is_parsed() {
        let cfg = Config::parse("action = \"hibernate\"").unwrap();
        assert_eq!(cfg.action, Action::Hibernate);
    }

    #[test]
    fn invalid_values_are_rejected() {
        assert!(Config::parse("idle_minutes = 0").is_err());
        assert!(Config::parse("poll_seconds = 0").is_err());
        assert!(Config::parse("action = \"explode\"").is_err());
        assert!(Config::parse("idle_minutes = [").is_err());
    }

    #[test]
    fn settings_conversion() {
        let s = Config::default().settings();
        assert_eq!(s.idle_threshold, Duration::from_secs(7200));
        assert_eq!(s.warning, Duration::from_secs(60));
    }

    #[test]
    fn processes_are_blank_by_default_but_configurable() {
        assert!(Config::default().guards.processes.is_empty());
        let cfg = Config::parse("[guards]\nprocesses = [\"ffmpeg.exe\", \"Blender.exe\"]").unwrap();
        assert_eq!(cfg.guards.processes, ["ffmpeg.exe", "Blender.exe"]);
    }

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("lights-out-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn load_or_create_writes_defaults_when_missing() {
        let dir = temp_dir("create");
        let path = dir.join("nested").join("config.toml");
        assert_eq!(Config::load_or_create(&path).unwrap(), Config::default());
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_or_create_keeps_existing_file() {
        let dir = temp_dir("keep");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "idle_minutes = 45").unwrap();
        assert_eq!(Config::load_or_create(&path).unwrap().idle_minutes, 45);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "idle_minutes = 45");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_or_create_reports_broken_file() {
        let dir = temp_dir("broken");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "idle_minutes = \"two hours\"").unwrap();
        assert!(Config::load_or_create(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cpu_percent_out_of_range_is_rejected() {
        assert!(Config::parse("[guards]\ncpu_above_percent = 101").is_err());
        assert!(Config::parse("[guards]\ncpu_above_percent = -1").is_err());
        assert!(Config::parse("[guards]\ncpu_above_percent = 0").is_ok());
    }

    #[test]
    fn toml_roundtrip_preserves_config() {
        let cfg = Config {
            idle_minutes: 7,
            action: Action::Sleep,
            autostart: false,
            ..Config::default()
        };
        let text = toml::to_string_pretty(&cfg).unwrap();
        assert_eq!(Config::parse(&text).unwrap(), cfg);
    }

    #[test]
    fn log_action_roundtrips_through_the_template() {
        let cfg = Config {
            action: Action::Log,
            ..Config::default()
        };
        let text = cfg.to_commented_toml();
        assert!(text.contains("action = \"log\""));
        assert_eq!(Config::parse(&text).unwrap().action, Action::Log);
    }
}

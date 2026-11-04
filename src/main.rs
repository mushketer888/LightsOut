#![windows_subsystem = "windows"]

use anyhow::Result;
use lights_out::app::{self, Options};
use lights_out::config::Paths;
use lights_out::i18n::Translator;
use lights_out::logs;
use lights_out::platform::windows as win;
use lights_out::version;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--version" || a == "-V") {
        win::print_to_parent_console(&version::describe());
        return Ok(());
    }
    let dry_run = args.iter().any(|a| a == "--dry-run");

    let paths = Paths::discover()?;
    std::fs::create_dir_all(&paths.log_dir)?;
    // Drop logs older than the retention window now and then daily while running.
    let log_dir = paths.log_dir.clone();
    std::thread::spawn(move || loop {
        let removed =
            logs::prune_old_logs(&log_dir, logs::RETENTION_DAYS, std::time::SystemTime::now());
        if removed > 0 {
            tracing::info!(removed, "old log files deleted");
        }
        std::thread::sleep(std::time::Duration::from_secs(24 * 3600));
    });
    let appender = tracing_appender::rolling::RollingFileAppender::builder()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("lights-out")
        .filename_suffix("log")
        .build(&paths.log_dir)
        .expect("log appender");
    let (writer, _log_guard) = tracing_appender::non_blocking(appender);
    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .init();

    // Hold the handle until the end of main, or the mutex is released too early.
    let Some(_instance) = win::acquire_single_instance()? else {
        tracing::info!("already running, exiting");
        let tr = Translator::for_setting("auto", &win::user_locale());
        win::message_box("LightsOut", &tr.t("dialog.already_running"));
        return Ok(());
    };

    app::run(Options { dry_run, paths })
}

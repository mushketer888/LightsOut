//! Log retention: old daily log files are deleted.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How long log files are kept.
pub const RETENTION_DAYS: u64 = 14;

/// Only files whose name starts with this are ever touched.
pub const LOG_PREFIX: &str = "lights-out.";

/// Deletes `lights-out.*` files in `dir` last modified more than `keep_days` before `now`.
/// Returns how many were removed. Other files and subfolders are left alone.
pub fn prune_old_logs(dir: &Path, keep_days: u64, now: SystemTime) -> usize {
    let Some(cutoff) = now.checked_sub(Duration::from_secs(keep_days * 24 * 3600)) else {
        return 0;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let is_log = entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with(LOG_PREFIX));
        let Ok(meta) = entry.metadata() else { continue };
        if !is_log || !meta.is_file() {
            continue;
        }
        if meta.modified().is_ok_and(|m| m < cutoff) && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// One line appended straight to the log file when Windows asks to end the
/// session. Written synchronously, outside `tracing`: the process can be
/// killed before the buffered writer flushes, so this path must not buffer.
/// The shape matches the `tracing` default so daily files stay uniform.
pub fn session_end_line(cancelled: bool, now: SystemTime) -> String {
    let msg = if cancelled {
        "windows session end cancelled"
    } else {
        "windows session end requested (shutdown, restart, logoff or installer)"
    };
    format!("{}  INFO lights_out::app: {msg}\n", rfc3339_micros(now))
}

/// Appends a session-end line to today's log file in `dir`.
pub fn append_session_end_line(
    dir: &Path,
    cancelled: bool,
    now: SystemTime,
) -> std::io::Result<()> {
    let (y, m, d) = civil_from_days(days_since_epoch(now));
    let path = dir.join(format!("lights-out.{y:04}-{m:02}-{d:02}.log"));
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(session_end_line(cancelled, now).as_bytes())
}

/// `YYYY-MM-DDTHH:MM:SS.ffffffZ` in UTC, the same shape `tracing` writes.
fn rfc3339_micros(now: SystemTime) -> String {
    let dur = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = dur.as_secs() as i64;
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let (h, min, s) = (
        secs.rem_euclid(86_400) / 3600,
        secs.rem_euclid(3600) / 60,
        secs.rem_euclid(60),
    );
    format!(
        "{y:04}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}.{:06}Z",
        dur.subsec_micros()
    )
}

fn days_since_epoch(now: SystemTime) -> i64 {
    now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64 / 86_400
}

/// Days since 1970-01-01 to (year, month, day); Howard Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::path::PathBuf;

    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("lights-out-logs-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch(dir: &Path, name: &str, age_days: u64) {
        let f = File::create(dir.join(name)).unwrap();
        let when = SystemTime::now() - Duration::from_secs(age_days * 24 * 3600);
        f.set_modified(when).unwrap();
    }

    #[test]
    fn removes_only_old_log_files() {
        let dir = temp_dir("prune");
        touch(&dir, "lights-out.log.2026-01-01", 30);
        touch(&dir, "lights-out.2026-01-02.log", 15);
        touch(&dir, "lights-out.log.2026-11-01", 3);
        touch(&dir, "lights-out.log.today", 0);
        // Not ours: must survive even though it is ancient.
        touch(&dir, "notes.txt", 400);

        let removed = prune_old_logs(&dir, RETENTION_DAYS, SystemTime::now());

        assert_eq!(removed, 2);
        assert!(!dir.join("lights-out.log.2026-01-01").exists());
        assert!(!dir.join("lights-out.2026-01-02.log").exists());
        assert!(dir.join("lights-out.log.2026-11-01").exists());
        assert!(dir.join("lights-out.log.today").exists());
        assert!(dir.join("notes.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn boundary_keeps_files_just_inside_the_window() {
        let dir = temp_dir("boundary");
        touch(&dir, "lights-out.log.13d", 13);
        touch(&dir, "lights-out.log.15d", 15);
        prune_old_logs(&dir, 14, SystemTime::now());
        assert!(dir.join("lights-out.log.13d").exists());
        assert!(!dir.join("lights-out.log.15d").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ignores_subfolders_and_missing_dir() {
        let dir = temp_dir("subdir");
        let sub = dir.join("lights-out.old");
        std::fs::create_dir_all(&sub).unwrap();
        File::create(sub.join("x")).unwrap();
        assert_eq!(
            prune_old_logs(&dir, 0, SystemTime::now() + Duration::from_secs(10)),
            0
        );
        assert!(sub.exists());
        assert_eq!(
            prune_old_logs(&dir.join("missing"), 14, SystemTime::now()),
            0
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_end_line_matches_tracing_shape() {
        // 2024-02-29T12:00:00Z: exercises the leap-day path of the date math.
        let t = UNIX_EPOCH + Duration::from_secs(1_709_208_000);
        assert_eq!(
            session_end_line(false, t),
            "2024-02-29T12:00:00.000000Z  INFO lights_out::app: \
             windows session end requested (shutdown, restart, logoff or installer)\n"
        );
        assert!(session_end_line(true, t)
            .ends_with("  INFO lights_out::app: windows session end cancelled\n"));
    }

    #[test]
    fn session_end_append_creates_and_extends_today_file() {
        let dir = temp_dir("session_end");
        let t = UNIX_EPOCH + Duration::from_secs(1_709_208_000);
        append_session_end_line(&dir, false, t).unwrap();
        append_session_end_line(&dir, true, t).unwrap();

        let text = std::fs::read_to_string(dir.join("lights-out.2024-02-29.log")).unwrap();
        assert_eq!(text.matches("lights_out::app").count(), 2);
        assert!(text.contains("session end requested"));
        assert!(text.contains("session end cancelled"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

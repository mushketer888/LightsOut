//! Log retention: old daily log files are deleted.

use std::path::Path;
use std::time::{Duration, SystemTime};

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
}

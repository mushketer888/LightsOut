pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const GIT: &str = env!("LIGHTS_OUT_GIT");
pub const BUILD_DATE: &str = env!("LIGHTS_OUT_BUILD_DATE");
pub const REPO: &str = "https://github.com/mushketer888/LightsOut";

/// `lights-out 0.1.0 (abc1234, 2026-11-04)`
pub fn describe() -> String {
    format!("lights-out {VERSION} ({GIT}, {BUILD_DATE})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_contains_version_git_and_date() {
        let d = describe();
        assert!(d.starts_with("lights-out "));
        assert!(d.contains(VERSION));
        assert!(d.contains(GIT));
        assert!(d.contains(BUILD_DATE));
    }

    #[test]
    fn build_date_looks_like_iso_date() {
        let parts: Vec<&str> = BUILD_DATE.split('-').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].len(), 4);
        assert!(parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit())));
    }
}

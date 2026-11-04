use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Version([u64; 3]);

impl Version {
    fn parse(tag: &str) -> Result<Self> {
        let version = tag.strip_prefix('v').unwrap_or(tag);
        let (version, build) = version
            .split_once('+')
            .map_or((version, None), |(v, b)| (v, Some(b)));
        if let Some(build) = build {
            anyhow::ensure!(
                build.split('.').all(|part| !part.is_empty()
                    && part.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')),
                "Invalid build metadata"
            );
        }
        let mut parts = version.split('.');
        let mut numbers = [0; 3];
        for number in &mut numbers {
            let part = parts.next().context("Version needs three components")?;
            anyhow::ensure!(
                !part.is_empty()
                    && part.bytes().all(|c| c.is_ascii_digit())
                    && (part.len() == 1 || !part.starts_with('0')),
                "Not a stable semantic version: {tag}"
            );
            *number = part.parse().context("Version component exceeds u64")?;
        }
        anyhow::ensure!(parts.next().is_none(), "Version needs three components");
        Ok(Self(numbers))
    }
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    draft: bool,
}

#[derive(Deserialize)]
struct Tag {
    name: String,
}

fn release_tag(body: &[u8]) -> Result<(Version, String)> {
    let release: Release = serde_json::from_slice(body).context("Invalid release response")?;
    anyhow::ensure!(
        !release.prerelease && !release.draft,
        "Release is not stable"
    );
    Ok((Version::parse(&release.tag_name)?, release.tag_name))
}

fn latest_tag(body: &[u8]) -> Result<(Version, String)> {
    let tags: Vec<Tag> = serde_json::from_slice(body).context("Invalid tags response")?;
    tags.into_iter()
        .filter_map(|tag| Version::parse(&tag.name).ok().map(|v| (v, tag.name)))
        .max_by_key(|(version, _)| *version)
        .context("Tags response contains no valid stable versions")
}

fn decide_update(
    current: &str,
    release: Result<Vec<u8>>,
    tags: Result<Vec<u8>>,
) -> Result<Option<String>> {
    let current = Version::parse(current).context("Invalid installed version")?;
    let release = release.and_then(|body| release_tag(&body));
    let tags = tags.and_then(|body| latest_tag(&body));
    let newest = release
        .as_ref()
        .ok()
        .into_iter()
        .chain(tags.as_ref().ok())
        .max_by_key(|(version, _)| *version);
    if let Some((version, tag)) = newest {
        if *version > current {
            return Ok(Some(tag.clone()));
        }
    }
    // Releases can lag behind published tags. Only tags can confirm no newer tag exists.
    match tags {
        Ok(_) => Ok(None),
        Err(tags_error) => bail!(
            "Cannot confirm latest version: tags: {tags_error:#}; release: {}",
            release
                .err()
                .map_or_else(|| "no newer release".into(), |e| format!("{e:#}"))
        ),
    }
}

/// Blocking native update check. Call from the UI's background thread.
/// `None` confirms no newer stable version; network/parse failures are errors.
#[cfg(windows)]
pub fn check_for_updates(current: &str) -> Result<Option<String>> {
    Version::parse(current).context("Invalid installed version")?;
    let release = native::fetch("/repos/mushketer888/LightsOut/releases/latest", current);
    let tags = native::fetch("/repos/mushketer888/LightsOut/tags?per_page=100", current);
    decide_update(current, release, tags)
}

#[cfg(windows)]
mod native {
    use super::*;
    use std::{
        ffi::c_void,
        ptr,
        sync::mpsc,
        thread,
        time::{Duration, Instant},
    };
    use windows::{
        core::{w, PCWSTR},
        Win32::Networking::WinHttp::*,
    };

    const REQUEST_BUDGET: Duration = Duration::from_secs(5);
    const MAX_RESPONSE: usize = 1024 * 1024;

    struct Handle(*mut c_void);

    impl Handle {
        fn new(raw: *mut c_void) -> Result<Self> {
            if raw.is_null() {
                Err(std::io::Error::last_os_error().into())
            } else {
                Ok(Self(raw))
            }
        }

        fn timeout(&self, deadline: Instant) -> Result<()> {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .context("Update request timed out")?;
            // Keep each native operation short as well as bounding the caller's whole request.
            let millis = remaining.as_millis().clamp(1, 1000) as i32;
            unsafe {
                WinHttpSetTimeouts(self.0, millis, millis, millis, millis)?;
            }
            Ok(())
        }
    }

    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                let _ = WinHttpCloseHandle(self.0);
            }
        }
    }

    pub(super) fn fetch(path: &'static str, current: &str) -> Result<Vec<u8>> {
        let deadline = Instant::now() + REQUEST_BUDGET;
        let agent = format!("LightsOut/{current}");
        let (send, receive) = mpsc::sync_channel(1);
        // WinHTTP timeouts apply to individual phases, not the whole request. The
        // receiver deadline also bounds DNS/proxy stalls without racing handle closure.
        thread::Builder::new()
            .name("LightsOut update HTTP".into())
            .spawn(move || {
                let result = request(path, &agent, deadline);
                let _ = send.send(result);
            })
            .context("Cannot start native update request")?;
        receive
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|e| anyhow!("Update request failed or exceeded five seconds: {e}"))?
    }

    fn request(path: &str, agent: &str, deadline: Instant) -> Result<Vec<u8>> {
        let agent: Vec<u16> = agent.encode_utf16().chain(Some(0)).collect();
        let path: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        unsafe {
            let session = Handle::new(WinHttpOpen(
                PCWSTR(agent.as_ptr()),
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                PCWSTR::null(),
                PCWSTR::null(),
                0,
            ))?;
            session.timeout(deadline)?;
            let connection = Handle::new(WinHttpConnect(session.0, w!("api.github.com"), 443, 0))?;
            let request = Handle::new(WinHttpOpenRequest(
                connection.0,
                w!("GET"),
                PCWSTR(path.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                ptr::null(),
                WINHTTP_FLAG_SECURE,
            ))?;
            // Retain WinHTTP's default certificate/hostname verification, never follow redirects.
            WinHttpSetOption(
                Some(request.0),
                WINHTTP_OPTION_REDIRECT_POLICY,
                Some(&WINHTTP_OPTION_REDIRECT_POLICY_NEVER.to_ne_bytes()),
            )?;
            WinHttpSetOption(
                Some(request.0),
                WINHTTP_OPTION_CONNECT_RETRIES,
                Some(&0u32.to_ne_bytes()),
            )?;
            request.timeout(deadline)?;
            let headers: Vec<u16> = "Accept: application/vnd.github+json\r\n"
                .encode_utf16()
                .collect();
            WinHttpSendRequest(request.0, Some(&headers), None, 0, 0, 0)?;
            request.timeout(deadline)?;
            WinHttpReceiveResponse(request.0, ptr::null_mut())?;
            let mut status = 0u32;
            let mut size = std::mem::size_of_val(&status) as u32;
            WinHttpQueryHeaders(
                request.0,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                PCWSTR::null(),
                Some((&mut status as *mut u32).cast()),
                &mut size,
                ptr::null_mut(),
            )?;
            anyhow::ensure!(status == 200, "GitHub returned HTTP {status}");
            let mut body = Vec::new();
            let mut buffer = [0u8; 8192];
            // Also cap read calls, so even a one-byte trickle cannot loop indefinitely.
            for _ in 0..4096 {
                request.timeout(deadline)?;
                let mut read = 0u32;
                WinHttpReadData(
                    request.0,
                    buffer.as_mut_ptr().cast(),
                    buffer.len() as u32,
                    &mut read,
                )?;
                anyhow::ensure!(Instant::now() < deadline, "Update request timed out");
                if read == 0 {
                    return Ok(body);
                }
                anyhow::ensure!(
                    body.len() + read as usize <= MAX_RESPONSE,
                    "Update response exceeds 1 MiB"
                );
                body.extend_from_slice(&buffer[..read as usize]);
            }
            bail!("Update response exceeded read limit")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(json: &str) -> Result<Vec<u8>> {
        Ok(json.as_bytes().to_vec())
    }
    fn failed() -> Result<Vec<u8>> {
        Err(anyhow!("offline"))
    }

    #[test]
    fn stable_versions_are_numeric_and_exact() {
        assert!(Version::parse("v0.10.0").unwrap() > Version::parse("0.9.0").unwrap());
        assert!(
            Version::parse("1.0.9007199254740993").unwrap()
                > Version::parse("1.0.9007199254740992").unwrap()
        );
        assert_eq!(
            Version::parse("v1.2.3+build.7").unwrap(),
            Version::parse("1.2.3").unwrap()
        );
        for invalid in [
            "1.2",
            "1.2.3.4",
            "1.2.3-beta",
            "01.2.3",
            "1.2.18446744073709551616",
            "1.2.3+",
            "1.2.3+bad!",
            "",
        ] {
            assert!(Version::parse(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn release_and_tag_shapes_and_prereleases() {
        assert_eq!(
            release_tag(br#"{"tag_name":"v0.10.0","prerelease":false,"draft":false}"#)
                .unwrap()
                .1,
            "v0.10.0"
        );
        assert!(release_tag(br#"{"tag_name":"v1.0.0","prerelease":true}"#).is_err());
        assert!(release_tag(br#"{"tag_name":"v1.0.0","draft":true}"#).is_err());
        assert!(release_tag(br#"{"name":"v1.0.0"}"#).is_err());
        assert_eq!(
            latest_tag(br#"[{"name":"v3.0.0-beta"},{"name":"v0.9.0"},{"name":"v0.10.0"}]"#)
                .unwrap()
                .1,
            "v0.10.0"
        );
        for invalid in [
            "{}",
            "[]",
            "[{\"name\":\"v1.0.0-beta\"}]",
            "[{\"tag_name\":\"v1.0.0\"}]",
            "not json",
        ] {
            assert!(latest_tag(invalid.as_bytes()).is_err());
        }
    }

    #[test]
    fn both_failed_is_an_error() {
        assert!(decide_update("0.2.0", failed(), failed()).is_err());
    }

    #[test]
    fn newer_tags_win_over_stale_release() {
        assert_eq!(
            decide_update(
                "0.9.0",
                body(r#"{"tag_name":"v0.9.0"}"#),
                body(r#"[{"name":"v0.10.0"}]"#)
            )
            .unwrap(),
            Some("v0.10.0".into())
        );
    }

    #[test]
    fn tags_failure_cannot_confirm_current_release() {
        assert!(decide_update("0.2.0", body(r#"{"tag_name":"v0.2.0"}"#), failed()).is_err());
    }

    #[test]
    fn valid_tags_confirm_current_even_without_release() {
        assert_eq!(
            decide_update(
                "0.2.0",
                failed(),
                body(r#"[{"name":"v0.2.0"},{"name":"v0.3.0-rc1"}]"#)
            )
            .unwrap(),
            None
        );
        assert!(decide_update("0.2.0", failed(), body("[]")).is_err());
    }

    #[test]
    fn proven_new_release_survives_tags_failure() {
        assert_eq!(
            decide_update("0.2.0", body(r#"{"tag_name":"v0.3.0"}"#), failed()).unwrap(),
            Some("v0.3.0".into())
        );
    }
}

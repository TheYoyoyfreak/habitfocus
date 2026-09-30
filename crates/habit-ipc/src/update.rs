//! The newest release on GitHub, for habitd's daily check and `hf update`.
//! Asks through `curl`, which keeps TLS out of the static binaries; without
//! curl, or offline, there's simply no answer.

use crate::repo;

/// A published release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// "v0.2.0"
    pub tag: String,
    /// "0.2.0"
    pub version: String,
    /// The release page.
    pub url: String,
}

/// The version of this build.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// Asks GitHub for the latest release (never a pre-release).
pub fn latest() -> Result<Release, String> {
    let url = format!("https://api.github.com/repos/{}/releases/latest", repo());
    // The status code goes on a last line of its own.
    let output = std::process::Command::new("curl")
        .args(["-sS", "--max-time", "10", "-w", "\n%{http_code}", "-H", "Accept: application/vnd.github+json", &url])
        .output()
        .map_err(|e| format!("can't run curl: {e}"))?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        return Err(format!("couldn't reach GitHub: {}", error.trim()));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let (body, code) = text.rsplit_once('\n').unwrap_or(("", &text));
    match code.trim() {
        "200" => parse_release(body),
        "404" => Err(format!(
            "no release found at https://github.com/{}/releases (none published yet, or the repository isn't public)",
            repo()
        )),
        "403" | "429" => Err("GitHub is rate-limiting requests from this address; try again later".into()),
        other => Err(format!("GitHub answered with status {other}")),
    }
}

fn parse_release(json: &str) -> Result<Release, String> {
    let value: serde_json::Value = serde_json::from_str(json).map_err(|e| format!("unexpected answer from GitHub: {e}"))?;
    let tag = value.get("tag_name").and_then(|t| t.as_str()).ok_or("GitHub's answer has no tag_name")?;
    let url = value.get("html_url").and_then(|u| u.as_str()).unwrap_or_default();
    Ok(Release { tag: tag.to_string(), version: tag.trim_start_matches('v').to_string(), url: url.to_string() })
}

/// major.minor.patch of "v1.2.3" or "1.2.3-rc.1" (the suffix is ignored).
fn numbers(version: &str) -> Option<[u64; 3]> {
    let core = version.trim_start_matches('v').split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    let numbers = [parts.next()??, parts.next().unwrap_or(Some(0))?, parts.next().unwrap_or(Some(0))?];
    parts.next().is_none().then_some(numbers)
}

/// Whether `candidate` is a newer version than `current`.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (numbers(candidate), numbers(current)) {
        (Some(new), Some(old)) => new > old,
        _ => false,
    }
}

/// The latest release when it's newer than this build.
pub fn available() -> Result<Option<Release>, String> {
    latest().map(|release| is_newer(&release.version, CURRENT).then_some(release))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions_by_number() {
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("v0.10.0", "0.9.0"), "numbers, not text");
        assert!(is_newer("1.0", "0.9.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0-rc.1"));
        assert!(!is_newer("0.2.0-rc.1", "0.2.0"), "a pre-release of the same version isn't newer");
        assert!(!is_newer("nightly", "0.1.0"));
    }

    #[test]
    fn reads_githubs_answer() {
        let json = r#"{"tag_name": "v0.2.0", "html_url": "https://github.com/x/y/releases/tag/v0.2.0", "prerelease": false}"#;
        let release = parse_release(json).unwrap();
        assert_eq!((release.tag.as_str(), release.version.as_str()), ("v0.2.0", "0.2.0"));
        assert!(release.url.ends_with("/v0.2.0"));
        assert!(parse_release(r#"{"message": "Not Found"}"#).is_err());
        assert!(parse_release("<html>").is_err());
    }
}

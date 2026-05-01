//! Optional polling of GitHub Releases vs embedded crate version — **hint only** (log line).
//! Replacing the running container still requires Watchtower, compose pull on the host, or manual redeploy.

use tokio::time::{interval, Duration};
use tracing::{debug, warn};

fn parse_semver_triple(s: &str) -> Option<(u64, u64, u64)> {
    let t = s.trim().trim_start_matches('v');
    let mut parts = t.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

fn remote_semver_newer(remote: &str, local: &str) -> bool {
    match (parse_semver_triple(remote), parse_semver_triple(local)) {
        (Some(r), Some(l)) => r > l,
        _ => false,
    }
}

async fn fetch_latest_release_tag(repo: &str) -> Result<Option<String>, String> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let client = reqwest::Client::builder()
        .user_agent("bittice-engine-release-check")
        .build()
        .map_err(|e| e.to_string())?;

    let resp = client.get(&url).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("GitHub API HTTP {}", resp.status()));
    }
    let j: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let tag = j
        .get("tag_name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    Ok(tag)
}

/// Background task: compares [`env!("CARGO_PKG_VERSION")`] with latest GitHub release tag.
pub fn spawn_if_configured() {
    let secs: u64 = match std::env::var("BITTICE_RELEASE_CHECK_INTERVAL_SECS") {
        Ok(s) => s.trim().parse().unwrap_or(0),
        Err(_) => return,
    };
    if secs == 0 {
        return;
    }

    let repo = std::env::var("BITTICE_RELEASE_GITHUB_REPO")
        .unwrap_or_else(|_| "JulianRodelo11/bittice".to_string());
    let local_ver = env!("CARGO_PKG_VERSION").to_string();

    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(secs.max(60)));
        loop {
            ticker.tick().await;
            match fetch_latest_release_tag(&repo).await {
                Ok(Some(tag)) => {
                    let remote = tag.trim_start_matches('v');
                    if remote_semver_newer(remote, &local_ver) {
                        warn!(
                            "GitHub release {tag} is newer than this engine (v{}). Deploy a newer image when convenient.",
                            local_ver
                        );
                    }
                }
                Ok(None) => debug!("release check: no tag_name in response"),
                Err(e) => debug!("release check failed: {e}"),
            }
        }
    });
}

//! Local Docker runner — pulls the published GHCR image and runs it with local data mounts.
//! When sync profiles use OpenVPN, starts the same VPN sidecar stack as EC2 deploy.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// Find a directory with `Cargo.toml` naming package `bittice`, walking up from `start`.
pub fn find_bittice_project_root() -> Option<PathBuf> {
    let mut p = std::env::current_dir().ok()?;
    loop {
        let cargo = p.join("Cargo.toml");
        if cargo.is_file() {
            if let Ok(m) = std::fs::read_to_string(&cargo) {
                if m.lines().any(|l| l.trim() == "name = \"bittice\"") {
                    return Some(p);
                }
            }
        }
        p = p.parent()?.to_path_buf();
    }
}

fn which_ok(cmd: &str) -> bool {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {cmd}"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Verify Docker is available and running.
pub fn check_docker_prerequisites() -> Result<()> {
    if !which_ok("docker") {
        bail!("`docker` is not on PATH. Install Docker and try again.");
    }
    let v = std::process::Command::new("docker")
        .arg("info")
        .output()
        .context("docker info")?;
    if !v.status.success() {
        bail!("Docker is not running or not accessible. Start Docker and try again.");
    }
    Ok(())
}

/// Get the latest git tag (e.g. "v0.1.66") from the repository.
fn get_latest_git_tag(project_root: &Path) -> Result<String> {
    let out = std::process::Command::new("git")
        .args(["describe", "--tags", "--abbrev=0"])
        .current_dir(project_root)
        .output()
        .context("failed to run git describe")?;
    if !out.status.success() {
        bail!(
            "Could not determine latest git tag. Make sure you have tags in this repo.\n\
             Error: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let tag = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if tag.is_empty() {
        bail!("No git tags found. Push a tag (e.g. git tag v0.1.0) and try again.");
    }
    Ok(tag)
}

/// Detect the GitHub owner/repo from the origin remote URL.
fn detect_ghcr_repo(project_root: &Path) -> Result<String> {
    let out = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(project_root)
        .output()
        .context("failed to run git remote")?;
    if !out.status.success() {
        bail!("Could not detect GitHub repository from git remote.");
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let repo = if let Some(stripped) = url.strip_prefix("https://github.com/") {
        stripped.trim_end_matches(".git").to_lowercase()
    } else if let Some(stripped) = url.strip_prefix("git@github.com:") {
        stripped.trim_end_matches(".git").to_lowercase()
    } else {
        bail!("Unsupported git remote format: {url}");
    };
    Ok(repo)
}

const EC2_OVPN_NAME: &str = crate::core::data_paths::DEPLOY_OVPN_NAME;

/// `vpn.conf` for dperson/openvpn-client (from deploy profile or existing bundle file).
fn ensure_vpn_conf_for_compose(data_root: &Path) -> Result<()> {
    let vpn_dir = data_root.join("vpn");
    let conf = vpn_dir.join("vpn.conf");
    if conf.is_file() {
        return Ok(());
    }
    let bundled = vpn_dir.join(EC2_OVPN_NAME);
    if bundled.is_file() {
        std::fs::copy(&bundled, &conf).context("copy bittice-ec2.ovpn → vpn.conf")?;
        return Ok(());
    }
    bail!(
        "Sync profile uses VPN but {} is missing.\n\
         Use Deploy → add OpenVPN profile, or Deploy to cloud VM once (creates {EC2_OVPN_NAME}).",
        bundled.display()
    );
}

fn run_compose_local(project_root: &Path, data_root: &Path, image: &str) -> Result<()> {
    ensure_vpn_conf_for_compose(data_root)?;

    let compose_file = project_root.join("deploy/docker-compose.local.yaml");
    if !compose_file.is_file() {
        bail!("Missing {}", compose_file.display());
    }

    let _ = std::process::Command::new("docker")
        .args(["compose", "-f", compose_file.to_str().unwrap(), "down"])
        .current_dir(project_root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    let status = std::process::Command::new("docker")
        .args(["compose", "-f", compose_file.to_str().unwrap(), "up", "-d"])
        .current_dir(project_root)
        .env("BITTICE_IMAGE", image)
        .status()
        .context("docker compose up")?;
    if !status.success() {
        bail!("docker compose up failed.");
    }
    Ok(())
}

fn run_plain_container(data_root: &Path, image: &str) -> Result<()> {
    let _ = std::process::Command::new("docker")
        .args(["stop", "bittice", "bittice-local", "bittice-vpn-local"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let _ = std::process::Command::new("docker")
        .args(["rm", "bittice", "bittice-local", "bittice-vpn-local"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    let data_vol = format!("{}:/app/data", data_root.display());
    let status = std::process::Command::new("docker")
        .args([
            "run",
            "-d",
            "--name",
            "bittice",
            "-p",
            "3000:3000",
            "-p",
            "8080:8080",
            "-p",
            "50051:50051",
            "-e",
            "BITTICE_ENGINE_ONLY=1",
            "-e",
            "BITTICE_HOST=0.0.0.0",
            "-e",
            "BITTICE_CDC_HEALTH_CHECK_MAX_FAILURES=0",
            "-e",
            "BITTICE_CDC_STREAM_SILENCE_TIMEOUT_SECS=90",
            "-v",
            &data_vol,
            image,
        ])
        .status()
        .context("docker run")?;
    if !status.success() {
        bail!("docker run failed.");
    }
    Ok(())
}

/// Run the Bittice container locally using the published GHCR image for the latest tag.
/// Uses VPN sidecar compose when any CDC profile references `vpn_file`.
pub fn run_local_docker_container(project_root: &Path) -> Result<()> {
    check_docker_prerequisites()?;

    let tag = get_latest_git_tag(project_root)?;
    let repo = detect_ghcr_repo(project_root)?;
    let image = format!("ghcr.io/{}:{}", repo, tag);

    println!("\n\x1b[34m→\x1b[0m  Pulling image \x1b[1m{}\x1b[0m…\n", image);
    let pull = std::process::Command::new("docker")
        .args(["pull", &image])
        .status()
        .context("docker pull")?;
    if !pull.success() {
        bail!("docker pull failed. Make sure the image exists in GHCR and you are logged in.");
    }

    let data_root = std::env::var(crate::core::data_paths::ENV_DATA_ROOT)
        .ok()
        .and_then(|v| {
            let t = v.trim();
            if t.is_empty() {
                return None;
            }
            let p = PathBuf::from(t);
            Some(if p.is_absolute() {
                p
            } else {
                project_root.join(p)
            })
        })
        .unwrap_or_else(|| project_root.join("data"));

    let with_vpn = crate::core::data_paths::deploy_requires_vpn_sidecar(&data_root);

    if with_vpn {
        println!(
            "\x1b[34m→\x1b[0m  VPN sidecar enabled (sync profile uses OpenVPN).\n"
        );
    }

    if with_vpn {
        run_compose_local(project_root, &data_root, &image)?;
    } else {
        run_plain_container(&data_root, &image)?;
    }

    let container = if with_vpn { "bittice-local" } else { "bittice" };
    println!(
        "\n\x1b[32m◆\x1b[0m  Container \x1b[1m{}\x1b[0m is running with image \x1b[1m{}\x1b[0m",
        container, image
    );
    println!("\n\x1b[90m│\x1b[0m  View logs:    docker logs -f {}", container);
    if with_vpn {
        println!("\x1b[90m│\x1b[0m  VPN logs:     docker logs -f bittice-vpn-local");
    }
    println!("\x1b[90m│\x1b[0m  Stop:         docker compose -f deploy/docker-compose.local.yaml down");
    Ok(())
}

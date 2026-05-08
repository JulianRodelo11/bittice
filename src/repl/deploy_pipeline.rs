//! Local Docker runner — pulls the published GHCR image and runs it with local data/vpn mounts.

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
    // Parse https://github.com/OWNER/REPO.git or git@github.com:OWNER/REPO.git
    let repo = if let Some(stripped) = url.strip_prefix("https://github.com/") {
        stripped.trim_end_matches(".git").to_lowercase()
    } else if let Some(stripped) = url.strip_prefix("git@github.com:") {
        stripped.trim_end_matches(".git").to_lowercase()
    } else {
        bail!("Unsupported git remote format: {url}");
    };
    Ok(repo)
}

/// Run the Bittice container locally using the published GHCR image for the latest tag.
/// Mounts local `data/` and optionally `vpn/` from the project root.
pub fn run_local_docker_container(project_root: &Path, use_vpn: bool) -> Result<()> {
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

    // Stop and remove any existing container named "bittice"
    let _ = std::process::Command::new("docker")
        .args(["stop", "bittice"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let _ = std::process::Command::new("docker")
        .args(["rm", "bittice"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    let data_root = std::env::var(crate::core::data_paths::ENV_DATA_ROOT)
        .ok()
        .and_then(|v| {
            let t = v.trim();
            if t.is_empty() { return None; }
            let p = PathBuf::from(t);
            Some(if p.is_absolute() { p } else { project_root.join(p) })
        })
        .unwrap_or_else(|| project_root.join("data"));

    let data_vol = format!("{}:/app/data", data_root.display());

    let mut args: Vec<String> = vec![
        "run".into(), "-d".into(), "--name".into(), "bittice".into(),
        "-p".into(), "3000:3000".into(),
        "-p".into(), "8080:8080".into(),
        "-p".into(), "50051:50051".into(),
        "-e".into(), "BITTICE_ENGINE_ONLY=1".into(),
        "-e".into(), "BITTICE_HOST=0.0.0.0".into(),
        "-v".into(), data_vol,
    ];

    if use_vpn {
        let vpn_vol = format!("{}:/app/vpn", data_root.join("vpn").display());
        let setup_cmd = "mkdir -p /dev/net && (test -c /dev/net/tun || mknod /dev/net/tun c 10 200) && chmod 600 /dev/net/tun && exec bittice";
        args.extend_from_slice(&[
            "--privileged".into(),
            "--cap-add".into(), "NET_ADMIN".into(),
            "--device".into(), "/dev/net/tun".into(),
            "--dns".into(), "8.8.8.8".into(),
            "--dns".into(), "8.8.4.4".into(),
            "-v".into(), vpn_vol,
            "--entrypoint".into(), "/bin/sh".into(),
        ]);
        args.push(image.clone());
        args.push("-c".into());
        args.push(setup_cmd.into());
    } else {
        args.push(image.clone());
    }

    println!("\n\x1b[34m→\x1b[0m  Starting container…\n");
    let run = std::process::Command::new("docker")
        .args(args.iter().map(|s| s.as_str()))
        .status()
        .context("docker run")?;
    if !run.success() {
        bail!("docker run failed.");
    }

    println!(
        "\n\x1b[32m◆\x1b[0m  Container \x1b[1mbittice\x1b[0m is running with image \x1b[1m{}\x1b[0m",
        image
    );
    println!("\n\x1b[90m│\x1b[0m  View logs:    docker logs -f bittice");
    println!("\x1b[90m│\x1b[0m  Stop:         docker stop bittice && docker rm bittice");
    println!("\x1b[90m│\x1b[0m  Restart:      docker start bittice");
    Ok(())
}

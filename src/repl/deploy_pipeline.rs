//! Full local deploy: build image from `Dockerfile.from-source`, export bundle, `docker save` to remote, extract, `docker compose up`.
//! Requires Docker, OpenSSH (ssh, scp), bash, and tar on the path.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{bail, Context, Result};

/// How to run `docker build` for the target instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildPlatform {
    /// For most x86 cloud VMs
    LinuxAmd64,
    /// e.g. AWS Graviton
    LinuxArm64,
    /// `docker build` with the host’s default platform
    HostNative,
}

/// Parameters collected by the REPL; see [`run_full_deploy`].
pub struct FullDeployConfig {
    pub project_root: PathBuf,
    pub local_image: String,
    pub ssh_target: String,
    /// Directory under the remote home (e.g. `bittice-run` → `~/bittice-run`)
    pub remote_subdir: String,
    pub platform: BuildPlatform,
}

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

pub fn check_prerequisites() -> Result<()> {
    for c in ["docker", "ssh", "bash", "tar", "python3"] {
        if !which_ok(c) {
            bail!("`{c}` is not on PATH. Install it and try again.");
        }
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

fn run_status(name: &str, args: &[&str], cwd: &Path) -> Result<()> {
    let s = std::process::Command::new(name)
        .args(args)
        .current_dir(cwd)
        .status()
        .with_context(|| format!("failed to run {name}"))?;
    if !s.success() {
        bail!("`{} {}` exited with {}", name, args.join(" "), s);
    }
    Ok(())
}

fn run_ssh(ssh: &str, remote_shell: &str) -> Result<()> {
    let s = std::process::Command::new("ssh")
        .arg(ssh)
        .arg(remote_shell)
        .status()
        .with_context(|| format!("ssh {ssh}"))?;
    if !s.success() {
        bail!("ssh command failed: {s}");
    }
    Ok(())
}

/// Build a runtime image. Uses buildx for non-native Linux targets.
pub fn docker_build_from_source(
    project_root: &Path,
    tag: &str,
    platform: BuildPlatform,
) -> Result<()> {
    let from_source = project_root
        .join("deploy")
        .join("Dockerfile.from-source");
    if !from_source.is_file() {
        bail!(
            "Missing {}. Are you in the Bittice repository root?",
            from_source.display()
        );
    }

    match platform {
        BuildPlatform::HostNative => {
            println!("\n\x1b[34m→\x1b[0m  docker build (host platform) — this can take several minutes on first run…\n");
            run_status(
                "docker",
                &["build", "-f", "deploy/Dockerfile.from-source", "-t", tag, "."],
                project_root,
            )?;
        }
        BuildPlatform::LinuxAmd64 | BuildPlatform::LinuxArm64 => {
            let pl = match platform {
                BuildPlatform::LinuxAmd64 => "linux/amd64",
                BuildPlatform::LinuxArm64 => "linux/arm64",
                BuildPlatform::HostNative => unreachable!(),
            };
            if !std::process::Command::new("docker")
                .args(["buildx", "version"])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
            {
                bail!("`docker buildx` is not available. Install/enable buildx, or pick “Native (this machine)”.");
            }
            println!("\n\x1b[34m→\x1b[0m  docker buildx for {pl} (first time may be slow)…\n");
            run_status(
                "docker",
                &[
                    "buildx",
                    "build",
                    "--load",
                    "--platform",
                    pl,
                    "-f",
                    "deploy/Dockerfile.from-source",
                    "-t",
                    tag,
                    ".",
                ],
                project_root,
            )?;
        }
    }
    Ok(())
}

/// Runs `export-server-bundle.sh` with `BITTICE_IMAGE` set. Destroys `out_dir` if it exists, then recreates.
pub fn run_export_server_bundle(
    project_root: &Path,
    out_dir: &Path,
    bitice_image: &str,
) -> Result<()> {
    let script = project_root
        .join("deploy")
        .join("scripts")
        .join("export-server-bundle.sh");
    if !script.is_file() {
        bail!("Missing script {}", script.display());
    }
    if out_dir.exists() {
        std::fs::remove_dir_all(out_dir)
            .with_context(|| format!("remove {}", out_dir.display()))?;
    }
    std::fs::create_dir_all(out_dir).context("create bundle dir")?;

    let s = std::process::Command::new("bash")
        .arg(script.to_string_lossy().as_ref())
        .arg(out_dir.as_os_str())
        .env("BITTICE_IMAGE", bitice_image)
        .env("BITTICE_PROJECT_ROOT", project_root)
        .current_dir(project_root)
        .status()
        .context("export-server-bundle.sh")?;
    if !s.success() {
        bail!("export-server-bundle.sh failed with {s}");
    }
    Ok(())
}

/// Pipes the image to the remote with `docker save | ssh DOCKER load`.
pub fn transfer_image_to_remote(ssh: &str, local_image: &str) -> Result<()> {
    println!("\n\x1b[34m→\x1b[0m  docker save … | ssh … docker load (large upload, be patient)…\n");
    let mut save = std::process::Command::new("docker")
        .args(["save", local_image])
        .stdout(Stdio::piped())
        .spawn()
        .context("docker save")?;
    let out = save.stdout.take().context("pipe")?;

    let st = std::process::Command::new("ssh")
        .arg(ssh)
        .arg("docker load")
        .stdin(out)
        .status()
        .context("ssh docker load")?;
    let save_st = save.wait().context("docker save wait")?;
    if !save_st.success() {
        bail!("docker save failed: {save_st}");
    }
    if !st.success() {
        bail!("ssh 'docker load' failed: {st}");
    }
    Ok(())
}

/// Copies the bundle to `~/{remote_subdir}/` on the remote using tar+ssh to preserve hidden files.
pub fn copy_bundle_to_remote(ssh: &str, local_bundle: &Path, remote_subdir: &str) -> Result<()> {
    if !local_bundle.is_dir() {
        bail!("bundle is not a directory: {}", local_bundle.display());
    }
    let setup = format!("rm -rf $HOME/{} && mkdir -p $HOME/{}", remote_subdir, remote_subdir);
    run_ssh(ssh, &setup)?;

    let mut tar = std::process::Command::new("tar")
        .arg("-C")
        .arg(local_bundle)
        .arg("-cf")
        .arg("-")
        .arg(".")
        .stdout(Stdio::piped())
        .spawn()
        .context("tar create")?;
    let tout = tar.stdout.take().context("tar stdout")?;
    let remote_tarx = format!("tar -C $HOME/{} -xf -", remote_subdir);
    let st = std::process::Command::new("ssh")
        .arg(ssh)
        .arg(&remote_tarx)
        .stdin(tout)
        .status()
        .context("ssh tar")?;
    let tar_st = tar.wait().context("tar wait")?;
    if !tar_st.success() {
        bail!("local tar failed: {tar_st}");
    }
    if !st.success() {
        bail!("remote tar extract failed: {st}");
    }
    Ok(())
}

/// Runs `docker compose` on the remote in `~/remote_subdir`.
pub fn start_compose_on_remote(ssh: &str, remote_subdir: &str) -> Result<()> {
    let cmd = format!(
        "cd $HOME/{} && docker compose -f docker-compose.yaml --env-file .env up -d",
        remote_subdir
    );
    run_ssh(ssh, &cmd)
}

/// Full pipeline: build → export → save|load → tar bundle → compose up.
pub fn run_full_deploy(cfg: &FullDeployConfig) -> Result<()> {
    check_prerequisites()?;
    if !cfg.project_root.join("data").is_dir() {
        bail!("No data/ directory in {}. Sync at least one entity first.", cfg.project_root.display());
    }
    for s in [cfg.ssh_target.as_str(), &cfg.local_image, &cfg.remote_subdir] {
        if s.contains('\'') || s.contains('\"') {
            bail!("Invalid characters in parameters (no quotes).");
        }
    }
    if !cfg.remote_subdir.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("Use only letters, digits, - and _ for the remote directory name.");
    }

    let staging = cfg
        .project_root
        .join(".bittice-ssh-staging");
    if staging.exists() {
        let _ = std::fs::remove_dir_all(&staging);
    }

    docker_build_from_source(&cfg.project_root, &cfg.local_image, cfg.platform)?;
    run_export_server_bundle(
        &cfg.project_root,
        &staging,
        &cfg.local_image,
    )?;
    transfer_image_to_remote(&cfg.ssh_target, &cfg.local_image)?;
    copy_bundle_to_remote(
        &cfg.ssh_target,
        &staging,
        &cfg.remote_subdir,
    )?;
    println!("\n\x1b[34m→\x1b[0m  docker compose on remote…\n");
    start_compose_on_remote(&cfg.ssh_target, &cfg.remote_subdir)?;

    let _ = std::fs::remove_dir_all(&staging);
    println!("\n\x1b[32m◆\x1b[0m  Deploy finished. Check: ssh {} 'docker ps && docker logs bittice'.\n", cfg.ssh_target);
    Ok(())
}

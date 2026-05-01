//! Full local deploy: build image from `Dockerfile.from-source`, `docker save` to remote, export a **small**
//! bundle (compose + `.env` + VPN dir — no embedded `data/`), **`rsync` `data/`** (incremental / resumable for
//! large mirrors), `docker compose up`, brief stop, **delta `rsync`**, start again.
//!
//! **`run_ssh_engine_image_refresh`** — when `~/remote_subdir` already has compose + `.env`: rebuild →
//! `docker save | ssh docker load` → optional `rsync data/` → `compose up --force-recreate`. On a **single EC2**
//! expect a **short TCP gap** during container swap (typically seconds); true zero-downtime needs two instances + LB.
//!
//! **Networking:** sync profiles **without** `vpn_file` need nothing extra for EC2. If `vpn_file` is set,
//! only **OpenVPN** `.ovpn` profiles are supported today — those files must live under `data/vpn/` or repo `vpn/`
//! before deploy so the bundle mounts `./vpn` correctly.
//!
//! Full SSH deploy needs Docker, OpenSSH (`ssh`), **`rsync`**, bash, tar, and python3. Image-only refresh needs
//! Docker, ssh, and bash ([`check_prerequisites_image_update`]).

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
    check_prerequisites_image_update()?;
    for c in ["rsync", "tar", "python3"] {
        if !which_ok(c) {
            bail!("`{c}` is not on PATH. Install it and try again.");
        }
    }
    Ok(())
}

/// Minimal toolchain for **image-only** SSH refresh (`docker save | ssh docker load`, compose recreate).
pub fn check_prerequisites_image_update() -> Result<()> {
    for c in ["docker", "ssh", "bash"] {
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

/// Data directory used for deploy (honours `BITTICE_DATA_ROOT`, otherwise `<project>/data`).
fn resolve_deploy_data_root(project_root: &Path) -> PathBuf {
    std::env::var(crate::core::data_paths::ENV_DATA_ROOT)
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
        .unwrap_or_else(|| project_root.join("data"))
}

/// When **`vpn_file` is set** in `cdc_config.json`, checks that the referenced `.ovpn` exists under
/// `data/vpn/` or repo `vpn/` so EC2 receives it (`./vpn` + rsync). Profiles **without** `vpn_file` are ignored.
/// Supported tunnel today: **OpenVPN only**.
fn ensure_openvpn_profiles_for_ec2_deploy(project_root: &Path) -> Result<()> {
    let data_root = resolve_deploy_data_root(project_root);
    let configs =
        crate::core::data_paths::scan_all_cdc_config_paths_in_data_root(&data_root);

    let mut errors: Vec<String> = Vec::new();

    for cfg_path in configs {
        let txt = match std::fs::read_to_string(&cfg_path) {
            Ok(t) => t,
            Err(e) => {
                errors.push(format!("{} (read failed: {e})", cfg_path.display()));
                continue;
            }
        };
        let j: serde_json::Value = match serde_json::from_str(&txt) {
            Ok(v) => v,
            Err(e) => {
                errors.push(format!("{} (invalid JSON: {e})", cfg_path.display()));
                continue;
            }
        };

        let Some(vpn_raw) = j.get("vpn_file").and_then(|v| v.as_str()).filter(|s| !s.is_empty())
        else {
            continue;
        };

        let basename = Path::new(vpn_raw)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(vpn_raw);

        let in_data_vpn = data_root.join("vpn").join(basename).is_file();
        let in_repo_vpn = project_root.join("vpn").join(basename).is_file();
        if in_data_vpn || in_repo_vpn {
            continue;
        }

        let raw_exists = Path::new(vpn_raw).is_file();
        if raw_exists {
            errors.push(format!(
                "{} — uses OpenVPN (`vpn_file`) on EC2; put '{}' in data/vpn/ (Deploy → Add OpenVPN profile) or repo vpn/ — file is only at {}",
                cfg_path.display(),
                basename,
                vpn_raw
            ));
        } else {
            errors.push(format!(
                "{} — `vpn_file` references '{}' but that OpenVPN profile is missing; add it via Deploy → Add OpenVPN profile (stores under data/vpn/)",
                cfg_path.display(),
                vpn_raw
            ));
        }
    }

    if errors.is_empty() {
        return Ok(());
    }

    bail!(
        "Some CDC profiles use OpenVPN (`vpn_file` set) but the .ovpn is not packaged for EC2:\n\n{}\n\n\
         Connections **without** `vpn_file` do not need VPN files.\n\
         When `vpn_file` is set, only OpenVPN profiles are supported — copy each .ovpn into data/vpn/ (Deploy menu) or repo vpn/ before deploy.",
        errors.join("\n")
    )
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
/// When `skip_data_copy`, the bundle omits mirroring `data/` (SSH deploy streams it with `rsync` instead).
pub fn run_export_server_bundle(
    project_root: &Path,
    out_dir: &Path,
    bitice_image: &str,
    skip_data_copy: bool,
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

    let mut cmd = std::process::Command::new("bash");
    cmd.arg(script.to_string_lossy().as_ref())
        .arg(out_dir.as_os_str())
        .env("BITTICE_IMAGE", bitice_image)
        .env("BITTICE_PROJECT_ROOT", project_root)
        .current_dir(project_root);
    if skip_data_copy {
        cmd.env("BITTICE_EXPORT_BUNDLE_SKIP_DATA", "1");
    }
    let s = cmd.status().context("export-server-bundle.sh")?;
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

/// Stops compose services so filesystem mirrors under `data/` can be replaced safely.
pub fn stop_compose_on_remote(ssh: &str, remote_subdir: &str) -> Result<()> {
    let cmd = format!(
        "cd $HOME/{} && docker compose -f docker-compose.yaml --env-file .env stop",
        remote_subdir
    );
    run_ssh(ssh, &cmd)
}

/// `rsync` `project_root/data/` → `~/remote_subdir/data/`. Incremental and resumable (`--partial`); large
/// files use `--inplace` on the remote to reduce spare disk spikes. Runs twice in [`run_full_deploy`]:
/// once before compose, once after `compose stop` for a delta.
pub fn rsync_project_data_dir_to_remote(
    ssh: &str,
    project_root: &Path,
    remote_subdir: &str,
    phase: &str,
) -> Result<()> {
    let data_dir = project_root.join("data");
    if !data_dir.is_dir() {
        bail!(
            "No data/ directory under {}",
            project_root.display()
        );
    }

    println!(
        "\n\x1b[34m→\x1b[0m  rsync data/ → server ({phase}) — large trees resume if interrupted…\n"
    );

    let remote_prep = format!("mkdir -p $HOME/{}/data", remote_subdir);
    run_ssh(ssh, &remote_prep)?;

    let dest = format!("{ssh}:~/{remote_subdir}/data/");
    run_status(
        "rsync",
        &[
            "-a",
            "--partial",
            "--inplace",
            "--info=progress2",
            "-e",
            "ssh",
            "data/",
            dest.as_str(),
        ],
        project_root,
    )?;
    Ok(())
}

fn verify_remote_toolchain(ssh: &str) -> Result<()> {
    let cmd = "command -v rsync >/dev/null 2>&1 || { echo 'remote host: install rsync (e.g. sudo apt install rsync / sudo yum install rsync)' >&2; exit 1; }";
    run_ssh(ssh, cmd)
}

fn verify_remote_has_docker_compose(ssh: &str) -> Result<()> {
    run_ssh(
        ssh,
        "command -v docker >/dev/null 2>&1 && docker compose version >/dev/null 2>&1",
    )
    .context("remote host needs Docker Engine and Compose v2 (`docker compose`)")?;
    Ok(())
}

fn verify_remote_deploy_bundle_ready(ssh: &str, remote_subdir: &str) -> Result<()> {
    let cmd = format!(
        "cd $HOME/{} && test -f docker-compose.yaml && test -f .env",
        remote_subdir
    );
    run_ssh(ssh, &cmd).context(
        "remote folder is missing docker-compose.yaml or .env — run “full SSH deploy” once first",
    )?;
    Ok(())
}

/// Replace running containers with a freshly `docker load`’d image for the same tag (`--force-recreate`).
pub fn recreate_engine_container_on_remote(ssh: &str, remote_subdir: &str) -> Result<()> {
    let cmd = format!(
        "cd $HOME/{} && docker compose -f docker-compose.yaml --env-file .env up -d --force-recreate --no-deps bittice",
        remote_subdir
    );
    run_ssh(ssh, &cmd)
}

/// Rebuild locally → `docker save | ssh docker load` → optional rsync `data/` → `--force-recreate`.  
/// Keeps `~/remote_subdir`; on a **single EC2** there is still a **short cutover** while the container restarts (typically seconds).
pub fn run_ssh_engine_image_refresh(cfg: &FullDeployConfig, sync_data_from_laptop: bool) -> Result<()> {
    check_prerequisites_image_update()?;
    for s in [cfg.ssh_target.as_str(), &cfg.local_image, &cfg.remote_subdir] {
        if s.contains('\'') || s.contains('\"') {
            bail!("Invalid characters in parameters (no quotes).");
        }
    }
    if !cfg.remote_subdir.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("Use only letters, digits, - and _ for the remote directory name.");
    }

    verify_remote_has_docker_compose(&cfg.ssh_target)?;
    verify_remote_deploy_bundle_ready(&cfg.ssh_target, &cfg.remote_subdir)?;

    if sync_data_from_laptop {
        let deploy_data = resolve_deploy_data_root(&cfg.project_root);
        if !deploy_data.is_dir() {
            bail!(
                "No data directory at {}. Pick “image only” or create data first.",
                deploy_data.display()
            );
        }
        ensure_openvpn_profiles_for_ec2_deploy(&cfg.project_root)?;
        verify_remote_toolchain(&cfg.ssh_target)?;
    }

    println!("\n\x1b[34m→\x1b[0m  docker build (same tag overwrites locally; server gets it on load)…\n");
    docker_build_from_source(&cfg.project_root, &cfg.local_image, cfg.platform)?;
    println!(
        "\n\x1b[90m│\x1b[0m  Current EC2 container keeps serving until recreate runs after image upload.\x1b[0m"
    );
    transfer_image_to_remote(&cfg.ssh_target, &cfg.local_image)?;

    if sync_data_from_laptop {
        println!("\n\x1b[34m→\x1b[0m  Stopping remote stack briefly for rsync of data/…\n");
        stop_compose_on_remote(&cfg.ssh_target, &cfg.remote_subdir)?;
        rsync_project_data_dir_to_remote(
            &cfg.ssh_target,
            &cfg.project_root,
            &cfg.remote_subdir,
            "refresh — merge local data/ before new container",
        )?;
    }

    println!(
        "\n\x1b[34m→\x1b[0m  docker compose up --force-recreate (expect a brief TCP gap on one host).\n"
    );
    recreate_engine_container_on_remote(&cfg.ssh_target, &cfg.remote_subdir)?;

    println!(
        "\n\x1b[32m◆\x1b[0m  Image refresh done. Logs: ssh {} 'docker logs -f --tail 80 bittice'\n",
        cfg.ssh_target
    );
    Ok(())
}

/// Full pipeline: build → save|load → lite bundle → tar bundle → rsync data → compose up → stop → rsync delta → start.
pub fn run_full_deploy(cfg: &FullDeployConfig) -> Result<()> {
    check_prerequisites()?;
    for s in [cfg.ssh_target.as_str(), &cfg.local_image, &cfg.remote_subdir] {
        if s.contains('\'') || s.contains('\"') {
            bail!("Invalid characters in parameters (no quotes).");
        }
    }
    if !cfg.remote_subdir.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("Use only letters, digits, - and _ for the remote directory name.");
    }

    let deploy_data = resolve_deploy_data_root(&cfg.project_root);
    if !deploy_data.is_dir() {
        bail!(
            "No data directory at {}. Sync at least one entity first.",
            deploy_data.display()
        );
    }

    ensure_openvpn_profiles_for_ec2_deploy(&cfg.project_root)?;

    let staging = cfg
        .project_root
        .join(".bittice-ssh-staging");
    if staging.exists() {
        let _ = std::fs::remove_dir_all(&staging);
    }

    verify_remote_toolchain(&cfg.ssh_target)?;

    docker_build_from_source(&cfg.project_root, &cfg.local_image, cfg.platform)?;
    // Large upload first; bundle export runs afterward so data/vpn/query snapshots are fresher than the old order.
    transfer_image_to_remote(&cfg.ssh_target, &cfg.local_image)?;
    run_export_server_bundle(
        &cfg.project_root,
        &staging,
        &cfg.local_image,
        true,
    )?;
    copy_bundle_to_remote(
        &cfg.ssh_target,
        &staging,
        &cfg.remote_subdir,
    )?;
    rsync_project_data_dir_to_remote(
        &cfg.ssh_target,
        &cfg.project_root,
        &cfg.remote_subdir,
        "initial — mirrors + queries before first start",
    )?;
    println!("\n\x1b[34m→\x1b[0m  docker compose on remote…\n");
    start_compose_on_remote(&cfg.ssh_target, &cfg.remote_subdir)?;

    stop_compose_on_remote(&cfg.ssh_target, &cfg.remote_subdir)?;
    rsync_project_data_dir_to_remote(
        &cfg.ssh_target,
        &cfg.project_root,
        &cfg.remote_subdir,
        "delta — changes during image upload / first sync",
    )?;
    println!("\n\x1b[34m→\x1b[0m  Starting containers again after data refresh…\n");
    start_compose_on_remote(&cfg.ssh_target, &cfg.remote_subdir)?;

    let _ = std::fs::remove_dir_all(&staging);
    println!("\n\x1b[32m◆\x1b[0m  Deploy finished. Check: ssh {} 'docker ps && docker logs bittice'.\n", cfg.ssh_target);
    Ok(())
}

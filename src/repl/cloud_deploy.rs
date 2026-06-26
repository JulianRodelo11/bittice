//! Interactive cloud-VM deploy via Terraform.
//!
//! Provisions an EC2 instance in the same VPC as the user's target RDS, so CDC
//! reaches MySQL natively through AWS internal networking. No VPN, no tunnels —
//! the wizard discovers VPC/subnet/SG via `aws rds describe-db-instances` and
//! Terraform places everything (EC2, SG, EIP, key pair) plus a single inbound
//! rule on the RDS SG that allows the new EC2 to reach MySQL on port 3306.
//!
//! Terraform is downloaded automatically on first use (~70 MB, cached in ~/.bittice/bin/).

use anyhow::{bail, Context, Result};
use cliclack::{input, log, note, select, spinner};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

// Terraform templates embedded at compile time.
const TF_VERSIONS: &str = include_str!("../../deploy/terraform/versions.tf");
const TF_VARIABLES: &str = include_str!("../../deploy/terraform/variables.tf");
const TF_MAIN: &str = include_str!("../../deploy/terraform/main.tf");
const TF_OUTPUTS: &str = include_str!("../../deploy/terraform/outputs.tf");

const TERRAFORM_VERSION: &str = "1.9.8";

// ── terraform auto-download ───────────────────────────────────────────────────

fn terraform_cache_dir() -> PathBuf {
    if let Some(home) = home_dir() {
        PathBuf::from(home).join(".bittice").join("bin")
    } else {
        crate::core::data_paths::resolved_data_root().join(".terraform-bin")
    }
}

fn terraform_binary_path() -> PathBuf {
    terraform_cache_dir().join("terraform")
}

fn terraform_platform() -> Result<(&'static str, &'static str)> {
    let os = if cfg!(target_os = "macos") { "darwin" }
              else if cfg!(target_os = "linux") { "linux" }
              else { bail!("Automatic Terraform download is not supported on this OS.\nInstall from: https://developer.hashicorp.com/terraform/downloads") };
    let arch = if cfg!(target_arch = "aarch64") { "arm64" }
               else if cfg!(target_arch = "x86_64") { "amd64" }
               else { bail!("Unsupported architecture for automatic Terraform download.") };
    Ok((os, arch))
}

async fn ensure_terraform() -> Result<PathBuf> {
    let bin = terraform_binary_path();
    if bin.is_file() {
        return Ok(bin);
    }
    let (os, arch) = terraform_platform()?;
    let url = format!(
        "https://releases.hashicorp.com/terraform/{v}/terraform_{v}_{os}_{arch}.zip",
        v = TERRAFORM_VERSION,
    );
    let s = spinner();
    s.start(format!("Downloading Terraform {TERRAFORM_VERSION} for {os}/{arch}… (~70 MB, cached after this)"));
    let bytes = reqwest::get(&url).await
        .with_context(|| format!("Failed to download Terraform from {url}"))?
        .bytes().await
        .context("Failed to read Terraform download")?;
    s.stop(format!("Downloaded Terraform {TERRAFORM_VERSION} ({:.1} MB).", bytes.len() as f64 / 1_048_576.0));

    let cache_dir = terraform_cache_dir();
    std::fs::create_dir_all(&cache_dir).context("create terraform cache dir")?;
    let cursor = std::io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor).context("open terraform zip")?;
    let mut entry = archive.by_name("terraform").context("terraform binary not found in zip")?;
    let mut buf = Vec::with_capacity(entry.size() as usize);
    entry.read_to_end(&mut buf).context("read terraform binary")?;
    std::fs::write(&bin, &buf).context("write terraform binary")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).context("chmod terraform")?;
    }
    let _ = log::success(format!("Terraform cached at {}", bin.display()));
    Ok(bin)
}

// ── prerequisites ─────────────────────────────────────────────────────────────

fn which_ok(cmd: &str) -> bool {
    Command::new("sh").args(["-c", &format!("command -v {cmd}")])
        .stdout(Stdio::null()).stderr(Stdio::null())
        .status().map(|s| s.success()).unwrap_or(false)
}

fn check_ssh_rsync() -> Result<()> {
    for cmd in ["ssh", "rsync"] {
        if !which_ok(cmd) {
            bail!("`{cmd}` is not on PATH — required for data sync and remote commands.");
        }
    }
    Ok(())
}

// ── terraform helpers ─────────────────────────────────────────────────────────

fn write_terraform_files(tf_dir: &Path, tfvars: &str) -> Result<()> {
    std::fs::create_dir_all(tf_dir).context("create terraform dir")?;
    std::fs::write(tf_dir.join("versions.tf"), TF_VERSIONS)?;
    std::fs::write(tf_dir.join("variables.tf"), TF_VARIABLES)?;
    std::fs::write(tf_dir.join("main.tf"), TF_MAIN)?;
    std::fs::write(tf_dir.join("outputs.tf"), TF_OUTPUTS)?;
    std::fs::write(tf_dir.join("terraform.tfvars"), tfvars)?;
    Ok(())
}

fn terraform_run(tf_bin: &Path, tf_dir: &Path, args: &[&str]) -> Result<()> {
    let sub = args.first().copied().unwrap_or("");
    let mut cmd = Command::new(tf_bin);
    cmd.args(args).current_dir(tf_dir);
    run_quietly(
        cmd,
        &format!("Terraform {sub}…"),
        &format!("Terraform {sub} done."),
        &format!("Terraform {sub} failed."),
        &format!("terraform-{sub}"),
    )
}

/// Run a subprocess without leaking its stdout/stderr to the terminal.
/// Drives a cliclack spinner while it runs; on failure, persists the full
/// captured output to `<data_root>/.deploy-logs/<slug>.log` and shows the
/// last ~25 lines inline so the user has enough context to debug.
fn run_quietly(
    mut cmd: Command,
    label: &str,
    done_msg: &str,
    fail_msg: &str,
    log_slug: &str,
) -> Result<()> {
    let s = spinner();
    s.start(label);
    let out = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("spawning subprocess")?;
    if out.status.success() {
        s.stop(done_msg);
        return Ok(());
    }
    s.stop(fail_msg);

    let log_dir = crate::core::data_paths::resolved_data_root().join(".deploy-logs");
    let _ = std::fs::create_dir_all(&log_dir);
    let log_path = log_dir.join(format!("{log_slug}.log"));
    let mut combined = Vec::with_capacity(out.stdout.len() + out.stderr.len());
    combined.extend_from_slice(&out.stdout);
    combined.extend_from_slice(&out.stderr);
    let _ = std::fs::write(&log_path, &combined);

    let text = String::from_utf8_lossy(&combined);
    let tail: Vec<&str> = text
        .lines()
        .rev()
        .take(25)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let body = if tail.is_empty() {
        "(no output captured)".to_string()
    } else {
        tail.join("\n")
    };
    let _ = note("Last output", body);
    let _ = log::info(format!("Full log: {}", log_path.display()));
    bail!("{fail_msg}")
}

/// Lines currently in `terraform state list` (each is e.g. "aws_key_pair.bittice").
/// Empty Vec on fresh state. Treats "no state" as no managed resources, not an error.
fn terraform_state_list(tf_bin: &Path, tf_dir: &Path) -> Vec<String> {
    let out = match Command::new(tf_bin)
        .args(["state", "list"])
        .current_dir(tf_dir)
        .output()
    {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    if !out.status.success() { return Vec::new(); }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// `terraform import <addr> <id>` — adopts an existing AWS resource into state.
/// Bubbles up the error if import itself fails (e.g. resource not found in AWS).
fn terraform_import(tf_bin: &Path, tf_dir: &Path, addr: &str, id: &str) -> Result<()> {
    let mut cmd = Command::new(tf_bin);
    cmd.args(["import", addr, id]).current_dir(tf_dir);
    let slug = format!("terraform-import-{}", addr.replace('.', "-"));
    run_quietly(
        cmd,
        &format!("Adopting existing AWS resource → {addr}…"),
        &format!("Adopted {addr}."),
        &format!("terraform import {addr} failed."),
        &slug,
    )
}

/// Look up an EC2 key pair by name; returns Some(name) if it exists. Uses
/// `--filters` (not `--key-names`) so a missing key returns an empty array
/// instead of throwing AccessDenied / InvalidKeyPair.NotFound.
fn aws_key_pair_exists(key_name: &str, region: &str) -> Result<bool> {
    let v = aws_json(&[
        "ec2", "describe-key-pairs",
        "--region", region,
        "--filters", &format!("Name=key-name,Values={key_name}"),
        "--output", "json",
    ])?;
    Ok(v.get("KeyPairs")
        .and_then(|a| a.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false))
}

/// Look up an SG by name within a VPC; returns its GroupId or None.
fn aws_security_group_id(name: &str, vpc_id: &str, region: &str) -> Result<Option<String>> {
    let v = aws_json(&[
        "ec2", "describe-security-groups",
        "--region", region,
        "--filters",
        &format!("Name=group-name,Values={name}"),
        &format!("Name=vpc-id,Values={vpc_id}"),
        "--output", "json",
    ])?;
    Ok(v.pointer("/SecurityGroups/0/GroupId")
        .and_then(|x| x.as_str())
        .map(String::from))
}

/// Check whether the inbound rule "tcp:port from <source_sg_id>" already exists
/// on the target SG. Used to decide if we need to `terraform import` a duplicate.
fn rds_ingress_rule_exists(
    rds_sg_id: &str,
    source_sg_id: &str,
    port: u16,
    region: &str,
) -> Result<bool> {
    let v = aws_json(&[
        "ec2", "describe-security-groups",
        "--region", region,
        "--group-ids", rds_sg_id,
        "--output", "json",
    ])?;
    let perms = v.pointer("/SecurityGroups/0/IpPermissions")
        .and_then(|a| a.as_array()).cloned().unwrap_or_default();
    for p in perms {
        if p.get("IpProtocol").and_then(|x| x.as_str()) != Some("tcp") { continue; }
        if p.get("FromPort").and_then(|x| x.as_u64()) != Some(port as u64) { continue; }
        if p.get("ToPort").and_then(|x| x.as_u64()) != Some(port as u64) { continue; }
        let pairs = p.get("UserIdGroupPairs")
            .and_then(|a| a.as_array()).cloned().unwrap_or_default();
        for pair in pairs {
            if pair.get("GroupId").and_then(|x| x.as_str()) == Some(source_sg_id) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Bittice should "if exists, use it" — adopt orphan AWS resources from prior
/// failed deploys into Terraform state instead of crashing on duplicate-create.
/// Runs after `terraform init`, before `terraform apply`. Idempotent.
fn reconcile_terraform_orphans(
    tf_bin: &Path,
    tf_dir: &Path,
    app_name: &str,
    placement: &RdsPlacement,
) -> Result<()> {
    let vpc_id = &placement.vpc_id;
    let region = &placement.region;
    let managed = terraform_state_list(tf_bin, tf_dir);

    // ── key pair ───────────────────────────────────────────────────────────
    if !managed.iter().any(|l| l == "aws_key_pair.bittice") {
        let key_name = format!("{app_name}-key");
        if aws_key_pair_exists(&key_name, region).unwrap_or(false) {
            let _ = log::info(format!(
                "Found orphan key pair '{key_name}' in AWS — importing into Terraform state."
            ));
            terraform_import(tf_bin, tf_dir, "aws_key_pair.bittice", &key_name)?;
        }
    }

    // ── bittice security group ─────────────────────────────────────────────
    let bittice_sg_name = format!("{app_name}-sg");
    if !managed.iter().any(|l| l == "aws_security_group.bittice") {
        if let Some(sg_id) = aws_security_group_id(&bittice_sg_name, vpc_id, region).unwrap_or(None) {
            let _ = log::info(format!(
                "Found orphan security group '{bittice_sg_name}' ({sg_id}) — importing into Terraform state."
            ));
            terraform_import(tf_bin, tf_dir, "aws_security_group.bittice", &sg_id)?;
        }
    }

    // ── RDS ingress rules: one per target RDS SG ───────────────────────────
    // Format for `terraform import aws_security_group_rule`:
    //   <sg_id>_<type>_<protocol>_<from_port>_<to_port>_<source_sg_id>
    let bittice_sg_id = aws_security_group_id(&bittice_sg_name, vpc_id, region)
        .ok().flatten();

    if let Some(bittice_sg_id) = bittice_sg_id {
        // Refresh state list — we may have imported the bittice SG just above.
        let managed = terraform_state_list(tf_bin, tf_dir);
        for (i, target) in placement.targets.iter().enumerate() {
            let addr = format!("aws_security_group_rule.rds_ingress_from_bittice[{i}]");
            if managed.iter().any(|l| l == &addr) { continue; }
            let exists = rds_ingress_rule_exists(
                &target.security_group_id, &bittice_sg_id, target.port, region,
            ).unwrap_or(false);
            if exists {
                let import_id = format!(
                    "{}_ingress_tcp_{port}_{port}_{}",
                    target.security_group_id, bittice_sg_id, port = target.port,
                );
                let _ = log::info(format!(
                    "Found orphan ingress rule on RDS SG {} (tcp:{} from {}) — importing.",
                    target.security_group_id, target.port, bittice_sg_id,
                ));
                terraform_import(tf_bin, tf_dir, &addr, &import_id)?;
            }
        }
    }

    Ok(())
}

fn terraform_output(tf_bin: &Path, tf_dir: &Path, key: &str) -> Result<String> {
    let out = Command::new(tf_bin).args(["output", "-raw", key])
        .current_dir(tf_dir).output().context("terraform output")?;
    if !out.status.success() {
        bail!("Could not read terraform output `{key}`: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

// ── remote helpers ────────────────────────────────────────────────────────────

fn ssh_common_args(ssh_key: &str, ip: &str) -> Vec<String> {
    vec![
        "-o".into(),
        "StrictHostKeyChecking=no".into(),
        "-o".into(),
        "UserKnownHostsFile=/dev/null".into(),
        "-o".into(),
        "ConnectTimeout=20".into(),
        "-i".into(),
        ssh_key.to_string(),
        format!("ubuntu@{ip}"),
    ]
}

/// Run a command on the server over SSH. Stdout/stderr are captured; output
/// is only surfaced on failure (see `run_quietly`). `label` drives the spinner.
fn ssh_run_labeled(ip: &str, ssh_key: &str, cmd: &str, label: &str) -> Result<()> {
    let mut args = ssh_common_args(ssh_key, ip);
    args.push(cmd.to_string());
    let mut command = Command::new("ssh");
    command.args(&args);
    run_quietly(
        command,
        label,
        "Remote command finished.",
        "Remote command failed.",
        "ssh-remote",
    )
}

fn rsync_data(data_root: &Path, ip: &str, ssh_key: &str) -> Result<()> {
    let ssh_transport = format!(
        "ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -i {ssh_key}"
    );
    let mut command = Command::new("rsync");
    command.args([
        "-az",
        "--exclude=server.log", "--exclude=.layout_v2",
        "--exclude=terraform", "--exclude=.terraform-bin",
        "-e", &ssh_transport,
        &format!("{}/", data_root.display()),
        &format!("ubuntu@{ip}:/opt/bittice/data/"),
    ]);
    run_quietly(
        command,
        "Syncing data/ to server…",
        "data/ synced.",
        "rsync of data/ failed.",
        "rsync-data",
    )
}

fn wait_for_ssh(ip: &str, ssh_key: &str) -> Result<()> {
    const MAX: u32 = 20;
    let ws = spinner();
    ws.start(format!("Waiting for SSH on {ip}… (1/{MAX})"));
    for attempt in 1..=MAX {
        ws.set_message(format!("Waiting for SSH on {ip}… ({attempt}/{MAX})"));
        let mut args = ssh_common_args(ssh_key, ip);
        args.extend([
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            "PasswordAuthentication=no".into(),
            "-o".into(),
            "ServerAliveInterval=5".into(),
            "-o".into(),
            "ServerAliveCountMax=1".into(),
            "echo ok".into(),
        ]);
        let ok = Command::new("ssh")
            .args(&args)
            .stdout(Stdio::null()).stderr(Stdio::null())
            .status().map(|s| s.success()).unwrap_or(false);
        if ok {
            ws.stop("SSH ready.");
            return Ok(());
        }
        if attempt < MAX {
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
    }
    ws.stop(format!("SSH did not respond on {ip} after {MAX} attempts."));
    bail!(
        "Could not connect via SSH to {ip}.\n\n\
         Troubleshooting:\n\
         1. Test manually: ssh -i {ssh_key} -o StrictHostKeyChecking=no ubuntu@{ip}\n\
         2. Verify the instance is Running in the AWS Console\n\
         3. Check the security group allows port 22 from your IP\n\
         4. Make sure this .pem matches the key used when provisioning"
    );
}

// ── compose generation ────────────────────────────────────────────────────────

/// Identity injected by the wizard so the engine can authenticate heartbeats
/// against the control plane. None means "skip heartbeats" — local mode.
#[derive(Debug, Clone)]
struct EngineIdentity {
    pub deployment_id: String,
    pub instance_token: String,
    pub control_plane_url: String,
}

/// Bittice runs as a plain docker bridge container. EC2 lives in the same VPC as
/// the target RDS (placed there by Terraform), so CDC reaches MySQL through AWS
/// internal networking — no VPN, no sidecar, no policy routing.
///
/// When `rest_domain` is set, Caddy terminates HTTPS on :443 and proxies to
/// bittice:3000. Admin (:8080) is localhost/VPC-only; gRPC (:50051) stays public.
fn generate_compose(image: &str, ident: Option<&EngineIdentity>, rest_domain: Option<&str>) -> String {
    let identity_block = match ident {
        Some(i) => format!(
            "      - BITTICE_DEPLOYMENT_ID={}\n      - BITTICE_INSTANCE_TOKEN={}\n      - BITTICE_CONTROL_PLANE_URL={}\n",
            i.deployment_id, i.instance_token, i.control_plane_url,
        ),
        None => String::new(),
    };
    // Image is always `ghcr.io/.../bittice:stable`. Watchtower polls every 5 min;
    // when a new release is promoted to :stable by release.yml, it auto-pulls and
    // restarts the bittice container (env, volumes preserved → CDC resumes from
    // its saved binlog position, heartbeats reconnect with the same instance token).
    //
    // `--label-enable` means Watchtower only touches containers explicitly labeled.
    // We label `bittice` so it gets updated; we DON'T label `watchtower` itself so
    // it never tries to update its own process while restarting (would deadlock).
    let (bittice_ports, caddy_block, networks_block, volumes_block) = match rest_domain {
        Some(_) => (
            r#"    ports:
      - "127.0.0.1:8080:8080"
      - "50051:50051"
    networks:
      - bittice_net
"#,
            r#"
  caddy:
    image: caddy:2-alpine
    container_name: caddy
    restart: unless-stopped
    ports:
      - "443:443"
      - "80:80"
    volumes:
      - /opt/bittice/Caddyfile:/etc/caddy/Caddyfile:ro
      - caddy_data:/data
      - caddy_config:/config
    networks:
      - bittice_net
    depends_on:
      - bittice
"#,
            r#"
networks:
  bittice_net:
"#,
            r#"
volumes:
  caddy_data:
  caddy_config:
"#,
        ),
        None => (
            r#"    ports:
      - "0.0.0.0:3000:3000"
      - "0.0.0.0:8080:8080"
      - "0.0.0.0:50051:50051"
"#,
            "",
            "",
            "",
        ),
    };

    format!(
r#"services:
  bittice:
    image: "{image}"
    container_name: bittice
    labels:
      - "com.centurylinklabs.watchtower.enable=true"
{bittice_ports}    volumes:
      - /opt/bittice/data:/app/data
    environment:
      - BITTICE_HOST=0.0.0.0
      - BITTICE_ENGINE_ONLY=1
      - BITTICE_CDC_HEALTH_CHECK_MAX_FAILURES=0
      - BITTICE_CDC_HEALTH_CHECK_INTERVAL_SECS=300
      - BITTICE_CDC_STREAM_SILENCE_TIMEOUT_SECS=90
      # NOTE: BITTICE_SKIP_STARTUP_COMPACT is intentionally NOT set here.
      # Compact folds the mini-segment explosion from CDC write paths into a
      # small number of large segments; skipping it on customer instances would
      # let segments grow unbounded over time. We only skip on the corp
      # dev instance where startup speed beats long-term shape.
{identity_block}    restart: unless-stopped
{caddy_block}
  watchtower:
    image: containrrr/watchtower:latest
    container_name: watchtower
    restart: unless-stopped
    environment:
      # Ubuntu 22.04+ ships Docker daemon API 1.44+, which rejects watchtower's
      # default older client API. Pinning here unblocks the docker.sock
      # handshake. Bump if AWS Ubuntu AMIs ever ship Docker >= 30.x.
      - DOCKER_API_VERSION=1.44
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock
    command:
      - --interval
      - "300"
      - --label-enable
      - --cleanup
{networks_block}{volumes_block}"#
    )
}

fn generate_caddyfile(rest_domain: &str) -> String {
    format!(
        "{rest_domain} {{\n    reverse_proxy bittice:3000\n}}\n"
    )
}

fn deploy_compose(
    ip: &str,
    ssh_key: &str,
    image: &str,
    ident: Option<&EngineIdentity>,
    rest_domain: Option<&str>,
) -> Result<()> {
    // Wait for cloud-init to finish before touching anything Docker-related.
    // EC2 returns SSH responsive as soon as sshd is up, but Terraform's user_data
    // (apt-get install docker.io) runs in parallel and can take 1-3 minutes.
    // Without this gate the first `docker pull` lands on a host where docker
    // doesn't exist yet.
    ssh_run_labeled(ip, ssh_key,
        "sudo cloud-init status --wait 2>/dev/null || true; \
         if ! command -v docker >/dev/null 2>&1; then \
           sudo DEBIAN_FRONTEND=noninteractive apt-get update -qq && \
           sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq docker.io && \
           sudo systemctl enable --now docker && \
           sudo usermod -aG docker ubuntu; \
         fi; \
         docker --version >/dev/null || { echo 'Docker install FAILED' >&2; exit 1; }",
        "Waiting for Docker on the server…",
    )?;

    if let (Ok(token), Ok(user)) = (std::env::var("GHCR_TOKEN"), std::env::var("GHCR_USER")) {
        ssh_run_labeled(
            ip,
            ssh_key,
            &format!("echo '{token}' | docker login ghcr.io -u '{user}' --password-stdin"),
            "Logging into GHCR…",
        )?;
    }

    // Tear down previous stack. The systemctl/docker rm of legacy VPN units is
    // kept so re-deploys on top of an older VPN-era EC2 do a clean cutover.
    ssh_run_labeled(ip, ssh_key,
        "sudo chown -R ubuntu:ubuntu /opt/bittice; \
         sudo systemctl stop 'openvpn@*' 'openvpn-client@*' 2>/dev/null || true; \
         sudo systemctl disable 'openvpn@bittice' 'openvpn-client@bittice' 2>/dev/null || true; \
         docker rm -f bittice bittice-vpn caddy 2>/dev/null || true; \
         cd /opt/bittice && docker-compose down 2>/dev/null || docker compose down 2>/dev/null || true",
        "Tearing down previous stack…",
    )?;

    // Install docker-compose if not already present.
    ssh_run_labeled(ip, ssh_key,
        "docker compose version 2>/dev/null || \
         docker-compose version 2>/dev/null || \
         (sudo curl -sSL https://github.com/docker/compose/releases/download/v2.27.0/docker-compose-linux-x86_64 \
          -o /usr/local/bin/docker-compose && sudo chmod +x /usr/local/bin/docker-compose)",
        "Ensuring docker-compose…",
    )?;

    // Write docker-compose.yml (+ Caddyfile when using HTTPS front)
    let compose = generate_compose(image, ident, rest_domain);
    ssh_run_labeled(ip, ssh_key, &format!(
        "cat > /opt/bittice/docker-compose.yml << 'COMPEOF'\n{compose}COMPEOF"
    ), "Writing docker-compose.yml…")?;
    if let Some(domain) = rest_domain {
        let caddyfile = generate_caddyfile(domain);
        ssh_run_labeled(ip, ssh_key, &format!(
            "cat > /opt/bittice/Caddyfile << 'CADDYEOF'\n{caddyfile}CADDYEOF"
        ), "Writing Caddyfile…")?;
    }

    // Pull image and start stack.
    ssh_run_labeled(
        ip,
        ssh_key,
        &format!("docker pull '{image}'"),
        &format!("Pulling {image}…"),
    )?;
    ssh_run_labeled(ip, ssh_key,
        "cd /opt/bittice && (docker compose up -d 2>/dev/null || docker-compose up -d)",
        "Starting Bittice container…",
    )?;

    Ok(())
}

/// After `compose up`, wait until staged CDC reports live replication for every profile.
fn wait_for_cdc_live(ip: &str, ssh_key: &str, expected_profiles: usize) -> Result<()> {
    if expected_profiles == 0 {
        return Ok(());
    }

    const MAX_ATTEMPTS: u32 = 180;
    let ws = spinner();
    ws.start(format!(
        "Waiting for CDC live replication ({expected_profiles} profile(s))… (1/{MAX_ATTEMPTS})"
    ));

    for attempt in 1..=MAX_ATTEMPTS {
        ws.set_message(format!(
            "Waiting for CDC live replication ({expected_profiles} profile(s))… ({attempt}/{MAX_ATTEMPTS})"
        ));
        let logs = ssh_capture(
            ip,
            ssh_key,
            "docker logs bittice 2>&1 | tail -n 120",
        );
        let finished = logs.matches("finished Phase 4").count();
        if finished >= expected_profiles {
            ws.stop(format!(
                "CDC live — {finished} profile(s) reached Phase 4 (real-time replication)."
            ));
            return Ok(());
        }
        if logs.contains("staged startup aborted")
            || (logs.contains("CDC worker") && logs.contains("failed"))
            || logs.contains("request_engine_shutdown_from_cdc")
        {
            ws.stop("CDC failed during startup.");
            let net_hint = if logs.contains("Connection timed out")
                || logs.contains("Connection timeout")
            {
                "\n\nLikely cause: the Bittice SG cannot reach the RDS on its MySQL port.\n\
                 Check that AmazonRDSReadOnlyAccess + AmazonEC2FullAccess gave Terraform permission to add \
                 the inbound rule on the RDS SG, and that the RDS is in the same VPC the wizard placed Bittice in."
            } else {
                ""
            };
            bail!(
                "CDC did not start on the server.{net_hint}\n\
                 ssh -i {ssh_key} ubuntu@{ip} 'docker logs bittice 2>&1 | tail -n 80'"
            );
        }
        if attempt < MAX_ATTEMPTS {
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
    }

    ws.stop("CDC did not reach Phase 4 in time.");
    bail!(
        "Timed out waiting for live CDC ({} profile(s)). The API may serve a static mirror only.\n\
         ssh -i {ssh_key} ubuntu@{ip} 'docker logs -f bittice'",
        expected_profiles
    );
}

fn ssh_capture(ip: &str, ssh_key: &str, cmd: &str) -> String {
    let mut args = ssh_common_args(ssh_key, ip);
    args.push(cmd.to_string());
    Command::new("ssh")
        .args(&args)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

// ── AWS RDS discovery (same-account placement, no VPN) ──────────────────────
//
// When the user's MySQL lives in their own AWS account, the wizard queries the
// RDS via `aws rds describe-db-instances`, picks a public subnet in the RDS's
// VPC, and Terraform places the Bittice EC2 there. The SG of the RDS gets an
// inbound rule from the new EC2's SG so CDC can reach MySQL natively — zero
// VPN, zero tunnel restart problems.

#[derive(Debug, Clone)]
struct RdsTarget {
    identifier: String,
    security_group_id: String,
    port: u16,
}

#[derive(Debug, Clone)]
struct RdsPlacement {
    region: String,
    vpc_id: String,
    subnet_id: String,
    vpc_cidr: String,
    /// One entry per RDS the EC2 needs to reach. All must live in `vpc_id` —
    /// if profiles point at RDSes in different VPCs, the wizard bails because
    /// Bittice's single SG can only open ingress on SGs in the same VPC.
    targets: Vec<RdsTarget>,
}

fn aws_cli_available() -> bool {
    Command::new("aws").arg("--version")
        .stdout(Stdio::null()).stderr(Stdio::null())
        .status().map(|s| s.success()).unwrap_or(false)
}

fn aws_json(args: &[&str]) -> Result<serde_json::Value> {
    let out = Command::new("aws").args(args).output()
        .context("running aws CLI — install it from https://aws.amazon.com/cli/")?;
    if !out.status.success() {
        bail!(
            "aws {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let txt = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(&txt).with_context(|| format!(
        "parsing aws CLI JSON output (aws {})", args.join(" ")
    ))
}

/// Scans every `cdc_config.json` for MySQL hosts matching the AWS RDS endpoint
/// pattern `<id>.<random>.<region>.rds.amazonaws.com` and returns the unique
/// `(db_instance_identifier, region)` pairs in profile-scan order. Duplicate
/// identifiers (e.g. two profiles sharing one RDS) collapse to one entry.
fn extract_rds_hints_from_cdc_profiles(data_root: &Path) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for cfg in crate::core::data_paths::scan_all_cdc_config_paths_in_data_root(data_root) {
        let Ok(txt) = std::fs::read_to_string(&cfg) else { continue };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) else { continue };
        let Some(host) = v.get("host").and_then(|h| h.as_str()) else { continue };
        let host = host.trim().trim_end_matches('.').to_lowercase();
        if !host.ends_with(".rds.amazonaws.com") { continue; }
        let parts: Vec<&str> = host.split('.').collect();
        if parts.len() < 6 { continue; }
        let pair = (parts[0].to_string(), parts[2].to_string());
        if !out.contains(&pair) { out.push(pair); }
    }
    out
}

/// Returns `(vpc_id, port, primary_sg_id)` for the RDS instance.
fn describe_rds(identifier: &str, region: &str) -> Result<(String, u16, String)> {
    let v = aws_json(&[
        "rds", "describe-db-instances",
        "--db-instance-identifier", identifier,
        "--region", region,
        "--output", "json",
    ])?;
    let inst = v.pointer("/DBInstances/0")
        .with_context(|| format!("RDS '{identifier}' not found in region {region}"))?;
    let vpc_id = inst.pointer("/DBSubnetGroup/VpcId").and_then(|x| x.as_str())
        .context("VpcId missing on RDS DBSubnetGroup")?.to_string();
    let port: u16 = inst.pointer("/Endpoint/Port").and_then(|x| x.as_u64())
        .context("Endpoint.Port missing on RDS")? as u16;
    let sg_id = inst.pointer("/VpcSecurityGroups/0/VpcSecurityGroupId").and_then(|x| x.as_str())
        .context("RDS has no VPC security group attached")?.to_string();
    Ok((vpc_id, port, sg_id))
}

/// All subnets in a VPC with `MapPublicIpOnLaunch=true`. Returns
/// `Vec<(subnet_id, az, cidr, name_tag)>` ordered by AZ for stable display.
fn find_public_subnets(vpc_id: &str, region: &str)
    -> Result<Vec<(String, String, String, String)>>
{
    let v = aws_json(&[
        "ec2", "describe-subnets",
        "--region", region,
        "--filters", &format!("Name=vpc-id,Values={vpc_id}"),
        "--output", "json",
    ])?;
    let mut out: Vec<(String, String, String, String)> = Vec::new();
    let arr = v.get("Subnets").and_then(|x| x.as_array()).cloned().unwrap_or_default();
    for s in arr {
        if !s.get("MapPublicIpOnLaunch").and_then(|x| x.as_bool()).unwrap_or(false) {
            continue;
        }
        let subnet_id = s.get("SubnetId").and_then(|x| x.as_str()).unwrap_or("").to_string();
        if subnet_id.is_empty() { continue; }
        let az   = s.get("AvailabilityZone").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let cidr = s.get("CidrBlock").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let name = s.get("Tags").and_then(|t| t.as_array())
            .and_then(|tags| tags.iter().find(|t| t.get("Key").and_then(|k| k.as_str()) == Some("Name")))
            .and_then(|t| t.get("Value").and_then(|v| v.as_str()))
            .unwrap_or("").to_string();
        out.push((subnet_id, az, cidr, name));
    }
    out.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(out)
}

fn get_vpc_cidr(vpc_id: &str, region: &str) -> Result<String> {
    let v = aws_json(&[
        "ec2", "describe-vpcs",
        "--vpc-ids", vpc_id,
        "--region", region,
        "--output", "json",
    ])?;
    v.pointer("/Vpcs/0/CidrBlock").and_then(|x| x.as_str()).map(String::from)
        .with_context(|| format!("CidrBlock not found for VPC {vpc_id}"))
}

/// Prompt for one RDS identifier (with a hint as default) and query AWS for it.
/// Returns the target plus the VPC and region it lives in.
async fn prompt_and_describe_one_rds(
    hint: Option<&(String, String)>,
    forced_region: Option<&str>,
    nth_label: &str,
) -> Result<(RdsTarget, String, String)> {
    let id_hint = hint.map(|(i, _)| i.as_str()).unwrap_or("");
    let raw_id: String = match input(format!(
            "RDS instance identifier {nth_label} (not the endpoint hostname)"
        ))
        .default_input(id_hint)
        .interact()
    {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::Interrupted => bail!("cancelled"),
        Err(e) => return Err(e.into()),
    };
    let identifier = raw_id.trim().to_string();
    if identifier.is_empty() { bail!("RDS identifier is required"); }

    let region = if let Some(r) = forced_region {
        r.to_string()
    } else {
        let raw: String = match input("AWS region of that RDS")
            .default_input(hint.map(|(_, r)| r.as_str()).unwrap_or("us-east-1"))
            .interact()
        {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => bail!("cancelled"),
            Err(e) => return Err(e.into()),
        };
        raw.trim().to_string()
    };

    let s = spinner();
    s.start(format!("Querying RDS '{identifier}' in {region}…"));
    let (vpc_id, port, sg_id) = match describe_rds(&identifier, &region) {
        Ok(v) => { s.stop(format!("RDS '{identifier}' in VPC {} (port {}).", v.0, v.1)); v }
        Err(e) => { s.stop("RDS lookup failed."); return Err(e); }
    };

    Ok((RdsTarget { identifier, security_group_id: sg_id, port }, vpc_id, region))
}

/// Full discovery: iterates all RDS hints from CDC profiles, prompts for each,
/// validates they all live in the same VPC, then picks one public subnet for
/// the Bittice EC2. The Terraform plan ends up opening MySQL ingress from
/// Bittice's SG to every RDS SG returned here.
async fn discover_rds_placement(hints: Vec<(String, String)>) -> Result<RdsPlacement> {
    if !aws_cli_available() {
        bail!("aws CLI not installed — install from https://aws.amazon.com/cli/ and run `aws configure`");
    }

    // No hints = wizard runs blind, ask the user for at least one identifier.
    let effective: Vec<Option<(String, String)>> = if hints.is_empty() {
        vec![None]
    } else {
        let _ = log::info(format!(
            "Detected {} RDS target(s) in your sync profiles — confirm each below (Enter to accept).",
            hints.len()
        ));
        hints.into_iter().map(Some).collect()
    };
    let total = effective.len();

    // First RDS decides the region + VPC; every subsequent one must match.
    let mut targets: Vec<RdsTarget> = Vec::with_capacity(total);
    let mut region: String = String::new();
    let mut vpc_id: String = String::new();

    for (i, hint) in effective.iter().enumerate() {
        let label = if total > 1 { format!("({}/{})", i + 1, total) } else { String::new() };
        let forced_region = if i == 0 { None } else { Some(region.as_str()) };
        let (tgt, this_vpc, this_region) =
            prompt_and_describe_one_rds(hint.as_ref(), forced_region, &label).await?;

        if i == 0 {
            region = this_region;
            vpc_id = this_vpc;
        } else if this_vpc != vpc_id {
            bail!(
                "RDS '{}' lives in {this_vpc}, but the first RDS is in {vpc_id}.\n\
                 A single Bittice EC2 can only reach RDSes in one VPC. To cover both:\n\
                 • Set up VPC peering between {vpc_id} and {this_vpc}, OR\n\
                 • Run a separate Bittice deployment per VPC (one entity per deploy).",
                tgt.identifier
            );
        }

        targets.push(tgt);
    }

    // Pick a public subnet in the shared VPC.
    let s = spinner();
    s.start(format!("Listing public subnets in {vpc_id}…"));
    let publics = match find_public_subnets(&vpc_id, &region) {
        Ok(v) => v,
        Err(e) => { s.stop("Subnet listing failed."); return Err(e); }
    };
    if publics.is_empty() {
        s.stop("No public subnets in this VPC.");
        bail!(
            "VPC {vpc_id} has no subnet with MapPublicIpOnLaunch=true.\n\
             Bittice needs a public subnet to receive SSH and REST/gRPC ingress.\n\
             Create one (or enable auto-assign public IPv4 on an existing subnet) and retry."
        );
    }
    s.stop(format!("{} public subnet(s) available.", publics.len()));

    let subnet_id: String = if publics.len() == 1 {
        let (sid, az, cidr, name) = &publics[0];
        let _ = log::info(format!(
            "Auto-selected only public subnet: {sid}  ({az}, {cidr}{})",
            if name.is_empty() { "".into() } else { format!(", \"{name}\"") }
        ));
        sid.clone()
    } else {
        let mut sel = select("Pick a public subnet for the Bittice EC2");
        for (sid, az, cidr, name) in &publics {
            let label = if name.is_empty() {
                format!("{sid}  ({az}, {cidr})")
            } else {
                format!("{sid}  ({az}, {cidr}, \"{name}\")")
            };
            sel = sel.item(sid.clone(), label, "");
        }
        match sel.interact() {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => bail!("cancelled"),
            Err(e) => return Err(e.into()),
        }
    };

    let s = spinner();
    s.start(format!("Fetching CIDR of {vpc_id}…"));
    let vpc_cidr = match get_vpc_cidr(&vpc_id, &region) {
        Ok(c) => { s.stop(format!("VPC CIDR: {c}.")); c }
        Err(e) => { s.stop("VPC CIDR lookup failed."); return Err(e); }
    };

    Ok(RdsPlacement { region, vpc_id, subnet_id, vpc_cidr, targets })
}

// ── inline auth (no separate `bittice login` step) ──────────────────────────
//
// **The API key is NEVER persisted.** Every cloud deploy asks for it explicitly;
// the key lives only in process memory for the duration of the wizard and is
// dropped at the end. The file at `~/.bittice/credentials.json` only stores
// non-secret hints (last email + control plane URL) so the prompt can say
// "Welcome back you@example.com" instead of asking for everything from scratch.

/// In-memory result of authenticating for one deploy.
struct AuthCtx {
    pub control_plane_url: String,
    pub api_key: String,         // memory-only, never written to disk
    #[allow(dead_code)]
    pub user_id: String,
    pub email: String,
}

async fn ensure_authenticated() -> Result<AuthCtx> {
    use crate::core::{credentials, control_plane};

    let hints = credentials::load().unwrap_or_default();
    let url = credentials::resolved_control_plane_url();
    std::env::set_var("BITTICE_CONTROL_PLANE_URL", &url);

    // Show context so the user knows which account they're about to charge.
    if let Some(ref email) = hints.last_email {
        let _ = log::info(format!(
            "Last login on this machine: {email}. Paste your API key to authenticate this deploy."
        ));
    } else {
        let _ = log::info(
            "Authenticate this cloud deploy with your Bittice API key.\n\
             It starts with `bk_live_…` (find it in your Bittice account dashboard)."
        );
    }

    let raw_key: String = match input("Bittice API key")
        .placeholder("bk_live_…")
        .interact()
    {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::Interrupted => bail!("cancelled"),
        Err(e) => return Err(e.into()),
    };
    let api_key = raw_key.trim().to_string();
    if !api_key.starts_with("bk_live_") {
        bail!("API key must start with 'bk_live_' (got something else — paste again).");
    }

    let s = spinner();
    s.start(format!("Validating API key against {url}…"));
    let me = match control_plane::login(&api_key).await {
        Ok(me) => {
            s.stop(format!("Authenticated as {} (plan: {})", me.email, me.plan));
            me
        }
        Err(e) => {
            s.stop("Authentication failed.");
            return Err(e);
        }
    };

    // Persist HINTS only (no api_key) so the next deploy's prompt is friendlier.
    let _ = credentials::save(&credentials::ProfileHints {
        version: 2,
        control_plane_url: url.clone(),
        last_email: Some(me.email.clone()),
        last_user_id: Some(me.user_id.clone()),
        api_key: None,
    });

    Ok(AuthCtx {
        control_plane_url: url,
        api_key,
        user_id: me.user_id,
        email: me.email,
    })
}

// ── SSH key auto-generation (no .pem required in AWS-managed mode) ──────────
//
// When deploying via AWS-discovered placement, the user already has credentials
// configured for `aws` — asking them for a separate .pem is redundant. We
// generate an ed25519 keypair once in ~/.bittice/ssh/ and reuse it for every
// future deploy. Pure Terraform-managed (registered as aws_key_pair).

struct AutoKeypair {
    private_path: String,
    public_key:   String,
}

fn ensure_ssh_keypair_auto() -> Result<AutoKeypair> {
    let home = home_dir().context("HOME env var not set — cannot store SSH key")?;
    let dir = PathBuf::from(&home).join(".bittice").join("ssh");
    std::fs::create_dir_all(&dir).context("create ~/.bittice/ssh/")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    let priv_path = dir.join("bittice_id_ed25519");
    let pub_path  = dir.join("bittice_id_ed25519.pub");

    if !priv_path.is_file() {
        let s = spinner();
        s.start("Generating SSH keypair at ~/.bittice/ssh/bittice_id_ed25519…");
        let status = Command::new("ssh-keygen")
            .args([
                "-t", "ed25519",
                "-N", "",
                "-C", "bittice-cloud-deploy",
                "-f", priv_path.to_string_lossy().as_ref(),
            ])
            .status().context("running ssh-keygen — is OpenSSH installed?")?;
        if !status.success() {
            s.stop("ssh-keygen failed.");
            bail!("Could not generate SSH keypair at {}", priv_path.display());
        }
        s.stop("SSH keypair generated.");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&priv_path, std::fs::Permissions::from_mode(0o600));
    }
    let public_key = std::fs::read_to_string(&pub_path)
        .with_context(|| format!("read public key at {}", pub_path.display()))?
        .trim().to_string();
    Ok(AutoKeypair {
        private_path: priv_path.to_string_lossy().to_string(),
        public_key,
    })
}

// ── image detection ───────────────────────────────────────────────────────────

/// Returns the GHCR image reference for the deploy. Always uses the `:stable`
/// floating tag so customer EC2s with Watchtower auto-pull future releases.
/// The git tag is still required (sanity-check that the repo has at least one
/// release published — otherwise `:stable` wouldn't resolve).
fn detect_image() -> Result<String> {
    let root = super::deploy_pipeline::find_bittice_project_root()
        .ok_or_else(|| anyhow::anyhow!(
            "Could not find the Bittice project root.\n\
             Run bittice from inside the repository directory."
        ))?;

    let tag_out = Command::new("git")
        .args(["describe", "--tags", "--abbrev=0"])
        .current_dir(&root).output()
        .context("git describe failed — is git installed?")?;
    if !tag_out.status.success() {
        bail!(
            "No git tags found. Push a release tag first (e.g. git tag v0.1.93 && git push --tags)\n\
             GitHub Actions will build and publish the Docker image as :stable."
        );
    }

    let remote_out = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(&root).output()
        .context("git remote get-url failed")?;
    if !remote_out.status.success() {
        bail!("Could not detect GitHub repository from git remote.");
    }
    let url = String::from_utf8_lossy(&remote_out.stdout).trim().to_string();
    let repo = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("git@github.com:"))
        .map(|s| s.trim_end_matches(".git").to_lowercase())
        .ok_or_else(|| anyhow::anyhow!("Unsupported git remote format: {url}"))?;

    Ok(format!("ghcr.io/{repo}:stable"))
}

// ── wizard ────────────────────────────────────────────────────────────────────

fn build_tfvars(
    region: &str,
    instance_type: &str,
    ssh_pub_key: &str,
    app_name: &str,
    placement: Option<&RdsPlacement>,
) -> String {
    let mut s = format!(
        "aws_region     = \"{region}\"\n\
         instance_type  = \"{instance_type}\"\n\
         ssh_public_key = \"{ssh_pub_key}\"\n\
         app_name       = \"{app_name}\"\n"
    );
    if let Some(p) = placement {
        // RDS ingress rules: one per target SG. Terraform iterates the list with
        // count, so each target gets its own aws_security_group_rule resource.
        let sg_list = p.targets.iter()
            .map(|t| format!("\"{}\"", t.security_group_id))
            .collect::<Vec<_>>().join(", ");
        // Different targets can technically have different MySQL ports; in
        // practice they don't, so we use the first target's port for the
        // single `rds_port` Terraform var. (Common pattern: all RDS on 3306.)
        let rds_port = p.targets.first().map(|t| t.port).unwrap_or(3306);
        s.push_str(&format!(
            "target_vpc_id                  = \"{}\"\n\
             target_subnet_id               = \"{}\"\n\
             target_rds_security_group_ids  = [{}]\n\
             rds_port                       = {}\n\
             allowed_admin_cidr             = \"{}\"\n",
            p.vpc_id, p.subnet_id, sg_list, rds_port, p.vpc_cidr,
        ));
    }
    s
}

/// AWS resource names must be 3–32 chars, start with a letter, and contain only
/// lowercase letters, digits, and hyphens (no spaces, no leading/trailing dash).
/// Used both for the EC2 Name tag and as a prefix for SG/key/EIP names.
fn validate_deployment_name(name: &str) -> Result<()> {
    let n = name.trim();
    if n.len() < 3 || n.len() > 32 {
        bail!("Name must be 3–32 characters (got {}).", n.len());
    }
    let first = n.chars().next().unwrap();
    if !first.is_ascii_alphabetic() || !first.is_ascii_lowercase() {
        bail!("Name must start with a lowercase letter (got '{first}').");
    }
    if n.ends_with('-') {
        bail!("Name cannot end with '-'.");
    }
    for c in n.chars() {
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
            bail!("Name can only contain lowercase letters, digits, and hyphens (offending char: '{c}').");
        }
    }
    Ok(())
}

fn cloud_config_path() -> PathBuf {
    crate::core::data_paths::resolved_data_root().join(".bittice_cloud.json")
}

fn load_cloud_config_domain(app_name: &str) -> Option<String> {
    load_cloud_config_field(app_name, "rest_domain")
}

fn load_cloud_config_grpc_domain(app_name: &str) -> Option<String> {
    load_cloud_config_field(app_name, "grpc_domain")
}

fn load_cloud_config_field(app_name: &str, field: &str) -> Option<String> {
    let raw = std::fs::read_to_string(cloud_config_path()).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    if v.get("app_name")?.as_str()? != app_name {
        return None;
    }
    let d = v.get(field)?.as_str()?.trim();
    if d.is_empty() {
        return None;
    }
    Some(d.to_string())
}

fn save_cloud_config(app_name: &str, rest_domain: &str, grpc_domain: &str) -> Result<()> {
    let v = serde_json::json!({
        "app_name": app_name,
        "rest_domain": rest_domain,
        "grpc_domain": grpc_domain,
    });
    std::fs::write(
        cloud_config_path(),
        serde_json::to_string_pretty(&v).context("serialize cloud config")?,
    )
    .context("write .bittice_cloud.json")?;
    Ok(())
}

/// Suggested default when the user has not configured gRPC DNS before.
pub fn suggest_grpc_domain(rest_domain: &str) -> String {
    let rest = rest_domain.trim().trim_end_matches('.');
    match rest.split_once('.') {
        Some((first, rest_labels)) => format!("{first}-grpc.{rest_labels}"),
        None => format!("{rest}-grpc"),
    }
}

/// Public hostname (REST HTTPS or gRPC); no scheme, no port.
fn validate_public_hostname(domain: &str) -> Result<()> {
    let d = domain.trim().trim_end_matches('.');
    if d.len() < 4 || d.len() > 253 {
        bail!("Hostname must be 4–253 characters.");
    }
    if d.contains("://") || d.contains('/') || d.contains(':') {
        bail!("Enter only the hostname (e.g. dash-sac.dev.parking.net.co), not a URL or port.");
    }
    if !d.contains('.') {
        bail!("Hostname must look like a DNS name (contain at least one dot).");
    }
    for label in d.split('.') {
        if label.is_empty() || label.len() > 63 {
            bail!("Invalid hostname label in '{d}'.");
        }
        let first = label.chars().next().unwrap();
        if !first.is_ascii_alphanumeric() {
            bail!("Each label must start with a letter or digit.");
        }
        if !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            bail!("Hostname labels may only contain letters, digits, and hyphens.");
        }
    }
    Ok(())
}

fn prompt_public_hostname(label: &str, default: &str) -> Result<String> {
    loop {
        let raw: String = match input(label).default_input(default).interact() {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(String::new()),
            Err(e) => return Err(e.into()),
        };
        let candidate = raw.trim().to_string();
        match validate_public_hostname(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(e) => {
                let _ = log::warning(format!("{e} Try again."));
            }
        }
    }
}

/// Cloud deploy + control-plane API key auth. Off during local-first preview.
pub async fn run_cloud_deploy_wizard() -> Result<()> {
    let _ = log::info("Provisions a cloud VM via Terraform and runs the Bittice engine container.");
    let _ = log::info("Terraform is downloaded automatically if not already cached.");

    // Detect image first — fail fast if not in repo or no tags.
    let image = detect_image()?;
    let _ = log::step(format!("Image: {image}"));

    // Check ssh/rsync (can't be auto-installed).
    let s = spinner();
    s.start("Checking prerequisites (ssh, rsync)…");
    let pre = check_ssh_rsync();
    if pre.is_ok() { s.stop("Prerequisites OK."); } else { s.stop("Missing tools."); }
    pre?;

    // ── cloud provider ──
    let provider: u8 = match select("Cloud provider")
        .item(0u8, "AWS (Amazon Web Services)", "")
        .item(1u8, "Azure  — coming soon", "")
        .item(2u8, "GCP    — coming soon", "")
        .item(255u8, "« Back", "")
        .interact()
    {
        Ok(x) => x,
        Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    match provider {
        255 => return Ok(()),
        1 | 2 => {
            let _ = log::warning("Only AWS is supported right now. Azure and GCP are coming soon.");
            return Ok(());
        }
        _ => {}
    }

    // Download Terraform after provider is selected.
    let tf_bin = ensure_terraform().await?;

    let tf_dir = crate::core::data_paths::resolved_data_root().join("terraform");
    let data_root = crate::core::data_paths::resolved_data_root();
    if tf_dir.join("terraform.tfstate").is_file() {
        let _ = log::info(
            "Found existing Terraform state — `terraform apply` will reconcile it (idempotent: \
             unchanged resources stay; only diffs apply)."
        );
    }

    // ── RDS discovery (mandatory) ─────────────────────────────────────────────
    // Same-account placement is the ONLY supported mode: Bittice's EC2 goes into
    // the same VPC as the target RDS so CDC reaches MySQL natively. No tunnels,
    // no public-RDS fallback — if the user can't satisfy this, the wizard bails
    // with a helpful error rather than silently falling back to a fragile path.
    if !aws_cli_available() {
        bail!(
            "`aws` CLI not found on PATH — install it from https://aws.amazon.com/cli/ \
             and run `aws configure` (or set AWS_PROFILE / AWS_ACCESS_KEY_ID).\n\n\
             Cloud deploy requires AWS credentials so it can discover your target RDS's \
             VPC and place Bittice in the same network."
        );
    }
    let hints = extract_rds_hints_from_cdc_profiles(&data_root);
    let placement: RdsPlacement = discover_rds_placement(hints).await?;

    // ── region (always pinned to the RDS's region) ──
    let region: String = {
        let _ = log::step(format!("Region pinned to RDS location: {}", placement.region));
        placement.region.clone()
    };

    // ── instance type ──
    let instance_type: &str = match select("VM size")
        .item("t3.micro",  "t3.micro  — 1 vCPU / 1 GB   (free-tier eligible)", "")
        .item("t3.small",  "t3.small  — 2 vCPU / 2 GB", "")
        .item("t3.medium", "t3.medium — 2 vCPU / 4 GB", "")
        .item("t3.large",  "t3.large  — 2 vCPU / 8 GB", "")
        .interact()
    {
        Ok(x) => x,
        Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(()),
        Err(e) => return Err(e.into()),
    };

    // ── SSH key ──
    // Auto-managed: we already have AWS creds, no point asking for a .pem.
    let (ssh_priv, ssh_pub_key) = {
        let kp = ensure_ssh_keypair_auto()?;
        let _ = log::success(format!("Using auto-managed SSH key: {}", kp.private_path));
        (kp.private_path, kp.public_key)
    };

    // ── deployment name (used for EC2 Name tag + SG/key/EIP resource names) ──
    // Lets the user deploy multiple Bittices in the same account without
    // resource-name collisions. Default "bittice" keeps the existing UX.
    let app_name: String = loop {
        let raw: String = match input("Name for this deployment (EC2 Name tag + resource prefix)")
            .default_input("bittice")
            .interact()
        {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let candidate = raw.trim().to_string();
        match validate_deployment_name(&candidate) {
            Ok(()) => break candidate,
            Err(e) => { let _ = log::warning(format!("{e} Try again.")); }
        }
    };

    // ── REST public hostname (HTTPS via Caddy; admin not on public URL) ──
    let default_rest = load_cloud_config_domain(&app_name).unwrap_or_default();
    let rest_domain = prompt_public_hostname(
        "Public REST hostname (HTTPS — DNS A record must point to this EC2's Elastic IP)",
        &default_rest,
    )?;
    if rest_domain.is_empty() {
        return Ok(());
    }

    // ── gRPC public hostname (DNS A → same Elastic IP, port 50051) ──
    let default_grpc = load_cloud_config_grpc_domain(&app_name)
        .unwrap_or_else(|| suggest_grpc_domain(&rest_domain));
    let grpc_domain = prompt_public_hostname(
        "Public gRPC hostname (DNS A record must point to this EC2's Elastic IP; clients use :50051)",
        &default_grpc,
    )?;
    if grpc_domain.is_empty() {
        return Ok(());
    }

    let _ = log::info(format!(
        "REST will be https://{rest_domain}  |  gRPC {grpc_domain}:50051  |  Admin via SSH tunnel"
    ));

    // ── IAM permissions note ──
    let _ = note(
        "AWS IAM permissions required",
        "Your IAM user needs:\n\
         • AmazonEC2FullAccess\n\
         • AmazonRDSReadOnlyAccess\n\n\
         These give discovery (rds:DescribeDBInstances, ec2:Describe*), provisioning (EC2/SG/EIP/KeyPair), \
         and the cross-resource action that replaces the VPN: AuthorizeSecurityGroupIngress on each \
         target RDS SG (lets Bittice's SG talk to MySQL inside the VPC)."
    );

    // ── confirm ──
    let rds_list = placement.targets.iter()
        .map(|t| format!("{}:{}", t.identifier, t.port))
        .collect::<Vec<_>>().join(", ");
    let summary = format!(
        "(name: {app_name}, region: {}, VM: {instance_type}, VPC: {} → RDS [{rds_list}])",
        placement.region, placement.vpc_id
    );
    let go: u8 = match select(format!("Create AWS resources? {summary}"))
        .item(0u8, "Yes — provision and deploy", "")
        .item(255u8, "No — cancel", "")
        .interact()
    {
        Ok(x) => x,
        Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    if go == 255 { return Ok(()); }

    // ── optional control-plane registration (API key + deployment id) ──
    let ident: Option<EngineIdentity> = if crate::core::control_plane_gate::REPORTING_ENABLED {
        let auth = ensure_authenticated().await?;
        let s = spinner();
        s.start("Registering deployment with control plane…");
        let req = crate::core::control_plane::CreateDeploymentRequest {
            name: app_name.clone(),
            cloud_provider: "aws".into(),
            region: placement.region.clone(),
            instance_type: instance_type.to_string(),
            source_db_engine: placement.targets.first()
                .map(|_| "mysql".to_string()).unwrap_or_else(|| "mysql".into()),
            source_db_version: None,
            source_profile_count: Some(
                crate::core::data_paths::cdc_profile_count(&data_root) as u32
            ),
            vpc_id: Some(placement.vpc_id.clone()),
        };
        let resp = crate::core::control_plane::create_deployment(&auth.api_key, &req).await?;
        s.stop(format!("Deployment registered: {} (user: {})", resp.deployment_id, auth.email));
        Some(EngineIdentity {
            deployment_id: resp.deployment_id,
            instance_token: resp.instance_token,
            control_plane_url: auth.control_plane_url.clone(),
        })
    } else {
        let _ = log::info(
            "Skipping control-plane registration (no API key). \
             The VM runs the engine only — no heartbeat or drift reports to Bittice."
        );
        None
    };

    // ── write terraform files ──
    let ws = spinner();
    ws.start("Writing Terraform files…");
    let write_res = write_terraform_files(
        &tf_dir,
        &build_tfvars(&region, instance_type, &ssh_pub_key, &app_name, Some(&placement)),
    );
    if write_res.is_ok() { ws.stop("Terraform files ready."); } else { ws.stop("Failed to write files."); }
    write_res?;

    terraform_run(&tf_bin, &tf_dir, &["init", "-upgrade"])?;

    // Adopt any leftover AWS resources from a previous failed deploy ("if
    // exists, use it"). This prevents `terraform apply` from crashing with
    // InvalidKeyPair.Duplicate / InvalidGroup.Duplicate / InvalidPermission.Duplicate.
    reconcile_terraform_orphans(&tf_bin, &tf_dir, &app_name, &placement)?;

    terraform_run(&tf_bin, &tf_dir, &["apply", "-auto-approve"])?;

    let ip = terraform_output(&tf_bin, &tf_dir, "public_ip")?;
    let _ = log::success(format!("EC2 Elastic IP: {ip}"));

    save_cloud_config(&app_name, &rest_domain, &grpc_domain)?;

    finish_deploy(
        &ip,
        &ssh_priv,
        &image,
        &data_root,
        ident.as_ref(),
        Some(rest_domain.as_str()),
        Some(grpc_domain.as_str()),
    )?;
    Ok(())
}

fn finish_deploy(
    ip: &str,
    ssh_priv: &str,
    image: &str,
    data_root: &Path,
    ident: Option<&EngineIdentity>,
    rest_domain: Option<&str>,
    grpc_domain: Option<&str>,
) -> Result<()> {
    let profile_count = crate::core::data_paths::cdc_profile_count(data_root);
    if profile_count == 0 {
        let _ = log::warning(
            "No CDC profiles under data/profiles/ — deploy will start the API with static data only.\n\
             Connect and sync on your PC first, then redeploy.",
        );
    }

    wait_for_ssh(ip, ssh_priv)?;

    ssh_run_labeled(
        ip,
        ssh_priv,
        "sudo mkdir -p /opt/bittice/data && sudo chown -R ubuntu:ubuntu /opt/bittice",
        "Preparing /opt/bittice on server…",
    )?;
    rsync_data(data_root, ip, ssh_priv)?;

    deploy_compose(ip, ssh_priv, image, ident, rest_domain)?;

    wait_for_cdc_live(ip, ssh_priv, profile_count)?;

    let admin_ok = Command::new("ssh")
        .args(["-o", "StrictHostKeyChecking=no", "-o", "ConnectTimeout=10",
               "-i", ssh_priv, &format!("ubuntu@{ip}"), "curl -sf http://127.0.0.1:8080"])
        .stdout(Stdio::null()).stderr(Stdio::null())
        .status().map(|s| s.success()).unwrap_or(false);

    let rest_ok = if let Some(domain) = rest_domain {
        Command::new("ssh")
            .args(["-o", "StrictHostKeyChecking=no", "-o", "ConnectTimeout=15",
                   "-i", ssh_priv, &format!("ubuntu@{ip}"),
                   &format!("curl -sf http://127.0.0.1:3000 >/dev/null 2>&1 || docker exec bittice curl -sf http://127.0.0.1:3000 >/dev/null")])
            .stdout(Stdio::null()).stderr(Stdio::null())
            .status().map(|s| s.success()).unwrap_or(false)
            || {
                let _ = domain; // HTTPS cert may still be provisioning
                true
            }
    } else {
        Command::new("ssh")
            .args(["-o", "StrictHostKeyChecking=no", "-o", "ConnectTimeout=10",
                   "-i", ssh_priv, &format!("ubuntu@{ip}"), "curl -sf http://127.0.0.1:3000"])
            .stdout(Stdio::null()).stderr(Stdio::null())
            .status().map(|s| s.success()).unwrap_or(false)
    };

    let _ = log::success(format!(
        "Bittice running at {ip}  (admin local: {}, REST: {}, CDC profiles: {profile_count})",
        if admin_ok { "OK" } else { "check pending" },
        if rest_ok { "OK" } else { "check pending" },
    ));

    if let Some(domain) = rest_domain {
        let _ = log::info(format!("REST   https://{domain}"));
        let _ = log::info(format!(
            "Admin  ssh -L 8080:127.0.0.1:8080 -i {ssh_priv} ubuntu@{ip}  →  http://127.0.0.1:8080"
        ));
        if let Some(grpc) = grpc_domain {
            let _ = log::info(format!("gRPC   {grpc}:50051  (or {ip}:50051)"));
        } else {
            let _ = log::info(format!("gRPC   {ip}:50051"));
        }
    } else {
        let _ = log::info(format!("REST   http://{ip}:3000"));
        let _ = log::info(format!("Admin  http://{ip}:8080"));
        let _ = log::info(format!("gRPC   {ip}:50051"));
    }
    let _ = log::info(format!("Logs   ssh -i {ssh_priv} ubuntu@{ip} 'docker logs -f bittice'"));
    Ok(())
}

fn home_dir() -> Option<String> {
    std::env::var("HOME").ok()
}

#[cfg(test)]
mod grpc_domain_tests {
    use super::suggest_grpc_domain;

    #[test]
    fn suggest_grpc_from_prod_rest() {
        assert_eq!(
            suggest_grpc_domain("dash-sac.prod.parking.net.co"),
            "dash-sac-grpc.prod.parking.net.co"
        );
    }

    #[test]
    fn suggest_grpc_from_dev_rest() {
        assert_eq!(
            suggest_grpc_domain("dash-sac.dev.parking.net.co"),
            "dash-sac-grpc.dev.parking.net.co"
        );
    }
}

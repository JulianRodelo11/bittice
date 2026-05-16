//! Interactive cloud-VM deploy via Terraform.
//! Provisions an EC2 instance, syncs data/, and starts the Bittice engine container.
//!
//! Terraform is downloaded automatically on first use (~70 MB, cached in ~/.bittice/bin/).
//! When VPN is needed, OpenVPN runs as a Docker sidecar container that shares its network
//! namespace with the Bittice container. This mirrors the local experience exactly:
//! Bittice sees tun0 directly, TCP keepalive works, and VPN reconnects transparently.

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
const EC2_OVPN_NAME: &str = "bittice-ec2.ovpn";

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
    let status = Command::new(tf_bin).args(args).current_dir(tf_dir)
        .status().context("running terraform")?;
    if !status.success() {
        bail!("terraform {} failed", args.first().unwrap_or(&""));
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

fn ssh_run(ip: &str, ssh_key: &str, cmd: &str) -> Result<()> {
    let status = Command::new("ssh")
        .args(["-o", "StrictHostKeyChecking=no", "-o", "ConnectTimeout=20",
               "-i", ssh_key, &format!("ubuntu@{ip}"), cmd])
        .status().context("ssh")?;
    if !status.success() { bail!("Remote command failed: {cmd}"); }
    Ok(())
}

fn rsync_data(data_root: &Path, ip: &str, ssh_key: &str) -> Result<()> {
    let status = Command::new("rsync")
        .args([
            "-avz", "--progress",
            "--exclude=server.log", "--exclude=.layout_v2",
            "--exclude=terraform", "--exclude=.terraform-bin",
            "-e", &format!("ssh -o StrictHostKeyChecking=no -i {ssh_key}"),
            &format!("{}/", data_root.display()),
            &format!("ubuntu@{ip}:/opt/bittice/data/"),
        ])
        .status().context("rsync")?;
    if !status.success() { bail!("rsync of data/ failed"); }
    Ok(())
}

fn wait_for_ssh(ip: &str, ssh_key: &str) -> Result<()> {
    const MAX: u32 = 20;
    let ws = spinner();
    ws.start(format!("Waiting for SSH on {ip}… (1/{MAX})"));
    for attempt in 1..=MAX {
        ws.set_message(format!("Waiting for SSH on {ip}… ({attempt}/{MAX})"));
        let ok = Command::new("ssh")
            .args([
                "-o", "StrictHostKeyChecking=no",
                "-o", "ConnectTimeout=8",
                "-o", "BatchMode=yes",
                "-o", "PasswordAuthentication=no",
                "-o", "ServerAliveInterval=5",
                "-o", "ServerAliveCountMax=1",
                "-i", ssh_key,
                &format!("ubuntu@{ip}"),
                "echo ok",
            ])
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

// ── VPN sidecar via docker-compose ────────────────────────────────────────────

/// up.sh injected into the VPN container at /vpn/up.sh.
/// Runs after every OpenVPN connect/reconnect and sets up conntrack-based
/// policy routing so incoming gRPC/REST connections reply via eth0, not tun0.
fn vpn_up_sh() -> &'static str {
    r#"#!/bin/sh
IFACE="eth0"
REAL_GW=$(ip route | awk '/^default/ && /eth0/{print $3; exit}')
[ -z "$REAL_GW" ] && REAL_GW=$(ip route | awk '/^default/{print $3; exit}')

ip rule add fwmark 1 table 200 pref 100 2>/dev/null || true
ip route replace default via "$REAL_GW" dev "$IFACE" table 200 2>/dev/null || true

iptables -t mangle -F PREROUTING
iptables -t mangle -A PREROUTING -i "$IFACE" -m conntrack --ctstate NEW -j CONNMARK --set-mark 1
iptables -t mangle -A PREROUTING -i "$IFACE" -j CONNMARK --restore-mark
iptables -t mangle -A OUTPUT -m connmark --mark 1 -j MARK --set-mark 1 2>/dev/null || true

ip route add 169.254.169.254/32 dev "$IFACE" 2>/dev/null || true
"#
}

fn generate_compose(image: &str, vpn: bool) -> String {
    if vpn {
        format!(
r#"services:
  vpn:
    image: dperson/openvpn-client:latest
    container_name: bittice-vpn
    cap_add: [NET_ADMIN, NET_RAW]
    devices: ["/dev/net/tun"]
    volumes: ["/opt/bittice/vpn:/vpn"]
    ports:
      - "0.0.0.0:3000:3000"
      - "0.0.0.0:8080:8080"
      - "0.0.0.0:50051:50051"
    restart: unless-stopped
    healthcheck:
      test: ["CMD", "ip", "link", "show", "tun0"]
      interval: 5s
      timeout: 3s
      retries: 20
      start_period: 5s

  bittice:
    image: "{image}"
    container_name: bittice
    network_mode: "service:vpn"
    depends_on:
      vpn:
        condition: service_healthy
    volumes: ["/opt/bittice/data:/app/data"]
    environment:
      - BITTICE_HOST=0.0.0.0
      - BITTICE_ENGINE_ONLY=1
      - BITTICE_CDC_HEALTH_CHECK_MAX_FAILURES=0
      - BITTICE_CDC_HEALTH_CHECK_INTERVAL_SECS=120
      - BITTICE_CDC_VPN_RESTART_COOLDOWN_SECS=20
      - BITTICE_CDC_STREAM_SILENCE_TIMEOUT_SECS=90
      - BITTICE_SKIP_STARTUP_COMPACT=1
    restart: unless-stopped
"#
        )
    } else {
        format!(
r#"services:
  bittice:
    image: "{image}"
    container_name: bittice
    ports:
      - "0.0.0.0:3000:3000"
      - "0.0.0.0:8080:8080"
      - "0.0.0.0:50051:50051"
    volumes: ["/opt/bittice/data:/app/data"]
    environment:
      - BITTICE_HOST=0.0.0.0
      - BITTICE_ENGINE_ONLY=1
      - BITTICE_CDC_HEALTH_CHECK_MAX_FAILURES=0
      - BITTICE_CDC_HEALTH_CHECK_INTERVAL_SECS=120
    restart: unless-stopped
"#
        )
    }
}

fn deploy_compose(ip: &str, ssh_key: &str, image: &str, vpn_configured: bool) -> Result<()> {
    if let (Ok(token), Ok(user)) = (std::env::var("GHCR_TOKEN"), std::env::var("GHCR_USER")) {
        ssh_run(ip, ssh_key, &format!("echo '{token}' | docker login ghcr.io -u '{user}' --password-stdin"))?;
    }

    // Ensure /opt/bittice is writable by ubuntu and clean up old deployment.
    ssh_run(ip, ssh_key,
        "sudo chown -R ubuntu:ubuntu /opt/bittice; \
         sudo systemctl stop 'openvpn@*' 2>/dev/null || true; \
         sudo systemctl disable 'openvpn@bittice' 2>/dev/null || true; \
         docker rm -f bittice bittice-vpn 2>/dev/null || true; \
         cd /opt/bittice && docker-compose down 2>/dev/null || docker compose down 2>/dev/null || true"
    )?;

    if vpn_configured {
        ssh_run(ip, ssh_key, "mkdir -p /opt/bittice/vpn")?;
        // Place .ovpn at the path dperson expects: /vpn/vpn.conf
        ssh_run(ip, ssh_key, &format!(
            "cp /opt/bittice/data/vpn/{EC2_OVPN_NAME} /opt/bittice/vpn/vpn.conf"
        ))?;
        // Write the up.sh with conntrack rules and make it executable.
        let up_sh = vpn_up_sh();
        ssh_run(ip, ssh_key, &format!(
            "cat > /opt/bittice/vpn/up.sh << 'UPEOF'\n{up_sh}UPEOF\nchmod +x /opt/bittice/vpn/up.sh"
        ))?;
    }

    // Install docker-compose if not already present.
    ssh_run(ip, ssh_key,
        "docker compose version 2>/dev/null || \
         (curl -sSL https://github.com/docker/compose/releases/download/v2.27.0/docker-compose-linux-x86_64 \
          -o /usr/local/bin/docker-compose && chmod +x /usr/local/bin/docker-compose)"
    )?;

    // Write docker-compose.yml
    let compose = generate_compose(image, vpn_configured);
    ssh_run(ip, ssh_key, &format!(
        "cat > /opt/bittice/docker-compose.yml << 'COMPEOF'\n{compose}COMPEOF"
    ))?;

    // Pull image and start stack.
    ssh_run(ip, ssh_key, &format!("docker pull '{image}'"))?;
    ssh_run(ip, ssh_key,
        "cd /opt/bittice && (docker compose up -d 2>/dev/null || docker-compose up -d)"
    )?;

    if vpn_configured {
        // Wait for VPN tunnel inside the sidecar container.
        const MAX_VPN: u32 = 24;
        let ws = spinner();
        ws.start(format!("Waiting for VPN tunnel (tun0 in sidecar)… (1/{MAX_VPN})"));
        let mut vpn_up = false;
        for attempt in 1..=MAX_VPN {
            ws.set_message(format!("Waiting for VPN tunnel (tun0 in sidecar)… ({attempt}/{MAX_VPN})"));
            let ok = Command::new("ssh")
                .args(["-o", "StrictHostKeyChecking=no", "-o", "ConnectTimeout=10",
                       "-i", ssh_key, &format!("ubuntu@{ip}"),
                       "docker exec bittice-vpn ip link show tun0 2>/dev/null"])
                .stdout(Stdio::null()).stderr(Stdio::null())
                .status().map(|s| s.success()).unwrap_or(false);
            if ok { vpn_up = true; break; }
            if attempt < MAX_VPN {
                std::thread::sleep(std::time::Duration::from_secs(5));
            }
        }
        if vpn_up {
            ws.stop("VPN sidecar tunnel established.");
        } else {
            ws.stop("VPN tunnel not detected after 2 minutes.");
            let logs = ssh_capture(ip, ssh_key, "docker logs bittice-vpn --tail 20 2>&1");
            let _ = log::warning(format!(
                "VPN sidecar logs:\n{logs}\n\n\
                 To debug: ssh -i {ssh_key} ubuntu@{ip} 'docker logs bittice-vpn'"
            ));
        }
    }
    Ok(())
}

// ── SSH key ───────────────────────────────────────────────────────────────────

/// Derives the SSH public key from a private key (.pem or id_rsa).
fn derive_public_key(private_key_path: &str) -> Result<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(private_key_path, std::fs::Permissions::from_mode(0o600));
    }
    let out = Command::new("ssh-keygen").args(["-y", "-f", private_key_path])
        .output().context("Failed to run ssh-keygen — is OpenSSH installed?")?;
    if !out.status.success() {
        bail!(
            "Could not derive public key from '{private_key_path}'.\n\
             Make sure it is a valid PEM or OpenSSH private key.\n\
             Error: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

// ── VPN setup on EC2 ──────────────────────────────────────────────────────────

fn prepare_ovpn_content(raw: &str) -> String {
    let mut content = raw.to_string();

    // Baseline compatibility for cloud instances.
    // Note: data-ciphers is OpenVPN 2.5+ only; dperson/openvpn-client uses 2.4.x.
    // Strip it so the sidecar image doesn't reject the config.
    let lines: Vec<&str> = content.lines()
        .filter(|l| !l.starts_with("data-ciphers "))
        .collect();
    content = lines.join("\n");
    if !content.ends_with('\n') { content.push('\n'); }

    for opt in ["client", "dev tun", "mssfix 1400"] {
        if !content.contains(opt) {
            if !content.ends_with('\n') { content.push('\n'); }
            content.push_str(opt);
            content.push('\n');
        }
    }

    // keepalive: ping every 10s, reconnect after 60s without response.
    if !content.contains("keepalive") {
        if !content.ends_with('\n') { content.push('\n'); }
        content.push_str("keepalive 10 60\n");
    }

    // Run the conntrack routing script inside the VPN sidecar container.
    if !content.contains("up /vpn/up.sh") {
        if !content.ends_with('\n') { content.push('\n'); }
        content.push_str("up /vpn/up.sh\n");
    }
    // Re-run the up script on every VPN reconnect (required for conntrack rules).
    if !content.contains("up-restart") {
        if !content.ends_with('\n') { content.push('\n'); }
        content.push_str("up-restart\n");
    }

    content
}

/// Installs OpenVPN as a systemd service with Restart=always.
/// The service is independent of the Bittice container: VPN drop → systemd restarts it,
/// the EC2 instance stays up, the container keeps running.
fn ssh_capture(ip: &str, ssh_key: &str, cmd: &str) -> String {
    Command::new("ssh")
        .args(["-o", "StrictHostKeyChecking=no", "-o", "ConnectTimeout=10",
               "-i", ssh_key, &format!("ubuntu@{ip}"), cmd])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

// ── .ovpn collection ──────────────────────────────────────────────────────────

async fn collect_and_store_ovpn(data_root: &Path) -> Result<bool> {
    let _ = log::info("The .ovpn profile runs in a Docker sidecar container that shares its network with Bittice.");
    let _ = log::info("Bittice sees tun0 directly — identical to local mode. VPN reconnects transparently.");

    let source: u8 = match select("How do you want to provide the .ovpn file?")
        .item(0u8, "File path on this machine", "")
        .item(1u8, "HTTP(S) URL", "")
        .item(2u8, "Paste the .ovpn content", "")
        .item(255u8, "Skip — database is reachable from EC2 directly", "")
        .interact()
    {
        Ok(x) => x,
        Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    if source == 255 { return Ok(false); }

    let raw: String = match source {
        0 => {
            let path: String = match input("Path to .ovpn file").placeholder("/path/to/file.ovpn").interact() {
                Ok(s) => s,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(false),
                Err(e) => return Err(e.into()),
            };
            std::fs::read_to_string(path.trim()).context("Could not read .ovpn file")?
        }
        1 => {
            let url: String = match input("URL of the .ovpn file").placeholder("https://...").interact() {
                Ok(s) => s,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(false),
                Err(e) => return Err(e.into()),
            };
            let s = spinner();
            s.start("Downloading .ovpn…");
            let text = reqwest::get(url.trim()).await?.text().await?;
            s.stop("Downloaded.");
            text
        }
        _ => {
            let _ = log::info("Paste the full .ovpn text (must contain 'client' and 'dev'):");
            match input("Paste .ovpn content").interact() {
                Ok(s) => s,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(false),
                Err(e) => return Err(e.into()),
            }
        }
    };

    if !raw.contains("client") || !raw.contains("dev") {
        bail!("The content does not look like a valid .ovpn file (missing 'client' or 'dev').");
    }
    let vpn_dir = data_root.join("vpn");
    std::fs::create_dir_all(&vpn_dir).context("create data/vpn")?;
    let dest = vpn_dir.join(EC2_OVPN_NAME);
    std::fs::write(&dest, prepare_ovpn_content(&raw)).context("write .ovpn")?;
    let _ = log::success(format!("Saved {} (will be rsynced to EC2)", dest.display()));
    Ok(true)
}

// ── image detection ───────────────────────────────────────────────────────────

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
             GitHub Actions will build and publish the Docker image."
        );
    }
    let tag = String::from_utf8_lossy(&tag_out.stdout).trim().to_string();

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

    Ok(format!("ghcr.io/{repo}:{tag}"))
}

// ── wizard ────────────────────────────────────────────────────────────────────

fn build_tfvars(region: &str, instance_type: &str, ssh_pub_key: &str) -> String {
    format!(
        "aws_region     = \"{region}\"\n\
         instance_type  = \"{instance_type}\"\n\
         ssh_public_key = \"{ssh_pub_key}\"\n"
    )
}

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
    let has_state = tf_dir.join("terraform.tfstate").is_file();

    if has_state {
        let _ = log::info("Found existing Terraform state.");
        let action: u8 = match select("What do you want to do?")
            .item(0u8, "Deploy latest image to existing VM", "skip terraform apply")
            .item(1u8, "Re-provision infrastructure + deploy", "runs terraform apply again")
            .item(255u8, "« Back", "")
            .interact()
        {
            Ok(x) => x,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        if action == 255 { return Ok(()); }
        if action == 0 { return deploy_to_existing(&tf_bin, &tf_dir, &data_root, &image).await; }
    }

    // ── region ──
    let region: &str = match select("AWS region")
        .item("us-east-1",      "us-east-1      — N. Virginia", "")
        .item("us-west-2",      "us-west-2      — Oregon", "")
        .item("eu-west-1",      "eu-west-1      — Ireland", "")
        .item("eu-central-1",   "eu-central-1   — Frankfurt", "")
        .item("ap-southeast-1", "ap-southeast-1 — Singapore", "")
        .item("ap-northeast-1", "ap-northeast-1 — Tokyo", "")
        .item("sa-east-1",      "sa-east-1      — São Paulo", "")
        .interact()
    {
        Ok(x) => x,
        Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(()),
        Err(e) => return Err(e.into()),
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

    // ── SSH key (.pem or id_rsa — public key is derived automatically) ──
    let default_priv = format!("{}/.ssh/id_rsa", home_dir().unwrap_or_else(|| "~".into()));
    let _ = log::info("Provide the private key AWS gave you (.pem) or your local id_rsa.");
    let ssh_priv_path: String = match input("SSH private key path (.pem or id_rsa)")
        .default_input(&default_priv).interact()
    {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let ssh_priv = ssh_priv_path.trim().to_string();

    let ks = spinner();
    ks.start("Deriving public key…");
    let pub_key_res = derive_public_key(&ssh_priv);
    if pub_key_res.is_ok() { ks.stop("Public key derived."); } else { ks.stop("Failed to derive public key."); }
    let ssh_pub_key = pub_key_res?;

    // ── VPN ──
    let needs_vpn: bool = match select("Does the database require VPN to be reached from EC2?")
        .item(0u8, "No  — database is directly reachable", "")
        .item(1u8, "Yes — configure OpenVPN on the EC2 instance", "")
        .interact()
    {
        Ok(x) => x == 1,
        Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let vpn_configured = if needs_vpn { collect_and_store_ovpn(&data_root).await? } else { false };

    // ── IAM permissions note ──
    let _ = note(
        "AWS IAM permissions required",
        "Your IAM user needs EC2 access. Easiest fix:\n\
         Go to IAM → Users → your user → Add permissions\n\
         → Attach policy → AmazonEC2FullAccess\n\n\
         Or attach a custom policy allowing: ec2:* on resource *"
    );

    // ── confirm ──
    let go: u8 = match select(format!(
        "Create AWS resources? (region: {region}, VM: {instance_type}, VPN: {})",
        if vpn_configured { "OpenVPN" } else { "none" }
    ))
        .item(0u8, "Yes — provision and deploy", "")
        .item(255u8, "No — cancel", "")
        .interact()
    {
        Ok(x) => x,
        Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    if go == 255 { return Ok(()); }

    // ── write terraform files ──
    let ws = spinner();
    ws.start("Writing Terraform files…");
    let write_res = write_terraform_files(&tf_dir, &build_tfvars(region, instance_type, &ssh_pub_key));
    if write_res.is_ok() { ws.stop("Terraform files ready."); } else { ws.stop("Failed to write files."); }
    write_res?;

    let _ = log::step("Running terraform init…");
    terraform_run(&tf_bin, &tf_dir, &["init", "-upgrade"])?;

    let _ = log::step("Running terraform apply…");
    terraform_run(&tf_bin, &tf_dir, &["apply", "-auto-approve"])?;

    let ip = terraform_output(&tf_bin, &tf_dir, "public_ip")?;
    let _ = log::success(format!("EC2 Elastic IP: {ip}"));

    finish_deploy(&ip, &ssh_priv, &image, &data_root, vpn_configured)
}

async fn deploy_to_existing(
    tf_bin: &Path, tf_dir: &PathBuf, data_root: &PathBuf, image: &str,
) -> Result<()> {
    let ip = terraform_output(tf_bin, tf_dir, "public_ip")?;
    let _ = log::success(format!("EC2 IP: {ip}"));

    let default_priv = format!("{}/.ssh/id_rsa", home_dir().unwrap_or_else(|| "~".into()));
    let _ = log::info("Provide the same .pem or id_rsa used when provisioning this instance.");
    let ssh_priv: String = match input("SSH private key path (.pem or id_rsa)").default_input(&default_priv).interact() {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(()),
        Err(e) => return Err(e.into()),
    };

    let existing_ovpn = data_root.join("vpn").join(EC2_OVPN_NAME);
    let vpn_configured: bool = if existing_ovpn.is_file() {
        match select("VPN profile")
            .item(0u8, "Use existing profile (already on EC2)", "")
            .item(1u8, "Update with a new .ovpn file", "")
            .item(2u8, "Disable VPN for this deploy", "")
            .interact()
        {
            Ok(0) => true,
            Ok(1) => collect_and_store_ovpn(data_root).await?,
            _ => false,
        }
    } else {
        match select("VPN")
            .item(0u8, "No VPN needed", "")
            .item(1u8, "Configure OpenVPN now", "")
            .interact()
        {
            Ok(1) => collect_and_store_ovpn(data_root).await?,
            _ => false,
        }
    };

    finish_deploy(&ip, ssh_priv.trim(), image, data_root, vpn_configured)
}

fn finish_deploy(ip: &str, ssh_priv: &str, image: &str, data_root: &Path, vpn_configured: bool) -> Result<()> {
    wait_for_ssh(ip, ssh_priv)?;

    let _ = log::step("Syncing data/…");
    ssh_run(ip, ssh_priv,
        "sudo mkdir -p /opt/bittice/data && sudo chown -R ubuntu:ubuntu /opt/bittice"
    )?;
    rsync_data(data_root, ip, ssh_priv)?;

    let _ = log::step(format!("Deploying {image}…"));
    deploy_compose(ip, ssh_priv, image, vpn_configured)?;

    std::thread::sleep(std::time::Duration::from_secs(5));
    let ok = Command::new("ssh")
        .args(["-o", "StrictHostKeyChecking=no", "-o", "ConnectTimeout=10",
               "-i", ssh_priv, &format!("ubuntu@{ip}"), "curl -sf http://localhost:8080"])
        .stdout(Stdio::null()).stderr(Stdio::null())
        .status().map(|s| s.success()).unwrap_or(false);

    let _ = log::success(format!(
        "Bittice engine running at {ip}  (health: {})",
        if ok { "OK" } else { "check pending" }
    ));
    let _ = log::info(format!("REST   http://{ip}:3000"));
    let _ = log::info(format!("Admin  http://{ip}:8080"));
    let _ = log::info(format!("gRPC   {ip}:50051"));
    if vpn_configured {
        let _ = log::info(format!("VPN    ssh ubuntu@{ip} 'docker logs -f bittice-vpn'"));
    }
    let _ = log::info(format!("Logs   ssh -i {ssh_priv} ubuntu@{ip} 'docker logs -f bittice'"));
    Ok(())
}

fn home_dir() -> Option<String> {
    std::env::var("HOME").ok()
}

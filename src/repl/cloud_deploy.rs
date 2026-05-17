//! Interactive cloud-VM deploy via Terraform.
//! Provisions an EC2 instance, syncs data/, and starts the Bittice engine container.
//!
//! Terraform is downloaded automatically on first use (~70 MB, cached in ~/.bittice/bin/).
//! When sync profiles reference `vpn_file`, deploy auto-starts an OpenVPN Docker sidecar
//! (same stack as `deploy/docker-compose.local.yaml`) and waits until CDC reaches Phase 4.

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
const EC2_OVPN_NAME: &str = crate::core::data_paths::DEPLOY_OVPN_NAME;

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

fn ssh_run(ip: &str, ssh_key: &str, cmd: &str) -> Result<()> {
    let mut args = ssh_common_args(ssh_key, ip);
    args.push(cmd.to_string());
    let status = Command::new("ssh").args(&args).status().context("ssh")?;
    if !status.success() { bail!("Remote command failed: {cmd}"); }
    Ok(())
}

fn rsync_data(data_root: &Path, ip: &str, ssh_key: &str) -> Result<()> {
    let ssh_transport = format!(
        "ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -i {ssh_key}"
    );
    let status = Command::new("rsync")
        .args([
            "-avz", "--progress",
            "--exclude=server.log", "--exclude=.layout_v2",
            "--exclude=terraform", "--exclude=.terraform-bin",
            "-e", &ssh_transport,
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

const DEFAULT_VPN_CIDRS: &str = "10.0.0.0/8,172.31.0.0/16";

/// When VPN is required, OpenVPN runs in a sidecar (`network_mode: service:vpn`) — same
/// layout as `deploy/docker-compose.local.yaml`, fully automated on `compose up`.
fn generate_compose(image: &str, with_vpn: bool) -> String {
    let bittice_env = r#"      - BITTICE_HOST=0.0.0.0
      - BITTICE_ENGINE_ONLY=1
      - BITTICE_CDC_HEALTH_CHECK_MAX_FAILURES=0
      - BITTICE_CDC_HEALTH_CHECK_INTERVAL_SECS=300
      - BITTICE_CDC_STREAM_SILENCE_TIMEOUT_SECS=90
      - BITTICE_SKIP_STARTUP_COMPACT=1"#;

    if with_vpn {
        return format!(
r#"services:
  vpn:
    image: dperson/openvpn-client:latest
    container_name: bittice-vpn
    cap_add: [NET_ADMIN, NET_RAW]
    devices: ["/dev/net/tun"]
    volumes:
      - /opt/bittice/data/vpn:/vpn
    ports:
      - "0.0.0.0:3000:3000"
      - "0.0.0.0:8080:8080"
      - "0.0.0.0:50051:50051"
    restart: unless-stopped
    healthcheck:
      test: ["CMD", "ip", "link", "show", "tun0"]
      interval: 5s
      timeout: 3s
      retries: 24
      start_period: 15s

  bittice:
    image: "{image}"
    container_name: bittice
    network_mode: "service:vpn"
    depends_on:
      vpn:
        condition: service_healthy
    volumes:
      - /opt/bittice/data:/app/data
    environment:
{bittice_env}
    restart: unless-stopped
"#
        );
    }

    format!(
r#"services:
  bittice:
    image: "{image}"
    container_name: bittice
    ports:
      - "0.0.0.0:3000:3000"
      - "0.0.0.0:8080:8080"
      - "0.0.0.0:50051:50051"
    volumes:
      - /opt/bittice/data:/app/data
    environment:
{bittice_env}
    restart: unless-stopped
"#
    )
}

fn deploy_compose(ip: &str, ssh_key: &str, image: &str, vpn_configured: bool) -> Result<()> {
    if let (Ok(token), Ok(user)) = (std::env::var("GHCR_TOKEN"), std::env::var("GHCR_USER")) {
        ssh_run(ip, ssh_key, &format!("echo '{token}' | docker login ghcr.io -u '{user}' --password-stdin"))?;
    }

    // Tear down previous stack (host systemd VPN from older deploys included).
    ssh_run(ip, ssh_key,
        "sudo chown -R ubuntu:ubuntu /opt/bittice; \
         sudo systemctl stop 'openvpn@*' 'openvpn-client@*' 2>/dev/null || true; \
         sudo systemctl disable 'openvpn@bittice' 'openvpn-client@bittice' 2>/dev/null || true; \
         docker rm -f bittice bittice-vpn 2>/dev/null || true; \
         cd /opt/bittice && docker-compose down 2>/dev/null || docker compose down 2>/dev/null || true"
    )?;

    if vpn_configured {
        ssh_run(ip, ssh_key, &format!(
            "test -f /opt/bittice/data/vpn/{EC2_OVPN_NAME} || (echo 'missing {EC2_OVPN_NAME} under data/vpn' >&2; exit 1); \
             cp /opt/bittice/data/vpn/{EC2_OVPN_NAME} /opt/bittice/data/vpn/vpn.conf"
        ))?;
    }

    // Install docker-compose if not already present.
    ssh_run(ip, ssh_key,
        "docker compose version 2>/dev/null || \
         docker-compose version 2>/dev/null || \
         (sudo curl -sSL https://github.com/docker/compose/releases/download/v2.27.0/docker-compose-linux-x86_64 \
          -o /usr/local/bin/docker-compose && sudo chmod +x /usr/local/bin/docker-compose)"
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
            let vpn_hint = if logs.contains("Connection timed out")
                || logs.contains("Connection timeout")
            {
                "\n\nLikely cause: MySQL is only reachable over VPN but the deploy stack has no VPN sidecar.\n\
                 Ensure data/vpn/bittice-ec2.ovpn exists locally, then redeploy (the CLI will start bittice-vpn + bittice)."
            } else {
                ""
            };
            bail!(
                "CDC did not start on the server.{vpn_hint}\n\
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

/// Converts "10.0.0.0/8" to ("10.0.0.0", "255.0.0.0") for OpenVPN's classic
/// `route <network> <netmask>` directive.
fn cidr_to_route(cidr: &str) -> Result<(String, String)> {
    let (ip, prefix) = cidr.split_once('/')
        .with_context(|| format!("CIDR must be in form ip/prefix (got '{cidr}')"))?;
    let prefix: u32 = prefix.trim().parse()
        .with_context(|| format!("Invalid CIDR prefix in '{cidr}'"))?;
    if prefix > 32 { bail!("CIDR prefix must be 0-32 (got /{prefix})"); }
    let mask: u32 = if prefix == 0 { 0 } else { (!0u32) << (32 - prefix) };
    let netmask = format!(
        "{}.{}.{}.{}",
        (mask >> 24) & 0xff, (mask >> 16) & 0xff, (mask >> 8) & 0xff, mask & 0xff,
    );
    Ok((ip.trim().to_string(), netmask))
}

/// Rewrites a raw .ovpn for host-OpenVPN with split-tunnel routing:
/// only the listed CIDRs flow through tun0 — default route stays on eth0, so
/// Bittice's inbound REST/gRPC and outbound to the public internet are
/// unaffected. Accepts a comma- or whitespace-separated list of CIDRs because
/// the database VPC's range is rarely the only network you need to reach
/// (e.g. `10.0.0.0/8,172.31.0.0/16`).
fn prepare_ovpn_content(raw: &str, cidrs_csv: &str) -> Result<String> {
    let cidrs: Vec<&str> = cidrs_csv
        .split(|c: char| c == ',' || c.is_whitespace())
        .map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
    if cidrs.is_empty() {
        bail!("No CIDR provided — pass at least one (e.g. 10.0.0.0/8).");
    }
    let mut route_lines = Vec::with_capacity(cidrs.len());
    for cidr in &cidrs {
        let (net, mask) = cidr_to_route(cidr)?;
        route_lines.push(format!("route {net} {mask}"));
    }

    // Drop any prior route/up/keepalive directives baked in from a previous prepare
    // or the raw upstream profile — we'll rewrite them with the right values.
    let mut content: String = raw
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            !(t.starts_with("route ")
                || t.starts_with("route-nopull")
                || t == "redirect-gateway def1"
                || t.starts_with("redirect-gateway ")
                || t.starts_with("up ")
                || t.starts_with("up-restart")
                || t.starts_with("keepalive ")
                || t.starts_with("ping ")
                || t.starts_with("ping-restart "))
        })
        .collect::<Vec<_>>()
        .join("\n");
    if !content.ends_with('\n') { content.push('\n'); }

    // Baseline compatibility.
    for opt in ["client", "dev tun", "mssfix 1400"] {
        if !content.contains(opt) {
            content.push_str(opt);
            content.push('\n');
        }
    }

    // Stability layer.
    // Use `ping` + `ping-restart` *separately* (NOT `keepalive 10 60`, which is
    // shorthand for `ping 10 ; ping-restart 60` — a 60s restart window is far too
    // aggressive: every transient UDP blip on a long-haul tunnel triggers a soft
    // restart that kills all in-flight TCP connections (incl. the MySQL binlog
    // stream). 300s lets the tunnel ride out short network hiccups; if MySQL
    // really stopped responding, Bittice's BITTICE_CDC_STREAM_SILENCE_TIMEOUT_SECS
    // (90s) fires first and force-reconnects the stream on the app layer.
    if !content.contains("\nping ") && !content.starts_with("ping ") {
        content.push_str("ping 10\n");
    }
    if !content.contains("ping-restart ") {
        content.push_str("ping-restart 300\n");
    }
    if !content.contains("pull-filter ignore \"ping-restart\"") {
        content.push_str("pull-filter ignore \"ping-restart\"\n");
    }
    if !content.contains("reneg-sec ") {
        content.push_str("reneg-sec 0\n");
    }

    // Split-tunnel: ignore the server's pushed default route; install only the
    // explicit routes to the database CIDRs.
    content.push_str("route-nopull\n");
    for line in &route_lines {
        content.push_str(line);
        content.push('\n');
    }

    Ok(content)
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

// ── .ovpn collection ──────────────────────────────────────────────────────────

/// Build `data/vpn/bittice-ec2.ovpn` from an existing profile `vpn_file` when the deploy bundle omits it.
fn bootstrap_ec2_ovpn_from_profiles(data_root: &Path) -> Result<bool> {
    let dest = data_root.join("vpn").join(EC2_OVPN_NAME);
    if dest.is_file() {
        return Ok(true);
    }

    for config_path in crate::core::data_paths::scan_all_cdc_config_paths_in_data_root(data_root) {
        let Ok(content) = std::fs::read_to_string(&config_path) else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue;
        };
        let Some(vpn_ref) = json
            .get("vpn_file")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };

        let file_name = std::path::Path::new(vpn_ref)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| vpn_ref.to_string());
        let candidates = [
            data_root.join("vpn").join(&file_name),
            crate::core::data_paths::vpn_storage_dir().join(&file_name),
            std::path::PathBuf::from(vpn_ref),
        ];
        let Some(src) = candidates.iter().find(|p| p.is_file()) else {
            continue;
        };

        let raw = std::fs::read_to_string(src)
            .with_context(|| format!("read VPN profile {}", src.display()))?;
        let prepared =
            prepare_ovpn_content(&raw, DEFAULT_VPN_CIDRS).context("prepare deploy .ovpn")?;
        std::fs::create_dir_all(data_root.join("vpn")).context("create data/vpn")?;
        std::fs::write(&dest, prepared).context("write deploy .ovpn")?;
        let _ = log::success(format!(
            "Prepared {} from sync profile (split-tunnel [{DEFAULT_VPN_CIDRS}])",
            dest.display()
        ));
        return Ok(true);
    }

    Ok(false)
}

/// Copy `bittice-ec2.ovpn` → `vpn.conf` for the dperson/openvpn-client sidecar.
fn ensure_vpn_conf_in_bundle(data_root: &Path) -> Result<()> {
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
        "VPN sidecar needs data/vpn/{} or data/vpn/vpn.conf — add it via Deploy → OpenVPN profile.",
        EC2_OVPN_NAME
    );
}

/// Ensures VPN material exists when profiles, hostnames, or data/vpn/ indicate a tunnel.
async fn ensure_vpn_ready_for_deploy(data_root: &Path) -> Result<bool> {
    if !crate::core::data_paths::deploy_requires_vpn_sidecar(data_root) {
        return Ok(false);
    }

    if crate::core::data_paths::deploy_vpn_material_present(data_root) {
        ensure_vpn_conf_in_bundle(data_root)?;
        let _ = log::info(
            "Found OpenVPN under data/vpn/ — deploy will start VPN + Bittice sidecars.",
        );
        return Ok(true);
    }

    if crate::core::data_paths::any_cdc_host_suggests_vpn(data_root) {
        let _ = log::info(
            "MySQL host looks VPC/VPN-only — OpenVPN is required on the server.",
        );
    } else {
        let _ = log::info(
            "Detected OpenVPN in your sync profile(s) — deploy will start a VPN sidecar automatically.",
        );
    }

    if bootstrap_ec2_ovpn_from_profiles(data_root)? {
        ensure_vpn_conf_in_bundle(data_root)?;
        return Ok(true);
    }

    let _ = log::info("Add the .ovpn file once; it is stored under data/vpn/ and reused on every deploy.");
    let ok = collect_and_store_ovpn(data_root).await?;
    if ok {
        ensure_vpn_conf_in_bundle(data_root)?;
    }
    Ok(ok)
}

async fn collect_and_store_ovpn(data_root: &Path) -> Result<bool> {
    let _ = log::info("OpenVPN runs in a Docker sidecar (same as local docker-compose.local.yaml).");
    let _ = log::info(format!(
        "Only these CIDRs use the tunnel by default: [{DEFAULT_VPN_CIDRS}]."
    ));

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

    // CIDR(s) to route through the tunnel (everything else stays on eth0).
    // Default covers both 10.x (typical private VPCs) and 172.31.x (AWS default-VPC
    // range used by peered accounts) — the user's own subnet's more-specific route
    // (usually /20 on ens5) wins, so this is safe.
    let _ = log::info(
        "Comma-separated list of CIDRs to route through VPN. Common targets:\n\
         • 10.0.0.0/8        — most private VPCs\n\
         • 172.31.0.0/16     — AWS default-VPC range (covers RDS in peered default VPCs)\n\
         • 192.168.0.0/16    — on-prem / SOHO networks"
    );
    let cidrs: String = loop {
        let entered: String = match input("CIDRs to route through VPN (comma-separated)")
            .default_input("10.0.0.0/8,172.31.0.0/16")
            .interact()
        {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        let mut bad: Option<String> = None;
        for c in entered.split(|c: char| c == ',' || c.is_whitespace())
            .map(|s| s.trim()).filter(|s| !s.is_empty())
        {
            if let Err(e) = cidr_to_route(c) {
                bad = Some(format!("{c}: {e}"));
                break;
            }
        }
        match bad {
            None => break entered.trim().to_string(),
            Some(msg) => {
                let _ = log::warning(format!(
                    "Invalid CIDR ({msg}). Try again (e.g. 10.0.0.0/8,172.31.0.0/16)."
                ));
            }
        }
    };

    let vpn_dir = data_root.join("vpn");
    std::fs::create_dir_all(&vpn_dir).context("create data/vpn")?;
    let dest = vpn_dir.join(EC2_OVPN_NAME);
    let prepared = prepare_ovpn_content(&raw, &cidrs).context("prepare .ovpn content")?;
    std::fs::write(&dest, prepared).context("write .ovpn")?;
    let _ = log::success(format!(
        "Saved {} with split-tunnel routes [{cidrs}] (will be rsynced to EC2)",
        dest.display()
    ));
    Ok(true)
}

// ── AWS RDS discovery (same-account placement, no VPN) ──────────────────────
//
// When the user's MySQL lives in their own AWS account, the wizard queries the
// RDS via `aws rds describe-db-instances`, picks a public subnet in the RDS's
// VPC, and Terraform places the Bittice EC2 there. The SG of the RDS gets an
// inbound rule from the new EC2's SG so CDC can reach MySQL natively — zero
// VPN, zero tunnel restart problems.

#[derive(Debug, Clone)]
struct RdsPlacement {
    rds_identifier: String,
    region: String,
    vpc_id: String,
    subnet_id: String,
    rds_security_group_id: String,
    rds_port: u16,
    vpc_cidr: String,
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

/// Scans data/profiles/*/cdc_config.json for the first MySQL host matching the
/// AWS RDS endpoint pattern `<id>.<random>.<region>.rds.amazonaws.com`.
/// Returns `(db_instance_identifier, region)` so the wizard can pre-fill them.
fn extract_rds_hint_from_cdc_profiles(data_root: &Path) -> Option<(String, String)> {
    for cfg in crate::core::data_paths::scan_all_cdc_config_paths_in_data_root(data_root) {
        let Ok(txt) = std::fs::read_to_string(&cfg) else { continue };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) else { continue };
        let Some(host) = v.get("host").and_then(|h| h.as_str()) else { continue };
        let host = host.trim().trim_end_matches('.').to_lowercase();
        if !host.ends_with(".rds.amazonaws.com") { continue; }
        let parts: Vec<&str> = host.split('.').collect();
        if parts.len() < 6 { continue; }
        return Some((parts[0].to_string(), parts[2].to_string()));
    }
    None
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

/// Full discovery: from a guessed identifier/region, returns everything Terraform
/// needs to place the EC2 in the RDS's VPC. Prompts the user to pick a public
/// subnet when the VPC has more than one.
async fn discover_rds_placement(
    rds_identifier_hint: Option<String>,
    region_hint: Option<String>,
) -> Result<RdsPlacement> {
    if !aws_cli_available() {
        bail!("aws CLI not installed — install from https://aws.amazon.com/cli/ and run `aws configure`");
    }
    let raw_id: String = match input("RDS instance identifier (not the endpoint hostname)")
        .default_input(rds_identifier_hint.as_deref().unwrap_or(""))
        .interact()
    {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::Interrupted => bail!("cancelled"),
        Err(e) => return Err(e.into()),
    };
    let identifier = raw_id.trim().to_string();
    if identifier.is_empty() { bail!("RDS identifier is required"); }

    let raw_region: String = match input("AWS region of that RDS")
        .default_input(region_hint.as_deref().unwrap_or("us-east-1"))
        .interact()
    {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::Interrupted => bail!("cancelled"),
        Err(e) => return Err(e.into()),
    };
    let region = raw_region.trim().to_string();

    let s = spinner();
    s.start(format!("Querying RDS '{identifier}' in {region}…"));
    let (vpc_id, rds_port, rds_sg_id) = match describe_rds(&identifier, &region) {
        Ok(v) => { s.stop(format!("RDS found in VPC {}.", v.0)); v }
        Err(e) => { s.stop("RDS lookup failed."); return Err(e); }
    };

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

    Ok(RdsPlacement {
        rds_identifier: identifier,
        region,
        vpc_id,
        subnet_id,
        rds_security_group_id: rds_sg_id,
        rds_port,
        vpc_cidr,
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
        s.push_str(&format!(
            "target_vpc_id                = \"{}\"\n\
             target_subnet_id             = \"{}\"\n\
             target_rds_security_group_id = \"{}\"\n\
             rds_port                     = {}\n\
             allowed_admin_cidr           = \"{}\"\n",
            p.vpc_id, p.subnet_id, p.rds_security_group_id, p.rds_port, p.vpc_cidr,
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

    // ── connectivity mode ─────────────────────────────────────────────────────
    // Decides whether we go RDS-aware (auto-discover, no VPN, auto SSH key)
    // or legacy (default VPC, prompt for .pem, optional VPN sidecar).
    let (hint_id, hint_region) = extract_rds_hint_from_cdc_profiles(&data_root)
        .map(|(i, r)| (Some(i), Some(r))).unwrap_or((None, None));
    let aws_ok = aws_cli_available();
    let _ = log::info(if aws_ok {
        "Detected `aws` CLI on PATH — same-account RDS auto-discovery available."
    } else {
        "`aws` CLI not found — only the legacy flow (default VPC, optional OpenVPN) is available."
    });

    let mode: u8 = if aws_ok {
        match select("How does Bittice reach your database?")
            .item(0u8, "AWS RDS in this account (auto-discover VPC, no VPN, no .pem)", "RECOMMENDED")
            .item(1u8, "Reachable directly from the internet (public RDS or non-AWS)", "")
            .item(2u8, "Behind OpenVPN — start a sidecar tunnel", "")
            .item(255u8, "« Back", "")
            .interact()
        {
            Ok(x) => x,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(()),
            Err(e) => return Err(e.into()),
        }
    } else { 1 };
    if mode == 255 { return Ok(()); }

    // RDS-aware path: discover up-front so the rest of the wizard can adapt
    // (region pinned, no SSH prompt, no VPN prompt).
    let placement: Option<RdsPlacement> = if mode == 0 {
        Some(discover_rds_placement(hint_id, hint_region).await?)
    } else { None };

    // ── region ──
    let region: String = if let Some(p) = &placement {
        let _ = log::step(format!("Region pinned to RDS location: {}", p.region));
        p.region.clone()
    } else {
        match select("AWS region")
            .item("us-east-1",      "us-east-1      — N. Virginia", "")
            .item("us-west-2",      "us-west-2      — Oregon", "")
            .item("eu-west-1",      "eu-west-1      — Ireland", "")
            .item("eu-central-1",   "eu-central-1   — Frankfurt", "")
            .item("ap-southeast-1", "ap-southeast-1 — Singapore", "")
            .item("ap-northeast-1", "ap-northeast-1 — Tokyo", "")
            .item("sa-east-1",      "sa-east-1      — São Paulo", "")
            .interact()
        {
            Ok(x) => x.to_string(),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(()),
            Err(e) => return Err(e.into()),
        }
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
    // Auto-managed in AWS-discovered mode (we already have AWS creds, no point
    // asking the user for a .pem); manual .pem path otherwise.
    let (ssh_priv, ssh_pub_key): (String, String) = if placement.is_some() {
        let kp = ensure_ssh_keypair_auto()?;
        let _ = log::success(format!("Using auto-managed SSH key: {}", kp.private_path));
        (kp.private_path, kp.public_key)
    } else {
        let default_priv = format!("{}/.ssh/id_rsa", home_dir().unwrap_or_else(|| "~".into()));
        let _ = log::info("Provide the private key AWS gave you (.pem) or your local id_rsa.");
        let p: String = match input("SSH private key path (.pem or id_rsa)")
            .default_input(&default_priv).interact()
        {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let priv_path = p.trim().to_string();
        let ks = spinner();
        ks.start("Deriving public key…");
        let pub_res = derive_public_key(&priv_path);
        if pub_res.is_ok() { ks.stop("Public key derived."); } else { ks.stop("Failed to derive public key."); }
        (priv_path, pub_res?)
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

    // ── VPN ── only in legacy mode; RDS-in-account placement reaches MySQL natively.
    let vpn_configured = if placement.is_some() {
        let _ = log::info("Same-account placement: skipping VPN setup (EC2 reaches RDS via VPC SG rule).");
        false
    } else {
        let v = ensure_vpn_ready_for_deploy(&data_root).await?;
        if crate::core::data_paths::any_cdc_host_suggests_vpn(&data_root) && !v {
            bail!(
                "MySQL hostnames look VPN-only but no .ovpn is under data/vpn/.\n\
                 Add it via Deploy → OpenVPN profile before provisioning."
            );
        }
        v
    };

    // ── IAM permissions note ──
    let _ = note(
        "AWS IAM permissions required",
        "Your IAM user needs EC2 access (and, for RDS auto-discovery mode, \
         rds:DescribeDBInstances + ec2:DescribeSubnets/Vpcs + ec2:AuthorizeSecurityGroupIngress \
         on the target VPC and the RDS SG).\n\n\
         Easiest fix: attach AmazonEC2FullAccess and AmazonRDSReadOnlyAccess to your IAM user."
    );

    // ── confirm ──
    let summary = match &placement {
        Some(p) => format!(
            "(name: {app_name}, region: {}, VM: {instance_type}, VPC: {} same as RDS {}:{} — no VPN)",
            p.region, p.vpc_id, p.rds_identifier, p.rds_port
        ),
        None => format!(
            "(name: {app_name}, region: {region}, VM: {instance_type}, VPN: {})",
            if vpn_configured { "OpenVPN" } else { "none" }
        ),
    };
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

    // ── write terraform files ──
    let ws = spinner();
    ws.start("Writing Terraform files…");
    let write_res = write_terraform_files(
        &tf_dir,
        &build_tfvars(&region, instance_type, &ssh_pub_key, &app_name, placement.as_ref()),
    );
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

    // Prefer the auto-managed key when present (this is how AWS-discovered
    // deploys provisioned the EC2); fall back to ~/.ssh/id_rsa.
    let home = home_dir().unwrap_or_else(|| "~".into());
    let auto_key = format!("{home}/.bittice/ssh/bittice_id_ed25519");
    let default_priv = if std::path::Path::new(&auto_key).is_file() {
        auto_key
    } else {
        format!("{home}/.ssh/id_rsa")
    };
    let _ = log::info("Provide the same private key used when provisioning this instance.");
    let ssh_priv: String = match input("SSH private key path (.pem, id_rsa, or auto-managed)").default_input(&default_priv).interact() {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(()),
        Err(e) => return Err(e.into()),
    };

    let vpn_configured = ensure_vpn_ready_for_deploy(data_root).await?;
    if crate::core::data_paths::any_cdc_host_suggests_vpn(data_root) && !vpn_configured {
        bail!(
            "MySQL hostnames look VPN-only (e.g. *openvpn* in RDS name) but no .ovpn is under data/vpn/.\n\
             Use Deploy → add OpenVPN profile, or paste your .ovpn when prompted."
        );
    }

    finish_deploy(&ip, ssh_priv.trim(), image, data_root, vpn_configured)
}

fn remote_has_deploy_vpn_material(ip: &str, ssh_key: &str) -> bool {
    let path = format!("/opt/bittice/data/vpn/{EC2_OVPN_NAME}");
    ssh_capture(
        ip,
        ssh_key,
        &format!("test -f {path} && echo yes || test -f /opt/bittice/data/vpn/vpn.conf && echo yes"),
    ) == "yes"
}

fn finish_deploy(
    ip: &str,
    ssh_priv: &str,
    image: &str,
    data_root: &Path,
    mut vpn_configured: bool,
) -> Result<()> {
    let profile_count = crate::core::data_paths::cdc_profile_count(data_root);
    if profile_count == 0 {
        let _ = log::warning(
            "No CDC profiles under data/profiles/ — deploy will start the API with static data only.\n\
             Connect and sync on your PC first, then redeploy.",
        );
    }

    wait_for_ssh(ip, ssh_priv)?;

    let _ = log::step("Syncing data/…");
    ssh_run(ip, ssh_priv,
        "sudo mkdir -p /opt/bittice/data && sudo chown -R ubuntu:ubuntu /opt/bittice"
    )?;
    rsync_data(data_root, ip, ssh_priv)?;

    if !vpn_configured && remote_has_deploy_vpn_material(ip, ssh_priv) {
        let _ = log::info("Using OpenVPN profile already on the server (data/vpn/).");
        vpn_configured = true;
    }
    if crate::core::data_paths::any_cdc_host_suggests_vpn(data_root) && !vpn_configured {
        bail!(
            "MySQL requires VPN but no data/vpn/{} on this machine or the server.\n\
             Use Deploy → add OpenVPN profile, then redeploy.",
            EC2_OVPN_NAME
        );
    }

    let _ = log::step(format!(
        "Deploying {image}{}…",
        if vpn_configured { " (VPN sidecar + Bittice)" } else { "" }
    ));
    deploy_compose(ip, ssh_priv, image, vpn_configured)?;

    wait_for_cdc_live(ip, ssh_priv, profile_count)?;

    let ok = Command::new("ssh")
        .args(["-o", "StrictHostKeyChecking=no", "-o", "ConnectTimeout=10",
               "-i", ssh_priv, &format!("ubuntu@{ip}"), "curl -sf http://localhost:8080"])
        .stdout(Stdio::null()).stderr(Stdio::null())
        .status().map(|s| s.success()).unwrap_or(false);

    let _ = log::success(format!(
        "Bittice running at {ip}  (HTTP admin: {}, CDC profiles: {profile_count})",
        if ok { "OK" } else { "check pending" }
    ));
    let _ = log::info(format!("REST   http://{ip}:3000"));
    let _ = log::info(format!("Admin  http://{ip}:8080"));
    let _ = log::info(format!("gRPC   {ip}:50051"));
    if vpn_configured {
        let _ = log::info(format!(
            "VPN    ssh -i {ssh_priv} ubuntu@{ip} 'docker logs -f bittice-vpn'"
        ));
    }
    let _ = log::info(format!("Logs   ssh -i {ssh_priv} ubuntu@{ip} 'docker logs -f bittice'"));
    Ok(())
}

fn home_dir() -> Option<String> {
    std::env::var("HOME").ok()
}

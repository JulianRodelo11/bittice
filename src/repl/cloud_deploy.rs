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

    // ── VPN (auto from cdc_config `vpn_file`, prompt only if .ovpn still missing) ──
    let vpn_configured = ensure_vpn_ready_for_deploy(&data_root).await?;
    if crate::core::data_paths::any_cdc_host_suggests_vpn(&data_root) && !vpn_configured {
        bail!(
            "MySQL hostnames look VPN-only but no .ovpn is under data/vpn/.\n\
             Add it via Deploy → OpenVPN profile before provisioning."
        );
    }

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

use anyhow::Result;
use std::process::{Command, Stdio};
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

pub struct VpnManager;

fn is_docker() -> bool {
    std::path::Path::new("/.dockerenv").exists() || std::env::var("BITTICE_HOST").is_ok()
}

impl VpnManager {
    pub fn storage_dir() -> PathBuf {
        if let Ok(dir) = std::env::var("BITTICE_VPN_DIR") {
            if !dir.trim().is_empty() {
                return PathBuf::from(dir);
            }
        }

        let app_vpn = PathBuf::from("/app/vpn");
        if app_vpn.exists() || is_docker() {
            return app_vpn;
        }

        PathBuf::from("data/vpn")
    }

    fn resolve_ovpn_path(original_path: &str) -> PathBuf {
        let given = PathBuf::from(original_path);
        if given.exists() {
            return given;
        }

        let file_name = Path::new(original_path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| original_path.to_string());

        let candidates = [
            Self::storage_dir().join(&file_name),
            PathBuf::from("data/vpn").join(&file_name),
            PathBuf::from("/app/vpn").join(&file_name),
            PathBuf::from("/app/data/vpn").join(&file_name),
        ];

        for candidate in candidates {
            if candidate.exists() {
                return candidate;
            }
        }

        given
    }

    /// Checks if OpenVPN is installed on the system
    pub fn is_installed() -> bool {
        Command::new("openvpn")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Attempts to install OpenVPN using apt-get (Debian/Ubuntu)
    pub fn install() -> Result<()> {
        info!("Installing OpenVPN...");
        
        let mut cmd_update = if is_docker() {
            Command::new("apt-get")
        } else {
            Command::new("sudo")
        };
        
        if !is_docker() { cmd_update.arg("apt-get"); }
        cmd_update.arg("update");

        let status = cmd_update.status()?;
        
        if !status.success() {
            warn!("Failed to update package list.");
        }

        let mut cmd_install = if is_docker() {
            Command::new("apt-get")
        } else {
            Command::new("sudo")
        };

        if !is_docker() { cmd_install.arg("apt-get"); }
        cmd_install.args(["install", "-y", "openvpn"]);

        let status = cmd_install.status()?;

        if status.success() {
            info!("OpenVPN installed successfully.");
            Ok(())
        } else {
            Err(anyhow::anyhow!("Failed to install OpenVPN. Please install it manually."))
        }
    }

    /// Prepares the .ovpn file while preserving server-pushed routes by default.
    /// Split tunnel can be enabled explicitly with BITTICE_VPN_SPLIT_TUNNEL=true.
    pub fn prepare_ovpn_file(original_path: &str, db_host: &str) -> Result<String> {
        let path = Self::resolve_ovpn_path(original_path);
        if !path.exists() {
            return Err(anyhow::anyhow!("The file {} does not exist", path.display()));
        }

        let mut content = fs::read_to_string(&path)?;
        let split_tunnel = std::env::var("BITTICE_VPN_SPLIT_TUNNEL")
            .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
            .unwrap_or_else(|_| is_docker());

        // 1. Add baseline compatibility options only.
        let baseline_options = [
            "client",
            "dev tun",
            "data-ciphers AES-256-GCM:AES-128-GCM",
        ];

        for opt in &baseline_options {
            if !content.contains(opt) {
                info!("Adding VPN compatibility option: {}", opt);
                if !content.ends_with('\n') { content.push('\n'); }
                content.push_str(opt);
                content.push('\n');
            }
        }

        // 2. Only enable split tunnel when explicitly requested.
        if split_tunnel {
            let split_options = [
                "route-nopull",
                "pull-filter ignore redirect-gateway",
            ];

            for opt in &split_options {
                if !content.contains(opt) {
                    info!("Adding split-tunnel option to VPN config: {}", opt);
                    if !content.ends_with('\n') { content.push('\n'); }
                    content.push_str(opt);
                    content.push('\n');
                }
            }

            use std::net::ToSocketAddrs;
            let addr_str = format!("{}:3306", db_host);
            if let Ok(mut addrs) = addr_str.to_socket_addrs() {
                if let Some(addr) = addrs.next() {
                    let ip = addr.ip();
                    let route_line = format!("route {} 255.255.255.255 vpn_gateway", ip);
                    if !content.contains(&route_line) {
                        info!("Adding specific split-tunnel route for DB host: {} (IP: {})", db_host, ip);
                        content.push_str(&route_line);
                        content.push('\n');
                    }
                }
            }
        } else {
            info!("VPN: Preserving server-pushed routes (full tunnel mode).");
        }

        let vpn_dir = Self::storage_dir();
        fs::create_dir_all(&vpn_dir)?;

        let new_file_name = format!("prepared_{}", path.file_name().unwrap().to_string_lossy());
        let new_path = vpn_dir.join(new_file_name);
        fs::write(&new_path, content)?;

        Ok(new_path.to_string_lossy().to_string())
    }

    /// Starts OpenVPN in the background
    pub fn start(ovpn_path: &str) -> Result<()> {
        info!("Starting OpenVPN with config: {}", ovpn_path);

        let log_path = Self::storage_dir().join("openvpn.log");
        let _ = fs::create_dir_all(Self::storage_dir());

        // Stop any previous OpenVPN process to avoid duplicate tunnels
        let stop_cmd = if is_docker() {
            "if [ -f /tmp/bittice-openvpn.pid ]; then kill $(cat /tmp/bittice-openvpn.pid) 2>/dev/null || true; fi; command -v pkill >/dev/null 2>&1 && pkill -f openvpn || true"
        } else {
            "if [ -f /tmp/bittice-openvpn.pid ]; then sudo kill $(cat /tmp/bittice-openvpn.pid) 2>/dev/null || true; fi; command -v pkill >/dev/null 2>&1 && sudo pkill -f openvpn || true"
        };
        let _ = Command::new("sh").arg("-c").arg(stop_cmd).status();

        let mut cmd = if is_docker() {
            Command::new("openvpn")
        } else {
            let mut c = Command::new("sudo");
            c.arg("openvpn");
            c
        };

        cmd.args([
            "--config",
            ovpn_path,
            "--daemon",
            "--log-append",
            log_path.to_string_lossy().as_ref(),
            "--writepid",
            "/tmp/bittice-openvpn.pid",
        ]);

        let child = cmd.stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;

        info!("OpenVPN launch command started (PID: {}).", child.id());
        info!("Waiting for VPN interface to initialize...");
        std::thread::sleep(std::time::Duration::from_secs(8));

        let running = Command::new("sh")
            .arg("-c")
            .arg("test -f /tmp/bittice-openvpn.pid && kill -0 $(cat /tmp/bittice-openvpn.pid) 2>/dev/null")
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        if running {
            info!("OpenVPN is running.");
            Ok(())
        } else {
            let tail = fs::read_to_string(&log_path).unwrap_or_default();
            let excerpt = tail.lines().rev().take(10).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join(" | ");
            Err(anyhow::anyhow!("OpenVPN failed to stay running. Log: {}", excerpt))
        }
    }
}

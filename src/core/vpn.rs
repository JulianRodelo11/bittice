use anyhow::Result;
use std::process::{Command, Stdio};
use std::fs;
use std::path::Path;
use tracing::{info, warn};

pub struct VpnManager;

impl VpnManager {
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
        let status = Command::new("sudo")
            .args(["apt-get", "update"])
            .status()?;
        
        if !status.success() {
            warn!("Failed to update package list.");
        }

        let status = Command::new("sudo")
            .args(["apt-get", "install", "-y", "openvpn"])
            .status()?;

        if status.success() {
            info!("OpenVPN installed successfully.");
            Ok(())
        } else {
            Err(anyhow::anyhow!("Failed to install OpenVPN. Please install it manually."))
        }
    }

    /// Prepares the .ovpn file by adding route-nopull and a specific route for the DB host
    pub fn prepare_ovpn_file(original_path: &str, db_host: &str) -> Result<String> {
        let path = Path::new(original_path);
        if !path.exists() {
            return Err(anyhow::anyhow!("The file {} does not exist", original_path));
        }

        let mut content = fs::read_to_string(path)?;

        // 1. Add route-nopull if not present
        if !content.contains("route-nopull") {
            info!("Adding 'route-nopull' to the VPN configuration.");
            if !content.ends_with('\n') { content.push('\n'); }
            content.push_str("route-nopull\n");
        }

        // 2. Add specific route for the DB host
        // We try to resolve the hostname to an IP if it's not already one
        use std::net::ToSocketAddrs;
        let addr_str = format!("{}:3306", db_host); // dummy port for resolution
        if let Ok(mut addrs) = addr_str.to_socket_addrs() {
            if let Some(addr) = addrs.next() {
                let ip = addr.ip();
                let route_line = format!("route {} 255.255.255.255 vpn_gateway", ip);
                if !content.contains(&route_line) {
                    info!("Adding specific route for DB host: {}", route_line);
                    content.push_str(&route_line);
                    content.push('\n');
                }
            }
        }

        // Save to specialized location
        let vpn_dir = Path::new("data/vpn");
        fs::create_dir_all(vpn_dir)?;
        
        let new_file_name = format!("prepared_{}", path.file_name().unwrap().to_string_lossy());
        let new_path = vpn_dir.join(new_file_name);
        fs::write(&new_path, content)?;

        Ok(new_path.to_string_lossy().to_string())
    }

    /// Starts OpenVPN in the background
    pub fn start(ovpn_path: &str) -> Result<()> {
        info!("Starting OpenVPN with config: {}", ovpn_path);
        
        // We use sudo because openvpn usually requires it to create the tun device
        let child = Command::new("sudo")
            .args(["openvpn", "--config", ovpn_path, "--daemon"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;

        info!("OpenVPN started in background (PID: {}).", child.id());
        
        // Wait a bit to allow the interface to initialize
        info!("Waiting for VPN interface to initialize...");
        std::thread::sleep(std::time::Duration::from_secs(8));
        
        Ok(())
    }
}

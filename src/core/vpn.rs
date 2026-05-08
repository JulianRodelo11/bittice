use anyhow::Result;
use std::fs;
#[cfg(target_os = "macos")]
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use tracing::info;
#[cfg(not(target_os = "macos"))]
use tracing::warn;

pub struct VpnManager;

fn is_docker() -> bool {
    std::path::Path::new("/.dockerenv").exists()
        || std::env::var("BITTICE_HOST").is_ok()
        || std::env::var("BITTICE_ENGINE_ONLY")
            .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
            .unwrap_or(false)
}

/// PID written by OpenVPN (`--writepid`). Root-owned daemon: plain `kill -0` from a user often fails (EPERM).
const BITTICE_OPENVPN_PID_FILE: &str = "/tmp/bittice-openvpn.pid";

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

        crate::core::data_paths::vpn_storage_dir()
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
            crate::core::data_paths::vpn_storage_dir().join(&file_name),
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

    #[cfg(target_os = "macos")]
    fn augmented_path_env() -> &'static str {
        "/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin:/usr/local/sbin:/usr/bin:/bin:/usr/sbin:/sbin"
    }

    #[cfg(target_os = "macos")]
    fn which_openvpn_macos() -> Option<PathBuf> {
        let Ok(out) = Command::new("/usr/bin/env")
            .env("PATH", Self::augmented_path_env())
            .args(["/bin/sh", "-c", "command -v openvpn"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
        else {
            return None;
        };
        if !out.status.success() {
            return None;
        }
        let line = BufReader::new(out.stdout.as_slice())
            .lines()
            .next()
            .and_then(|l| l.ok())?;
        let t = line.trim();
        if t.is_empty() {
            return None;
        }
        Some(PathBuf::from(t))
    }

    #[cfg(not(target_os = "macos"))]
    fn which_openvpn_macos() -> Option<PathBuf> {
        None
    }

    fn openvpn_version_ok(bin: &Path) -> bool {
        let mut cmd = Command::new(bin);
        cmd.arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(target_os = "macos")]
        cmd.env("PATH", Self::augmented_path_env());
        cmd.status().map(|s| s.success()).unwrap_or(false)
    }

    /// Prefer absolute path so `sudo openvpn` still resolves on macOS (sudo resets PATH).
    pub fn resolved_openvpn_binary() -> Option<PathBuf> {
        if let Ok(p) = std::env::var("OPENVPN_BINARY") {
            let pb = PathBuf::from(p.trim());
            if pb.as_os_str().is_empty() {
                return None;
            }
            if Self::openvpn_version_ok(&pb) {
                return Some(pb);
            }
        }

        #[cfg(target_os = "macos")]
        {
            let candidates: &[&str] = &[
                "/opt/homebrew/opt/openvpn/sbin/openvpn",
                "/opt/homebrew/sbin/openvpn",
                "/opt/homebrew/bin/openvpn",
                "/usr/local/opt/openvpn/sbin/openvpn",
                "/usr/local/sbin/openvpn",
                "/usr/local/bin/openvpn",
            ];
            for p in candidates {
                let pb = PathBuf::from(p);
                if Self::openvpn_version_ok(&pb) {
                    return Some(pb);
                }
            }
            if let Some(pb) = Self::which_openvpn_macos() {
                if Self::openvpn_version_ok(&pb) {
                    return Some(pb);
                }
            }
        }

        let fallback = PathBuf::from("openvpn");
        if Self::openvpn_version_ok(&fallback) {
            #[cfg(target_os = "macos")]
            if let Some(abs) = Self::which_openvpn_macos() {
                return Some(abs);
            }
            return Some(fallback);
        }
        None
    }

    pub fn is_installed() -> bool {
        Self::resolved_openvpn_binary().is_some()
    }

    #[cfg(target_os = "macos")]
    fn install_macos_brew() -> Result<()> {
        let brew_candidates = [
            PathBuf::from("/opt/homebrew/bin/brew"),
            PathBuf::from("/usr/local/bin/brew"),
        ];
        let brew = brew_candidates
            .into_iter()
            .find(|p| p.exists())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Homebrew not found. Install from https://brew.sh then run: brew install openvpn"
                )
            })?;
        info!("Installing OpenVPN via Homebrew ({})...", brew.display());
        let status = Command::new(&brew)
            .args(["install", "openvpn"])
            .status()
            .map_err(|e| anyhow::anyhow!("brew install openvpn failed to run: {}", e))?;
        if status.success() {
            info!("OpenVPN installed successfully.");
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "`brew install openvpn` failed. Install manually or set OPENVPN_BINARY."
            ))
        }
    }

    pub fn install() -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            return Self::install_macos_brew();
        }
        #[cfg(not(target_os = "macos"))]
        {
            info!("Installing OpenVPN...");

            let mut cmd_update = if is_docker() {
                Command::new("apt-get")
            } else {
                Command::new("sudo")
            };

            if !is_docker() {
                cmd_update.arg("apt-get");
            }
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

            if !is_docker() {
                cmd_install.arg("apt-get");
            }
            cmd_install.args(["install", "-y", "openvpn"]);

            let status = cmd_install.status()?;

            if status.success() {
                info!("OpenVPN installed successfully.");
                Ok(())
            } else {
                Err(anyhow::anyhow!(
                    "Failed to install OpenVPN. Please install it manually."
                ))
            }
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
            .unwrap_or(false);

        let baseline_options = [
            "client",
            "dev tun",
            "data-ciphers AES-256-GCM:AES-128-GCM",
            "mssfix 1400",
        ];

        for opt in &baseline_options {
            if !content.contains(opt) {
                info!("Adding VPN compatibility option: {}", opt);
                if !content.ends_with('\n') {
                    content.push('\n');
                }
                content.push_str(opt);
                content.push('\n');
            }
        }

        if split_tunnel {
            let split_options = [
                "route-nopull",
                "pull-filter ignore redirect-gateway",
            ];

            for opt in &split_options {
                if !content.contains(opt) {
                    info!("Adding split-tunnel option to VPN config: {}", opt);
                    if !content.ends_with('\n') {
                        content.push('\n');
                    }
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
                        info!(
                            "Adding specific split-tunnel route for DB host: {} (IP: {})",
                            db_host, ip
                        );
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

    fn openvpn_daemon_reports_alive(pid_file: &Path) -> bool {
        let Ok(raw) = fs::read_to_string(pid_file) else {
            return false;
        };
        let pid = raw.trim();
        if pid.is_empty() || pid.parse::<u32>().is_err() {
            return false;
        }
        #[cfg(unix)]
        {
            Command::new("ps")
                .args(["-p", pid])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        }
        #[cfg(not(unix))]
        {
            let _ = pid;
            false
        }
    }

    pub fn start(ovpn_path: &str) -> Result<()> {
        let ovpn_exe = Self::resolved_openvpn_binary().ok_or_else(|| {
            anyhow::anyhow!(
                "openvpn executable not found (install it or set OPENVPN_BINARY to the full path)"
            )
        })?;

        info!(
            "Starting OpenVPN with config: {} (binary: {})",
            ovpn_path,
            ovpn_exe.display()
        );

        let log_path = Self::storage_dir().join("openvpn.log");
        let _ = fs::create_dir_all(Self::storage_dir());
        let pid_path = Path::new(BITTICE_OPENVPN_PID_FILE);

        let stop_cmd = if is_docker() {
            format!(
                "if [ -f {f} ]; then kill $(cat {f}) 2>/dev/null || true; fi; command -v pkill >/dev/null 2>&1 && pkill -f openvpn || true",
                f = BITTICE_OPENVPN_PID_FILE
            )
        } else {
            format!(
                "if [ -f {f} ]; then sudo kill $(cat {f}) 2>/dev/null || true; fi; command -v pkill >/dev/null 2>&1 && sudo pkill -f openvpn || true",
                f = BITTICE_OPENVPN_PID_FILE
            )
        };
        let _ = Command::new("sh").arg("-c").arg(&stop_cmd).status();

        let mut cmd = if is_docker() {
            Command::new(&ovpn_exe)
        } else {
            let mut c = Command::new("sudo");
            c.arg(&ovpn_exe);
            c
        };

        cmd.args([
            "--config",
            ovpn_path,
            "--daemon",
            "--log-append",
            log_path.to_string_lossy().as_ref(),
            "--writepid",
            BITTICE_OPENVPN_PID_FILE,
        ]);

        let child = cmd.stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;

        info!("OpenVPN launch command started (wrapper PID: {}).", child.id());
        info!("Waiting for VPN (polling PID + log for up to ~20s)...");

        let mut tail = String::new();
        let mut ok = false;
        for waited in 1..=20u64 {
            std::thread::sleep(Duration::from_secs(1));
            let pid_alive = Self::openvpn_daemon_reports_alive(pid_path);
            if let Ok(s) = fs::read_to_string(&log_path) {
                tail = s;
            }
            let initialized = tail.contains("Initialization Sequence Completed");
            if pid_alive || initialized {
                info!(
                    "OpenVPN is running (after {}s; pid_alive={}, init_seq={}).",
                    waited, pid_alive, initialized
                );
                ok = true;
                break;
            }
        }

        if ok {
            Ok(())
        } else {
            let excerpt = tail
                .lines()
                .rev()
                .take(12)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join(" | ");
            Err(anyhow::anyhow!(
                "OpenVPN failed to stay running or never reached \"Initialization Sequence Completed\". Log tail: {}",
                if excerpt.is_empty() { "(empty)".into() } else { excerpt }
            ))
        }
    }
}

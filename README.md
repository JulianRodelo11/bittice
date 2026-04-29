# Bittice: High-Performance Data Engine

**Bittice** is a high-performance read-layer engine written in Rust, designed to bridge the gap between heavy transactional databases and instant data availability. 

It provides an ultra-fast, columnar-based storage model that acts as a real-time replica, offloading massive search and analytical workloads from your primary databases to save infrastructure costs and maximize performance.

> **Note:** Bittice is licensed under the **Elastic License v2.0**. It is free to use for personal, internal, and commercial purposes (e.g., to save infrastructure costs). However, you may not provide it to others as a managed service.

## 🚀 The Vision: Multi-Source Synchronization
While currently featuring a robust **MySQL** connector via CDC (Change Data Capture), Bittice is architected to be source-agnostic. Our roadmap includes native synchronization for:
*   🐘 **PostgreSQL** (Coming Soon)
*   🗄️ **SQL Server** (Coming Soon)
*   🍃 **MongoDB** (Coming Soon)

## 🦀 Technical Foundation
Bittice is built for extreme efficiency using:
*   **Rust:** For memory safety and zero-cost abstractions.
*   **Tri-File Columnar Mapping (TFCM):** Proprietary storage logic for O(1) retrieval.
*   **Memory-Mapped Files (mmap):** Leveraging the OS page cache for near-instant data access.
*   **Roaring Bitmaps:** For high-speed logical filtering across massive datasets.

---

## 🛠 For Developers: Building from Source

### Prerequisites
*   **Rust & Cargo:** Latest stable version.
*   **Protobuf Compiler:** Required for gRPC interface compilation.

### Build and Run
```bash
# Clone the repository
git clone https://github.com/julianrodelo/bittice.git
cd bittice

# Build and run the interactive CLI
cargo run
```

### Prebuilt binaries (GitHub Releases)
Each `v*` release publishes:

- **Per-OS bundles** (recommended for scripted downloads): `bittice-<tag>-macos.zip`, `bittice-<tag>-linux.tar.gz`, `bittice-<tag>-windows.zip`
- **Standalone** files per target, e.g. `bittice-linux-x86_64`, `bittice-macos-aarch64`, `bittice-windows-x86_64.exe`
- **`bittice-<tag>-downloads.json`** — lists bundle filenames and standalone asset names

`install.sh` (Linux/macOS) prefers the OS bundle and falls back to standalone assets. `install.ps1` (Windows) does the same.

### One-command install (macOS / Linux)

This downloads the latest release, places the binary under **`~/.local/bin`**, marks it executable, and adds that directory to your PATH — **no manual `chmod` or `mv`**, and **no `sudo` on a normal laptop**:

```bash
curl -fsSL https://raw.githubusercontent.com/JulianRodelo11/bittice/main/install.sh | bash
```

Then open a **new terminal** (or `source ~/.zshrc` on macOS / `source ~/.profile` on many Linux setups) and run `bittice`.

- **Cloud VMs** (AWS, GCP, Azure): the script detects them and uses `/usr/local/bin` plus optional Docker.
- **System-wide `/usr/local`** on your own machine (may prompt `sudo` once; the installer then assigns the binary to your user so `bittice update` / uninstall usually need no further `sudo`):

```bash
curl -fsSL https://raw.githubusercontent.com/JulianRodelo11/bittice/main/install.sh | env BITTICE_USE_SYSTEM_INSTALL=1 bash
```

### One-command install (Windows)

From **cmd** or **PowerShell**, same idea as `irm … | iex` installers (e.g. OpenClaw). **Important in cmd:** the full expression `irm … | iex` must stay **inside** the same pair of double quotes after `-c`. If you end the quote before `| iex`, only the script text is fetched and **nothing runs** (so the binary and PATH are not updated).

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -Command "irm https://raw.githubusercontent.com/JulianRodelo11/bittice/main/install.ps1 | iex"
```

Shorter (if execution policy already allows scripts):

```powershell
powershell -c "irm https://raw.githubusercontent.com/JulianRodelo11/bittice/main/install.ps1 | iex"
```

This downloads the **latest** GitHub release, prefers the Windows bundle (`bittice-<tag>-windows.zip`), falls back to `bittice-windows-x86_64.exe`, and installs `bittice.exe` under **`%LOCALAPPDATA%\Programs\Bittice`**, appending that folder to your **user** PATH. Run the installer from a **normal** shell if you can; **Run as administrator** updates the PATH for the admin account, not necessarily the one you use every day. Close the terminal, open a **new** one, and run `bittice --help`. If `cmd` still does not see it, sign out and back in once so Windows reloads PATH.

Optional (same meaning as on Linux/macOS): pin a version from PowerShell directly:

```powershell
$env:BITTICE_VERSION = "v0.1.64"; irm https://raw.githubusercontent.com/JulianRodelo11/bittice/main/install.ps1 | iex
```

### Docker and production servers
See [`deploy/README.md`](deploy/README.md) for building the runtime image, `docker compose` and publishing to GitHub Container Registry (automated on version tags `v*`).

---

## 📜 Documentation & License
For full documentation, API reference, and installation guides, please visit our documentation portal (Coming Soon).

### License
Bittice is licensed under the **Elastic License v2.0**. 

*   **Permitted:** Free for personal use and internal production use within any organization to optimize their own data infrastructure. You can modify and redistribute the code for internal purposes.
*   **Prohibited:** You may not provide Bittice to third parties as a hosted or managed service (SaaS), and you may not sell the software itself or remove licensing/copyright notices.

See [LICENSE](LICENSE) for full details.

---
*Built with passion by Julian Rodelo.*

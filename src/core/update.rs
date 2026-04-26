use self_update::cargo_crate_version;
use anyhow::{Result, anyhow};
use indicatif::{ProgressBar, ProgressStyle};
use self_update::backends::github::ReleaseList;
use std::path::Path;

/// Fail fast if we cannot create files next to the current binary (e.g. `/usr/local/bin` on macOS).
fn ensure_can_replace_current_binary() -> Result<()> {
    let exe = std::env::current_exe().map_err(|e| anyhow!(e))?;
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow!("Could not resolve install directory for {:?}", exe))?;
    let probe = dir.join(format!(".bittice-write-test-{}", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Err(anyhow!(
            "Permission denied writing under {}.\n\
             The updater replaces {:?}, which requires the same privileges as the first install.\n\
             Run:\n\
               sudo {:?} update\n\
             Or reinstall with curl (see README) so your user owns the binary (e.g. under ~/.local/bin).",
            dir.display(),
            exe,
            exe
        )),
        Err(e) => Err(anyhow!(
            "Cannot verify write access to {}: {}",
            dir.display(),
            e
        )),
    }
}

fn is_permission_denied(err: &anyhow::Error) -> bool {
    err.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::PermissionDenied)
    })
}

pub async fn perform_update() -> Result<()> {
    let status = tokio::task::spawn_blocking(move || {
        let pb = ProgressBar::new_spinner();
        pb.set_style(ProgressStyle::default_spinner()
            .template("{spinner:.green} {msg}")
            .unwrap());
        pb.set_message("Checking for updates...");
        pb.enable_steady_tick(std::time::Duration::from_millis(100));

        let os = std::env::consts::OS;
        let arch = std::env::consts::ARCH;
        
        let target = if cfg!(windows) {
            format!("bittice-windows-{}.exe", arch)
        } else {
            format!("bittice-{}-{}", os, arch)
        };

        // 1. Fetch releases
        let releases = ReleaseList::configure()
            .repo_owner("JulianRodelo11")
            .repo_name("bittice")
            .build()?
            .fetch()?;

        let latest_release = releases.first()
            .ok_or_else(|| anyhow!("No releases found in the repository"))?;

        let tag = if latest_release.version.starts_with('v') {
            latest_release.version.clone()
        } else {
            format!("v{}", latest_release.version)
        };

        let current_version = cargo_crate_version!();
        let latest_version = latest_release.version.trim_start_matches('v');
        
        if current_version == latest_version {
            pb.finish_and_clear();
            return Ok::<self_update::Status, anyhow::Error>(self_update::Status::UpToDate(current_version.to_string()));
        }

        ensure_can_replace_current_binary()?;

        // Finish our progress bar before self_update takes over the terminal
        pb.finish_and_clear();

        // 2. Configure and perform update
        let status = match self_update::backends::github::Update::configure()
            .repo_owner("JulianRodelo11")
            .repo_name("bittice")
            .bin_name("bittice")
            .target(&target)
            .target_version_tag(&tag)
            .show_download_progress(true)
            .current_version(cargo_crate_version!())
            .build()?
            .update()
        {
            Ok(s) => s,
            Err(e) => {
                let ae = anyhow::Error::from(e);
                if is_permission_denied(&ae) {
                    let exe = std::env::current_exe().unwrap_or_else(|_| Path::new("bittice").to_path_buf());
                    return Err(anyhow!(
                        "{:#}\n\
                         If you saw this after the download, run:\n\
                           sudo {:?} update",
                        ae,
                        exe
                    ));
                }
                return Err(ae);
            }
        };

        Ok::<self_update::Status, anyhow::Error>(status)
    }).await??;

    if status.updated() {
        println!("✓ Successfully updated to version: {}", status.version());
    } else {
        println!("✓ Already at the latest version: {}", status.version());
    }

    Ok(())
}

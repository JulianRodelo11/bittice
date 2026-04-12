use self_update::cargo_crate_version;
use anyhow::{Result, anyhow};
use indicatif::{ProgressBar, ProgressStyle};
use self_update::backends::github::ReleaseList;

pub async fn perform_update() -> Result<()> {
    let status = tokio::task::spawn_blocking(move || {
        let pb = ProgressBar::new_spinner();
        pb.set_style(ProgressStyle::default_spinner()
            .template("{spinner:.green} {msg}")
            .unwrap());
        pb.set_message("Checking for updates (including betas)...");
        pb.enable_steady_tick(std::time::Duration::from_millis(100));

        let os = std::env::consts::OS;
        let arch = std::env::consts::ARCH;
        
        // Match our GitHub artifact naming convention
        let target = if cfg!(windows) {
            format!("bittice-windows-{}.exe", arch)
        } else {
            format!("bittice-{}-{}", os, arch)
        };

        // 1. Fetch all releases to include Pre-releases (Betas)
        let releases = ReleaseList::configure()
            .repo_owner("JulianRodelo11")
            .repo_name("bittice")
            .build()?
            .fetch()?;

        let latest_release = releases.first()
            .ok_or_else(|| anyhow!("No releases found in the repository"))?;

        // 2. Configure update targeting the specific latest tag found
        let status = self_update::backends::github::Update::configure()
            .repo_owner("JulianRodelo11")
            .repo_name("bittice")
            .bin_name("bittice")
            .target(&target)
            .target_version_tag(&latest_release.version)
            .show_download_progress(true)
            .current_version(cargo_crate_version!())
            .build()?
            .update()?;
        
        pb.finish_and_clear();
        Ok::<self_update::Status, anyhow::Error>(status)
    }).await??;

    if status.updated() {
        println!("Successfully updated to version: {}", status.version());
    } else {
        println!("Already at the latest version: {}", status.version());
    }

    Ok(())
}

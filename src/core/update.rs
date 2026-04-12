use self_update::cargo_crate_version;
use anyhow::Result;
use indicatif::{ProgressBar, ProgressStyle};

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
        
        // Ajustar el nombre del target para que coincida con nuestros artefactos de GitHub
        let target = if cfg!(windows) {
            format!("bittice-windows-{}.exe", arch)
        } else {
            format!("bittice-{}-{}", os, arch)
        };

        let status = self_update::backends::github::Update::configure()
            .repo_owner("JulianRodelo11")
            .repo_name("bittice")
            .bin_name("bittice")
            .target(&target)
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

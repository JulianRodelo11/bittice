use anyhow::Result;
use cliclack::{confirm, intro, outro};
use std::fs;

pub async fn perform_uninstall() -> Result<()> {
    intro("Bittice Uninstaller")?;

    // Docker detection
    let is_docker = std::path::Path::new("/.dockerenv").exists();

    if is_docker {
        println!("\nAttention! Bittice is running inside a Docker container.");
        println!("To completely remove Bittice from this instance, please run the following command on your HOST:");
        println!("\n  \x1b[34m$ docker compose down --rmi all\x1b[0m  (if using docker-compose)");
        println!("  \x1b[34m$ docker rm -f bittice && docker rmi bittice\x1b[0m (if using direct docker commands)\n");
        
        outro("Please perform uninstallation from your instance terminal.")?;
        return Ok(());
    }

    let confirm_prog = confirm("Are you sure you want to uninstall Bittice from your system?")
        .initial_value(false)
        .interact()?;

    if !confirm_prog {
        outro("Uninstallation cancelled.")?;
        return Ok(());
    }

    let delete_data = confirm("Do you also want to delete the 'data/' folder with all your indexes and configurations?")
        .initial_value(false)
        .interact()?;

    // 1. Identify current binary path
    let current_exe = std::env::current_exe()?;
    
    // 2. Delete data folder if requested
    if delete_data {
        let data_root = crate::core::data_paths::resolved_data_root();
        if data_root.exists() {
            fs::remove_dir_all(&data_root)?;
            println!("✓ 'data' folder deleted.");
        }
    }

    // 3. Delete the executable
    if cfg!(windows) {
        println!("\nAttention! On Windows, the .exe file cannot be deleted while it's in use.");
        println!("Please manually delete it at: {:?}", current_exe);
    } else {
        fs::remove_file(&current_exe)?;
        println!("✓ Bittice binary removed from: {:?}", current_exe);
    }

    outro("Bittice has been uninstalled. See you soon!")?;
    Ok(())
}

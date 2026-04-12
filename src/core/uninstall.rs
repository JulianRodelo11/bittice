use anyhow::Result;
use cliclack::{confirm, intro, outro};
use std::fs;

pub async fn perform_uninstall() -> Result<()> {
    intro("Bittice Uninstaller")?;

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
        if std::path::Path::new("data").exists() {
            fs::remove_dir_all("data")?;
            println!("✓ 'data' folder deleted.");
        }
    }

    // 3. Delete the executable
    // On Unix (Linux/Mac) we can delete the file while it's running.
    // On Windows it's more complex, we'll give final instructions.
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

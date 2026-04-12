use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "Bittice")]
#[command(version)]
#[command(about = "Bittice Data Engine", long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Start CDC synchronization from MySQL
    Cdc {
        /// MySQL connection URL (e.g., mysql://root:sakila@localhost:3306/sakila)
        #[arg(short, long)]
        url: String,
        /// Entity name in Bittice
        #[arg(short, long)]
        entity: String,
        /// Database to synchronize
        #[arg(short, long)]
        database: String,
    },
    /// Update the bittice binary to the latest version (Manual)
    Update,
    /// Uninstall Bittice from your system
    Uninstall,
}

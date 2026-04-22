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
        /// Connection profile folder under data/ (stores cdc_config.json and cdc_state.json)
        #[arg(short, long)]
        entity: String,
        /// Database to synchronize (omit when using --sync-all)
        #[arg(short, long)]
        database: Option<String>,
        /// Sync every user database on the server into data/<schema>/
        #[arg(long, default_value_t = false)]
        sync_all: bool,
    },
    /// Update the bittice binary to the latest version (Manual)
    Update,
    /// Uninstall Bittice from your system
    Uninstall,
    /// Run the interactive configuration wizard
    Setup,
}

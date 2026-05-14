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
    /// Start servers in local static mode (no CDC sync)
    Test,
    /// Start CDC synchronization from MySQL
    Cdc {
        /// MySQL connection URL (e.g., mysql://root:sakila@localhost:3306/sakila)
        #[arg(short, long)]
        url: String,
        /// Connection profile under data/profiles/<name>/ (cdc_config.json and cdc_state.json)
        #[arg(short, long)]
        entity: String,
        /// Database to synchronize (omit when using --sync-all)
        #[arg(short, long)]
        database: Option<String>,
        /// Sync every user database on the server into data/mirror/<schema>/
        #[arg(long, default_value_t = false)]
        sync_all: bool,
    },
    /// Update the bittice binary to the latest version (Manual)
    Update,
    /// Uninstall Bittice from your system
    Uninstall,
    /// Run the interactive configuration wizard
    Setup,
    /// Migrate primary.idx from legacy/v1 format to v2 (hash-based)
    MigratePrimaryIndex {
        /// Entity (schema) name. Mutually exclusive with --all.
        #[arg()]
        entity: Option<String>,
        /// Table name. Mutually exclusive with --all.
        #[arg()]
        table: Option<String>,
        /// Migrate all tables in the data directory
        #[arg(long, default_value_t = false)]
        all: bool,
        /// Report statistics without writing anything
        #[arg(long, default_value_t = false)]
        dry_run: bool,
        /// Keep the .v1.bak backup after migration (default: true)
        #[arg(long, default_value_t = true)]
        keep_backup: bool,
        /// Force re-migration even if already v2
        #[arg(long, default_value_t = false)]
        force: bool,
    },
}

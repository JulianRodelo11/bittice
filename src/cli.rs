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
    /// Print the currently authenticated Bittice user (reads ~/.bittice/credentials.json).
    /// You don't log in explicitly — the cloud-deploy wizard prompts for your API key
    /// the first time and persists it for subsequent deploys.
    Whoami,
    /// Remove ~/.bittice/credentials.json. Next cloud deploy will prompt for the API key again.
    Logout,
    /// Migrate exact_<field>.idx files from legacy/v1/v2 to v3 (mmap snapshot)
    MigrateExactIndex {
        /// Entity (schema) name. Mutually exclusive with --all.
        #[arg()]
        entity: Option<String>,
        /// Table name. Mutually exclusive with --all.
        #[arg()]
        table: Option<String>,
        /// Field name (column) — migrates only exact_<field>.idx. Optional.
        #[arg()]
        field: Option<String>,
        /// Migrate all exact index files under data/mirror/
        #[arg(long, default_value_t = false)]
        all: bool,
        /// Report statistics without writing anything
        #[arg(long, default_value_t = false)]
        dry_run: bool,
        /// Keep the .pre_v3.bak backup after migration (default: true)
        #[arg(long, default_value_t = true)]
        keep_backup: bool,
        /// Force re-migration even if already v3
        #[arg(long, default_value_t = false)]
        force: bool,
    },
    /// Compact mirror segment files (reconcile tombstones + merge micro-segments).
    /// Run in a one-shot container while the engine is stopped, or on a copy of data/.
    CompactMirror {
        /// Mirror entity / schema folder (e.g. bittice).
        entity: String,
        /// Table directory name. Omit with --all-tables.
        #[arg()]
        table: Option<String>,
        /// Compact every table under the entity mirror dir.
        #[arg(long, default_value_t = false)]
        all_tables: bool,
    },
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

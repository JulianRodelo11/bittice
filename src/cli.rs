use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "Bittice")]
#[command(version = "1.0")]
#[command(about = "Bittice Data Engine", long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Start the server
    Server {
        /// Port to listen on (default: 50051)
        #[arg(short, long, default_value_t = 50051)]
        port: u16,

        /// Server type: 'grpc', 'http' or 'all'
        #[arg(short, long, default_value = "all")]
        r#type: String,

        /// Specific entity to activate (optional)
        #[arg(short, long)]
        entity: Option<String>,
    },
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
}

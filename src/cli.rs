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
    /// Carga datos desde un NDJSON al formato Bittice
    Load,
}

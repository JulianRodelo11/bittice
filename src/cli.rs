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
    Load {
        /// Ruta al archivo .ndjson
        #[arg(short, long)]
        input: String,
        /// Nombre de la entidad
        #[arg(short, long)]
        entity: String,
        /// Nombre de la tabla
        #[arg(short, long)]
        table: String,
    },
}

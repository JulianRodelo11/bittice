use clap::Parser;
use anyhow::Result;

// Importar módulos desde la librería (nombre del paquete: bittice)
use bittice::cli::{Cli, Commands};
use bittice::repl;
use bittice::commands::load;

fn main() -> Result<()> {
    // Si hay argumentos (más allá del nombre del programa), ejecutamos normalmente.
    // Si no, entramos al modo interactivo.
    if std::env::args().len() > 1 {
        let cli = Cli::parse();
        match cli.command {
            Commands::Load => {
                load::execute_load()?;
            }
        }
    } else {
        repl::run_interactive()?;
    }

    Ok(())
}

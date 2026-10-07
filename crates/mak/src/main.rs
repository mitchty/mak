mod synology;

use clap::{Parser, Subcommand};
use color_eyre::eyre::Result;

#[derive(Parser)]
#[command(name = "mak", about = "Mitchty Army Knife", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Synology NAS management
    Synology {
        #[command(subcommand)]
        command: synology::SynologyCommand,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    let cli = Cli::parse();
    match cli.command {
        None => {
            println!("cli: {}", common::greeting());
            Ok(())
        }
        Some(Command::Synology { command }) => synology::run(command).await,
    }
}

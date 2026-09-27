use std::{path::PathBuf, time::Duration};

use clap::Parser;
use color_eyre::eyre::{Result, WrapErr};
use common::wrap::{self, WrapConfig};

/// Run a command without echoing its output anywhere, so CI runners don't drown
/// in stdout/err noise on crap that is a deluge of text that isn't worth
/// logging until there is a "real" failure. This is to let forgejo mostly run
/// this until failure and only have output that matters logged to a log file.
/// This is mostly just to replace a ton of hacky shell with stuff like timeout
/// and other crap that didn't always do what I wanted.
///
/// With `--timeout` when signalled the command is SIGTERM'd and the exit code
/// is 0. Aka: it ran fine until we were told to stop. If a command exits non 0
/// then this does too.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Cli {
    /// Log file for the commands output. Other steps can pick through it for
    /// whatever is important.
    #[arg(short = 'l', long)]
    log_file: Option<PathBuf>,

    /// How long should this one exec() command run for in total.
    #[arg(short = 't', long)]
    timeout: Option<humantime::Duration>,

    /// Add timestamps to the front of the log output.
    #[arg(short = 'T', long)]
    timestamps: bool,

    /// Repeat the command until it fails.
    #[arg(short = 'r', long)]
    repeat: bool,

    /// Rest is the command to run, and its arguments to exec()
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
    command: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    let cli = Cli::parse();
    let config = WrapConfig {
        command: cli.command,
        log_file: cli.log_file,
        timeout: cli.timeout.map(Into::<Duration>::into),
        timestamps: cli.timestamps,
        repeat: cli.repeat,
    };

    let outcome = wrap::run(config)
        .await
        .wrap_err("cilogger failed to run the wrapped command at all")?;

    std::process::exit(wrap::process_exit_code(outcome));
}

use std::io::Read;
use std::process::ExitCode;

use ecahook::history::{self, Entry};

// Exit 1 on any failure, ECA treats that as non-blocking and runs the original command in this case.
fn main() -> ExitCode {
    let mut raw = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut raw) {
        eprintln!("ecahook: reading stdin: {e}");
        return ExitCode::FAILURE;
    }

    let path = std::env::var_os("PATH").unwrap_or_default();
    match ecahook::run(&raw, &path) {
        Ok((decision, out)) => {
            history::record(&Entry::from_decision(&raw, &decision));
            println!("{out}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            history::record(&Entry::from_error(&raw, &e.to_string()));
            eprintln!("ecahook: {e}");
            ExitCode::FAILURE
        }
    }
}

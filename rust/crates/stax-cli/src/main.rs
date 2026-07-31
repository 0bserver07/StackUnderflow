//! The `stax` binary — a thin shell over [`stax_cli`].

#![forbid(unsafe_code)]

use std::process::ExitCode;

fn main() -> ExitCode {
    match stax_cli::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("stax: {error:#}");
            ExitCode::FAILURE
        }
    }
}

//! `caucus` binary entry point. All real logic lives in `caucus::cli`.

use std::process::ExitCode;

fn main() -> ExitCode {
    caucus::cli::run()
}

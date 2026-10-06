// The Nix helper is also built directly with rustc; share its publication owner.
#[path = "lib.rs"]
mod publication;
use publication::compile;
use std::{path::Path, process::ExitCode};

fn main() -> ExitCode {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    if args.len() < 3 {
        eprintln!("usage: compile-wrap STAGING OUTPUT COMMAND [ARGS...]");
        return ExitCode::FAILURE;
    }
    match compile(Path::new(&args[0]), Path::new(&args[1]), &args[2..]) {
        Ok(status) => match status.code() {
            Some(code) => ExitCode::from(code as u8),
            None => {
                eprintln!("compile-wrap: compiler terminated with {status}");
                ExitCode::FAILURE
            }
        },
        Err(error) => {
            eprintln!("compile-wrap: {error}");
            ExitCode::FAILURE
        }
    }
}

// Nix makeHelper compiles this std-only binary directly with rustc. Share the
// same implementation exposed as a Cargo library to backend source validation.
#[path = "lib.rs"]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Writable outputs are used by the dyndrv library consumer, not this standalone CLI"
    )
)]
mod source_tree;

use std::{
    path::Path,
    process::{Command, ExitCode},
};

fn run() -> std::io::Result<ExitCode> {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    if args.first().is_some_and(|arg| arg == "--generated-v1") {
        return source_tree::generated::run(&args[1..]);
    }
    if args.len() < 2 {
        return Err(std::io::Error::other(
            "usage: source-wrap SOURCE COMMAND [ARGS...]",
        ));
    }
    // Fixed paths require the existing private Linux Nix /build sandbox.
    if std::env::current_dir()? != Path::new("/build") {
        return Err(std::io::Error::other(
            "source-wrap requires a private /build cwd",
        ));
    }
    let source = Path::new(&args[0]);
    let destination = Path::new("/build/source");
    source_tree::Materializer::Copy.materialize(source, destination, &[])?;
    let status = Command::new(&args[1])
        .args(&args[2..])
        .current_dir(destination)
        .status()?;
    source_tree::verify_unchanged(source, destination, &[])?;
    match status.code() {
        Some(code) => Ok(ExitCode::from(code as u8)),
        None => Err(std::io::Error::other(format!(
            "source command terminated with {status}"
        ))),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("source-wrap: {error}");
            ExitCode::FAILURE
        }
    }
}

use std::{
    ffi::OsStr,
    os::unix::{ffi::OsStrExt, process::CommandExt},
    process::Command,
};

fn main() {
    let mut args = std::env::args_os();
    args.next().expect("own command name argument");

    let mut env_files = Vec::new();
    loop {
        let name = args.next().expect("environment variable files before a --");
        if name == OsStr::from_bytes(b"--") {
            break;
        }
        env_files.push(name);
    }

    let mut command = Command::new(args.next().expect("command name"));
    command.args(args);

    for file in env_files {
        // files should be small, hopefully no one is putting megabytes in environment
        let content = std::fs::read_to_string(file).expect("unable to read file");
        for line in content.lines() {
            if let Some((key, val)) = line.split_once('=') {
                command.env(key, val);
            } else {
                eprintln!("env-wrap: unable to parse line {}, skipping", line);
            }
        }
    }

    panic!("{}: {}", command.exec(), "unable to execute");
}

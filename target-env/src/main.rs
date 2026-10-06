use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
};

fn main() {
    let mut args = std::env::args();
    args.next().expect("self argument");
    let rustc_path: PathBuf = args.next().expect("rustc path argument").into();
    let specified_target = args.next().expect("target argument");
    let out_path: PathBuf = args.next().expect("out path argument").into();

    let mut out_file = File::create(out_path).expect("unable to create out file");

    let host_tuple_output = Command::new(&rustc_path)
        .arg("--print=host-tuple")
        .output()
        .expect("running rustc --print=host-tuple");

    if !host_tuple_output.status.success() {
        panic!("rustc --print=host-tuple reported failure");
    }

    let host_tuple = str::from_utf8(&host_tuple_output.stdout)
        .expect("host tuple was not valid utf-8")
        .trim();

    writeln!(out_file, "HOST={}", host_tuple).unwrap();

    let target = if specified_target.is_empty() {
        host_tuple
    } else {
        &specified_target
    };

    writeln!(out_file, "TARGET={}", target).unwrap();

    let cfg_output = Command::new(&rustc_path)
        .arg("--target")
        .arg(target)
        .arg("--print=cfg")
        .stderr(Stdio::inherit())
        .output()
        .expect("running rustc --print=cfg");

    if !cfg_output.status.success() {
        panic!("rustc --print=cfg reported failure");
    }

    let cfg_raw =
        str::from_utf8(&cfg_output.stdout).expect("rustc --print=cfg was not valid utf-8");

    // values may be specified multiple times, should be comma separated in env
    // rustc normally does keep order, but that's not guaranteed
    let mut cfg_map: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for line in cfg_raw.lines() {
        if line.is_empty() {
            continue;
        }

        let (key, value) = if let Some((lhs, rhs)) = line.split_once('=') {
            // format should be key="value"
            if rhs.len() < 2 {
                continue;
            }
            (lhs, &rhs[1..rhs.len() - 1])
        } else {
            (line, "")
        };
        cfg_map.entry(key).or_default().insert(value);
    }

    for (key, values) in cfg_map {
        let value = values.into_iter().collect::<Vec<_>>().join(",");
        writeln!(out_file, "CARGO_CFG_{}={}", key.to_uppercase(), value).unwrap();
    }
}

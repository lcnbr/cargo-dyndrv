mod process;

use std::{
    fs::File,
    io::{self, BufRead as _, BufReader, Write as _},
    path::PathBuf,
    process::ExitCode,
};

fn push_arg(file: &mut File, flag: &str, value: &str) -> io::Result<()> {
    // write_all_vectored is unstable, unfortunately. easiest to do this
    // flag files are utf-8, trailing newlines are fine
    file.write_all(format!("{}\n{}\n", flag, value).as_bytes())
}

fn envify(value: &str) -> String {
    value.to_uppercase().replace('-', "_")
}

#[derive(Debug, PartialEq, Eq)]
enum DependencyDirective<'a> {
    Rerun,
    Metadata { key: &'a str, value: &'a str },
}

impl<'a> DependencyDirective<'a> {
    const MALFORMED_METADATA: &'static str =
        "cargo::metadata requires KEY=VALUE with an '=' character";

    fn parse(key: &'a str, value: &'a str, modern_syntax: bool) -> Result<Self, String> {
        match key {
            // These instructions stay in logged stdout. They are not DEP_*
            // metadata; Cargo-style file/environment invalidation is not yet
            // implemented by this wrapper.
            "rerun-if-changed" | "rerun-if-env-changed" => Ok(Self::Rerun),
            "metadata" if modern_syntax => {
                let (key, value) = value
                    .split_once('=')
                    .ok_or_else(|| Self::MALFORMED_METADATA.to_owned())?;
                Ok(Self::Metadata {
                    key,
                    value: value.trim_end(),
                })
            }
            // `metadata` is not reserved in legacy cargo: syntax. Preserve
            // that key, as for any other legacy metadata key. Unknown modern
            // directives retain existing handling; full directive validation
            // remains outside this metadata correction.
            key => Ok(Self::Metadata {
                key,
                value: value.trim_end(),
            }),
        }
    }

    fn write(self, file: &mut impl io::Write, links_key: &str) -> io::Result<()> {
        match self {
            Self::Rerun => Ok(()),
            Self::Metadata { key, value } => {
                file.write_all(format!("DEP_{}_{}={}\n", links_key, envify(key), value).as_bytes())
            }
        }
    }
}

// Cargo accepts only -l and -L, with either attached or whitespace-separated
// values. Validate the complete directive before writing any response arguments.
fn parse_rustc_flags(value: &str) -> Result<Vec<(&str, &str)>, String> {
    let mut words = value.split_whitespace();
    let mut flags = Vec::new();
    while let Some(word) = words.next() {
        if !word.starts_with("-l") && !word.starts_with("-L") {
            return Err(format!(
                "only -l and -L are allowed in rustc-flags: {value}"
            ));
        }
        let (flag, attached) = word.split_at(2);
        let argument = if attached.is_empty() {
            words
                .next()
                .ok_or_else(|| format!("flag in rustc-flags has no value: {value}"))?
        } else {
            attached
        };
        flags.push((flag, argument));
    }
    Ok(flags)
}

fn main() -> ExitCode {
    let mut args = std::env::args();
    args.next().expect("more than 0 arguments");
    // nix derivations can't set cwd, do it ourselves
    let cwd = args.next().expect("crate root argument");

    let links_key = envify(&args.next().expect("links key argument"));
    let flags_dir: PathBuf = args.next().expect("flags directory argument").into();
    let out_dir: PathBuf = args.next().expect("out directory argument").into();
    let executable: PathBuf = args.next().expect("executable argument").into();

    std::fs::create_dir(&flags_dir).expect("creating flags directory");
    std::fs::create_dir(&out_dir).expect("creating out directory");

    // TODO: all the other link-arg-*
    let mut immediate_file = File::create_new(flags_dir.join("args-immediate")).unwrap();
    let mut transitive_file = File::create_new(flags_dir.join("args-transitive")).unwrap();
    // must be separate since --env-set is unstable
    let mut env_file = File::create_new(flags_dir.join("env")).unwrap();
    // separate from env since it gets set in different packages

    let mut metadata_file = if !links_key.is_empty() {
        Some(File::create_new(flags_dir.join("metadata")).unwrap())
    } else {
        None
    };

    let mut error_msg = None;

    let process =
        process::ScriptProcess::new(executable, cwd.into(), out_dir, args.map(Into::into));
    let mut child = process.command().spawn().expect("could not start process");

    let reader = BufReader::new(child.stdout.take().unwrap());

    for line in reader.lines() {
        // technically using a string here means we're limiting args,
        // but cargo also forces args to be valid utf-8
        let line = line.expect("Error reading line");
        println!("{}", line);
        let Some(rest) = line.strip_prefix("cargo:") else {
            continue;
        };
        // Retain the syntax distinction: metadata= is an instruction only
        // with cargo::, while cargo:metadata= defines the legacy metadata key.
        let modern_syntax = rest.starts_with(':');
        let rest = rest.strip_prefix(":").unwrap_or(rest);

        let Some((key, value)) = rest.split_once('=') else {
            if modern_syntax && rest == "metadata" {
                error_msg = Some(DependencyDirective::MALFORMED_METADATA.to_owned());
            }
            continue;
        };

        match key {
            "rustc-link-arg" => {
                push_arg(&mut transitive_file, "-C", &format!("link-arg={}", value))
            }
            "rustc-link-lib" => push_arg(&mut transitive_file, "-l", value),
            "rustc-link-search" => push_arg(&mut transitive_file, "-L", value),
            "rustc-cfg" => push_arg(&mut immediate_file, "--cfg", value),
            // A response-file line is one argument; preserve quotes and escapes.
            "rustc-check-cfg" => push_arg(&mut immediate_file, "--check-cfg", value),
            "rustc-flags" => match parse_rustc_flags(value) {
                Ok(flags) => flags
                    .iter()
                    .try_for_each(|(flag, value)| push_arg(&mut transitive_file, flag, value)),
                Err(message) => {
                    // Drain the child's output and wait for it before failing,
                    // just as for cargo::error, without publishing invalid flags.
                    error_msg = Some(message);
                    Ok(())
                }
            },
            "rustc-env" => {
                // the lines format already disallows variables with newlines,
                // might as well use it as a separator
                env_file.write_all(format!("{}\n", value).as_bytes())
            }
            "error" => {
                error_msg = Some(value.to_string());
                Ok(())
            }
            key => match DependencyDirective::parse(key, value, modern_syntax) {
                Ok(directive) => {
                    if let Some(metadata_file) = &mut metadata_file {
                        directive.write(metadata_file, &links_key)
                    } else {
                        Ok(())
                    }
                }
                Err(message) => {
                    // Keep draining/waiting through the existing error path.
                    error_msg = Some(message);
                    Ok(())
                }
            },
        }
        .expect("could not write arguments");
    }

    let status = child.wait().expect("could not wait for process");

    if let Some(msg) = error_msg {
        println!("Build script error: {}", msg);
        ExitCode::FAILURE
    } else if let Some(exit_code) = status.code() {
        (exit_code as u8).into()
    } else {
        eprintln!("Build script terminated without an exit code: {status}");
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rerun_instructions_do_not_become_dependency_metadata() {
        for modern in [false, true] {
            let mut output = Vec::new();
            for key in ["rerun-if-changed", "rerun-if-env-changed"] {
                let directive =
                    DependencyDirective::parse(key, "path=or_variable", modern).unwrap();
                assert_eq!(directive, DependencyDirective::Rerun);
                directive.write(&mut output, "NATIVE").unwrap();
            }
            assert!(output.is_empty());
        }
    }

    #[test]
    fn real_metadata_can_be_named_like_a_rerun_instruction() {
        let mut output = Vec::new();
        DependencyDirective::parse("metadata", "rerun-if-changed=real-value", true)
            .unwrap()
            .write(&mut output, "NATIVE")
            .unwrap();
        DependencyDirective::parse("rerun-if-changed-extra", "legacy-value", false)
            .unwrap()
            .write(&mut output, "NATIVE")
            .unwrap();
        assert_eq!(output, b"DEP_NATIVE_RERUN_IF_CHANGED=real-value\nDEP_NATIVE_RERUN_IF_CHANGED_EXTRA=legacy-value\n");
    }

    #[test]
    fn legacy_and_modern_metadata_syntax_remain_distinct() {
        let mut output = Vec::new();
        for (key, value, modern) in [
            ("metadata", "broken", false),
            ("metadata", "key=value=with=equals", false),
            ("metadata", "key=value=with=equals", true),
            ("key", "", false),
            ("metadata", "empty=", true),
            ("metadata", "spaces= leading value \t", true),
        ] {
            DependencyDirective::parse(key, value, modern)
                .unwrap()
                .write(&mut output, "NATIVE")
                .unwrap();
        }
        assert_eq!(output, b"DEP_NATIVE_METADATA=broken\nDEP_NATIVE_METADATA=key=value=with=equals\nDEP_NATIVE_KEY=value=with=equals\nDEP_NATIVE_KEY=\nDEP_NATIVE_EMPTY=\nDEP_NATIVE_SPACES= leading value\n");
    }

    #[test]
    fn malformed_modern_metadata_is_rejected_before_writing() {
        for value in ["", "broken", "no equals"] {
            let result = DependencyDirective::parse("metadata", value, true);
            assert!(result.is_err(), "{value}");
            assert!(result.unwrap_err().contains("KEY=VALUE"));
        }
        // Cargo's split-at-first-equals parser permits an empty key/value.
        assert_eq!(
            DependencyDirective::parse("metadata", "=", true).unwrap(),
            DependencyDirective::Metadata { key: "", value: "" }
        );
    }

    #[test]
    fn metadata_environment_names_match_cargo() {
        assert_eq!(envify("native-lib"), "NATIVE_LIB");
        assert_eq!(envify("include-dir"), "INCLUDE_DIR");
        assert_eq!(envify("straße-Δ"), "STRASSE_Δ");
        assert_eq!(envify("keep.dot_underscore"), "KEEP.DOT_UNDERSCORE");
    }

    #[test]
    fn rustc_flags_accept_attached_and_separated_values_and_whitespace() {
        assert_eq!(
            parse_rustc_flags("  -Lnative=/lib\t-l static=foo \u{2003}-L /other -lbar  ").unwrap(),
            vec![
                ("-L", "native=/lib"),
                ("-l", "static=foo"),
                ("-L", "/other"),
                ("-l", "bar"),
            ]
        );
        assert!(parse_rustc_flags(" \t ").unwrap().is_empty());
        // Match Cargo's token grammar, without adding independent value parsing.
        assert_eq!(
            parse_rustc_flags("-l -Lfoo").unwrap(),
            vec![("-l", "-Lfoo")]
        );
    }

    #[test]
    fn rustc_flags_reject_other_options_and_missing_values() {
        for invalid in [
            "-C opt-level=3",
            "--cfg foo",
            "-l okay --cfg foo",
            "-L",
            "-l",
        ] {
            assert!(parse_rustc_flags(invalid).is_err(), "{invalid}");
        }
    }
}

use std::{
    collections::{BTreeMap, VecDeque},
    ffi::OsStr,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use bytes::Bytes;
use color_eyre::eyre::{self, Context as _};
use harmonia_store_path::{StoreDir, StorePath};

use crate::util::CloneBytes;

fn containing_store_path(store_dir: &StoreDir, path: &Path) -> Option<StorePath> {
    let mut components = path.strip_prefix(store_dir.to_path()).ok()?.components();

    let std::path::Component::Normal(part) = components.next()? else {
        return None;
    };
    let store_path = StorePath::from_base_path(part.to_str()?).ok()?;
    Some(store_path)
}

#[derive(Debug, Clone)]
pub struct Tool {
    pub store_path: StorePath,
    pub real_path: PathBuf,
}

impl Tool {
    /// Explicit bound-input route: no environment lookup or host-tool fallback.
    pub(crate) fn bound(store_dir: &StoreDir, path: &Path) -> eyre::Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        if !path.is_absolute()
            || path
                .components()
                .any(|p| matches!(p, std::path::Component::ParentDir))
        {
            eyre::bail!("bound tool path must be absolute without parent components");
        }
        let canonical = path.canonicalize()?;
        let store_path = containing_store_path(store_dir, path)
            .ok_or_else(|| eyre::eyre!("bound tool is not in the Nix store"))?;
        if containing_store_path(store_dir, &canonical).is_none()
            || !canonical.is_file()
            || canonical.metadata()?.permissions().mode() & 0o111 == 0
        {
            eyre::bail!("bound tool does not resolve to an executable in the Nix store");
        }
        Ok(Self {
            store_path,
            real_path: path.to_path_buf(),
        })
    }
    pub(crate) fn find(
        store_dir: &StoreDir,
        tool_name: &str,
        fallback: Option<&str>,
    ) -> eyre::Result<Self> {
        let var_name = tool_name.to_uppercase().replace('-', "_");
        let real_path = if let Some(discovered) = std::env::var_os(var_name) {
            which::which(&discovered).wrap_err_with(|| {
                format!(
                    "could not find tool {}, tried {}",
                    tool_name,
                    discovered.display()
                )
            })?
        } else if let Some(fallback) = fallback {
            fallback.into()
        } else {
            which::which(tool_name)
                .wrap_err_with(|| format!("could not find tool {}", tool_name))?
        };

        let store_path = containing_store_path(store_dir, &real_path)
            .ok_or_else(|| eyre::eyre!("tool {} is not in the Nix store", tool_name))?;

        Ok(Self {
            store_path,
            real_path,
        })
    }

    /// Bind the planned linker to this declared tool without overriding Cargo's
    /// argument position. This does not admit native flags into graph planning.
    pub(crate) fn bind_linker(
        &self,
        planned_args: &[String],
        argv: &mut VecDeque<Bytes>,
    ) -> eyre::Result<()> {
        let mut args = planned_args.iter();
        let mut linker = None;
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-C" => {
                    let value = args
                        .next()
                        .ok_or_else(|| eyre::eyre!("incomplete planned codegen argument"))?;
                    if value == "linker" {
                        eyre::bail!("planned linker has no value");
                    }
                    if let Some(path) = value.strip_prefix("linker=")
                        && (path.is_empty() || linker.replace(path).is_some())
                    {
                        eyre::bail!("planned linker must occur once with a nonempty value");
                    }
                }
                "--codegen" => {
                    let value = args
                        .next()
                        .ok_or_else(|| eyre::eyre!("incomplete planned codegen argument"))?;
                    if value == "linker" || value.starts_with("linker=") {
                        eyre::bail!("planned linker requires separated -C syntax");
                    }
                }
                value
                    if value == "-Clinker"
                        || value.starts_with("-Clinker=")
                        || value == "--codegen=linker"
                        || value.starts_with("--codegen=linker=") =>
                {
                    eyre::bail!("planned linker requires separated -C syntax");
                }
                _ => {}
            }
        }
        if linker.is_some_and(|path| OsStr::new(path) != self.real_path.as_os_str()) {
            eyre::bail!("planned linker differs from the declared tool");
        }
        if linker.is_none() {
            argv.extend([
                "-C".into(),
                format!("linker={}", self.real_path.display()).into(),
            ]);
        }
        Ok(())
    }

    pub fn path_entry(&self) -> &[u8] {
        // any binary in the nix store should have a parent, it cannot be `/`
        self.real_path.parent().unwrap().as_os_str().as_bytes()
    }
}

pub struct Tools {
    pub rustc: Tool,
    pub cargo: Tool,
    pub target_cc: Tool,
    pub host_cc: Tool,
    pub target_cxx: Tool,
    pub host_cxx: Tool,
    pub env_wrap: Tool,
    pub build_wrap: Tool,
    pub compile_wrap: Tool,
    pub target_env: Tool,
    pub ln: Tool,
}

impl Tools {
    pub fn find(store_dir: &StoreDir) -> eyre::Result<Self> {
        let cc = Tool::find(store_dir, "cc", None)?;
        // This is really CC_FOR_BUILD, but this is what Rust calls it
        let host_cc = Tool::find(store_dir, "host-cc", None).unwrap_or_else(|_| cc.clone());
        let cxx = Tool::find(store_dir, "cxx", None)?;
        // This is really CC_FOR_BUILD, but this is what Rust calls it
        let host_cxx = Tool::find(store_dir, "host-cxx", None).unwrap_or_else(|_| cc.clone());

        Ok(Self {
            rustc: Tool::find(store_dir, "rustc", None)?,
            cargo: Tool::find(store_dir, "cargo", None)?,
            target_cc: cc,
            host_cc,
            target_cxx: cxx,
            host_cxx,
            env_wrap: Tool::find(store_dir, "env-wrap", option_env!("ENV_WRAP"))?,
            build_wrap: Tool::find(store_dir, "build-wrap", option_env!("BUILD_WRAP"))?,
            compile_wrap: Tool::find(store_dir, "compile-wrap", option_env!("COMPILE_WRAP"))?,
            target_env: Tool::find(store_dir, "target-env", option_env!("TARGET_ENV"))?,
            ln: Tool::find(store_dir, "ln", option_env!("LN"))?,
        })
    }

    pub fn base_environment(&self) -> BTreeMap<Bytes, Bytes> {
        BTreeMap::from([("RUSTC".into(), self.rustc.real_path.clone_bytes())])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> Tool {
        Tool {
            store_path: StorePath::from_base_path("00000000000000000000000000000000-cc").unwrap(),
            real_path: "/nix/store/00000000000000000000000000000000-cc/bin/cc".into(),
        }
    }
    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }
    fn command(planned: &[String]) -> VecDeque<Bytes> {
        let mut argv = VecDeque::from(["rustc".into(), "src/lib.rs".into()]);
        argv.extend(planned.iter().cloned().map(Bytes::from));
        argv.push_back("@native-directives".into());
        argv
    }

    #[test]
    fn fallback_preserves_backend_position_and_unrelated_codegen_flags() {
        let tool = tool();
        let planned = strings(&[
            "-C",
            "linker-plugin-lto",
            "-Clinker-plugin-lto",
            "--codegen",
            "linker-plugin-lto",
            "--codegen=linker-plugin-lto",
        ]);
        let mut argv = command(&planned);
        let mut expected = argv.clone();
        expected.extend([
            "-C".into(),
            format!("linker={}", tool.real_path.display()).into(),
        ]);
        tool.bind_linker(&planned, &mut argv).unwrap();
        assert_eq!(argv, expected);
    }

    #[test]
    fn matching_linker_preserves_the_entire_command_without_an_override() {
        let tool = tool();
        let planned = strings(&[
            "--cfg",
            "before",
            "-C",
            "linker=/nix/store/00000000000000000000000000000000-cc/bin/cc",
            "--check-cfg=cfg(after)",
        ]);
        let mut argv = command(&planned);
        let expected = argv.clone();
        tool.bind_linker(&planned, &mut argv).unwrap();
        assert_eq!(argv, expected);
    }

    #[test]
    fn ambiguous_or_unbound_linkers_fail_before_mutating_the_command() {
        let tool = tool();
        let matching = "linker=/nix/store/00000000000000000000000000000000-cc/bin/cc";
        for args in [
            vec!["-C", "linker=/unbound/cc"],
            vec!["-C", matching, "-C", matching],
            vec!["-C", matching, "-C", "linker=/other/cc"],
            vec!["-C", "linker="],
            vec!["-C", "linker"],
            vec!["-C"],
            vec!["-Clinker=/unbound/cc"],
            vec!["-Clinker="],
            vec!["-Clinker"],
            vec!["--codegen", matching],
            vec!["--codegen", "linker"],
            vec!["--codegen=linker=/unbound/cc"],
            vec!["--codegen=linker="],
            vec!["--codegen=linker"],
            vec!["--codegen"],
        ] {
            let planned = strings(&args);
            let mut argv = command(&planned);
            let before = argv.clone();
            assert!(tool.bind_linker(&planned, &mut argv).is_err(), "{args:?}");
            assert_eq!(argv, before, "{args:?}");
        }
    }

    #[test]
    fn linker_binding_does_not_normalize_the_requested_path() {
        let planned = strings(&[
            "-C",
            "linker=/nix/store/00000000000000000000000000000000-cc/bin/./cc",
        ]);
        let mut argv = command(&planned);
        let before = argv.clone();
        assert!(tool().bind_linker(&planned, &mut argv).is_err());
        assert_eq!(argv, before);
    }
}

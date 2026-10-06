//! Publish completed Cargo outputs through Nix's restricted builder RPC.
//! Cargo remains the compilation owner; this command does not submit builds.
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::Command,
    str::FromStr,
    time::Instant,
};

use color_eyre::eyre::{self, WrapErr as _};
use harmonia_store_derivation::derived_path::OutputName;
use harmonia_store_path::{StoreDir, StorePath, StorePathName};

use crate::tools::Tool;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Publication {
    nix: PathBuf,
    outputs: BTreeMap<String, PathBuf>,
}

impl Publication {
    pub(crate) fn cli(args: &[String]) -> eyre::Result<()> {
        let [option, path] = args else {
            eyre::bail!("usage: cargo-dyndrv publish-native --spec <immutable-json>");
        };
        if option != "--spec" {
            eyre::bail!("usage: cargo-dyndrv publish-native --spec <immutable-json>");
        }
        let store = StoreDir::default();
        let spec = Path::new(path).canonicalize()?;
        if !spec.starts_with(store.to_path()) || !spec.is_file() {
            eyre::bail!("publication spec must be an immutable Nix input");
        }
        let publication: Self = serde_json::from_slice(&std::fs::read(spec)?)?;
        let nix = Tool::bound(&store, &publication.nix)?;
        let top = PathBuf::from(std::env::var("NIX_BUILD_TOP")?).canonicalize()?;
        let remote = std::env::var("NIX_REMOTE")?;
        if !remote.starts_with("unix://") {
            eyre::bail!("native publication requires the builder RPC socket");
        }
        let declared = std::env::var("outputs")?;
        let name = std::env::var("name")?;
        publication.validate(&top, &declared, &name)?;

        let started = Instant::now();
        let mut receipts = Vec::new();
        for (output, directory) in &publication.outputs {
            let store_name = if output == "out" {
                name.clone()
            } else {
                format!("{name}-{output}")
            };
            let added = Instant::now();
            // Scan the actual NAR contents. An empty explicit reference set
            // would silently omit compiler/native inputs from snapshot delivery.
            let result = Command::new(&nix.real_path)
                .args([
                    "--extra-experimental-features",
                    "nix-command",
                    "store",
                    "add",
                    "--scan",
                    "--name",
                    &store_name,
                ])
                .arg(directory)
                .output()?;
            if !result.status.success() {
                eyre::bail!(
                    "adding {output} failed: {}",
                    String::from_utf8_lossy(&result.stderr)
                );
            }
            let path = String::from_utf8(result.stdout)?.trim().to_owned();
            let base = Path::new(&path).strip_prefix(store.to_path())?;
            StorePath::from_base_path(
                base.to_str()
                    .ok_or_else(|| eyre::eyre!("non-UTF8 store result"))?,
            )?;
            let add_seconds = added.elapsed().as_secs_f64();
            let submitted = Instant::now();
            let status = Command::new(&nix.real_path)
                .args([
                    "--extra-experimental-features",
                    "nix-command",
                    "store",
                    "submit-output",
                    &path,
                    output,
                ])
                .status()?;
            if !status.success() {
                eyre::bail!("submitting {output} failed with {status}");
            }
            receipts.push(serde_json::json!({
                "output": output, "path": path, "addSeconds": add_seconds,
                "submitSeconds": submitted.elapsed().as_secs_f64(),
            }));
        }
        println!(
            "{}",
            serde_json::json!({
                "kind": "cargo-dyndrv-native-publication", "schema": 1,
                "seconds": started.elapsed().as_secs_f64(), "outputs": receipts,
                "compilerOwner": "cargo", "nestedStore": false,
            })
        );
        Ok(())
    }

    fn validate(&self, top: &Path, declared: &str, name: &str) -> eyre::Result<()> {
        StorePathName::from_str(name)?;
        let declared: BTreeSet<_> = declared.split_whitespace().collect();
        let supplied: BTreeSet<_> = self.outputs.keys().map(String::as_str).collect();
        if supplied.is_empty() || supplied != declared {
            eyre::bail!("publication must supply every declared output exactly once");
        }
        let mut directories = BTreeSet::new();
        for (output, directory) in &self.outputs {
            OutputName::from_str(output)?;
            if !directory.is_absolute()
                || directory
                    .components()
                    .any(|component| matches!(component, std::path::Component::ParentDir))
            {
                eyre::bail!("output directory must be absolute without parent components");
            }
            let directory = directory
                .canonicalize()
                .wrap_err("resolving publication directory")?;
            if !directory.is_dir() || directory == top || !directory.starts_with(top) {
                eyre::bail!("publication directory must be inside the private build directory");
            }
            if directories.iter().any(|other: &PathBuf| {
                directory.starts_with(other) || other.starts_with(&directory)
            }) {
                eyre::bail!("publication directories must not overlap");
            }
            directories.insert(directory);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_incomplete_and_escaping_publications() {
        let top = std::env::temp_dir().join(format!("dyndrv-publish-{}", std::process::id()));
        std::fs::create_dir_all(top.join("out")).unwrap();
        let publication = Publication {
            nix: PathBuf::new(),
            outputs: BTreeMap::from([("out".into(), top.join("out"))]),
        };
        assert!(
            publication
                .validate(&top, "out compilerState", "fixture")
                .is_err()
        );
        assert!(
            publication
                .validate(&top.join("out"), "out", "fixture")
                .is_err()
        );
        assert!(publication.validate(&top, "out", "fixture").is_ok());
        let overlapping = Publication {
            nix: PathBuf::new(),
            outputs: BTreeMap::from([
                ("out".into(), top.join("out")),
                ("nested".into(), top.join("out")),
            ]),
        };
        assert!(overlapping.validate(&top, "out nested", "fixture").is_err());
        std::fs::remove_dir_all(top).unwrap();
    }
}

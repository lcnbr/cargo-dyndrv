use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, LazyLock, OnceLock},
};

use color_eyre::eyre::{self, Context};
use harmonia_store_content_address::ContentAddressMethodAlgorithm;
use harmonia_store_derivation::{
    derivation::{Derivation, DerivationOutput},
    derived_path::{OutputName, SingleDerivedPath},
    placeholder::Placeholder,
};
use harmonia_store_path::{StoreDir, StorePath, StorePathName};
use harmonia_store_remote::DaemonStore;
use harmonia_utils_hash::Algorithm::SHA256;
use tokio::sync::Mutex;

use crate::{
    tools::{Tool, Tools},
    util::{CloneBytes, IntoBytes},
};

// TODO: won't work on all platforms, perhaps could be truly static
pub static PLATFORM: LazyLock<bytes::Bytes> =
    LazyLock::new(|| format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS).into());

pub static OUTPUT_OUT: LazyLock<OutputName> = LazyLock::new(OutputName::default);
pub static OUTPUT_FLAGS: LazyLock<OutputName> =
    LazyLock::new(|| OutputName::from_str("flags").unwrap());

pub fn is_in_derivation() -> bool {
    static VALUE: OnceLock<bool> = OnceLock::new();
    *VALUE.get_or_init(|| std::env::var_os("CARGO_DYNDRV_IN_DRV").is_some())
}

pub fn daemon_path() -> PathBuf {
    if let Ok(from_env) = std::env::var("NIX_REMOTE") {
        from_env
            .strip_prefix("unix://")
            .map(ToString::to_string)
            .unwrap_or(from_env)
            .into()
    } else {
        "/nix/var/nix/daemon-socket/socket".into()
    }
}

pub(crate) fn source_nar(real_path: &Path) -> eyre::Result<Vec<u8>> {
    // Cargo vendor directories may link the package root into another store path.
    // Import its contents, not an unreferenced symlink that is broken in the sandbox.
    // This only resolves the root; nested source symlinks keep NAR's existing behavior.
    let root = real_path
        .canonicalize()
        .wrap_err_with(|| format!("resolving package source root {}", real_path.display()))?;
    let mut encoder = nix_nar::Encoder::new(&root)?;
    let mut buf = Vec::new();
    std::io::copy(&mut encoder, &mut buf)?;
    Ok(buf)
}

/// Import the exact source image already validated by a bound graph. No path
/// reread or root-key cache is involved in this opt-in route.
pub(crate) async fn add_prepared_nar<T: DaemonStore>(
    store: &mut T,
    source_nar: &[u8],
) -> eyre::Result<StorePath> {
    Ok(store
        .add_ca_to_store(
            "bound-workspace-src",
            ContentAddressMethodAlgorithm::NixArchive(SHA256),
            &Default::default(),
            false,
            source_nar,
        )
        .await?
        .path)
}

pub async fn add_to_store_nar<T: DaemonStore>(
    store: &mut T,
    real_path: PathBuf,
    name: &str,
) -> eyre::Result<StorePath> {
    static CACHE_MUTEX: Mutex<Option<HashMap<PathBuf, StorePath>>> = Mutex::const_new(None);
    let mut guard = CACHE_MUTEX.lock().await;
    let cache = guard.get_or_insert_default();

    if let Some(store_path) = cache.get(&real_path) {
        Ok(store_path.clone())
    } else {
        // TODO: Don't put the entire nar in memory
        let buf = source_nar(&real_path)?;
        let store_path = store
            .add_ca_to_store(
                name,
                ContentAddressMethodAlgorithm::NixArchive(SHA256),
                &Default::default(),
                false,
                buf.as_slice(),
            )
            .await?
            .path;
        cache.insert(real_path, store_path.clone());
        Ok(store_path)
    }
}

pub async fn add_drv_to_store<T: DaemonStore>(
    store: &mut T,
    store_dir: &StoreDir,
    drv: Derivation,
    drv_name: &str,
) -> eyre::Result<StorePath> {
    let refs = drv.inputs.iter().map(|p| p.root_path().clone()).collect();
    let buf = harmonia_store_aterm::print_derivation_aterm(store_dir, &drv.into_full());
    Ok(store
        .add_ca_to_store(
            &format!("{}.drv", drv_name),
            ContentAddressMethodAlgorithm::Text,
            &refs,
            false,
            buf.as_slice(),
        )
        .await?
        .path)
}

pub async fn submit_wrapper<T: DaemonStore>(
    store: &mut T,
    store_dir: &StoreDir,
    ln: &Tool,
    unit_name: &str,
    base_drv: &StorePath,
) -> eyre::Result<()> {
    // We're only allowed to submit if the name is correct.
    // The easiest way to do this without breaking the inner loop is a symlink
    // If we're running in nixpkgs, we'll get a $name environment variable with the
    // derivation name. outputs should be named "$name-$output_name.drv"
    let base_name = std::env::var("name")
        .wrap_err("could not read derivation name, is this in nixpkgs stdenv?")?;

    let output_name = format!("{}.drv", unit_name);
    let drv_name = format!("{}-{}", base_name, unit_name);

    let wrapper_drv = Derivation {
        name: StorePathName::from_str(&drv_name)?,
        outputs: BTreeMap::from([(
            OUTPUT_OUT.clone(),
            DerivationOutput::CAFloating(ContentAddressMethodAlgorithm::NixArchive(SHA256)),
        )]),
        inputs: BTreeSet::from([
            SingleDerivedPath::Built {
                drv_path: Arc::new(SingleDerivedPath::Opaque(base_drv.clone())),
                output: OUTPUT_OUT.clone(),
            },
            SingleDerivedPath::Opaque(ln.store_path.clone()),
        ]),
        platform: PLATFORM.clone(),
        builder: ln.real_path.clone_bytes(),
        args: Vec::from([
            "-s".into(),
            Placeholder::ca_output(base_drv, &OUTPUT_OUT)
                .render()
                .into_bytes(),
            Placeholder::standard_output(&OUTPUT_OUT)
                .render()
                .into_bytes(),
        ]),

        env: BTreeMap::new(),
        structured_attrs: None,
    };

    let wrapper_drv_path = add_drv_to_store(store, store_dir, wrapper_drv, &drv_name).await?;

    store
        .submit_output(
            &SingleDerivedPath::Opaque(wrapper_drv_path),
            &OutputName::from_str(&output_name)?,
        )
        .await?;

    Ok(())
}

pub async fn target_env_drv<T: DaemonStore>(
    store: &mut T,
    store_dir: &StoreDir,
    tools: &Tools,
    target: &Option<String>,
) -> eyre::Result<StorePath> {
    static CACHE_MUTEX: Mutex<Option<HashMap<Option<String>, StorePath>>> = Mutex::const_new(None);
    let mut guard = CACHE_MUTEX.lock().await;
    let cache = guard.get_or_insert_default();
    if let Some(cached) = cache.get(target) {
        return Ok(cached.clone());
    }

    let name = StorePathName::from_str(&format!(
        "cargo-cfg-{}",
        target.as_deref().unwrap_or("host")
    ))?;
    let drv = Derivation {
        name: name.clone(),
        outputs: BTreeMap::from([(
            OUTPUT_OUT.clone(),
            DerivationOutput::CAFloating(ContentAddressMethodAlgorithm::Flat(SHA256)),
        )]),
        inputs: BTreeSet::from([
            SingleDerivedPath::Opaque(tools.rustc.store_path.clone()),
            SingleDerivedPath::Opaque(tools.target_env.store_path.clone()),
        ]),
        platform: PLATFORM.clone(),
        builder: tools.target_env.real_path.clone_bytes(),
        args: Vec::from([
            tools.rustc.real_path.clone_bytes(),
            target.clone().unwrap_or_default().into(),
            Placeholder::standard_output(&OUTPUT_OUT)
                .render()
                .clone_bytes(),
        ]),
        env: BTreeMap::new(),
        structured_attrs: None,
    };

    add_drv_to_store(store, store_dir, drv, &name).await
}

#[cfg(test)]
mod source_tests {
    use super::source_nar;
    use std::{fs, os::unix::fs::symlink, time::SystemTime};

    #[test]
    fn vendor_root_symlink_imports_package_contents() {
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "cargo-dyndrv-source-{}-{nonce}",
            std::process::id()
        ));
        let package = root.join("package");
        fs::create_dir_all(&package).unwrap();
        fs::write(package.join("build.rs"), b"fn main() {}\n").unwrap();
        let vendor = root.join("vendor-package");
        symlink(&package, &vendor).unwrap();

        let direct = source_nar(&package).unwrap();
        assert_eq!(source_nar(&vendor).unwrap(), direct);
        fs::write(
            package.join("build.rs"),
            b"fn main() { println!(\"changed\"); }\n",
        )
        .unwrap();
        assert_ne!(source_nar(&vendor).unwrap(), direct);
        fs::remove_file(&vendor).unwrap();
        symlink(root.join("missing"), &vendor).unwrap();
        assert!(source_nar(&vendor).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}

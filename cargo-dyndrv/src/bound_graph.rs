//! Hash-bound, deliberately restricted graph-v2 execution inputs. Validation is
//! pure filesystem work and finishes before any daemon connection or store write.
use crate::{
    package_metadata::Metadata,
    plan::{self, CompilerPlan},
    source::{PackageSource, WorkspaceSource},
    store,
    tools::{Tool, Tools},
    unit_graph::UnitGraph,
};
use color_eyre::eyre::{self, bail};
use harmonia_store_path::StoreDir;
use harmonia_utils_hash::{Algorithm::SHA256, HashFormat};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct BoundFile {
    path: PathBuf,
    sha256: String,
}
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct BoundTool {
    executable: BoundFile,
    version_output: BoundFile,
}
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Invocation {
    argv: Vec<String>,
    cwd: PathBuf,
    environment: BTreeMap<String, String>,
}
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Source {
    root: PathBuf,
    nar_sha256: String,
}
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    version: u32,
    graph: BoundFile,
    metadata: BoundFile,
    cargo: BoundTool,
    rustc: BoundTool,
    source: Source,
    host: String,
    source_layout: String,
    graph_invocation: Invocation,
    metadata_invocation: Invocation,
    builder_tools: BTreeMap<String, BoundFile>,
    diagnostic_rustc_probe: Option<BoundFile>,
}

fn sha256(bytes: &[u8]) -> String {
    SHA256.digest(bytes).as_base16().as_bare().to_string()
}
fn check_digest(actual: &str, expected: &str) -> eyre::Result<()> {
    if expected.len() != 64
        || !expected
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        || actual != expected
    {
        bail!("bound input SHA256 mismatch or invalid hash");
    }
    Ok(())
}
fn read(bound: &BoundFile) -> eyre::Result<Vec<u8>> {
    if !bound.path.is_absolute() {
        bail!("bound file path must be absolute");
    }
    let bytes = std::fs::read(&bound.path)?;
    check_digest(&sha256(&bytes), &bound.sha256)?;
    Ok(bytes)
}

const TOOL_KEYS: [&str; 12] = [
    "rustc",
    "cargo",
    "cc",
    "host-cc",
    "cxx",
    "host-cxx",
    "env-wrap",
    "build-wrap",
    "compile-wrap",
    "target-env",
    "ln",
    "source-wrap",
];
fn tools(input: &Input) -> eyre::Result<(Tools, Tool, Option<Tool>)> {
    if input
        .builder_tools
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>()
        != TOOL_KEYS.into_iter().collect()
    {
        bail!("builder_tools requires exactly the documented twelve keys");
    }
    let store = StoreDir::default();
    let mut bound = BTreeMap::new();
    for (key, file) in &input.builder_tools {
        read(file)?;
        bound.insert(key.as_str(), Tool::bound(&store, &file.path)?);
    }
    let compiler = &input.builder_tools["rustc"];
    if compiler.path != input.rustc.executable.path
        || compiler.sha256 != input.rustc.executable.sha256
    {
        bail!("actual builder compiler differs from declared producer rustc");
    }
    let probe = input
        .diagnostic_rustc_probe
        .as_ref()
        .map(|file| {
            read(file)?;
            Tool::bound(&store, &file.path)
        })
        .transpose()?;
    let get = |name| bound[name].clone();
    Ok((
        Tools {
            rustc: get("rustc"),
            cargo: get("cargo"),
            target_cc: get("cc"),
            host_cc: get("host-cc"),
            target_cxx: get("cxx"),
            host_cxx: get("host-cxx"),
            env_wrap: get("env-wrap"),
            build_wrap: get("build-wrap"),
            compile_wrap: get("compile-wrap"),
            target_env: get("target-env"),
            ln: get("ln"),
        },
        get("source-wrap"),
        probe,
    ))
}

fn invocation(inv: &Invocation, input: &Input, graph: bool) -> eyre::Result<()> {
    if inv.cwd.canonicalize()? != input.source.root.canonicalize()?
        || inv.argv.first().map(Path::new) != Some(input.cargo.executable.path.as_path())
        || !inv.argv.iter().any(|a| a == "--offline")
        || !inv.argv.iter().any(|a| a == "--locked")
        || (graph && !inv.argv.iter().any(|a| a == "--unit-graph"))
        || (!graph && inv.argv.get(1).map(String::as_str) != Some("metadata"))
        || inv.environment.get("RUSTC").map(Path::new)
            != Some(input.rustc.executable.path.as_path())
    {
        bail!("declared Cargo invocation does not match bound source/tools/offline operation");
    }
    const ALLOWED: &[&str] = &[
        "PATH",
        "HOME",
        "RUSTC",
        "RUSTDOC",
        "CARGO_HOME",
        "CARGO_TARGET_DIR",
        "CARGO_BUILD_BUILD_DIR",
        "CARGO_INCREMENTAL",
        "CARGO_BUILD_INCREMENTAL",
        "CARGO_BUILD_JOBS",
        "NIX_BUILD_CORES",
        "RUSTC_BOOTSTRAP",
        "CARGO_NET_OFFLINE",
        "LD_LIBRARY_PATH",
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_PROFILE_DEV_CODEGEN_UNITS",
        "CARGO_PROFILE_DEV_BUILD_OVERRIDE_CODEGEN_UNITS",
    ];
    if inv
        .environment
        .keys()
        .any(|key| !ALLOWED.contains(&key.as_str()))
    {
        bail!("Cargo provenance contains an unsupported environment key");
    }
    if inv.environment.get("CARGO_INCREMENTAL").map(String::as_str) != Some("0") {
        bail!("bound prototype requires CARGO_INCREMENTAL=0");
    }
    Ok(())
}

/// Only load() may construct this type. Its accepted source bytes are consumed
/// once by the import owner, not reread from a mutable source path.
pub(crate) struct ValidatedBoundGraph {
    parts: Parts,
    receipt: Value,
}
pub(crate) struct Parts {
    pub graph: UnitGraph,
    pub metadata: Metadata,
    pub plans: Vec<CompilerPlan>,
    pub workspace: WorkspaceSource,
    pub package_sources: Vec<Option<PackageSource>>,
    pub source_nar: Vec<u8>,
    pub tools: Tools,
    pub source_wrapper: Tool,
    pub probe: Option<Tool>,
}
impl ValidatedBoundGraph {
    pub(crate) fn receipt(&self) -> &Value {
        &self.receipt
    }
    pub(crate) fn into_parts(self) -> Parts {
        self.parts
    }
}

pub(crate) fn load(path: &Path) -> eyre::Result<ValidatedBoundGraph> {
    if store::is_in_derivation()
        || std::env::var_os("EXTERN_PATH").is_some()
        || Path::new("extern.json").exists()
        || std::env::var_os("CARGO_DYNDRV_WORKSPACE_SOURCE").is_some()
    {
        bail!(
            "bound prototype rejects derivation execution and implicit extern/source-layout overrides"
        );
    }
    let manifest = std::fs::read(path)?;
    let input: Input = serde_json::from_slice(&manifest)?;
    if input.version != 1
        || input.source_layout != "workspace-readonly"
        || input.host != "x86_64-unknown-linux-gnu"
    {
        bail!("unsupported bound manifest version/layout/host");
    }
    read(&input.cargo.executable)?;
    read(&input.cargo.version_output)?;
    read(&input.rustc.executable)?;
    read(&input.rustc.version_output)?;
    invocation(&input.graph_invocation, &input, true)?;
    invocation(&input.metadata_invocation, &input, false)?;
    let (graph, plans) =
        plan::execution_plans(serde_json::from_slice(&read(&input.graph)?)?, &input.host)?;
    let metadata = Metadata::from_slice(&read(&input.metadata)?)?;
    let workspace = WorkspaceSource::new(
        &input.source.root,
        metadata.workspace_root.as_std_path(),
        metadata.target_directory.as_std_path(),
    )?;
    let packages: BTreeMap<_, _> = metadata
        .packages
        .iter()
        .map(|record| (record.package.id.repr.as_str(), &record.package))
        .collect();
    if packages.len() != metadata.packages.len() {
        bail!("duplicate metadata package IDs");
    }
    let mut package_sources = Vec::with_capacity(graph.units.len());
    for unit in &graph.units {
        let package = packages
            .get(unit.pkg_id.as_str())
            .ok_or_else(|| eyre::eyre!("graph package absent from metadata"))?;
        if package.source.is_some()
            || !metadata.workspace_members.contains(&package.id)
            || package.targets.iter().any(|t| t.is_custom_build())
        {
            bail!("selected metadata package is nonlocal or declares a build script");
        }
        package_sources.push(Some(
            workspace.package(package.manifest_path.as_std_path(), &unit.target.src_path)?,
        ));
        let found = package.targets.iter().any(|target| {
            let target = serde_json::to_value(target).expect("serializable metadata target");
            target["name"] == unit.target.name
                && target["src_path"] == unit.target.src_path.to_string_lossy().as_ref()
                && target["kind"] == json!(unit.target.kind)
                && target["crate_types"] == json!(unit.target.crate_types)
                && target["edition"] == unit.target.edition
        });
        if !found {
            bail!("graph target does not match bound package metadata");
        }
    }
    let (tools, source_wrapper, probe) = tools(&input)?;
    let source_nar = workspace.nar()?;
    check_digest(&sha256(&source_nar), &input.source.nar_sha256)?;
    let receipt = json!({"schemaVersion":1,"manifestSha256":sha256(&manifest),"graph":input.graph,"metadata":input.metadata,"sourceRoot":workspace.root,"sourceNarSha256":input.source.nar_sha256,"sourceNarBytes":source_nar.len(),"units":graph.units.len(),"roots":graph.roots,"builderTools":input.builder_tools,"producerCargo":input.cargo.executable,"producerRustc":input.rustc.executable,"diagnosticRustcProbe":input.diagnostic_rustc_probe,"sourceLayout":"workspace-readonly","scope":"Restricted local no-build-script Linux Build graph; no incremental state or full Cargo replay","attestationLimit":"Hashes bind the caller's reviewed producer attestation; they do not prove arbitrary JSON was generated by Cargo."});
    Ok(ValidatedBoundGraph {
        parts: Parts {
            graph,
            metadata,
            plans,
            workspace,
            package_sources,
            source_nar,
            tools,
            source_wrapper,
            probe,
        },
        receipt,
    })
}

pub(crate) fn socket(path: &Path) -> eyre::Result<PathBuf> {
    if !path.is_absolute() {
        bail!("explicit private daemon socket must be absolute");
    }
    let info = path.symlink_metadata()?;
    let parent = path
        .parent()
        .ok_or_else(|| eyre::eyre!("socket has no parent"))?
        .canonicalize()?;
    let owner = parent.metadata()?;
    if !info.file_type().is_socket()
        || owner.permissions().mode() & 0o777 != 0o700
        || owner.uid() != info.uid()
    {
        bail!("daemon socket must have a same-owner private0700 parent");
    }
    let canonical = path.canonicalize()?;
    if canonical == Path::new("/nix/var/nix/daemon-socket/socket") {
        bail!("default daemon forbidden for bound prototype");
    }
    Ok(canonical)
}

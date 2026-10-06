mod native_publish;
mod stock_cargo;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    fmt::Display,
    hash::{Hash, Hasher},
    str::FromStr,
    sync::Arc,
};

use color_eyre::eyre::{self, OptionExt as _, WrapErr as _};
use harmonia_store_content_address::ContentAddressMethodAlgorithm;
use harmonia_store_derivation::{
    derivation::{Derivation, DerivationOutput},
    derived_path::SingleDerivedPath,
    placeholder::Placeholder,
};
use harmonia_store_path::{FromStoreDirStr, StoreDir, StorePath, StorePathName};
use harmonia_store_remote::HandshakeDaemonStore as _;
use harmonia_utils_hash::Algorithm::SHA256;

use crate::{
    cargo_command::CargoInvocation,
    store::{OUTPUT_FLAGS, OUTPUT_OUT, PLATFORM},
    unit_graph::{CompileMode, Unit},
    util::{CloneBytes as _, IntoBytes as _},
};

mod bound_graph;
mod cargo_command;
mod package_metadata;
mod plan;
mod source;
mod store;
mod tools;
mod unit_graph;
mod util;

const SCRIPT_IMMEDIATE_ARGS: &str = "args-immediate";
const SCRIPT_TRANSITIVE_ARGS: &str = "args-transitive";
const SCRIPT_IMMEDIATE_ENV: &str = "env";
const SCRIPT_METADATA_ENV: &str = "metadata";

#[derive(Debug, Clone, Default)]
struct UnitCacheMeta {
    pub artifacts: Vec<plan::Artifact>,
    pub custom_output: bool,
    pub has_links: bool,
}
#[derive(Debug, Clone)]
struct UnitCache {
    pub drv_path: StorePath,
    pub meta: UnitCacheMeta,
}

#[derive(Debug, serde::Deserialize)]
struct ExternConfig {
    #[serde(default)]
    pub inputs: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub path: Vec<String>,
}

fn order_units(
    all_transitive_deps: &mut HashMap<usize, BTreeSet<usize>>,
    ordered_units: &mut Vec<usize>,
    all_units: &[Unit],
    idx: usize,
) {
    if all_transitive_deps.contains_key(&idx) {
        return;
    }

    let mut transitive_deps = BTreeSet::new();
    let unit = &all_units[idx];
    for dep in &unit.dependencies {
        order_units(all_transitive_deps, ordered_units, all_units, dep.index);
        // Must not attempt to link dependencies of a build.rs
        transitive_deps.append(&mut all_transitive_deps[&dep.index].clone());
        transitive_deps.insert(dep.index);
    }

    all_transitive_deps.insert(idx, transitive_deps);

    ordered_units.push(idx);
}

fn add_long<T: Display>(args: &mut VecDeque<bytes::Bytes>, option: &str, value: &T) {
    args.push_back(format!("--{}={}", option, value).into());
}

fn add_codegen<T: Display>(args: &mut VecDeque<bytes::Bytes>, option: &str, value: &T) {
    args.push_back("-C".into());
    args.push_back(format!("{}={}", option, value).into());
}

fn add_metadata_env(
    env: &mut BTreeMap<bytes::Bytes, bytes::Bytes>,
    record: &package_metadata::PackageRecord,
) {
    let meta = &record.package;
    env.insert("CARGO_PKG_VERSION".into(), meta.version.to_string().into());
    env.insert(
        "CARGO_PKG_VERSION_MAJOR".into(),
        meta.version.major.to_string().into(),
    );
    env.insert(
        "CARGO_PKG_VERSION_MINOR".into(),
        meta.version.minor.to_string().into(),
    );
    env.insert(
        "CARGO_PKG_VERSION_PATCH".into(),
        meta.version.patch.to_string().into(),
    );
    env.insert(
        "CARGO_PKG_VERSION_PRE".into(),
        meta.version.pre.as_str().to_owned().into(),
    );

    env.insert("CARGO_PKG_NAME".into(), meta.name.to_string().into());
    env.insert("CARGO_PKG_AUTHORS".into(), meta.authors.join(":").into());
    for (key, value) in [
        ("CARGO_PKG_DESCRIPTION", meta.description.as_deref()),
        ("CARGO_PKG_HOMEPAGE", meta.homepage.as_deref()),
        ("CARGO_PKG_REPOSITORY", meta.repository.as_deref()),
        ("CARGO_PKG_LICENSE", meta.license.as_deref()),
        (
            "CARGO_PKG_LICENSE_FILE",
            meta.license_file.as_ref().map(|p| p.as_str()),
        ),
        ("CARGO_PKG_README", meta.readme.as_ref().map(|p| p.as_str())),
    ] {
        env.insert(key.into(), value.unwrap_or_default().to_owned().into());
    }
    env.insert(
        "CARGO_PKG_RUST_VERSION".into(),
        record.rust_version_env().to_owned().into(),
    );
}

fn add_build_script_feature_env(
    env: &mut BTreeMap<bytes::Bytes, bytes::Bytes>,
    features: &[String],
) {
    for feature in features {
        let feature_name = feature.to_uppercase().replace('-', "_");
        // Cargo exposes every enabled build-script feature with the value "1".
        env.insert(format!("CARGO_FEATURE_{}", feature_name).into(), "1".into());
    }
}

fn declared_check_cfg_args(features: &BTreeMap<String, Vec<String>>) -> Vec<String> {
    // Cargo checks every declared feature, including disabled and implicit ones.
    // Package metadata supplies this complete, sorted map; unit.features does not.
    let values = features
        .keys()
        .map(|feature| format!("{feature:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    vec![
        "--check-cfg".into(),
        "cfg(docsrs,test)".into(),
        "--check-cfg".into(),
        format!("cfg(feature, values({values}))"),
    ]
}

fn main() -> eyre::Result<()> {
    color_eyre::install()?;
    let native_args = std::env::args_os().collect::<Vec<_>>();
    if let Some(result) = stock_cargo::dispatch(&native_args) {
        return result;
    }
    let sys_args: Vec<_> = std::env::args().collect();
    if sys_args.get(1).is_some_and(|arg| arg == "publish-native") {
        return native_publish::Publication::cli(&sys_args[2..]);
    }
    // Planning must work with no Nix daemon, installed wrappers or source imports.
    if sys_args.get(1).is_some_and(|arg| arg == "plan") {
        return plan::cli(&sys_args[2..]);
    }
    if sys_args
        .get(1)
        .is_some_and(|arg| arg == "plan-generated-layout")
    {
        return plan::generated_cli(&sys_args[2..]);
    }
    match sys_args.get(1).map(String::as_str) {
        Some("inspect-bound-graph") if sys_args.len() == 4 && sys_args[2] == "--input" => {
            let bound = bound_graph::load(std::path::Path::new(&sys_args[3]))?;
            println!("{}", serde_json::to_string_pretty(bound.receipt())?);
            Ok(())
        }
        Some("execute-bound-graph")
            if sys_args.len() == 6
                && sys_args[2] == "--input"
                && sys_args[4] == "--daemon-socket" =>
        {
            // All input checks occur before async setup or daemon connection.
            let bound = bound_graph::load(std::path::Path::new(&sys_args[3]))?;
            let socket = bound_graph::socket(std::path::Path::new(&sys_args[5]))?;
            run(sys_args, Some(bound), Some(socket))
        }
        Some("inspect-bound-graph" | "execute-bound-graph") => eyre::bail!(
            "usage: inspect-bound-graph --input FILE | execute-bound-graph --input FILE --daemon-socket PRIVATE_SOCKET"
        ),
        _ => run(sys_args, None, None),
    }
}

#[tokio::main]
async fn run(
    sys_args: Vec<String>,
    bound: Option<bound_graph::ValidatedBoundGraph>,
    explicit_socket: Option<std::path::PathBuf>,
) -> eyre::Result<()> {
    let bound_mode = bound.is_some();
    // Someone might want a different one for some reason, idk how to get it
    let store_dir = StoreDir::default();
    let mut store = harmonia_store_remote::DaemonClientBuilder::new()
        .set_store_dir(&store_dir)
        .build_unix(explicit_socket.unwrap_or_else(store::daemon_path))
        .await?
        .handshake()
        .await?;

    // TODO: accept this via some argument

    let all_extern_config: BTreeMap<String, ExternConfig> = if bound_mode {
        BTreeMap::new()
    } else {
        let extern_path = std::env::var("EXTERN_PATH").unwrap_or("extern.json".to_string());

        if let Ok(content) = std::fs::read_to_string(extern_path) {
            serde_json::from_str(&content).context("Could not parse extern configuration")?
        } else {
            Default::default()
        }
    };

    let (
        tools,
        unit_graph,
        metadata,
        workspace_source,
        source_wrapper,
        bound_plans,
        bound_nar,
        diagnostic_probe,
        bound_package_sources,
    ) = if let Some(bound) = bound {
        let bound = bound.into_parts();
        (
            bound.tools,
            bound.graph,
            bound.metadata,
            Some(bound.workspace),
            Some(bound.source_wrapper),
            Some(bound.plans),
            Some(bound.source_nar),
            bound.probe,
            Some(bound.package_sources),
        )
    } else {
        let tools = tools::Tools::find(&store_dir)?;
        // Shelling out to `cargo` since the cargo crate does not provide what we need
        let cargo = CargoInvocation::new(&tools.cargo.real_path, &sys_args[1..]);
        let unit_graph = crate::unit_graph::discover(&cargo)?;

        // The unit graph doesn't give us everything we want,
        // but the remainder can come from the packages in `cargo metadata`
        // The package ids match the unit graph, so we can index on them
        let metadata = cargo.metadata()?;
        let workspace_source = std::env::var_os("CARGO_DYNDRV_WORKSPACE_SOURCE")
            .map(|root| {
                source::WorkspaceSource::new(
                    std::path::Path::new(&root),
                    metadata.workspace_root.as_std_path(),
                    metadata.target_directory.as_std_path(),
                )
            })
            .transpose()?;
        // Default mode does not require the optional source wrapper.
        let source_wrapper = workspace_source
            .as_ref()
            .map(|_| tools::Tool::find(&store_dir, "source-wrap", option_env!("SOURCE_WRAP")))
            .transpose()?;
        (
            tools,
            unit_graph,
            metadata,
            workspace_source,
            source_wrapper,
            None,
            None,
            None,
            None,
        )
    };
    let package_metadata: HashMap<_, _> = metadata
        .packages
        .into_iter()
        .map(|record| (record.package.id.repr.clone(), record))
        .collect();
    // Bound mappings were validated with the captured source image. Do not
    // resolve the mutable source filesystem again after daemon connection.
    let package_sources: Vec<_> = if let Some(sources) = bound_package_sources {
        sources
    } else {
        unit_graph
            .units
            .iter()
            .map(|unit| {
                let package = &package_metadata[&unit.pkg_id].package;
                match (&workspace_source, &package.source) {
                    (Some(layout), None) => layout
                        .package(
                            package.manifest_path.as_std_path(),
                            unit.target.src_path.as_path(),
                        )
                        .map(Some),
                    _ => Ok(None),
                }
            })
            .collect::<eyre::Result<_>>()?
    };

    if unit_graph.version != if bound_mode { 2 } else { 1 } {
        eyre::bail!("Unsupported unit graph version {}", unit_graph.version);
    }

    // TODO: parse Cargo.toml and make registry to cache packages in store.
    // Would be useful to map package IDs to store paths persistently
    // TODO: improve sorting
    let (ordered_units, transitive_deps) = {
        let mut transitive_deps = HashMap::with_capacity(unit_graph.units.len());
        let mut units = Vec::with_capacity(unit_graph.units.len());
        for root in &unit_graph.roots {
            order_units(&mut transitive_deps, &mut units, &unit_graph.units, *root);
        }
        (units, transitive_deps)
    };

    // Reject unsupported compiler semantics before importing any package sources.
    // This backend currently targets Linux; host/target naming is checked by the
    // shared planner. Test harness settings need manifest plumbing before tests
    // may enter this backend.
    let compiler_plans: Vec<_> = if let Some(plans) = bound_plans {
        plans.into_iter().map(Some).collect()
    } else {
        unit_graph
            .units
            .iter()
            .map(|unit| {
                if unit.mode == CompileMode::RunCustomBuild {
                    Ok(None)
                } else {
                    plan::compiler_plan(unit, "x86_64-unknown-linux-gnu", None).map(Some)
                }
            })
            .collect::<eyre::Result<_>>()?
    };

    let workspace_store_source = if let Some(nar) = bound_nar {
        Some(store::add_prepared_nar(&mut store, &nar).await?)
    } else if package_sources.iter().any(Option::is_some) {
        Some(store::add_prepared_nar(&mut store, &workspace_source.as_ref().unwrap().nar()?).await?)
    } else {
        None
    };

    let base_env = tools.base_environment();

    // TODO: find some way of caching this on disk for interactive builds
    let mut drv_cache: Vec<Option<UnitCache>> = vec![None; unit_graph.units.len()];

    for unit_idx in ordered_units {
        // TODO: wrap rustc so we can get additional args from
        // build.rs outputs
        let unit = &unit_graph.units[unit_idx];
        let package_record = &package_metadata[&unit.pkg_id];
        let unit_meta = &package_record.package;
        let crate_root = unit_meta
            .manifest_path
            .parent()
            .ok_or_eyre("unit does not have a source path")?;

        let drv_name = crate_root.file_name().ok_or_eyre("empty path")?;

        let package_source = &package_sources[unit_idx];
        let src_path = if package_source.is_some() {
            workspace_store_source.as_ref().unwrap().clone()
        } else {
            store::add_to_store_nar(
                &mut store,
                crate_root.to_path_buf().into(),
                &format!("{}-src", drv_name),
            )
            .await?
        };

        let mut path: bytes::BytesMut = tools.rustc.path_entry().into();

        let mut env = base_env.clone();
        // TODO: more cargo env vars not from cargo metadata
        add_metadata_env(&mut env, package_record);

        let manifest_directory = if let Some(package) = package_source {
            env.insert(
                "CARGO_MANIFEST_PATH".into(),
                std::path::Path::new(source::LOGICAL_ROOT)
                    .join(&package.manifest)
                    .into_bytes(),
            );
            std::path::Path::new(source::LOGICAL_ROOT).join(&package.directory)
        } else {
            src_path.to_absolute_path(&store_dir)
        };
        env.insert(
            "CARGO_MANIFEST_DIR".into(),
            manifest_directory.clone().into_bytes(),
        );

        let mut inputs = BTreeSet::from([
            SingleDerivedPath::Opaque(tools.rustc.store_path.clone()),
            SingleDerivedPath::Opaque(src_path.clone()),
        ]);

        let (mut drv, meta) = if unit.mode == CompileMode::RunCustomBuild {
            // running build scripts always get HOST_CC/HOST_CXX, but if the target is local
            // (e.g. a build script for a dependency of a build script) its CC will be HOST_CC,
            // not the target CC
            inputs.insert(SingleDerivedPath::Opaque(tools.host_cc.store_path.clone()));
            inputs.insert(SingleDerivedPath::Opaque(tools.host_cxx.store_path.clone()));
            env.insert("HOST_CC".into(), tools.host_cc.real_path.clone_bytes());
            env.insert("HOST_CXX".into(), tools.host_cxx.real_path.clone_bytes());

            // some build scripts won't work if it can't find the CC in PATH
            path.extend_from_slice(b":");
            path.extend_from_slice(tools.host_cc.path_entry());

            // unit graph uses an empty platform to mean native
            if unit.platform.is_some() {
                inputs.insert(SingleDerivedPath::Opaque(
                    tools.target_cc.store_path.clone(),
                ));
                inputs.insert(SingleDerivedPath::Opaque(
                    tools.target_cxx.store_path.clone(),
                ));
                env.insert("CC".into(), tools.target_cc.real_path.clone_bytes());
                env.insert("CXX".into(), tools.target_cxx.real_path.clone_bytes());
                path.extend_from_slice(b":");
                path.extend_from_slice(tools.target_cc.path_entry());
            } else {
                env.insert("CC".into(), tools.host_cc.real_path.clone_bytes());
                env.insert("CXX".into(), tools.host_cxx.real_path.clone_bytes());
            }

            // TODO cfg flags set by dependencies, perhaps it could go through the same
            // path as metadata
            inputs.insert(SingleDerivedPath::Opaque(
                tools.build_wrap.store_path.clone(),
            ));
            inputs.insert(SingleDerivedPath::Opaque(tools.env_wrap.store_path.clone()));

            let mut args = Vec::new();

            // args that need rustc to calculate (e.g. CARGO_CFG_TARGET_OS, HOST, TARGET)
            {
                let target_env_drv =
                    store::target_env_drv(&mut store, &store_dir, &tools, &unit.platform).await?;
                args.push(
                    Placeholder::ca_output(&target_env_drv, &OUTPUT_OUT)
                        .render()
                        .into_bytes(),
                );
                inputs.insert(SingleDerivedPath::Built {
                    drv_path: Arc::new(SingleDerivedPath::Opaque(target_env_drv)),
                    output: OUTPUT_OUT.clone(),
                });
            }

            let mut executable_dep = None;
            // cargo gives us a few dependencies.
            // one has the actual built executable, the others are build script
            // executions of dependencies that set metadata
            for dep in &unit.dependencies {
                let dep_cache = drv_cache[dep.index].as_ref().expect("units out of order");
                let dep_unit = &unit_graph.units[dep.index];
                if dep_unit.mode == CompileMode::Build {
                    executable_dep = Some(dep);
                } else if dep_cache.meta.has_links {
                    inputs.insert(SingleDerivedPath::Built {
                        drv_path: Arc::new(SingleDerivedPath::Opaque(dep_cache.drv_path.clone())),
                        output: OUTPUT_FLAGS.clone(),
                    });
                    let flags = Placeholder::ca_output(&dep_cache.drv_path, &OUTPUT_FLAGS);
                    args.push(flags.render().join(SCRIPT_METADATA_ENV).into_bytes());
                }
            }

            let Some(executable_dep) = executable_dep else {
                eyre::bail!("build script execution did not specify what to run");
            };

            let executable_cache = drv_cache[executable_dep.index]
                .as_ref()
                .expect("units out of order");
            inputs.insert(SingleDerivedPath::Built {
                drv_path: Arc::new(SingleDerivedPath::Opaque(executable_cache.drv_path.clone())),
                output: OUTPUT_OUT.clone(),
            });

            // terminate env-wrap args
            args.push("--".into());
            // build-wrap executable (running inside env-wrap)
            args.push(tools.build_wrap.real_path.clone_bytes());
            // cwd (directory for build.rs exeuction)
            args.push(manifest_directory.into_bytes());
            // links_key
            args.push(unit_meta.links.clone().unwrap_or_default().into());
            // flags_dir
            args.push(Placeholder::standard_output(&OUTPUT_FLAGS).into_bytes());
            // out_dir
            args.push(Placeholder::standard_output(&OUTPUT_OUT).into_bytes());
            // script
            args.push({
                let mut script =
                    Placeholder::ca_output(&executable_cache.drv_path, &OUTPUT_OUT).render();
                script.push(&executable_dep.extern_crate_name);
                script.into_bytes()
            });

            if let Some(extern_config) = all_extern_config.get(&unit.pkg_id) {
                eprintln!("Handling external config for {}", unit.pkg_id);
                for extra_input in &extern_config.inputs {
                    let store_path = StorePath::from_store_dir_str(&store_dir, extra_input)
                        .wrap_err("Invalid store path in extra")?;
                    inputs.insert(SingleDerivedPath::Opaque(store_path));
                }

                for (var, value) in &extern_config.env {
                    env.insert(var.clone().into(), value.clone().into());
                }

                for item in &extern_config.path {
                    path.extend_from_slice(b":");
                    path.extend_from_slice(item.as_bytes());
                }
            }

            add_build_script_feature_env(&mut env, &unit.features);

            env.insert("OPT_LEVEL".into(), unit.profile.opt_level.clone().into());
            env.insert("PATH".into(), path.into());
            (
                Derivation {
                    name: StorePathName::from_str(&format!("{}-run", drv_name))
                        .wrap_err("invalid derivation name")?,
                    outputs: BTreeMap::from([
                        (
                            OUTPUT_OUT.clone(),
                            DerivationOutput::CAFloating(
                                ContentAddressMethodAlgorithm::NixArchive(SHA256),
                            ),
                        ),
                        (
                            OUTPUT_FLAGS.clone(),
                            DerivationOutput::CAFloating(
                                ContentAddressMethodAlgorithm::NixArchive(SHA256),
                            ),
                        ),
                    ]),
                    inputs,
                    platform: PLATFORM.clone(),
                    builder: tools.env_wrap.real_path.clone_bytes(),
                    args,
                    // TODO: cargo build script environment variables
                    // (https://doc.rust-lang.org/cargo/reference/environment-variables.html#environment-variables-cargo-sets-for-build-scripts)
                    env,
                    structured_attrs: None,
                },
                UnitCacheMeta {
                    custom_output: true,
                    has_links: unit_meta.links.is_some(),
                    ..Default::default()
                },
            )
        } else {
            let compiler_plan = compiler_plans[unit_idx]
                .as_ref()
                .expect("compiler plan missing");

            // nix derivation args start at argv[1], but we put rustc in here anyway.
            // It's easier to add the wrapper if needed
            let mut args = VecDeque::from([tools.rustc.real_path.clone_bytes()]);
            if let Some(probe) = &diagnostic_probe {
                // The real compiler remains the declared/bound tool. A separate
                // diagnostic layer receives that exact path as argv[1].
                inputs.insert(SingleDerivedPath::Opaque(probe.store_path.clone()));
                args.push_front(probe.real_path.clone_bytes());
            }
            {
                // rustc handles finding other source files for us
                let input = if let Some(package) = package_source {
                    package.compiler_input.clone()
                } else {
                    let relative = unit
                        .target
                        .src_path
                        .strip_prefix(crate_root)
                        .wrap_err("internal: crate main is not in crate root???")?;
                    src_path.to_absolute_path(&store_dir).join(relative)
                };
                args.push_back(input.into_bytes())
            }

            add_long(&mut args, "out-dir", &compiler_plan.output_directory());

            args.extend(compiler_plan.rustc_args.iter().cloned().map(Into::into));

            args.extend(
                declared_check_cfg_args(&unit_meta.features)
                    .into_iter()
                    .map(Into::into),
            );

            if !bound_mode {
                // TODO: Cargo-configured rustflags
                // TODO: improve hash calculation
                let dep_hash = {
                    let mut hasher = std::hash::DefaultHasher::new();
                    for direct_dep in &unit.dependencies {
                        let dep = drv_cache[direct_dep.index].as_ref().unwrap();
                        Hash::hash(&dep.drv_path, &mut hasher);
                    }
                    unit.hash(&mut hasher);

                    hasher.finish()
                };
                // Notice that there is metadata but no extra-filename.
                // Since every crate goes in its own derivation with its own output directory,
                // there is no chance filenames will ever conflict.
                // Symbol names, on the other hand, are linked into the same binary,
                // so metadata is required to prevent linking conflicts.
                add_codegen(&mut args, "metadata", &format_args!("{:016x}", dep_hash));
            }
            for transitive_dep in &transitive_deps[&unit_idx] {
                let dep = drv_cache[*transitive_dep].as_ref().unwrap();
                // Needed for either library deps or OUT_PATH
                inputs.insert(SingleDerivedPath::Built {
                    drv_path: Arc::new(SingleDerivedPath::Opaque(dep.drv_path.clone())),
                    output: OUTPUT_OUT.clone(),
                });

                if dep.meta.custom_output {
                    inputs.insert(SingleDerivedPath::Built {
                        drv_path: Arc::new(SingleDerivedPath::Opaque(dep.drv_path.clone())),
                        output: OUTPUT_FLAGS.clone(),
                    });

                    let flags = Placeholder::ca_output(&dep.drv_path, &OUTPUT_FLAGS).render();

                    args.push_back(
                        format!("@{}", flags.join(SCRIPT_TRANSITIVE_ARGS).display()).into(),
                    );
                } else {
                    let placeholder = Placeholder::ca_output(&dep.drv_path, &OUTPUT_OUT).render();
                    args.push_back("-L".into());
                    args.push_back(format!("dependency={}", placeholder.display()).into());
                }
            }

            for direct_dep in &unit.dependencies {
                // no need to add inputs here, they're already handled from transitive deps
                let dep = drv_cache[direct_dep.index].as_ref().unwrap();

                let out = Placeholder::ca_output(&dep.drv_path, &OUTPUT_OUT).render();

                if !dep.meta.custom_output {
                    let artifact = plan::dependency_artifact(compiler_plan, &dep.meta.artifacts)?;
                    args.push_back("--extern".into());
                    args.push_back(
                        format!(
                            "{}={}",
                            direct_dep.extern_crate_name,
                            out.join(&artifact.file_name).display()
                        )
                        .into(),
                    );
                }
                if dep.meta.custom_output {
                    inputs.insert(SingleDerivedPath::Opaque(tools.env_wrap.store_path.clone()));
                    env.insert("OUT_DIR".into(), out.into_bytes());
                    let flags = Placeholder::ca_output(&dep.drv_path, &OUTPUT_FLAGS).render();

                    // reversed since we're pushing from the front
                    args.push_front("--".into());
                    args.push_front(flags.join(SCRIPT_IMMEDIATE_ENV).into_bytes());
                    args.push_front(tools.env_wrap.real_path.clone_bytes());

                    args.push_back(
                        format!("@{}", flags.join(SCRIPT_IMMEDIATE_ARGS).display()).into(),
                    );
                }
            }

            {
                // rustc doesn't respect CC environment variable
                // unix-like platforms use cc as linker
                // this won't work for wasm, though.
                let cc = if unit.platform.is_some() {
                    &tools.target_cc
                } else {
                    &tools.host_cc
                };

                cc.bind_linker(&compiler_plan.rustc_args, &mut args)?;
                inputs.insert(SingleDerivedPath::Opaque(cc.store_path.clone()));
            }
            env.insert("PATH".into(), path.into());
            // Wrap the complete existing command, including build-script env-wrap
            // layers. Only successful compiler outputs enter the public output.
            inputs.insert(SingleDerivedPath::Opaque(
                tools.compile_wrap.store_path.clone(),
            ));
            args.push_front(Placeholder::standard_output(&OUTPUT_OUT).into_bytes());
            args.push_front(compiler_plan.output_directory().into());
            let builder = tools.compile_wrap.real_path.clone_bytes();
            (
                Derivation {
                    name: StorePathName::from_str(drv_name).wrap_err("invalid derivation name")?,
                    outputs: BTreeMap::from([(
                        OUTPUT_OUT.clone(),
                        DerivationOutput::CAFloating(ContentAddressMethodAlgorithm::NixArchive(
                            SHA256,
                        )),
                    )]),
                    inputs,
                    platform: PLATFORM.clone(),
                    builder,
                    args: args.into(),
                    // TODO: cargo crate environment variables
                    // (https://doc.rust-lang.org/cargo/reference/environment-variables.html)
                    env,
                    structured_attrs: None,
                },
                UnitCacheMeta {
                    artifacts: compiler_plan.artifacts.clone(),
                    ..Default::default()
                },
            )
        };

        if package_source.is_some() {
            let wrapper = source_wrapper.as_ref().unwrap();
            drv.inputs
                .insert(SingleDerivedPath::Opaque(wrapper.store_path.clone()));
            let mut args = vec![
                src_path.to_absolute_path(&store_dir).into_bytes(),
                drv.builder,
            ];
            args.extend(drv.args);
            drv.builder = wrapper.real_path.clone_bytes();
            drv.args = args;
        }

        let drv_path = store::add_drv_to_store(&mut store, &store_dir, drv, drv_name).await?;
        drv_cache[unit_idx] = Some(UnitCache { drv_path, meta });
    }

    for unit_idx in unit_graph.roots {
        let drv_path = &drv_cache[unit_idx].as_ref().unwrap().drv_path;
        println!("{}", store_dir.display(drv_path));

        if store::is_in_derivation() {
            // TODO: need more handling if there is both a lib and a bin of the same crate
            let name = unit_graph.units[unit_idx].target.name.as_str();
            store::submit_wrapper(&mut store, &store_dir, &tools.ln, name, drv_path).await?;
        }
    }

    // TODO: run the build if we are outside a derivation,
    // attach to output if we are inside a derivation.

    Ok(())
}

#[cfg(test)]
mod check_cfg_tests {
    use super::declared_check_cfg_args;
    use std::collections::BTreeMap;

    #[test]
    fn declared_features_include_disabled_implicit_and_unicode_names() {
        let features = BTreeMap::from([
            ("disabled".into(), Vec::new()),
            ("optional-dep".into(), vec!["dep:optional-dep".into()]),
            ("a+b.c".into(), Vec::new()),
            ("Δ".into(), Vec::new()),
        ]);
        assert_eq!(
            declared_check_cfg_args(&features),
            [
                "--check-cfg",
                "cfg(docsrs,test)",
                "--check-cfg",
                "cfg(feature, values(\"a+b.c\", \"disabled\", \"optional-dep\", \"Δ\"))"
            ]
        );
        assert_eq!(
            declared_check_cfg_args(&BTreeMap::new())[3],
            "cfg(feature, values())"
        );
    }
}

#[cfg(test)]
mod feature_env_tests {
    use super::add_build_script_feature_env;
    use std::collections::BTreeMap;

    #[test]
    fn empty_feature_set_preserves_other_environment() {
        let mut env = BTreeMap::from([("KEEP".into(), "value".into())]);
        let original = env.clone();
        add_build_script_feature_env(&mut env, &[]);
        assert_eq!(env, original);
    }

    #[test]
    fn enabled_feature_overrides_prior_value_without_adding_disabled_features() {
        let mut env = BTreeMap::from([("CARGO_FEATURE_PROBE".into(), "external".into())]);
        add_build_script_feature_env(&mut env, &["probe".into()]);
        assert_eq!(
            env,
            BTreeMap::from([("CARGO_FEATURE_PROBE".into(), "1".into())])
        );
    }

    #[cfg(unix)]
    #[test]
    fn actual_child_reads_cargo_feature_values_from_production_owner() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt, process::Command};
        let mut env = BTreeMap::from([("KEEP".into(), "unchanged".into())]);
        add_build_script_feature_env(
            &mut env,
            &["default".into(), "probe".into(), "hyphen-feature".into()],
        );
        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg(r#"printf '%s\000' "$CARGO_FEATURE_DEFAULT" "$CARGO_FEATURE_PROBE" "$CARGO_FEATURE_HYPHEN_FEATURE" "$KEEP" "${CARGO_FEATURE_DISABLED+set}""#)
            .env_clear()
            .envs(env.into_iter().map(|(key, value)| (OsString::from_vec(key.to_vec()), OsString::from_vec(value.to_vec()))))
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        assert_eq!(
            output.stdout,
            [b"1\0".as_slice(), b"1\0", b"1\0", b"unchanged\0\0"].concat()
        );
    }
}

#[cfg(test)]
mod package_env_tests {
    use super::add_metadata_env;
    use std::collections::BTreeMap;

    const KEYS: [&str; 14] = [
        "CARGO_PKG_NAME",
        "CARGO_PKG_AUTHORS",
        "CARGO_PKG_DESCRIPTION",
        "CARGO_PKG_HOMEPAGE",
        "CARGO_PKG_REPOSITORY",
        "CARGO_PKG_LICENSE",
        "CARGO_PKG_LICENSE_FILE",
        "CARGO_PKG_README",
        "CARGO_PKG_VERSION",
        "CARGO_PKG_VERSION_MAJOR",
        "CARGO_PKG_VERSION_MINOR",
        "CARGO_PKG_VERSION_PATCH",
        "CARGO_PKG_VERSION_PRE",
        "CARGO_PKG_RUST_VERSION",
    ];

    fn package(extra: serde_json::Value) -> crate::package_metadata::PackageRecord {
        let mut value = serde_json::json!({
            "name":"minimal-name", "version":"1.2.3-alpha.1+build.5",
            "id":"path+file:///fixture#minimal-name@1.2.3-alpha.1+build.5",
            "manifest_path":"/fixture/Cargo.toml", "targets":[],
            "dependencies":[], "features":{}
        });
        value
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let document = serde_json::json!({"packages":[value],"workspace_members":[],"workspace_root":"/fixture","target_directory":"/fixture/target","version":1,"resolve":null});
        crate::package_metadata::Metadata::from_slice(&serde_json::to_vec(&document).unwrap())
            .unwrap()
            .packages
            .remove(0)
    }

    #[test]
    fn minimal_package_sets_empty_optional_values_and_preserves_precedence() {
        let meta = package(serde_json::json!({}));
        let mut env = BTreeMap::from([
            ("KEEP".into(), "unchanged".into()),
            ("CARGO_PKG_NAME".into(), "stale".into()),
            (
                "CARGO_PKG_RUST_VERSION".into(),
                "unrelated-existing-value".into(),
            ),
        ]);
        add_metadata_env(&mut env, &meta);
        let expected = [
            "minimal-name",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "1.2.3-alpha.1+build.5",
            "1",
            "2",
            "3",
            "alpha.1",
            "",
        ];
        for (key, value) in KEYS.into_iter().zip(expected) {
            assert_eq!(env[key.as_bytes()].as_ref(), value.as_bytes(), "{key}");
        }
        assert_eq!(env.len(), 15);
        assert_eq!(env[b"KEEP".as_slice()], "unchanged");
        assert_eq!(env[b"CARGO_PKG_RUST_VERSION".as_slice()], "");
        assert!(!env.contains_key(b"CARGO_PKG_DOCUMENTATION".as_slice()));
    }

    #[test]
    fn populated_and_distinct_packages_keep_exact_text_and_paths() {
        let populated = package(serde_json::json!({
            "name":"other-package", "authors":["A:one", "Δ two"],
            "description":" leading\ntext ", "homepage":"https://example.test/home",
            "repository":"https://example.test/repo", "license":"MIT OR Apache-2.0",
            "license_file":"../licenses/custom file", "readme":"./read me.md",
            "rust_version":"1.60"
        }));
        let mut env = BTreeMap::new();
        add_metadata_env(&mut env, &populated);
        for (key, value) in [
            ("CARGO_PKG_NAME", "other-package"),
            ("CARGO_PKG_AUTHORS", "A:one:Δ two"),
            ("CARGO_PKG_DESCRIPTION", " leading\ntext "),
            ("CARGO_PKG_HOMEPAGE", "https://example.test/home"),
            ("CARGO_PKG_REPOSITORY", "https://example.test/repo"),
            ("CARGO_PKG_LICENSE", "MIT OR Apache-2.0"),
            ("CARGO_PKG_LICENSE_FILE", "../licenses/custom file"),
            ("CARGO_PKG_README", "./read me.md"),
        ] {
            assert_eq!(env[key.as_bytes()].as_ref(), value.as_bytes(), "{key}");
        }
        assert_eq!(env[b"CARGO_PKG_RUST_VERSION".as_slice()], "1.60");
        let explicit_patch = package(serde_json::json!({"rust_version":"1.60.0"}));
        assert_eq!(
            populated.package.rust_version,
            explicit_patch.package.rust_version
        );
        let mut explicit_env = BTreeMap::new();
        add_metadata_env(&mut explicit_env, &explicit_patch);
        assert_eq!(explicit_env[b"CARGO_PKG_RUST_VERSION".as_slice()], "1.60.0");
        add_metadata_env(&mut env, &package(serde_json::json!({})));
        assert_eq!(env[b"CARGO_PKG_NAME".as_slice()], "minimal-name");
        assert_eq!(env[b"CARGO_PKG_AUTHORS".as_slice()], "");
        assert_eq!(env[b"CARGO_PKG_README".as_slice()], "");
        assert_eq!(env.len(), 14);
    }

    #[cfg(unix)]
    #[test]
    fn real_cargo_metadata_env_macro_and_child_agree_with_package_owner() {
        use std::{
            ffi::OsString,
            fs,
            os::unix::ffi::OsStringExt,
            path::PathBuf,
            process::{Command, Output},
            time::{Instant, SystemTime, UNIX_EPOCH},
        };
        let parent = std::env::var_os("CARGO_DYNDRV_PACKAGE_TEST_ROOT");
        let preserve = parent.is_some();
        let root = parent
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!(
                "package-env-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        fs::create_dir(&root).unwrap();
        let cargo = PathBuf::from(std::env::var_os("CARGO").unwrap());
        let rustc =
            which::which(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into())).unwrap();
        let tool_path = std::env::var_os("PATH").unwrap();
        let linker = std::env::var_os("CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER");
        let mut receipts = Vec::new();
        let mut run = |name: &str, command: &mut Command, case: &std::path::Path| -> Output {
            let now = Instant::now();
            let output = command.output().unwrap();
            fs::write(case.join(format!("{name}.stdout")), &output.stdout).unwrap();
            fs::write(case.join(format!("{name}.stderr")), &output.stderr).unwrap();
            receipts.push(serde_json::json!({"name":name,"case":case,"program":command.get_program(),"argv":command.get_args().collect::<Vec<_>>(),"seconds":now.elapsed().as_secs_f64(),"exitCode":output.status.code()}));
            fs::write(
                root.join("commands.json"),
                serde_json::to_vec_pretty(&receipts).unwrap(),
            )
            .unwrap();
            assert!(
                output.status.success(),
                "{name}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            output
        };
        let expressions = KEYS.map(|key| format!("env!(\"{key}\")")).join(",");
        let names = KEYS.map(|key| format!("\"{key}\"")).join(",");
        let source = format!(
            "use std::io::Write;fn main(){{let compile=[{expressions}];let keys=[{names}];let runtime=std::env::args().nth(1).as_deref()==Some(\"runtime\");for(i,key)in keys.iter().enumerate(){{let value=if runtime{{std::env::var(key).unwrap()}}else{{compile[i].to_owned()}};std::io::stdout().write_all(value.as_bytes()).unwrap();std::io::stdout().write_all(&[0]).unwrap();}}}}"
        );
        for (name, inherited, fields) in [
            ("minimal", false, ""),
            (
                "populated",
                false,
                "rust-version=\"1.85\"\nauthors=[\"A:one\",\"Δ two\"]\ndescription=\" leading text \"\nhomepage=\"https://example.test/home\"\nrepository=\"https://example.test/repo\"\nlicense=\"MIT\"\nlicense-file=\"custom license\"\nreadme=\"read me.md\"\n",
            ),
            (
                "inherited",
                true,
                "rust-version.workspace=true\nauthors.workspace=true\ndescription.workspace=true\nhomepage.workspace=true\nrepository.workspace=true\nlicense.workspace=true\nlicense-file.workspace=true\nreadme.workspace=true\n",
            ),
        ] {
            let case = root.join(name);
            fs::create_dir(&case).unwrap();
            let crate_dir = if inherited {
                let dir = case.join("member");
                fs::create_dir(&dir).unwrap();
                dir
            } else {
                case.clone()
            };
            if inherited {
                fs::write(case.join("Cargo.toml"), "[workspace]\nmembers=[\"member\"]\nresolver=\"3\"\n[workspace.package]\nrust-version=\"1.85.0\"\nauthors=[\"Workspace author\"]\ndescription=\"Inherited text\"\nhomepage=\"https://example.test/ws\"\nrepository=\"https://example.test/ws-repo\"\nlicense=\"Apache-2.0\"\nlicense-file=\"ROOT-LICENSE\"\nreadme=\"ROOT-README.md\"\n").unwrap();
                fs::write(case.join("ROOT-LICENSE"), "fixture license").unwrap();
                fs::write(case.join("ROOT-README.md"), "fixture readme").unwrap();
            }
            fs::create_dir(crate_dir.join("src")).unwrap();
            fs::write(crate_dir.join("src/main.rs"), &source).unwrap();
            fs::write(crate_dir.join("Cargo.toml"), format!("[package]\nname=\"package-{name}\"\nversion=\"1.2.3-alpha.1\"\nedition=\"2024\"\n{fields}{}", if inherited {""} else {"[workspace]\n"})).unwrap();
            if name == "minimal" {
                fs::write(crate_dir.join("README.md"), "default readme fixture").unwrap();
            }
            if name == "populated" {
                for file in ["custom license", "read me.md"] {
                    fs::write(crate_dir.join(file), "fixture").unwrap();
                }
            }
            fs::write(case.join("Cargo.lock"), format!("version = 4\n[[package]]\nname = \"package-{name}\"\nversion = \"1.2.3-alpha.1\"\n")).unwrap();
            fs::create_dir(case.join("home")).unwrap();
            fs::create_dir(case.join("cargo-home")).unwrap();
            let clean = |program: &std::path::Path| {
                let mut command = Command::new(program);
                command
                    .env_clear()
                    .current_dir(&case)
                    .env("HOME", case.join("home"))
                    .env("CARGO_HOME", case.join("cargo-home"))
                    .env("PATH", &tool_path)
                    .env("RUSTC", &rustc)
                    .env("CARGO_INCREMENTAL", "0")
                    .env("CARGO_BUILD_JOBS", "1")
                    .env("CARGO_TARGET_DIR", case.join("target"));
                if let Some(linker) = &linker {
                    command.env("CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER", linker);
                }
                command
            };
            let metadata = run(
                "metadata",
                clean(&cargo).args([
                    "metadata",
                    "--format-version",
                    "1",
                    "--no-deps",
                    "--offline",
                    "--locked",
                ]),
                &case,
            );
            let metadata = crate::package_metadata::Metadata::from_slice(&metadata.stdout).unwrap();
            assert_eq!(metadata.packages.len(), 1);
            let mut env = BTreeMap::new();
            add_metadata_env(&mut env, &metadata.packages[0]);
            assert_eq!(env.len(), 14);
            if name == "minimal" {
                assert_eq!(env[b"CARGO_PKG_README".as_slice()], "README.md");
            }
            if inherited {
                assert_eq!(env[b"CARGO_PKG_AUTHORS".as_slice()], "Workspace author");
                assert_eq!(env[b"CARGO_PKG_README".as_slice()], "../ROOT-README.md");
                assert_eq!(env[b"CARGO_PKG_LICENSE_FILE".as_slice()], "../ROOT-LICENSE");
            }
            run(
                "cargo-build",
                clean(&cargo).args(["build", "--offline", "--locked", "-j", "1"]),
                &case,
            );
            let stock = run(
                "cargo-env-macro",
                Command::new(case.join(format!("target/debug/package-{name}"))).env_clear(),
                &case,
            );
            let expected = KEYS
                .iter()
                .flat_map(|key| {
                    env[key.as_bytes()]
                        .iter()
                        .copied()
                        .chain(std::iter::once(0))
                })
                .collect::<Vec<_>>();
            assert_eq!(stock.stdout, expected, "{name}");
            let direct = case.join("direct-env-macro");
            let mut compile = clean(&rustc);
            compile
                .args(["--edition=2024", "-C", "codegen-units=1"])
                .arg(crate_dir.join("src/main.rs"))
                .arg("-o")
                .arg(&direct);
            if let Some(linker) = &linker {
                compile
                    .arg("-C")
                    .arg(format!("linker={}", linker.to_str().unwrap()));
            }
            let environment = env
                .iter()
                .map(|(key, value)| {
                    (
                        OsString::from_vec(key.to_vec()),
                        OsString::from_vec(value.to_vec()),
                    )
                })
                .collect::<Vec<_>>();
            compile.envs(environment.clone());
            run("direct-compile", &mut compile, &case);
            let compiled = run("direct-env-macro", Command::new(&direct).env_clear(), &case);
            assert_eq!(compiled.stdout, stock.stdout, "{name}");
            let actual_child = run(
                "direct-child-env",
                Command::new(&direct)
                    .arg("runtime")
                    .env_clear()
                    .envs(environment),
                &case,
            );
            assert_eq!(actual_child.stdout, stock.stdout, "{name}");
        }
        if !preserve {
            fs::remove_dir_all(root).unwrap();
        }
    }
}

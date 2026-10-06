//! Cargo unit semantics, independent of Nix paths and daemon access.
//!
//! This is deliberately a partial compiler plan: source/output paths, dependency
//! paths, metadata disambiguation and build-script results are bound by the backend.
mod graph2;
pub(crate) use graph2::execution_plans;

use std::collections::BTreeMap;

use color_eyre::eyre::{self, bail};

use crate::unit_graph::{CompileMode, Unit, UnitGraph};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactKind {
    Metadata,
    DepInfo,
    Rlib,
    DynamicLibrary,
    StaticLibrary,
    Executable,
    ProcMacro,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Artifact {
    pub kind: ArtifactKind,
    pub file_name: String,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompilerPlan {
    pub rustc_args: Vec<String>,
    pub artifacts: Vec<Artifact>,
    /// Compiler side outputs, never candidates for Rust dependency selection.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub auxiliary_artifacts: Vec<Artifact>,
    pub dependency_artifact: ArtifactKind,
}

impl CompilerPlan {
    /// Each Nix builder has a private /build; public CA output paths must not
    /// become rustc output paths. Source paths and compiler identity are separate.
    pub fn output_directory(&self) -> &'static str {
        "/build/compiler-output"
    }
}

fn codegen(args: &mut Vec<String>, key: &str, value: impl std::fmt::Display) {
    args.extend(["-C".into(), format!("{key}={value}")]);
}

/// Already validated Cargo policy; this carries decisions, not profile inference.
#[derive(Clone, Copy)]
struct GeneratedCodegen<'a> {
    emission_args: &'a [String],
    lto_args: &'a [String],
    metadata: bool,
    link: bool,
}

#[derive(Clone, Copy)]
enum CodegenSource<'a> {
    Legacy,
    Cargo(GeneratedCodegen<'a>),
}

/// The unit graph's `target.test` is eligibility, not the manifest's `harness`.
/// Test-mode units therefore require that separately resolved manifest setting.
/// For checks, Some(None) means ordinary checking, Some(Some(bool)) means
/// checking a test target; None means unknown and is rejected.
pub fn compiler_plan(
    unit: &Unit,
    host: &str,
    test_context: Option<Option<bool>>,
) -> eyre::Result<CompilerPlan> {
    compiler_plan_with_codegen(unit, host, test_context, CodegenSource::Legacy)
}

fn compiler_plan_with_codegen(
    unit: &Unit,
    host: &str,
    test_context: Option<Option<bool>>,
    codegen_source: CodegenSource<'_>,
) -> eyre::Result<CompilerPlan> {
    if !host.contains("-linux-")
        || unit
            .platform
            .as_ref()
            .is_some_and(|p| !p.contains("-linux-"))
    {
        bail!("artifact naming currently supports Linux targets only");
    }
    match unit.mode {
        CompileMode::Build | CompileMode::Check | CompileMode::Test => {}
        _ => bail!("mode {:?} requires a separate execution owner", unit.mode),
    }
    if unit.profile.incremental {
        bail!("incremental compilation requires a writable state owner");
    }
    if unit.profile.codegen_backend.is_some() {
        bail!("custom codegen backends are not supported");
    }
    if matches!(codegen_source, CodegenSource::Legacy)
        && !matches!(unit.profile.lto.as_str(), "false" | "off")
    {
        bail!("explicit LTO requires graph-wide bitcode planning");
    }
    if !matches!(
        unit.profile.opt_level.as_str(),
        "0" | "1" | "2" | "3" | "s" | "z"
    ) {
        bail!("unsupported optimization level");
    }
    if !matches!(unit.profile.panic.as_str(), "unwind" | "abort") {
        bail!("unsupported panic strategy");
    }
    if unit.profile.codegen_units == Some(0) {
        bail!("codegen units must be positive");
    }
    if unit
        .dependencies
        .iter()
        .any(|d| d.public || d.noprelude || d.nounused)
    {
        bail!("dependency visibility/prelude flags are not supported");
    }
    // Cargo serializes Check { test: bool } as just "check". A missing
    // context is therefore unknown, while an explicit null means ordinary check.
    let test_harness =
        match unit.mode {
            CompileMode::Test => Some(test_context.flatten().ok_or_else(|| {
                eyre::eyre!("test mode requires explicit manifest harness setting")
            })?),
            CompileMode::Check => test_context.ok_or_else(|| {
                eyre::eyre!(
                    "check mode requires explicit test context; the unit graph loses this setting"
                )
            })?,
            _ => None,
        };
    let testing = test_harness.is_some();
    let harness = test_harness.unwrap_or(false);
    let types = &unit.target.crate_types;
    if types.is_empty()
        || types.iter().any(|t| {
            !matches!(
                t.as_str(),
                "lib" | "rlib" | "cdylib" | "dylib" | "staticlib" | "bin" | "proc-macro"
            )
        })
    {
        bail!("unsupported crate types {:?}", types);
    }
    if types.len() > 1
        && types
            .iter()
            .any(|t| matches!(t.as_str(), "bin" | "proc-macro"))
    {
        bail!("bin and proc-macro crate types must be exclusive");
    }
    let name = unit.target.name.replace('-', "_");
    let mut args = vec![
        format!("--crate-name={name}"),
        format!("--edition={}", unit.target.edition),
    ];
    if testing {
        if harness {
            args.push("--test".into());
        } else {
            args.extend(["--cfg".into(), "test".into()]);
        }
    } else {
        for ty in types {
            args.push(format!("--crate-type={ty}"));
        }
    }
    let checking = unit.mode == CompileMode::Check;
    let (metadata_output, link_output, auxiliary_artifacts) = match codegen_source {
        CodegenSource::Legacy => {
            let metadata = types.len() == 1 && matches!(types[0].as_str(), "lib" | "rlib");
            args.push(format!(
                "--emit={}",
                if checking {
                    "metadata"
                } else if metadata && !testing {
                    "metadata,link"
                } else {
                    "link"
                }
            ));
            (checking || metadata && !testing, !checking, Vec::new())
        }
        CodegenSource::Cargo(policy) => {
            args.extend(policy.emission_args.iter().cloned());
            (
                policy.metadata,
                policy.link,
                vec![Artifact {
                    kind: ArtifactKind::DepInfo,
                    file_name: format!("{name}.d"),
                }],
            )
        }
    };
    let exported = matches!(codegen_source, CodegenSource::Cargo(_));
    let proc_macro = types.iter().any(|t| t == "proc-macro");
    // The v3 stages follow pinned Cargo: emission, prefer-dynamic, opt/panic,
    // generated LTO, CGU/debug. Existing v1/v2 argument order stays unchanged.
    if exported && proc_macro {
        codegen(&mut args, "prefer-dynamic", "yes");
    }
    codegen(&mut args, "opt-level", &unit.profile.opt_level);
    if exported && !testing && unit.profile.panic != "unwind" {
        codegen(&mut args, "panic", &unit.profile.panic);
    }
    match codegen_source {
        CodegenSource::Legacy => {
            codegen(&mut args, "debuginfo", unit.profile.debuginfo);
            // `false` permits local ThinLTO; keep the legacy argument sequence.
            codegen(&mut args, "embed-bitcode", "no");
            if unit.profile.lto == "off" {
                codegen(&mut args, "lto", "off");
            }
        }
        CodegenSource::Cargo(policy) => args.extend(policy.lto_args.iter().cloned()),
    }
    if let Some(count) = unit.profile.codegen_units {
        codegen(&mut args, "codegen-units", count);
    }
    if exported {
        codegen(&mut args, "debuginfo", unit.profile.debuginfo);
    }
    codegen(&mut args, "debug-assertions", unit.profile.debug_assertions);
    codegen(&mut args, "overflow-checks", unit.profile.overflow_checks);
    if unit.profile.rpath {
        bail!(
            "profile rpath=true is unsupported with staged outputs: rustc computes paths relative to its output directory"
        );
    }
    if let Some(split) = &unit.profile.split_debuginfo {
        if !matches!(split.as_str(), "off" | "packed" | "unpacked") {
            bail!("unsupported split-debuginfo setting");
        }
        codegen(&mut args, "split-debuginfo", split);
    }
    if testing && unit.profile.panic != "unwind" {
        bail!("test panic strategy requires unwind");
    }
    if !exported && !testing && unit.profile.panic != "unwind" {
        codegen(&mut args, "panic", &unit.profile.panic);
    }
    if let Some(strip) = &unit.profile.strip {
        let strip = strip.resolved_name()?;
        if strip != "none" {
            codegen(&mut args, "strip", strip);
        }
    }
    for feature in &unit.features {
        args.extend(["--cfg".into(), format!("feature={feature:?}")]);
    }
    if let Some(target) = &unit.platform {
        args.push(format!("--target={target}"));
    }
    if proc_macro {
        if !exported {
            codegen(&mut args, "prefer-dynamic", "yes");
        }
        args.extend(["--extern".into(), "proc_macro".into()]);
    }
    let mut artifacts = Vec::new();
    let mut artifact = |kind, file_name| artifacts.push(Artifact { kind, file_name });
    if metadata_output {
        artifact(ArtifactKind::Metadata, format!("lib{name}.rmeta"));
    }
    if link_output {
        if testing {
            artifact(ArtifactKind::Executable, name.clone());
        } else {
            for ty in types {
                let (kind, file_name) = match ty.as_str() {
                    "lib" | "rlib" => (ArtifactKind::Rlib, format!("lib{name}.rlib")),
                    "cdylib" | "dylib" => (ArtifactKind::DynamicLibrary, format!("lib{name}.so")),
                    "staticlib" => (ArtifactKind::StaticLibrary, format!("lib{name}.a")),
                    "proc-macro" => (ArtifactKind::ProcMacro, format!("lib{name}.so")),
                    "bin" => (ArtifactKind::Executable, name.clone()),
                    _ => unreachable!(),
                };
                if artifacts.iter().any(|a| a.file_name == file_name) {
                    bail!("crate types have colliding output names");
                }
                artifacts.push(Artifact { kind, file_name });
            }
        }
    }
    Ok(CompilerPlan {
        rustc_args: args,
        artifacts,
        auxiliary_artifacts,
        dependency_artifact: if checking {
            ArtifactKind::Metadata
        } else {
            ArtifactKind::Rlib
        },
    })
}

pub fn dependency_artifact<'a>(
    consumer: &CompilerPlan,
    artifacts: &'a [Artifact],
) -> eyre::Result<&'a Artifact> {
    artifacts
        .iter()
        .find(|a| a.kind == ArtifactKind::ProcMacro)
        .or_else(|| {
            artifacts
                .iter()
                .find(|a| a.kind == consumer.dependency_artifact)
        })
        .ok_or_else(|| eyre::eyre!("dependency has no compatible Rust artifact"))
}

/// Plan-only output deliberately retains unsupported units instead of pretending
/// that parsing a doctest graph node means the doctest has been executed.
pub fn graph_plan(
    graph: &UnitGraph,
    host: &str,
    harnesses: &BTreeMap<usize, Option<bool>>,
) -> eyre::Result<serde_json::Value> {
    if graph.version != 1 {
        bail!("unsupported unit graph version {}", graph.version);
    }
    for (index, unit) in graph.units.iter().enumerate() {
        if unit
            .dependencies
            .iter()
            .any(|d| d.index >= graph.units.len())
        {
            bail!("unit {index} has an out-of-range dependency");
        }
    }
    if graph.roots.iter().any(|i| *i >= graph.units.len()) {
        bail!("out-of-range graph root");
    }
    if harnesses.keys().any(|i| {
        graph
            .units
            .get(*i)
            .is_none_or(|u| !matches!(u.mode, CompileMode::Test | CompileMode::Check))
    }) {
        bail!("harness override must identify a test/check-mode unit");
    }
    let units: Vec<_> = graph.units.iter().enumerate().map(|(index, unit)| {
        let (plan, unsupported) = match compiler_plan(unit, host, harnesses.get(&index).copied()) {
            Ok(plan) => (Some(plan), None),
            Err(error) => (None, Some(error.to_string())),
        };
        serde_json::json!({"index":index,"packageId":unit.pkg_id,"targetName":unit.target.name,"mode":unit.mode,"source":unit.target.src_path,"plan":plan,"unsupported":unsupported})
    }).collect();
    Ok(serde_json::json!({"schemaVersion":1,"host":host,"units":units,"roots":graph.roots}))
}

pub fn cli(args: &[String]) -> eyre::Result<()> {
    let mut graph = None;
    let mut host = None;
    let mut harnesses = None;
    let (pairs, remainder) = args.as_chunks::<2>();
    if !remainder.is_empty() {
        bail!("usage: plan --unit-graph FILE --host TRIPLE [--test-harnesses FILE]");
    }
    for pair in pairs {
        match pair[0].as_str() {
            "--unit-graph" if graph.is_none() => {
                graph = Some(serde_json::from_slice::<serde_json::Value>(
                    &std::fs::read(&pair[1])?,
                )?)
            }
            "--host" if host.is_none() => host = Some(pair[1].as_str()),
            "--test-harnesses" if harnesses.is_none() => {
                harnesses = Some(serde_json::from_slice(&std::fs::read(&pair[1])?)?)
            }
            _ => bail!("unrecognized or repeated plan option {}", pair[0]),
        }
    }
    let graph = graph.ok_or_else(|| eyre::eyre!("--unit-graph required"))?;
    let host = host.ok_or_else(|| eyre::eyre!("--host required"))?;
    let harnesses = harnesses.unwrap_or_default();
    let result = match graph.get("version").and_then(serde_json::Value::as_u64) {
        Some(2..=4) => graph2::graph_plan(graph, host, &harnesses)?,
        _ => graph_plan(&serde_json::from_value(graph)?, host, &harnesses)?,
    };
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

/// Pure path/required-output contract. It neither binds nor executes Nix inputs.
pub fn generated_cli(args: &[String]) -> eyre::Result<()> {
    if args.len() != 4 || args[0] != "--unit-graph" || args[2] != "--host" {
        bail!("usage: plan-generated-layout --unit-graph FILE --host TRIPLE");
    }
    let graph = serde_json::from_slice(&std::fs::read(&args[1])?)?;
    let result = graph2::generated_plan(graph, &args[3])?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit() -> Unit {
        serde_json::from_value(serde_json::json!({
            "pkg_id":"path+file:///fixture#example@0.1.0",
            "target":{"kind":["lib"],"crate_types":["rlib","cdylib"],"name":"example-name","src_path":"/fixture/src/lib.rs","edition":"2024","doc":true,"doctest":true,"test":true},
            "profile":{"name":"ci-optim","opt_level":"2","lto":"false","codegen_backend":null,"codegen_units":16,"debuginfo":0,"split_debuginfo":null,"debug_assertions":true,"overflow_checks":true,"rpath":false,"incremental":false,"panic":"unwind","strip":{"resolved":{"Named":"debuginfo"}}},
            "platform":null,"mode":"build","features":["python"],"dependencies":[]
        })).unwrap()
    }
    const HOST: &str = "x86_64-unknown-linux-gnu";

    #[test]
    fn mixed_library_outputs_and_profile_are_explicit() {
        let p = compiler_plan(&unit(), HOST, None).unwrap();
        assert_eq!(
            p.artifacts
                .iter()
                .map(|a| a.file_name.as_str())
                .collect::<Vec<_>>(),
            ["libexample_name.rlib", "libexample_name.so"]
        );
        for flag in [
            "--crate-type=rlib",
            "--crate-type=cdylib",
            "--emit=link",
            "codegen-units=16",
            "embed-bitcode=no",
            "strip=debuginfo",
            "debug-assertions=true",
            "overflow-checks=true",
        ] {
            assert!(p.rustc_args.iter().any(|arg| arg == flag), "{flag}");
        }
        assert!(!p.rustc_args.iter().any(|arg| arg.starts_with("lto=")));
        assert_eq!(p.output_directory(), "/build/compiler-output");
        assert_eq!(
            dependency_artifact(&p, &p.artifacts).unwrap().kind,
            ArtifactKind::Rlib
        );
    }
    #[test]
    fn test_eligibility_is_not_harness_configuration() {
        let mut u = unit();
        u.mode = CompileMode::Test;
        assert!(
            compiler_plan(&u, HOST, None)
                .unwrap_err()
                .to_string()
                .contains("harness")
        );
        let p = compiler_plan(&u, HOST, Some(Some(true))).unwrap();
        assert!(p.rustc_args.contains(&"--test".into()));
        assert!(
            !p.rustc_args
                .iter()
                .any(|arg| arg.starts_with("--crate-type"))
        );
        assert_eq!(p.artifacts.len(), 1);
        assert_eq!(p.artifacts[0].kind, ArtifactKind::Executable);
    }
    #[test]
    fn custom_harness_keeps_cfg_test_and_proc_macro_test_keeps_host_runtime() {
        let mut u = unit();
        u.mode = CompileMode::Test;
        let p = compiler_plan(&u, HOST, Some(Some(false))).unwrap();
        assert!(
            p.rustc_args
                .windows(2)
                .any(|pair| pair == ["--cfg", "test"])
        );
        assert!(
            !p.rustc_args
                .iter()
                .any(|arg| arg == "--test" || arg.starts_with("--crate-type"))
        );
        assert_eq!(p.artifacts[0].kind, ArtifactKind::Executable);
        u.target.crate_types = vec!["proc-macro".into()];
        let p = compiler_plan(&u, HOST, Some(Some(true))).unwrap();
        assert!(p.rustc_args.contains(&"prefer-dynamic=yes".into()));
        assert!(
            p.rustc_args
                .windows(2)
                .any(|pair| pair == ["--extern", "proc_macro"])
        );
    }
    #[test]
    fn check_context_cannot_be_inferred_from_serialized_graph() {
        let mut u = unit();
        u.mode = CompileMode::Check;
        assert!(
            compiler_plan(&u, HOST, None)
                .unwrap_err()
                .to_string()
                .contains("loses")
        );
        let ordinary = compiler_plan(&u, HOST, Some(None)).unwrap();
        let test = compiler_plan(&u, HOST, Some(Some(true))).unwrap();
        assert!(!ordinary.rustc_args.contains(&"--test".into()));
        assert!(test.rustc_args.contains(&"--test".into()));
        assert_eq!(test.artifacts[0].kind, ArtifactKind::Metadata);
    }
    #[test]
    fn explicit_lto_off_is_distinct_from_default_local_lto() {
        let mut u = unit();
        u.profile.lto = "off".into();
        assert!(
            compiler_plan(&u, HOST, None)
                .unwrap()
                .rustc_args
                .contains(&"lto=off".into())
        );
    }
    #[test]
    fn output_relative_rpath_cannot_be_staged() {
        let mut u = unit();
        u.profile.rpath = true;
        let error = compiler_plan(&u, HOST, None).unwrap_err();
        assert!(error.to_string().contains("rpath=true"));
        assert!(error.to_string().contains("staged outputs"));
    }

    #[test]
    fn unsupported_semantics_are_not_silently_discarded() {
        for mode in [
            CompileMode::Doc,
            CompileMode::Doctest,
            CompileMode::Docscrape,
            CompileMode::RunCustomBuild,
        ] {
            let mut u = unit();
            u.mode = mode;
            assert!(compiler_plan(&u, HOST, None).is_err());
        }
        let mut u = unit();
        u.profile.incremental = true;
        assert!(compiler_plan(&u, HOST, None).is_err());
        let mut u = unit();
        u.profile.lto = "thin".into();
        assert!(compiler_plan(&u, HOST, None).is_err());
        let mut u = unit();
        u.profile.codegen_backend = Some("cranelift".into());
        assert!(compiler_plan(&u, HOST, None).is_err());
        assert!(compiler_plan(&unit(), "aarch64-apple-darwin", None).is_err());
    }
    #[test]
    fn check_selects_metadata_but_proc_macros_require_executable_code() {
        let mut u = unit();
        u.mode = CompileMode::Check;
        let p = compiler_plan(&u, HOST, Some(None)).unwrap();
        assert_eq!(p.artifacts.len(), 1);
        assert_eq!(
            dependency_artifact(&p, &p.artifacts).unwrap().kind,
            ArtifactKind::Metadata
        );
        let mut dep = unit();
        dep.target.crate_types = vec!["proc-macro".into()];
        let d = compiler_plan(&dep, HOST, None).unwrap();
        assert_eq!(
            dependency_artifact(&p, &d.artifacts).unwrap().kind,
            ArtifactKind::ProcMacro
        );
    }
    #[test]
    fn corrected_cargo_schema_fields_are_not_ignored() {
        let u = unit();
        assert_eq!(u.profile.codegen_units, Some(16));
        assert_eq!(
            serde_json::from_str::<CompileMode>("\"doctest\"").unwrap(),
            CompileMode::Doctest
        );
        assert!(serde_json::from_str::<CompileMode>("\"dooctest\"").is_err());
    }
}

//! Research Cargo graph inspection: v2/v3 retain their partial diagnostics;
//! schema 4 requires a complete parameterized recipe or an explicit refusal.
//! The bound execution gate remains closed pending complete recipe parity.
mod generated;
mod recipe;

use std::collections::{BTreeMap, BTreeSet};

use cargo_compiler_recipe::graph::validate_structure;
use color_eyre::eyre::{self, bail};
use serde_json::{Value, json};

use super::{
    ArtifactKind, CodegenSource, CompilerPlan, GeneratedCodegen, codegen,
    compiler_plan_with_codegen,
};
use crate::unit_graph::{CompileMode, UnitGraph};

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct CompilerIdentity {
    c_metadata: String,
    unit_id: String,
    extra_filename: Option<String>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct CompileContext {
    rustc_test: bool,
    harness: bool,
}

/// A support boundary for the pinned exporter, not a Cargo LTO resolver.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct GeneratedRustcPolicy {
    version: u32,
    generated_emission_args: Vec<String>,
    generated_lto_args: Vec<String>,
}

impl GeneratedRustcPolicy {
    fn validate(&self, mode: CompileMode) -> eyre::Result<()> {
        if self.version != 1 {
            bail!("unsupported generated Rust compiler policy version");
        }
        let emission: Vec<_> = self
            .generated_emission_args
            .iter()
            .map(String::as_str)
            .collect();
        match (mode, emission.as_slice()) {
            (CompileMode::Check, ["--emit=dep-info,metadata"])
            | (CompileMode::Build | CompileMode::Test, ["--emit=dep-info,link"])
            | (CompileMode::Build, ["--emit=dep-info,metadata,link"])
            | (CompileMode::Build, ["--emit=dep-info,metadata,link", "-Z", "embed-metadata=no"]) => {
            }
            _ => bail!("unsupported generated emission argument sequence or mode"),
        }
        let lto: Vec<_> = self.generated_lto_args.iter().map(String::as_str).collect();
        match lto.as_slice() {
            []
            | ["-C", "embed-bitcode=no"]
            | ["-C", "linker-plugin-lto"]
            | ["-C", "lto"]
            | ["-C", "lto=thin"]
            | ["-C", "lto=fat"]
            | ["-C", "lto=off", "-C", "embed-bitcode=no"] => Ok(()),
            _ => bail!("unsupported generated LTO argument sequence"),
        }
    }

    fn codegen(&self) -> eyre::Result<GeneratedCodegen<'_>> {
        if self.generated_emission_args.len() != 1 {
            bail!("no-embed-metadata requires a separate artifact/dependency contract");
        }
        // Interpret the already selected emission contract, never target/profile
        // predicates or graph ancestry to choose Cargo's compiler policy.
        let (metadata, link) = match self.generated_emission_args[0].as_str() {
            "--emit=dep-info,metadata" => (true, false),
            "--emit=dep-info,metadata,link" => (true, true),
            "--emit=dep-info,link" => (false, true),
            _ => unreachable!("policy was structurally validated"),
        };
        Ok(GeneratedCodegen {
            emission_args: &self.generated_emission_args,
            lto_args: &self.generated_lto_args,
            metadata,
            link,
        })
    }
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct Export {
    compiler_identity: CompilerIdentity,
    compile_context: CompileContext,
    rustflags: Vec<String>,
    rustdocflags: Vec<String>,
    extra_compiler_args: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generated_rustc_policy: Option<GeneratedRustcPolicy>,
}

impl Export {
    fn report(&self, graph_version: u32) -> Value {
        let mut value = json!(self);
        if graph_version == 3 {
            // Preserve explicit null for non-rustc units without changing v2 JSON.
            value["generated_rustc_policy"] = json!(self.generated_rustc_policy);
        }
        value
    }
}

fn exports(value: &Value, graph: &UnitGraph) -> eyre::Result<Vec<Export>> {
    value["units"]
        .as_array()
        .ok_or_else(|| eyre::eyre!("units must be an array"))?
        .iter()
        .zip(&graph.units)
        .enumerate()
        .map(|(i, (raw, unit))| {
            // Option fields must be explicitly present: absent is not the same
            // provenance assertion as Cargo's explicit null.
            if raw.get("extra_compiler_args").is_none()
                || raw["compiler_identity"].get("extra_filename").is_none()
            {
                bail!("unit {i} is missing required graph-v2 fields");
            }
            if graph.version == 3 && raw.get("generated_rustc_policy").is_none() {
                bail!("unit {i} is missing required graph-v3 generated policy");
            }
            if graph.version == 2 && raw.get("generated_rustc_policy").is_some() {
                bail!("unit {i} contains graph-v3 policy in a graph-v2 export");
            }
            let export: Export = serde_json::from_value(raw.clone())?;
            if graph.version == 3 {
                match (unit.mode, &export.generated_rustc_policy) {
                    (CompileMode::Build | CompileMode::Test | CompileMode::Check, Some(policy)) => {
                        policy.validate(unit.mode)?;
                    }
                    (
                        CompileMode::Doc
                        | CompileMode::Doctest
                        | CompileMode::Docscrape
                        | CompileMode::RunCustomBuild,
                        None,
                    ) => {}
                    _ => bail!("unit {i} has inconsistent generated policy presence for its mode"),
                }
            }
            let id = &export.compiler_identity;
            let valid_hash = |s: &str| {
                s.len() == 16
                    && s.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            };
            if !valid_hash(&id.c_metadata) || !valid_hash(&id.unit_id) {
                bail!("unit {i} has an invalid Cargo compiler identity");
            }
            if id
                .extra_filename
                .as_ref()
                .is_some_and(|s| s != &format!("-{}", id.unit_id))
            {
                bail!("unit {i} extra-filename disagrees with Cargo unit_id");
            }
            let testing = export.compile_context.rustc_test;
            if (unit.mode == CompileMode::Test && !testing)
                || (!matches!(unit.mode, CompileMode::Test | CompileMode::Check) && testing)
            {
                bail!("unit {i} has an inconsistent Cargo test role");
            }
            Ok(export)
        })
        .collect()
}

/// Accept only flag classes whose argument boundaries and effect on artifact
/// names/profile/identity are known here. Keep every other raw flag in the report.
fn checked_flags(flags: &[String]) -> eyre::Result<()> {
    let mut i = 0;
    while i < flags.len() {
        let flag = &flags[i];
        if flag == "--cfg" || flag == "--check-cfg" {
            if i + 1 == flags.len() || flags[i + 1].contains('\0') {
                bail!("incomplete or invalid resolved flag {flag}");
            }
            i += 2;
        } else if (flag.starts_with("--cfg=") || flag.starts_with("--check-cfg="))
            && !flag.contains('\0')
        {
            i += 1;
        } else {
            bail!("resolved flag {flag:?} needs unsupported ordering/output semantics");
        }
    }
    Ok(())
}

fn with_identity(mut plan: CompilerPlan, export: &Export) -> eyre::Result<CompilerPlan> {
    checked_flags(&export.rustflags)?;
    checked_flags(export.extra_compiler_args.as_deref().unwrap_or_default())?;
    codegen(
        &mut plan.rustc_args,
        "metadata",
        &export.compiler_identity.c_metadata,
    );
    if let Some(suffix) = &export.compiler_identity.extra_filename {
        codegen(&mut plan.rustc_args, "extra-filename", suffix);
        for artifact in plan
            .artifacts
            .iter_mut()
            .chain(&mut plan.auxiliary_artifacts)
        {
            artifact.file_name = if artifact.kind == ArtifactKind::Executable {
                format!("{}{suffix}", artifact.file_name)
            } else {
                let (stem, extension) = artifact.file_name.rsplit_once('.').unwrap();
                format!("{stem}{suffix}.{extension}")
            };
        }
    }
    // This remains a partial plan. Source/dependency paths and dynamic build
    // script flags have not been bound; never present it as a final command.
    // Pinned Cargo compiler/mod.rs appends extra_args_for before rustflags.
    plan.rustc_args
        .extend(export.extra_compiler_args.iter().flatten().cloned());
    plan.rustc_args.extend(export.rustflags.iter().cloned());
    Ok(plan)
}

struct ValidatedGraph {
    graph: UnitGraph,
    exports: Vec<Export>,
    script_dependencies: Vec<BTreeSet<usize>>,
    nondefault_lto: bool,
}

fn validate(
    value: &Value,
    harnesses: &BTreeMap<usize, Option<bool>>,
) -> eyre::Result<ValidatedGraph> {
    // Keep all v2 fields outside Unit. Its derived Hash is consumed by the
    // existing v1 backend; adding even None fields would change legacy IDs.
    let graph: UnitGraph = serde_json::from_value(value.clone())?;
    if !matches!(graph.version, 2 | 3) {
        bail!("expected graph version 2 or 3");
    }
    let script_dependencies = validate_structure(&graph)?;
    let exports = exports(value, &graph)?;
    for (i, supplied) in harnesses {
        let unit = graph
            .units
            .get(*i)
            .ok_or_else(|| eyre::eyre!("out-of-range harness index"))?;
        let context = &exports[*i].compile_context;
        if !matches!(unit.mode, CompileMode::Test | CompileMode::Check)
            || supplied != &context.rustc_test.then_some(context.harness)
        {
            bail!("harness sidecar conflicts with Cargo context for unit {i}");
        }
    }
    let nondefault_lto = graph.units.iter().any(|u| u.profile.lto != "false");
    Ok(ValidatedGraph {
        graph,
        exports,
        script_dependencies,
        nondefault_lto,
    })
}

impl ValidatedGraph {
    fn unit_plan(&self, i: usize, host: &str) -> eyre::Result<CompilerPlan> {
        if self.graph.version == 2 && self.nondefault_lto {
            bail!("graph has explicit LTO; effective per-unit bitcode/emit policy is not exported");
        }
        let unit = &self.graph.units[i];
        let export = &self.exports[i];
        let context = &export.compile_context;
        let test_context = match unit.mode {
            CompileMode::Check | CompileMode::Test => {
                Some(context.rustc_test.then_some(context.harness))
            }
            _ => None,
        };
        let source = match self.graph.version {
            2 => CodegenSource::Legacy,
            3 => CodegenSource::Cargo(
                export
                    .generated_rustc_policy
                    .as_ref()
                    .ok_or_else(|| {
                        eyre::eyre!("mode {:?} requires a separate execution owner", unit.mode)
                    })?
                    .codegen()?,
            ),
            _ => unreachable!("graph version was validated"),
        };
        compiler_plan_with_codegen(unit, host, test_context, source)
            .and_then(|plan| with_identity(plan, export))
    }
}

/// Restricted execution consumes the same validation and plan owner as inspection.
/// Source/tool/package binding is performed by the private bound-input constructor.
pub(crate) fn execution_plans(
    value: Value,
    host: &str,
) -> eyre::Result<(UnitGraph, Vec<CompilerPlan>)> {
    // This gate precedes the old planner and every backend/tool/store call.
    // Neither graph v2 nor the partial v3 policy can repair a missing recipe.
    if value.get("version").and_then(Value::as_u64) != Some(4) {
        bail!("bound execution requires complete graph schema 4; legacy plans are not a fallback");
    }
    recipe::inspect(&value, host)?;
    bail!(
        "modern execution remains closed pending complete recipe and binding parity; no units admitted"
    )
}

pub(super) fn graph_plan(
    value: Value,
    host: &str,
    harnesses: &BTreeMap<usize, Option<bool>>,
) -> eyre::Result<Value> {
    if value.get("version").and_then(Value::as_u64) == Some(4) {
        if !harnesses.is_empty() {
            bail!("schema 4 does not accept legacy harness sidecars");
        }
        return recipe::inspect(&value, host);
    }
    let validated = validate(&value, harnesses)?;
    let graph = &validated.graph;
    let units: Vec<_> = graph.units.iter().enumerate().map(|(i, unit)| {
        let (plan, unsupported) = match validated.unit_plan(i, host) {
            Ok(plan) => (Some(plan), None),
            Err(error) => (None, Some(error.to_string())),
        };
        json!({"index":i,"packageId":unit.pkg_id,"targetName":unit.target.name,"mode":unit.mode,"source":unit.target.src_path,"cargoUnit":validated.exports[i].report(graph.version),"plan":plan,"unsupported":unsupported,"buildScriptDependencies":validated.script_dependencies[i],"executionReady":false})
    }).collect();
    Ok(
        json!({"schemaVersion":graph.version,"inputGraphVersion":graph.version,"host":host,"units":units,"roots":graph.roots,"executionReady":false,"unresolvedInputs":["source/output/dependency path binding","Cargo/package/native environment and lint policy","build-script execution, generated OUT_DIR and directive outputs where required","Cargo/rustc/source/config provenance binding by the caller"],"identityMeaning":"Cargo-generated identities; raw user flags can override generated arguments. Unsupported flag classes are preserved but not applied.","rustdocIdentityMeaning":"Raw graph Doc/Docscrape identities are not the selected Check metadata used by Cargo rustdoc."}),
    )
}

/// Share graph-v2 identity, role, bounds and cycle validation with the compiler
/// planner; generated paths do not make previously unsupported execution valid.
pub(super) fn generated_plan(value: Value, host: &str) -> eyre::Result<Value> {
    generated::plan(value, host)
}

#[cfg(test)]
mod tests {
    use super::*;
    const HOST: &str = "x86_64-unknown-linux-gnu";

    fn fixture() -> Value {
        json!({"version":3,"roots":[0],"units":[{
            "pkg_id":"path+file:///fixture#example@0.1.0",
            "target":{"kind":["lib"],"crate_types":["rlib"],"name":"example-name",
                "src_path":"/fixture/src/lib.rs","edition":"2024","doc":true,"doctest":true,"test":true},
            "profile":{"name":"dev","opt_level":"0","lto":"thin","codegen_backend":null,
                "codegen_units":16,"debuginfo":0,"split_debuginfo":null,"debug_assertions":true,
                "overflow_checks":true,"rpath":false,"incremental":false,"panic":"unwind"},
            "platform":null,"mode":"build","features":[],"dependencies":[],
            "compiler_identity":{"c_metadata":"0000000000000001","unit_id":"0000000000000002",
                "extra_filename":"-0000000000000002"},
            "compile_context":{"rustc_test":false,"harness":true},
            "rustflags":[],"rustdocflags":[],"extra_compiler_args":null,
            "generated_rustc_policy":{"version":1,"generated_emission_args":["--emit=dep-info,metadata,link"],
                "generated_lto_args":["-C","linker-plugin-lto"]}
        }]})
    }

    fn unit_plan(graph: &Value) -> CompilerPlan {
        validate(graph, &BTreeMap::new())
            .unwrap()
            .unit_plan(0, HOST)
            .unwrap()
    }

    #[test]
    fn generated_lto_is_consumed_verbatim_without_legacy_generation() {
        for lto in [
            vec![],
            vec!["-C", "embed-bitcode=no"],
            vec!["-C", "linker-plugin-lto"],
            vec!["-C", "lto"],
            vec!["-C", "lto=thin"],
            vec!["-C", "lto=fat"],
            vec!["-C", "lto=off", "-C", "embed-bitcode=no"],
        ] {
            let mut graph = fixture();
            graph["units"][0]["generated_rustc_policy"]["generated_lto_args"] = json!(lto);
            let plan = unit_plan(&graph);
            let emit = plan
                .rustc_args
                .iter()
                .position(|a| a == "--emit=dep-info,metadata,link")
                .unwrap();
            let before = plan
                .rustc_args
                .iter()
                .position(|a| a == "opt-level=0")
                .unwrap()
                + 1;
            let after = plan
                .rustc_args
                .iter()
                .position(|a| a == "codegen-units=16")
                .unwrap()
                - 1;
            assert_eq!(&plan.rustc_args[before..after], lto.as_slice());
            assert!(emit < before);
            assert_eq!(
                plan.rustc_args
                    .iter()
                    .filter(|a| a.starts_with("--emit="))
                    .count(),
                1
            );
            assert_eq!(plan.auxiliary_artifacts[0].kind, ArtifactKind::DepInfo);
            assert_eq!(
                plan.auxiliary_artifacts[0].file_name,
                "example_name-0000000000000002.d"
            );
        }
    }

    #[test]
    fn generated_stages_follow_pinned_cargo_without_changing_legacy_order() {
        let mut graph = fixture();
        graph["units"][0]["target"]["crate_types"] = json!(["proc-macro"]);
        graph["units"][0]["profile"]["panic"] = json!("abort");
        graph["units"][0]["generated_rustc_policy"] = json!({"version":1,
            "generated_emission_args":["--emit=dep-info,link"],
            "generated_lto_args":["-C","embed-bitcode=no"]});
        let plan = unit_plan(&graph);
        let positions: Vec<_> = [
            "--emit=dep-info,link",
            "prefer-dynamic=yes",
            "opt-level=0",
            "panic=abort",
            "embed-bitcode=no",
            "codegen-units=16",
            "debuginfo=0",
        ]
        .iter()
        .map(|token| plan.rustc_args.iter().position(|arg| arg == token).unwrap())
        .collect();
        assert!(positions.windows(2).all(|p| p[0] < p[1]));
        graph["version"] = json!(2);
        graph["units"][0]["profile"]["lto"] = json!("false");
        graph["units"][0]
            .as_object_mut()
            .unwrap()
            .remove("generated_rustc_policy");
        let legacy = unit_plan(&graph);
        let positions: Vec<_> = [
            "--emit=link",
            "opt-level=0",
            "debuginfo=0",
            "embed-bitcode=no",
            "codegen-units=16",
            "panic=abort",
            "prefer-dynamic=yes",
        ]
        .iter()
        .map(|token| {
            legacy
                .rustc_args
                .iter()
                .position(|arg| arg == token)
                .unwrap()
        })
        .collect();
        assert!(positions.windows(2).all(|p| p[0] < p[1]));
    }

    #[test]
    fn dep_info_is_auxiliary_and_cargo_suffix_is_applied_once() {
        for types in [
            vec!["rlib"],
            vec!["rlib", "cdylib"],
            vec!["bin"],
            vec!["proc-macro"],
        ] {
            let mut graph = fixture();
            graph["units"][0]["target"]["crate_types"] = json!(types);
            if types != ["rlib"] {
                graph["units"][0]["generated_rustc_policy"]["generated_emission_args"] =
                    json!(["--emit=dep-info,link"]);
            }
            let plan = unit_plan(&graph);
            assert_eq!(plan.auxiliary_artifacts.len(), 1);
            assert_eq!(
                plan.auxiliary_artifacts[0].file_name,
                "example_name-0000000000000002.d"
            );
            assert!(
                !plan
                    .artifacts
                    .iter()
                    .any(|a| a.kind == ArtifactKind::DepInfo)
            );
            assert_eq!(
                plan.artifacts
                    .iter()
                    .any(|a| a.kind == ArtifactKind::Metadata),
                types == ["rlib"]
            );
            graph["units"][0]["compiler_identity"]["extra_filename"] = Value::Null;
            assert_eq!(
                unit_plan(&graph).auxiliary_artifacts[0].file_name,
                "example_name.d"
            );
        }
    }

    #[test]
    fn generated_policy_structure_and_tokens_are_strict() {
        let mut cases = Vec::new();
        let mut graph = fixture();
        graph["units"][0]
            .as_object_mut()
            .unwrap()
            .remove("generated_rustc_policy");
        cases.push(graph);
        for replacement in [
            Value::Null,
            json!({}),
            json!({"version":2,"generated_emission_args":["--emit=dep-info,metadata,link"],"generated_lto_args":[]}),
            json!({"version":1,"generated_emission_args":["--emit=dep-info,metadata,link"],"generated_lto_args":[],"extra":false}),
        ] {
            let mut graph = fixture();
            graph["units"][0]["generated_rustc_policy"] = replacement;
            cases.push(graph);
        }
        for (field, replacement) in [
            ("generated_lto_args", json!(["-C", "linker=/native/cc"])),
            (
                "generated_lto_args",
                json!(["-C", "embed-bitcode=no", "-C", "lto=off"]),
            ),
            (
                "generated_lto_args",
                json!(["-C", "lto=thin", "-C", "lto=thin"]),
            ),
            ("generated_lto_args", json!(["-C", "lto=thin\0"])),
            ("generated_lto_args", json!([1])),
            (
                "generated_emission_args",
                json!(["--emit=metadata,link,dep-info"]),
            ),
            (
                "generated_emission_args",
                json!(["--emit=dep-info,link", "--out-dir=/tmp/elsewhere"]),
            ),
            (
                "generated_emission_args",
                json!(["--emit=dep-info,metadata"]),
            ),
        ] {
            let mut graph = fixture();
            graph["units"][0]["generated_rustc_policy"][field] = replacement;
            cases.push(graph);
        }
        let mut graph = fixture();
        graph["units"][0]["mode"] = json!("test");
        graph["units"][0]["compile_context"]["rustc_test"] = json!(true);
        // Pinned Cargo always needs upstream objects in Test mode; it never
        // generates a metadata emission here, regardless of the target type.
        cases.push(graph);
        let mut graph = fixture();
        graph["version"] = json!(2);
        cases.push(graph);
        for graph in cases {
            assert!(validate(&graph, &BTreeMap::new()).is_err(), "{graph}");
        }
    }

    #[test]
    fn raw_order_and_existing_profile_restrictions_remain_explicit() {
        let mut graph = fixture();
        graph["units"][0]["extra_compiler_args"] = json!(["--cfg", "extra_first"]);
        graph["units"][0]["rustflags"] = json!(["--cfg", "rustflags_second"]);
        let plan = unit_plan(&graph);
        assert_eq!(
            &plan.rustc_args[plan.rustc_args.len() - 4..],
            ["--cfg", "extra_first", "--cfg", "rustflags_second"]
        );
        for flags in [
            json!(["-C", "linker=/native/cc"]),
            json!(["-C", "lto=off"]),
            json!(["--emit=link"]),
            json!(["-C", "metadata=other"]),
        ] {
            graph["units"][0]["rustflags"] = flags;
            assert!(
                validate(&graph, &BTreeMap::new())
                    .unwrap()
                    .unit_plan(0, HOST)
                    .is_err()
            );
        }
        for (key, value) in [
            ("incremental", json!(true)),
            ("rpath", json!(true)),
            ("codegen_backend", json!("cranelift")),
        ] {
            let mut graph = fixture();
            graph["units"][0]["profile"][key] = value;
            assert!(
                validate(&graph, &BTreeMap::new())
                    .unwrap()
                    .unit_plan(0, HOST)
                    .is_err()
            );
        }
        assert!(
            validate(&fixture(), &BTreeMap::new())
                .unwrap()
                .unit_plan(0, "aarch64-apple-darwin")
                .is_err()
        );
    }

    #[test]
    fn no_embed_metadata_is_reported_without_claiming_artifact_support() {
        let mut graph = fixture();
        let policy = json!({"version":1,"generated_emission_args":["--emit=dep-info,metadata,link","-Z","embed-metadata=no"],"generated_lto_args":[]});
        graph["units"][0]["generated_rustc_policy"] = policy.clone();
        let report = graph_plan(graph, HOST, &BTreeMap::new()).unwrap();
        assert!(report["units"][0]["plan"].is_null());
        assert!(
            report["units"][0]["unsupported"]
                .as_str()
                .unwrap()
                .contains("no-embed-metadata")
        );
        assert_eq!(
            report["units"][0]["cargoUnit"]["generated_rustc_policy"],
            policy
        );
    }

    #[test]
    fn test_check_contexts_null_nonrustc_and_execution_gate_are_preserved() {
        for mode in ["test", "check"] {
            let mut graph = fixture();
            graph["units"][0]["mode"] = json!(mode);
            graph["units"][0]["compile_context"]["rustc_test"] = json!(true);
            graph["units"][0]["generated_rustc_policy"]["generated_emission_args"] =
                if mode == "check" {
                    json!(["--emit=dep-info,metadata"])
                } else {
                    json!(["--emit=dep-info,link"])
                };
            assert!(unit_plan(&graph).rustc_args.contains(&"--test".into()));
            assert!(validate(&graph, &BTreeMap::from([(0, Some(false))])).is_err());
            assert!(
                execution_plans(graph, HOST)
                    .unwrap_err()
                    .to_string()
                    .contains("schema 4")
            );
        }
        for mode in ["doc", "doctest", "docscrape", "run-custom-build"] {
            let mut graph = fixture();
            graph["units"][0]["mode"] = json!(mode);
            assert!(validate(&graph, &BTreeMap::new()).is_err());
            graph["units"][0]["generated_rustc_policy"] = Value::Null;
            let report = graph_plan(graph, HOST, &BTreeMap::new()).unwrap();
            assert!(report["units"][0]["plan"].is_null());
            assert_eq!(
                report["units"][0]["cargoUnit"].get("generated_rustc_policy"),
                Some(&Value::Null)
            );
        }
        assert!(
            execution_plans(fixture(), HOST)
                .unwrap_err()
                .to_string()
                .contains("schema 4")
        );
        let mut legacy = fixture();
        legacy["version"] = json!(2);
        legacy["units"][0]
            .as_object_mut()
            .unwrap()
            .remove("generated_rustc_policy");
        assert!(
            graph_plan(legacy.clone(), HOST, &BTreeMap::new()).unwrap()["units"][0]["unsupported"]
                .as_str()
                .unwrap()
                .contains("LTO")
        );
        legacy["units"][0]["profile"]["lto"] = json!("false");
        let report = graph_plan(legacy, HOST, &BTreeMap::new()).unwrap();
        assert!(
            report["units"][0]["plan"]
                .get("auxiliaryArtifacts")
                .is_none()
        );
        assert!(
            report["units"][0]["cargoUnit"]
                .get("generated_rustc_policy")
                .is_none()
        );
    }
}

//! Pure generated-layout-v1 contract. Output selections are symbolic obligations
//! for a future backend, not existing Nix derivation inputs or execution support.
use super::{BTreeMap, BTreeSet, CompileMode, Value, bail, eyre, json, validate};
use crate::plan::ArtifactKind;

pub(super) fn plan(value: Value, host: &str) -> eyre::Result<Value> {
    let validated = validate(&value, &BTreeMap::new())?;
    let graph = &validated.graph;
    if host != "x86_64-unknown-linux-gnu" {
        bail!("generated-layout-v1 currently requires the x86_64 Linux host");
    }
    let mut roots = BTreeMap::new();
    let mut compiled = BTreeMap::new();
    for (i, unit) in graph.units.iter().enumerate() {
        if unit.mode != CompileMode::RunCustomBuild {
            continue;
        }
        if unit.target.kind != ["custom-build"] || unit.target.crate_types != ["bin"] {
            bail!("run unit {i} is not a custom-build executable target");
        }
        let id = &validated.exports[i].compiler_identity.unit_id;
        // Opaque Cargo identity owns the root. Never infer it from a compiler
        // unit, graph index, source path, Nix output or metadata disambiguator.
        if validated
            .exports
            .iter()
            .enumerate()
            .any(|(j, e)| i != j && e.compiler_identity.unit_id == *id)
        {
            bail!("run unit {i} identity aliases another graph context");
        }
        roots.insert(i, format!("/build/generated/v1/{id}/out"));
        let scripts: Vec<_> = unit
            .dependencies
            .iter()
            .filter_map(|dep| {
                let candidate = &graph.units[dep.index];
                (candidate.mode == CompileMode::Build
                    && candidate.pkg_id == unit.pkg_id
                    && candidate.target.kind == ["custom-build"]
                    && candidate.target.crate_types == ["bin"]
                    && candidate.target.name == unit.target.name
                    && candidate.target.src_path == unit.target.src_path)
                    .then_some(dep.index)
            })
            .collect();
        if scripts.len() != 1 {
            bail!("run unit {i} must identify exactly one compiled build script");
        }
        compiled.insert(i, scripts[0]);
    }
    let binding = |i: usize| {
        json!({
            "producerUnitIndex":i,
            "cargoRunUnitId":validated.exports[i].compiler_identity.unit_id,
            "logicalOutDir":roots[&i],
            "requiredOutputSelection":{"producerUnitIndex":i,"outputs":["OUT","FLAGS"]}
        })
    };
    let producers: Vec<_> = roots.iter().map(|(i, root)| {
        let compiled_index = compiled[i];
        let (executable, unsupported) = match validated.unit_plan(compiled_index, host) {
            Ok(plan) => {
                let executables: Vec<_> = plan.artifacts.iter()
                    .filter(|a| a.kind == ArtifactKind::Executable).collect();
                if executables.len() != 1 || plan.artifacts.len() != 1 {
                    (None, Some("compiled build script does not have exactly one executable".to_owned()))
                } else {
                    (Some(json!({"fileName":executables[0].file_name,"compilerPlan":plan})), None)
                }
            }
            Err(error) => (None, Some(error.to_string())),
        };
        json!({
            "unitIndex":i,"cargoRunUnitId":validated.exports[*i].compiler_identity.unit_id,
            "logicalOutDir":root,"compiledScriptUnitIndex":compiled_index,
            "compiledScriptExecutable":executable,"compiledScriptUnsupported":unsupported,
            "immutableOutputNames":["OUT","FLAGS"],
            "requiredPredecessorOutputs":validated.script_dependencies[*i].iter().copied().map(&binding).collect::<Vec<_>>(),
            "executionReady":false
        })
    }).collect();
    let mut consumers = Vec::new();
    for (i, unit) in graph.units.iter().enumerate() {
        let own_runs: BTreeSet<_> = unit
            .dependencies
            .iter()
            .filter_map(|dep| {
                let producer = &graph.units[dep.index];
                (producer.mode == CompileMode::RunCustomBuild && producer.pkg_id == unit.pkg_id)
                    .then_some(dep.index)
            })
            .collect();
        if own_runs.len() > 1 {
            bail!("unit {i} has ambiguous own build-script output");
        }
        let own = if unit.mode == CompileMode::RunCustomBuild {
            Some(i)
        } else {
            own_runs.iter().next().copied()
        };
        consumers.push(json!({
            "unitIndex":i,"mode":unit.mode,
            "ownOutDir":own.map(|j| &roots[&j]),
            "materializations":validated.script_dependencies[i].iter().copied().map(&binding).collect::<Vec<_>>()
        }));
    }
    Ok(json!({
        "schemaVersion":1,"inputGraphVersion":graph.version,"layout":"generated-layout-v1",
        "host":host,"producers":producers,"units":consumers,
        "executionReady":false,"nixInputsBound":false,
        "bindingMeaning":"Required symbolic OUT+FLAGS selections from the same producer; a backend must bind both to real Built inputs. No Nix graph is constructed here.",
        "materializationMeaning":"Conservative transitive generated tree availability; does not specify directive propagation or order. A run creates its own fresh writable root; predecessors must be materialized read-only before execution.",
        "identityMeaning":"Cargo run-unit identity determines only a logical path. Source/tool/recipe/environment and actual output contents remain dependency inputs; equal IDs do not authorize artifact or incremental reuse.",
        "unresolvedInputs":["hash-bound Cargo/rustc/source/config provenance","real producer OUT and FLAGS derivation output bindings","private generated tree materialization and export","build-script environment, typed directive roles and ordering","filesystem isolation and generated/runtime delivery"]
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    const HOST: &str = "x86_64-unknown-linux-gnu";

    fn unit(index: usize, mode: &str, package: &str, custom: bool, deps: &[usize]) -> Value {
        let id = format!("{:016x}", index + 256);
        json!({
            "pkg_id":format!("path+file:///fixture/{package}#{package}@0.1.0"),
            "target":{"kind":[if custom {"custom-build"} else {"lib"}],
                "crate_types":[if custom {"bin"} else {"rlib"}],
                "name":if custom {"build-script-build"} else {package},
                "src_path":format!("/fixture/{package}/{}",if custom {"build.rs"} else {"src/lib.rs"}),
                "edition":"2024","doc":false,"doctest":false,"test":false},
            "profile":{"name":"dev","opt_level":"0","lto":"false","codegen_backend":null,
                "codegen_units":16,"debuginfo":0,"split_debuginfo":null,"debug_assertions":true,
                "overflow_checks":true,"rpath":false,"incremental":false,"panic":"unwind",
                "strip":{"resolved":{"Named":"none"}}},
            "platform":null,"mode":mode,"features":[],
            "dependencies":deps.iter().map(|i|json!({"index":i,"extern_crate_name":"fixture"})).collect::<Vec<_>>(),
            "compiler_identity":{"c_metadata":format!("{:016x}",index + 1),"unit_id":id,"extra_filename":format!("-{id}")},
            "compile_context":{"rustc_test":false,"harness":false},
            "rustflags":[],"rustdocflags":[],"extra_compiler_args":null
        })
    }
    fn fixture() -> Value {
        json!({"version":2,"units":[
            unit(0,"build","a",true,&[]),unit(1,"run-custom-build","a",true,&[0]),
            unit(2,"build","b",true,&[]),unit(3,"run-custom-build","b",true,&[2,1]),
            unit(4,"build","b",false,&[3]),unit(5,"build","c",false,&[4])
        ],"roots":[5]})
    }
    fn root(index: usize) -> String {
        format!("/build/generated/v1/{:016x}/out", index + 256)
    }
    fn set_id(graph: &mut Value, index: usize, id: &str) {
        graph["units"][index]["compiler_identity"]["unit_id"] = json!(id);
        graph["units"][index]["compiler_identity"]["extra_filename"] = json!(format!("-{id}"));
    }

    #[test]
    fn roots_use_run_identity_and_script_executable_uses_compile_identity() {
        let result = plan(fixture(), HOST).unwrap();
        assert_eq!(result["producers"][0]["logicalOutDir"], root(1));
        assert_eq!(result["producers"][1]["logicalOutDir"], root(3));
        assert_eq!(result["producers"][0]["compiledScriptUnitIndex"], 0);
        assert_eq!(
            result["producers"][0]["compiledScriptExecutable"]["fileName"],
            "build_script_build-0000000000000100"
        );
        assert_eq!(result["units"][1]["ownOutDir"], root(1));
        assert_eq!(result["units"][4]["ownOutDir"], root(3));
        assert!(result["units"][5]["ownOutDir"].is_null());
        assert_eq!(result["executionReady"], false);
        assert_eq!(result["nixInputsBound"], false);
    }

    #[test]
    fn transitive_closure_requires_both_outputs_from_each_actual_producer() {
        let result = plan(fixture(), HOST).unwrap();
        assert_eq!(
            result["producers"][0]["requiredPredecessorOutputs"],
            json!([])
        );
        let prior = result["producers"][1]["requiredPredecessorOutputs"]
            .as_array()
            .unwrap();
        assert_eq!(prior.len(), 1);
        assert_eq!(prior[0]["producerUnitIndex"], 1);
        for consumer in [4, 5] {
            let bindings = result["units"][consumer]["materializations"]
                .as_array()
                .unwrap();
            assert_eq!(bindings.len(), 2);
            for (binding, producer) in bindings.iter().zip([1, 3]) {
                assert_eq!(binding["logicalOutDir"], root(producer));
                assert_eq!(
                    binding["requiredOutputSelection"],
                    json!({"producerUnitIndex":producer,"outputs":["OUT","FLAGS"]})
                );
            }
        }
        // Own outputs are never predecessor inputs, avoiding a recipe cycle.
        assert!(
            result["units"][1]["materializations"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            result["units"][3]["materializations"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn compile_identity_and_source_changes_do_not_replace_run_identity_or_edges() {
        let before = plan(fixture(), HOST).unwrap();
        let mut graph = fixture();
        set_id(&mut graph, 0, "000000000000ffff");
        for unit in graph["units"].as_array_mut().unwrap() {
            unit["target"]["src_path"] = json!(
                unit["target"]["src_path"]
                    .as_str()
                    .unwrap()
                    .replace("/fixture/", "/changed-source/")
            );
        }
        let after = plan(graph, HOST).unwrap();
        assert_eq!(before["units"], after["units"]);
        assert_eq!(after["producers"][0]["logicalOutDir"], root(1));
        assert_eq!(
            after["producers"][0]["compiledScriptExecutable"]["fileName"],
            "build_script_build-000000000000ffff"
        );
        // Identical layout is deliberately not a content/cache identity.
        assert_eq!(after["nixInputsBound"], false);
        assert!(!after["unresolvedInputs"].as_array().unwrap().is_empty());
    }

    #[test]
    fn unsupported_compiler_semantics_remain_explicit() {
        let mut graph = fixture();
        graph["units"][0]["profile"]["lto"] = json!("thin");
        let result = plan(graph, HOST).unwrap();
        assert_eq!(result["producers"][0]["logicalOutDir"], root(1));
        assert!(result["producers"][0]["compiledScriptExecutable"].is_null());
        assert!(
            result["producers"][0]["compiledScriptUnsupported"]
                .as_str()
                .unwrap()
                .contains("LTO")
        );
        assert_eq!(result["executionReady"], false);
        assert!(crate::plan::execution_plans(fixture(), HOST).is_err());
    }

    #[test]
    fn v3_root_lto_does_not_override_exported_script_policy_or_layout() {
        let original = plan(fixture(), HOST).unwrap();
        let mut graph = fixture();
        graph["version"] = json!(3);
        for unit in graph["units"].as_array_mut().unwrap() {
            unit["generated_rustc_policy"] = if unit["mode"] == "run-custom-build" {
                Value::Null
            } else {
                json!({"version":1,"generated_emission_args":["--emit=dep-info,link"],
                    "generated_lto_args":["-C","embed-bitcode=no"]})
            };
        }
        graph["units"][5]["profile"]["lto"] = json!("thin");
        let result = plan(graph, HOST).unwrap();
        assert_eq!(result["inputGraphVersion"], 3);
        assert_eq!(result["units"], original["units"]);
        for (i, producer) in result["producers"].as_array().unwrap().iter().enumerate() {
            assert_eq!(
                producer["logicalOutDir"],
                original["producers"][i]["logicalOutDir"]
            );
            assert!(producer["compiledScriptUnsupported"].is_null());
            let plan = &producer["compiledScriptExecutable"]["compilerPlan"];
            assert_eq!(plan["artifacts"].as_array().unwrap().len(), 1);
            assert_eq!(plan["auxiliaryArtifacts"][0]["kind"], "dep-info");
            assert_eq!(
                plan["auxiliaryArtifacts"][0]["fileName"],
                format!(
                    "{}.d",
                    producer["compiledScriptExecutable"]["fileName"]
                        .as_str()
                        .unwrap()
                )
            );
        }
        assert_eq!(result["executionReady"], false);
        assert_eq!(result["nixInputsBound"], false);
    }

    #[test]
    fn malformed_identity_role_and_dependency_graphs_are_rejected() {
        let mut cases = Vec::new();
        let mut graph = fixture();
        set_id(&mut graph, 1, "../escape");
        cases.push(("invalid id", graph));
        let mut graph = fixture();
        set_id(&mut graph, 3, "0000000000000101");
        cases.push(("duplicate run id", graph));
        let mut graph = fixture();
        set_id(&mut graph, 1, "0000000000000100");
        cases.push(("compile id substituted", graph));
        let mut graph = fixture();
        graph["units"][1]["compiler_identity"]
            .as_object_mut()
            .unwrap()
            .remove("unit_id");
        cases.push(("missing id", graph));
        let mut graph = fixture();
        graph["units"][1]["target"]["kind"] = json!(["lib"]);
        cases.push(("wrong run role", graph));
        let mut graph = fixture();
        graph["units"][1]["target"]["crate_types"] = json!(["bin", "rlib"]);
        cases.push(("mixed run outputs", graph));
        let mut graph = fixture();
        graph["units"][1]["dependencies"] = json!([]);
        cases.push(("missing executable", graph));
        let mut graph = fixture();
        graph["units"][0]["pkg_id"] = json!("other");
        cases.push(("wrong script package", graph));
        let mut graph = fixture();
        graph["units"][1]["dependencies"]
            .as_array_mut()
            .unwrap()
            .push(json!({"index":0,"extern_crate_name":"duplicate"}));
        cases.push(("ambiguous executable", graph));
        let mut graph = fixture();
        graph["units"][0]["dependencies"] = json!([{"index":1,"extern_crate_name":"cycle"}]);
        cases.push(("cycle", graph));
        let mut graph = fixture();
        graph["units"][1]["dependencies"][0]["index"] = json!(999);
        cases.push(("out of bounds", graph));
        let mut graph = fixture();
        graph["version"] = json!(1);
        cases.push(("legacy graph", graph));
        for (name, graph) in cases {
            assert!(plan(graph, HOST).is_err(), "{name}");
        }
        assert!(plan(fixture(), "aarch64-unknown-linux-gnu").is_err());
    }
}

use super::*;

const PACKAGE: &str = "path+file:///fixture#sample@0.1.0";

fn fixture() -> (Value, Value) {
    let graph = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/one-unit.json"
    )))
    .unwrap();
    let bindings = json!({
        "version":1,"platform":"x86_64-unknown-linux-gnu","workspace_root":b"/fixture",
        "packages":[{"id":PACKAGE,"root":b"/fixture","manifest":b"/fixture/Cargo.toml"}],
        "tools":{"rustc":b"/tool/rustc","cargo":b"/tool/cargo"},
        "units":[{"unit":0,"output_directory":b"/target/deps","dependency_directory":b"/target/deps","host_dependency_directory":b"/target/deps","delivery_directory":b"/target","preparation_dylib_components":[]}],
        "compilation_search_directories":[{"platform":null,"paths":[b"/target/deps"]}],
        "export_directory":b"/export"
    });
    (graph, bindings)
}

fn recipe(graph: &mut Value) -> &mut Value {
    &mut graph["units"][0]["resolved_rustc_recipe"]["recipe"]
}

#[test]
fn native_binding_preserves_words_relative_source_and_environment_operations() {
    let (mut graph, bindings) = fixture();
    recipe(&mut graph)["arguments"]
        .as_array_mut()
        .unwrap()
        .push(json!({"parts":[{"literal":[255,61,32]}]}));
    let inherited = BTreeMap::from([
        (b"UNSET".to_vec(), b"remove".to_vec()),
        (b"EMPTY".to_vec(), b"replace".to_vec()),
        (b"UNCHANGED".to_vec(), vec![255]),
        (b"CARGO_MAKEFLAGS".to_vec(), b"old capability".to_vec()),
    ]);
    let bound = bind(&graph, &bindings, &inherited).unwrap();
    assert_eq!(bound.roots, [0]);
    let command = &bound.commands[0];
    assert_eq!(command.program, b"/tool/rustc");
    assert_eq!(
        command.arguments,
        [
            b"--crate-name".to_vec(),
            b"sample".to_vec(),
            b"src/lib.rs".to_vec(),
            vec![255, 61, 32]
        ]
    );
    assert_eq!(command.working_directory, b"/fixture");
    assert_eq!(command.dep_info, b"/target/deps/sample.d");
    assert_eq!(command.outputs[0].path, b"/target/deps/libsample.rlib");
    assert_eq!(command.environment_overlay["UNSET"], None);
    assert_eq!(command.environment_overlay["EMPTY"], Some(Vec::new()));
    let effective = bound.environment_before_jobserver(0).unwrap();
    assert!(!effective.contains_key(b"UNSET".as_slice()));
    assert_eq!(effective[b"EMPTY".as_slice()], b"");
    assert_eq!(effective[b"UNCHANGED".as_slice()], [255]);
    // Deliberately unchanged until the live runtime capability applies it.
    assert_eq!(effective[b"CARGO_MAKEFLAGS".as_slice()], b"old capability");
    assert_eq!(bound.inherited_environment, inherited);
    assert!(bound.environment_before_jobserver(1).is_err());
}

#[test]
fn native_binding_retains_sampled_dylib_empty_relative_repeated_and_byte_components() {
    let (mut graph, mut bindings) = fixture();
    let components = vec![
        vec![],
        b"relative".to_vec(),
        vec![b'/', 255],
        b"relative".to_vec(),
    ];
    bindings["units"][0]["preparation_dylib_components"] = json!(components);
    recipe(&mut graph)["environment"]["preparation_dylib_path"]["components"] = json!(4);
    recipe(&mut graph)["environment"]["overlay"]["LD_LIBRARY_PATH"] = json!({"set":{"path_list":(0..4).map(|index| json!({"kind":"preparation-dylib-component","index":index})).collect::<Vec<_>>()}});
    let bound = bind(&graph, &bindings, &BTreeMap::new()).unwrap();
    assert_eq!(
        bound.commands[0].environment_overlay["LD_LIBRARY_PATH"],
        Some([b":relative:/".as_slice(), &[255], b":relative"].concat())
    );
    bindings["units"][0]["preparation_dylib_components"][0] = json!(b"/contains:separator");
    assert!(bind(&graph, &bindings, &BTreeMap::new()).is_err());
}

#[test]
fn native_binding_uses_declared_dependency_output_and_delivery_roles() {
    let (mut graph, mut bindings) = fixture();
    let mut producer = graph["units"][0].clone();
    producer["resolved_rustc_recipe"]["recipe"]["unit"] = json!(1);
    producer["resolved_rustc_recipe"]["recipe"]["arguments"][2]["parts"][0]["path"]["unit"] =
        json!(1);
    producer["resolved_rustc_recipe"]["recipe"]["outputs"][0]["path"]["unit"] = json!(1);
    producer["resolved_rustc_recipe"]["recipe"]["dep_info"]["unit"] = json!(1);
    graph["units"].as_array_mut().unwrap().push(producer);
    graph["units"][0]["dependencies"] = json!([{"index":1,"extern_crate_name":"dep"}]);
    let mut producer_binding = bindings["units"][0].clone();
    producer_binding["unit"] = json!(1);
    producer_binding["output_directory"] = json!(b"/producer/deps");
    bindings["units"]
        .as_array_mut()
        .unwrap()
        .push(producer_binding);
    recipe(&mut graph)["arguments"].as_array_mut().unwrap().extend([
        json!({"parts":[{"literal":b"dep="},{"path":{"kind":"artifact","unit":1,"output":0}}]}),
        json!({"parts":[{"path":{"kind":"package-path","package":PACKAGE,"relative":b"src/lib.rs"}}]}),
        json!({"parts":[{"path":{"kind":"tool","binding":"cargo"}}]}),
    ]);
    recipe(&mut graph)["outputs"][0]["hardlink"] =
        json!({"kind":"delivery-path","unit":0,"relative":b"libsample.rlib"});
    recipe(&mut graph)["outputs"][0]["export"] =
        json!({"kind":"export-path","relative":b"libsample.rlib"});
    let bound = bind(&graph, &bindings, &BTreeMap::new()).unwrap();
    let command = &bound.commands[0];
    assert_eq!(command.arguments[3], b"dep=/producer/deps/libsample.rlib");
    assert_eq!(command.arguments[4], b"/fixture/src/lib.rs");
    assert_eq!(command.arguments[5], b"/tool/cargo");
    assert_eq!(
        command.outputs[0].hardlink.as_deref(),
        Some(b"/target/libsample.rlib".as_slice())
    );
    assert_eq!(
        command.outputs[0].export.as_deref(),
        Some(b"/export/libsample.rlib".as_slice())
    );
    for field in ["delivery_directory", "export_directory"] {
        let mut invalid = bindings.clone();
        if field == "export_directory" {
            invalid[field] = Value::Null;
        } else {
            invalid["units"][0][field] = Value::Null;
        }
        assert!(bind(&graph, &invalid, &BTreeMap::new()).is_err());
    }
}

#[test]
fn native_binding_rejects_incomplete_ambiguous_and_non_native_bindings() {
    let (graph, bindings) = fixture();
    for (pointer, invalid) in [
        ("/version", json!(2)),
        ("/platform", json!("aarch64-unknown-linux-gnu")),
        ("/workspace_root", json!(b"relative")),
        ("/tools/rustc", json!([47, 0])),
        ("/packages", json!([])),
        ("/packages/0/id", json!("unknown")),
        ("/units", json!([])),
        ("/units/0/unit", json!(1)),
        ("/units/0/preparation_dylib_components", json!([[]])),
        ("/compilation_search_directories/0/platform", json!("other")),
    ] {
        let mut invalid_bindings = bindings.clone();
        *invalid_bindings.pointer_mut(pointer).unwrap() = invalid;
        assert!(
            bind(&graph, &invalid_bindings, &BTreeMap::new()).is_err(),
            "accepted {pointer}"
        );
    }
    for field in ["packages", "units", "compilation_search_directories"] {
        let mut invalid = bindings.clone();
        let duplicate = invalid[field][0].clone();
        invalid[field].as_array_mut().unwrap().push(duplicate);
        assert!(bind(&graph, &invalid, &BTreeMap::new()).is_err());
    }
    let mut unknown = bindings;
    unknown["ambient_environment"] = json!(true);
    assert!(bind(&graph, &unknown, &BTreeMap::new()).is_err());
}

#[test]
fn native_binding_requires_valid_explicit_environment() {
    let (graph, bindings) = fixture();
    for (name, value) in [
        (b"".as_slice(), b"v".as_slice()),
        (b"A=B", b"v"),
        (b"A\0", b"v"),
        (b"A", b"v\0"),
    ] {
        assert!(
            bind(
                &graph,
                &bindings,
                &BTreeMap::from([(name.to_vec(), value.to_vec())])
            )
            .is_err()
        );
    }
    let bound = bind(&graph, &bindings, &BTreeMap::new()).unwrap();
    assert!(bound.inherited_environment.is_empty());
    assert_eq!(bound.environment_before_jobserver(0).unwrap().len(), 1);
}

#[test]
fn native_binding_rejects_refusal_and_never_uses_legacy_policy() {
    let (mut graph, bindings) = fixture();
    graph["units"][0]["resolved_rustc_recipe"] =
        json!({"status":"unsupported","reason":"build script requires runtime owner"});
    graph["units"][0]["generated_rustc_policy"] =
        json!({"version":1,"generated_emission_args":["--emit=link"],"generated_lto_args":[]});
    assert!(inspect(&graph, "x86_64-unknown-linux-gnu").is_ok());
    assert!(bind(&graph, &bindings, &BTreeMap::new()).is_err());
}

#[test]
fn native_binding_rejects_dependency_cycles_and_unbound_search_paths() {
    let (mut graph, bindings) = fixture();
    graph["units"][0]["dependencies"] = json!([{"index":0,"extern_crate_name":"self"}]);
    assert!(bind(&graph, &bindings, &BTreeMap::new()).is_err());
    let (mut graph, mut bindings) = fixture();
    recipe(&mut graph)["arguments"][0] =
        json!({"path_list":[{"kind":"compilation-search-directory","platform":null,"index":0}]});
    bindings["compilation_search_directories"] = json!([]);
    assert!(bind(&graph, &bindings, &BTreeMap::new()).is_err());
}

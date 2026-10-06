use super::*;
use serde_json::{Value, json};

const HOST: &str = "x86_64-unknown-linux-gnu";

// A contract-shape fixture, not expected rustc arguments or a compiler oracle.
fn fixture() -> Value {
    serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../compiler-recipe/tests/fixtures/one-unit.json"
    )))
    .unwrap()
}

#[test]
fn schema4_inspection_keeps_binding_and_execution_gates_closed() {
    let graph = fixture();
    let inspection = inspect(&graph, HOST).unwrap();
    assert_eq!(inspection["executionReady"], false);
    assert_eq!(inspection["bindingsValidated"], false);
    assert!(
        super::super::execution_plans(graph.clone(), HOST)
            .unwrap_err()
            .to_string()
            .contains("remains closed")
    );
    for version in [1, 2, 3, 5] {
        let mut graph = graph.clone();
        graph["version"] = json!(version);
        assert!(super::super::execution_plans(graph, HOST).is_err());
    }
    let mut missing = graph.clone();
    missing["units"][0]
        .as_object_mut()
        .unwrap()
        .remove("resolved_rustc_recipe");
    assert!(inspect(&missing, HOST).is_err());
    let mut version = graph.clone();
    version["units"][0]["resolved_rustc_recipe"]["recipe"]["version"] = json!(2);
    assert!(inspect(&version, HOST).is_err());
    let mut supplied = graph;
    supplied["recipe_input_bindings"] =
        json!({"execution-environment":{"TOKEN":"not-a-real-credential"}});
    assert!(
        inspect(&supplied, HOST)
            .unwrap_err()
            .to_string()
            .contains("bindings are not admitted")
    );
}

#[test]
fn schema4_rejects_wrong_path_roles_owners_and_ambiguous_words() {
    let cases = [
        ("/cwd", json!({"kind":"artifact","unit":0,"output":0})),
        ("/cwd", json!({"kind":"package-root","package":"unknown"})),
        (
            "/outputs/0/hardlink",
            json!({"kind":"tool","binding":"cargo"}),
        ),
        (
            "/outputs/0/hardlink",
            json!({"kind":"delivery-path","unit":1,"relative":[120]}),
        ),
        (
            "/outputs/0/export",
            json!({"kind":"output-directory","unit":0}),
        ),
        (
            "/outputs/0/path",
            json!({"kind":"output-path","unit":1,"relative":[120]}),
        ),
        (
            "/dep_info",
            json!({"kind":"output-path","unit":0,"relative":[46,46,47,120]}),
        ),
        (
            "/arguments/0",
            json!({"parts":[{"literal":[120]}],"path_list":[]}),
        ),
        ("/arguments/0", json!({"parts":[{"literal":[0]}]})),
        (
            "/arguments/0",
            json!({"parts":[{"path":{"kind":"dependency-directory","unit":1}}]}),
        ),
        (
            "/arguments/0",
            json!({"parts":[{"path":{"kind":"preparation-dylib-component","index":0}}]}),
        ),
        ("/environment/overlay/UNSET", json!({"unset":false})),
        ("/capabilities/executor_io/stdin", json!("inherit")),
    ];
    for (pointer, invalid) in cases {
        let mut graph = fixture();
        *graph["units"][0]["resolved_rustc_recipe"]["recipe"]
            .pointer_mut(pointer)
            .unwrap() = invalid;
        assert!(
            inspect(&graph, HOST).is_err(),
            "accepted {pointer}: {graph}"
        );
    }
    for (field, value) in [
        ("is_std", json!(true)),
        ("rustflags", json!(["-Lnative=/external"])),
        ("mode", json!("check")),
    ] {
        let mut graph = fixture();
        graph["units"][0][field] = value;
        assert!(inspect(&graph, HOST).is_err());
    }
}

#[test]
fn schema4_preserves_explicit_refusal_without_using_partial_policy() {
    let mut graph = fixture();
    graph["units"][0]["resolved_rustc_recipe"] =
        json!({"status":"unsupported","reason":"native-only input"});
    graph["units"][0]["generated_rustc_policy"] =
        json!({"version":1,"generated_emission_args":["--emit=link"],"generated_lto_args":[]});
    assert_eq!(
        inspect(&graph, HOST).unwrap()["units"][0]["resolvedRustcRecipe"]["status"],
        "unsupported"
    );
    assert!(super::super::execution_plans(graph, HOST).is_err());
}

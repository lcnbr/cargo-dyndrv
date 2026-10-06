//! Strict schema-4/recipe-v1 decoding shared by inspection and native binding.
//! Backend admission still requires complete native argv/env/artifact parity.

use crate::graph::{CompileMode, UnitGraph, validate_structure};
use eyre::{self, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[path = "native.rs"]
pub(crate) mod native;

#[derive(Debug, Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case", deny_unknown_fields)]
enum Resolution {
    Resolved { recipe: Box<Recipe> },
    Unsupported { reason: String },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Recipe {
    version: u32,
    platform: String,
    unit: usize,
    program: Binding,
    arguments: Vec<Word>,
    cwd: PathRef,
    environment: Environment,
    capabilities: Capabilities,
    outputs: Vec<Output>,
    dep_info: PathRef,
    binding_scope: String,
    binding_status: String,
    execution_admitted: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    binding: String,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Word {
    Parts(Parts),
    PathList(PathList),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Parts {
    parts: Vec<Part>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PathList {
    path_list: Vec<PathRef>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Part {
    Literal(Literal),
    Path(PathPart),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Literal {
    literal: Vec<u8>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PathPart {
    path: PathRef,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum PathRef {
    RelativeSource {
        unit: usize,
        bytes: Vec<u8>,
    },
    PackagePath {
        package: String,
        relative: Vec<u8>,
    },
    WorkspaceRoot,
    PackageRoot {
        package: String,
    },
    PackageManifest {
        package: String,
    },
    Tool {
        binding: String,
    },
    OutputDirectory {
        unit: usize,
    },
    Artifact {
        unit: usize,
        output: usize,
    },
    DependencyDirectory {
        unit: usize,
    },
    HostDependencyDirectory {
        unit: usize,
    },
    CompilationSearchDirectory {
        platform: Option<String>,
        index: usize,
    },
    PreparationDylibComponent {
        index: usize,
    },
    OutputPath {
        unit: usize,
        relative: Vec<u8>,
    },
    DeliveryPath {
        unit: usize,
        relative: Vec<u8>,
    },
    ExportPath {
        relative: Vec<u8>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Environment {
    inherited: InheritedEnvironment,
    preparation_dylib_path: PreparationDylibPath,
    overlay: BTreeMap<String, EnvironmentAction>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InheritedEnvironment {
    binding: String,
    sample: String,
    policy: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparationDylibPath {
    binding: String,
    sample: String,
    variable: String,
    components: usize,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum EnvironmentAction {
    Set(Set),
    Unset(Unset),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Set {
    set: Word,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Unset {
    unset: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Capabilities {
    jobserver: Jobserver,
    retry_with_argfile: bool,
    executor_io: ExecutorIo,
    arg0: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecutorIo {
    stdin: String,
    stdout: String,
    stderr: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Jobserver {
    binding: String,
    environment_after_overlay: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Output {
    index: usize,
    path: PathRef,
    flavor: String,
    hardlink: Option<PathRef>,
    export: Option<PathRef>,
    presence: String,
}

struct Validated {
    graph: UnitGraph,
    recipes: Vec<Resolution>,
}

fn validated(value: &Value, host: &str) -> eyre::Result<Validated> {
    if value.get("recipe_input_bindings").is_some() {
        bail!(
            "recipe input bindings are not admitted: the effective-environment validation boundary is closed"
        );
    }
    let graph: UnitGraph = serde_json::from_value(value.clone())?;
    if graph.version != 4 || host != "x86_64-unknown-linux-gnu" {
        bail!("complete recipes require graph schema 4 and x86_64 Linux");
    }
    let _scripts = validate_structure(&graph)?;
    let raw_units = value["units"]
        .as_array()
        .ok_or_else(|| eyre::eyre!("missing units"))?;
    let recipes: Vec<_> = raw_units
        .iter()
        .map(|unit| {
            let raw = unit
                .get("resolved_rustc_recipe")
                .ok_or_else(|| eyre::eyre!("missing complete recipe or explicit refusal"))?;
            serde_json::from_value::<Resolution>(raw.clone()).map_err(Into::into)
        })
        .collect::<eyre::Result<_>>()?;
    if recipes
        .iter()
        .any(|resolution| matches!(resolution, Resolution::Resolved { .. }))
    {
        for (raw, unit) in raw_units.iter().zip(&graph.units) {
            if raw["mode"] != "build"
                || !unit.pkg_id.starts_with("path+")
                || unit.profile.incremental
                || unit.profile.codegen_backend.is_some()
                || unit.platform.is_some()
                || unit.target.crate_types.len() != 1
                || !matches!(
                    unit.target.crate_types[0].as_str(),
                    "lib" | "rlib" | "bin" | "proc-macro"
                )
                || raw.get("is_std").is_some_and(|v| v != false)
                || raw["rustflags"] != json!([])
                || raw
                    .get("extra_compiler_args")
                    .is_none_or(|args| !args.is_null())
                || raw["target"]["kind"].as_array().is_none_or(|kinds| {
                    kinds.len() != 1
                        || !matches!(
                            kinds[0].as_str(),
                            Some("lib" | "rlib" | "bin" | "proc-macro")
                        )
                })
            {
                bail!("resolved recipe graph contains unsupported raw roles or flags");
            }
        }
    }
    for (index, resolution) in recipes.iter().enumerate() {
        match resolution {
            Resolution::Unsupported { reason } if reason.is_empty() => {
                bail!("empty recipe refusal")
            }
            Resolution::Unsupported { .. } => {}
            Resolution::Resolved { recipe } => {
                recipe.validate(index, &graph, &recipes)?;
            }
        }
    }
    Ok(Validated { graph, recipes })
}

pub fn inspect(value: &Value, host: &str) -> eyre::Result<Value> {
    let Validated { graph, .. } = validated(value, host)?;
    let raw_units = value["units"].as_array().expect("validated units");
    Ok(
        json!({"schemaVersion":4,"inputGraphVersion":4,"recipeVersion":1,
        "commandAuthority":"resolved_rustc_recipe only; generated_rustc_policy is inspection-only",
        "units":raw_units.iter().enumerate().map(|(index, unit)| json!({"index":index,"resolvedRustcRecipe":unit["resolved_rustc_recipe"],"executionReady":false})).collect::<Vec<_>>(),
        "roots":graph.roots,"executionReady":false,"bindingsValidated":false,
        "executionGate":"closed pending complete ordered argv, effective child environment, artifact, and binding equivalence"}),
    )
}

impl Recipe {
    fn validate(
        &self,
        index: usize,
        graph: &UnitGraph,
        recipes: &[Resolution],
    ) -> eyre::Result<()> {
        let unit = &graph.units[index];
        if self.version != 1
            || self.platform != "x86_64-unknown-linux-gnu"
            || self.unit != index
            || self.execution_admitted
            || self.program.binding != "rustc"
            || self.binding_scope != "native-path-and-environment-equivalence-required"
            || self.binding_status != "requires-validated-inputs"
            || unit.mode != CompileMode::Build
            || unit.profile.incremental
            || unit.profile.codegen_backend.is_some()
            || unit.platform.is_some()
            || unit.target.crate_types.len() != 1
            || !matches!(
                unit.target.crate_types[0].as_str(),
                "lib" | "rlib" | "bin" | "proc-macro"
            )
        {
            bail!(
                "unit {index} has unsupported recipe version, role, binding, or compiler settings"
            );
        }
        let inherited = &self.environment.inherited;
        let dylib = &self.environment.preparation_dylib_path;
        let capabilities = &self.capabilities;
        if inherited.binding != "execution-environment"
            || inherited.sample != "execution"
            || inherited.policy != "credential-free-explicit-binding-v1"
            || dylib.binding != "preparation-dylib-path"
            || dylib.sample != "preparation"
            || dylib.variable != "LD_LIBRARY_PATH"
            || capabilities.jobserver.binding != "cargo-jobserver"
            || capabilities.jobserver.environment_after_overlay != ["CARGO_MAKEFLAGS"]
            || !capabilities.retry_with_argfile
            || capabilities.arg0 != "program"
            || capabilities.executor_io.stdin != "null"
            || capabilities.executor_io.stdout != "streamed"
            || capabilities.executor_io.stderr != "streamed"
        {
            bail!("unit {index} has unsupported environment sampling or live capabilities");
        }
        let check_path = |path: &PathRef| path.validate(index, graph, recipes, dylib.components);
        match &self.cwd {
            PathRef::WorkspaceRoot => {}
            PathRef::PackageRoot { package } if package == &unit.pkg_id => {}
            _ => bail!("compiler cwd has an invalid path role or package"),
        }
        check_path(&self.cwd)?;
        for word in &self.arguments {
            word.validate(&check_path)?;
        }
        for (name, action) in &self.environment.overlay {
            if name.is_empty() || name.contains(['=', '\0']) {
                bail!("invalid environment name");
            }
            match action {
                EnvironmentAction::Set(value) => value.set.validate(&check_path)?,
                EnvironmentAction::Unset(value) if !value.unset => bail!("invalid unset action"),
                EnvironmentAction::Unset(_) => {}
            }
        }
        if self.outputs.is_empty() {
            bail!("missing compiler output declarations");
        }
        for (expected, output) in self.outputs.iter().enumerate() {
            if output.index != expected
                || output.presence != "compiler-declaration"
                || !matches!(
                    output.flavor.as_str(),
                    "Normal" | "Linkable" | "Rmeta" | "DebugInfo"
                )
            {
                bail!("invalid output declaration");
            }
            match &output.path {
                PathRef::OutputPath { unit, .. } if *unit == index => {}
                _ => bail!("output has wrong owner"),
            }
            check_path(&output.path)?;
            if let Some(path) = &output.hardlink {
                match path {
                    PathRef::DeliveryPath { unit, .. } if *unit == index => {}
                    _ => bail!("hardlink has an invalid path role or owner"),
                }
                check_path(path)?;
            }
            if let Some(path) = &output.export {
                if !matches!(path, PathRef::ExportPath { .. }) {
                    bail!("export has an invalid path role");
                }
                check_path(path)?;
            }
        }
        match &self.dep_info {
            PathRef::OutputPath { unit, .. } if *unit == index => {}
            _ => bail!("dep-info has wrong owner"),
        }
        check_path(&self.dep_info)
    }
}

impl Word {
    fn validate(&self, path: &impl Fn(&PathRef) -> eyre::Result<()>) -> eyre::Result<()> {
        match self {
            Self::Parts(parts) => {
                if parts.parts.is_empty() {
                    bail!("word has no parts");
                }
                for part in &parts.parts {
                    match part {
                        Part::Literal(literal) => check_bytes(&literal.literal)?,
                        Part::Path(value) => path(&value.path)?,
                    }
                }
            }
            Self::PathList(list) => {
                for component in &list.path_list {
                    path(component)?;
                }
            }
        }
        Ok(())
    }
}

impl PathRef {
    fn validate(
        &self,
        consumer: usize,
        graph: &UnitGraph,
        recipes: &[Resolution],
        dylib_components: usize,
    ) -> eyre::Result<()> {
        let unit_ref = |unit: usize| {
            if unit < graph.units.len() {
                Ok(())
            } else {
                Err(eyre::eyre!("out-of-range path unit"))
            }
        };
        let package_ref = |package: &str| {
            if graph.units[consumer].pkg_id == package {
                Ok(())
            } else {
                Err(eyre::eyre!("unknown path package"))
            }
        };
        match self {
            Self::RelativeSource { unit, bytes } => {
                unit_ref(*unit)?;
                if *unit != consumer {
                    bail!("source path has a different owner");
                }
                relative(bytes)?;
            }
            Self::PackagePath {
                package,
                relative: bytes,
            } => {
                package_ref(package)?;
                relative(bytes)?;
            }
            Self::WorkspaceRoot => {}
            Self::PackageRoot { package } | Self::PackageManifest { package } => {
                package_ref(package)?
            }
            Self::Tool { binding } if binding == "cargo" => {}
            Self::Tool { .. } => bail!("unknown tool path binding"),
            Self::OutputDirectory { unit }
            | Self::DependencyDirectory { unit }
            | Self::HostDependencyDirectory { unit } => {
                unit_ref(*unit)?;
                if *unit != consumer {
                    bail!("directory path has a different owner");
                }
            }
            Self::Artifact { unit, output } => {
                unit_ref(*unit)?;
                if !graph.units[consumer]
                    .dependencies
                    .iter()
                    .any(|dep| dep.index == *unit)
                {
                    bail!("artifact is not a direct dependency");
                }
                match &recipes[*unit] {
                    Resolution::Resolved { recipe }
                        if *output < recipe.outputs.len()
                            && matches!(
                                recipe.outputs[*output].flavor.as_str(),
                                "Linkable" | "Rmeta"
                            ) => {}
                    _ => bail!("artifact has an unsupported producer/output"),
                }
            }
            Self::CompilationSearchDirectory { platform, index }
                if platform.is_none() && *index == 0 => {}
            Self::CompilationSearchDirectory { .. } => bail!("unsupported compilation search view"),
            Self::PreparationDylibComponent { index } if *index < dylib_components => {}
            Self::PreparationDylibComponent { .. } => {
                bail!("out-of-range sampled dynamic path component")
            }
            Self::OutputPath {
                unit,
                relative: bytes,
            }
            | Self::DeliveryPath {
                unit,
                relative: bytes,
            } => {
                unit_ref(*unit)?;
                if *unit != consumer {
                    bail!("output or delivery path has a different owner");
                }
                relative(bytes)?;
            }
            Self::ExportPath { relative: bytes } => relative(bytes)?,
        }
        Ok(())
    }
}

fn check_bytes(bytes: &[u8]) -> eyre::Result<()> {
    if bytes.contains(&0) {
        bail!("NUL cannot enter a compiler word");
    }
    Ok(())
}

fn relative(bytes: &[u8]) -> eyre::Result<()> {
    check_bytes(bytes)?;
    if bytes.is_empty()
        || bytes.starts_with(b"/")
        || bytes
            .split(|byte| *byte == b'/')
            .any(|component| component == b"..")
    {
        bail!("path escapes semantic root");
    }
    Ok(())
}

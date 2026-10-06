//! Explicit native-path materialization for the local parity fixture.
//!
//! These bindings are not content-addressed inputs or backend admission. The
//! caller must bind the exact source, tools and credential-free environment and
//! attach Cargo's live jobserver after applying the overlay. No ambient state is
//! read here, and no process, output directory or runtime lease is created.
use super::*;
use std::collections::BTreeSet;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeBindings {
    version: u32,
    platform: String,
    workspace_root: Vec<u8>,
    packages: Vec<PackageBinding>,
    tools: Tools,
    units: Vec<UnitBinding>,
    compilation_search_directories: Vec<SearchDirectories>,
    export_directory: Option<Vec<u8>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PackageBinding {
    id: String,
    root: Vec<u8>,
    manifest: Vec<u8>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Tools {
    rustc: Vec<u8>,
    cargo: Vec<u8>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UnitBinding {
    unit: usize,
    output_directory: Vec<u8>,
    dependency_directory: Vec<u8>,
    host_dependency_directory: Vec<u8>,
    delivery_directory: Option<Vec<u8>>,
    preparation_dylib_components: Vec<Vec<u8>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchDirectories {
    platform: Option<String>,
    paths: Vec<Vec<u8>>,
}

/// An owned native command. The original Cargo runtime still supplies IO,
/// argument-file retry, jobserver capabilities, scheduling and output delivery.
#[derive(Debug, PartialEq, Eq)]
pub struct BoundCommand {
    pub program: Vec<u8>,
    pub arguments: Vec<Vec<u8>>,
    pub working_directory: Vec<u8>,
    pub environment_overlay: BTreeMap<String, Option<Vec<u8>>>,
    pub outputs: Vec<BoundOutput>,
    pub dep_info: Vec<u8>,
}

/// A compiler declaration can be absent (for example optional DWP output).
/// Runtime publication must inspect the result, not assume every declaration exists.
#[derive(Debug, PartialEq, Eq)]
pub struct BoundOutput {
    pub index: usize,
    pub path: Vec<u8>,
    pub flavor: String,
    pub hardlink: Option<Vec<u8>>,
    pub export: Option<Vec<u8>>,
}

pub struct BoundGraph {
    /// Indexed by the serialized Cargo unit index, not execution order.
    pub commands: Vec<BoundCommand>,
    pub roots: Vec<usize>,
    /// Explicit execution-time input, never obtained from the decoder's process.
    pub inherited_environment: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl BoundGraph {
    /// The live jobserver must still apply CARGO_MAKEFLAGS after this overlay.
    /// This is not the final child environment until that capability is attached.
    pub fn environment_before_jobserver(
        &self,
        unit: usize,
    ) -> eyre::Result<BTreeMap<Vec<u8>, Vec<u8>>> {
        let command = self
            .commands
            .get(unit)
            .ok_or_else(|| eyre::eyre!("unknown command unit"))?;
        let mut environment = self.inherited_environment.clone();
        for (name, value) in &command.environment_overlay {
            if let Some(value) = value {
                environment.insert(name.as_bytes().to_vec(), value.clone());
            } else {
                environment.remove(name.as_bytes());
            }
        }
        Ok(environment)
    }
}

/// Decode and materialize a complete graph with an explicit native binding set.
/// Structural validation is shared with inspection; no legacy policy fallback
/// is accepted. This operation does not attest source/tool identity or authorize
/// the Nix backend. The runtime parity caller checks the actual native command.
pub fn bind(
    value: &Value,
    bindings: &Value,
    inherited_environment: &BTreeMap<Vec<u8>, Vec<u8>>,
) -> eyre::Result<BoundGraph> {
    let bindings: NativeBindings = serde_json::from_value(bindings.clone())?;
    let validated = validated(value, &bindings.platform)?;
    bindings.validate(&validated)?;
    for (name, value) in inherited_environment {
        if name.is_empty() || name.contains(&b'=') {
            bail!("invalid inherited environment name");
        }
        check_bytes(name)?;
        check_bytes(value)?;
    }
    let context = Materializer {
        validated: &validated,
        bindings: &bindings,
    };
    let commands = validated
        .recipes
        .iter()
        .enumerate()
        .map(|(unit, resolution)| {
            let Resolution::Resolved { recipe } = resolution else {
                bail!("unit {unit} has no complete executable recipe");
            };
            context.command(recipe)
        })
        .collect::<eyre::Result<_>>()?;
    Ok(BoundGraph {
        commands,
        roots: validated.graph.roots,
        inherited_environment: inherited_environment.clone(),
    })
}

impl NativeBindings {
    fn validate(&self, validated: &Validated) -> eyre::Result<()> {
        if self.version != 1 || self.platform != "x86_64-unknown-linux-gnu" {
            bail!("unsupported native binding version or platform");
        }
        absolute(&self.workspace_root)?;
        absolute(&self.tools.rustc)?;
        absolute(&self.tools.cargo)?;
        if let Some(path) = &self.export_directory {
            absolute(path)?;
        }
        let packages: BTreeSet<_> = validated
            .graph
            .units
            .iter()
            .map(|unit| unit.pkg_id.as_str())
            .collect();
        let supplied: BTreeSet<_> = self
            .packages
            .iter()
            .map(|package| package.id.as_str())
            .collect();
        if packages != supplied || supplied.len() != self.packages.len() {
            bail!("native package bindings must match graph packages exactly once");
        }
        for package in &self.packages {
            absolute(&package.root)?;
            absolute(&package.manifest)?;
        }
        if self.units.len() != validated.graph.units.len() {
            bail!("missing or extra native unit bindings");
        }
        for (index, unit) in self.units.iter().enumerate() {
            if unit.unit != index {
                bail!("native unit bindings must be in graph index order");
            }
            absolute(&unit.output_directory)?;
            absolute(&unit.dependency_directory)?;
            absolute(&unit.host_dependency_directory)?;
            if let Some(path) = &unit.delivery_directory {
                absolute(path)?;
            }
            let Resolution::Resolved { recipe } = &validated.recipes[index] else {
                bail!("unit {index} was refused by Cargo");
            };
            if unit.preparation_dylib_components.len()
                != recipe.environment.preparation_dylib_path.components
            {
                bail!("sampled dynamic path component count differs from recipe");
            }
            for component in &unit.preparation_dylib_components {
                // Empty, relative and repeated components are meaningful and are
                // retained exactly. Colon cannot occur inside a Linux component.
                check_bytes(component)?;
                if component.contains(&b':') {
                    bail!("dynamic path component contains separator");
                }
            }
        }
        let mut platforms = BTreeSet::new();
        for search in &self.compilation_search_directories {
            if search.platform.is_some() || !platforms.insert(&search.platform) {
                bail!("unsupported or duplicate compilation search binding");
            }
            for path in &search.paths {
                absolute(path)?;
            }
        }
        Ok(())
    }
}

struct Materializer<'a> {
    validated: &'a Validated,
    bindings: &'a NativeBindings,
}

impl Materializer<'_> {
    fn command(&self, recipe: &Recipe) -> eyre::Result<BoundCommand> {
        let arguments = recipe
            .arguments
            .iter()
            .map(|word| self.word(recipe.unit, word))
            .collect::<eyre::Result<_>>()?;
        let environment_overlay = recipe
            .environment
            .overlay
            .iter()
            .map(|(name, action)| {
                let value = match action {
                    EnvironmentAction::Set(value) => Some(self.word(recipe.unit, &value.set)?),
                    EnvironmentAction::Unset(_) => None,
                };
                Ok((name.clone(), value))
            })
            .collect::<eyre::Result<_>>()?;
        let outputs = recipe
            .outputs
            .iter()
            .map(|output| {
                Ok(BoundOutput {
                    index: output.index,
                    path: self.path(recipe.unit, &output.path)?,
                    flavor: output.flavor.clone(),
                    hardlink: output
                        .hardlink
                        .as_ref()
                        .map(|path| self.path(recipe.unit, path))
                        .transpose()?,
                    export: output
                        .export
                        .as_ref()
                        .map(|path| self.path(recipe.unit, path))
                        .transpose()?,
                })
            })
            .collect::<eyre::Result<_>>()?;
        Ok(BoundCommand {
            program: self.bindings.tools.rustc.clone(),
            arguments,
            working_directory: self.path(recipe.unit, &recipe.cwd)?,
            environment_overlay,
            outputs,
            dep_info: self.path(recipe.unit, &recipe.dep_info)?,
        })
    }

    fn word(&self, unit: usize, word: &Word) -> eyre::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        match word {
            Word::Parts(parts) => {
                for part in &parts.parts {
                    match part {
                        Part::Literal(literal) => bytes.extend(&literal.literal),
                        Part::Path(part) => bytes.extend(self.path(unit, &part.path)?),
                    }
                }
            }
            Word::PathList(list) => {
                for (index, path) in list.path_list.iter().enumerate() {
                    let component = self.path(unit, path)?;
                    if component.contains(&b':') {
                        bail!("bound path cannot enter a Linux path list");
                    }
                    if index != 0 {
                        bytes.push(b':');
                    }
                    bytes.extend(component);
                }
            }
        }
        Ok(bytes)
    }

    fn path(&self, consumer: usize, path: &PathRef) -> eyre::Result<Vec<u8>> {
        let package = |id: &str| {
            self.bindings
                .packages
                .iter()
                .find(|package| package.id == id)
                .ok_or_else(|| eyre::eyre!("missing package binding"))
        };
        let bytes = match path {
            PathRef::RelativeSource { bytes, .. } => bytes.clone(),
            PathRef::PackagePath {
                package: id,
                relative,
            } => join(&package(id)?.root, relative),
            PathRef::WorkspaceRoot => self.bindings.workspace_root.clone(),
            PathRef::PackageRoot { package: id } => package(id)?.root.clone(),
            PathRef::PackageManifest { package: id } => package(id)?.manifest.clone(),
            PathRef::Tool { binding } if binding == "cargo" => self.bindings.tools.cargo.clone(),
            PathRef::Tool { .. } => bail!("unknown tool binding"),
            PathRef::OutputDirectory { unit } => {
                self.bindings.units[*unit].output_directory.clone()
            }
            PathRef::DependencyDirectory { unit } => {
                self.bindings.units[*unit].dependency_directory.clone()
            }
            PathRef::HostDependencyDirectory { unit } => {
                self.bindings.units[*unit].host_dependency_directory.clone()
            }
            PathRef::Artifact { unit, output } => {
                let Resolution::Resolved { recipe } = &self.validated.recipes[*unit] else {
                    bail!("refused dependency recipe");
                };
                self.path(*unit, &recipe.outputs[*output].path)?
            }
            PathRef::CompilationSearchDirectory { platform, index } => self
                .bindings
                .compilation_search_directories
                .iter()
                .find(|search| &search.platform == platform)
                .and_then(|search| search.paths.get(*index))
                .ok_or_else(|| eyre::eyre!("missing compilation search binding"))?
                .clone(),
            PathRef::PreparationDylibComponent { index } => {
                self.bindings.units[consumer].preparation_dylib_components[*index].clone()
            }
            PathRef::OutputPath { unit, relative } => {
                join(&self.bindings.units[*unit].output_directory, relative)
            }
            PathRef::DeliveryPath { unit, relative } => join(
                self.bindings.units[*unit]
                    .delivery_directory
                    .as_ref()
                    .ok_or_else(|| eyre::eyre!("missing delivery directory"))?,
                relative,
            ),
            PathRef::ExportPath { relative } => join(
                self.bindings
                    .export_directory
                    .as_ref()
                    .ok_or_else(|| eyre::eyre!("missing export directory"))?,
                relative,
            ),
        };
        Ok(bytes)
    }
}

fn absolute(bytes: &[u8]) -> eyre::Result<()> {
    check_bytes(bytes)?;
    if !bytes.starts_with(b"/") {
        bail!("native path binding must be absolute");
    }
    Ok(())
}

fn join(root: &[u8], relative: &[u8]) -> Vec<u8> {
    let mut bytes = root.to_vec();
    if !bytes.ends_with(b"/") {
        bytes.push(b'/');
    }
    bytes.extend(relative);
    bytes
}

#[cfg(test)]
#[path = "native/tests.rs"]
mod tests;

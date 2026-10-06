use eyre::{self, bail};
use std::{collections::BTreeSet, path::PathBuf};

#[derive(PartialEq, Eq, Copy, Clone, Debug, Hash, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompileMode {
    Test,
    Build,
    Check,
    Doc,
    Doctest,
    Docscrape,
    RunCustomBuild,
}

#[derive(Debug, Hash, serde::Deserialize)]
#[allow(unused)]
pub struct Target {
    pub kind: Vec<String>,
    pub crate_types: Vec<String>,
    pub name: String,
    pub src_path: PathBuf,
    pub edition: String,
    pub doc: bool,
    pub doctest: bool,
    pub test: bool,
}

#[derive(Debug, Hash, serde::Deserialize)]
#[allow(unused)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub name: String,
    pub opt_level: String,
    pub lto: String,
    pub codegen_backend: Option<String>,
    pub codegen_units: Option<u32>,
    pub debuginfo: u32,
    pub split_debuginfo: Option<String>,
    pub debug_assertions: bool,
    pub overflow_checks: bool,
    pub rpath: bool,
    pub incremental: bool,
    pub panic: String,
    #[serde(default)]
    pub strip: Option<Strip>,
}

#[derive(Debug, Hash, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Strip {
    Resolved(ResolvedStrip),
    Deferred(String),
}

#[derive(Debug, Hash, serde::Deserialize)]
pub enum ResolvedStrip {
    Named(String),
}

impl Strip {
    pub fn resolved_name(&self) -> eyre::Result<&str> {
        match self {
            Self::Resolved(ResolvedStrip::Named(name))
                if matches!(name.as_str(), "none" | "debuginfo" | "symbols") =>
            {
                Ok(name)
            }
            Self::Deferred(name) if name == "None" => Ok("none"),
            _ => eyre::bail!("unsupported or unresolved strip setting"),
        }
    }
}

#[derive(Debug, Hash, serde::Deserialize)]
pub struct Dependency {
    pub index: usize,
    pub extern_crate_name: String,
    #[serde(default)]
    pub public: bool,
    #[serde(default)]
    pub noprelude: bool,
    #[serde(default)]
    pub nounused: bool,
}

#[derive(Debug, Hash, serde::Deserialize)]
#[allow(unused)]
pub struct Unit {
    pub pkg_id: String,
    pub target: Target,
    pub profile: Profile,
    pub platform: Option<String>,
    pub mode: CompileMode,
    pub features: Vec<String>,
    pub dependencies: Vec<Dependency>,
}

#[derive(Debug, serde::Deserialize)]
pub struct UnitGraph {
    pub version: u32,
    pub units: Vec<Unit>,
    pub roots: Vec<usize>,
}

pub fn validate_structure(graph: &UnitGraph) -> eyre::Result<Vec<BTreeSet<usize>>> {
    for (i, unit) in graph.units.iter().enumerate() {
        if unit
            .dependencies
            .iter()
            .any(|d| d.index >= graph.units.len())
        {
            bail!("unit {i} has an out-of-range dependency");
        }
    }
    if graph.roots.iter().any(|i| *i >= graph.units.len()) {
        bail!("out-of-range graph root");
    }
    build_script_dependencies(graph)
}

fn build_script_dependencies(graph: &UnitGraph) -> eyre::Result<Vec<BTreeSet<usize>>> {
    fn visit(
        i: usize,
        graph: &UnitGraph,
        active: &mut BTreeSet<usize>,
        memo: &mut [Option<BTreeSet<usize>>],
    ) -> eyre::Result<BTreeSet<usize>> {
        if let Some(found) = &memo[i] {
            return Ok(found.clone());
        }
        if !active.insert(i) {
            bail!("cyclic unit graph at unit {i}");
        }
        let mut result = BTreeSet::new();
        for dep in &graph.units[i].dependencies {
            if graph.units[dep.index].mode == CompileMode::RunCustomBuild {
                result.insert(dep.index);
            }
            result.extend(visit(dep.index, graph, active, memo)?);
        }
        active.remove(&i);
        memo[i] = Some(result.clone());
        Ok(result)
    }
    let mut memo = vec![None; graph.units.len()];
    for i in 0..graph.units.len() {
        visit(i, graph, &mut BTreeSet::new(), &mut memo)?;
    }
    Ok(memo.into_iter().map(Option::unwrap).collect())
}

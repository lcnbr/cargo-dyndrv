use crate::cargo_command::CargoInvocation;
pub use cargo_compiler_recipe::graph::{CompileMode, Unit, UnitGraph};
use color_eyre::eyre::{self, Context as _};

pub(crate) fn discover(cargo: &CargoInvocation<'_>) -> eyre::Result<UnitGraph> {
    let output = cargo
        .unit_graph_command()
        .output()
        .wrap_err("Executing cargo")?;

    if !output.status.success() {
        eyre::bail!(
            "Cargo failed with error {}",
            output.status.code().unwrap_or(-1)
        )
    }

    Ok(serde_json::from_slice(&output.stdout)?)
}

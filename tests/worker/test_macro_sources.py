"""Qualify downstream expansion reading a dependency's complete source tree.

Uses the pinned bootstrap and retains real compiler requests and private-store
evidence under --directory. The proc macro reads data at expansion time rather
than embedding it while compiling the proc-macro library.
"""

import json
import sys
from pathlib import Path

from test_invalidation import InvalidationHarness


class MacroHarness(InvalidationHarness):
    def qualify(self):
        with self.step("prepare external proc macro and pinned tools"):
            self.prepare()
            self.write(
                self.fork_a,
                "packages/itoa/Cargo.toml",
                '[package]\nname = "data-macro"\nversion.workspace = true\n'
                'edition = "2021"\n\n[lib]\nproc-macro = true\n',
            )
            self.write(
                self.fork_a,
                "packages/itoa/src/lib.rs",
                """use proc_macro::TokenStream;
use std::fs;
use std::path::Path;

#[proc_macro]
pub fn value(_: TokenStream) -> TokenStream {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("data/value.txt");
    fs::read_to_string(&path).expect("read macro dependency data").parse().unwrap()
}
""",
            )
            self.write(self.fork_a, "packages/itoa/data/value.txt", "42\n")
            self.write(
                self.a,
                "Cargo.toml",
                '[workspace]\nmembers = ["app"]\nresolver = "2"\n\n'
                '[profile.ci-optim]\ninherits = "dev"\nopt-level = 0\n',
            )
            self.write(
                self.a,
                "app/Cargo.toml",
                '[package]\nname = "application"\nversion = "0.1.0"\n'
                'edition = "2021"\n\n[dependencies]\n'
                'data-macro = { path = "../../fork-alpha/packages/itoa" }\n',
            )
            (self.a / "app/src/lib.rs").unlink()
            self.write(
                self.a,
                "app/src/main.rs",
                'fn main() { println!("{}", data_macro::value!()); }\n',
            )
            self.lock_and_metadata(self.a, "macro")

        with self.step("ordinary pinned Cargo expands dependency macro"):
            output, _, _ = self.command(
                "ordinary-macro",
                [self.spec["cargo"], "run", "--offline", "--locked", "-q"],
                cwd=self.a,
                env=self.cargo_environment(
                    self.run_directory / "ordinary-cargo-target"
                ),
            )
            self.require(output.splitlines() == ["42"], "Wrong ordinary macro output")

        with self.step("immutable downstream compiler reads dependency data") as step:
            baseline = self.run_probe(self.a, "macro-baseline", "application", ["42"])
            app = self.delivery(baseline, "application")
            macro = self.delivery(baseline, "data_macro")
            app_trees = json.loads(Path(app["request"]).read_text())["trees"]
            macro_trees = json.loads(Path(macro["request"]).read_text())["trees"]
            self.require(
                set(macro_trees) < set(app_trees),
                "Downstream compiler did not retain dependency source roots",
            )
            step.update(application=app, macro=macro)

        with self.step(
            "Cargo-fresh macro reads current data in a changed consumer"
        ) as step:
            self.write(self.fork_a, "packages/itoa/data/value.txt", "43\n")
            self.write(
                self.a,
                "app/src/main.rs",
                'fn main() { println!("{}", data_macro::value!()); }\n// consumer edit\n',
            )
            changed = self.run_probe(self.a, "macro-data-edit", "application", ["43"])
            self.require(
                all(r["crate"] != "data_macro" for r in changed["receipts"]),
                "Macro rebuilt instead of remaining Cargo-fresh",
            )
            rebuilt = self.delivery(changed, "application")
            self.compare(app, rebuilt, reused=False)
            changed_trees = json.loads(Path(rebuilt["request"]).read_text())["trees"]
            self.require(
                all(changed_trees[root] != app_trees[root] for root in macro_trees),
                "Consumer retained the macro producer's old source snapshot",
            )
            step["application"] = rebuilt

        with self.step("reexported macro retains its transitive source root") as step:
            self.write(
                self.b,
                "Cargo.toml",
                '[package]\nname = "macro-bridge"\nversion = "0.1.0"\n'
                'edition = "2021"\n\n[dependencies]\n'
                'data-macro = { path = "../fork-alpha/packages/itoa" }\n\n'
                '[workspace]\nresolver = "2"\n',
            )
            self.write(self.b, "src/lib.rs", "pub use data_macro::value;\n")
            self.command("bridge-git-init", ["git", "init", "--quiet", self.b])
            self.command("bridge-git-add", ["git", "add", "."], cwd=self.b)
            self.write(
                self.a,
                "app/Cargo.toml",
                '[package]\nname = "application"\nversion = "0.1.0"\n'
                'edition = "2021"\n\n[dependencies]\n'
                'macro-bridge = { path = "../../different-workspace-name" }\n',
            )
            self.write(
                self.a,
                "app/src/main.rs",
                'fn main() { println!("{}", macro_bridge::value!()); }\n',
            )
            self.lock_and_metadata(self.a, "macro-reexport")
            reexport = self.run_probe(self.a, "macro-reexport", "application", ["43"])
            consumer = self.delivery(reexport, "application")
            request = json.loads(Path(consumer["request"]).read_text())
            arguments = [bytes(arg).decode() for arg in request["arguments"]]
            self.require(
                any(arg.startswith("macro_bridge=") for arg in arguments)
                and not any(arg.startswith("data_macro=") for arg in arguments),
                "Consumer did not exercise the transitive reexport boundary",
            )
            self.require(
                set(macro_trees) < set(request["trees"]),
                "Transitive macro source root was omitted",
            )
            step["application"] = consumer

        with self.step("fresh reexport and macro read edited dependency data") as step:
            self.write(self.fork_a, "packages/itoa/data/value.txt", "44\n")
            self.write(
                self.a,
                "app/src/main.rs",
                'fn main() { println!("{}", macro_bridge::value!()); }\n// downstream edit\n',
            )
            changed = self.run_probe(
                self.a, "reexport-data-edit", "application", ["44"]
            )
            self.require(
                all(
                    r["crate"] not in {"data_macro", "macro_bridge"}
                    for r in changed["receipts"]
                ),
                "Macro or reexport rebuilt instead of remaining Cargo-fresh",
            )
            rebuilt = self.delivery(changed, "application")
            self.compare(consumer, rebuilt, reused=False)
            step["application"] = rebuilt


if __name__ == "__main__":
    sys.exit(MacroHarness.run_cli(MacroHarness.parser(__doc__, "invalidation-macro")))

"""Qualify compiler invalidation boundaries with real host Nix builds.

Retains receipts, structured Nix build logs, fixture checkouts and private-store
inputs under --directory. Registry fixtures use pinned releases; Cargo may fetch
the alternate itoa release if it is absent from the caller's shared cache.
"""

import argparse
import json
import re
import shutil
import sys
import tomllib
from pathlib import Path

from test_integration import Harness


class InvalidationHarness(Harness):
    def delivery(self, record, crate):
        receipts = [r for r in record["receipts"] if r["crate"] == crate]
        self.require(len(receipts) == 1, f"Expected one {crate} delivery: {receipts}")
        receipt = receipts[0]
        self.require(receipt["compilerSucceeded"], f"{crate} compiler failed")
        log = Path(receipt["path"].replace(".nix-result.json", ".nix-stderr"))
        events = [
            json.loads(line[5:])
            for line in log.read_text().splitlines()
            if line.startswith("@nix ")
        ]
        self.require(events, f"Missing structured Nix evidence: {log}")
        receipt["build_starts"] = [
            event
            for event in events
            if event.get("action") == "start" and event.get("type") == 105
        ]
        store = (
            Path("/")
            if self.environment["CARGO_DYNDRV_STORE"] == "system"
            else self.state / "store"
        )
        derivation = store / receipt["derivation"].lstrip("/")
        requests = set(
            re.findall(
                r"/nix/store/[a-z0-9]{32}-stock-cargo-invocation.json",
                derivation.read_text(),
            )
        )
        self.require(len(requests) == 1, "Could not identify this delivery's request")
        request = store / requests.pop().lstrip("/")
        # The failure/GC case may collect immutable requests. Preserve the exact
        # payload and its derivation alongside receipts for later review.
        archive = self.run_directory / "compiler-requests"
        archive.mkdir(exist_ok=True)
        for source in (derivation, request):
            destination = archive / source.name
            if not destination.exists():
                destination.write_bytes(source.read_bytes())
            self.require(
                destination.read_bytes() == source.read_bytes(),
                "Archived immutable compiler input changed",
            )
        receipt["request"] = str(archive / request.name)
        return receipt

    def compare(self, baseline, changed, *, reused):
        before = (baseline["derivation"], baseline["output"])
        after = (changed["derivation"], changed["output"])
        if reused:
            self.require(
                before == after, f"Compiler identity changed: {before} -> {after}"
            )
            self.require(not changed["build_starts"], "Reuse started a Nix builder")
        else:
            self.require(
                before[0] != after[0],
                f"Relevant edit did not change derivation: {before} -> {after}",
            )
            self.require(
                any(
                    event.get("fields", [None])[0] == changed["derivation"]
                    for event in changed["build_starts"]
                ),
                "Invalidation did not start this compiler's Nix builder",
            )

    def run_probe(self, workspace, label, package, expected, *, env=None):
        output, record = self.public(workspace, label, "run", "-p", package, env=env)
        self.require(output.splitlines() == expected, f"Wrong runtime output: {output}")
        return record

    def fresh_target(self, workspace, label, record):
        self.public(workspace, label, "clean")
        self.require(
            not Path(record["target"]).exists(), "Cargo target was not removed"
        )

    def host_store_mount(self):
        with self.step(
            "host store package mounts preserve locked read-only flags"
        ) as step:
            stage = self.run_directory / "host-store-stage"
            logical = self.run_directory / "host-store-logical"
            (stage / "package").mkdir(parents=True, mode=0o700)
            logical.mkdir(mode=0o700)
            configuration = self.run_directory / "host-store-namespace.json"
            configuration.write_text(
                json.dumps(
                    {
                        "stage": str(stage),
                        "logical": str(logical),
                        "store": "/nix/store",
                        "cwd": str(logical),
                        "packages": [
                            {
                                "source": str(self.bootstrap),
                                "target": str(logical / "package"),
                            }
                        ],
                    }
                )
                + "\n"
            )
            output, _, _ = self.command(
                "host-store-package-mount",
                [
                    self.spec["worker"],
                    "namespace",
                    configuration,
                    "--",
                    self.spec["python"],
                    "-c",
                    """import json, os, sys
from pathlib import Path
source, bound = map(Path, sys.argv[1:])
assert (bound / "environment.json").read_bytes() == (source / "environment.json").read_bytes()
source_flags = os.statvfs(source).f_flag
bound_flags = os.statvfs(bound).f_flag
assert bound_flags & os.ST_RDONLY, "Package mount is writable"
assert bound_flags & (2 | 4 | 8) == source_flags & (2 | 4 | 8), "Locked mount flags changed"
print(json.dumps({"source_flags": source_flags, "bound_flags": bound_flags}))
""",
                    self.bootstrap,
                    logical / "package",
                ],
                cwd=self.a,
            )
            step.update(json.loads(output))

    def local_fork(self):
        with self.step("external local fork compiles from a cold target") as step:
            first = self.run_value(self.a, "fork-baseline", "dirty")
            baseline = self.delivery(first, "itoa")
            self.require(baseline["build_starts"], "Cold fork did not start a builder")
            step["baseline"] = baseline

        with self.step(
            "unrelated local trees reuse the fork from a fresh target"
        ) as step:
            self.write(self.a, "README.md", "Unrelated workspace source changed.\n")
            self.write(
                self.a,
                "crates/support/src/lib.rs",
                'pub fn support() -> &\'static str { "support-edited" }\n',
            )
            self.fresh_target(self.a, "fork-clean", first)
            changed = self.run_probe(
                self.a,
                "fork-unrelated-edit",
                "application",
                ["dirty+new|support-edited|ok", f"cwd={self.a}"],
            )
            receipt = self.delivery(changed, "itoa")
            self.compare(baseline, receipt, reused=True)
            step["reused"] = receipt

        with self.step(
            "second checkout reuses fork while keeping a distinct target"
        ) as step:
            for source, destination in ((self.a, self.b), (self.fork_a, self.fork_b)):
                shutil.copytree(
                    source,
                    destination,
                    symlinks=True,
                    ignore=shutil.ignore_patterns("target"),
                )
            manifest = self.b / "Cargo.toml"
            manifest.write_text(
                manifest.read_text().replace(
                    "../fork-alpha/", "../different-fork-name/"
                )
            )
            self.write(self.b, "README.md", "Different checkout and unrelated text.\n")
            self.lock_and_metadata(self.b, "relocated-fork")
            relocated = self.run_probe(
                self.b,
                "fork-second-checkout",
                "application",
                ["dirty+new|support-edited|ok", f"cwd={self.b}"],
            )
            self.require(first["target"] != relocated["target"], "Shared Cargo target")
            receipt = self.delivery(relocated, "itoa")
            self.compare(baseline, receipt, reused=True)
            step["reused"] = receipt

        with self.step(
            "changed fork bytes invalidate and reach the executable"
        ) as step:
            self.edit_fork(self.fork_b, "changed-fork")
            changed = self.run_probe(
                self.b,
                "fork-relevant-edit",
                "application",
                ["changed-fork+new|support-edited|ok", f"cwd={self.b}"],
            )
            receipt = self.delivery(changed, "itoa")
            self.compare(baseline, receipt, reused=False)
            step["rebuilt"] = receipt

    def registry_manifest(self, *, version="1.0.18", optional=False):
        self.write(
            self.a,
            "Cargo.toml",
            '[package]\nname="invalidation-probe"\nversion="0.1.0"\nedition="2021"\n'
            "[workspace]\n[dependencies]\n"
            f'itoa="={version}"\n'
            + ('memchr={version="=2.8.0",optional=true}\n' if optional else "")
            + '[profile.ci-optim]\ninherits="dev"\n',
        )

    def registry(self):
        for name in ("app", "crates"):
            shutil.rmtree(self.a / name)
        self.registry_manifest()
        self.write(
            self.a,
            "src/main.rs",
            "fn main() { let value = if cfg!(qualification_compiler_input) { 44 } else { 42 }; "
            'println!("{}", itoa::Buffer::new().format(value)); }\n',
        )
        with self.step("registry itoa baseline with selected default features") as step:
            metadata = self.lock_and_metadata(self.a, "registry-baseline")
            first = self.run_probe(
                self.a, "registry-baseline", "invalidation-probe", ["42"]
            )
            baseline = self.delivery(first, "itoa")
            self.require(
                baseline["build_starts"], "Cold registry did not start a builder"
            )
            step["baseline"] = baseline
            lock = tomllib.loads((self.a / "Cargo.lock").read_text())
            selected = next(p for p in metadata["packages"] if p["name"] == "itoa")
            self.require(
                selected["version"] == "1.0.18"
                and selected["source"].startswith("registry+"),
                "Baseline itoa is not the pinned registry package",
            )
            features = next(
                n["features"]
                for n in metadata["resolve"]["nodes"]
                if n["id"] == selected["id"]
            )

        with self.step(
            "unused optional lock entry leaves itoa identity unchanged"
        ) as step:
            self.registry_manifest(optional=True)
            metadata = self.lock_and_metadata(self.a, "registry-extra-lock")
            new_lock = tomllib.loads((self.a / "Cargo.lock").read_text())
            self.require(lock != new_lock, "Cargo.lock did not change")
            self.require(
                any(p["name"] == "memchr" for p in new_lock["package"])
                and not any(p["name"] == "memchr" for p in lock["package"]),
                "Unused optional package was not newly locked",
            )
            self.require(
                next(p for p in metadata["packages"] if p["name"] == "itoa")
                == selected,
                "The unrelated lock edit changed the selected itoa package",
            )
            self.require(
                next(
                    n["features"]
                    for n in metadata["resolve"]["nodes"]
                    if n["id"] == selected["id"]
                )
                == features,
                "The unrelated lock edit changed selected itoa features",
            )
            self.fresh_target(self.a, "registry-clean", first)
            changed = self.run_probe(
                self.a, "registry-extra-lock", "invalidation-probe", ["42"]
            )
            self.require(
                all(r["crate"] != "memchr" for r in changed["receipts"]),
                "Optional memchr unexpectedly compiled",
            )
            receipt = self.delivery(changed, "itoa")
            self.require(
                json.loads(Path(baseline["request"]).read_text())["environment"]
                == json.loads(Path(receipt["request"]).read_text())["environment"],
                "Unused lock entry changed the itoa compiler environment",
            )
            self.compare(baseline, receipt, reused=True)
            step.update(reused=receipt, itoa_features=features)

        with self.step(
            "existing launcher survives lock dependency removal by check"
        ) as step:
            launcher = self.a / "target/ci-optim/invalidation-probe"
            installed = launcher.read_bytes()
            launch_config = Path(changed["session"]).parent.parent / "launch.json"
            mounts = json.loads(launch_config.read_text())["packages"]
            self.require(
                any(Path(p["target"]).name == "memchr-2.8.0" for p in mounts),
                "Existing launcher did not retain the optional package mount",
            )
            self.registry_manifest()
            self.lock_and_metadata(self.a, "registry-remove-optional")
            self.require(
                not any(
                    p["name"] == "memchr"
                    for p in tomllib.loads((self.a / "Cargo.lock").read_text())[
                        "package"
                    ]
                ),
                "Optional dependency remained in Cargo.lock",
            )
            _, checked = self.public(
                self.a, "registry-check-removal", "check", "-p", "invalidation-probe"
            )
            self.require(
                launcher.read_bytes() == installed,
                "Check replaced the existing launcher",
            )
            mounts = json.loads(launch_config.read_text())["packages"]
            self.require(
                not any(Path(p["target"]).name == "memchr-2.8.0" for p in mounts),
                "Check left a stale optional package mount in the launcher",
            )
            output, _, _ = self.command(
                "launcher-after-lock-removal", [launcher], cwd=self.a
            )
            self.require(
                output.splitlines() == ["42"], "Retained launcher failed after check"
            )
            step["check_session"] = checked["session"]

        with self.step(
            "failed changed-lock check retains launcher inputs through GC"
        ) as step:
            self.registry_manifest(optional=True)
            self.lock_and_metadata(self.a, "launcher-failed-add")
            source = self.a / "src/main.rs"
            original = source.read_text()
            source.write_text(
                original
                + '\ncompile_error!("intentional invalidation fixture failure");\n'
            )
            try:
                _, failed = self.public(
                    self.a,
                    "launcher-failed-check",
                    "check",
                    "-p",
                    "invalidation-probe",
                    expected=101,
                )
                self.require(
                    any(
                        r["crate"] == "invalidation_probe"
                        and not r["compilerSucceeded"]
                        for r in failed["receipts"]
                    ),
                    "Expected a real failed compiler delivery",
                )
                mounts = json.loads(launch_config.read_text())["packages"]
                self.require(
                    any(Path(p["target"]).name == "memchr-2.8.0" for p in mounts),
                    "Failed check did not update the launcher's package mounts",
                )
                self.public(self.a, "launcher-private-gc", "gc")
                self.require(
                    launcher.read_bytes() == installed,
                    "Failed check or GC replaced the existing launcher",
                )
                output, _, _ = self.command(
                    "launcher-after-failed-check-gc", [launcher], cwd=self.a
                )
                self.require(
                    output.splitlines() == ["42"],
                    "Retained launcher failed after changed-lock compiler failure and GC",
                )
                step["failure_session"] = failed["session"]
            finally:
                source.write_text(original)
            self.registry_manifest()
            self.lock_and_metadata(self.a, "launcher-repaired")
            self.public(
                self.a, "launcher-repaired-check", "check", "-p", "invalidation-probe"
            )

        with self.step("relevant registry version change rebuilds itoa") as step:
            self.registry_manifest(version="1.0.17")
            self.command(
                "registry-fetch-alternate",
                [self.spec["cargo"], "fetch"],
                cwd=self.a,
                env=self.cargo_environment(
                    self.run_directory / "ordinary-cargo-target"
                ),
            )
            metadata = self.lock_and_metadata(self.a, "registry-relevant-lock")
            self.require(
                next(p["version"] for p in metadata["packages"] if p["name"] == "itoa")
                == "1.0.17",
                "Cargo did not select the alternate registry version",
            )
            changed = self.run_probe(
                self.a, "registry-relevant-lock", "invalidation-probe", ["42"]
            )
            receipt = self.delivery(changed, "itoa")
            self.compare(baseline, receipt, reused=False)
            step["rebuilt"] = receipt
        return changed, receipt

    def session_tools(self, previous, baseline):
        with self.step(
            "real session-tool derivation changes without changing compiler inputs"
        ) as step:
            self.fresh_target(self.a, "session-clean", previous)
            # Build a real session-tool variant without depending on an
            # application's Nix expressions or changing compiler configuration.
            expression = self.run_directory / "session-tools.nix"
            expression.write_text(
                """let
  bootstrap = builtins.storePath @BOOTSTRAP@;
  spec = builtins.fromJSON @SPEC@;
  build = name: script: extra: builtins.derivation ({
    inherit name bootstrap;
    system = builtins.currentSystem;
    builder = bootstrap + "/tools/bin/bash";
    args = ["-c" script];
    PATH = spec.runtime_path;
  } // extra);
  nextest = build "dyndrv-qualification-session-nextest" ''
    mkdir -p "$out/bin"
    ln -s ${spec.nextest} "$out/bin/cargo-nextest"
  '' {};
  tools = build "dyndrv-qualification-session-tools" ''
    mkdir -p "$out/bin"
    for file in ${bootstrap}/tools/bin/*; do
      ln -s "$file" "$out/bin/''${file##*/}"
    done
    rm "$out/bin/cargo-nextest"
    ln -s ${nextest}/bin/cargo-nextest "$out/bin/cargo-nextest"
  '' {};
  environment = build "dyndrv-session-environment.json" ''
    printf '%s' "$text" > "$out"
  '' { text = builtins.toJSON
    (spec // { nextest = "${nextest}/bin/cargo-nextest"; runtime_path = "${tools}/bin"; }); };
in build "dyndrv-qualification-bootstrap" ''
  mkdir "$out"
  ln -s ${tools} "$out/tools"
  cp ${environment} "$out/environment.json"
  for name in compiler-environment.json compiler-closure closure worker-tests; do
    ln -s ${bootstrap}/$name "$out/$name"
  done
'' {}
""".replace("@BOOTSTRAP@", json.dumps(str(self.bootstrap))).replace(
                    "@SPEC@", json.dumps(json.dumps(self.spec))
                )
            )
            environment = self.environment.copy()
            for name in self.spec.get("private_environment_names", []):
                environment.pop(name, None)
            output, _, _ = self.command(
                "session-bootstrap",
                [
                    self.spec["nix"],
                    "build",
                    "--impure",
                    "--no-link",
                    "--print-out-paths",
                    "--file",
                    expression,
                ],
                env=environment,
            )
            variant = Path(output.strip().splitlines()[-1])
            spec = json.loads((variant / "environment.json").read_text())
            self.require(variant != self.bootstrap, "Session bootstrap did not change")
            for key in ("nextest", "runtime_path"):
                self.require(
                    spec[key] != self.spec[key], f"Session {key} did not change"
                )
            for key in ("environment", "inputs", "compiler_closure"):
                self.require(
                    spec[key] == self.spec[key], f"Session edit changed compiler {key}"
                )
            for name in ("compiler-environment.json", "compiler-closure"):
                self.require(
                    (variant / name).resolve() == (self.bootstrap / name).resolve(),
                    f"Session edit changed {name} store identity",
                )
            self.bootstrap = variant
            self.spec = spec
            changed = self.run_probe(
                self.a, "session-tools-changed", "invalidation-probe", ["42"]
            )
            self.require(
                changed["target"] != previous["target"],
                "Session policy target did not change",
            )
            root = Path(changed["session"]).parent.parent / "bootstrap-root"
            self.require(
                root.resolve() == variant,
                "Public command did not use variant bootstrap",
            )
            receipt = self.delivery(changed, "itoa")
            self.compare(baseline, receipt, reused=True)
            step.update(
                bootstrap=str(variant),
                compiler_closure=spec["compiler_closure"],
                reused=receipt,
            )

        with self.step(
            "compiler flags invalidate with session tools held fixed"
        ) as step:
            self.fresh_target(self.a, "compiler-flags-clean", changed)
            environment = self.environment | {
                "RUSTFLAGS": self.environment.get(
                    "RUSTFLAGS", self.spec["environment"]["RUSTFLAGS"]
                )
                + " --cfg qualification_compiler_input"
            }
            changed = self.run_probe(
                self.a,
                "compiler-flags-changed",
                "invalidation-probe",
                ["44"],
                env=environment,
            )
            receipt = self.delivery(changed, "itoa")
            request = json.loads(Path(receipt["request"]).read_text())
            self.require(
                "qualification_compiler_input"
                in [bytes(value).decode() for value in request["arguments"]],
                "Changed compiler flags did not reach the current immutable request",
            )
            self.compare(baseline, receipt, reused=False)
            step["rebuilt"] = receipt

    def system_source_scope(self):
        configuration = Path("/etc/cargo-dyndrv/store.json")
        if not configuration.is_file():
            self.summary["system_source_scope"] = "not configured"
            return
        system = InvalidationHarness(
            argparse.Namespace(
                bootstrap=self.bootstrap,
                directory=self.run_directory / "system-source-scope",
                timeout=self.timeout,
            )
        )
        system.environment["CARGO_DYNDRV_STORE"] = "system"
        # Use the production configuration even if the caller selected private
        # mode or supplied another configuration for their own development.
        system.environment["CARGO_DYNDRV_STORE_CONFIG"] = str(configuration)
        system.environment.pop("DATA_ROOT", None)
        try:
            with system.step("prepare isolated system-store registry fixture"):
                system.prepare()
                for name in ("app", "crates"):
                    shutil.rmtree(system.a / name)
                system.registry_manifest()
                system.write(
                    system.a,
                    "src/main.rs",
                    'fn main() { println!("{}", itoa::Buffer::new().format(42)); }\n',
                )
                system.lock_and_metadata(system.a, "system-baseline")
            with system.step(
                "system request retains only its selected package root"
            ) as step:
                first = system.run_probe(
                    system.a, "system-baseline", "invalidation-probe", ["42"]
                )
                baseline = system.delivery(first, "itoa")
                request = json.loads(Path(baseline["request"]).read_text())
                roots = list(request["trees"])
                system.require(
                    len(roots) == 1
                    and roots[0].startswith("/build/cargo-dyndrv/")
                    and roots[0].endswith("/itoa-1.0.18"),
                    f"System itoa captured unrelated source roots: {roots}",
                )
                environment = {
                    bytes(key).decode(): bytes(value).decode()
                    for key, value in request["environment"]
                }
                system.require(
                    environment.get("NIX_BUILD_TOP") == "/build",
                    "NIX_BUILD_TOP was removed or changed in compiler environment",
                )
                # A system store may already contain this baseline. Reuse is
                # required only for the subsequent fresh-target comparisons.
                step.update(baseline=baseline, roots=roots, nix_build_top="/build")
            with system.step("system unrelated workspace edit reuses compiler") as step:
                system.write(
                    system.a, "README.md", "Unrelated system workspace edit.\n"
                )
                system.fresh_target(system.a, "system-unrelated-clean", first)
                changed = system.run_probe(
                    system.a, "system-unrelated", "invalidation-probe", ["42"]
                )
                receipt = system.delivery(changed, "itoa")
                system.compare(baseline, receipt, reused=True)
                step["reused"] = receipt
            with system.step(
                "system unused optional lock dependency reuses compiler"
            ) as step:
                old_lock = (system.a / "Cargo.lock").read_bytes()
                system.registry_manifest(optional=True)
                system.lock_and_metadata(system.a, "system-extra-lock")
                system.require(
                    (system.a / "Cargo.lock").read_bytes() != old_lock,
                    "System optional dependency did not change lockfile",
                )
                system.fresh_target(system.a, "system-lock-clean", changed)
                changed = system.run_probe(
                    system.a, "system-extra-lock", "invalidation-probe", ["42"]
                )
                receipt = system.delivery(changed, "itoa")
                system.compare(baseline, receipt, reused=True)
                system.require(
                    json.loads(Path(receipt["request"]).read_text())["trees"]
                    == request["trees"],
                    "System unused dependency changed selected source trees",
                )
                step["reused"] = receipt
            system.summary["passed"] = True
        except BaseException as error:
            system.summary.update(passed=False, error=system.redact(str(error)))
            raise
        finally:
            try:
                # Release only this fixture's roots. Never stop or collect the
                # shared daemon, including when an assertion fails.
                if system.a.exists():
                    system.public(system.a, "system-final-clean", "clean")
            finally:
                system.save()
                self.summary["system_source_scope"] = str(
                    system.run_directory / "validation.json"
                )

    def qualify(self):
        with self.step("prepare local source fixtures and supplied bootstrap"):
            self.prepare()
        self.host_store_mount()
        self.local_fork()
        previous, baseline = self.registry()
        self.session_tools(previous, baseline)
        self.system_source_scope()


if __name__ == "__main__":
    sys.exit(
        InvalidationHarness.run_cli(
            InvalidationHarness.parser(__doc__, "invalidation-matrix")
        )
    )

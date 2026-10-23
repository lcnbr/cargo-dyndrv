= Native publication of completed Cargo outputs

`cargo-dyndrv publish-native --spec` publishes completed output directories through
Nix's restricted builder RPC. Cargo remains the compilation and test owner. This
entry point does not schedule per-crate Nix builds or create a nested store/daemon.
The separate `nix-cargo` backend and its worker lifecycle are described in
#link("stock-cargo.typ")[the stock Cargo backend guide].

== Build and check

The recovered workspace includes the helper crates and Nix package definition:

```sh
nix develop -c cargo test --workspace --locked -- --test-threads=1
nix develop -c cargo clippy --workspace --all-targets --locked -- -D warnings
nix build .#cargo-dyndrv
```

Building the executable does not provide a builder RPC session. Native
publication requires a Nix installation supporting `builder-rpc-v0`,
`nix store add --scan`, and `nix store submit-output`.

== Builder contract

Use a content-addressed parent derivation with
`requiredSystemFeatures = ["builder-rpc-v0"]`, `__contentAddressed = true`,
`outputHashAlgo = "sha256"`, and `outputHashMode = "recursive"`. Its builder
prepares each output in a separate directory beneath `NIX_BUILD_TOP`, finishes
the application workload and required checks, then invokes the publisher.

The JSON specification must itself be an immutable Nix store input. For example,
using the actual pinned Nix executable in place of the placeholder below:

```json
{
  "nix": "/nix/store/REPLACE-WITH-PINNED-NIX/bin/nix",
  "outputs": {
    "out": "/build/native-delivery",
    "compilerState": "/build/native-compiler-state"
  }
}
```

Include `compilerState` only when the parent declares and prepares that output.
The supplied keys must exactly match the builder's `outputs` environment value.
Directories must exist, be disjoint, and resolve beneath the private build
directory. The publisher also requires the builder's `name` and Unix-socket
`NIX_REMOTE`; it rejects missing, escaping, or overlapping outputs.

Inside that builder, run:

```sh
cargo-dyndrv publish-native --spec /nix/store/REPLACE-WITH-PUBLICATION-SPEC.json
```

For each output the publisher scans references while adding its NAR, submits the
resulting store path under the declared output name, and prints a JSON receipt
containing immutable paths and publication timings. The caller owns workload
selection, compiler-state sanitization, compatibility, and cache delivery.

== Recovery and GammaLoop integration

The original feature commit is
`e7d05f8d6bed9a51e5fbb2a2c451ff359a4a0699` (6 October 2026), with tree
`aa137c77564408cf4fbc9f01133f5739a71b7f33`. It was recovered with its Git objects
from the surviving `dy3ndgh5d5dfgfqqsn56ry71h2gr6l5p-source` snapshot on ITPhlies.
The original three-commit history starts at a source-preservation commit and
does not contain the earlier upstream ancestry. It remains preserved on
#link("https://github.com/lcnbr/cargo-dyndrv/tree/archive/gammaloop-native-state-publisher-2026-10-23")[the recovery archive branch],
including the documentation added during recovery.

On 23 October 2026, `gammaloop-native-state-publisher` was rebased onto the latest
fetched upstream `main`, `d5245b85f82627fcaf041e2c57c8aeb7d57d20fe`. The snapshot
root becomes an ordinary commit containing its differences from upstream, and
the later commits are replayed on it. This establishes upstream ancestry while
preserving the implementation and original author metadata.

The separate GammaLoop integration commit is
`0ed70c33db5dea5a53b5877f9ed483484ea8cca3`. It adds the opt-in `nativePublisher`
argument to GammaLoop's `nix/ci-clean-sheet.nix` and calls this publisher after
the shared Cargo workload. That application integration is not included in this
repository.

Historical GammaLoop notes record a successful full-workload native publication
run. They do not establish a repeatable cold speed improvement or production
qualification. Publishing this recovered branch does not rerun those application
benchmarks or reproduce the earlier cache-delivery experiments.

== Recovery validation (23 October 2026)

The original Rust sources, manifests, lockfile and Nix packaging were preserved.
On x86_64 Linux, `cargo fmt --all -- --check` passed, all 153 workspace tests
passed with `--test-threads=1`, and workspace/all-target Clippy passed with
`-D warnings`. The `cargo-dyndrv` Nix package derivation also evaluated
successfully; a fresh full Nix package build was not run during recovery.

The initial parallel test run failed in
`stock_cargo::nix::jobserver::tests::recovery_preserves_unexpected_files_and_reuses_the_ownership_inode`
with OS error 11 (`WouldBlock`, temporary resource unavailability). Serial
execution passed. The failure is retained as a known validation limitation;
recovery did not change the jobserver implementation or establish its cause.

The upstream rebase produced a tree identical to the archived branch before
these provenance notes were updated. A range comparison also verified that all
three commits following the snapshot root retained identical patches. Since
only history and documentation changed, the code checks above were not rerun.

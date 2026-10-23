# cargo-dyndrv

## GammaLoop native publication

The `gammaloop-native-state-publisher` branch includes `publish-native --spec`.
Cargo owns compilation and tests; the publisher adds completed output directories
to Nix and submits them through `builder-rpc-v0`. See the
[native publication guide](docs/native-publication.typ) for the builder contract
and the distinction from the private compiler backend below.

This research branch preserves the recovered ITPhlies commit
`e7d05f8d6bed9a51e5fbb2a2c451ff359a4a0699` and its original three-commit history.
That history begins at preservation root `df01839e654c6cd1b039ce07f024a13509533fec`;
its earlier upstream ancestry was not recovered. The original author metadata is
retained. Historical GammaLoop validation does not establish a performance gain
or current production readiness.

## Stock Cargo backend in this fork

For the GammaLoop experiment, use the implemented `capture-cargo`, `nix-cargo`
and `snapshot-references` commands with an unmodified Cargo executable. The
[stock-Cargo backend guide](docs/stock-cargo.typ) describes command usage, the
private Nix worker, jobserver transport and compiler-state ownership. This route
uses stock Nix with content-addressed derivations; its worker lifecycle and cache
delivery must be supplied explicitly.

The following sections describe the original unit-graph executor and earlier
research graph variants, with their separate requirements and limitations.

This repo contains a tool to build Rust programs in Nix with dynamic derivations.
`cargo-dyndrv` uses the Cargo unit graph to generate a minimal Nix minimal derivation
for each crate, adding only the necessary dependencies.

All generated derivations are content-addressing, which can can reduce rebuilds when a crate's source code or build
flags change but the resulting binary is identical.

## Inside a derivation
A simple example is included in the [ffmpeg-example](https://github.com/obsidiansystems/cargo-dyndrv/tree/ffmpeg-example)
branch.

Enable the `cargo-dyndrv.overlays.default` overlay from `cargo-dyndrv` flake and
replace instances of `rustPlatform.buildRustPackage` with `pkgs.buildDynamicCrate`.

Then, add an argument `outputs = ["crate-name"];` with the name of the built crate.

If needed, add dependencies of each build script to a new `extern` field.
The keys are the Cargo crate IDs, and the values are attrsets that may contain
`path` and `env` keys for extra directories to add to the path and environment variables to set.

An entry for a crate that uses pkg-config might look like:
```nix
"registry+https://github.com/rust-lang/crates.io-index#ffmpeg-sys-next@9.0.0" = {
  path = [ "${lib.getBin pkgs.buildPackages.pkg-config}/bin" ];
  env = {
    PKG_CONFIG_PATH = "${lib.getDev pkgs.ffmpeg-headless}/lib/pkgconfig";
    PKG_CONFIG = lib.getExe pkgs.buildPackages.pkg-config;
  };
};
```

## Outside a derivation
It is possible to run cargo-dyndrv outside of a derivation for testing or development.

Build `cargo-dyndrv` with a standard `cargo build` in the default devShell or with the `.#cargo-dyndrv` flake package.

Then, with `rustc` and `cargo` in the PATH, run normal Cargo commands with `cargo-dyndrv` instead of `cargo`.

It will print out derivation paths, which can be built with Nix.

For example:
```shell
drvPath="$(nix run github:obsidiansystems/cargo-dyndrv#cargo-dyndrv -- build)"
nix build --print-out-paths "$drvPath^*"
```

If external dependencies are required, they must be converted into a JSON format then passed to `cargo-dyndrv`.

Pass values in the same format as the `extern` field when building inside a derivation to the `writeExtern` function,
then set the `EXTERN_PATH` environment variable to its path or copy it to `extern.json` in the current directory.

## Unit-graph dependencies
`cargo-dyndrv` always generates content-addressing derivations,
which require the `ca-derivations` experimental feature in the Nix daemon.

When running inside a derivation, `cargo-dyndrv` requires the `dynamic-derivations` experimental feature to build the
generated derivations and a sufficiently recent version of Nix to add derivations to the store.

At time of writing, the latest stable version of Nix is 2.35, which does not include the necessary changes.
Before Nix 2.36 is released, only git versions of Nix are supported.

## Platform support
Due to limited hardware and time, `cargo-dyndrv` has only been tested on x86_64 Linux machines,
both natively and cross-compiled for AArch64 Linux. However, we expect it should work on other Linux architectures.

macOS is unlikely to work properly due to hardcoded name assumptions in the code and lack of support for the special
"Framework" libraries. This may be changed in the future.

## Blog posts

As we work on `cargo-dyndrv`, we aim to write up what we are doing in a series of blog posts:

1. [cargo-dyndrv: A Beginning](https://blog.obsidian.systems/cargo-dyndrv-a-beginning/) (2026-09-16)

---

<p align="center">
  <img src="logos.svg" alt="Obsidian Systems × Saronic" width="70%">
</p>

## Inspecting a compiler plan without Nix

The experimental planner separates Cargo unit semantics from Nix path binding:

```shell
CARGO_INCREMENTAL=0 RUSTC_BOOTSTRAP=1 cargo build -Z unstable-options --unit-graph > units.json
cargo-dyndrv plan --unit-graph units.json --host x86_64-unknown-linux-gnu
```

This command reads the graph and emits JSON without connecting to a daemon,
importing source trees, running a compiler, or requiring the Nix helper tools.
Each unit has either a `plan` or an `unsupported` reason. A successful planner
invocation means the graph was inspected; it does not mean every unit is
supported or any checks have run. `doctest` nodes are reported as unsupported,
including graph placeholders from a `cargo test --no-run` request.

`rustcArgs` contains semantic compiler arguments. The backend still binds the
source and output paths, dependency paths, metadata disambiguation, linker, and
build-script results. Artifact filenames are the Linux rustc names before Cargo
hash suffixes and public-output renaming. Build/check plans cover library output
combinations, binaries and proc macros; link-producing consumers select `.rlib`
dependencies, while check consumers select metadata (proc macros always require
executable code). Codegen units, assertions, overflow checks, panic, stripping,
and split debuginfo are preserved. Cargo's `lto = false` keeps default
local LTO; only `lto = "off"` emits `-C lto=off`.

Test-mode units require `--test-harnesses FILE`, a JSON object mapping graph unit
indices to manifest `harness` booleans (for example `{"4": true, "5": false}`).
For check-mode units every context must also be supplied: `null` means ordinary
checking, and `true`/`false` mean checking a test with/without a harness. Cargo
serializes both ordinary and test checks as `"check"`; even byte-identical graph
units can require different compiler arguments. Missing contexts are rejected.
The graph's `target.test` field does not provide these settings. Test plans describe
compilation only; the current Nix backend rejects tests and checks until context settings
and execution ownership are wired. Incremental compilation, explicit cross-crate
LTO, custom codegen backends, dependency visibility modifiers, non-Linux artifact
naming, rustdoc and build-script execution are also explicitly unsupported by the
planner. Build scripts continue through the existing backend execution path.

This is not yet a complete Cargo command replay or GammaLoop build backend.
Cargo configuration/rustflags, complete package and build-script environments,
workspace-relative source/assets/fixtures and writable
runtime data, Clippy, nextest execution and doctests still require independent
integration. In particular, importing just a package directory does not preserve
workspace-relative paths. The existing compiler backend uses the same semantic
planner; there is no second implementation of these compiler arguments.

The compiler backend now declares `docsrs`, `test`, and every feature from Cargo
package metadata, including disabled and implicit features. It also forwards
`rustc-check-cfg` build-script directives to the owning compiler invocation without
altering quoted values. This does not yet provide Cargo's complete lint policy:
`--cap-lints`, Cargo-configured rustflags and other command-context settings remain
separate gaps.


## Compiler output staging

The compiler plan selects `/build/compiler-output` inside each private Nix builder.
`compile-wrap` creates that directory exclusively, executes the existing complete
compiler command (including build-script environment wrappers), and copies its
regular files/directories to the existing public `$out` only after success.
Artifact filenames, file modes, source paths, working directory, profile flags and
dependency argument construction are otherwise unchanged. A pre-existing
staging directory or public output is an error. Paths must be absolute and
disjoint, without parent (`..`) components; nested publication is rejected before
executing the compiler. Compiler failure and termination
by signal fail the wrapper and publish no artifacts; non-regular output entries
are rejected. Builder isolation is required: this path is not a shared host cache.

This narrows the supported profiles: `rpath = true` is now rejected before any
source import or compilation. Rustc computes those runtime search paths relative
to its output directory, so copying can invalidate relative entries. An absolute
search entry from a native wrapper may mask this problem; it is not a guarantee.
The wrapper does not rewrite linker flags or embedded paths. Absolute native search
paths and externally supplied runtime search paths still need to be valid after
installation. The backend currently does not request dep-info; if invoked directly
with dep-info, the wrapper preserves its staged target paths without rewriting them.

Only the compiler output path is stabilized here. Package source imports,
`CARGO_MANIFEST_DIR`, working directories and dependency paths retain their existing
behavior; the dependency-derivation/Unit metadata hash algorithm is also unchanged.
Adding a wrapper changes dependency derivations, so downstream metadata values can
change even without a source edit. This does not promise identity continuity. Build-script
execution and generated `OUT_DIR` remain separate public outputs. Workspace-relative
sources and generated paths, Cargo-owned stable identity, state restoration/export,
and changed-crate incremental reuse are not implemented by this change. Proc-macro
and dynamic-library consumers must load the copied public artifact, not a path in
the temporary staging directory.


The integrated fixed path currently requires a Linux Nix sandbox with private
`/build`. Nonsandbox builds, a different sandbox build directory, Darwin, and
arbitrary symlink aliases for the wrapper's path arguments are not supported by
this prototype. The host wrapper fixtures use distinct temporary directories;
they never create a shared host `/build/compiler-output`.

## Opt-in workspace source layout

`CARGO_DYNDRV_WORKSPACE_SOURCE=/absolute/prepared/workspace` enables a whole-tree
source layout for local path packages. It must match Cargo's canonical workspace
root. Set `CARGO_TARGET_DIR` outside that tree; VCS metadata, a root `target`,
symlinks and special files are rejected. Local manifests and target source files
must lie inside the declared tree. Registry/git packages keep their existing
isolated-source behavior. The default mode is unchanged and does not require the
new `SOURCE_WRAP` helper.

Every local compiler and build-script derivation depends on the entire workspace
source. `source-wrap` exclusively copies it to private `/build/source`, removes
write bits, and checks bytes, entry types/names and modes after execution. Source
mutation fails the build; generated files must use the existing declared
`OUT_DIR`. Rustc runs from workspace cwd with a workspace-relative input and
mapped `CARGO_MANIFEST_DIR`/`CARGO_MANIFEST_PATH`; build-wrap still executes scripts
from their package cwd. Compiler output staging and public artifact locations
are unchanged. This requires the existing Linux private `/build` sandbox.

This is a functional reference, not a granular source cache: every workspace
edit invalidates every local recipe. Source-copy/verification costs are also
per derivation. Stable Cargo metadata, stable generated `OUT_DIR` paths, and
incremental state are not implemented here. The existing metadata algorithm is
unchanged, so altered dependency derivations can alter downstream metadata.
Proc macros reading workspace assets during expansion receive the same declared
workspace tree. Source-mutating build scripts, external path packages, nested
symlinks and unreviewed registry/git source remapping are unsupported.

Embedded `/build/source` paths are only valid where the required runtime tree is
also materialized. Runtime tests and public packaging need their own reviewed
fixture/asset-delivery contract before this mode can be used for those outputs.
Compilation success alone does not establish runtime compatibility.

## Inspecting Cargo-owned identities (research graph v2)

The `plan` command also accepts the separately patched Cargo graph-v2 export.
It consumes Cargo's generated `c_metadata`, `unit_id`, optional filename suffix,
and exact test/check role; it does not reconstruct those hashes. Library and
executable artifact names include Cargo's supplied suffix. Matching harness
sidecars are permitted; conflicting values fail. Graph-v2 fields remain separate
from the original `Unit` hash. The default Cargo-like invocation still accepts
only v1; the separately bound prototype below is an explicit execution route.

The v2 report preserves all resolved rustflags, rustdocflags and extra compiler
arguments. Only `--cfg` and `--check-cfg` flag forms are currently applied to its
partial rustc plan; other flags produce an explicit unsupported reason. User
flags can override Cargo-generated identities or output names, so these fields
must not be described as the final compiler command. Any explicit LTO anywhere
in a v2 graph disables partial compiler plans: the export contains profile data
but not Cargo's effective graph-wide bitcode/emit policy. Incremental and other
existing unsupported profiles/modes remain unsupported. Raw rustdoc graph
identities are not necessarily the Check metadata that Cargo selects for docs.

Every v2 row and report has `executionReady: false`. Build-script dependencies
are listed explicitly, including transitive ones; generated `OUT_DIR`, directive
outputs, package/native environment, lint policy and path binding remain separate
inputs. Callers must bind the graph to the producing Cargo/rustc, source and
configuration hashes. A successful inspection does not validate those external
inputs, execute tests, preserve compiler state or establish incremental reuse.


## Restricted bound graph-v2 prototype

`inspect-bound-graph --input FILE` verifies a strict version1 manifest without
connecting to Nix. `execute-bound-graph --input FILE --daemon-socket PRIVATE_SOCKET`
performs the same validation, imports the captured source NAR, creates the CA
derivation graph, and prints root derivation paths. It does not build them.

The manifest binds graph/metadata files and their SHA256 hashes, producing Cargo
and rustc binaries plus saved version outputs, recorded offline/locked invocations
and environment, a source root/NAR hash, and an explicit `workspace-readonly`
layout. It requires exactly these `builder_tools` entries: `rustc`, `cargo`, `cc`,
`host-cc`, `cxx`, `host-cxx`, `env-wrap`, `build-wrap`, `compile-wrap`, `target-env`,
`ln`, and `source-wrap`. Each entry specifies an absolute immutable store executable
and its SHA256. The actual rustc must equal the producer rustc. An optional
`diagnostic_rustc_probe` is a distinct executable layer receiving that exact rustc
as its first argument; it does not replace the compiler identity.

This route accepts only reachable local workspace Build units on
`x86_64-unknown-linux-gnu`, with one ordinary library, proc-macro, or binary output
role per unit. Every selected package and target must match Cargo metadata and
remain within the source tree. A selected package declaring any build script is
rejected, including when its graph node was omitted. Tests, checks, docs, nonlocal
packages, unsupported dependency flags, incremental profiles, explicit LTO,
rpath, and unhandled raw compiler flags remain rejected. Legacy environment
extern/source overrides and execution inside a derivation are rejected as well.

Cargo's generated metadata/suffix are used directly through the shared planner;
source is staged at `/build/source` and compiler outputs at
`/build/compiler-output`. All validation completes before daemon connection, and
source import consumes the same captured NAR bytes that were checked. The socket
must be explicit with a private0700 parent; an experiment driver must additionally
verify current-user ownership and its isolated daemon/store configuration.

The hash manifest binds a reviewed producer attestation, not proof that arbitrary
JSON originated from Cargo. This is a small functional prototype, not full Cargo
replay: package/native environment, lint policy and build-script semantics remain
incomplete. The whole-workspace source input invalidates every local unit on any
workspace change. It provides no shared compiler state or incremental reuse.

### Generated output layout planning (research only)

`cargo-dyndrv plan-generated-layout --unit-graph graph-v2.json --host
x86_64-unknown-linux-gnu` prints a pure `generated-layout-v1` contract. Each
RunCustomBuild unit owns `/build/generated/v1/<Cargo unit_id>/out`. Its selected
build-script executable uses the distinct compiler unit's planned artifact name.
Every consumer lists conservative transitive generated trees and required `OUT`
and `FLAGS` selections from the same producer. Filesystem availability does not
specify which directives Cargo applies or their ordering.

These are **symbolic required inputs**, not constructed Nix dependency edges.
The command reads the graph and prints JSON; it does not contact a daemon,
materialize trees, run scripts, or enable build-script execution in the restricted
bound executor. It reports unsupported compiler semantics and unresolved
provenance, source/tool/environment, filesystem and directive contracts explicitly.
An unchanged logical root establishes neither unchanged contents nor incremental
reuse. A future backend must bind both actual producer outputs, retain ordinary
source and recipe dependencies, and materialize generated trees before execution.

### Generated tree helper (research only)

`source-wrap --generated-v1 --source SOURCE [--input RUN_ID OUT FLAGS]...
[--owner RUN_ID | --producer RUN_ID PUBLIC_OUT PUBLIC_FLAGS] -- COMMAND...`
requires a private `/build` working directory. It copies the source read-only to
`/build/source`, each predecessor's `OUT` to
`/build/generated/v1/RUN_ID/out`, and its `FLAGS` to
`/build/generated-flags/v1/RUN_ID`. `--owner` selects an admitted predecessor as
the compiler's `OUT_DIR`. `--producer` selects a fresh output root; the child
creates that root and its flags directory, and both public outputs are copied
only after successful execution and source/predecessor validation. The generated
mode removes inherited `out` and `flags` environment variables from the child.
The legacy source-wrapper invocation retains its original behavior.

Generated copies and exports retain non-write permission bits and normalize file
and directory mtimes to exactly one second after the Unix epoch. Inputs and
outputs must be disjoint regular-file/directory trees; symlinks, special files,
existing destinations, malformed identities and missing input pairs are rejected.
Checks also run after the child so changed generated/output ancestors cannot
redirect export. Failed export cleans only newly reserved output roots whose
inode identities still match; preexisting or replaced paths are preserved.

This helper assumes a private builder and immutable declared inputs. It is not a
security boundary for malicious children or a resolver of Nix dependencies. A
CLI pair does not prove the two paths belong to one producer: the future typed
backend must bind both actual `out` and `flags` selections. Shell-generator tests
establish materialization and failure behavior; they do not establish Cargo
build-script compatibility, Nix closure delivery, persistent `OUT_DIR` semantics,
native directive ordering, incremental reuse or GammaLoop execution support.

### Cargo-generated codegen policy inspection (research graph v3)

The pure `plan` and `plan-generated-layout` commands also accept the separately
validated Cargo graph-v3 export. The shared compiler planner copies Cargo's
ordered generated emission and LTO argument slices; it does not resolve LTO from
profiles or graph ancestry. A root requesting LTO therefore no longer disables
an otherwise supported script compiler's partial v3 plan. Graph-v2 retains its
existing graph-wide LTO refusal, and bound execution still accepts only v2.

Graph-v3 requires policy version1 on Build/Test/Check units and explicit null on
rustdoc and script-run units. Unknown versions, missing fields and unsupported
argument sequences fail validation. Raw native or output/identity-changing flags,
incremental profiles, rpath, custom backends and unsupported modes retain their
existing restrictions. The known no-embed-metadata policy is preserved in the
report but remains unsupported until its artifact/dependency contract is handled.

Cargo's emission slice includes dep-info. V3 plans list its typed `.d` file under
`auxiliaryArtifacts`, separately from primary Rust dependency/executable outputs,
using Cargo's optional filename suffix exactly once. These files are not Rust
dependency candidates. This describes compiler filenames; it does not implement
Cargo's public dep-info rewriting, relocation or writable compiler-state reuse.

These remain partial plans. The generated slices retain every token and their
internal order and occupy Cargo's emission/LTO stages among the supported
prefer-dynamic, opt/panic and CGU/debug flags; extra compiler arguments precede
rustflags. Other argument spelling, explicit default flags and remaining profile
positions retain the existing planner's behavior, so a match of selected slices
and stages is not complete Cargo argv equality.
Source/output/dependency paths, native and build-script environment/directives,
lint policy and provenance still need separate binding. Both reports retain
`executionReady: false`; no compiler, script, daemon or cache operation is enabled
by graph-v3 inspection.

The backend binds its selected linker through the existing declared `Tool`.
Without a planned linker it preserves the existing fallback arguments. A single
separated `-C linker=PATH` must match that tool exactly and stays at its original
position; the backend does not append an override. Conflicting or repeated
linkers and unsupported argument spellings fail before command mutation.

This prerequisite does not admit native flags into graph planning or enable
schema3, build-script or incremental execution. Before those gates change, the
production argv and environment need comparison with Cargo, including native
inputs, build-script environment removals and directive target/order semantics.
The generated `OUT`/`FLAGS` materializer does not establish those semantics.

The build wrapper constructs its final child through one private `ScriptProcess`
owner, also available to the exact-source observation harness. Constructing the
command creates no outputs and starts no process. Its environment contains an
`OUT_DIR` override and otherwise inherits the preceding wrappers' environment;
`Command::get_envs()` reports only explicit set/removal operations. Tests observe
the actual child to check byte values, empty values, removals and inheritance.
This adds no CLI execution route or graph admission. Full Cargo parity still
requires the production derivation environment and complete wrapper chain,
bound native inputs, live jobserver capabilities and correct directive ordering.
